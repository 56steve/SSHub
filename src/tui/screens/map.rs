//! Network map tab (issue #79, variant D): hops as a left-to-right tree, hosts
//! as status glyphs behind their last hop, a path ribbon and facts below.
//!
//! Layout and copy follow `docs/network-map/mockups/D*.txt`. Every colour is an
//! existing theme role. Status is honest about its source: `●`/`●` come from the
//! local ICMP ping (reachability from this machine, not through the jump), so a
//! missing reply reads as unknown (`○`), never as a down hop.

use std::collections::{BTreeMap, HashMap, HashSet};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::*;
use unicode_width::UnicodeWidthStr;

use crate::app::network_map::{MapSel, MapView};
use crate::app::App;
use crate::map::{Chain, Hop, HopKey, HopKind, JumpSource, Map, Row, PROXY_CMD_LABEL};
use crate::theme::catalog::{ColorRole, StyleRole};
use crate::theme::model::ResolvedTheme;

/// Most glyphs one group shows before `+n`.
const MAX_GLYPHS: usize = 10;

/// Terminal cells a string takes.
fn cells(s: &str) -> u16 {
    s.width().min(u16::MAX as usize) as u16
}

/// Write `s` at (`x`, `y`), clipped at `max_x` (exclusive); returns the x after it.
fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style, max_x: u16) -> u16 {
    if x >= max_x {
        return x;
    }
    let (nx, _) = buf.set_stringn(x, y, s, (max_x - x) as usize, style);
    nx
}

#[derive(Clone, Copy)]
struct Styles {
    text: Style,
    bright: Style,
    muted: Style,
    dim: Style,
    accent: Style,
    ok: Style,
    warn: Style,
    err: Style,
    info: Style,
    unknown: Style,
    selected: Style,
}

impl Styles {
    fn of(theme: &ResolvedTheme) -> Self {
        let fg = |r| Style::default().fg(theme.color(r));
        Self {
            text: theme.style(StyleRole::TextPrimary),
            bright: theme.style(StyleRole::TextBright),
            muted: theme.style(StyleRole::TextMuted),
            dim: theme.style(StyleRole::TextDim),
            accent: theme.style(StyleRole::FocusIndicator),
            ok: fg(ColorRole::StatusSuccess),
            warn: fg(ColorRole::StatusWarning),
            err: fg(ColorRole::StatusError),
            info: fg(ColorRole::StatusInfo),
            unknown: fg(ColorRole::StatusUnknown),
            selected: theme.style(StyleRole::SelectionActive),
        }
    }
}

/// Status of one host as drawn: glyph, style and latency when known.
struct Status {
    glyph: char,
    style: Style,
    ms: Option<u32>,
}

fn host_status(app: &App, st: &Styles, host: usize) -> Status {
    let Some(view) = app.map_view.as_ref() else {
        return Status {
            glyph: '○',
            style: st.unknown,
            ms: None,
        };
    };
    let name = &view.map.hosts[host].name;
    if app
        .sessions
        .iter()
        .any(|s| !s.local_shell && s.config.host_name == *name)
    {
        return Status {
            glyph: '▶',
            style: st.accent.add_modifier(Modifier::BOLD),
            ms: None,
        };
    }
    let samples = app.ping_data.get(name).map(Vec::as_slice);
    match crate::ping::classify_ping(samples) {
        crate::ping::PingClass::Online => Status {
            glyph: '●',
            style: st.ok,
            ms: samples.and_then(|s| s.last().copied()),
        },
        crate::ping::PingClass::Slow => Status {
            glyph: '●',
            style: st.warn,
            ms: samples.and_then(|s| s.last().copied()),
        },
        // No reply is not a verdict: ICMP can be filtered, and a host behind a
        // jump is often not pingable from here. Only a failed test connect
        // shows `×`.
        _ => Status {
            glyph: '○',
            style: st.unknown,
            ms: None,
        },
    }
}

/// `ms` text for a latency, empty when unknown.
fn ms_text(ms: Option<u32>) -> String {
    ms.map(|m| format!("{m}ms")).unwrap_or_default()
}

/// Layout constants: columns are offsets inside the map panel.
#[derive(Clone, Copy)]
struct Cols {
    hop1: u16,
    hop2: u16,
    cargo: u16,
    /// Hop-column step: names are clipped to fit it.
    step: u16,
    latency: bool,
}

impl Cols {
    fn for_width(w: u16) -> Self {
        if w >= 120 {
            Self {
                hop1: 11,
                hop2: 38,
                cargo: 65,
                step: 27,
                latency: true,
            }
        } else {
            Self {
                hop1: 9,
                hop2: 30,
                cargo: 51,
                step: 21,
                latency: false,
            }
        }
    }

    fn trunk1(&self) -> u16 {
        self.hop1 - 3
    }

    fn trunk2(&self) -> u16 {
        self.hop2 - 3
    }
}

pub fn render_map(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let st = Styles::of(theme);
    if area.width < 40 || area.height < 12 {
        let buf = frame.buffer_mut();
        put(
            buf,
            area.x + 1,
            area.y + area.height / 2,
            "map needs 40x12, press 1 for hosts",
            st.muted,
            area.x + area.width,
        );
        return;
    }
    let Some(view) = app.map_view.as_ref() else {
        put(
            frame.buffer_mut(),
            area.x + 1,
            area.y + 1,
            "the map could not be built",
            st.err,
            area.x + area.width,
        );
        return;
    };

    let full = area.width >= 120 && area.height >= 36;
    let (ribbon_h, bottom_h) = if full {
        (10, 6)
    } else if area.height >= 18 {
        (4, 0)
    } else {
        (0, 0)
    };
    let map_h = area.height - ribbon_h - bottom_h;
    let map_area = Rect::new(area.x, area.y, area.width, map_h);
    let ribbon_area = Rect::new(area.x, area.y + map_h, area.width, ribbon_h);
    let bottom_area = Rect::new(area.x, area.y + map_h + ribbon_h, area.width, bottom_h);

    let buf = frame.buffer_mut();
    let sel = view.selection();
    let title = "map \u{2500} hops as a tree \u{b7} hosts as glyphs behind their last hop";
    let inner = panel(buf, map_area, title, st.accent, st.bright);
    draw_map(buf, inner, app, view, &st);

    if ribbon_h > 0 {
        let ribbon_title = format!("path ribbon \u{2500} {}", sel_title(view, sel));
        let inner = panel(buf, ribbon_area, &ribbon_title, st.dim, st.bright);
        if full {
            draw_ribbon_full(buf, inner, app, view, sel, &st);
        } else {
            draw_ribbon_line(buf, inner, app, view, sel, &st);
        }
    }
    if bottom_h > 0 {
        let left_w = (bottom_area.width / 2).min(79);
        let legend = Rect::new(bottom_area.x, bottom_area.y, left_w, bottom_h);
        let facts = Rect::new(
            bottom_area.x + left_w + 1,
            bottom_area.y,
            bottom_area.width.saturating_sub(left_w + 1),
            bottom_h,
        );
        let inner = panel(buf, legend, "legend", st.dim, st.bright);
        draw_legend(buf, inner, &st);
        let facts_title = format!("node \u{2500} {}", sel_title(view, sel));
        let inner = panel(buf, facts, &facts_title, st.dim, st.bright);
        draw_facts(buf, inner, view, sel, &st);
    }
}

/// A bordered panel with a title; returns the inner rect.
fn panel(buf: &mut Buffer, area: Rect, title: &str, border: Style, title_style: Style) -> Rect {
    let r = area.x + area.width - 1;
    let b = area.y + area.height - 1;
    for x in area.x + 1..r {
        buf.set_string(x, area.y, "\u{2500}", border);
        buf.set_string(x, b, "\u{2500}", border);
    }
    for y in area.y + 1..b {
        buf.set_string(area.x, y, "\u{2502}", border);
        buf.set_string(r, y, "\u{2502}", border);
    }
    buf.set_string(area.x, area.y, "\u{250c}", border);
    buf.set_string(r, area.y, "\u{2510}", border);
    buf.set_string(area.x, b, "\u{2514}", border);
    buf.set_string(r, b, "\u{2518}", border);
    let t = crate::tui::text::ellipsize(title, area.width.saturating_sub(6) as usize);
    put(buf, area.x + 2, area.y, &format!(" {t} "), title_style, r);
    Rect::new(
        area.x + 1,
        area.y + 1,
        area.width - 2,
        area.height.saturating_sub(2),
    )
}

fn sel_title(view: &MapView, sel: Option<MapSel>) -> String {
    match sel {
        Some(MapSel::Hop(n)) => view.map.key_label(&view.map.nodes[n].key),
        Some(MapSel::Host(h)) => match view.map.chains[h] {
            Chain::Cycle(_) => format!("{} \u{b7} cycle", view.map.hosts[h].name),
            _ => view.map.hosts[h].name.clone(),
        },
        Some(MapSel::Cycle(i)) => format!(
            "{} \u{b7} cycle",
            view.map.hosts[view.map.cycles[i].path[0]].name
        ),
        Some(MapSel::Direct) => "direct-only".into(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------- the tree

/// What one hop node shows.
struct NodeLook {
    text: String,
    style: Style,
    /// Edge fill: `\u{2500}` for a real hop, `\u{2504}` for a ProxyCommand.
    dashed: bool,
}

fn node_look(
    app: &App,
    view: &MapView,
    st: &Styles,
    cols: &Cols,
    node: usize,
    shared: &HashSet<HopKey>,
) -> NodeLook {
    let n = &view.map.nodes[node];
    let room = cols.step.saturating_sub(5) as usize;
    match &n.key {
        HopKey::Saved(h) => {
            let s = host_status(app, st, *h);
            let name =
                crate::tui::text::ellipsize(&view.map.hosts[*h].name, room.saturating_sub(7));
            let mut text = format!("{} {}", s.glyph, name);
            if cols.latency {
                if let Some(ms) = s.ms {
                    text.push_str(&format!("  {}", ms_text(Some(ms))));
                }
            }
            if shared.contains(&n.key) {
                text.push_str(" \u{25c7}");
            }
            NodeLook {
                text,
                style: st.text,
                dashed: false,
            }
        }
        HopKey::Raw(r) => NodeLook {
            text: format!(
                "\u{2506}{}\u{2506}",
                crate::tui::text::ellipsize(&r.label(), room.saturating_sub(2))
            ),
            style: st.muted,
            dashed: false,
        },
        HopKey::ProxyCmd(_) => NodeLook {
            text: format!("\u{2039}{PROXY_CMD_LABEL}\u{203a}"),
            style: st.info,
            dashed: true,
        },
        HopKey::Invalid(t) => NodeLook {
            text: format!(
                "\u{2506}{}\u{2506}",
                crate::tui::text::ellipsize(t, room.saturating_sub(2))
            ),
            style: st.err,
            dashed: false,
        },
    }
}

/// Group strip for `hosts` by group: `group \u{25cf}\u{25cf}\u{25cb}   group2 \u{25cf}`.
/// Returns the x after it and the x range of the selected host's glyph.
#[allow(clippy::too_many_arguments)]
fn draw_strip(
    buf: &mut Buffer,
    mut x: u16,
    y: u16,
    max_x: u16,
    app: &App,
    view: &MapView,
    st: &Styles,
    groups: &[(String, Vec<usize>)],
    selected: Option<usize>,
) -> u16 {
    for (gi, (group, hosts)) in groups.iter().enumerate() {
        if gi > 0 {
            x += 3;
        }
        if x >= max_x {
            break;
        }
        x = put(buf, x, y, group, st.muted, max_x);
        x = put(buf, x, y, " ", st.muted, max_x);
        // Keep the selected glyph on screen when the group is longer than the cap.
        let start = selected
            .and_then(|s| hosts.iter().position(|&h| h == s))
            .map_or(0, |p| (p + 1).saturating_sub(MAX_GLYPHS));
        let shown = hosts.iter().skip(start).take(MAX_GLYPHS);
        for &h in shown {
            let s = host_status(app, st, h);
            let style = if selected == Some(h) {
                st.selected
            } else {
                s.style
            };
            x = put(buf, x, y, &s.glyph.to_string(), style, max_x);
        }
        let hidden = hosts.len().saturating_sub(MAX_GLYPHS);
        if hidden > 0 {
            x = put(buf, x, y, &format!("+{hidden}"), st.muted, max_x);
        }
    }
    let _ = view;
    x
}

fn draw_map(buf: &mut Buffer, inner: Rect, app: &App, view: &MapView, st: &Styles) {
    if inner.height < 4 {
        return;
    }
    let cols = Cols::for_width(inner.width + 2);
    let right = inner.x + inner.width;
    let sigma_x = right.saturating_sub(5);
    let cx = |c: u16| inner.x + c;

    // Header.
    let hy = inner.y;
    put(buf, cx(1), hy, "you", st.muted, right);
    put(buf, cx(cols.hop1), hy, "hop 1", st.muted, right);
    put(buf, cx(cols.hop2), hy, "hop 2", st.muted, right);
    put(
        buf,
        cx(cols.cargo),
        hy,
        "hosts behind this hop \u{b7} one glyph each",
        st.muted,
        sigma_x,
    );
    put(buf, sigma_x + 3, hy, "\u{3a3}", st.muted, right);

    let map = &view.map;
    let sel = view.selection();
    let sel_host = match sel {
        Some(MapSel::Host(h)) => Some(h),
        _ => None,
    };
    let path_nodes: HashSet<usize> = match sel {
        Some(MapSel::Hop(n)) => map.node_path(n).into_iter().collect(),
        Some(MapSel::Host(h)) => map.host_node[h]
            .map(|n| map.node_path(n).into_iter().collect())
            .unwrap_or_default(),
        _ => HashSet::new(),
    };
    let mut counts: HashMap<&HopKey, usize> = HashMap::new();
    for n in &map.nodes {
        *counts.entry(&n.key).or_default() += 1;
    }
    let shared: HashSet<HopKey> = counts
        .into_iter()
        .filter(|(_, c)| *c > 1)
        .map(|(k, _)| k.clone())
        .collect();

    // Top-level items own the left trunk: root nodes and loops.
    let top: Vec<usize> = view
        .rows
        .iter()
        .enumerate()
        .filter(|(_, r)| match r {
            Row::Node(n) => map.nodes[*n].depth == 0,
            Row::Cycle(_) => true,
            Row::Direct => false,
        })
        .map(|(i, _)| i)
        .collect();
    let last_top = top.last().copied();

    // One blank row between top-level items when everything fits.
    let visible = inner.height.saturating_sub(2) as usize;
    let blanks = top.len().saturating_sub(1) + usize::from(map.rows().last() == Some(&Row::Direct));
    let spacious = view.rows.len() + blanks <= visible;
    let mut ys: Vec<usize> = Vec::with_capacity(view.rows.len());
    let mut y = 0usize;
    for (i, r) in view.rows.iter().enumerate() {
        let starts_item = top.contains(&i) || matches!(r, Row::Direct);
        if spacious && starts_item && i > 0 {
            y += 1;
        }
        ys.push(y);
        y += 1;
    }
    let sel_y = ys.get(view.row).copied().unwrap_or(0);
    let offset = (sel_y + 1).saturating_sub(visible);
    let row_y = |i: usize| -> Option<u16> {
        let v = ys[i].checked_sub(offset)?;
        (v < visible).then(|| inner.y + 2 + v as u16)
    };

    // Vertical trunks first, so a connector glyph drawn later wins.
    if let (Some(&first), Some(&last)) = (top.first(), top.last()) {
        let lo = ys[first].saturating_sub(offset);
        let hi = ys[last].saturating_sub(offset);
        for v in lo..=hi.min(visible.saturating_sub(1)) {
            put(
                buf,
                cx(cols.trunk1()),
                inner.y + 2 + v as u16,
                "\u{2502}",
                st.dim,
                right,
            );
        }
    }

    for (i, &row) in view.rows.iter().enumerate() {
        let Some(y) = row_y(i) else {
            continue;
        };
        let row_selected = i == view.row;
        match row {
            Row::Node(n) => {
                let node = &map.nodes[n];
                let look = node_look(app, view, st, &cols, n, &shared);
                let on_path = path_nodes.contains(&n);
                let edge_style = if on_path { st.accent } else { st.dim };
                let fill = if look.dashed { "\u{2504}" } else { "\u{2500}" };
                let col = if node.depth == 0 {
                    cols.hop1
                } else {
                    cols.hop2
                };
                // Connector into the node.
                if node.depth == 0 {
                    let first = top.first() == Some(&i);
                    let last = last_top == Some(i);
                    let joint = match (first, last) {
                        (true, true) => "\u{2500}",
                        (true, false) => "\u{252c}",
                        (false, true) => "\u{2514}",
                        _ => "\u{251c}",
                    };
                    if first {
                        put(
                            buf,
                            cx(1),
                            y,
                            "\u{25c9} you",
                            st.accent.add_modifier(Modifier::BOLD),
                            right,
                        );
                        put(buf, cx(7), y, "\u{2500}\u{2500}", edge_style, right);
                    }
                    put(buf, cx(cols.trunk1()), y, joint, edge_style, right);
                    put(buf, cx(cols.trunk1() + 1), y, fill, edge_style, right);
                    put(buf, cx(cols.trunk1() + 2), y, "\u{25b8}", edge_style, right);
                } else {
                    let last_child = node
                        .parent
                        .map(|p| map.nodes[p].children.last() == Some(&n))
                        .unwrap_or(true);
                    let joint = if last_child { "\u{2514}" } else { "\u{251c}" };
                    put(buf, cx(cols.trunk2()), y, joint, edge_style, right);
                    put(buf, cx(cols.trunk2() + 1), y, fill, edge_style, right);
                    put(buf, cx(cols.trunk2() + 2), y, "\u{25b8}", edge_style, right);
                }
                // Folded middle hops: only two hop columns exist.
                let mut text = look.text.clone();
                if node.depth >= 2 {
                    text = format!("\u{2039}+{}\u{203a} {text}", node.depth - 1);
                }
                let mut style = if on_path { st.accent } else { look.style };
                let hop_selected = row_selected && matches!(sel, Some(MapSel::Hop(h)) if h == n);
                if hop_selected {
                    style = st.selected;
                }
                let end = put(buf, cx(col), y, &text, style, sigma_x);
                // Edge from the node to the cargo column.
                if !node.children.is_empty() {
                    for x in end + 1..cx(cols.trunk2()) {
                        put(buf, x, y, fill, edge_style, right);
                    }
                    put(buf, cx(cols.trunk2()), y, "\u{252c}", edge_style, right);
                    for x in cx(cols.trunk2()) + 1..cx(cols.cargo).saturating_sub(1) {
                        put(buf, x, y, "\u{2508}", st.dim, right);
                    }
                } else {
                    for x in end + 1..cx(cols.cargo).saturating_sub(1) {
                        put(
                            buf,
                            x,
                            y,
                            if look.dashed { "\u{2504}" } else { "\u{2508}" },
                            st.dim,
                            right,
                        );
                    }
                }
                let end = draw_strip(
                    buf,
                    cx(cols.cargo),
                    y,
                    sigma_x,
                    app,
                    view,
                    st,
                    &node.cargo,
                    sel_host.filter(|_| row_selected),
                );
                if let (true, Some(h)) = (row_selected, sel_host) {
                    put(
                        buf,
                        end + 1,
                        y,
                        &format!("\u{25c2} {}", map.hosts[h].name),
                        st.accent,
                        sigma_x,
                    );
                }
                let sigma = format!("{:>4}", node.sigma);
                put(buf, sigma_x, y, &sigma, st.bright, right);
                // Child trunks: a vertical line from this row to its last child.
                if !node.children.is_empty() {
                    draw_child_trunk(buf, inner, &cols, view, &ys, offset, visible, i, n, st);
                }
            }
            Row::Cycle(ci) => {
                let c = &map.cycles[ci];
                let last = last_top == Some(i);
                let first = top.first() == Some(&i);
                if first {
                    put(
                        buf,
                        cx(1),
                        y,
                        "\u{25c9} you",
                        st.accent.add_modifier(Modifier::BOLD),
                        right,
                    );
                    put(buf, cx(7), y, "\u{2500}\u{2500}", st.dim, right);
                }
                put(
                    buf,
                    cx(cols.trunk1()),
                    y,
                    if last { "\u{2558}" } else { "\u{255e}" },
                    st.err,
                    right,
                );
                put(
                    buf,
                    cx(cols.trunk1() + 1),
                    y,
                    "\u{2550}\u{2550}",
                    st.err,
                    right,
                );
                let names: Vec<&str> = c.path.iter().map(|&h| map.hosts[h].name.as_str()).collect();
                let path = format!("\u{21bb} {}", names.join(" \u{2550}\u{2550}\u{25b8} "));
                let path =
                    crate::tui::text::ellipsize(&path, (cols.cargo - cols.hop1) as usize - 1);
                let style = if row_selected && matches!(sel, Some(MapSel::Cycle(_))) {
                    st.selected
                } else {
                    st.err
                };
                let end = put(buf, cx(cols.hop1), y, &path, style, sigma_x);
                for x in end + 1..cx(cols.cargo).saturating_sub(1) {
                    put(buf, x, y, "\u{2508}", st.dim, right);
                }
                let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                for &h in &c.hosts {
                    groups
                        .entry(map.hosts[h].group.clone())
                        .or_default()
                        .push(h);
                }
                let groups: Vec<(String, Vec<usize>)> = groups.into_iter().collect();
                let end = draw_strip(
                    buf,
                    cx(cols.cargo),
                    y,
                    sigma_x,
                    app,
                    view,
                    st,
                    &groups,
                    sel_host.filter(|_| row_selected),
                );
                put(
                    buf,
                    end + 3,
                    y,
                    "cycle \u{b7} not connectable",
                    st.err,
                    sigma_x,
                );
                put(
                    buf,
                    sigma_x,
                    y,
                    &format!("{:>4}", c.hosts.len()),
                    st.bright,
                    right,
                );
            }
            Row::Direct => {
                put(buf, cx(1), y, "\u{22ef}", st.muted, right);
                let style = if row_selected { st.selected } else { st.muted };
                put(
                    buf,
                    cx(cols.hop1),
                    y,
                    "direct-only \u{b7} no jump, nothing behind them",
                    style,
                    cx(cols.cargo),
                );
                let mut groups: BTreeMap<&str, usize> = BTreeMap::new();
                for &h in &map.direct {
                    *groups.entry(map.hosts[h].group.as_str()).or_default() += 1;
                }
                let mut x = cx(cols.cargo);
                for (gi, (g, n)) in groups.iter().enumerate() {
                    if gi > 0 {
                        x += 3;
                    }
                    x = put(buf, x, y, &format!("{g} {n}"), st.muted, sigma_x);
                }
                put(
                    buf,
                    sigma_x,
                    y,
                    &format!("{:>4}", map.direct.len()),
                    st.bright,
                    right,
                );
            }
        }
    }
}

/// Vertical trunk under a node that has children, down to its last child row.
#[allow(clippy::too_many_arguments)]
fn draw_child_trunk(
    buf: &mut Buffer,
    inner: Rect,
    cols: &Cols,
    view: &MapView,
    ys: &[usize],
    offset: usize,
    visible: usize,
    row: usize,
    node: usize,
    st: &Styles,
) {
    let map = &view.map;
    // Last descendant row of `node`: rows are depth-first, so it is the last
    // consecutive row whose node sits below `node`.
    let mut last = row;
    for (j, r) in view.rows.iter().enumerate().skip(row + 1) {
        match r {
            Row::Node(m) if map.node_path(*m).contains(&node) => last = j,
            _ => break,
        }
    }
    let x = inner.x + cols.trunk2();
    for &yj in &ys[row + 1..last] {
        let Some(v) = yj.checked_sub(offset) else {
            continue;
        };
        if v < visible {
            let y = inner.y + 2 + v as u16;
            let cell = buf.cell((x, y)).map(|c| c.symbol().to_string());
            if cell.as_deref() == Some(" ") {
                buf.set_string(x, y, "\u{2502}", st.dim);
            }
        }
    }
}

// ---------------------------------------------------------- ribbon, facts

/// One card of the path ribbon.
struct Card {
    title: String,
    status: Option<usize>,
    name: String,
    addr: String,
    line3: String,
    border: Style,
}

fn source_text(source: &JumpSource) -> String {
    match source {
        JumpSource::Own => "own ProxyJump".into(),
        JumpSource::Inherited { group } => format!("inherited \u{b7} group \"{group}\""),
        JumpSource::SshConfig => "ssh_config".into(),
    }
}

fn host_addr(map: &Map, h: usize) -> String {
    let host = &map.hosts[h];
    let user = host
        .user
        .as_deref()
        .map(|u| format!("{u}@"))
        .unwrap_or_default();
    format!("{user}{}:{}", host.address, host.port)
}

fn hop_card(map: &Map, hop: &Hop, idx: usize, st: &Styles) -> Card {
    let title = format!("hop {}", idx + 1);
    match &hop.kind {
        HopKind::Saved(h) => Card {
            title,
            status: Some(*h),
            name: map.hosts[*h].name.clone(),
            addr: host_addr(map, *h),
            line3: source_text(&hop.source),
            border: st.dim,
        },
        HopKind::Raw(r) => Card {
            title,
            status: None,
            name: format!("\u{2506}{}\u{2506}", r.label()),
            addr: String::new(),
            line3: "raw \u{b7} not a saved host".into(),
            border: st.dim,
        },
        HopKind::ProxyCommand(_) => Card {
            title,
            status: None,
            name: format!("\u{2039}{PROXY_CMD_LABEL}\u{203a}"),
            addr: String::new(),
            line3: "ssh_config, opaque".into(),
            border: st.dim,
        },
        HopKind::Invalid(t) => Card {
            title,
            status: None,
            name: t.clone(),
            addr: String::new(),
            line3: "invalid hop".into(),
            border: st.err,
        },
    }
}

fn target_card(map: &Map, h: usize, st: &Styles, border_focus: Style) -> Card {
    let _ = st;
    Card {
        title: "target".into(),
        status: Some(h),
        name: map.hosts[h].name.clone(),
        addr: host_addr(map, h),
        line3: "target".into(),
        border: border_focus,
    }
}

/// Cards for the cursor: hops from the root, then the target.
fn cards_for(view: &MapView, sel: Option<MapSel>, st: &Styles) -> (Vec<Card>, bool) {
    let map = &view.map;
    let chain_cards = |hops: &[Hop]| -> Vec<Card> {
        hops.iter()
            .enumerate()
            .map(|(i, h)| hop_card(map, h, i, st))
            .collect()
    };
    match sel {
        Some(MapSel::Hop(n)) => {
            let mut cards = Vec::new();
            for (i, p) in map.node_path(n).into_iter().enumerate() {
                let last = p == n;
                let key = &map.nodes[p].key;
                let mut c = match key {
                    HopKey::Saved(h) if last => target_card(map, *h, st, st.accent),
                    HopKey::Saved(h) => Card {
                        title: format!("hop {}", i + 1),
                        status: Some(*h),
                        name: map.hosts[*h].name.clone(),
                        addr: host_addr(map, *h),
                        line3: String::new(),
                        border: st.dim,
                    },
                    other => Card {
                        title: if last {
                            "target".into()
                        } else {
                            format!("hop {}", i + 1)
                        },
                        status: None,
                        name: map.key_label(other),
                        addr: String::new(),
                        line3: String::new(),
                        border: if last { st.accent } else { st.dim },
                    },
                };
                if let HopKey::Saved(h) = key {
                    if !last {
                        c.line3 = hop_source_in_path(map, n, i, *h);
                    }
                }
                cards.push(c);
            }
            (cards, false)
        }
        Some(MapSel::Host(h)) => match &map.chains[h] {
            Chain::Hops(hops) => {
                let mut cards = chain_cards(hops);
                cards.push(target_card(map, h, st, st.accent));
                (cards, false)
            }
            Chain::Cycle(path) => (cycle_cards(map, path, st), true),
        },
        Some(MapSel::Cycle(i)) => (cycle_cards(map, &map.cycles[i].path, st), true),
        _ => (Vec::new(), false),
    }
}

/// Where the hop at `idx` of `node`'s path comes from, read from the host
/// that routes through it: any host whose chain runs through that prefix.
fn hop_source_in_path(map: &Map, node: usize, idx: usize, hop_host: usize) -> String {
    let path = map.node_path(node);
    let prefix: Vec<HopKey> = path[..=idx]
        .iter()
        .map(|&p| map.nodes[p].key.clone())
        .collect();
    for (h, chain) in map.chains.iter().enumerate() {
        if let Chain::Hops(hops) = chain {
            if hops.len() > idx && hops[..=idx].iter().map(Hop::key).eq(prefix.iter().cloned()) {
                return source_text(&hops[idx].source);
            }
        }
        let _ = h;
    }
    let _ = hop_host;
    String::new()
}

fn cycle_cards(map: &Map, path: &[usize], st: &Styles) -> Vec<Card> {
    path.iter()
        .enumerate()
        .map(|(i, &h)| Card {
            title: format!("hop {}", i + 1),
            status: Some(h),
            name: map.hosts[h].name.clone(),
            addr: host_addr(map, h),
            line3: match &map.hosts[h].jump {
                Some(j) => format!("own ProxyJump = {j}"),
                None => String::new(),
            },
            border: st.err,
        })
        .collect()
}

const CARD_W: u16 = 30;
const CARD_H: u16 = 5;
const ARROW_W: u16 = 6;

fn draw_ribbon_full(
    buf: &mut Buffer,
    inner: Rect,
    app: &App,
    view: &MapView,
    sel: Option<MapSel>,
    st: &Styles,
) {
    let right = inner.x + inner.width;
    let Some(sel) = sel else {
        return;
    };
    let (mut cards, is_cycle) = cards_for(view, Some(sel), st);
    if cards.is_empty() {
        let msg = match sel {
            MapSel::Direct => format!(
                "{} hosts connect directly: no jump, and nothing jumps through them.",
                view.map.direct.len()
            ),
            _ => String::new(),
        };
        put(buf, inner.x + 1, inner.y + 1, &msg, st.muted, right);
        return;
    }
    let avail = inner.width.saturating_sub(12);
    let fit = ((avail + ARROW_W) / (CARD_W + ARROW_W)).max(1) as usize;
    let hidden = cards.len().saturating_sub(fit);
    if hidden > 0 {
        cards.drain(..hidden);
    }
    let arrow_style = if is_cycle { st.err } else { st.dim };
    let (bar, head) = if is_cycle {
        ("\u{2550}", "\u{25b6}")
    } else {
        ("\u{2501}", "\u{25b6}")
    };
    let top = inner.y + 1;
    let mid = top + 2;
    let mut x = inner.x + 1;
    x = put(
        buf,
        x,
        mid,
        "\u{25c9} you",
        st.accent.add_modifier(Modifier::BOLD),
        right,
    );
    x = put(
        buf,
        x,
        mid,
        &format!(" {bar}{bar}{bar}{head} "),
        arrow_style,
        right,
    );
    if hidden > 0 {
        put(
            buf,
            inner.x + 1,
            inner.y,
            &format!("\u{2039} +{hidden} hops \u{203a}"),
            st.info.add_modifier(Modifier::BOLD),
            right,
        );
    }
    let count = cards.len();
    for (i, c) in cards.iter().enumerate() {
        draw_card(buf, x, top, c, app, st, right);
        x += CARD_W;
        if i + 1 < count {
            x = put(
                buf,
                x,
                mid,
                &format!(" {bar}{bar}{bar}{head} "),
                arrow_style,
                right,
            );
        }
    }
    if is_cycle {
        put(buf, x + 1, mid, "\u{21bb} repeats", st.err, right);
    }
    put(
        buf,
        inner.x + 1,
        inner.y + inner.height - 1,
        &ribbon_hint(sel),
        st.muted,
        right,
    );
}

fn ribbon_hint(sel: MapSel) -> String {
    match sel {
        MapSel::Hop(_) => "\u{2192} walks right along the row: hop, then its glyphs, one host per step.  \u{21b5} connects.".into(),
        MapSel::Host(_) => "\u{21b5} connects through this path.".into(),
        MapSel::Cycle(_) => "jump loop: not connectable. e edits a host in it.".into(),
        MapSel::Direct => String::new(),
    }
}

fn draw_card(buf: &mut Buffer, x: u16, y: u16, c: &Card, app: &App, st: &Styles, max_x: u16) {
    let w = CARD_W;
    let b = c.border;
    let title = format!("\u{2500} {} ", c.title);
    put(buf, x, y, "\u{250c}", b, max_x);
    put(buf, x + 1, y, &title, b, max_x);
    for xx in x + 1 + cells(&title)..x + w - 1 {
        put(buf, xx, y, "\u{2500}", b, max_x);
    }
    put(buf, x + w - 1, y, "\u{2510}", b, max_x);
    for yy in y + 1..y + CARD_H - 1 {
        put(buf, x, yy, "\u{2502}", b, max_x);
        put(buf, x + w - 1, yy, "\u{2502}", b, max_x);
    }
    put(buf, x, y + CARD_H - 1, "\u{2514}", b, max_x);
    for xx in x + 1..x + w - 1 {
        put(buf, xx, y + CARD_H - 1, "\u{2500}", b, max_x);
    }
    put(buf, x + w - 1, y + CARD_H - 1, "\u{2518}", b, max_x);

    let inner_w = (w - 4) as usize;
    let mut name_x = x + 2;
    let mut ms = String::new();
    if let Some(h) = c.status {
        let s = host_status(app, st, h);
        name_x = put(buf, name_x, y + 1, &format!("{} ", s.glyph), s.style, max_x);
        ms = ms_text(s.ms);
    }
    let name_room = inner_w
        .saturating_sub(if c.status.is_some() { 2 } else { 0 })
        .saturating_sub(ms.len() + 1);
    put(
        buf,
        name_x,
        y + 1,
        &crate::tui::text::ellipsize(&c.name, name_room),
        st.bright,
        max_x,
    );
    if !ms.is_empty() {
        put(buf, x + w - 2 - cells(&ms), y + 1, &ms, st.muted, max_x);
    }
    put(
        buf,
        x + 2,
        y + 2,
        &crate::tui::text::ellipsize(&c.addr, inner_w),
        st.text,
        max_x,
    );
    put(
        buf,
        x + 2,
        y + 3,
        &crate::tui::text::ellipsize(&c.line3, inner_w),
        st.muted,
        max_x,
    );
}

/// Compact ribbon: one line `you > a > b > target` plus the source line.
fn draw_ribbon_line(
    buf: &mut Buffer,
    inner: Rect,
    app: &App,
    view: &MapView,
    sel: Option<MapSel>,
    st: &Styles,
) {
    let right = inner.x + inner.width;
    let Some(sel) = sel else {
        return;
    };
    let (cards, is_cycle) = cards_for(view, Some(sel), st);
    let arrow = if is_cycle {
        " \u{2550}\u{25b6} "
    } else {
        " \u{2501}\u{25b6} "
    };
    let arrow_style = if is_cycle { st.err } else { st.dim };
    let mut x = put(
        buf,
        inner.x + 1,
        inner.y,
        "\u{25c9} you",
        st.accent.add_modifier(Modifier::BOLD),
        right,
    );
    for c in &cards {
        x = put(buf, x, inner.y, arrow, arrow_style, right);
        if let Some(h) = c.status {
            let s = host_status(app, st, h);
            x = put(buf, x, inner.y, &format!("{} ", s.glyph), s.style, right);
        }
        x = put(buf, x, inner.y, &c.name, st.bright, right);
    }
    if let Some(last) = cards.last() {
        if let Some(h) = last.status {
            let s = host_status(app, st, h);
            if let Some(ms) = s.ms {
                x = put(buf, x, inner.y, &format!("  {}ms", ms), st.muted, right);
            }
            if s.glyph == '\u{25b6}' {
                put(buf, x, inner.y, " \u{25b6}", st.accent, right);
            }
        }
    }
    if inner.height > 1 {
        let note = match sel {
            MapSel::Host(h) | MapSel::Cycle(h) if matches!(sel, MapSel::Host(_)) => {
                jump_note(&view.map, h)
            }
            MapSel::Hop(n) => match view.map.nodes[n].key {
                HopKey::Saved(h) => jump_note(&view.map, h),
                _ => String::new(),
            },
            MapSel::Cycle(_) => "jump loop: not connectable".into(),
            _ => String::new(),
        };
        put(buf, inner.x + 1, inner.y + 1, &note, st.muted, right);
    }
}

/// `jump inherited from group "prod/db"` for a host.
fn jump_note(map: &Map, h: usize) -> String {
    let host = &map.hosts[h];
    match (&host.jump, &host.jump_source) {
        (Some(_), Some(JumpSource::Inherited { group })) => {
            format!("jump inherited from group \"{group}\"")
        }
        (Some(_), Some(JumpSource::SshConfig)) => "jump from ssh_config".into(),
        (Some(_), _) => "jump set on this host".into(),
        _ => "reached directly, no jump".into(),
    }
}

fn draw_legend(buf: &mut Buffer, inner: Rect, st: &Styles) {
    let right = inner.x + inner.width;
    let line = |buf: &mut Buffer, y: u16, parts: &[(&str, Style)]| {
        let mut x = inner.x + 1;
        for (s, style) in parts {
            x = put(buf, x, inner.y + y, s, *style, right);
        }
    };
    line(
        buf,
        0,
        &[
            ("\u{25cf} up ", st.ok),
            ("\u{25cf} slow ", st.warn),
            ("\u{d7} test failed ", st.err),
            ("\u{25cb} unknown ", st.unknown),
            ("\u{25b6} session", st.accent),
        ],
    );
    line(
        buf,
        1,
        &[(
            "\u{2500}\u{25b8} jump edge   \u{2508}\u{2508} \"hosts behind\" leader   \u{3a3} all hosts behind, any depth",
            st.muted,
        )],
    );
    line(
        buf,
        2,
        &[
            ("\u{2506}raw\u{2506}  ", st.muted),
            ("\u{2039}proxy cmd\u{203a} \u{2504}\u{2504}  ", st.info),
            ("\u{21bb} \u{2550}\u{2550} cycle  ", st.err),
            ("\u{25c7} reused hop", st.info),
        ],
    );
    line(
        buf,
        3,
        &[(
            "\u{25cf} is ICMP from this machine, not through the jump",
            st.dim,
        )],
    );
}

fn draw_facts(buf: &mut Buffer, inner: Rect, view: &MapView, sel: Option<MapSel>, st: &Styles) {
    let right = inner.x + inner.width;
    let map = &view.map;
    let mut rows: Vec<(&str, String)> = Vec::new();
    let host_rows = |h: usize, rows: &mut Vec<(&str, String)>| {
        let host = &map.hosts[h];
        let tags: String = host.tags.iter().map(|t| format!(" #{t}")).collect();
        rows.push((
            "address",
            format!("{}  \u{b7}  group {}{tags}", host_addr(map, h), host.group),
        ));
        rows.push(("jump", jump_note(map, h)));
    };
    match sel {
        Some(MapSel::Hop(n)) => {
            let node = &map.nodes[n];
            match &node.key {
                HopKey::Saved(h) => {
                    host_rows(*h, &mut rows);
                    rows.push(("carries", format!("{} hosts behind this hop", node.sigma)));
                }
                other => {
                    rows.push(("hop", map.key_label(other)));
                    rows.push(("carries", format!("{} hosts behind this hop", node.sigma)));
                }
            }
            rows.push(("command", ssh_command(map, n_target(map, n))));
        }
        Some(MapSel::Host(h)) => {
            host_rows(h, &mut rows);
            rows.push(("command", ssh_command(map, Some(h))));
        }
        Some(MapSel::Cycle(i)) => {
            let names: Vec<&str> = map.cycles[i]
                .path
                .iter()
                .map(|&h| map.hosts[h].name.as_str())
                .collect();
            rows.push(("loop", names.join(" \u{2192} ")));
            rows.push((
                "hosts",
                format!("{} stuck behind it", map.cycles[i].hosts.len()),
            ));
        }
        Some(MapSel::Direct) => {
            rows.push(("direct", format!("{} hosts, no jump", map.direct.len())));
        }
        None => {}
    }
    for (i, (k, v)) in rows.iter().take(inner.height as usize).enumerate() {
        let y = inner.y + i as u16;
        let x = put(buf, inner.x + 1, y, &format!("{k:<8}"), st.muted, right);
        put(buf, x + 1, y, v, st.text, right);
    }
}

fn n_target(map: &Map, node: usize) -> Option<usize> {
    match map.nodes[node].key {
        HopKey::Saved(h) | HopKey::ProxyCmd(h) => Some(h),
        _ => None,
    }
}

/// `ssh -J a,b target`, hop names as the map shows them.
fn ssh_command(map: &Map, target: Option<usize>) -> String {
    let Some(h) = target else {
        return String::new();
    };
    match &map.chains[h] {
        Chain::Hops(hops) if hops.is_empty() => format!("ssh {}", map.hosts[h].name),
        Chain::Hops(hops) => {
            let jump: Vec<String> = hops
                .iter()
                .filter_map(|hop| match &hop.kind {
                    HopKind::Saved(s) => Some(map.hosts[*s].name.clone()),
                    HopKind::Raw(r) => Some(r.label()),
                    HopKind::ProxyCommand(_) | HopKind::Invalid(_) => None,
                })
                .collect();
            if jump.is_empty() {
                format!("ssh {}", map.hosts[h].name)
            } else {
                format!("ssh -J {} {}", jump.join(","), map.hosts[h].name)
            }
        }
        Chain::Cycle(_) => "jump loop: not connectable".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::MapHost;
    use crate::test_support::{find_text, frame_at, resolved_default, themed_app};

    fn host(name: &str, group: &str, jump: Option<&str>) -> MapHost {
        MapHost {
            name: name.into(),
            address: format!("{name}.example"),
            user: Some("ops".into()),
            port: 22,
            group: group.into(),
            jump: jump.map(String::from),
            jump_source: jump.map(|_| JumpSource::Own),
            ..Default::default()
        }
    }

    fn app_with(hosts: Vec<MapHost>) -> App {
        let mut app = themed_app(resolved_default());
        app.map_view = Some(MapView::new(Map::build(hosts)));
        app.active_tab = 5;
        app
    }

    fn fleet() -> Vec<MapHost> {
        let mut iot = host("iot-1", "iot", None);
        iot.proxy_command = true;
        iot.ssh_config = true;
        vec![
            host("bastion-eu", "prod", None),
            host("jump-2", "prod", Some("bastion-eu")),
            host("web-1", "prod", Some("bastion-eu")),
            host("db-1", "prod/db", Some("jump-2")),
            host("bastion-us", "us", None),
            host("db-us-2", "us", Some("bastion-us,jump-2")),
            host("legacy-1", "legacy", Some("ops@203.0.113.7:2222")),
            iot,
            host("loop-a", "lab", Some("loop-b")),
            host("loop-b", "lab", Some("loop-a")),
            host("standalone", "staging", None),
        ]
    }

    fn render(app: &App, w: u16, h: u16) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        frame_at(area, |f| render_map(f, area, app))
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect::<String>()
    }

    fn all_text(buf: &Buffer) -> String {
        (0..buf.area.height)
            .map(|y| row_text(buf, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn default_screen_shows_hops_cargo_sigma_and_special_rows() {
        let app = app_with(fleet());
        let buf = render(&app, 160, 45);
        let text = all_text(&buf);
        for needle in [
            "hosts behind this hop",
            "\u{25c9} you",
            "bastion-eu",
            "bastion-us",
            "jump-2",
            "\u{25c7}", // jump-2 sits under two parents
            "\u{2506}ops@203.0.113.7:2222\u{2506}",
            "\u{2039}proxy cmd\u{203a}",
            "\u{21bb} loop-a",
            "cycle \u{b7} not connectable",
            "direct-only",
            "path ribbon",
            "legend",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        // The ProxyCommand text is never shown.
        assert!(!text.contains("cloudflared"));
        // Σ of bastion-eu: web-1, jump-2 and db-1 behind it.
        let (_, y) = find_text(&buf, "bastion-eu");
        assert!(row_text(&buf, y)
            .trim_end_matches([' ', '\u{2502}'])
            .ends_with('3'));
    }

    #[test]
    fn cursor_walks_hop_then_glyphs_and_ribbon_follows() {
        let mut app = app_with(fleet());
        // Down to the jump-2 row under bastion-eu, right to its glyph.
        {
            let v = app.map_view.as_mut().unwrap();
            v.move_row(1);
            assert!(matches!(v.selection(), Some(MapSel::Hop(_))));
        }
        let buf = render(&app, 160, 45);
        assert!(all_text(&buf).contains("path ribbon \u{2500} "));
        let v = app.map_view.as_mut().unwrap();
        v.move_col(1);
        v.move_col(1);
        assert!(matches!(v.selection(), Some(MapSel::Host(_))));
        let buf = render(&app, 160, 45);
        let text = all_text(&buf);
        assert!(text.contains("\u{25c2} db-1"), "{text}");
        assert!(text.contains("target"));
    }

    #[test]
    fn tiny_area_shows_the_size_hint_instead_of_panicking() {
        let app = app_with(fleet());
        let buf = render(&app, 39, 11);
        assert!(all_text(&buf).contains("map needs 40x12"));
        // Compact sizes draw without a ribbon panel but do not panic.
        for (w, h) in [(40, 12), (100, 30), (80, 24), (120, 36), (200, 60)] {
            let _ = render(&app, w, h);
        }
    }

    #[test]
    fn ping_down_is_unknown_never_a_failed_hop() {
        let mut app = app_with(fleet());
        app.ping_data
            .insert("bastion-eu".into(), vec![crate::ping::PING_UNREACHABLE]);
        let buf = render(&app, 160, 45);
        let (x, y) = find_text(&buf, "bastion-eu");
        assert_eq!(buf.cell((x - 2, y)).unwrap().symbol(), "\u{25cb}");
    }

    #[test]
    fn empty_map_renders() {
        let app = app_with(Vec::new());
        let buf = render(&app, 160, 45);
        assert!(all_text(&buf).contains("hosts behind this hop"));
    }
}
