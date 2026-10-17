//! Default tool-result body: truncated text, with a `... +N lines` footer on overflow.

use ratatui::text::Line;

use super::super::RenderCtx;
use super::{
    MAX_TOOL_OUTPUT_LINE_BYTES, MAX_TOOL_OUTPUT_LINES, border_style_for, bordered_row,
    truncate_to_bytes,
};
use crate::tui::wrap::expand_tabs;

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    content: &str,
    label: &str,
    is_error: bool,
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

    let border_style = border_style_for(ctx.theme, is_error);
    let text_style = ctx.theme.dim();
    let truncated = output_lines.len() > MAX_TOOL_OUTPUT_LINES;
    let visible = if truncated {
        &output_lines[..MAX_TOOL_OUTPUT_LINES]
    } else {
        output_lines
    };

    for text_line in visible {
        let expanded = expand_tabs(text_line);
        let display_text = truncate_to_bytes(&expanded, MAX_TOOL_OUTPUT_LINE_BYTES);
        bordered_row::render(out, ctx, border_style, display_text, text_style);
    }

    if truncated {
        let n = output_lines.len() - MAX_TOOL_OUTPUT_LINES;
        let label = if n == 1 { "line" } else { "lines" };
        bordered_row::render(
            out,
            ctx,
            border_style,
            format!("... +{n} {label}"),
            ctx.theme.dim(),
        );
    }
}
