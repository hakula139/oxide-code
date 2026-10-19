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

use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use self::rows::prefixed_row;
use crate::tui::glyphs::INLINE_SEPARATOR;
use crate::tui::markdown::render_markdown;
use crate::tui::theme::Theme;

// ── Trait ──

/// Per-render snapshot threaded through every [`ChatBlock::render`] call.
///
/// `width` is the inner content width (already stripped of any chrome). `show_thinking` lets
/// thinking blocks elide their content without removing themselves from the block list.
#[derive(Clone, Copy)]
pub(super) struct RenderCtx<'a> {
    pub(super) width: u16,
    pub(super) theme: &'a Theme,
    pub(super) show_thinking: bool,
    /// Whether the block renders in full. A compact block caps its body and ends it with an
    /// overflow footer.
    pub(super) expanded: bool,
    /// The hint an overflow footer ends with, such as `Ctrl+O to expand`, or `None` when no key
    /// expands the chat.
    pub(super) expand_hint: Option<&'a str>,
}

impl RenderCtx<'_> {
    /// The most items a body shows: `compact` in the compact view, or all of them when expanded.
    pub(super) fn cap(&self, compact: usize) -> usize {
        if self.expanded { usize::MAX } else { compact }
    }

    /// `text` followed by the expand hint, as in `... +3 lines · Ctrl+O to expand`.
    pub(super) fn with_expand_hint(&self, text: &str) -> String {
        match self.expand_hint {
            Some(hint) => format!("{text}{INLINE_SEPARATOR}{hint}"),
            None => text.to_owned(),
        }
    }
}

/// A block's rendered rows, with the ones a click expands it from.
#[derive(Default)]
pub(super) struct BlockRows {
    pub(super) lines: Vec<Line<'static>>,
    /// Runs of `lines` that expand the block when clicked, such as its overflow footers.
    expanders: Vec<Range<usize>>,
}

impl BlockRows {
    /// Emits rows through `emit` as a run that expands the block when clicked.
    pub(super) fn push_expander(&mut self, emit: impl FnOnce(&mut Vec<Line<'static>>)) {
        let start = self.lines.len();
        emit(&mut self.lines);
        self.expanders.push(start..self.lines.len());
    }

    /// Returns whether a click on row `row` expands the block.
    pub(super) fn expands_from(&self, row: usize) -> bool {
        self.expanders.iter().any(|run| run.contains(&row))
    }

    /// Indices of the rows a click expands the block from.
    #[cfg(test)]
    pub(super) fn expander_rows(&self) -> Vec<usize> {
        (0..self.lines.len())
            .filter(|&row| self.expands_from(row))
            .collect()
    }
}

/// Render contract for one chat block (assistant prose, tool call, error marker, etc.).
///
/// `render`, `render_rows`, and `visible` must be pure functions of the block's state and the
/// [`RenderCtx`], since the chat view caches them until the width, theme, or view changes.
pub(super) trait ChatBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>>;

    /// Renders the block with the rows a click expands it from. The default marks none, for a
    /// block that always shows in full.
    fn render_rows(&self, ctx: &RenderCtx<'_>) -> BlockRows {
        BlockRows {
            lines: self.render(ctx),
            expanders: Vec::new(),
        }
    }

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
    prefixed_row(&mut out, ctx, prefix, styles.bar, header, styles.header);
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

pub(super) fn last_has_width(lines: &[Line<'_>]) -> bool {
    lines.last().is_some_and(|l| l.width() > 0)
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use super::*;

    mod block_rows {
        use super::*;

        mod push_expander {
            use super::*;

            #[test]
            fn marks_every_row_the_emit_adds() {
                let mut rows = BlockRows::default();
                rows.lines.push(Line::raw("head"));

                rows.push_expander(|lines| {
                    lines.extend([Line::raw("footer"), Line::raw("wrapped")]);
                });
                rows.lines.push(Line::raw("tail"));

                assert_eq!(rows.expander_rows(), [1, 2]);
            }
        }
    }

    mod fill_plain_spans {
        use super::*;

        #[test]
        fn patches_only_spans_without_a_color() {
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
        fn keeps_a_line_colored_as_a_whole() {
            let theme = Theme::default();
            let line = Line::styled("cargo test", theme.code());

            let out = fill_plain_spans(line.clone(), theme.text());

            assert_eq!(out, line);
        }
    }

    mod prepend_markdown_prefix {
        use super::*;

        #[test]
        fn adds_styled_prefix_and_keeps_line_style_and_alignment() {
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
    }
}
