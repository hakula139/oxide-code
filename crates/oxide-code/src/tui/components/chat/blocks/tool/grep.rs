//! `grep` tool body (content mode) — per-file groups of line-numbered match rows; non-match
//! context lines render dim.

use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use super::super::RenderCtx;
use super::{MAX_TOOL_OUTPUT_LINES, border_style_for, bordered_row, numbered_row};
use crate::tool::GrepFileGroup;

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
    let rows = numbered_row::Renderer::new(ctx, border_style, line_number_width);

    let mut emitted: usize = 0;
    'outer: for group in groups {
        if emitted >= visible_rows {
            break;
        }
        bordered_row::render(
            out,
            ctx,
            border_style,
            group.path.clone(),
            ctx.theme.muted(),
        );
        emitted += 1;

        for line in &group.lines {
            if emitted >= visible_rows {
                break 'outer;
            }
            let text_style = if line.is_match {
                ctx.theme.text()
            } else {
                ctx.theme.dim()
            };
            rows.render(out, line.number, &line.text, text_style);
            emitted += 1;
        }
    }

    if let Some(text) = footer_text(hidden, truncated) {
        bordered_row::render(out, ctx, border_style, text, ctx.theme.dim());
    }
}

/// Footer combining TUI-side row hiding and grep's `head_limit` cap; `None` when neither applies.
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
    use super::*;
    use crate::tui::theme::Theme;

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
        // truncated=true is intentional: empty groups short-circuit before the footer.
        render(&mut out, &ctx, &[], true, false);
        assert!(out.is_empty());
    }

    #[test]
    fn render_stops_when_budget_fills_at_path_boundary() {
        // Six 0-line groups overflow the 5-row budget on path headers alone, hitting the outer
        // guard that `break 'outer` does not reach when groups have no match rows.
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
    fn footer_text_no_hidden_no_truncation_is_none() {
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
