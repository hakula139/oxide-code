//! `grep` tool body (content mode) — per-file groups of line-numbered
//! match rows. Context lines (`-` separator in grep output) render dim
//! to keep matches visually distinct.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::super::RenderCtx;
use super::{
    MAX_TOOL_OUTPUT_LINE_BYTES, MAX_TOOL_OUTPUT_LINES, STATUS_LINE_CONT,
    border_continuation_prefix, border_style_for, truncate_to_bytes,
};
use crate::tool::GrepFileGroup;
use crate::tui::wrap::{expand_tabs, wrap_line};

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    groups: &[GrepFileGroup],
    truncated: bool,
    is_error: bool,
) {
    if groups.is_empty() {
        return;
    }

    let border_style = border_style_for(ctx.theme, is_error);
    let width = usize::from(ctx.width);
    let status_cont_prefix = border_continuation_prefix(STATUS_LINE_CONT, border_style);

    // Budget spans both path headers and match rows.
    let total_rows: usize = groups.iter().map(|g| 1 + g.lines.len()).sum();
    let visible_rows = total_rows.min(MAX_TOOL_OUTPUT_LINES);
    let hidden = total_rows.saturating_sub(visible_rows);

    // Pad to the widest line number across all groups for column alignment.
    let line_number_width = groups
        .iter()
        .flat_map(|g| g.lines.iter())
        .map(|l| l.number.to_string().width())
        .max()
        .unwrap_or(1);
    let line_cont_prefix = format!("{STATUS_LINE_CONT}{}   ", " ".repeat(line_number_width));
    let line_cont_spans = border_continuation_prefix(&line_cont_prefix, border_style);

    let mut emitted: usize = 0;
    'outer: for group in groups {
        if emitted >= visible_rows {
            break;
        }
        let path_line = Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled(group.path.clone(), ctx.theme.muted()),
        ]);
        out.extend(wrap_line(
            path_line,
            width,
            STATUS_LINE_CONT.width(),
            Some(&status_cont_prefix),
        ));
        emitted += 1;

        for line in &group.lines {
            if emitted >= visible_rows {
                break 'outer;
            }
            let expanded = expand_tabs(&line.text);
            let display_text = truncate_to_bytes(&expanded, MAX_TOOL_OUTPUT_LINE_BYTES);
            let line_number = format!("{:>width$}", line.number, width = line_number_width);
            let text_style = if line.is_match {
                ctx.theme.text()
            } else {
                ctx.theme.dim()
            };
            let rendered = Line::from(vec![
                Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
                Span::styled(line_number, ctx.theme.muted()),
                Span::styled(" │ ", ctx.theme.dim()),
                Span::styled(display_text, text_style),
            ]);
            out.extend(wrap_line(
                rendered,
                width,
                line_cont_prefix.width(),
                Some(&line_cont_spans),
            ));
            emitted += 1;
        }
    }

    if let Some(text) = footer_text(hidden, truncated) {
        out.push(Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled(text, ctx.theme.dim()),
        ]));
    }
}

/// Footer combining TUI-side row hiding (`hidden`) and grep's
/// server-side `head_limit` (`truncated`). `None` when neither applies.
fn footer_text(hidden: usize, truncated: bool) -> Option<String> {
    match (hidden, truncated) {
        (0, false) => None,
        (0, true) => Some("... limit reached".to_owned()),
        (n, false) => {
            let noun = if n == 1 { "line" } else { "lines" };
            Some(format!("... +{n} {noun}"))
        }
        (n, true) => {
            let noun = if n == 1 { "line" } else { "lines" };
            Some(format!("... +{n} {noun} (limit reached)"))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::theme::Theme;

    use super::*;

    fn collect_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|sp| sp.content.as_ref()))
            .collect::<Vec<_>>()
            .join("|")
    }

    // ── render ──

    #[test]
    fn render_empty_groups_emits_nothing() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        // truncated=true is intentional: empty groups short-circuit
        // before the footer, so even a `truncated` flag emits no body.
        render(&mut out, &ctx, &[], true, false);
        assert!(out.is_empty());
    }

    #[test]
    fn render_stops_when_budget_fills_at_path_boundary() {
        // Six 0-line groups overflow the 5-row budget on path headers
        // alone, exercising the outer-loop budget guard that the inner
        // `break 'outer` doesn't reach when groups have no match rows.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        let groups: Vec<GrepFileGroup> = (0..6)
            .map(|i| GrepFileGroup {
                path: format!("f{i}.rs"),
                lines: Vec::new(),
            })
            .collect();
        render(&mut out, &ctx, &groups, false, false);
        let body = collect_text(&out);
        for i in 0..5 {
            assert!(
                body.contains(&format!("f{i}.rs")),
                "first 5 path headers should render: {body}",
            );
        }
        assert!(
            !body.contains("f5.rs"),
            "6th path header should be hidden by the budget guard: {body}",
        );
    }

    // ── footer_text ──

    #[test]
    fn footer_text_no_hidden_no_truncation_returns_none() {
        assert_eq!(footer_text(0, false), None);
    }

    #[test]
    fn footer_text_truncated_only_names_limit() {
        assert_eq!(footer_text(0, true), Some("... limit reached".to_owned()));
    }

    #[test]
    fn footer_text_hidden_uses_singular_or_plural() {
        assert_eq!(footer_text(1, false), Some("... +1 line".to_owned()));
        assert_eq!(footer_text(3, false), Some("... +3 lines".to_owned()));
    }

    #[test]
    fn footer_text_hidden_and_truncated_combines_both() {
        assert_eq!(
            footer_text(2, true),
            Some("... +2 lines (limit reached)".to_owned()),
        );
    }
}
