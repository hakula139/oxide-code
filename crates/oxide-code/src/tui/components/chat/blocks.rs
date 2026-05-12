//! Chat render blocks — each conversation unit implements [`ChatBlock`].

mod assistant;
mod compacted;
mod error;
mod git_diff;
mod interrupted;
mod streaming;
mod system;
mod tool;
mod user;

pub(super) use assistant::{AssistantText, AssistantThinking};
pub(super) use compacted::CompactedBlock;
pub(super) use error::ErrorBlock;
pub(super) use git_diff::GitDiffBlock;
pub(super) use interrupted::InterruptedMarker;
pub(super) use streaming::StreamingAssistant;
pub(super) use system::SystemMessageBlock;
pub(super) use tool::{ToolCallBlock, ToolResultBlock};
pub(super) use user::UserMessage;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::tui::glyphs::BAR;
use crate::tui::theme::Theme;
use crate::tui::wrap::wrap_line;

// ── Trait ──

/// Per-render snapshot threaded through every [`ChatBlock::render`] call.
///
/// `width` is the inner content width (already stripped of any chrome). `show_thinking` lets
/// thinking blocks elide their content without removing themselves from the block list.
pub(super) struct RenderCtx<'a> {
    pub(super) width: u16,
    pub(super) theme: &'a Theme,
    pub(super) show_thinking: bool,
}

/// Categorizes a block for inter-block spacing decisions.
///
/// Adjacent `Call` + `Result` pairs render flush against each other so a tool call and its
/// result read as a single unit; `Other` blocks always get a blank-line separator.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockKind {
    Call,
    Result,
    Other,
}

/// Render contract for one chat block (assistant prose, tool call, error marker, etc.).
///
/// Implementations are pure functions of their state plus the [`RenderCtx`]; the chat view caches
/// nothing and may re-render any block on every frame.
pub(super) trait ChatBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>>;

    /// Standalone blocks get blank-line separators; tool call/result pairs hug.
    fn standalone(&self) -> bool {
        true
    }

    fn block_kind(&self) -> BlockKind {
        BlockKind::Other
    }

    fn visible(&self, _ctx: &RenderCtx<'_>) -> bool {
        true
    }

    /// True when this block is committed assistant prose, so a fresh streaming buffer can
    /// continue its turn without inserting a separator.
    fn continues_assistant_turn(&self) -> bool {
        false
    }

    #[cfg(test)]
    fn is_error_marker(&self) -> bool {
        false
    }

    #[cfg(test)]
    fn error_text(&self) -> Option<&str> {
        None
    }

    #[cfg(test)]
    fn system_text(&self) -> Option<&str> {
        None
    }
}

// ── Shared Helpers ──

/// Icon-prefixed line with continuation indent aligned to text start.
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

/// Prepends a styled prefix span, preserving `Line.style` for code blocks.
pub(super) fn prepend_markdown_prefix(
    line: Line<'static>,
    prefix: &str,
    prefix_style: Style,
) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix.to_owned(), prefix_style)];
    spans.extend(line.spans);
    let mut out = Line::from(spans);
    out.style = line.style;
    out
}

/// Continuation prefix that keeps the shared left bar styled after wrapping.
pub(super) fn bar_continuation_prefix(prefix: &str, bar_style: Style) -> Vec<Span<'static>> {
    let bar_pos = prefix.find(BAR).expect("prefix must contain bar glyph");
    let left = &prefix[..bar_pos];
    let right = &prefix[bar_pos + BAR.len()..];

    let mut spans = Vec::with_capacity(3);
    if !left.is_empty() {
        spans.push(Span::raw(left.to_owned()));
    }
    spans.push(Span::styled(BAR, bar_style));
    if !right.is_empty() {
        spans.push(Span::raw(right.to_owned()));
    }
    spans
}

pub(super) fn last_has_width(lines: &[Line<'_>]) -> bool {
    lines.last().is_some_and(|l| l.width() > 0)
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use super::*;
    use crate::tui::glyphs::USER_PROMPT_PREFIX;

    // ── push_icon_wrapped ──

    #[test]
    fn push_icon_wrapped_short_text_single_line() {
        let mut out = Vec::new();
        push_icon_wrapped(
            &mut out,
            USER_PROMPT_PREFIX,
            Style::default().fg(Color::Red),
            "hello",
            Style::default(),
            80,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].spans.len(), 2);
        assert_eq!(out[0].spans[0].content, USER_PROMPT_PREFIX);
        assert_eq!(out[0].spans[1].content, "hello");
    }

    #[test]
    fn push_icon_wrapped_wraps_long_text_with_indent() {
        let mut out = Vec::new();
        push_icon_wrapped(
            &mut out,
            USER_PROMPT_PREFIX,
            Style::default(),
            "one two three four five six seven",
            Style::default(),
            16,
        );
        assert!(out.len() >= 2, "should wrap: {out:?}");
        let cont = &out[1];
        assert_eq!(cont.spans[0].content.as_ref(), "  ");
    }

    #[test]
    fn push_icon_wrapped_uses_display_width_not_bytes() {
        let mut out = Vec::new();
        push_icon_wrapped(
            &mut out,
            USER_PROMPT_PREFIX,
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

    // ── bar_continuation_prefix ──

    #[test]
    fn bar_continuation_prefix_preserves_bar_position_and_style() {
        let style = Style::default().fg(Color::Blue);
        let result = bar_continuation_prefix("  ▎   ", style);

        let text: String = result.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "  ▎   ");
        let bar = result
            .iter()
            .find(|s| s.content == BAR)
            .expect("bar span present");
        assert_eq!(bar.style, style);
    }
}
