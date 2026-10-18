//! Chat render blocks. Each conversation unit implements [`ChatBlock`].

mod assistant;
mod compacted;
mod error;
mod git_diff;
mod interrupted;
mod notice;
mod rows;
mod streaming;
mod system;
mod tool;
mod user;

pub(super) use assistant::{AssistantText, AssistantThinking, ProgressUpdate};
pub(super) use compacted::CompactedBlock;
pub(super) use error::ErrorBlock;
pub(super) use git_diff::GitDiffBlock;
pub(super) use interrupted::InterruptedMarker;
pub(super) use notice::NoticeBlock;
pub(super) use streaming::StreamingAssistant;
pub(super) use system::SystemMessageBlock;
pub(super) use tool::{ToolCallBlock, ToolResultBlock};
pub(super) use user::UserMessage;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use self::rows::bar_row;
use crate::tui::glyphs::BAR;
use crate::tui::markdown::render_markdown;
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

/// Render contract for one chat block (assistant prose, tool call, error marker, etc.).
///
/// `render` and `visible` must be pure functions of the block's state and the [`RenderCtx`], since
/// the chat view caches both until the width or theme changes.
pub(super) trait ChatBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>>;

    /// Returns whether blank rows separate this block from its neighbors. A tool call and its
    /// result return false, so they stay flush.
    fn standalone(&self) -> bool {
        true
    }

    /// Returns whether this block closes a tool group, so the next block sits a blank row below.
    fn ends_tool_group(&self) -> bool {
        false
    }

    fn visible(&self, _ctx: &RenderCtx<'_>) -> bool {
        true
    }

    /// Returns whether this block is committed assistant prose, so a fresh streaming buffer can
    /// continue its turn without inserting a separator.
    fn continues_assistant_turn(&self) -> bool {
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
fn push_icon_wrapped(
    out: &mut Vec<Line<'static>>,
    prefix: &str,
    prefix_style: Style,
    text: &str,
    text_style: Style,
    width: usize,
) {
    let line = Line::from(vec![
        Span::styled(prefix.to_owned(), prefix_style),
        Span::styled(text.to_owned(), text_style),
    ]);
    out.extend(wrap_line(line, width, prefix.width(), None));
}

/// Styles of a [`bar_section`]. Body spans without a color take `body`.
struct SectionStyles {
    bar: Style,
    header: Style,
    body: Style,
}

/// Renders `header` and the markdown `body` behind a left bar, with a bar-only row between them.
/// A blank body leaves the header alone.
fn bar_section(
    ctx: &RenderCtx<'_>,
    prefix: &str,
    styles: &SectionStyles,
    header: &str,
    body: &str,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    bar_row(&mut out, ctx, prefix, styles.bar, header, styles.header);
    if body.trim().is_empty() {
        return out;
    }

    out.push(Line::from(Span::styled(prefix.to_owned(), styles.bar)));
    let width = usize::from(ctx.width).saturating_sub(prefix.width());
    for line in render_markdown(body, ctx.theme, width).lines {
        let line = fill_plain_spans(line, styles.body);
        out.push(prepend_markdown_prefix(line, prefix, styles.bar));
    }
    out
}

/// Patches `base` onto spans without a color. A line colored as a whole, such as a plain code
/// fence, keeps its color, since any span color would paint over it.
fn fill_plain_spans(mut line: Line<'static>, base: Style) -> Line<'static> {
    if line.style.fg.is_some() {
        return line;
    }
    for span in &mut line.spans {
        if span.style.fg.is_none() {
            span.style = span.style.patch(base);
        }
    }
    line
}

/// Prepends a styled prefix span, keeping the line's style and alignment.
fn prepend_markdown_prefix(
    mut line: Line<'static>,
    prefix: &str,
    prefix_style: Style,
) -> Line<'static> {
    line.spans
        .insert(0, Span::styled(prefix.to_owned(), prefix_style));
    line
}

/// Continuation prefix that keeps the shared left bar styled after wrapping.
fn bar_continuation_prefix(prefix: &str, bar_style: Style) -> Vec<Span<'static>> {
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
        let icon_style = Style::default().fg(Color::Red);
        let mut out = Vec::new();

        push_icon_wrapped(
            &mut out,
            USER_PROMPT_PREFIX,
            icon_style,
            "hello",
            Style::default(),
            80,
        );

        assert_eq!(
            out,
            [Line::from(vec![
                Span::styled(USER_PROMPT_PREFIX, icon_style),
                Span::raw("hello"),
            ])]
        );
    }

    #[test]
    fn push_icon_wrapped_wraps_long_text_with_indent() {
        let text_style = Style::default().fg(Color::Green);
        let mut out = Vec::new();

        push_icon_wrapped(
            &mut out,
            USER_PROMPT_PREFIX,
            Style::default(),
            "one two three four five six seven",
            text_style,
            16,
        );

        let row = |lead, text| Line::from(vec![Span::raw(lead), Span::styled(text, text_style)]);
        assert_eq!(
            out,
            [
                row(USER_PROMPT_PREFIX, "one two three "),
                row("  ", "four five six "),
                row("  ", "seven"),
            ]
        );
    }

    #[test]
    fn push_icon_wrapped_uses_display_width_not_bytes() {
        let mut out = Vec::new();
        push_icon_wrapped(
            &mut out,
            USER_PROMPT_PREFIX,
            Style::default(),
            "你好 世界 再见",
            Style::default(),
            12,
        );
        let rows: Vec<String> = out.iter().map(ToString::to_string).collect();
        assert_eq!(rows, ["❯ 你好 世界 ", "  再见"]);
    }

    // ── fill_plain_spans ──

    #[test]
    fn fill_plain_spans_patches_only_spans_without_a_color() {
        let theme = Theme::default();
        let line = Line::from(vec![
            Span::raw("plain "),
            Span::styled("code", theme.code()),
        ]);

        let out = fill_plain_spans(line, theme.thinking());

        assert_eq!(
            out,
            Line::from(vec![
                Span::styled("plain ", theme.thinking()),
                Span::styled("code", theme.code()),
            ])
        );
    }

    #[test]
    fn fill_plain_spans_keeps_a_line_colored_as_a_whole() {
        let theme = Theme::default();
        let line = Line::styled("cargo test", theme.code());

        let out = fill_plain_spans(line.clone(), theme.text());

        assert_eq!(out, line);
    }

    // ── prepend_markdown_prefix ──

    #[test]
    fn prepend_markdown_prefix_adds_styled_prefix_and_keeps_line_style_and_alignment() {
        let line_style = Style::default().fg(Color::Green);
        let prefix_style = Style::default().fg(Color::Blue);
        let line = Line::styled("content", line_style).centered();

        let result = prepend_markdown_prefix(line, "◉ ", prefix_style);

        assert_eq!(
            result,
            Line::from(vec![Span::styled("◉ ", prefix_style), Span::raw("content")])
                .style(line_style)
                .centered()
        );
    }

    // ── bar_continuation_prefix ──

    #[test]
    fn bar_continuation_prefix_preserves_bar_position_and_style() {
        let style = Style::default().fg(Color::Blue);

        let result = bar_continuation_prefix("  ▎   ", style);

        assert_eq!(
            result,
            [Span::raw("  "), Span::styled(BAR, style), Span::raw("   ")]
        );
    }
}
