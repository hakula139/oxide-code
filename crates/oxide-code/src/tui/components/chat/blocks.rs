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
use unicode_width::UnicodeWidthStr;

use crate::tui::theme::Theme;
use crate::tui::wrap::wrap_line;

// ── Trait ──

/// Immutable context passed to [`ChatBlock::render`].
pub(super) struct RenderCtx<'a> {
    pub(super) width: u16,
    pub(super) theme: &'a Theme,
    pub(super) show_thinking: bool,
}

/// A single renderable unit in the chat history.
pub(super) trait ChatBlock {
    /// Render this block's lines for the given context.
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>>;

    /// Whether the block wants breathing room (a blank line) on both
    /// sides. Standalone blocks (user, assistant, thinking, error)
    /// return `true`; tool-group blocks (call, result) return `false`
    /// so the call and its output hug. The container de-duplicates
    /// adjacent blank lines, so two standalone blocks in a row produce
    /// exactly one blank between them.
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
    /// scraping rendered glyphs. Kept on the always-compiled trait
    /// surface so the vtable shape is consistent between test and
    /// release builds.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "consumed only by ChatView::last_is_error in tests"
        )
    )]
    fn is_error_marker(&self) -> bool {
        false
    }
}

// ── Shared Helpers ──

/// Pushes a line with a styled first-line icon prefix and wraps
/// continuation to a matching-width space indent. Used by the bar-less
/// blocks (user, assistant, thinking body, error) so their wrapped
/// content aligns under the text, not under the icon itself.
///
/// `prefix` is measured in display columns (not bytes) so multi-byte
/// icons like `❯` wrap correctly.
pub(super) fn push_icon_wrapped(
    out: &mut Vec<Line<'static>>,
    prefix: &str,
    prefix_style: Style,
    text: &str,
    text_style: Style,
    width: usize,
) {
    let indent = prefix.width();
    let cont_prefix = vec![Span::raw(" ".repeat(indent))];
    let line = Line::from(vec![
        Span::styled(prefix.to_owned(), prefix_style),
        Span::styled(text.to_owned(), text_style),
    ]);
    out.extend(wrap_line(line, width, indent, Some(&cont_prefix)));
}

/// Prepends a styled prefix span to a markdown-rendered line. Used by
/// the bar-less markdown blocks (assistant text, streaming, thinking
/// body) for per-line first-column decoration (icon on line 1, plain
/// indent on continuations).
pub(super) fn prepend_markdown_prefix(
    line: Line<'static>,
    prefix: &str,
    prefix_style: Style,
) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix.to_owned(), prefix_style)];
    spans.extend(line.spans);
    Line::from(spans)
}

/// Whether the last rendered line is non-empty. Used to decide where
/// to insert blank-line separators between blocks — adjacent blanks
/// collapse to one.
pub(super) fn last_has_width(lines: &[Line<'_>]) -> bool {
    lines.last().is_some_and(|l| l.width() > 0)
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use super::*;

    // ── push_icon_wrapped ──

    #[test]
    fn push_icon_wrapped_short_text_single_line() {
        let mut out = Vec::new();
        push_icon_wrapped(
            &mut out,
            "❯ ",
            Style::default().fg(Color::Red),
            "hello",
            Style::default(),
            80,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].spans.len(), 2);
        assert_eq!(out[0].spans[0].content, "❯ ");
        assert_eq!(out[0].spans[1].content, "hello");
    }

    #[test]
    fn push_icon_wrapped_wraps_long_text_with_indent() {
        let mut out = Vec::new();
        push_icon_wrapped(
            &mut out,
            "❯ ",
            Style::default(),
            "one two three four five six seven",
            Style::default(),
            16,
        );
        assert!(out.len() >= 2, "should wrap: {out:?}");
        // Continuation starts with a 2-col space indent (width of "❯ ").
        let cont = &out[1];
        assert_eq!(cont.spans[0].content.as_ref(), "  ");
    }

    #[test]
    fn push_icon_wrapped_uses_display_width_not_bytes() {
        // `❯ ` is 4 bytes (3 for ❯ + 1 for space) but 2 display columns.
        // Continuation indent must use columns, not bytes.
        let mut out = Vec::new();
        push_icon_wrapped(
            &mut out,
            "❯ ",
            Style::default(),
            "one two three four five",
            Style::default(),
            12,
        );
        assert!(out.len() >= 2);
        assert_eq!(out[1].spans[0].content.as_ref(), "  ");
    }

    // ── prepend_markdown_prefix ──

    #[test]
    fn prepend_markdown_prefix_adds_styled_prefix() {
        let line = Line::from(vec![Span::raw("content")]);
        let result = prepend_markdown_prefix(line, "◉ ", Style::default().fg(Color::Blue));
        assert_eq!(result.spans.len(), 2);
        assert_eq!(result.spans[0].content, "◉ ");
        assert_eq!(result.spans[0].style.fg, Some(Color::Blue));
        assert_eq!(result.spans[1].content, "content");
    }
}
