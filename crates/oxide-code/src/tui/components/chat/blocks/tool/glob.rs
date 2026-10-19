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

    mod render {
        use super::*;

        /// Rows of `files` from a `pattern` search that found `total`.
        fn render_rows(pattern: &str, files: &[String], total: usize) -> Vec<String> {
            let theme = Theme::default();
            let ctx = RenderCtx {
                width: 80,
                theme: &theme,
                show_thinking: true,
            };
            let mut out = Vec::new();
            render(&mut out, &ctx, theme.tool_border(), pattern, files, total);
            out.iter().map(ToString::to_string).collect()
        }

        fn numbered_files(count: usize) -> Vec<String> {
            (0..count).map(|i| format!("f{i}.rs")).collect()
        }

        #[test]
        fn empty_files_shows_no_files_found_row() {
            let rows = render_rows("**/*.rs", &[], 0);

            assert_eq!(rows, ["▎   No files found"]);
        }

        #[test]
        fn short_list_emits_header_and_no_footer() {
            let files = vec!["src/main.rs".to_owned(), "src/lib.rs".to_owned()];

            let rows = render_rows("**/*.rs", &files, 2);

            assert_eq!(
                rows,
                ["▎   **/*.rs (2 of 2)", "▎   src/main.rs", "▎   src/lib.rs"]
            );
        }

        #[test]
        fn caps_at_max_output_lines_with_hidden_footer() {
            let files = numbered_files(8);

            let rows = render_rows("**/*.rs", &files, 8);

            assert_eq!(
                rows,
                [
                    "▎   **/*.rs (5 of 8)",
                    "▎   f0.rs",
                    "▎   f1.rs",
                    "▎   f2.rs",
                    "▎   f3.rs",
                    "▎   f4.rs",
                    "▎   ... +3 files",
                ]
            );
        }

        #[test]
        fn tool_truncation_surfaces_total_in_footer() {
            let files = numbered_files(10);

            let rows = render_rows("**/*.rs", &files, 1234);

            assert_eq!(
                rows,
                [
                    "▎   **/*.rs (5 of 1234)",
                    "▎   f0.rs",
                    "▎   f1.rs",
                    "▎   f2.rs",
                    "▎   f3.rs",
                    "▎   f4.rs",
                    "▎   ... +5 files (limit reached)",
                ]
            );
        }
    }
}
