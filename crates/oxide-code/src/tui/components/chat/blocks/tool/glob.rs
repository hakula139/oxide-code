//! `glob` body: a `pattern (visible of total)` header over a flat path list. The footer flags
//! the tool's `MAX_RESULTS` cap when the search hits it.

use ratatui::style::Style;
use ratatui::text::Line;

use super::{MAX_TOOL_OUTPUT_LINES, hidden_footer};
use crate::tui::components::chat::blocks::RenderCtx;
use crate::tui::components::chat::blocks::rows::{MAX_ROW_BYTES, body_row, truncate_to_bytes};

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    pattern: &str,
    files: &[String],
    total: usize,
) {
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

    if let Some(footer) = hidden_footer(hidden, "file", "files", truncated_by_tool) {
        body_row(out, ctx, border_style, footer, ctx.theme.dim());
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
        render(&mut out, &ctx, theme.tool_border(), "**/*.rs", &[], 0);

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
        render(&mut out, &ctx, theme.tool_border(), "**/*.rs", &files, 2);

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
        render(
            &mut out,
            &ctx,
            theme.tool_border(),
            "**/*.rs",
            &files,
            files.len(),
        );

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
        render(&mut out, &ctx, theme.tool_border(), "**/*.rs", &files, 1234);

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
}
