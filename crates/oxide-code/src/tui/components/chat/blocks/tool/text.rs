//! Default tool-result body: truncated text, with a `... +N lines` footer on overflow.

use ratatui::style::Style;
use ratatui::text::Line;

use super::{MAX_TOOL_OUTPUT_LINES, hidden_footer};
use crate::tui::components::chat::blocks::RenderCtx;
use crate::tui::components::chat::blocks::rows::{MAX_ROW_BYTES, body_row, truncate_to_bytes};
use crate::tui::wrap::expand_tabs;

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
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
    let visible = &output_lines[..output_lines.len().min(MAX_TOOL_OUTPUT_LINES)];

    for text_line in visible {
        let expanded = expand_tabs(text_line);
        let display_text = truncate_to_bytes(&expanded, MAX_ROW_BYTES);
        body_row(out, ctx, border_style, display_text, text_style);
    }

    let hidden = output_lines.len() - visible.len();
    if let Some(footer) = hidden_footer(hidden, "line", "lines", false) {
        body_row(out, ctx, border_style, footer, ctx.theme.dim());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::glyphs::TOOL_BORDER_CONT;
    use crate::tui::theme::Theme;

    mod render {
        use super::*;

        /// Rows of `content` under `label`, wide enough that no row wraps.
        fn render_rows(content: &str, label: &str) -> Vec<String> {
            let theme = Theme::default();
            let ctx = RenderCtx {
                width: 4096,
                theme: &theme,
                show_thinking: false,
            };
            let mut out = Vec::new();
            render(&mut out, &ctx, theme.tool_border(), content, label);
            out.iter().map(ToString::to_string).collect()
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

                let rows = render_rows(&content, "result");

                let mut expected: Vec<&str> = shown.iter().map(String::as_str).collect();
                expected.extend(footer);
                assert_eq!(rows, body(&expected), "{extra} extra lines");
            }
        }

        #[test]
        fn blank_content_emits_nothing() {
            for content in ["", "  \n  "] {
                assert_eq!(render_rows(content, "result"), body(&[]), "{content:?}");
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
                    render_rows(content, "Found 2 files"),
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
                assert_eq!(render_rows(content, "out"), body(texts), "{content:?}");
            }
        }

        #[test]
        fn truncates_a_long_line_to_the_byte_budget() {
            let long_line = "x".repeat(MAX_ROW_BYTES + 100);

            let rows = render_rows(&long_line, "result");

            let kept = format!("{}...", "x".repeat(MAX_ROW_BYTES));
            assert_eq!(rows, body(&[&kept]));
        }
    }
}
