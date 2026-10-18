//! `read` tool body: a line-numbered excerpt under a path and range header.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{
    MAX_TOOL_OUTPUT_LINES, NumberedRows, RenderCtx, TOOL_BORDER_CONT, body_row, border_style_for,
};
use crate::tool::ReadExcerptLine;

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    path: &str,
    lines: &[ReadExcerptLine],
    total_lines: usize,
    is_error: bool,
) {
    let border_style = border_style_for(ctx.theme, is_error);
    let context = context_label(path, lines, total_lines);
    body_row(out, ctx, border_style, context, ctx.theme.dim());
    if lines.is_empty() {
        return;
    }

    let visible = if lines.len() > MAX_TOOL_OUTPUT_LINES {
        &lines[..MAX_TOOL_OUTPUT_LINES]
    } else {
        lines
    };
    let line_number_width = visible
        .iter()
        .map(|line| line.number.to_string().width())
        .max()
        .unwrap_or(1);
    let rows = NumberedRows::new(ctx, border_style, line_number_width);

    for line in visible {
        rows.render(out, line.number, &line.text, ctx.theme.text());
    }

    if lines.len() > MAX_TOOL_OUTPUT_LINES {
        let hidden = lines.len() - MAX_TOOL_OUTPUT_LINES;
        let noun = if hidden == 1 { "line" } else { "lines" };
        out.push(Line::from(vec![
            Span::styled(TOOL_BORDER_CONT.to_owned(), border_style),
            Span::styled(format!("... +{hidden} {noun}"), ctx.theme.dim()),
        ]));
    }
}

fn context_label(path: &str, lines: &[ReadExcerptLine], total_lines: usize) -> String {
    let Some(first) = lines.first() else {
        return format!("{path} (empty file)");
    };
    let last = lines.last().unwrap_or(first);
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
    use crate::tui::theme::Theme;

    fn render_rows(path: &str, lines: &[ReadExcerptLine], total_lines: usize) -> Vec<String> {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: false,
        };
        let mut out = Vec::new();
        render(&mut out, &ctx, path, lines, total_lines, false);
        out.iter().map(ToString::to_string).collect()
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

    // ── render ──

    #[test]
    fn render_numbers_rows_under_the_path_and_range_header() {
        let rows = render_rows("/tmp/example.rs", &excerpt(9..=10), 12);

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
    fn render_caps_rows_and_counts_the_hidden_ones() {
        for (extra, footer) in [(1, "... +1 line"), (2, "... +2 lines")] {
            let total = MAX_TOOL_OUTPUT_LINES + extra;

            let rows = render_rows("/tmp/example.rs", &excerpt(1..=total), total);

            let mut expected = vec![format!("▎   /tmp/example.rs:1-{total}")];
            expected.extend((1..=MAX_TOOL_OUTPUT_LINES).map(|n| format!("▎   {n} │ line {n}")));
            expected.push(format!("▎   {footer}"));
            assert_eq!(rows, expected, "{extra} extra lines");
        }
    }

    #[test]
    fn render_empty_file_keeps_only_the_header() {
        let rows = render_rows("/tmp/empty.rs", &[], 0);

        assert_eq!(rows, ["▎   /tmp/empty.rs (empty file)"]);
    }

    // ── context_label ──

    #[test]
    fn context_label_full_file_omits_total_suffix() {
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
    fn context_label_single_line_uses_single_number() {
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
