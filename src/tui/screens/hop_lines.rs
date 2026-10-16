//! The hop-editor lines of the host form (issue #79): list, hint, `saves as`,
//! path, raw input, picker and test result, drawn under the ProxyJump row as
//! boxes with their own right border (design D10, D11, D13).

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::hop_editor::HopSection;
use crate::theme::catalog::StyleRole;
use crate::theme::model::ResolvedTheme;

/// Left indent of the boxes under the field label.
const INDENT: usize = 4;

struct Boxes {
    indent: String,
    /// Total width of a box, borders included.
    width: usize,
}

impl Boxes {
    fn top(&self, title: &str) -> String {
        let head = format!("\u{250c}\u{2500} {title} ");
        let fill = self.width.saturating_sub(head.width() + 1);
        format!("{}{head}{}\u{2510}", self.indent, "\u{2500}".repeat(fill))
    }

    fn bottom(&self, title: &str) -> String {
        let head = if title.is_empty() {
            "\u{2514}".to_string()
        } else {
            format!("\u{2514}\u{2500} {title} ")
        };
        let fill = self.width.saturating_sub(head.width() + 1);
        format!("{}{head}{}\u{2518}", self.indent, "\u{2500}".repeat(fill))
    }

    /// One row between the side borders, padded (or cut) to the box width.
    fn row(&self, content: &str) -> String {
        let inner = self.width.saturating_sub(2);
        let mut text = String::new();
        let mut used = 0;
        let cut = content.width() > inner;
        let room = if cut { inner.saturating_sub(1) } else { inner };
        for c in content.chars() {
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if used + w > room {
                break;
            }
            text.push(c);
            used += w;
        }
        if cut {
            text.push('\u{2026}');
            used += 1;
        }
        format!(
            "{}\u{2502}{text}{}\u{2502}",
            self.indent,
            " ".repeat(inner - used)
        )
    }

    /// `left` and `right` on one row, `right` flush with the border.
    fn split(&self, left: &str, right: &str) -> String {
        let inner = self.width.saturating_sub(2);
        let pad = inner
            .saturating_sub(left.width() + right.width() + 1)
            .max(1);
        self.row(&format!("{left}{}{right} ", " ".repeat(pad)))
    }
}

/// Push the hop section as lines (no field index: a click there does nothing).
/// `width` is the form's inner width.
pub fn push_hop_lines(
    lines: &mut Vec<(Line<'static>, Option<usize>)>,
    h: &HopSection,
    theme: &ResolvedTheme,
    width: u16,
) {
    let help = theme.style(StyleRole::FormHelp);
    let value = theme.style(StyleRole::FormValue);
    let focus = theme.style(StyleRole::FocusIndicator);
    let muted = theme.style(StyleRole::TextMuted);
    let dim = theme.style(StyleRole::TextDim);
    let err = Style::default().fg(theme.color(crate::theme::catalog::ColorRole::StatusError));
    let ok = Style::default().fg(theme.color(crate::theme::catalog::ColorRole::StatusSuccess));
    let mut push = |text: String, style: Style| {
        lines.push((Line::from(Span::styled(text, style)), None));
    };

    // Boxes keep clear of the form's own right edge (it wraps wider lines).
    let bw = (width as usize).saturating_sub(INDENT + 2).clamp(40, 150);
    let bx = Boxes {
        indent: " ".repeat(INDENT),
        width: bw,
    };

    // ---- the list
    push(bx.top(&h.title), muted);
    if let Some(head) = &h.head {
        push(bx.row(&format!(" {head}")), help);
    }
    if let Some(e) = &h.empty_text {
        push(bx.row(&format!(" {e}")), dim);
    }
    for (i, r) in h.rows.iter().enumerate() {
        let style = if r.dim {
            dim
        } else if r.selected {
            focus
        } else {
            value
        };
        let mark = if r.selected { '\u{25b8}' } else { ' ' };
        let glyph = if r.status == ' ' { ' ' } else { r.status };
        let left = format!(
            "{mark}{}  {glyph} {:<nw$}  {:<24} {}",
            i + 1,
            r.name,
            r.source,
            r.addr,
            nw = h.name_w.max(8)
        );
        push(bx.split(&left, &r.latency), style);
    }
    push(bx.row(&format!(" {}", h.hint)), help);
    push(bx.bottom(&h.saves_as), muted);
    push(
        format!("{}path   {}", " ".repeat(INDENT), h.path),
        if h.inherited { dim } else { muted },
    );
    if let Some(note) = &h.join_note {
        push(format!("{}{note}", " ".repeat(INDENT)), dim);
    }

    // ---- raw hop input
    if let Some((text, error)) = &h.raw {
        push(bx.top("add a raw hop"), muted);
        push(bx.row(&format!(" raw hop \u{203a} {text}\u{258f}")), value);
        match error {
            Some(e) => push(bx.row(&format!(" {e}")), err),
            None => push(bx.row(" user@host:port   \u{21b5} add   Esc cancel"), help),
        }
        push(bx.bottom(""), muted);
    }

    // ---- picker
    if let Some(p) = &h.picker {
        push(bx.top(&p.title), muted);
        push(
            bx.split(
                &format!(" \u{203a} {}\u{258f}", p.query),
                "fuzzy \u{b7} name, group, tag",
            ),
            value,
        );
        for r in &p.rows {
            if let Some(sec) = r.section {
                push(bx.row(&format!(" {sec}")), muted);
            }
            let style = if r.selected {
                focus
            } else if r.dim {
                dim
            } else {
                value
            };
            push(
                bx.row(&format!(
                    " {}{}",
                    if r.selected { "\u{25b8} " } else { "  " },
                    r.text
                )),
                style,
            );
        }
        if !p.preview.is_empty() {
            push(bx.row(&format!(" {}", p.preview)), muted);
        }
        push(bx.bottom("\u{21b5} add  End raw hop  Esc close"), help);
    }

    // ---- test result
    if let Some(t) = &h.test {
        push(bx.top(&t.title), muted);
        let card_w = 24usize;
        let n = t.rows.len();
        // Cards when they fit, a plain list when they do not.
        let cards = n * (card_w + 5) + 8 <= bw;
        if cards {
            let gap = "     ";
            let row = |f: &dyn Fn(usize) -> String| -> String {
                (0..n).map(f).collect::<Vec<_>>().join(gap)
            };
            let pad = |s: &str| format!("{s:<w$}", w = card_w - 2);
            push(
                bx.row(&format!(
                    "       {}",
                    row(&|_| format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(card_w - 2)))
                )),
                muted,
            );
            push(
                bx.row(&format!(
                    " \u{25c9} \u{2501}\u{2501}\u{25b6} {}",
                    t.rows
                        .iter()
                        .map(|r| format!("\u{2502}{}\u{2502}", pad(&format!(" {}", r.1))))
                        .collect::<Vec<_>>()
                        .join(" \u{2501}\u{2501}\u{25b6} ")
                )),
                value,
            );
            push(
                bx.row(&format!(
                    "       {}",
                    row(&|i| format!(
                        "\u{2502}{}\u{2502}",
                        pad(&format!(" {} {}", t.rows[i].0, t.rows[i].2))
                    ))
                )),
                if t.rows.iter().any(|r| r.4) { err } else { ok },
            );
            push(
                bx.row(&format!(
                    "       {}",
                    row(&|i| format!("\u{2502}{}\u{2502}", pad(&format!(" {}", t.rows[i].3))))
                )),
                err,
            );
            push(
                bx.row(&format!(
                    "       {}",
                    row(&|_| format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(card_w - 2)))
                )),
                muted,
            );
        } else {
            for (glyph, name, text, reason, failed) in &t.rows {
                push(
                    bx.row(&format!(" {glyph} {name:<20} {text} {reason}")),
                    if *failed { err } else { value },
                );
            }
        }
        push(
            bx.row(&format!(" {}", t.result)),
            if t.result.contains('\u{2717}') {
                err
            } else {
                muted
            },
        );
        push(bx.bottom(&t.hint), help);
    }
    if let Some(n) = &h.notice {
        push(format!("{}{n}", " ".repeat(INDENT)), err);
    }
}
