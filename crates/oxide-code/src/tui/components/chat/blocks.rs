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

pub(crate) use assistant::{AssistantText, AssistantThinking};
pub(crate) use error::ErrorBlock;
pub(crate) use streaming::StreamingAssistant;
pub(crate) use tool::{ToolCallBlock, ToolResultBlock};
pub(crate) use user::UserMessage;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::tui::theme::Theme;

// ── Shared Prefix Constants ──

/// Left bar character for bordered content.
pub(super) const BAR: &str = "▎";

/// Border prefix for continuation lines and non-first content lines.
pub(super) const BORDER_PREFIX: &str = "  ▎ ";

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
    fn visible(&self, ctx: &RenderCtx<'_>) -> bool {
        _ = ctx;
        true
    }

    /// Whether the block represents committed assistant prose. The
    /// [`StreamingAssistant`] block uses this to decide whether the next
    /// streaming tokens continue the current turn (no icon / gap) or
    /// begin a fresh one (icon + gap).
    fn continues_assistant_turn(&self) -> bool {
        false
    }
}

// ── Shared Helpers ──

/// Builds a continuation prefix that keeps the `▎` bar aligned under the
/// original prefix. For a prefix like `"  ▎ "` (4 cols), produces spans
/// `["  ", "▎", " "]` where the bar span is styled.
pub(super) fn border_continuation_prefix(prefix: &str, bar_style: Style) -> Vec<Span<'static>> {
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
pub(super) fn border_markdown_line(
    line: Line<'static>,
    prefix: &str,
    bar_style: Style,
) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix.to_owned(), bar_style)];
    spans.extend(line.spans);
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

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
