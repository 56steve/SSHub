//! The structured hop editor of the host form (issue #79).
//!
//! The stored value stays the comma separated ProxyJump string in
//! `HostFormEdit::proxy_jump`; this module is a list view and editor on top of
//! it. While the ProxyJump field is focused it takes the keys the list needs
//! (`a r d n X Enter`, `Ctrl+Up/Down`, arrows inside the list) and lets the
//! rest of the form (Tab, Esc, save) through.

use std::collections::HashSet;

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

use super::*;
use crate::map::hops::{self, Route};
use crate::map::{Map, MapHost};

/// Where a picked hop goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickTarget {
    /// Insert after hop `n` (or first, when the list is empty).
    Add(usize),
    /// Replace hop `n`.
    Replace(usize),
}

/// One saved host offered by the picker.
#[derive(Debug, Clone)]
pub struct PickHost {
    pub name: String,
    pub group: String,
    /// `via bastion-eu` or `direct`.
    pub via: String,
    pub forbidden: bool,
}

/// A selectable picker row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickRow {
    /// Index into [`HopPicker::routes`].
    Route(usize),
    /// Index into [`HopPicker::hosts`].
    Host(usize),
    /// `+ raw user@host:port`.
    Raw,
}

pub struct HopPicker {
    pub query: String,
    pub selected: usize,
    pub target: PickTarget,
    pub routes: Vec<Route>,
    pub hosts: Vec<PickHost>,
    pub forbidden: HashSet<String>,
}

impl HopPicker {
    /// Rows that survive the query: routes first, then hosts, then raw.
    pub fn rows(&self) -> Vec<PickRow> {
        let q = self.query.trim();
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(q, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut fits = |text: &str| {
            q.is_empty() || {
                buf.clear();
                pattern
                    .score(Utf32Str::new(text, &mut buf), &mut matcher)
                    .is_some()
            }
        };
        let mut rows = Vec::new();
        for (i, r) in self.routes.iter().enumerate() {
            if fits(&r.tokens.join(" ")) {
                rows.push(PickRow::Route(i));
            }
        }
        for (i, h) in self.hosts.iter().enumerate() {
            if fits(&format!("{} {}", h.name, h.group)) {
                rows.push(PickRow::Host(i));
            }
        }
        rows.push(PickRow::Raw);
        rows
    }
}

/// Raw hop input line.
pub struct RawInput {
    pub text: String,
    pub target: PickTarget,
}

/// Editor state; the value itself lives in the form.
#[derive(Default)]
pub struct HopEditor {
    pub selected: usize,
    pub picker: Option<HopPicker>,
    pub raw: Option<RawInput>,
    /// Last refusal or result, shown under the list.
    pub notice: Option<String>,
    /// The map of every host, built on first use and kept while the form is
    /// open (the picker and the raw-hop loop check both read it).
    map_cache: std::cell::RefCell<Option<Map>>,
}

/// One row of the hop list as drawn.
pub struct HopRowView {
    pub status: char,
    pub name: String,
    pub source: String,
    pub addr: String,
    pub latency: String,
    pub selected: bool,
    pub dim: bool,
}

/// One row of the picker as drawn.
pub struct PickRowView {
    pub text: String,
    pub selected: bool,
    pub dim: bool,
    pub section: Option<&'static str>,
}

/// Everything the form screen needs to draw the hop section.
pub struct HopSection {
    pub title: String,
    pub rows: Vec<HopRowView>,
    pub empty_text: Option<String>,
    pub hint: String,
    pub saves_as: String,
    pub path: String,
    pub picker: Option<PickerView>,
    pub raw: Option<(String, Option<&'static str>)>,
    pub notice: Option<String>,
    /// The chain is inherited, so the list is shown muted.
    pub inherited: bool,
    pub test: Option<TestView>,
    /// `inherited from group "prod/db" \u{b7} add a hop to override`.
    pub head: Option<String>,
    /// `\u{3a3} db-9 would join jump-2's row on the map (4 \u{2192} 5 hosts)`.
    pub join_note: Option<String>,
    /// Width of the widest hop name, so the columns of the list line up.
    pub name_w: usize,
}

/// The editor's test-connect result block.
pub struct TestView {
    pub title: String,
    /// One card per step: (glyph, hop name, outcome text, reason, failed).
    pub rows: Vec<(char, String, String, String, bool)>,
    pub result: String,
    pub hint: String,
}

pub struct PickerView {
    pub title: String,
    pub query: String,
    pub rows: Vec<PickRowView>,
    pub preview: String,
}

/// Rows of the picker shown at once.
const PICKER_ROWS: usize = 9;

impl HopEditor {
    /// The picker is open.
    pub fn is_picking(&self) -> bool {
        self.picker.is_some()
    }

    /// The raw hop input is open.
    pub fn is_typing_raw(&self) -> bool {
        self.raw.is_some()
    }
}

impl App {
    /// The hop-editor section for the open host form, `None` when the
    /// ProxyJump field is not focused or the form is read-only (ssh_config).
    pub fn hop_section(&self, inherited: &crate::store::ResolvedConnection) -> Option<HopSection> {
        let form = self.host_form.as_ref()?;
        if form.field != HostFormField::ProxyJump || form.metadata_only {
            return None;
        }
        let own = form.proxy_jump.trim();
        let direct = hops::split(own).is_empty() && !own.is_empty();
        let using_inherited = own.is_empty();
        let inh = inherited
            .proxy_jump
            .clone()
            .filter(|j| !hops::split(j).is_empty());
        let tokens: Vec<String> = if using_inherited {
            inh.as_deref().map(hops::split).unwrap_or_default()
        } else {
            hops::split(own)
        };
        let sel = self.hop_editor.selected.min(tokens.len().saturating_sub(1));
        let rows: Vec<HopRowView> = tokens
            .iter()
            .enumerate()
            .map(|(i, t)| self.hop_row(t, i == sel && !using_inherited, using_inherited))
            .collect();
        let title = if direct {
            "hops \u{2500} direct (overrides inherited)".to_string()
        } else if using_inherited {
            "hops \u{2500} inherited".to_string()
        } else {
            "hops \u{2500} own value".to_string()
        };
        let empty_text = rows.is_empty().then(|| {
            if direct {
                "direct: no jump, even though a group sets one".to_string()
            } else {
                "no jump".to_string()
            }
        });
        let hint = if using_inherited {
            "a saved host  r raw  n direct  (either replaces the inherited chain)".to_string()
        } else {
            "a saved host  r raw \u{2502} \u{21b5} replace  d remove  Ctrl+\u{2191}\u{2193} move  n direct  X clear \u{2192} inherit"
                .to_string()
        };
        let saves_as = if direct {
            "saves as: none".to_string()
        } else if using_inherited {
            match &inh {
                Some(j) => format!("saves as: (empty) \u{2192} inherits \"{j}\""),
                None => "saves as: (empty)".to_string(),
            }
        } else {
            format!("saves as: {}", hops::join(&tokens))
        };
        let path = {
            let mut p = String::from("\u{25c9} you");
            for t in &tokens {
                p.push_str(&format!(" \u{2501}\u{25b6} {}", self.hop_glyph(t)));
                p.push(' ');
                p.push_str(t);
            }
            let me = if form.name.trim().is_empty() {
                "(this host)"
            } else {
                form.name.trim()
            };
            p.push_str(&format!(" \u{2501}\u{25b6} {me}"));
            p
        };
        let picker = self
            .hop_editor
            .picker
            .as_ref()
            .map(|p| self.picker_view(p, &tokens));
        let raw = self.hop_editor.raw.as_ref().map(|r| {
            let err = match hops::validate_raw(&r.text) {
                Ok(_) => None,
                Err(e) if r.text.trim().is_empty() => {
                    let _ = e;
                    None
                }
                Err(e) => Some(e),
            };
            (r.text.clone(), err)
        });
        let head = using_inherited
            .then(|| {
                inh.as_ref()?;
                let primary = self
                    .groups
                    .iter()
                    .find(|g| form.group_ids.contains(&g.id))
                    .map(|g| g.id);
                let ids: Vec<i64> = form.group_ids.iter().copied().collect();
                let g = self.store.jump_group_for(primary, &ids).ok().flatten()?;
                let path = self
                    .store
                    .group_ancestor_chain(Some(g.id))
                    .ok()?
                    .iter()
                    .rev()
                    .map(|a| a.name.clone())
                    .collect::<Vec<_>>()
                    .join("/");
                Some(format!(
                    "inherited from group \"{path}\" \u{b7} add a hop to override"
                ))
            })
            .flatten();
        // Where the host would land on the map with this chain.
        let join_note = (!tokens.is_empty()).then(|| {
            let me = if form.name.trim().is_empty() {
                "this host".to_string()
            } else {
                form.name.trim().to_string()
            };
            let last = tokens.last().cloned().unwrap_or_default();
            let map = self.editor_map();
            // ssh itself follows the first hop's own ssh_config chain, so the
            // row the host lands on starts with that chain.
            let mut want: Vec<String> = Vec::new();
            if let Some(h) = tokens
                .first()
                .and_then(|t| map.hosts.iter().position(|x| &x.name == t))
                .filter(|&h| map.hosts[h].ssh_config)
            {
                if let crate::map::Chain::Hops(c) = &map.chains[h] {
                    want.extend(c.iter().map(|hop| map.key_label(&hop.key())));
                }
            }
            want.extend(tokens.iter().cloned());
            let at = (0..map.nodes.len()).find(|&n| {
                map.node_path(n)
                    .iter()
                    .map(|&p| map.key_label(&map.nodes[p].key))
                    .eq(want.iter().cloned())
            });
            match at {
                Some(n) => {
                    let before = map.nodes[n].sigma;
                    let there = map
                        .hosts
                        .iter()
                        .position(|h| h.name == me)
                        .is_some_and(|i| map.host_node[i] == Some(n));
                    let after = if there { before } else { before + 1 };
                    format!("\u{3a3} {me} would join {last}'s row on the map ({before} \u{2192} {after} hosts)")
                }
                None => format!("\u{3a3} {me} would start a new row under {last} on the map"),
            }
        });
        let name_w = rows
            .iter()
            .map(|r| r.name.chars().count())
            .max()
            .unwrap_or(0);
        Some(HopSection {
            head,
            join_note,
            name_w,
            title,
            rows,
            empty_text,
            hint,
            saves_as,
            path,
            picker,
            raw,
            notice: self.hop_editor.notice.clone(),
            inherited: using_inherited,
            test: self.test_view(),
        })
    }

    fn test_view(&self) -> Option<TestView> {
        use crate::ssh::test_connect::StepState;
        let run = self
            .test_run
            .as_ref()
            .filter(|r| r.owner == super::test_connect::TestOwner::Editor)?;
        let rows = run
            .steps
            .iter()
            .zip(&run.states)
            .map(|(step, st)| {
                let (glyph, text) = step_text(st);
                let (text, reason) = match st {
                    StepState::Failed { reason, .. } => ("failed".to_string(), reason.clone()),
                    StepState::Skipped { .. } => ("skipped".to_string(), String::new()),
                    _ => (text, String::new()),
                };
                (glyph, step.label.clone(), text, reason, st.is_failure())
            })
            .collect();
        let result = match run.failure() {
            Some((i, reason, detail)) => format!(
                "result \u{2717} failed at hop {} ({}): {reason}{}",
                i + 1,
                run.steps[i].label,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" \u{b7} {detail}")
                }
            ),
            None if run.done
                && run
                    .states
                    .iter()
                    .all(|s| matches!(s, StepState::Ok { .. } | StepState::Skipped { .. })) =>
            {
                "\u{2713} every step answered".to_string()
            }
            None if run.done => "cancelled".to_string(),
            None => "testing\u{2026}".to_string(),
        };
        let hint = if run.running() {
            "Esc cancel"
        } else {
            "H known hosts   t test again   Save still allowed   (P push key and l ssh log are on the map)"
        };
        Some(TestView {
            title: format!("test chain \u{2500} {}", run.headline()),
            rows,
            result,
            hint: hint.to_string(),
        })
    }

    /// Status glyph of a hop token from the local ping readings (unknown when
    /// there is none: ICMP from here says nothing about a hop's SSH).
    fn hop_glyph(&self, token: &str) -> char {
        let samples = self.ping_data.get(token).map(Vec::as_slice);
        match crate::ping::classify_ping(samples) {
            crate::ping::PingClass::Online | crate::ping::PingClass::Slow => '\u{25cf}',
            _ => '\u{25cb}',
        }
    }

    /// Status glyph and latency text of a hop by name, from the local ping.
    fn hop_health(&self, name: &str) -> (char, String) {
        let samples = self.ping_data.get(name).map(Vec::as_slice);
        let ms = match (crate::ping::classify_ping(samples), samples) {
            (crate::ping::PingClass::Online | crate::ping::PingClass::Slow, Some(s)) => {
                s.last().map(|m| format!("{m}ms")).unwrap_or_default()
            }
            _ => String::new(),
        };
        (self.hop_glyph(name), ms)
    }

    fn hop_row(&self, token: &str, selected: bool, dim: bool) -> HopRowView {
        let saved = self.hosts.iter().find(|h| h.name() == token);
        let samples = self.ping_data.get(token).map(Vec::as_slice);
        let latency = match (crate::ping::classify_ping(samples), samples) {
            (crate::ping::PingClass::Online | crate::ping::PingClass::Slow, Some(s)) => {
                s.last().map(|m| format!("{m}ms")).unwrap_or_default()
            }
            _ => String::new(),
        };
        match saved {
            Some(h) => {
                let (user, addr, port) = match h.managed() {
                    Some(m) => (m.username.clone(), m.address.clone(), m.port),
                    None => (None, h.name().to_string(), None),
                };
                let user = user.map(|u| format!("{u}@")).unwrap_or_default();
                let group = h
                    .managed()
                    .and_then(|m| m.group.as_ref())
                    .map(|g| g.name.as_str())
                    .unwrap_or("_ungrouped");
                let kind = if h.source() == HostSource::SshConfig {
                    "ssh_config"
                } else {
                    "saved"
                };
                HopRowView {
                    status: self.hop_glyph(token),
                    name: token.to_string(),
                    source: format!("{kind} \u{b7} {group}"),
                    addr: format!("{user}{addr}:{}", port.unwrap_or(22)),
                    latency,
                    selected,
                    dim,
                }
            }
            None => HopRowView {
                status: ' ',
                name: format!("\u{2506}{token}\u{2506}"),
                source: "raw \u{b7} not a saved host".into(),
                addr: String::new(),
                latency: String::new(),
                selected,
                dim,
            },
        }
    }

    fn picker_view(&self, p: &HopPicker, tokens: &[String]) -> PickerView {
        let rows = p.rows();
        let me = self
            .host_form
            .as_ref()
            .map(|f| f.name.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "this host".into());
        let first = p.selected.saturating_sub(PICKER_ROWS - 1);
        let mut out = Vec::new();
        let mut last_section = None;
        for (i, r) in rows.iter().enumerate().skip(first).take(PICKER_ROWS) {
            let section = match r {
                PickRow::Route(_) => "reuse a route (replaces all hops)",
                PickRow::Host(_) => "one saved host",
                PickRow::Raw => "",
            };
            let sect = (last_section != Some(section) && !section.is_empty()).then_some(section);
            last_section = Some(section);
            let (text, dim) = match r {
                PickRow::Route(ri) => {
                    let route = &p.routes[*ri];
                    let loops = route.tokens.iter().any(|t| p.forbidden.contains(t));
                    let last = route.tokens.last().map(String::as_str).unwrap_or("");
                    let (glyph, ms) = self.hop_health(last);
                    let group = self
                        .hosts
                        .iter()
                        .find(|h| h.name() == last)
                        .and_then(|h| h.managed().and_then(|m| m.group.as_ref()))
                        .map_or("_ungrouped", |g| g.name.as_str());
                    let biggest = p.routes.iter().map(|r| r.hosts).max().unwrap_or(1).max(1);
                    let filled = (route.hosts * 8).div_ceil(biggest).clamp(1, 8);
                    let bar = format!(
                        "\u{2595}{}{}\u{258f}",
                        "\u{2588}".repeat(filled),
                        " ".repeat(8 - filled)
                    );
                    (
                        format!(
                            "{:<30} {glyph} {ms:<6} {:>3} hosts \u{b7} {group:<10} {bar}{}",
                            format!("you \u{203a} {}", route.tokens.join(" \u{203a} ")),
                            route.hosts,
                            if loops { "  \u{21bb} would loop" } else { "" }
                        ),
                        loops,
                    )
                }
                PickRow::Host(hi) => {
                    let h = &p.hosts[*hi];
                    let (glyph, ms) = self.hop_health(&h.name);
                    (
                        format!(
                            "{glyph} {:<14} {:<10} {:<20} {ms}{}",
                            crate::tui::text::ellipsize(&h.name, 14),
                            crate::tui::text::ellipsize(&h.group, 10),
                            crate::tui::text::ellipsize(&h.via, 20),
                            if h.forbidden {
                                "  \u{21bb} would loop"
                            } else {
                                ""
                            }
                        ),
                        h.forbidden,
                    )
                }
                PickRow::Raw => ("+ raw user@host:port   End".to_string(), false),
            };
            out.push(PickRowView {
                text,
                selected: i == p.selected,
                dim,
                section: sect,
            });
        }
        let preview = match rows.get(p.selected) {
            Some(PickRow::Route(ri)) => format!(
                "preview  you \u{2501}\u{25b6} {} \u{2501}\u{25b6} {me}",
                p.routes[*ri].tokens.join(" \u{2501}\u{25b6} ")
            ),
            Some(PickRow::Host(hi)) => {
                let mut t = tokens.to_vec();
                apply(&mut t, p.target, p.hosts[*hi].name.clone());
                format!(
                    "preview  you \u{2501}\u{25b6} {} \u{2501}\u{25b6} {me}",
                    t.join(" \u{2501}\u{25b6} ")
                )
            }
            _ => String::new(),
        };
        let title = match p.target {
            PickTarget::Add(n) if tokens.is_empty() => {
                let _ = n;
                "add the first hop \u{2500} routes first, then hosts".to_string()
            }
            PickTarget::Add(n) => {
                format!("add hop after {} \u{2500} routes first, then hosts", n + 1)
            }
            PickTarget::Replace(n) => {
                format!("replace hop {} \u{2500} routes first, then hosts", n + 1)
            }
        };
        PickerView {
            title,
            query: p.query.clone(),
            rows: out,
            preview,
        }
    }

    fn form_hops(&self) -> Vec<String> {
        self.host_form
            .as_ref()
            .map(|f| hops::split(&f.proxy_jump))
            .unwrap_or_default()
    }

    fn set_form_hops(&mut self, tokens: &[String]) {
        if let Some(f) = self.host_form.as_mut() {
            f.proxy_jump = hops::join(tokens);
            f.cursor = f.proxy_jump.chars().count();
            f.dirty = true;
        }
    }

    /// Handle a key for the hop editor. `true` = consumed; `false` lets the
    /// form's own navigation and save keys act.
    pub(crate) fn handle_key_hop_editor(&mut self, key: &KeyEvent) -> bool {
        let Some(form) = self.host_form.as_ref() else {
            return false;
        };
        if form.field != HostFormField::ProxyJump || form.metadata_only {
            return false;
        }
        if self.hop_editor.picker.is_some() {
            self.hop_picker_key(key);
            return true;
        }
        if self.hop_editor.raw.is_some() {
            self.hop_raw_key(key);
            return true;
        }
        self.hop_editor.notice = None;
        let tokens = self.form_hops();
        let len = tokens.len();
        let sel = self.hop_editor.selected.min(len.saturating_sub(1));
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        match key.code {
            _ if self.is_save_key(key) => return false,
            KeyCode::Esc => return self.cancel_or_clear_test(true),
            KeyCode::Tab | KeyCode::BackTab => return false,
            KeyCode::Char('t') if plain => self.editor_start_test(),
            // The known-hosts manager, then back to this form. Push key and the
            // ssh log need a saved host and the hosts tab: they are on the map.
            _ if self.is_action(KeyAction::KnownHosts, key) => {
                self.known_hosts_return = Some(AppMode::HostForm);
                self.open_known_hosts();
            }
            KeyCode::Up if key.modifiers.is_empty() => {
                if sel > 0 {
                    self.hop_editor.selected = sel - 1;
                    return true;
                }
                return false;
            }
            KeyCode::Down if key.modifiers.is_empty() => {
                if sel + 1 < len {
                    self.hop_editor.selected = sel + 1;
                    return true;
                }
                return false;
            }
            _ if self.is_action(KeyAction::MoveHostUp, key) => {
                if sel > 0 {
                    let mut t = tokens;
                    t.swap(sel, sel - 1);
                    self.set_form_hops(&t);
                    self.hop_editor.selected = sel - 1;
                }
            }
            _ if self.is_action(KeyAction::MoveHostDown, key) => {
                if sel + 1 < len {
                    let mut t = tokens;
                    t.swap(sel, sel + 1);
                    self.set_form_hops(&t);
                    self.hop_editor.selected = sel + 1;
                }
            }
            KeyCode::Enter if plain => {
                let target = if len == 0 {
                    PickTarget::Add(0)
                } else {
                    PickTarget::Replace(sel)
                };
                self.open_hop_picker(target);
            }
            KeyCode::Char('a') if plain => {
                let target = PickTarget::Add(if len == 0 { 0 } else { sel });
                self.open_hop_picker(target);
            }
            KeyCode::Char('r') if plain => {
                let target = PickTarget::Add(if len == 0 { 0 } else { sel });
                self.hop_editor.raw = Some(RawInput {
                    text: String::new(),
                    target,
                });
            }
            KeyCode::Char('d') | KeyCode::Delete if plain => {
                if len > 0 {
                    let mut t = tokens;
                    t.remove(sel);
                    self.set_form_hops(&t);
                    self.hop_editor.selected = sel.saturating_sub(1);
                }
            }
            KeyCode::Char('n') if plain => {
                if let Some(f) = self.host_form.as_mut() {
                    f.proxy_jump = "none".into();
                    f.dirty = true;
                }
                self.hop_editor.selected = 0;
            }
            KeyCode::Char('X') => {
                if let Some(f) = self.host_form.as_mut() {
                    f.proxy_jump.clear();
                    f.dirty = true;
                }
                self.hop_editor.selected = 0;
            }
            // Typing must not leak into the value: the list is the editor.
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Left | KeyCode::Right => {}
            _ => return false,
        }
        true
    }

    /// The map of every host, built once per open form (also while drawing: the
    /// `\u{3a3}` line under the list reads it).
    fn editor_map(&self) -> std::cell::Ref<'_, Map> {
        if self.hop_editor.map_cache.borrow().is_none() {
            let hosts: Vec<MapHost> =
                crate::map::collect_hosts(&self.store, &self.hosts).unwrap_or_default();
            *self.hop_editor.map_cache.borrow_mut() = Some(Map::build(hosts));
        }
        std::cell::Ref::map(self.hop_editor.map_cache.borrow(), |m| {
            m.as_ref().expect("just built")
        })
    }

    fn open_hop_picker(&mut self, target: PickTarget) {
        let edited = self
            .host_form
            .as_ref()
            .map(|f| f.name.trim().to_string())
            .unwrap_or_default();
        let map = self.editor_map().clone();
        let forbidden = hops::forbidden_hops(&map, &edited);
        let routes = hops::routes_in_use(&map, 8);
        let mut hosts: Vec<PickHost> = map
            .hosts
            .iter()
            .enumerate()
            .map(|(i, h)| PickHost {
                name: h.name.clone(),
                group: h.group.clone(),
                via: match &map.chains[i] {
                    crate::map::Chain::Hops(c) if !c.is_empty() => format!(
                        "via {}",
                        c.iter()
                            .map(|hop| map.key_label(&hop.key()))
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                    _ => "direct".into(),
                },
                forbidden: forbidden.contains(&h.name),
            })
            .collect();
        hosts.sort_by(|a, b| a.forbidden.cmp(&b.forbidden).then(a.name.cmp(&b.name)));
        self.hop_editor.picker = Some(HopPicker {
            query: String::new(),
            selected: 0,
            target,
            routes,
            hosts,
            forbidden,
        });
    }

    fn hop_picker_key(&mut self, key: &KeyEvent) {
        let Some(p) = self.hop_editor.picker.as_mut() else {
            return;
        };
        let rows = p.rows();
        match key.code {
            KeyCode::Esc => self.hop_editor.picker = None,
            KeyCode::Up => p.selected = p.selected.saturating_sub(1),
            KeyCode::Down => p.selected = (p.selected + 1).min(rows.len() - 1),
            KeyCode::End => p.selected = rows.len() - 1,
            KeyCode::Backspace => {
                p.query.pop();
                p.selected = 0;
            }
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                p.query.push(c);
                p.selected = 0;
            }
            KeyCode::Enter => {
                let row = rows.get(p.selected).copied();
                self.hop_picker_choose(row);
            }
            _ => {}
        }
        if let Some(p) = self.hop_editor.picker.as_mut() {
            p.selected = p.selected.min(p.rows().len() - 1);
        }
    }

    fn hop_picker_choose(&mut self, row: Option<PickRow>) {
        let Some(p) = self.hop_editor.picker.as_ref() else {
            return;
        };
        let target = p.target;
        match row {
            Some(PickRow::Route(i)) => {
                let route = p.routes[i].tokens.clone();
                if route.iter().any(|t| p.forbidden.contains(t)) {
                    self.hop_editor.notice = Some("that route would loop back to this host".into());
                    return;
                }
                self.set_form_hops(&route);
                self.hop_editor.selected = route.len().saturating_sub(1);
            }
            Some(PickRow::Host(i)) => {
                let h = &p.hosts[i];
                if h.forbidden {
                    self.hop_editor.notice =
                        Some(format!("{} would loop back to this host", h.name));
                    return;
                }
                let name = h.name.clone();
                self.insert_hop(target, name);
            }
            Some(PickRow::Raw) => {
                let text = p.query.clone();
                self.hop_editor.picker = None;
                self.hop_editor.raw = Some(RawInput { text, target });
                return;
            }
            None => return,
        }
        self.hop_editor.picker = None;
    }

    fn hop_raw_key(&mut self, key: &KeyEvent) {
        let Some(r) = self.hop_editor.raw.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.hop_editor.raw = None,
            KeyCode::Backspace => {
                r.text.pop();
            }
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                r.text.push(c);
            }
            KeyCode::Enter => match hops::validate_raw(&r.text) {
                Ok(token) => {
                    let target = r.target;
                    let host_part = crate::map::parse_raw_hop(&token).map(|h| h.host);
                    let edited = self
                        .host_form
                        .as_ref()
                        .map(|f| f.name.trim().to_string())
                        .unwrap_or_default();
                    let loops = match host_part {
                        Some(h) => hops::forbidden_hops(&self.editor_map(), &edited).contains(&h),
                        None => false,
                    };
                    if loops {
                        self.hop_editor.notice =
                            Some("that host would loop back to this host".into());
                    } else {
                        self.insert_hop(target, token);
                    }
                    self.hop_editor.raw = None;
                }
                Err(e) => self.hop_editor.notice = Some(e.to_string()),
            },
            _ => {}
        }
    }

    fn insert_hop(&mut self, target: PickTarget, token: String) {
        let mut t = self.form_hops();
        let at = apply(&mut t, target, token);
        self.set_form_hops(&t);
        self.hop_editor.selected = at;
    }
}

/// Put `token` into `tokens` per `target`; returns the index it now sits at.
fn apply(tokens: &mut Vec<String>, target: PickTarget, token: String) -> usize {
    match target {
        PickTarget::Replace(n) if n < tokens.len() => {
            tokens[n] = token;
            n
        }
        PickTarget::Add(n) => {
            let at = if tokens.is_empty() {
                0
            } else {
                (n + 1).min(tokens.len())
            };
            tokens.insert(at, token);
            at
        }
        PickTarget::Replace(_) => {
            tokens.push(token);
            tokens.len() - 1
        }
    }
}

/// Why a form's ProxyJump cannot be saved, if it cannot.
pub(crate) fn proxy_jump_problem(value: &str) -> Option<String> {
    hops::jump_problem(value)
}

/// Glyph and short text of one test step.
pub(crate) fn step_text(st: &crate::ssh::test_connect::StepState) -> (char, String) {
    use crate::ssh::test_connect::StepState;
    match st {
        StepState::Pending => ('\u{b7}', "waiting".into()),
        StepState::Running => ('\u{bb}', "testing\u{2026}".into()),
        StepState::Ok { ms } => ('\u{2713}', format!("ok {ms}ms")),
        StepState::Failed { reason, .. } => ('\u{2717}', reason.clone()),
        StepState::Skipped { reason } => ('-', reason.clone()),
        StepState::Cancelled => ('-', "cancelled".into()),
    }
}
