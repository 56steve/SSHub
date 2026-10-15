//! Network map tab (issue #79): cursor, navigation and actions over
//! [`crate::map::Map`]. The model is pure; this is the stateful side.

use super::*;
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

/// The map model plus the cursor. `pos` walks along the selected row's path:
/// first each hop from the root down to the row's own node, then its host
/// glyphs, left to right.
pub struct MapView {
    pub map: Map,
    pub rows: Vec<Row>,
    pub row: usize,
    pub pos: usize,
}

impl MapView {
    pub fn new(map: Map) -> Self {
        let rows = map.rows();
        Self {
            map,
            rows,
            row: 0,
            pos: 0,
        }
    }

    /// Hops on the path to `row`'s node (0 for a loop or the direct bucket).
    pub fn hop_count(&self, row: usize) -> usize {
        match self.rows.get(row) {
            Some(Row::Node(n)) => self.map.nodes[*n].depth + 1,
            Some(Row::Cycle(_)) => 1,
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
}

impl App {
    pub(crate) fn switch_to_map_tab(&mut self) {
        self.active_tab = 5;
        if self.map_view.is_none() {
            self.rebuild_map();
        }
    }

    /// Rebuild the map from the current host list, keeping the cursor row.
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
        if let Some(old) = &self.map_view {
            view.row = old.row;
            view.pos = old.pos;
            view.clamp();
        }
        self.map_view = Some(view);
    }

    pub(crate) fn handle_key_map(&mut self, key: KeyEvent) -> Result<()> {
        self.host_notice = None;

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
            _ => {}
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
