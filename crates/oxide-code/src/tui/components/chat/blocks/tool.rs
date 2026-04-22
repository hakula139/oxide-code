//! Tool call and tool result blocks.
//!
//! The tool group is the only chat block that keeps a left-edge bar —
//! it visually couples a call to its output and color-codes success /
//! error at the same time. Every other block (user, assistant, error)
//! uses the bar-less icon-prefix helpers in [`super`] and flushes to
//! col 0. The bar / border machinery therefore lives here, not in the
//! trait module, so it scopes to exactly the blocks that use it.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::{ChatBlock, RenderCtx};
use crate::tui::theme::Theme;
use crate::tui::wrap::{expand_tabs, wrap_line};

/// Maximum lines of tool output shown inline before truncation.
const MAX_TOOL_OUTPUT_LINES: usize = 5;

/// Maximum bytes per tool output line before horizontal truncation.
/// Measured in bytes (matched against `str::len`) rather than Unicode
/// characters — display width is already gated by the terminal width
/// budget; this cap exists to avoid pathological multi-kilobyte lines
/// pasted into tool output.
const MAX_TOOL_OUTPUT_LINE_BYTES: usize = 512;

/// Left bar character for tool blocks.
const BAR: &str = "▎";

/// First-line prefix for tool-call and tool-result status lines — bar +
/// space. Content sits at col 2.
const BORDER_PREFIX: &str = "▎ ";

/// Prefix for lines subordinate to the status header — wrapped tool
/// name / result label (when the header overflows) and tool output body
/// lines. Aligns content at col 4, past the `✓` / `✗` indicator, so the
/// body reads as a child of the status header rather than a peer.
const STATUS_LINE_CONT: &str = "▎   ";

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
        let cont_prefix = border_continuation_prefix(STATUS_LINE_CONT, border_style);
        let line = Line::from(vec![
            Span::styled(BORDER_PREFIX.to_owned(), border_style),
            Span::styled(self.icon.to_owned(), ctx.theme.tool_icon()),
            Span::raw(" "),
            Span::styled(self.label.clone(), ctx.theme.text()),
        ]);
        wrap_line(
            line,
            usize::from(ctx.width),
            STATUS_LINE_CONT.len(),
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
    let cont_prefix = border_continuation_prefix(STATUS_LINE_CONT, border_style);
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
        let line = Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled(display_text, text_style),
        ]);
        out.extend(wrap_line(
            line,
            width,
            STATUS_LINE_CONT.len(),
            Some(&cont_prefix),
        ));
    }

    if truncated {
        let n = output_lines.len() - MAX_TOOL_OUTPUT_LINES;
        let label = if n == 1 { "line" } else { "lines" };
        out.push(Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled(format!("... +{n} {label}"), ctx.theme.dim()),
        ]));
    }
}

/// Renders a status line with success / error indicator, styled label,
/// and wrapped continuation. Shared between the tool result status
/// header and any future bar-carrying block.
fn render_status_line(
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
    let cont_prefix = border_continuation_prefix(STATUS_LINE_CONT, border_style);
    let line = Line::from(vec![
        Span::styled(BORDER_PREFIX.to_owned(), border_style),
        Span::styled(indicator, indicator_style),
        Span::raw(" "),
        Span::styled(label.to_owned(), ctx.theme.muted()),
    ]);
    out.extend(wrap_line(
        line,
        usize::from(ctx.width),
        STATUS_LINE_CONT.len(),
        Some(&cont_prefix),
    ));
}

/// Builds a continuation prefix that keeps the `▎` bar aligned under
/// the original prefix. For a prefix like `"▎   "` (4 cols), produces
/// `["", "▎", "   "]` where the bar span is styled.
fn border_continuation_prefix(prefix: &str, bar_style: Style) -> Vec<Span<'static>> {
    if let Some(bar_pos) = prefix.find(BAR) {
        let left = &prefix[..bar_pos];
        let right = &prefix[bar_pos + BAR.len()..];
        vec![
            Span::raw(left.to_owned()),
            Span::styled(BAR, bar_style),
            Span::raw(right.to_owned()),
        ]
    } else {
        vec![Span::raw(" ".repeat(prefix.len()))]
    }
}

fn border_style_for(theme: &Theme, is_error: bool) -> Style {
    if is_error {
        theme.error()
    } else {
        theme.tool_border()
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

#[cfg(test)]
mod tests {
    use ratatui::style::Style;

    use super::*;

    // ── border_continuation_prefix ──

    #[test]
    fn border_continuation_prefix_preserves_bar_position() {
        let style = Style::default();
        let spans = border_continuation_prefix(BORDER_PREFIX, style);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "");
        assert_eq!(spans[1].content, BAR);
        assert_eq!(spans[2].content, " ");
    }

    #[test]
    fn border_continuation_prefix_without_bar_pads_with_spaces() {
        let style = Style::default();
        let spans = border_continuation_prefix("    ", style);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "    ");
    }
}
