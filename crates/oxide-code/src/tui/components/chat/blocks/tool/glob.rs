//! `glob` body: a `pattern (visible of total)` header over a flat path list. The footer flags
//! the tool's `MAX_RESULTS` cap when the search hits it.

use ratatui::text::Line;

use super::{
    MAX_ROW_BYTES, MAX_TOOL_OUTPUT_LINES, RenderCtx, body_row, border_style_for, truncate_to_bytes,
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
        // "No files found" already labels the result, so the header is skipped.
        body_row(out, ctx, border_style, "No files found", ctx.theme.dim());
        return;
    }

    let visible = files.len().min(MAX_TOOL_OUTPUT_LINES);
    let hidden = files.len() - visible;
    let truncated_by_tool = total > files.len();

    body_row(
        out,
        ctx,
        border_style,
        format!("{pattern} ({visible} of {total})"),
        ctx.theme.dim(),
    );

    for path in &files[..visible] {
        let display = truncate_to_bytes(path, MAX_ROW_BYTES);
        body_row(out, ctx, border_style, display, ctx.theme.text());
    }

    if let Some(text) = footer_text(hidden, truncated_by_tool) {
        body_row(out, ctx, border_style, text, ctx.theme.dim());
    }
}

/// Footer combining TUI-side row hiding with the tool's `MAX_RESULTS` cap. Counts live in the
/// header, and the footer flags the cap with grep's `(limit reached)` token.
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
        // An empty result still emits a body row but drops the pattern header.
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
    fn render_error_flag_paints_the_bar_in_the_error_color() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let files = vec!["a.rs".to_owned()];

        for (is_error, bar_style) in [(false, theme.tool_border()), (true, theme.error())] {
            let mut out = Vec::new();

            render(&mut out, &ctx, "**/*.rs", &files, 1, is_error);

            assert_eq!(out.len(), 2, "header and one path: {out:?}");
            for line in &out {
                assert_eq!(
                    line.spans[0].style, bar_style,
                    "is_error={is_error}: {line:?}"
                );
            }
        }
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
    fn footer_text_no_hidden_no_truncation_is_none() {
        assert_eq!(footer_text(0, false), None);
    }

    #[test]
    fn footer_text_hidden_uses_singular_or_plural() {
        assert_eq!(footer_text(1, false), Some("... +1 file".to_owned()));
        assert_eq!(footer_text(3, false), Some("... +3 files".to_owned()));
    }

    #[test]
    fn footer_text_tool_truncated_with_no_tui_hidden_names_limit() {
        // Defensive arm: MAX_RESULTS (100) far exceeds MAX_TOOL_OUTPUT_LINES (5) in practice.
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
