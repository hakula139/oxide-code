//! Chat render blocks.
//!
//! Each visible unit in the conversation — user messages, assistant
//! replies, tool calls and results, errors — implements [`ChatBlock`] in
//! its own module. The trait owns the left-edge style, icons, wrapping,
//! and per-type truncation; [`super::ChatView`] stacks `render` outputs
//! and inserts blank-line separators between blocks that request them.
//!
//! Adding a new block type (plan approval, task list, permission prompt)
//! means writing a new `impl ChatBlock` struct — no cascade through a
//! central `match`.

mod assistant;
mod error;
mod streaming;
mod tool;
mod user;

pub(super) use assistant::{AssistantText, AssistantThinking};
pub(super) use error::ErrorBlock;
pub(super) use streaming::StreamingAssistant;
pub(super) use tool::{ToolCallBlock, ToolResultBlock};
pub(super) use user::UserMessage;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::tui::theme::Theme;
use crate::tui::wrap::wrap_line;

// ── Shared Prefix Constants ──

/// Left bar character for bordered content.
const BAR: &str = "▎";

/// Border prefix for continuation lines and non-first content lines.
const BORDER_PREFIX: &str = "  ▎ ";

/// Border prefix for status lines (indicator + label).
const STATUS_LINE_PREFIX: &str = "  ▎   ";

/// Border prefix for status-body lines (tool output, wrapped label
/// continuation). Also used as the continuation prefix for the status
/// line itself so wrapped labels align under the indicator.
const STATUS_BODY_PREFIX: &str = "  ▎     ";

// ── Trait ──

/// Immutable context passed to [`ChatBlock::render`].
pub(crate) struct RenderCtx<'a> {
    pub(crate) width: u16,
    pub(crate) theme: &'a Theme,
    pub(crate) show_thinking: bool,
}

/// A single renderable unit in the chat history.
pub(crate) trait ChatBlock: Send + Sync {
    /// Render this block's lines for the given context.
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>>;

    /// Whether the block wants breathing room (a blank line) on both
    /// sides. Standalone blocks (user, assistant, thinking) return
    /// `true`; tool-group blocks (call, result) and error markers
    /// return `false` so they sit flush with their neighbors. The
    /// container de-duplicates adjacent blank lines, so two standalone
    /// blocks in a row produce exactly one blank between them.
    fn standalone(&self) -> bool {
        true
    }

    /// Whether the block is visible in the current context. Thinking
    /// blocks collapse to zero when `show_thinking` is off, keeping the
    /// conditional in the block itself rather than in the container.
    fn visible(&self, _ctx: &RenderCtx<'_>) -> bool {
        true
    }

    /// Whether the block represents committed assistant prose. The
    /// [`StreamingAssistant`] block uses this to decide whether the next
    /// streaming tokens continue the current turn (no icon / gap) or
    /// begin a fresh one (icon + gap).
    fn continues_assistant_turn(&self) -> bool {
        false
    }

    /// Whether this block is a fatal error marker. A dedicated
    /// predicate avoids `Any`-based downcasting just for tests: the
    /// parent module uses it to assert on error dispatch without
    /// scraping rendered glyphs.
    #[cfg(test)]
    fn is_error_marker(&self) -> bool {
        false
    }
}

// ── Shared Helpers ──

/// Builds a continuation prefix that keeps the `▎` bar aligned under the
/// original prefix. For a prefix like `"  ▎ "` (4 cols), produces spans
/// `["  ", "▎", " "]` where the bar span is styled.
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

/// Prepends a styled border prefix to a markdown-rendered line.
fn border_markdown_line(line: Line<'static>, prefix: &str, bar_style: Style) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix.to_owned(), bar_style)];
    spans.extend(line.spans);
    Line::from(spans)
}

/// Pushes a bordered single text line into `out`, wrapping to `width`.
/// Shared by blocks with the "styled bar prefix + styled text" shape:
/// user messages, thinking prose, tool output bodies.
fn push_bordered_wrapped(
    out: &mut Vec<Line<'static>>,
    prefix: &str,
    bar_style: Style,
    text: &str,
    text_style: Style,
    width: usize,
    cont_prefix: &[Span<'static>],
) {
    let line = Line::from(vec![
        Span::styled(prefix.to_owned(), bar_style),
        Span::styled(text.to_owned(), text_style),
    ]);
    out.extend(wrap_line(line, width, prefix.len(), Some(cont_prefix)));
}

/// Renders a status line with success / error indicator, styled label,
/// and wrapped continuation. Shared between [`ToolResultBlock`] and
/// [`ErrorBlock`] so their visual treatment stays consistent.
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
    let cont_prefix = border_continuation_prefix(STATUS_BODY_PREFIX, border_style);
    let line = Line::from(vec![
        Span::styled(STATUS_LINE_PREFIX.to_owned(), border_style),
        Span::styled(indicator, indicator_style),
        Span::raw(" "),
        Span::styled(label.to_owned(), ctx.theme.muted()),
    ]);
    out.extend(wrap_line(
        line,
        usize::from(ctx.width),
        STATUS_BODY_PREFIX.len(),
        Some(&cont_prefix),
    ));
}

fn border_style_for(theme: &Theme, is_error: bool) -> Style {
    if is_error {
        theme.error()
    } else {
        theme.tool_border()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── border_continuation_prefix ──

    #[test]
    fn border_continuation_prefix_preserves_bar_position() {
        let style = Style::default();
        let spans = border_continuation_prefix("  ▎ ", style);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "  ");
        assert_eq!(spans[1].content, BAR);
        assert_eq!(spans[2].content, " ");
    }

    #[test]
    fn border_continuation_prefix_without_bar_pads_with_spaces() {
        // Defensive fallback for any future prefix that doesn't contain
        // the bar — return plain spaces of the same visual width.
        let style = Style::default();
        let spans = border_continuation_prefix("    ", style);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "    ");
    }
}
