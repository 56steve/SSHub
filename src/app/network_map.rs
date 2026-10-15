//! Network map tab (issue #79): cursor, navigation and actions over
//! [`crate::map::Map`]. The model is pure; this is the stateful side.

use super::*;
use std::collections::{HashMap, HashSet};

use crate::map::{HopKey, Map, Row};

/// What the map cursor is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapSel {
    /// A hop node (any ancestor on the row's path).
    Hop(usize),
    /// One host glyph.
    Host(usize),
    /// A jump loop, by index in [`Map::cycles`].
    Cycle(usize),
    /// The direct-only bucket.
    Direct,
}

/// Run state of a tunnel, for the tunnel lens colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelState {
    Running,
    Reconnecting,
    Stopped,
}

/// A tunnel as the map shows it.
#[derive(Debug, Clone)]
pub struct HostTunnel {
    pub id: i64,
    pub kind: crate::store::TunnelType,
    pub state: TunnelState,
    pub summary: String,
}

/// Blast-radius lens: one hop and every host that depends on it.
pub struct BlastLens {
    pub key: HopKey,
    pub hosts: HashSet<usize>,
}

/// Which lens re-skins the map.
pub enum Lens {
    None,
    Blast(BlastLens),
    Tunnel,
}

/// How one row is drawn under the active lens and filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    Normal,
    /// Out of the lens, or only kept as context for a filter match.
    Dim,
    /// Inside a blast radius.
    Warn,
}

/// `#` tag picker: feeds `tag:` terms into the filter.
pub struct TagPicker {
    pub items: Vec<String>,
    pub selected: usize,
}

/// The map model plus the cursor, lens and filter. `pos` walks along the
/// selected row's path: first each hop from the root down to the row's own
/// node, then its host glyphs, left to right.
pub struct MapView {
    pub map: Map,
    /// Every row of the map; `rows` is the part the filter keeps.
    all_rows: Vec<Row>,
    pub rows: Vec<Row>,
    /// Rows (indexes into `rows`) kept only as context above a filter match.
    pub context: HashSet<usize>,
    /// Hosts the filter matches; `None` while no filter is set.
    pub matched: Option<HashSet<usize>>,
    pub row: usize,
    pub pos: usize,
    pub lens: Lens,
    pub query: String,
    /// The filter line owns the keyboard.
    pub editing: bool,
    pub picker: Option<TagPicker>,
}

impl MapView {
    pub fn new(map: Map) -> Self {
        let all_rows = map.rows();
        Self {
            map,
            rows: all_rows.clone(),
            all_rows,
            context: HashSet::new(),
            matched: None,
            row: 0,
            pos: 0,
            lens: Lens::None,
            query: String::new(),
            editing: false,
            picker: None,
        }
    }

    /// Hops on the path to `row`'s node (1 for a loop or the direct bucket).
    pub fn hop_count(&self, row: usize) -> usize {
        match self.rows.get(row) {
            Some(Row::Node(n)) => self.map.nodes[*n].depth + 1,
            _ => 1,
        }
    }

    fn slots(&self, row: usize) -> usize {
        let Some(&r) = self.rows.get(row) else {
            return 0;
        };
        self.hop_count(row)
            + match r {
                Row::Direct => 0,
                _ => self.map.row_hosts(r).len(),
            }
    }

    pub fn selection(&self) -> Option<MapSel> {
        let r = *self.rows.get(self.row)?;
        let hops = self.hop_count(self.row);
        Some(match r {
            Row::Node(n) => {
                if self.pos < hops {
                    MapSel::Hop(self.map.node_path(n)[self.pos])
                } else {
                    MapSel::Host(*self.map.row_hosts(r).get(self.pos - hops)?)
                }
            }
            Row::Cycle(i) => {
                if self.pos < hops {
                    MapSel::Cycle(i)
                } else {
                    MapSel::Host(*self.map.row_hosts(r).get(self.pos - hops)?)
                }
            }
            Row::Direct => MapSel::Direct,
        })
    }

    /// Step to the previous / next row. From a hop the cursor keeps its depth
    /// (clamped); from a glyph it lands on the same glyph index (clamped), or
    /// on the row's own hop when that row has no glyphs.
    pub fn move_row(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let next = (self.row as isize + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        if next == self.row {
            return;
        }
        let old_hops = self.hop_count(self.row);
        let new_hops = self.hop_count(next);
        let new_glyphs = self.slots(next) - new_hops;
        self.pos = if self.pos < old_hops || new_glyphs == 0 {
            self.pos.min(new_hops - 1)
        } else {
            new_hops + (self.pos - old_hops).min(new_glyphs - 1)
        };
        self.row = next;
    }

    /// Walk along the row: `-1` toward the root, `+1` toward the glyphs.
    pub fn move_col(&mut self, delta: isize) {
        let max = self.slots(self.row).saturating_sub(1) as isize;
        self.pos = (self.pos as isize + delta).clamp(0, max.max(0)) as usize;
    }

    fn clamp(&mut self) {
        self.row = self.row.min(self.rows.len().saturating_sub(1));
        self.pos = self.pos.min(self.slots(self.row).saturating_sub(1));
    }

    /// Row and `pos` of host `h`'s glyph, if its row is currently shown.
    pub fn locate(&self, h: usize) -> Option<(usize, usize)> {
        for (i, &r) in self.rows.iter().enumerate() {
            if let Some(k) = self.map.row_hosts(r).iter().position(|&x| x == h) {
                return Some((i, self.hop_count(i) + k));
            }
        }
        None
    }

    /// Put the cursor on host `h`'s glyph. `false` when it is not shown.
    pub fn jump_to_host(&mut self, h: usize) -> bool {
        match self.locate(h) {
            Some((row, pos)) => {
                self.row = row;
                self.pos = pos;
                true
            }
            None => false,
        }
    }

    /// Re-derive the visible rows from `query`, keeping the cursor on what it
    /// stood on when that is still shown.
    pub fn apply_filter(&mut self) {
        let before = self.selection();
        let filter = crate::map::MapFilter::parse(&self.query);
        self.context.clear();
        if filter.is_empty() {
            self.matched = None;
            self.rows = self.all_rows.clone();
        } else {
            let matched: HashSet<usize> = filter.matching(&self.map).into_iter().collect();
            let map = &self.map;
            let strong: Vec<bool> = self
                .all_rows
                .iter()
                .map(|&r| match r {
                    Row::Node(n) => {
                        matches!(map.nodes[n].key, HopKey::Saved(h) if matched.contains(&h))
                            || map.row_hosts(r).iter().any(|h| matched.contains(h))
                    }
                    Row::Cycle(i) => {
                        map.cycles[i].path.iter().any(|h| matched.contains(h))
                            || map.row_hosts(r).iter().any(|h| matched.contains(h))
                    }
                    Row::Direct => map.direct.iter().any(|h| matched.contains(h)),
                })
                .collect();
            let depth = |r: Row| match r {
                Row::Node(n) => map.nodes[n].depth,
                _ => 0,
            };
            let mut rows = Vec::new();
            let mut context = HashSet::new();
            for (i, &r) in self.all_rows.iter().enumerate() {
                let d = depth(r);
                let below = self.all_rows[i + 1..]
                    .iter()
                    .take_while(|&&x| matches!(x, Row::Node(_)) && depth(x) > d)
                    .count();
                let has_match_below = (i + 1..=i + below).any(|j| strong[j]);
                if strong[i] || has_match_below {
                    if !strong[i] {
                        context.insert(rows.len());
                    }
                    rows.push(r);
                }
            }
            self.rows = rows;
            self.context = context;
            self.matched = Some(matched);
        }
        self.row = self.row.min(self.rows.len().saturating_sub(1));
        match before {
            Some(MapSel::Host(h)) if self.jump_to_host(h) => {}
            Some(MapSel::Hop(n)) => {
                if let Some(i) = self.rows.iter().position(|&r| r == Row::Node(n)) {
                    self.row = i;
                    self.pos = self.hop_count(i) - 1;
                }
            }
            _ => {}
        }
        self.clamp();
    }

    /// Whether `row` (index into `rows`) lies inside the active blast radius.
    fn row_in_radius(&self, blast: &BlastLens, row: usize) -> bool {
        match self.rows[row] {
            Row::Node(n) => self
                .map
                .node_path(n)
                .iter()
                .any(|&p| self.map.nodes[p].key == blast.key),
            Row::Cycle(i) => self.map.cycles[i]
                .hosts
                .iter()
                .any(|h| blast.hosts.contains(h)),
            Row::Direct => false,
        }
    }

    /// How `row` is drawn under the active lens and filter.
    pub fn tint(&self, row: usize) -> Tint {
        if let Lens::Blast(b) = &self.lens {
            return if self.row_in_radius(b, row) {
                Tint::Warn
            } else {
                Tint::Dim
            };
        }
        if self.context.contains(&row) {
            Tint::Dim
        } else {
            Tint::Normal
        }
    }

    /// Dependents of the blast lens in reading order.
    fn blast_order(&self, blast: &BlastLens) -> Vec<usize> {
        let mut at: Vec<(usize, usize, usize)> = blast
            .hosts
            .iter()
            .filter_map(|&h| self.locate(h).map(|(r, p)| (r, p, h)))
            .collect();
        at.sort_unstable();
        at.into_iter().map(|(_, _, h)| h).collect()
    }

    /// `n` / `N`: move the cursor to the next / previous dependent.
    pub fn blast_step(&mut self, delta: isize) {
        let Lens::Blast(b) = &self.lens else {
            return;
        };
        let order = self.blast_order(b);
        if order.is_empty() {
            return;
        }
        let here = match self.selection() {
            Some(MapSel::Host(h)) => order.iter().position(|&x| x == h),
            _ => None,
        };
        let next = match (here, delta > 0) {
            (Some(i), true) => (i + 1) % order.len(),
            (Some(i), false) => (i + order.len() - 1) % order.len(),
            (None, true) => 0,
            (None, false) => order.len() - 1,
        };
        self.jump_to_host(order[next]);
    }

    /// The hop the blast lens would start from at the cursor.
    pub fn blast_key_at_cursor(&self) -> Option<HopKey> {
        match self.selection()? {
            MapSel::Hop(n) => Some(self.map.nodes[n].key.clone()),
            MapSel::Host(h) => Some(HopKey::Saved(h)),
            _ => None,
        }
    }

    pub fn filter_active(&self) -> bool {
        self.matched.is_some()
    }

    /// Sorted, deduplicated tags of every host.
    pub fn all_tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = self
            .map
            .hosts
            .iter()
            .flat_map(|h| h.tags.iter().cloned())
            .collect();
        tags.sort();
        tags.dedup();
        tags
    }
}

impl App {
    pub(crate) fn switch_to_map_tab(&mut self) {
        self.active_tab = 5;
        // The tunnel lens and the facts read the tunnel list.
        let _ = self.reload_tunnels();
        if self.map_view.is_none() {
            self.rebuild_map();
        }
    }

    /// Rebuild the map from the current host list, keeping the cursor row, the
    /// filter and the lens.
    pub(crate) fn rebuild_map(&mut self) {
        let hosts = match crate::map::collect_hosts(&self.store, &self.hosts) {
            Ok(h) => h,
            Err(e) => {
                self.map_view = None;
                self.host_notice = Some(format!("map: {e:#}"));
                return;
            }
        };
        let mut view = MapView::new(Map::build(hosts));
        if let Some(old) = self.map_view.take() {
            view.row = old.row;
            view.pos = old.pos;
            view.query = old.query;
            view.editing = old.editing;
            view.apply_filter();
            view.lens = match old.lens {
                Lens::Blast(b) => {
                    let hosts: HashSet<usize> = view.map.blast_radius(&b.key).into_iter().collect();
                    Lens::Blast(BlastLens { key: b.key, hosts })
                }
                other => other,
            };
        }
        self.map_view = Some(view);
    }

    /// Tunnels by the index of the host they run on, for the map.
    pub fn map_tunnels(&self) -> HashMap<usize, Vec<HostTunnel>> {
        let by_id: HashMap<i64, usize> = self
            .hosts
            .iter()
            .enumerate()
            .filter_map(|(i, h)| Some((h.managed_id()?, i)))
            .collect();
        let mut out: HashMap<usize, Vec<HostTunnel>> = HashMap::new();
        for t in &self.tunnels {
            let Some(&idx) = t.host_id.and_then(|id| by_id.get(&id)) else {
                continue;
            };
            let state = if self.tunnel_manager.is_running(t.id) {
                TunnelState::Running
            } else if self.tunnel_manager.is_reconnecting(t.id) {
                TunnelState::Reconnecting
            } else {
                TunnelState::Stopped
            };
            out.entry(idx).or_default().push(HostTunnel {
                id: t.id,
                kind: t.tunnel_type,
                state,
                summary: format!(
                    "{}\u{2192}{}:{}",
                    t.local_port, t.remote_host, t.remote_port
                ),
            });
        }
        out
    }

    pub(crate) fn handle_key_map(&mut self, key: KeyEvent) -> Result<()> {
        self.host_notice = None;

        // The tag picker and the filter line own the keyboard while open.
        if self.map_view.as_ref().is_some_and(|v| v.picker.is_some()) {
            return self.handle_key_map_picker(key);
        }
        if self.map_view.as_ref().is_some_and(|v| v.editing) {
            self.handle_key_map_filter(key);
            return Ok(());
        }

        if self.try_tab_switch(&key)? {
            return Ok(());
        }

        match key.code {
            _ if self.is_action(KeyAction::Quit, &key) => self.request_quit(),
            _ if self.is_action(KeyAction::Help, &key) => self.open_help(),
            _ if self.is_action(KeyAction::MapDown, &key) => self.map_move_row(1),
            _ if self.is_action(KeyAction::MapUp, &key) => self.map_move_row(-1),
            _ if self.is_action(KeyAction::MapRight, &key) => self.map_move_col(1),
            _ if self.is_action(KeyAction::MapLeft, &key) => self.map_move_col(-1),
            _ if self.is_action(KeyAction::Connect, &key) => self.map_connect()?,
            _ if self.is_action(KeyAction::Edit, &key) => self.map_edit()?,
            _ if self.is_action(KeyAction::Search, &key) => {
                if let Some(v) = self.map_view.as_mut() {
                    v.editing = true;
                }
            }
            _ if self.is_action(KeyAction::TagFilter, &key) => {
                if let Some(v) = self.map_view.as_mut() {
                    let items = v.all_tags();
                    if items.is_empty() {
                        self.host_notice = Some("no tags on any host".into());
                    } else {
                        v.picker = Some(TagPicker { items, selected: 0 });
                    }
                }
            }
            _ if self.is_action(KeyAction::Cancel, &key) => {
                if let Some(v) = self.map_view.as_mut() {
                    // Lens first, then the filter.
                    if !matches!(v.lens, Lens::None) {
                        v.lens = Lens::None;
                    } else if !v.query.is_empty() {
                        v.query.clear();
                        v.apply_filter();
                    }
                }
            }
            _ if self.is_action(KeyAction::MapBlast, &key) => self.map_toggle_blast(),
            _ if self.is_action(KeyAction::MapNextDependent, &key) => {
                if let Some(v) = self.map_view.as_mut() {
                    v.blast_step(1);
                }
            }
            _ if self.is_action(KeyAction::MapPrevDependent, &key) => {
                if let Some(v) = self.map_view.as_mut() {
                    v.blast_step(-1);
                }
            }
            _ if self.is_action(KeyAction::Broadcast, &key) => self.map_broadcast(),
            _ if self.is_action(KeyAction::MapTunnelLens, &key) => {
                if let Some(v) = self.map_view.as_mut() {
                    v.lens = match v.lens {
                        Lens::Tunnel => Lens::None,
                        _ => Lens::Tunnel,
                    };
                }
            }
            _ if self.is_action(KeyAction::MapToggleTunnel, &key) => self.map_toggle_tunnel()?,
            _ => {}
        }
        Ok(())
    }

    /// Keys while the filter line is open: typing edits the query live, Enter
    /// keeps it, Esc clears it.
    fn handle_key_map_filter(&mut self, key: KeyEvent) {
        let Some(v) = self.map_view.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                v.query.clear();
                v.editing = false;
            }
            KeyCode::Enter => v.editing = false,
            KeyCode::Backspace => {
                v.query.pop();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                v.query.push(c);
            }
            _ => return,
        }
        v.apply_filter();
    }

    fn handle_key_map_picker(&mut self, key: KeyEvent) -> Result<()> {
        let down = self.is_action(KeyAction::MapDown, &key);
        let up = self.is_action(KeyAction::MapUp, &key);
        let Some(v) = self.map_view.as_mut() else {
            return Ok(());
        };
        let Some(p) = v.picker.as_mut() else {
            return Ok(());
        };
        match key.code {
            KeyCode::Esc => v.picker = None,
            KeyCode::Enter => {
                let tag = p.items[p.selected].clone();
                let term = format!("tag:{tag}");
                let mut terms: Vec<&str> = v.query.split_whitespace().collect();
                if let Some(at) = terms.iter().position(|t| *t == term) {
                    terms.remove(at);
                } else {
                    terms.push(&term);
                }
                v.query = terms.join(" ");
                v.picker = None;
                v.apply_filter();
            }
            _ if down => p.selected = (p.selected + 1).min(p.items.len() - 1),
            _ if up => p.selected = p.selected.saturating_sub(1),
            _ => {}
        }
        Ok(())
    }

    /// `B`: blast radius of the hop under the cursor, or off again.
    fn map_toggle_blast(&mut self) {
        let Some(v) = self.map_view.as_mut() else {
            return;
        };
        if matches!(v.lens, Lens::Blast(_)) {
            v.lens = Lens::None;
            return;
        }
        let Some(key) = v.blast_key_at_cursor() else {
            self.host_notice = Some("blast radius needs a hop or host under the cursor".into());
            return;
        };
        let hosts: HashSet<usize> = v.map.blast_radius(&key).into_iter().collect();
        v.lens = Lens::Blast(BlastLens { key, hosts });
    }

    /// `b` inside the blast lens: broadcast to the dependents.
    fn map_broadcast(&mut self) {
        let Some(v) = self.map_view.as_ref() else {
            return;
        };
        let Lens::Blast(b) = &v.lens else {
            self.host_notice = Some("broadcast on the map works from the blast lens (B)".into());
            return;
        };
        let mut ids: Vec<i64> = b
            .hosts
            .iter()
            .filter_map(|&h| self.hosts.get(h).and_then(|e| e.managed_id()))
            .collect();
        ids.sort_unstable();
        let label = format!("blast: {} ({} hosts)", v.map.key_label(&b.key), ids.len());
        self.open_broadcast_for_hosts(ids, label);
    }

    /// `x`: start or stop the tunnel of the host under the cursor.
    fn map_toggle_tunnel(&mut self) -> Result<()> {
        let host = self.map_view.as_ref().and_then(|v| match v.selection()? {
            MapSel::Host(h) => Some(h),
            MapSel::Hop(n) => match v.map.nodes[n].key {
                HopKey::Saved(h) => Some(h),
                _ => None,
            },
            _ => None,
        });
        let id = host
            .and_then(|h| self.hosts.get(h))
            .and_then(|e| e.managed_id())
            .and_then(|hid| self.tunnels.iter().find(|t| t.host_id == Some(hid)))
            .map(|t| t.id);
        match id {
            Some(id) => {
                self.tunnel_notice = None;
                self.toggle_tunnel_by_id(id)?;
                self.host_notice = self.tunnel_notice.take();
            }
            None => self.host_notice = Some("no tunnel on this host".into()),
        }
        Ok(())
    }

    fn map_move_row(&mut self, delta: isize) {
        if let Some(v) = self.map_view.as_mut() {
            v.move_row(delta);
        }
    }

    fn map_move_col(&mut self, delta: isize) {
        if let Some(v) = self.map_view.as_mut() {
            v.move_col(delta);
        }
    }

    /// Enter: connect through the normal connect path for what the cursor is
    /// on. The hosts of a jump loop are not connectable.
    fn map_connect(&mut self) -> Result<()> {
        let Some(v) = self.map_view.as_ref() else {
            return Ok(());
        };
        let Some(sel) = v.selection() else {
            return Ok(());
        };
        let in_loop = |h: usize| matches!(v.map.chains[h], crate::map::Chain::Cycle(_));
        match sel {
            MapSel::Host(h) if in_loop(h) => {
                self.host_notice = Some("jump loop: not connectable, fix the ProxyJump".into());
            }
            MapSel::Host(h) => self.connect_host_at(h)?,
            MapSel::Hop(n) => match v.map.nodes[n].key.clone() {
                HopKey::Saved(h) | HopKey::ProxyCmd(h) => self.connect_host_at(h)?,
                HopKey::Raw(r) => {
                    let target = crate::app::adhoc::AdhocTarget {
                        user: r.user,
                        host: r.host,
                        port: r.port,
                        ipv6: r.ipv6,
                    };
                    self.connect_adhoc(target)?;
                }
                HopKey::Invalid(t) => {
                    self.host_notice = Some(format!("hop {t:?} is not a valid [user@]host[:port]"));
                }
            },
            MapSel::Cycle(_) => {
                self.host_notice = Some("jump loop: not connectable, edit a host in it (e)".into());
            }
            MapSel::Direct => {
                self.host_notice = Some(format!(
                    "{} hosts connect directly, nothing jumps through them",
                    v.map.direct.len()
                ));
            }
        }
        Ok(())
    }

    /// `e`: open the host form for the host the cursor stands for.
    fn map_edit(&mut self) -> Result<()> {
        let Some(v) = self.map_view.as_ref() else {
            return Ok(());
        };
        let host = match v.selection() {
            Some(MapSel::Host(h)) => Some(h),
            Some(MapSel::Hop(n)) => match v.map.nodes[n].key {
                HopKey::Saved(h) | HopKey::ProxyCmd(h) => Some(h),
                _ => None,
            },
            Some(MapSel::Cycle(i)) => v.map.cycles[i].path.first().copied(),
            _ => None,
        };
        match host {
            Some(h) if self.reveal_host(h) => self.edit_selected_host(),
            _ => {
                self.host_notice = Some("nothing to edit here: not a saved host".into());
                Ok(())
            }
        }
    }
}
