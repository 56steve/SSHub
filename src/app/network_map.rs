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
    /// A cluster of root hops, by index in [`Map::clusters`].
    Cluster(usize),
    /// The folded tail of an open cluster.
    More(usize),
}

/// Root hops of an open cluster shown before the tail folds.
pub const CLUSTER_SHOWN: usize = 12;
/// Extra hops one reveal step shows.
pub const REVEAL_STEP: usize = 20;

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

/// `u`: pick the host that should use a hop as its jump.
pub struct UsePicker {
    /// The jump token to apply (host name or `user@host:port`).
    pub token: String,
    pub query: String,
    pub selected: usize,
    /// Add after the host's own hops instead of replacing them.
    pub append: bool,
    /// (host index, would loop), in name order.
    pub hosts: Vec<(usize, bool)>,
}

impl UsePicker {
    /// Hosts that survive the query.
    pub fn visible(&self, map: &Map) -> Vec<(usize, bool)> {
        let q = self.query.to_lowercase();
        self.hosts
            .iter()
            .copied()
            .filter(|(h, _)| q.is_empty() || map.hosts[*h].name.to_lowercase().contains(&q))
            .collect()
    }
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
    /// The map panel takes the whole body (no ribbon, legend or facts).
    pub zoomed: bool,
    /// Set by the renderer: the map is drawn as an outline, which has no room
    /// for the pickers, so they do not open.
    pub outline: std::cell::Cell<bool>,
    pub use_picker: Option<UsePicker>,
    /// Clusters (by name) whose hops are listed.
    pub open_clusters: HashSet<String>,
    /// Extra hops revealed per open cluster, in [`REVEAL_STEP`] units.
    pub revealed: HashMap<String, usize>,
    /// The direct-only bucket is expanded into one strip per group.
    pub direct_open: bool,
    /// Host index by managed id, built with the map (for the tunnel lens).
    pub host_by_id: HashMap<i64, usize>,
    /// Per trie node: some host behind it (or on it) matches the filter.
    match_node: Vec<bool>,
}

impl MapView {
    pub fn new(map: Map) -> Self {
        let mut open_clusters = HashSet::new();
        // The biggest cluster starts open so the tree is visible.
        if let Some(c) = map.clusters.first() {
            open_clusters.insert(c.name.clone());
        }
        let mut view = Self {
            map,
            all_rows: Vec::new(),
            rows: Vec::new(),
            context: HashSet::new(),
            matched: None,
            row: 0,
            pos: 0,
            lens: Lens::None,
            query: String::new(),
            editing: false,
            picker: None,
            zoomed: false,
            outline: std::cell::Cell::new(false),
            use_picker: None,
            open_clusters,
            revealed: HashMap::new(),
            direct_open: false,
            match_node: Vec::new(),
            host_by_id: HashMap::new(),
        };
        view.all_rows = view.build_rows();
        view.rows = view.all_rows.clone();
        view
    }

    pub fn clustered(&self) -> bool {
        !self.map.clusters.is_empty()
    }

    /// Roots of cluster `c` that are listed: the top of the Σ order, every
    /// root a filter matches, and whatever reveal steps added.
    fn shown_roots(&self, c: usize) -> Vec<usize> {
        let cl = &self.map.clusters[c];
        let extra = self.revealed.get(&cl.name).copied().unwrap_or(0);
        let top = CLUSTER_SHOWN + extra * REVEAL_STEP;
        cl.roots
            .iter()
            .enumerate()
            .filter(|(i, &r)| *i < top || self.match_node.get(r).copied().unwrap_or(false))
            .map(|(_, &r)| r)
            .collect()
    }

    /// Every row of the map under the current fold state.
    fn build_rows(&self) -> Vec<Row> {
        fn walk(map: &Map, n: usize, out: &mut Vec<Row>) {
            out.push(Row::Node(n));
            for &c in &map.nodes[n].children {
                walk(map, c, out);
            }
        }
        let map = &self.map;
        let mut rows = Vec::new();
        if map.clusters.is_empty() {
            for &r in &map.roots {
                walk(map, r, &mut rows);
            }
        } else {
            for (ci, c) in map.clusters.iter().enumerate() {
                rows.push(Row::Cluster(ci));
                if self.open_clusters.contains(&c.name) {
                    let shown = self.shown_roots(ci);
                    for &r in &shown {
                        walk(map, r, &mut rows);
                    }
                    if shown.len() < c.roots.len() {
                        rows.push(Row::More(ci));
                    }
                }
            }
        }
        rows.extend((0..map.cycles.len()).map(Row::Cycle));
        if !map.direct.is_empty() {
            rows.push(Row::Direct);
            if self.direct_open {
                rows.extend((0..map.direct_groups.len()).map(Row::DirectGroup));
            }
        }
        rows
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
        self.hop_count(row) + self.map.row_hosts(r).len()
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
            Row::Cluster(i) => {
                if self.pos < hops {
                    MapSel::Cluster(i)
                } else {
                    MapSel::Host(*self.map.row_hosts(r).get(self.pos - hops)?)
                }
            }
            Row::More(i) => MapSel::More(i),
            Row::Direct => MapSel::Direct,
            Row::DirectGroup(_) => {
                if self.pos < hops {
                    MapSel::Direct
                } else {
                    MapSel::Host(*self.map.row_hosts(r).get(self.pos - hops)?)
                }
            }
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

    /// Re-derive the visible rows from `query` and the fold state, keeping the
    /// cursor on what it stood on when that is still shown.
    pub fn apply_filter(&mut self) {
        let before = self.selection();
        let filter = crate::map::MapFilter::parse(&self.query);
        self.context.clear();
        if filter.is_empty() {
            self.matched = None;
            self.match_node.clear();
            self.all_rows = self.build_rows();
            self.rows = self.all_rows.clone();
        } else {
            let matched: HashSet<usize> = filter.matching(&self.map).into_iter().collect();
            // A node matches when a host on it or anywhere behind it does, or
            // when the node is itself a matching host. Parents come before
            // their children, so one reverse pass lifts matches up the tree.
            let map = &self.map;
            let mut match_node: Vec<bool> = map
                .nodes
                .iter()
                .map(|n| {
                    matches!(n.key, HopKey::Saved(h) if matched.contains(&h))
                        || n.cargo
                            .iter()
                            .any(|(_, hs)| hs.iter().any(|h| matched.contains(h)))
                })
                .collect();
            for n in (0..map.nodes.len()).rev() {
                if match_node[n] {
                    if let Some(p) = map.nodes[n].parent {
                        match_node[p] = true;
                    }
                }
            }
            self.match_node = match_node;
            self.matched = Some(matched);
            self.all_rows = self.build_rows();
            let map = &self.map;
            let matched = self.matched.as_ref().expect("just set");
            let strong: Vec<bool> = self
                .all_rows
                .iter()
                .map(|&r| match r {
                    Row::Node(n) => {
                        matches!(map.nodes[n].key, HopKey::Saved(h) if matched.contains(&h))
                            || map.row_hosts(r).iter().any(|h| matched.contains(h))
                    }
                    Row::Cluster(c) => map.clusters[c].roots.iter().any(|&r| self.match_node[r]),
                    Row::More(_) => false,
                    Row::Cycle(i) => {
                        map.cycles[i].path.iter().any(|h| matched.contains(h))
                            || map.row_hosts(r).iter().any(|h| matched.contains(h))
                    }
                    Row::Direct => map.direct.iter().any(|h| matched.contains(h)),
                    Row::DirectGroup(_) => map.row_hosts(r).iter().any(|h| matched.contains(h)),
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

    /// After a filter is kept (Enter): open only the clusters that hold a
    /// match and fold the rest.
    pub fn reveal_matches(&mut self) {
        if !self.clustered() || self.matched.is_none() {
            return;
        }
        self.open_clusters = self
            .map
            .clusters
            .iter()
            .filter(|c| c.roots.iter().any(|&r| self.match_node[r]))
            .map(|c| c.name.clone())
            .collect();
        self.apply_filter();
    }

    /// Space: fold or unfold what the cursor is on.
    pub fn toggle_fold(&mut self) {
        match self.selection() {
            Some(MapSel::Cluster(c)) => {
                let name = self.map.clusters[c].name.clone();
                if !self.open_clusters.remove(&name) {
                    self.open_clusters.insert(name);
                }
            }
            Some(MapSel::More(c)) => {
                *self
                    .revealed
                    .entry(self.map.clusters[c].name.clone())
                    .or_default() += 1;
            }
            Some(MapSel::Direct) => self.direct_open = !self.direct_open,
            _ => return,
        }
        self.apply_filter();
    }

    /// Shift+Z: fold every cluster, or when none is open, expand the
    /// direct-only bucket (and fold it again on the next press).
    pub fn fold_all(&mut self) {
        if !self.open_clusters.is_empty() {
            self.open_clusters.clear();
            self.revealed.clear();
        } else {
            self.direct_open = !self.direct_open;
        }
        self.apply_filter();
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
            Row::Cluster(c) => self.map.clusters[c]
                .roots
                .iter()
                .any(|&r| self.node_in_radius(blast, r)),
            Row::More(_) | Row::Direct | Row::DirectGroup(_) => false,
        }
    }

    /// Whether `node` or anything below it is on the blast radius' path.
    fn node_in_radius(&self, blast: &BlastLens, node: usize) -> bool {
        self.map.nodes[node].key == blast.key
            || self.map.nodes[node]
                .children
                .iter()
                .any(|&c| self.node_in_radius(blast, c))
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
        view.host_by_id = self
            .hosts
            .iter()
            .enumerate()
            .filter_map(|(i, h)| Some((h.managed_id()?, i)))
            .collect();
        if let Some(old) = self.map_view.take() {
            view.row = old.row;
            view.pos = old.pos;
            view.zoomed = old.zoomed;
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
        // The host-id index is built with the map, not every frame.
        let Some(by_id) = self.map_view.as_ref().map(|v| &v.host_by_id) else {
            return HashMap::new();
        };
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

        if self
            .map_view
            .as_ref()
            .is_some_and(|v| v.use_picker.is_some())
        {
            return self.handle_key_map_use(key);
        }
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
                if self.map_view.as_ref().is_some_and(|v| v.outline.get()) {
                    self.host_notice =
                        Some("the tag picker needs a bigger terminal (80x24)".into());
                    return Ok(());
                }
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
                if self.cancel_or_clear_test(false) {
                    return Ok(());
                }
                if let Some(v) = self.map_view.as_mut() {
                    if v.zoomed {
                        v.zoomed = false;
                        return Ok(());
                    }
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
            _ if self.is_action(KeyAction::MapUseAsJump, &key) => self.map_open_use_picker(),
            _ if self.is_action(KeyAction::MapTest, &key) => self.map_start_test(),
            _ if self.is_action(KeyAction::MapCopyCmd, &key) => self.map_copy_command(),
            _ if self.is_action(KeyAction::TogglePanelZoom, &key) => {
                if let Some(v) = self.map_view.as_mut() {
                    v.zoomed = !v.zoomed;
                }
            }
            _ if self.is_action(KeyAction::AddHost, &key) => self.map_save_raw_hop()?,
            _ if self.is_action(KeyAction::KnownHosts, &key) => self.open_known_hosts(),
            _ if self.is_action(KeyAction::PushKey, &key) => self.map_push_key()?,
            _ if self.is_action(KeyAction::MapSshLog, &key) => self.map_show_ssh_log(),
            _ if self.is_action(KeyAction::MapFold, &key) => {
                if let Some(v) = self.map_view.as_mut() {
                    v.toggle_fold();
                }
            }
            _ if self.is_action(KeyAction::MapFoldAll, &key) => {
                if let Some(v) = self.map_view.as_mut() {
                    v.fold_all();
                }
            }
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
            KeyCode::Enter => {
                v.editing = false;
                v.reveal_matches();
                return;
            }
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

    /// `u`: pick the host that will use the hop under the cursor as its jump.
    fn map_open_use_picker(&mut self) {
        let Some(v) = self.map_view.as_mut() else {
            return;
        };
        if v.outline.get() {
            self.host_notice = Some("use as jump needs a bigger terminal (80x24)".into());
            return;
        }
        let hop = match v.selection() {
            Some(MapSel::Hop(n)) => v.map.nodes[n].key.clone(),
            Some(MapSel::Host(h)) => HopKey::Saved(h),
            _ => {
                self.host_notice = Some("put the cursor on a hop or host to use as a jump".into());
                return;
            }
        };
        let (token, own) = match &hop {
            HopKey::Saved(h) => (v.map.hosts[*h].name.clone(), Some(*h)),
            HopKey::Raw(r) => (r.label(), None),
            _ => {
                self.host_notice = Some("that hop cannot be used as a jump".into());
                return;
            }
        };
        // A loop forms when the hop itself depends on the target.
        let depends: HashSet<usize> = own
            .map(|h| {
                v.map
                    .chain_keys(h)
                    .into_iter()
                    .filter_map(|k| match k {
                        HopKey::Saved(s) => Some(s),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut hosts: Vec<(usize, bool)> = (0..v.map.hosts.len())
            .map(|h| (h, Some(h) == own || depends.contains(&h)))
            .collect();
        hosts.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| v.map.hosts[a.0].name.cmp(&v.map.hosts[b.0].name))
        });
        v.use_picker = Some(UsePicker {
            token,
            query: String::new(),
            selected: 0,
            append: false,
            hosts,
        });
    }

    fn handle_key_map_use(&mut self, key: KeyEvent) -> Result<()> {
        let down = self.is_action(KeyAction::MapDown, &key);
        let up = self.is_action(KeyAction::MapUp, &key);
        let Some(v) = self.map_view.as_mut() else {
            return Ok(());
        };
        let Some(p) = v.use_picker.as_mut() else {
            return Ok(());
        };
        let visible = p.visible(&v.map);
        match key.code {
            KeyCode::Esc => v.use_picker = None,
            KeyCode::Tab => p.append = !p.append,
            KeyCode::Backspace => {
                p.query.pop();
                p.selected = 0;
            }
            KeyCode::Enter => {
                let Some(&(h, loops)) = visible.get(p.selected) else {
                    return Ok(());
                };
                if loops {
                    self.host_notice = Some("that would loop: the hop depends on this host".into());
                    return Ok(());
                }
                let token = p.token.clone();
                let append = p.append;
                v.use_picker = None;
                self.map_apply_jump(h, &token, append)?;
            }
            _ if down => p.selected = (p.selected + 1).min(visible.len().saturating_sub(1)),
            _ if up => p.selected = p.selected.saturating_sub(1),
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                p.query.push(c);
                p.selected = 0;
            }
            _ => {}
        }
        Ok(())
    }

    /// Set (or append) `token` as a hop of host `h`'s own ProxyJump and save.
    fn map_apply_jump(&mut self, h: usize, token: &str, append: bool) -> Result<()> {
        let Some(entry) = self.hosts.get(h) else {
            return Ok(());
        };
        let Some(m) = entry.managed().filter(|m| m.source == HostSource::Launcher) else {
            self.host_notice =
                Some("ssh_config hosts are read-only here: edit ~/.ssh/config".into());
            return Ok(());
        };
        let (id, name) = (m.id, m.name.clone());
        let mut tokens = if append {
            // Start from what the host uses now, own or inherited.
            let eff = self
                .store
                .resolve_connection(m)
                .ok()
                .and_then(|r| r.proxy_jump)
                .unwrap_or_default();
            crate::map::hops::split(&eff)
        } else {
            Vec::new()
        };
        if !tokens.iter().any(|t| t == token) {
            tokens.push(token.to_string());
        }
        let value = crate::map::hops::join(&tokens);
        self.store.update_host(
            id,
            &crate::store::HostUpdate {
                proxy_jump: Some(Some(value.clone())),
                ..Default::default()
            },
        )?;
        self.reload_hosts()?;
        self.host_notice = Some(format!("{name}: ProxyJump = {value}"));
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

    /// A click on the map body: select the hop or glyph the last frame drew at
    /// (`x`, `y`); a double click connects to it.
    pub(crate) fn map_click(&mut self, x: u16, y: u16, is_double: bool) -> Result<()> {
        if self
            .map_view
            .as_ref()
            .is_none_or(|v| v.picker.is_some() || v.use_picker.is_some() || v.editing)
        {
            return Ok(());
        }
        let hit = self
            .click_map
            .borrow()
            .iter()
            .rev()
            .find(|t| t.area.contains(ratatui::layout::Position::new(x, y)))
            .and_then(|t| match t.hit {
                ClickHit::MapCell { row, pos } => Some((row, pos)),
                _ => None,
            });
        let Some((row, pos)) = hit else {
            return Ok(());
        };
        if let Some(v) = self.map_view.as_mut() {
            v.row = row;
            v.pos = pos;
            v.clamp();
        }
        if is_double {
            self.map_connect()?;
        }
        Ok(())
    }

    /// The host the failed-test actions are about: the hop that failed, else
    /// what the cursor is on. `None` when neither is a saved host.
    fn map_failed_host(&self) -> Option<usize> {
        let by_name = |n: &str| self.hosts.iter().position(|h| h.name() == n);
        if let Some((i, _, _)) = self.test_run.as_ref().and_then(|r| r.failure()) {
            let step = &self.test_run.as_ref()?.steps[i];
            if let Some(h) = step.host.as_deref().and_then(by_name) {
                return Some(h);
            }
        }
        let v = self.map_view.as_ref()?;
        let h = match v.selection()? {
            MapSel::Host(h) => h,
            MapSel::Hop(n) => match v.map.nodes[n].key {
                HopKey::Saved(h) => h,
                _ => return None,
            },
            _ => return None,
        };
        by_name(&v.map.hosts[h].name)
    }

    /// `P`: push a public key to the failed hop (or the host under the cursor)
    /// through the usual identity picker.
    fn map_push_key(&mut self) -> Result<()> {
        let Some(h) = self.map_failed_host() else {
            self.host_notice = Some("no saved host to push a key to here".into());
            return Ok(());
        };
        if self.hosts[h].managed().is_none() {
            self.host_notice = Some("push key needs a saved host".into());
            return Ok(());
        }
        if !self.reveal_host(h) {
            return Ok(());
        }
        self.trigger_push_key_from_hosts()
    }

    /// `l`: the ssh log of the failed hop on the hosts tab, zoomed.
    fn map_show_ssh_log(&mut self) {
        let Some(h) = self.map_failed_host() else {
            self.host_notice = Some("no saved host to show a log for here".into());
            return;
        };
        if !self.reveal_host(h) {
            return;
        }
        self.active_tab = 0;
        self.focused_panel = PanelId::SshLog;
        self.panel_zoomed = true;
    }

    /// `y`: copy the `ssh -J ...` command of what the cursor is on.
    fn map_copy_command(&mut self) {
        let Some(v) = self.map_view.as_ref() else {
            return;
        };
        let cmd = match v.selection() {
            Some(MapSel::Host(h)) => v.map.ssh_command(h),
            Some(MapSel::Hop(n)) => v.map.node_command(n),
            _ => String::new(),
        };
        if cmd.is_empty() || cmd.starts_with("jump loop") {
            self.host_notice = Some("no command for this row".into());
            return;
        }
        self.host_notice = Some(match write_osc52(&cmd) {
            Ok(()) => format!("copied: {cmd}"),
            Err(e) => format!("clipboard copy failed: {e}"),
        });
    }

    /// `a` on a raw hop: open the new-host form filled from it.
    fn map_save_raw_hop(&mut self) -> Result<()> {
        let raw = self.map_view.as_ref().and_then(|v| match v.selection()? {
            MapSel::Hop(n) => match &v.map.nodes[n].key {
                HopKey::Raw(r) => Some(r.clone()),
                _ => None,
            },
            _ => None,
        });
        let Some(raw) = raw else {
            self.host_notice = Some(
                "a saves a raw hop as a host: put the cursor on one (add hosts on the hosts tab)"
                    .into(),
            );
            return Ok(());
        };
        self.enter_host_form(None, false)?;
        if let Some(f) = self.host_form.as_mut() {
            f.address = raw.host.clone();
            f.name = raw.host.clone();
            f.username = raw.user.unwrap_or_default();
            f.port = raw.port.map(|p| p.to_string()).unwrap_or_default();
            f.dirty = true;
        }
        Ok(())
    }

    /// Wheel over the map: move the cursor row (the view follows it).
    pub(crate) fn map_wheel(&mut self, delta: isize) {
        self.map_move_row(delta);
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
        if matches!(sel, MapSel::Cluster(_) | MapSel::More(_)) {
            if let Some(v) = self.map_view.as_mut() {
                v.toggle_fold();
            }
            return Ok(());
        }
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
            MapSel::Cluster(_) | MapSel::More(_) => {}
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
