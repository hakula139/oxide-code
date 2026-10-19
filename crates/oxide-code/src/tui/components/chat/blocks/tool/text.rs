//! Default tool-result body: plain text, capped in the compact view with a `... +N lines` footer.

use ratatui::style::Style;

use super::{MAX_TOOL_OUTPUT_LINES, overflow_footer};
use crate::tui::components::chat::blocks::rows::body_row;
use crate::tui::components::chat::blocks::{BlockRows, RenderCtx};
use crate::tui::wrap::expand_tabs;

pub(super) fn render(
    out: &mut BlockRows,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    content: &str,
    label: &str,
) {
    let lines: Vec<&str> = content.lines().collect();
    let has_text = |line: &&str| !line.trim().is_empty();
    let (Some(first), Some(last)) = (
        lines.iter().position(has_text),
        lines.iter().rposition(has_text),
    ) else {
        return;
    };
    let mut output_lines = &lines[first..=last];
    if output_lines
        .first()
        .is_some_and(|l| l.trim() == label.trim())
    {
        output_lines = &output_lines[1..];
    }
    if output_lines.is_empty() {
        return;
    }

    let text_style = ctx.theme.dim();
    let shown = output_lines.len().min(ctx.cap(MAX_TOOL_OUTPUT_LINES));

    for text_line in &output_lines[..shown] {
        let display_text = ctx.cap_row(&expand_tabs(text_line));
        body_row(&mut out.lines, ctx, border_style, display_text, text_style);
    }

    let hidden = output_lines.len() - shown;
    overflow_footer(out, ctx, border_style, hidden, "line", "lines", false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::components::chat::blocks::rows::MAX_ROW_BYTES;
    use crate::tui::glyphs::TOOL_BORDER_CONT;
    use crate::tui::theme::Theme;

    mod render {
        use super::*;

        /// Rows of `content` under `label` in the compact or expanded view, wide enough that no row
        /// wraps.
        fn render_rows(content: &str, label: &str, expanded: bool) -> Vec<String> {
            let theme = Theme::default();
            let ctx = RenderCtx {
                width: 4096,
                theme: &theme,
                show_thinking: false,
                expanded,
                expand_hint: None,
            };
            let mut out = BlockRows::default();
            render(&mut out, &ctx, theme.tool_border(), content, label);
            out.lines.iter().map(ToString::to_string).collect()
        }

        fn body(texts: &[&str]) -> Vec<String> {
            texts
                .iter()
                .map(|text| format!("{TOOL_BORDER_CONT}{text}"))
                .collect()
        }

        #[test]
        fn caps_rows_and_counts_the_hidden_ones() {
            let shown: Vec<String> = (0..MAX_TOOL_OUTPUT_LINES)
                .map(|i| format!("line {i}"))
                .collect();
            for (extra, footer) in [
                (0, None),
                (1, Some("... +1 line")),
                (5, Some("... +5 lines")),
            ] {
                let content = (0..MAX_TOOL_OUTPUT_LINES + extra)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");

                let rows = render_rows(&content, "result", false);

                let mut expected: Vec<&str> = shown.iter().map(String::as_str).collect();
                expected.extend(footer);
                assert_eq!(rows, body(&expected), "{extra} extra lines");
            }
        }

        #[test]
        fn expanded_view_shows_every_row() {
            let lines: Vec<String> = (0..MAX_TOOL_OUTPUT_LINES + 3)
                .map(|i| format!("line {i}"))
                .collect();

            let rows = render_rows(&lines.join("\n"), "result", true);

            let expected: Vec<&str> = lines.iter().map(String::as_str).collect();
            assert_eq!(rows, body(&expected));
        }

        #[test]
        fn blank_content_emits_nothing() {
            for content in ["", "  \n  "] {
                assert_eq!(
                    render_rows(content, "result", false),
                    body(&[]),
                    "{content:?}"
                );
            }
        }

        #[test]
        fn drops_a_first_line_that_repeats_the_label() {
            let cases: [(&str, &[&str]); 4] = [
                ("Found 2 files\na.rs\nb.rs", &["a.rs", "b.rs"]),
                ("  Found 2 files  \na.rs", &["a.rs"]),
                (
                    "Found 2 files in cache\na.rs",
                    &["Found 2 files in cache", "a.rs"],
                ),
                ("Found 2 files", &[]),
            ];
            for (content, texts) in cases {
                assert_eq!(
                    render_rows(content, "Found 2 files", false),
                    body(texts),
                    "{content:?}"
                );
            }
        }

        #[test]
        fn trims_blank_edge_lines_and_keeps_indents() {
            let cases: [(&str, &[&str]); 3] = [
                ("\n\n real line\n\n\n", &[" real line"]),
                (" a.rs | 1 +\n b.rs | 2 +", &[" a.rs | 1 +", " b.rs | 2 +"]),
                ("first\n\nlast", &["first", "", "last"]),
            ];
            for (content, texts) in cases {
                assert_eq!(
                    render_rows(content, "out", false),
                    body(texts),
                    "{content:?}"
                );
            }
        }

        #[test]
        fn cuts_a_long_line_to_the_byte_budget_until_expanded() {
            let long_line = "x".repeat(MAX_ROW_BYTES + 100);

            let compact = render_rows(&long_line, "result", false);
            let expanded = render_rows(&long_line, "result", true);

            let kept = format!("{}...", "x".repeat(MAX_ROW_BYTES));
            assert_eq!(compact, body(&[&kept]));
            assert_eq!(expanded, body(&[&long_line]));
        }
    }
}
