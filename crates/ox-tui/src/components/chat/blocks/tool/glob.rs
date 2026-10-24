//! `glob` body: a `pattern (visible of total)` header over a flat path list. The footer flags
//! the tool's `MAX_RESULTS` cap when the search hits it.

use ratatui::style::Style;

use ox_core::util::text::escape_line;

use super::{MAX_TOOL_OUTPUT_LINES, overflow_footer};
use crate::components::chat::blocks::rows::body_row;
use crate::components::chat::blocks::{BlockRows, RenderCtx};

pub(super) fn render(
    out: &mut BlockRows,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    pattern: &str,
    files: &[String],
    total: usize,
) {
    if files.is_empty() {
        // "No files found" already labels the result, so the header is skipped.
        body_row(
            &mut out.lines,
            ctx,
            border_style,
            "No files found",
            ctx.theme.dim(),
        );
        return;
    }

    let visible = files.len().min(ctx.cap(MAX_TOOL_OUTPUT_LINES));
    let hidden = files.len() - visible;
    let truncated_by_tool = total > files.len();

    body_row(
        &mut out.lines,
        ctx,
        border_style,
        format!("{pattern} ({visible} of {total})"),
        ctx.theme.dim(),
    );

    for path in &files[..visible] {
        let display = ctx.cap_row(&escape_line(path));
        body_row(&mut out.lines, ctx, border_style, display, ctx.theme.text());
    }

    overflow_footer(
        out,
        ctx,
        border_style,
        hidden,
        "file",
        "files",
        truncated_by_tool,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    mod render {
        use super::*;

        /// Rows of `files` from a `pattern` search that found `total`, in the compact or expanded
        /// view.
        fn render_rows(
            pattern: &str,
            files: &[String],
            total: usize,
            expanded: bool,
        ) -> Vec<String> {
            let theme = Theme::default();
            let ctx = RenderCtx {
                expanded,
                ..RenderCtx::new(80, &theme)
            };
            let mut out = BlockRows::default();
            render(&mut out, &ctx, theme.tool_border(), pattern, files, total);
            out.lines.iter().map(ToString::to_string).collect()
        }

        fn numbered_files(count: usize) -> Vec<String> {
            (0..count).map(|i| format!("f{i}.rs")).collect()
        }

        #[test]
        fn empty_files_shows_no_files_found_row() {
            let rows = render_rows("**/*.rs", &[], 0, false);

            assert_eq!(rows, ["▎   No files found"]);
        }

        #[test]
        fn short_list_emits_header_and_no_footer() {
            let files = vec!["src/main.rs".to_owned(), "src/lib.rs".to_owned()];

            let rows = render_rows("**/*.rs", &files, 2, false);

            assert_eq!(
                rows,
                ["▎   **/*.rs (2 of 2)", "▎   src/main.rs", "▎   src/lib.rs"]
            );
        }

        #[test]
        fn counts_native_paths_without_splitting_their_newlines() {
            let files = vec![
                "src/line\nname.rs".to_owned(),
                "src/name:12:part.rs".to_owned(),
            ];

            let rows = render_rows("**/*.rs", &files, 2, false);

            assert_eq!(
                rows,
                [
                    "▎   **/*.rs (2 of 2)",
                    r"▎   src/line\nname.rs",
                    "▎   src/name:12:part.rs"
                ],
            );
        }

        #[test]
        fn caps_at_max_output_lines_with_hidden_footer() {
            let files = numbered_files(8);

            let rows = render_rows("**/*.rs", &files, 8, false);

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

            let rows = render_rows("**/*.rs", &files, 1234, false);

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

        #[test]
        fn expanded_view_lists_every_file_and_still_flags_the_tool_limit() {
            let files = numbered_files(7);

            let rows = render_rows("**/*.rs", &files, 1234, true);

            let mut expected = vec!["▎   **/*.rs (7 of 1234)".to_owned()];
            expected.extend(files.iter().map(|file| format!("▎   {file}")));
            expected.push("▎   ... limit reached".to_owned());
            assert_eq!(rows, expected);
        }
    }
}
