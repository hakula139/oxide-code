//! Default tool-result body — truncated monospace text with a
//! `+N lines` footer when output overflows.

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
    if content.trim().is_empty() {
        return;
    }

    let border_style = border_style_for(ctx.theme, is_error);
    let text_style = ctx.theme.dim();

    // Preserve per-line leading whitespace — some tools (e.g., `git
    // diff --stat`) indent every line with a meaningful space that
    // `content.trim()` would strip off the first line only, producing
    // a misaligned first row. Drop whole blank surrounding lines
    // instead, one unit of work at a time.
    let mut output_lines: Vec<&str> = content.lines().collect();
    while output_lines.first().is_some_and(|l| l.trim().is_empty()) {
        output_lines.remove(0);
    }
    while output_lines.last().is_some_and(|l| l.trim().is_empty()) {
        output_lines.pop();
    }

    // Tools (grep, glob) commonly use their own summary line as both
    // the `title` metadata (shown in the status line) and the first
    // line of `content` (shown in the body) — the model needs the
    // summary to parse counts, but rendering both duplicates it on
    // screen. Skip the first body line when it matches the label
    // verbatim.
    if output_lines
        .first()
        .is_some_and(|l| l.trim() == label.trim())
    {
        output_lines.remove(0);
    }
    if output_lines.is_empty() {
        return;
    }
    let truncated = output_lines.len() > MAX_TOOL_OUTPUT_LINES;
    let visible = if truncated {
        &output_lines[..MAX_TOOL_OUTPUT_LINES]
    } else {
        &output_lines
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
