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

use crate::app::network_map::{HostTunnel, Lens, MapSel, MapView, Tint, TunnelState, REVEAL_STEP};
use crate::app::test_connect::{TestOwner, TestRun};
use crate::app::{App, ClickHit};
use crate::map::{Chain, Hop, HopKey, HopKind, JumpSource, Map, Row, PROXY_CMD_LABEL};
use crate::theme::catalog::{ColorRole, StyleRole};
use crate::theme::model::ResolvedTheme;

/// Most glyphs one group shows before `+n`.
/// Glyphs one group shows before `+n` (6 under 120 columns).
fn max_glyphs(width: u16) -> usize {
    if width >= 120 {
        10
    } else {
        6
    }
}

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

/// The test run that belongs to what the cursor is on.
fn map_test(app: &App, sel: Option<MapSel>) -> Option<&TestRun> {
    let sel = sel?;
    app.test_run
        .as_ref()
        .filter(|r| matches!(&r.owner, TestOwner::Map(s) if *s == sel))
}

/// Put each step's outcome on the matching ribbon card.
fn apply_test(cards: &mut [Card], run: &TestRun, st: &Styles) {
    if cards.len() != run.states.len() {
        return;
    }
    for (c, state) in cards.iter_mut().zip(&run.states) {
        let (glyph, text) = crate::app::hop_editor::step_text(state);
        c.line3 = format!("{glyph} {text}");
        if state.is_failure() {
            c.border = st.err;
        }
    }
}

/// Everything a frame of the map reads.
struct Ctx<'a> {
    app: &'a App,
    view: &'a MapView,
    st: &'a Styles,
    /// Tunnels by host index (tunnel lens, facts).
    tunnels: HashMap<usize, Vec<HostTunnel>>,
    max_glyphs: usize,
    test: Option<&'a TestRun>,
}

impl Ctx<'_> {
    /// The glyph and style of one host under the filter, lens and row tint.
    fn glyph(&self, host: usize, tint: Tint) -> (char, Style) {
        let st = self.st;
        let base = host_status(self.app, st, host);
        let dim = ('\u{b7}', st.dim);
        if let Some(over) = self.test_glyph(host) {
            return over;
        }
        if self
            .view
            .matched
            .as_ref()
            .is_some_and(|m| !m.contains(&host))
        {
            return dim;
        }
        match &self.view.lens {
            Lens::Blast(b) => {
                return if b.hosts.contains(&host) {
                    (base.glyph, st.warn)
                } else {
                    (base.glyph, st.dim)
                };
            }
            Lens::Tunnel => {
                return match self.tunnels.get(&host) {
                    Some(t) => ('\u{21c4}', tunnel_style(st, t[0].state)),
                    None => dim,
                };
            }
            Lens::None => {}
        }
        if tint == Tint::Dim {
            return (base.glyph, st.dim);
        }
        (base.glyph, base.style)
    }
}

impl Ctx<'_> {
    /// `»` while a test connect runs on this host, `×` after one failed on it.
    /// A hop is only ever shown down because a test of it failed.
    fn test_glyph(&self, host: usize) -> Option<(char, Style)> {
        use crate::ssh::test_connect::StepState;
        let run = self.test?;
        let name = &self.view.map.hosts[host].name;
        run.steps
            .iter()
            .zip(&run.states)
            .find(|(step, _)| step.host.as_deref() == Some(name.as_str()))
            .and_then(|(_, state)| match state {
                StepState::Running => Some(('\u{bb}', self.st.warn)),
                StepState::Failed { .. } => Some(('\u{d7}', self.st.err)),
                _ => None,
            })
    }
}

fn tunnel_style(st: &Styles, state: TunnelState) -> Style {
    match state {
        TunnelState::Running => st.ok,
        TunnelState::Reconnecting => st.warn,
        TunnelState::Stopped => st.muted,
    }
}

fn tunnel_letter(kind: crate::store::TunnelType) -> char {
    match kind {
        crate::store::TunnelType::Local => 'L',
        crate::store::TunnelType::Remote => 'R',
        crate::store::TunnelType::Dynamic => 'D',
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
    let (ribbon_h, bottom_h) = if view.zoomed {
        (0, 0)
    } else if full {
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

    let ctx = Ctx {
        app,
        view,
        st: &st,
        tunnels: app.map_tunnels(),
        max_glyphs: max_glyphs(area.width),
        test: map_test(app, view.selection()),
    };
    view.outline.set(area.width < 80 || area.height < 24);
    if view.outline.get() {
        draw_outline(frame.buffer_mut(), area, &ctx);
        return;
    }
    let buf = frame.buffer_mut();
    let sel = view.selection();
    let title = map_title(&ctx);
    let inner = panel(buf, map_area, &title, st.accent, st.bright);
    draw_map(buf, inner, &ctx);
    if let Some(p) = &view.picker {
        draw_tag_picker(buf, inner, p, &st);
    }
    if let Some(p) = &view.use_picker {
        draw_use_picker(buf, inner, view, p, &st);
    }

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
        draw_facts(buf, inner, &ctx, sel);
    }
}

/// Below 80x24 the tree is an outline: one hop per row, depth as indent, the
/// selected path on the last line (cut from the left when it is too long).
fn draw_outline(buf: &mut Buffer, area: Rect, ctx: &Ctx) {
    let (view, st) = (ctx.view, ctx.st);
    let right = area.x + area.width;
    let map = &view.map;
    let list_h = area.height.saturating_sub(2) as usize;
    let title = crate::tui::text::ellipsize(&map_title(ctx), area.width as usize - 1);
    put(buf, area.x, area.y, &title, st.bright, right);
    let first = (view.row + 1).saturating_sub(list_h);
    for (k, &row) in view.rows.iter().enumerate().skip(first).take(list_h) {
        let y = area.y + 1 + (k - first) as u16;
        let (indent, text, n) = match row {
            Row::Node(n) => {
                let node = &map.nodes[n];
                let label = match &node.key {
                    HopKey::Saved(h) => {
                        let g = host_status(ctx.app, st, *h).glyph;
                        format!("{g} {}", map.hosts[*h].name)
                    }
                    other => map.key_label(other),
                };
                (node.depth + 1, label, node.sigma)
            }
            Row::Cluster(c) => (
                0,
                format!(
                    "{} {}",
                    if view.open_clusters.contains(&map.clusters[c].name) {
                        '\u{25be}'
                    } else {
                        '\u{25b8}'
                    },
                    map.clusters[c].name
                ),
                map.clusters[c].sigma,
            ),
            Row::More(c) => (1, "\u{2026} more hops".into(), map.clusters[c].roots.len()),
            Row::Cycle(i) => (
                0,
                format!("\u{21bb} loop {}", map.hosts[map.cycles[i].path[0]].name),
                map.cycles[i].hosts.len(),
            ),
            Row::Direct => (0, "\u{22ef} direct-only".into(), map.direct.len()),
            Row::DirectGroup(g) => (
                1,
                map.direct_groups[g].0.clone(),
                map.direct_groups[g].1.len(),
            ),
        };
        let style = if k == view.row {
            st.selected
        } else {
            match view.tint(k) {
                Tint::Dim => st.dim,
                Tint::Warn => st.warn,
                Tint::Normal => st.text,
            }
        };
        let tree = if indent == 0 {
            String::new()
        } else {
            format!("{}\u{251c} ", "  ".repeat(indent - 1))
        };
        let line = format!("{tree}{text}");
        let room = (area.width as usize).saturating_sub(6);
        put(
            buf,
            area.x,
            y,
            &crate::tui::text::ellipsize(&line, room),
            style,
            right,
        );
        put(
            buf,
            right.saturating_sub(5),
            y,
            &format!("{n:>4}"),
            st.bright,
            right,
        );
    }
    // Selected path, cut from the left.
    let (cards, _) = cards_for(view, view.selection(), st);
    let mut path = String::from("you");
    for c in &cards {
        path.push_str(" \u{203a} ");
        path.push_str(&c.name);
    }
    let w = area.width as usize;
    let shown = if path.chars().count() > w {
        let tail: String = path
            .chars()
            .rev()
            .take(w.saturating_sub(1))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("\u{2026}{tail}")
    } else {
        path
    };
    put(
        buf,
        area.x,
        area.y + area.height - 1,
        &shown,
        st.accent,
        right,
    );
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
        Some(MapSel::Cluster(c)) => format!("cluster {}", view.map.clusters[c].name),
        Some(MapSel::More(c)) => format!("more hops in {}", view.map.clusters[c].name),
        None => String::new(),
    }
}

/// The map panel title: the active lens, then the filter.
fn map_title(ctx: &Ctx) -> String {
    let view = ctx.view;
    if let Some(run) = ctx.test {
        return format!(
            "map \u{2500} testing path to {} \u{b7} {}",
            run.title,
            run.headline()
        );
    }
    let mut title = match &view.lens {
        Lens::Blast(b) => format!(
            "map \u{2500} blast radius \u{b7} {} hosts depend on {}",
            b.hosts.len(),
            view.map.key_label(&b.key)
        ),
        Lens::Tunnel => {
            "map \u{2500} tunnel lens \u{b7} \u{21c4} on hops and hosts that carry a tunnel".into()
        }
        Lens::None => {
            "map \u{2500} hops as a tree \u{b7} hosts as glyphs behind their last hop".into()
        }
    };
    if view.editing || view.filter_active() {
        let shown = view
            .matched
            .as_ref()
            .map_or(view.map.hosts.len(), HashSet::len);
        title.push_str(&format!(
            " \u{b7} / {}{} \u{b7} {shown} of {} hosts",
            view.query,
            if view.editing { "\u{258f}" } else { "" },
            view.map.hosts.len()
        ));
    }
    title
}

/// `#` tag picker, centred over the map.
fn draw_tag_picker(
    buf: &mut Buffer,
    inner: Rect,
    p: &crate::app::network_map::TagPicker,
    st: &Styles,
) {
    let rows = p
        .items
        .len()
        .min(inner.height.saturating_sub(4) as usize)
        .max(1);
    let w = 36.min(inner.width.saturating_sub(2));
    let area = Rect::new(
        inner.x + (inner.width - w) / 2,
        inner.y + 1,
        w,
        rows as u16 + 2,
    );
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            buf.set_string(x, y, " ", st.text);
        }
    }
    let body = panel(buf, area, "tag filter", st.accent, st.bright);
    let first = p.selected.saturating_sub(rows - 1);
    for (i, tag) in p.items.iter().skip(first).take(rows).enumerate() {
        let style = if first + i == p.selected {
            st.selected
        } else {
            st.text
        };
        let text = format!(" #{tag}");
        put(
            buf,
            body.x,
            body.y + i as u16,
            &format!("{text:<w$}", w = body.width as usize),
            style,
            body.x + body.width,
        );
    }
}

/// `u` picker: which host takes the hop as its jump.
fn draw_use_picker(
    buf: &mut Buffer,
    inner: Rect,
    view: &MapView,
    p: &crate::app::network_map::UsePicker,
    st: &Styles,
) {
    let visible = p.visible(&view.map);
    let rows = visible
        .len()
        .min(inner.height.saturating_sub(6) as usize)
        .max(1);
    let w = 56.min(inner.width.saturating_sub(2));
    let area = Rect::new(
        inner.x + (inner.width - w) / 2,
        inner.y + 1,
        w,
        rows as u16 + 4,
    );
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            buf.set_string(x, y, " ", st.text);
        }
    }
    let title = format!(
        "use {} as jump \u{b7} Tab: {}",
        p.token,
        if p.append { "append" } else { "replace" }
    );
    let body = panel(buf, area, &title, st.accent, st.bright);
    let max_x = body.x + body.width;
    put(
        buf,
        body.x,
        body.y,
        &format!(" \u{203a} {}\u{258f}", p.query),
        st.text,
        max_x,
    );
    let first = p.selected.saturating_sub(rows - 1);
    for (i, &(h, loops)) in visible.iter().skip(first).take(rows).enumerate() {
        let name = &view.map.hosts[h].name;
        let style = if first + i == p.selected {
            st.selected
        } else if loops {
            st.dim
        } else {
            st.text
        };
        let text = format!(
            "   {name}{}",
            if loops { "   \u{21bb} would loop" } else { "" }
        );
        put(
            buf,
            body.x,
            body.y + 1 + i as u16,
            &format!("{text:<w$}", w = body.width as usize),
            style,
            max_x,
        );
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

fn node_look(ctx: &Ctx, cols: &Cols, node: usize, shared: &HashSet<HopKey>) -> NodeLook {
    let (app, view, st) = (ctx.app, ctx.view, ctx.st);
    let n = &view.map.nodes[node];
    let room = cols.step.saturating_sub(5) as usize;
    match &n.key {
        HopKey::Saved(h) => {
            let s = host_status(app, st, *h);
            let glyph = ctx.test_glyph(*h).map_or(s.glyph, |(g, _)| g);
            let name =
                crate::tui::text::ellipsize(&view.map.hosts[*h].name, room.saturating_sub(7));
            let mut text = format!("{glyph} {name}");
            if cols.latency {
                if let Some(ms) = s.ms {
                    text.push_str(&format!("  {}", ms_text(Some(ms))));
                }
            }
            if shared.contains(&n.key) {
                text.push_str(" \u{25c7}");
            }
            if matches!(view.lens, Lens::Tunnel) {
                for t in ctx.tunnels.get(h).into_iter().flatten() {
                    text.push_str(&format!(" \u{21c4}{}", tunnel_letter(t.kind)));
                }
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
/// Record host `h`'s glyph at (`x`, `y`) as a click target.
fn record_glyph(ctx: &Ctx, x: u16, y: u16, row: usize, h: usize) {
    let view = ctx.view;
    let Some(&r) = view.rows.get(row) else {
        return;
    };
    if let Some(k) = view.map.row_hosts(r).iter().position(|&v| v == h) {
        ctx.app.record_click(
            Rect::new(x, y, 1, 1),
            ClickHit::MapCell {
                row,
                pos: view.hop_count(row) + k,
            },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_strip(
    buf: &mut Buffer,
    mut x: u16,
    y: u16,
    max_x: u16,
    ctx: &Ctx,
    tint: Tint,
    groups: &[(String, Vec<usize>)],
    selected: Option<usize>,
    row: usize,
) -> u16 {
    let st = ctx.st;
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
            .map_or(0, |p| (p + 1).saturating_sub(ctx.max_glyphs));
        let shown = hosts.iter().skip(start).take(ctx.max_glyphs);
        for &h in shown {
            let (glyph, gstyle) = ctx.glyph(h, tint);
            let style = if selected == Some(h) {
                st.selected
            } else {
                gstyle
            };
            if x < max_x {
                record_glyph(ctx, x, y, row, h);
            }
            x = put(buf, x, y, &glyph.to_string(), style, max_x);
        }
        let hidden = hosts.len().saturating_sub(ctx.max_glyphs);
        if hidden > 0 {
            x = put(buf, x, y, &format!("+{hidden}"), st.muted, max_x);
        }
    }
    x
}

fn draw_map(buf: &mut Buffer, inner: Rect, ctx: &Ctx) {
    let (view, st) = (ctx.view, ctx.st);
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
            Row::Cycle(_) | Row::Cluster(_) => true,
            Row::Direct | Row::More(_) | Row::DirectGroup(_) => false,
        })
        .map(|(i, _)| i)
        .collect();
    let top_set: HashSet<usize> = top.iter().copied().collect();
    // In a clustered map the cluster rows own the left trunk and the root
    // hops sit one step in, under their cluster.
    let clustered = view.clustered();
    let top: Vec<usize> = if clustered {
        top.into_iter()
            .filter(|&i| !matches!(view.rows[i], Row::Node(_)))
            .collect()
    } else {
        top
    };
    let last_top = top.last().copied();

    // One blank row between top-level items when everything fits.
    let visible = inner.height.saturating_sub(2) as usize;
    let blanks = top.len().saturating_sub(1) + usize::from(view.rows.contains(&Row::Direct));
    let spacious = view.rows.len() + blanks <= visible;
    let mut ys: Vec<usize> = Vec::with_capacity(view.rows.len());
    let mut y = 0usize;
    for (i, r) in view.rows.iter().enumerate() {
        let starts_item = (top_set.contains(&i) && !clustered) || matches!(r, Row::Direct);
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
        // The whole row selects its own hop; glyphs record themselves on top.
        ctx.app.record_click(
            Rect::new(inner.x, y, inner.width, 1),
            ClickHit::MapCell {
                row: i,
                pos: view.hop_count(i) - 1,
            },
        );
        match row {
            Row::Node(n) => {
                let node = &map.nodes[n];
                let look = node_look(ctx, &cols, n, &shared);
                let tint = view.tint(i);
                let on_path = path_nodes.contains(&n) && tint == Tint::Normal;
                let edge_style = match tint {
                    Tint::Dim => st.dim,
                    Tint::Warn => st.warn,
                    Tint::Normal if on_path => st.accent,
                    Tint::Normal => st.dim,
                };
                let fill = if look.dashed { "\u{2504}" } else { "\u{2500}" };
                let col = if node.depth == 0 {
                    cols.hop1 + if clustered { 3 } else { 0 }
                } else {
                    cols.hop2
                };
                // Connector into the node.
                if node.depth == 0 && clustered {
                    put(buf, cx(cols.hop1), y, "\u{251c}", edge_style, right);
                    put(buf, cx(cols.hop1 + 1), y, fill, edge_style, right);
                    put(buf, cx(cols.hop1 + 2), y, "\u{25b8}", edge_style, right);
                } else if node.depth == 0 {
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
                let mut style = match tint {
                    Tint::Dim => st.dim,
                    Tint::Warn => st.warn,
                    Tint::Normal if on_path => st.accent,
                    Tint::Normal => look.style,
                };
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
                    ctx,
                    tint,
                    &node.cargo,
                    sel_host.filter(|_| row_selected),
                    i,
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
                let sigma_style = match tint {
                    Tint::Dim => st.dim,
                    Tint::Warn => st.warn,
                    Tint::Normal => st.bright,
                };
                put(buf, sigma_x, y, &sigma, sigma_style, right);
                // Child trunks: a vertical line from this row to its last child.
                if !node.children.is_empty() {
                    draw_child_trunk(buf, inner, &cols, view, &ys, offset, visible, i, n, st);
                }
            }
            Row::Cycle(ci) => {
                let c = &map.cycles[ci];
                let tint = view.tint(i);
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
                } else if tint == Tint::Dim {
                    st.dim
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
                    ctx,
                    tint,
                    &groups,
                    sel_host.filter(|_| row_selected),
                    i,
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
            Row::Cluster(c) => {
                let cl = &map.clusters[c];
                let open = view.open_clusters.contains(&cl.name);
                let tint = view.tint(i);
                let first = top.first() == Some(&i);
                let last = last_top == Some(i);
                let edge = if tint == Tint::Warn { st.warn } else { st.dim };
                if first {
                    put(
                        buf,
                        cx(1),
                        y,
                        "\u{25c9} you",
                        st.accent.add_modifier(Modifier::BOLD),
                        right,
                    );
                    put(buf, cx(7), y, "\u{2500}\u{2500}", edge, right);
                }
                let joint = match (first, last) {
                    (true, true) => "\u{2500}",
                    (true, false) => "\u{252c}",
                    (false, true) => "\u{2514}",
                    _ => "\u{251c}",
                };
                put(buf, cx(cols.trunk1()), y, joint, edge, right);
                put(
                    buf,
                    cx(cols.trunk1() + 1),
                    y,
                    "\u{2500}\u{25b8}",
                    edge,
                    right,
                );
                let style = match (
                    row_selected && matches!(sel, Some(MapSel::Cluster(_))),
                    tint,
                ) {
                    (true, _) => st.selected,
                    (_, Tint::Dim) => st.dim,
                    (_, Tint::Warn) => st.warn,
                    _ => st.text,
                };
                let head = format!(
                    "{} {}  {} hops",
                    if open { "\u{25be}" } else { "\u{25b8}" },
                    crate::tui::text::ellipsize(&cl.name, 20),
                    cl.roots.len()
                );
                let end = put(buf, cx(cols.hop1), y, &head, style, cx(cols.cargo));
                for x in end + 1..cx(cols.cargo).saturating_sub(1) {
                    put(buf, x, y, "\u{2508}", st.dim, right);
                }
                // The strip of the cluster is its hops themselves, one glyph each.
                put(buf, cx(cols.cargo), y, "hops ", st.muted, sigma_x);
                let hops = map.row_hosts(row);
                let mut x = cx(cols.cargo) + 5;
                let selected = sel_host.filter(|_| row_selected);
                for (k, &h) in hops.iter().take(ctx.max_glyphs).enumerate() {
                    let _ = k;
                    let (glyph, gstyle) = ctx.glyph(h, tint);
                    let style = if selected == Some(h) {
                        st.selected
                    } else {
                        gstyle
                    };
                    if x < sigma_x {
                        record_glyph(ctx, x, y, i, h);
                    }
                    x = put(buf, x, y, &glyph.to_string(), style, sigma_x);
                }
                if hops.len() > ctx.max_glyphs {
                    put(
                        buf,
                        x,
                        y,
                        &format!("+{}", hops.len() - ctx.max_glyphs),
                        st.muted,
                        sigma_x,
                    );
                }
                let sigma_style = match tint {
                    Tint::Dim => st.dim,
                    Tint::Warn => st.warn,
                    Tint::Normal => st.bright,
                };
                put(
                    buf,
                    sigma_x,
                    y,
                    &format!("{:>4}", cl.sigma),
                    sigma_style,
                    right,
                );
            }
            Row::More(c) => {
                let cl = &map.clusters[c];
                let shown = cl
                    .roots
                    .iter()
                    .filter(|&&r| view.rows.contains(&Row::Node(r)))
                    .count();
                let hidden = cl.roots.len() - shown;
                let tail_max = cl.roots[shown.min(cl.roots.len() - 1)..]
                    .iter()
                    .map(|&r| map.nodes[r].sigma)
                    .max()
                    .unwrap_or(0);
                put(
                    buf,
                    cx(cols.hop1),
                    y,
                    "\u{2514}\u{2500}\u{25b8}",
                    st.dim,
                    right,
                );
                let style = if row_selected { st.selected } else { st.muted };
                put(
                    buf,
                    cx(cols.hop1 + 4),
                    y,
                    &format!(
                        "\u{2026} {hidden} more hops  \u{3a3} \u{2264} {tail_max} each \u{b7} space shows {REVEAL_STEP} more"
                    ),
                    style,
                    sigma_x,
                );
            }
            Row::DirectGroup(g) => {
                let (name, hosts) = &map.direct_groups[g];
                let style = if row_selected && matches!(sel, Some(MapSel::Direct)) {
                    st.selected
                } else {
                    st.muted
                };
                put(
                    buf,
                    cx(3),
                    y,
                    &format!(
                        "{:<12}{:>4}",
                        crate::tui::text::ellipsize(name, 12),
                        hosts.len()
                    ),
                    style,
                    cx(cols.cargo),
                );
                draw_strip(
                    buf,
                    cx(cols.hop1 + 6),
                    y,
                    sigma_x,
                    ctx,
                    Tint::Normal,
                    &[(String::new(), hosts.clone())],
                    sel_host.filter(|_| row_selected),
                    i,
                );
            }
            Row::Direct => {
                put(
                    buf,
                    cx(1),
                    y,
                    if view.direct_open {
                        "\u{25be}"
                    } else {
                        "\u{22ef}"
                    },
                    st.muted,
                    right,
                );
                let style = if row_selected { st.selected } else { st.muted };
                put(
                    buf,
                    cx(cols.hop1),
                    y,
                    "direct-only \u{b7} no jump, nothing behind them",
                    style,
                    cx(cols.cargo),
                );
                if view.direct_open {
                    put(
                        buf,
                        cx(cols.cargo),
                        y,
                        &format!("{} hosts, one strip per group", map.direct.len()),
                        st.muted,
                        sigma_x,
                    );
                    put(
                        buf,
                        sigma_x,
                        y,
                        &format!("{:>4}", map.direct.len()),
                        st.bright,
                        right,
                    );
                    continue;
                }
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
    if let Some(run) = map_test(app, Some(sel)) {
        apply_test(&mut cards, run, st);
    }
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
        MapSel::Cluster(_) => "\u{21b5} or space opens the cluster. Shift+Z folds all.".into(),
        MapSel::More(_) => "space reveals more hops.".into(),
        MapSel::Direct => "space expands one strip per group.".into(),
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
    if let (true, Some(run)) = (inner.height > 1, map_test(app, Some(sel))) {
        put(
            buf,
            inner.x + 1,
            inner.y + 1,
            &format!("test: {}", run.headline()),
            st.muted,
            right,
        );
    } else if inner.height > 1 {
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

fn draw_facts(buf: &mut Buffer, inner: Rect, ctx: &Ctx, sel: Option<MapSel>) {
    let (view, st) = (ctx.view, ctx.st);
    let right = inner.x + inner.width;
    let map = &view.map;
    if let Some(run) = ctx.test {
        for (i, (step, state)) in run.steps.iter().zip(&run.states).enumerate() {
            if i >= inner.height.saturating_sub(1) as usize {
                break;
            }
            let (glyph, text) = crate::app::hop_editor::step_text(state);
            let style = if state.is_failure() { st.err } else { st.text };
            let cmd = if step.cmd.is_empty() {
                step.label.clone()
            } else {
                crate::tui::text::ellipsize(&step.cmd, 40)
            };
            put(
                buf,
                inner.x + 1,
                inner.y + i as u16,
                &format!("{} {cmd:<40} {glyph} {text}", i + 1),
                style,
                right,
            );
        }
        let last = inner.y + inner.height.saturating_sub(1);
        let note = match run.failure() {
            Some((i, reason, detail)) => format!(
                "\u{2717} hop {} ({}): {reason} {detail}   H known hosts  P push key  l ssh log  t again",
                i + 1,
                run.steps[i].label
            ),
            None if run.running() => "Stops at the first failure. Esc cancels.".to_string(),
            None => "Esc closes. t tests again.".to_string(),
        };
        put(buf, inner.x + 1, last, &note, st.muted, right);
        return;
    }
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
    if let Lens::Blast(b) = &view.lens {
        let sessions = b
            .hosts
            .iter()
            .filter(|&&h| host_status(ctx.app, st, h).glyph == '\u{25b6}')
            .count();
        let tunnels: usize = b
            .hosts
            .iter()
            .filter_map(|h| ctx.tunnels.get(h))
            .map(Vec::len)
            .sum();
        rows.push((
            "radius",
            format!(
                "{} hosts depend on {}",
                b.hosts.len(),
                map.key_label(&b.key)
            ),
        ));
        rows.push((
            "riding",
            format!("{sessions} sessions open \u{b7} {tunnels} tunnels on dependents"),
        ));
        rows.push((
            "keys",
            "n/N next and previous dependent   b broadcast   B or Esc off".into(),
        ));
        for (i, (k, v)) in rows.iter().take(inner.height as usize).enumerate() {
            let y = inner.y + i as u16;
            let x = put(buf, inner.x + 1, y, &format!("{k:<8}"), st.muted, right);
            put(buf, x + 1, y, v, st.text, right);
        }
        return;
    }
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
            rows.push(("command", map.node_command(n)));
            if let HopKey::Saved(h) = node.key {
                tunnel_rows(ctx, h, &mut rows);
            }
        }
        Some(MapSel::Host(h)) => {
            host_rows(h, &mut rows);
            rows.push(("command", map.ssh_command(h)));
            tunnel_rows(ctx, h, &mut rows);
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
        Some(MapSel::Cluster(c)) => {
            let cl = &map.clusters[c];
            rows.push(("cluster", cl.name.clone()));
            rows.push((
                "carries",
                format!("{} hops, {} hosts behind them", cl.roots.len(), cl.sigma),
            ));
        }
        Some(MapSel::More(c)) => {
            rows.push(("more", format!("hops folded in {}", map.clusters[c].name)));
        }
        None => {}
    }
    for (i, (k, v)) in rows.iter().take(inner.height as usize).enumerate() {
        let y = inner.y + i as u16;
        let x = put(buf, inner.x + 1, y, &format!("{k:<8}"), st.muted, right);
        put(buf, x + 1, y, v, st.text, right);
    }
}

fn tunnel_rows(ctx: &Ctx, h: usize, rows: &mut Vec<(&'static str, String)>) {
    for t in ctx.tunnels.get(&h).into_iter().flatten() {
        let state = match t.state {
            TunnelState::Running => "running",
            TunnelState::Reconnecting => "reconnecting",
            TunnelState::Stopped => "stopped",
        };
        rows.push((
            "tunnels",
            format!("\u{21c4}{} {} {state}", tunnel_letter(t.kind), t.summary),
        ));
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
            {
                let mut h = host("jump-2", "prod", Some("bastion-eu"));
                h.ssh_config = true;
                h
            },
            host("web-1", "prod", Some("bastion-eu")),
            host("db-1", "prod/db", Some("jump-2")),
            host("bastion-us", "us", None),
            host("db-us-2", "us", Some("bastion-us,jump-2")),
            host("legacy-1", "legacy", Some("ops@203.0.113.7:2222")),
            iot,
            MapHost {
                ssh_config: true,
                ..host("loop-a", "lab", Some("loop-b"))
            },
            MapHost {
                ssh_config: true,
                ..host("loop-b", "lab", Some("loop-a"))
            },
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

    /// 212 jump hosts in 4 groups of 53 (a chain of up to 6 hops in the first
    /// group) and 1,840 hosts in all.
    fn big_fleet() -> Vec<MapHost> {
        let mut hosts = Vec::new();
        for g in 0..4 {
            for j in 0..53 {
                let name = format!("jump-{g}-{j}");
                let jump = if g == 0 && (1..6).contains(&j) {
                    Some(format!("jump-0-{}", j - 1))
                } else {
                    None
                };
                let mut h = host(&name, &format!("hops-{g}"), jump.as_deref());
                h.ssh_config = true;
                hosts.push(h);
            }
        }
        let jumps = hosts.len();
        for k in 0..(1840 - jumps) {
            let via = format!("jump-{}-{}", k % 4, (k / 4) % 53);
            hosts.push(host(
                &format!("srv-{k}"),
                &format!("fleet-{}", k % 9),
                Some(&via),
            ));
        }
        hosts
    }

    #[test]
    fn big_map_is_clustered_sorted_and_tail_folded() {
        let app = app_with(big_fleet());
        let v = app.map_view.as_ref().unwrap();
        assert_eq!(v.map.hosts.len(), 1840);
        assert_eq!(v.map.clusters.len(), 4);
        assert!(v.clustered());
        // Only the biggest cluster starts open; its tail folds after 12 hops.
        let clusters = v
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Cluster(_)))
            .count();
        assert_eq!(clusters, 4);
        assert_eq!(v.open_clusters.len(), 1);
        assert!(v.rows.iter().any(|r| matches!(r, Row::More(_))));
        let buf = render(&app, 160, 45);
        let text = all_text(&buf);
        assert!(text.contains("hops-0"), "{text}");
        assert!(text.contains("more hops"), "{text}");
        assert!(text.contains("hops-1"), "{text}");
    }

    #[test]
    fn cluster_fold_reveal_and_filter_reveal() {
        let mut app = app_with(big_fleet());
        let v = app.map_view.as_mut().unwrap();
        // Cursor on the first cluster row: space folds it, space opens it.
        assert!(matches!(v.selection(), Some(MapSel::Cluster(_))));
        let rows_open = v.rows.len();
        v.toggle_fold();
        assert!(v.rows.len() < rows_open);
        v.toggle_fold();
        assert_eq!(v.rows.len(), rows_open);
        // Walk to the More row: space reveals 20 more hops.
        let more = v
            .rows
            .iter()
            .position(|r| matches!(r, Row::More(_)))
            .unwrap();
        v.row = more;
        v.pos = 0;
        v.toggle_fold();
        assert!(v.rows.len() > rows_open);
        // A filter + Enter opens only the clusters that hold the match.
        v.query = "via:jump-2-7".into();
        v.apply_filter();
        v.reveal_matches();
        let open: Vec<_> = v.open_clusters.iter().cloned().collect();
        assert_eq!(open, ["hops-2"]);
        // Shift+Z folds all, the next press expands direct-only (none here).
        v.fold_all();
        assert!(v.open_clusters.is_empty());
    }

    #[test]
    fn direct_only_expands_into_one_strip_per_group() {
        let mut hosts = fleet();
        for k in 0..30 {
            hosts.push(host(
                &format!("lone-{k}"),
                if k % 2 == 0 { "lab" } else { "home" },
                None,
            ));
        }
        let mut app = app_with(hosts);
        let v = app.map_view.as_mut().unwrap();
        let direct = v.rows.iter().position(|r| *r == Row::Direct).unwrap();
        v.row = direct;
        v.pos = 0;
        v.toggle_fold();
        assert!(v.rows.iter().any(|r| matches!(r, Row::DirectGroup(_))));
        let text = all_text(&render(&app, 160, 45));
        assert!(text.contains("one strip per group"), "{text}");
        assert!(text.contains("lab") && text.contains("home"), "{text}");
    }

    #[test]
    fn narrow_modes_render_without_overflow() {
        let app = app_with(fleet());
        // 100x30: compact columns, no latency, one-line ribbon.
        let t = all_text(&render(&app, 100, 30));
        assert!(t.contains("bastion-eu") && t.contains("path ribbon"), "{t}");
        // Under 80x24: outline with the selected path on the last line.
        let buf = render(&app, 70, 20);
        let last = row_text(&buf, 19);
        assert!(last.starts_with("you "), "{last:?}");
        let t = all_text(&buf);
        assert!(t.contains("\u{251c} "), "{t}");
        // Under 40x12: the size hint.
        assert!(all_text(&render(&app, 39, 11)).contains("map needs 40x12"));
        // Every size draws.
        for (w, h) in [(40, 12), (79, 23), (80, 24), (99, 29), (200, 60)] {
            let _ = render(&app, w, h);
        }
    }

    #[test]
    fn the_212_jump_host_fleet_renders_within_one_frame_budget() {
        let mut app = app_with(big_fleet());
        // Open everything and walk to the middle: the worst case.
        let v = app.map_view.as_mut().unwrap();
        for c in v.map.clusters.clone() {
            v.open_clusters.insert(c.name);
        }
        v.apply_filter();
        v.row = v.rows.len() / 2;
        // Warm up, then take the best of a few frames.
        let _ = render(&app, 160, 45);
        let best = (0..5)
            .map(|_| {
                let t = std::time::Instant::now();
                let _ = render(&app, 160, 45);
                t.elapsed()
            })
            .min()
            .unwrap();
        assert!(
            best < std::time::Duration::from_millis(50),
            "frame took {best:?}"
        );
    }

    #[test]
    fn empty_map_renders() {
        let app = app_with(Vec::new());
        let buf = render(&app, 160, 45);
        assert!(all_text(&buf).contains("hosts behind this hop"));
    }
}
