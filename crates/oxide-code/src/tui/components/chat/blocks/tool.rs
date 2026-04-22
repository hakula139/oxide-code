//! Tool call and tool result blocks.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{BORDER_PREFIX, ChatBlock, RenderCtx, border_continuation_prefix};
use crate::tui::theme::Theme;
use crate::tui::wrap::{expand_tabs, wrap_line};

/// Border prefix for tool result status lines (indicator + label).
const TOOL_RESULT_PREFIX: &str = "  ▎   ";

/// Border prefix for tool output body lines.
const TOOL_OUTPUT_PREFIX: &str = "  ▎     ";

/// Maximum lines of tool output shown inline before truncation.
const MAX_TOOL_OUTPUT_LINES: usize = 5;

/// Maximum characters per tool output line before horizontal truncation.
const MAX_TOOL_OUTPUT_LINE_CHARS: usize = 512;

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
        let cont_prefix = border_continuation_prefix(TOOL_RESULT_PREFIX, border_style);
        let line = Line::from(vec![
            Span::styled(BORDER_PREFIX.to_owned(), border_style),
            Span::styled(self.icon.to_owned(), ctx.theme.tool_icon()),
            Span::raw(" "),
            Span::styled(self.label.clone(), ctx.theme.text()),
        ]);
        wrap_line(
            line,
            usize::from(ctx.width),
            TOOL_RESULT_PREFIX.len(),
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

// ── Error Display Variant ──

/// Render a status line with success / error indicator and label. Shared
/// between [`ToolResultBlock`] and [`super::ErrorBlock`] so the visual
/// language stays consistent.
pub(super) fn render_status_line(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    label: &str,
    is_error: bool,
) {
    let (indicator, indicator_style) = if is_error {
        ("✗", ctx.theme.error())
    } else {
        ("✓", ctx.theme.success())
    };
    let border_style = border_style_for(ctx.theme, is_error);
    let cont_prefix = border_continuation_prefix(TOOL_OUTPUT_PREFIX, border_style);
    let line = Line::from(vec![
        Span::styled(TOOL_RESULT_PREFIX.to_owned(), border_style),
        Span::styled(indicator, indicator_style),
        Span::raw(" "),
        Span::styled(label.to_owned(), ctx.theme.muted()),
    ]);
    for wrapped in wrap_line(
        line,
        usize::from(ctx.width),
        TOOL_OUTPUT_PREFIX.len(),
        Some(&cont_prefix),
    ) {
        out.push(wrapped);
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
    let cont_prefix = border_continuation_prefix(TOOL_OUTPUT_PREFIX, border_style);
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
        let display_text = truncate_to_chars(&expanded, MAX_TOOL_OUTPUT_LINE_CHARS);
        let line = Line::from(vec![
            Span::styled(TOOL_OUTPUT_PREFIX.to_owned(), border_style),
            Span::styled(display_text, text_style),
        ]);
        for wrapped in wrap_line(line, width, TOOL_OUTPUT_PREFIX.len(), Some(&cont_prefix)) {
            out.push(wrapped);
        }
    }

    if truncated {
        let n = output_lines.len() - MAX_TOOL_OUTPUT_LINES;
        let label = if n == 1 { "line" } else { "lines" };
        out.push(Line::from(vec![
            Span::styled(TOOL_OUTPUT_PREFIX.to_owned(), border_style),
            Span::styled(format!("... {n} more {label}"), ctx.theme.dim()),
        ]));
    }
}

fn border_style_for(theme: &Theme, is_error: bool) -> Style {
    if is_error {
        theme.error()
    } else {
        theme.tool_border()
    }
}

/// Truncates a string to `max_chars` characters, appending `...` if cut.
fn truncate_to_chars(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_owned();
    }
    let boundary = s.floor_char_boundary(max_chars);
    format!("{}...", &s[..boundary])
}
