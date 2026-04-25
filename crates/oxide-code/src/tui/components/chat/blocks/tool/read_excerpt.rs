//! `read` tool body — line-numbered excerpt with a path / range header
//! that summarizes which slice of the file the model just looked at.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::super::RenderCtx;
use super::{
    MAX_TOOL_OUTPUT_LINE_BYTES, MAX_TOOL_OUTPUT_LINES, STATUS_LINE_CONT,
    border_continuation_prefix, border_style_for, truncate_to_bytes,
};
use crate::tool::ReadExcerptLine;
use crate::tui::wrap::{expand_tabs, wrap_line};

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    path: &str,
    lines: &[ReadExcerptLine],
    total_lines: usize,
    is_error: bool,
) {
    let border_style = border_style_for(ctx.theme, is_error);
    let width = usize::from(ctx.width);
    let status_cont_prefix = border_continuation_prefix(STATUS_LINE_CONT, border_style);
    let context = context_label(path, lines, total_lines);
    let context_line = Line::from(vec![
        Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
        Span::styled(context, ctx.theme.dim()),
    ]);
    out.extend(wrap_line(
        context_line,
        width,
        STATUS_LINE_CONT.width(),
        Some(&status_cont_prefix),
    ));
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
    let line_cont_prefix = format!("{STATUS_LINE_CONT}{}   ", " ".repeat(line_number_width));
    let line_cont_spans = border_continuation_prefix(&line_cont_prefix, border_style);

    for line in visible {
        let expanded = expand_tabs(&line.text);
        let display_text = truncate_to_bytes(&expanded, MAX_TOOL_OUTPUT_LINE_BYTES);
        let line_number = format!("{:>width$}", line.number, width = line_number_width);
        let rendered = Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled(line_number, ctx.theme.muted()),
            Span::styled(" │ ", ctx.theme.dim()),
            Span::styled(display_text, ctx.theme.text()),
        ]);
        out.extend(wrap_line(
            rendered,
            width,
            line_cont_prefix.width(),
            Some(&line_cont_spans),
        ));
    }

    if lines.len() > MAX_TOOL_OUTPUT_LINES {
        let hidden = lines.len() - MAX_TOOL_OUTPUT_LINES;
        let noun = if hidden == 1 { "line" } else { "lines" };
        out.push(Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
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
    use super::*;

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
