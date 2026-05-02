//! `glob` body — `pattern (visible of total)` header + flat path
//! list. Footer flags the producer's `MAX_RESULTS` cap when hit.

use ratatui::text::Line;

use super::super::RenderCtx;
use super::{
    MAX_TOOL_OUTPUT_LINE_BYTES, MAX_TOOL_OUTPUT_LINES, border_style_for, bordered_row,
    truncate_to_bytes,
};

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    pattern: &str,
    files: &[String],
    total: usize,
    is_error: bool,
) {
    let border_style = border_style_for(ctx.theme, is_error);

    if files.is_empty() {
        // Skip the header — "No files found" already labels the result.
        bordered_row::render(out, ctx, border_style, "No files found", ctx.theme.dim());
        return;
    }

    let visible = files.len().min(MAX_TOOL_OUTPUT_LINES);
    let hidden = files.len() - visible;
    let truncated_by_tool = total > files.len();

    bordered_row::render(
        out,
        ctx,
        border_style,
        format!("{pattern} ({visible} of {total})"),
        ctx.theme.dim(),
    );

    for path in &files[..visible] {
        let display = truncate_to_bytes(path, MAX_TOOL_OUTPUT_LINE_BYTES);
        bordered_row::render(out, ctx, border_style, display, ctx.theme.text());
    }

    if let Some(text) = footer_text(hidden, truncated_by_tool) {
        bordered_row::render(out, ctx, border_style, text, ctx.theme.dim());
    }
}

/// Footer combining TUI-side hiding (`hidden`) with the tool's
/// `MAX_RESULTS` cap. Header carries the actual counts; footer just
/// flags the cap with grep's `(limit reached)` token.
fn footer_text(hidden: usize, truncated_by_tool: bool) -> Option<String> {
    let noun = |n: usize| if n == 1 { "file" } else { "files" };
    match (hidden, truncated_by_tool) {
        (0, false) => None,
        (0, true) => Some("... limit reached".to_owned()),
        (n, false) => Some(format!("... +{n} {}", noun(n))),
        (n, true) => Some(format!("... +{n} {} (limit reached)", noun(n))),
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
    fn render_empty_files_shows_no_files_found_row() {
        // Empty result still emits a body row; pattern header is suppressed.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        render(&mut out, &ctx, "**/*.rs", &[], 0, false);

        assert_eq!(out.len(), 1);
        let body = collect_text(&out);
        assert!(body.contains("No files found"), "body: {body}");
        assert!(!body.contains("**/*.rs"), "no header on empty: {body}");
    }

    #[test]
    fn render_short_list_emits_header_and_no_footer() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        let files = vec!["src/main.rs".to_owned(), "src/lib.rs".to_owned()];
        render(&mut out, &ctx, "**/*.rs", &files, 2, false);

        assert_eq!(
            out.len(),
            3,
            "expected header + two path rows, no footer: {out:#?}",
        );
        let body = collect_text(&out);
        assert!(body.contains("**/*.rs (2 of 2)"), "header text: {body}");
        assert!(body.contains("src/main.rs"));
        assert!(body.contains("src/lib.rs"));
        assert!(!body.contains("..."));
    }

    #[test]
    fn render_caps_at_max_output_lines_with_hidden_footer() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        let files: Vec<String> = (0..MAX_TOOL_OUTPUT_LINES + 3)
            .map(|i| format!("f{i}.rs"))
            .collect();
        render(&mut out, &ctx, "**/*.rs", &files, files.len(), false);

        // Header + MAX_TOOL_OUTPUT_LINES body rows + one footer row.
        assert_eq!(out.len(), MAX_TOOL_OUTPUT_LINES + 2);
        let body = collect_text(&out);
        assert!(
            body.contains(&format!(
                "**/*.rs ({MAX_TOOL_OUTPUT_LINES} of {})",
                MAX_TOOL_OUTPUT_LINES + 3
            )),
            "header should report visible / total: {body}",
        );
        for i in 0..MAX_TOOL_OUTPUT_LINES {
            assert!(body.contains(&format!("f{i}.rs")), "row {i} hidden: {body}");
        }
        assert!(
            !body.contains(&format!("f{MAX_TOOL_OUTPUT_LINES}.rs")),
            "row past cap should be hidden: {body}",
        );
        assert!(body.contains("... +3 files"), "footer text: {body}");
    }

    #[test]
    fn render_error_flag_swaps_border_style() {
        // Pins `is_error` flowing into `border_style_for`.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let files = vec!["a.rs".to_owned()];

        let mut ok = Vec::new();
        render(&mut ok, &ctx, "**/*.rs", &files, 1, false);
        let mut err = Vec::new();
        render(&mut err, &ctx, "**/*.rs", &files, 1, true);

        assert_eq!(ok.len(), err.len());
        assert_ne!(
            ok[0].spans[0].style, err[0].spans[0].style,
            "is_error should swap the bar style",
        );
    }

    #[test]
    fn render_tool_truncation_surfaces_total_in_footer() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        let returned = MAX_TOOL_OUTPUT_LINES + 5;
        let files: Vec<String> = (0..returned).map(|i| format!("f{i}.rs")).collect();
        render(&mut out, &ctx, "**/*.rs", &files, 1234, false);

        let body = collect_text(&out);
        assert!(
            body.contains(&format!("**/*.rs ({MAX_TOOL_OUTPUT_LINES} of 1234)")),
            "header should anchor the body to the input pattern: {body}",
        );
        assert!(
            body.contains("... +5 files (limit reached)"),
            "footer: {body}",
        );
    }

    // ── footer_text ──

    #[test]
    fn footer_text_no_hidden_no_truncation_returns_none() {
        assert_eq!(footer_text(0, false), None);
    }

    #[test]
    fn footer_text_hidden_uses_singular_or_plural() {
        assert_eq!(footer_text(1, false), Some("... +1 file".to_owned()));
        assert_eq!(footer_text(3, false), Some("... +3 files".to_owned()));
    }

    #[test]
    fn footer_text_tool_truncated_with_no_tui_hidden_names_limit() {
        // Defensive arm: in practice MAX_RESULTS (100) is well above
        // MAX_TOOL_OUTPUT_LINES (5), so this combination is unreachable.
        assert_eq!(footer_text(0, true), Some("... limit reached".to_owned()));
    }

    #[test]
    fn footer_text_combines_hidden_count_with_limit_token_when_tool_truncated() {
        assert_eq!(
            footer_text(95, true),
            Some("... +95 files (limit reached)".to_owned()),
        );
    }
}
