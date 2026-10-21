//! `read` tool body: a line-numbered excerpt under a path and range header.

use ratatui::style::Style;

use ox_core::tool::ReadExcerptLine;

use super::{MAX_TOOL_OUTPUT_LINES, overflow_footer};
use crate::components::chat::blocks::rows::{NumberedRows, body_row};
use crate::components::chat::blocks::{BlockRows, RenderCtx};

pub(super) fn render(
    out: &mut BlockRows,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    path: &str,
    lines: &[ReadExcerptLine],
    total_lines: usize,
) {
    let context = context_label(path, lines, total_lines);
    body_row(&mut out.lines, ctx, border_style, context, ctx.theme.dim());
    if lines.is_empty() {
        return;
    }

    let visible = &lines[..lines.len().min(ctx.cap(MAX_TOOL_OUTPUT_LINES))];
    let max_number = visible.iter().map(|line| line.number).max().unwrap_or(0);
    let rows = NumberedRows::new(ctx, border_style, max_number);

    for line in visible {
        rows.render(&mut out.lines, line.number, &line.text, ctx.theme.text());
    }

    let hidden = lines.len() - visible.len();
    overflow_footer(out, ctx, border_style, hidden, "line", "lines", false);
}

fn context_label(path: &str, lines: &[ReadExcerptLine], total_lines: usize) -> String {
    let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
        return format!("{path} (empty file)");
    };
    let range = if first.number == last.number {
        first.number.to_string()
    } else {
        format!("{}-{}", first.number, last.number)
    };
    if first.number == 1 && last.number == total_lines {
        format!("{path}:{range}")
    } else {
        format!("{path}:{range} of {total_lines}")
    }
}

#[cfg(test)]
mod tests {
    use std::ops::RangeInclusive;

    use super::*;
    use crate::theme::Theme;

    mod render {
        use super::*;

        fn render_rows(
            path: &str,
            lines: &[ReadExcerptLine],
            total_lines: usize,
            expanded: bool,
        ) -> Vec<String> {
            let theme = Theme::default();
            let ctx = RenderCtx {
                expanded,
                ..RenderCtx::new(80, &theme)
            };
            let mut out = BlockRows::default();
            render(
                &mut out,
                &ctx,
                theme.tool_border(),
                path,
                lines,
                total_lines,
            );
            out.lines.iter().map(ToString::to_string).collect()
        }

        /// Excerpt lines numbered `numbers`, each reading `line {number}`.
        fn excerpt(numbers: RangeInclusive<usize>) -> Vec<ReadExcerptLine> {
            numbers
                .map(|number| ReadExcerptLine {
                    number,
                    text: format!("line {number}"),
                })
                .collect()
        }

        #[test]
        fn numbers_rows_under_the_path_and_range_header() {
            let rows = render_rows("/tmp/example.rs", &excerpt(9..=10), 12, false);

            assert_eq!(
                rows,
                [
                    "▎   /tmp/example.rs:9-10 of 12",
                    "▎    9 │ line 9",
                    "▎   10 │ line 10",
                ]
            );
        }

        #[test]
        fn caps_rows_and_counts_the_hidden_ones() {
            for (extra, footer) in [(1, "... +1 line"), (2, "... +2 lines")] {
                let total = MAX_TOOL_OUTPUT_LINES + extra;

                let rows = render_rows("/tmp/example.rs", &excerpt(1..=total), total, false);

                let mut expected = vec![format!("▎   /tmp/example.rs:1-{total}")];
                expected.extend((1..=MAX_TOOL_OUTPUT_LINES).map(|n| format!("▎   {n} │ line {n}")));
                expected.push(format!("▎   {footer}"));
                assert_eq!(rows, expected, "{extra} extra lines");
            }
        }

        #[test]
        fn expanded_view_numbers_every_row() {
            let total = MAX_TOOL_OUTPUT_LINES + 2;

            let rows = render_rows("/tmp/example.rs", &excerpt(1..=total), total, true);

            let mut expected = vec![format!("▎   /tmp/example.rs:1-{total}")];
            expected.extend((1..=total).map(|n| format!("▎   {n} │ line {n}")));
            assert_eq!(rows, expected);
        }

        #[test]
        fn empty_file_keeps_only_the_header() {
            let rows = render_rows("/tmp/empty.rs", &[], 0, false);

            assert_eq!(rows, ["▎   /tmp/empty.rs (empty file)"]);
        }
    }

    mod context_label {
        use super::*;

        #[test]
        fn full_file_omits_total_suffix() {
            let lines = vec![
                ReadExcerptLine {
                    number: 1,
                    text: "alpha".to_owned(),
                },
                ReadExcerptLine {
                    number: 2,
                    text: "beta".to_owned(),
                },
            ];

            assert_eq!(
                context_label("/tmp/example.rs", &lines, 2),
                "/tmp/example.rs:1-2"
            );
        }

        #[test]
        fn single_line_uses_single_number() {
            let lines = vec![ReadExcerptLine {
                number: 4,
                text: "delta".to_owned(),
            }];

            assert_eq!(
                context_label("/tmp/example.rs", &lines, 10),
                "/tmp/example.rs:4 of 10"
            );
        }
    }
}
