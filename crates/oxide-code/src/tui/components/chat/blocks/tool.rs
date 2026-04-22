//! Tool call and tool result blocks.

use ratatui::text::{Line, Span};

use super::{
    BORDER_PREFIX, ChatBlock, RenderCtx, STATUS_BODY_PREFIX, STATUS_LINE_PREFIX,
    border_continuation_prefix, border_style_for, push_bordered_wrapped, render_status_line,
};
use crate::tui::wrap::{expand_tabs, wrap_line};

/// Maximum lines of tool output shown inline before truncation.
const MAX_TOOL_OUTPUT_LINES: usize = 5;

/// Maximum bytes per tool output line before horizontal truncation.
/// Measured in bytes (matched against `str::len`) rather than Unicode
/// characters — display width is already gated by the terminal width
/// budget; this cap exists to avoid pathological multi-kilobyte lines
/// pasted into tool output.
const MAX_TOOL_OUTPUT_LINE_BYTES: usize = 512;

// ── Tool Call ──

/// A running or completed tool invocation — one bordered line with the
/// tool icon and input summary.
pub(crate) struct ToolCallBlock {
    icon: &'static str,
    label: String,
}

impl ToolCallBlock {
    pub(crate) fn new(icon: &'static str, label: impl Into<String>) -> Self {
        Self {
            icon,
            label: label.into(),
        }
    }
}

impl ChatBlock for ToolCallBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let border_style = ctx.theme.tool_border();
        let cont_prefix = border_continuation_prefix(STATUS_LINE_PREFIX, border_style);
        let line = Line::from(vec![
            Span::styled(BORDER_PREFIX.to_owned(), border_style),
            Span::styled(self.icon.to_owned(), ctx.theme.tool_icon()),
            Span::raw(" "),
            Span::styled(self.label.clone(), ctx.theme.text()),
        ]);
        wrap_line(
            line,
            usize::from(ctx.width),
            STATUS_LINE_PREFIX.len(),
            Some(&cont_prefix),
        )
    }

    fn standalone(&self) -> bool {
        false
    }
}

// ── Tool Result ──

/// The outcome of a tool call — indicator (✓ / ✗), label, and a truncated
/// body preview.
pub(crate) struct ToolResultBlock {
    label: String,
    content: String,
    is_error: bool,
}

impl ToolResultBlock {
    pub(crate) fn new(
        label: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            label: label.into(),
            content: content.into(),
            is_error,
        }
    }
}

impl ChatBlock for ToolResultBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        render_status_line(&mut out, ctx, &self.label, self.is_error);
        render_output_body(&mut out, ctx, &self.content, self.is_error);
        out
    }

    fn standalone(&self) -> bool {
        false
    }
}

fn render_output_body(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    content: &str,
    is_error: bool,
) {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return;
    }

    let border_style = border_style_for(ctx.theme, is_error);
    let text_style = ctx.theme.dim();
    let cont_prefix = border_continuation_prefix(STATUS_BODY_PREFIX, border_style);
    let width = usize::from(ctx.width);

    let output_lines: Vec<&str> = trimmed.lines().collect();
    let truncated = output_lines.len() > MAX_TOOL_OUTPUT_LINES;
    let visible = if truncated {
        &output_lines[..MAX_TOOL_OUTPUT_LINES]
    } else {
        &output_lines
    };

    for text_line in visible {
        let expanded = expand_tabs(text_line);
        let display_text = truncate_to_bytes(&expanded, MAX_TOOL_OUTPUT_LINE_BYTES);
        push_bordered_wrapped(
            out,
            STATUS_BODY_PREFIX,
            border_style,
            &display_text,
            text_style,
            width,
            &cont_prefix,
        );
    }

    if truncated {
        let n = output_lines.len() - MAX_TOOL_OUTPUT_LINES;
        let label = if n == 1 { "line" } else { "lines" };
        out.push(Line::from(vec![
            Span::styled(STATUS_BODY_PREFIX.to_owned(), border_style),
            Span::styled(format!("... {n} more {label}"), ctx.theme.dim()),
        ]));
    }
}

/// Truncates a string to `max_bytes` bytes, appending `...` if cut.
/// Falls back to the nearest char boundary at or before `max_bytes` to
/// avoid splitting multi-byte UTF-8 sequences.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let boundary = s.floor_char_boundary(max_bytes);
    format!("{}...", &s[..boundary])
}
