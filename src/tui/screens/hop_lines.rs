//! The hop-editor lines of the host form (issue #79): list, hint, `saves as`,
//! path, raw input and picker, drawn under the ProxyJump row.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::hop_editor::HopSection;
use crate::theme::catalog::StyleRole;
use crate::theme::model::ResolvedTheme;

/// The hop list, hint, `saves as`, path and (when open) the picker, as extra
/// lines under the ProxyJump row. They carry no field index: clicks on them do
/// nothing.
pub fn push_hop_lines(
    lines: &mut Vec<(Line<'static>, Option<usize>)>,
    h: &HopSection,
    theme: &ResolvedTheme,
) {
    let help = theme.style(StyleRole::FormHelp);
    let value = theme.style(StyleRole::FormValue);
    let focus = theme.style(StyleRole::FocusIndicator);
    let muted = theme.style(StyleRole::TextMuted);
    let dim = theme.style(StyleRole::TextDim);
    let err = Style::default().fg(theme.color(crate::theme::catalog::ColorRole::StatusError));
    let mut push = |spans: Vec<Span<'static>>| lines.push((Line::from(spans), None));

    push(vec![Span::styled(
        format!("    \u{250c}\u{2500} {} ", h.title),
        muted,
    )]);
    if let Some(e) = &h.empty_text {
        push(vec![Span::styled(format!("    \u{2502} {e}"), dim)]);
    }
    for (i, r) in h.rows.iter().enumerate() {
        let style = if r.dim {
            dim
        } else if r.selected {
            focus
        } else {
            value
        };
        let mark = if r.selected { "\u{25b8}" } else { " " };
        let glyph = if r.status == ' ' {
            String::new()
        } else {
            format!("{} ", r.status)
        };
        push(vec![Span::styled(
            format!(
                "    \u{2502}{mark}{} {glyph}{}   {}   {}   {}",
                i + 1,
                r.name,
                r.source,
                r.addr,
                r.latency
            )
            .trim_end()
            .to_string(),
            style,
        )]);
    }
    push(vec![Span::styled(format!("    \u{2502} {}", h.hint), help)]);
    push(vec![Span::styled(
        format!("    \u{2514}\u{2500} {} ", h.saves_as),
        muted,
    )]);
    push(vec![Span::styled(
        format!("    path   {}", h.path),
        if h.inherited { dim } else { muted },
    )]);
    if let Some((text, error)) = &h.raw {
        push(vec![Span::styled(
            format!("    raw hop \u{203a} {text}\u{258f}"),
            value,
        )]);
        push(vec![Span::styled(
            match error {
                Some(e) => format!("    {e}"),
                None => "    \u{21b5} add   Esc cancel".to_string(),
            },
            if error.is_some() { err } else { help },
        )]);
    }
    if let Some(p) = &h.picker {
        push(vec![Span::styled(
            format!("    \u{250c}\u{2500} {} ", p.title),
            muted,
        )]);
        push(vec![Span::styled(
            format!(
                "    \u{2502} \u{203a} {}\u{258f}   fuzzy \u{b7} name, group",
                p.query
            ),
            value,
        )]);
        for r in &p.rows {
            if let Some(sec) = r.section {
                push(vec![Span::styled(format!("    \u{2502} {sec}"), muted)]);
            }
            let style = if r.selected {
                focus
            } else if r.dim {
                dim
            } else {
                value
            };
            push(vec![Span::styled(
                format!(
                    "    \u{2502} {}{}",
                    if r.selected { "\u{25b8} " } else { "  " },
                    r.text
                ),
                style,
            )]);
        }
        if !p.preview.is_empty() {
            push(vec![Span::styled(
                format!("    \u{2502} {}", p.preview),
                muted,
            )]);
        }
        push(vec![Span::styled(
            "    \u{2514}\u{2500} \u{21b5} add   End raw hop   Esc close ".to_string(),
            help,
        )]);
    }
    if let Some(t) = &h.test {
        push(vec![Span::styled(
            format!("    \u{250c}\u{2500} {} ", t.title),
            muted,
        )]);
        for (glyph, name, text, failed) in &t.rows {
            push(vec![Span::styled(
                format!("    \u{2502} {glyph} {name:<20} {text}"),
                if *failed { err } else { value },
            )]);
        }
        push(vec![Span::styled(
            format!("    \u{2502} {}", t.result),
            if t.result.starts_with('\u{2717}') {
                err
            } else {
                muted
            },
        )]);
        push(vec![Span::styled(
            format!("    \u{2514}\u{2500} {} ", t.hint),
            help,
        )]);
    }
    if let Some(n) = &h.notice {
        push(vec![Span::styled(format!("    {n}"), err)]);
    }
}
