//! Assistant text and thinking blocks.

use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use super::{ChatBlock, RenderCtx, prepend_markdown_prefix, push_icon_wrapped};
use crate::tui::markdown::render_markdown;

/// First-line prefix for assistant text — diamond + space. Continuation
/// (and all lines when the streaming block is continuing a turn) uses a
/// 2-column space indent.
pub(super) const ASSISTANT_PREFIX: &str = "◉ ";

/// Continuation prefix for assistant markdown — two spaces matching the
/// visual width of [`ASSISTANT_PREFIX`].
pub(super) const ASSISTANT_CONT: &str = "  ";

/// First-line prefix for the thinking header — diamond + space.
const THINKING_PREFIX: &str = "◇ ";

// ── AssistantText ──

/// A committed assistant text response, rendered through the markdown
/// pipeline.
pub(crate) struct AssistantText {
    text: String,
}

impl AssistantText {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl ChatBlock for AssistantText {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        render_assistant_markdown(&self.text, ctx, true)
    }

    fn continues_assistant_turn(&self) -> bool {
        true
    }
}

/// Render assistant prose as markdown with a first-line icon and a
/// matching-width space indent on every subsequent line.
///
/// `starts_new_turn = true` emits [`ASSISTANT_PREFIX`] on the first line
/// (a fresh turn). `false` uses [`ASSISTANT_CONT`] on every line so the
/// block flows into an existing assistant turn (used by the streaming
/// cache after its first line has already been emitted).
///
/// The markdown renderer wraps to `width - 2` so the 2-column lead-in
/// never pushes content past the terminal edge.
pub(super) fn render_assistant_markdown(
    text: &str,
    ctx: &RenderCtx<'_>,
    starts_new_turn: bool,
) -> Vec<Line<'static>> {
    let icon_style = ctx.theme.secondary();
    let md_width = usize::from(ctx.width).saturating_sub(ASSISTANT_PREFIX.width());
    let rendered = render_markdown(text, ctx.theme, md_width);
    rendered
        .lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let prefix = if i == 0 && starts_new_turn {
                ASSISTANT_PREFIX
            } else {
                ASSISTANT_CONT
            };
            prepend_markdown_prefix(line, prefix, icon_style)
        })
        .collect()
}

// ── AssistantThinking ──

/// Extended-thinking block, shown dimmed-italic under a `◇ Thinking`
/// header. Collapses to zero lines when `show_thinking` is off.
pub(crate) struct AssistantThinking {
    text: String,
}

impl AssistantThinking {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl ChatBlock for AssistantThinking {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let header_style = ctx.theme.thinking();
        let text_style = ctx.theme.thinking();
        let width = usize::from(ctx.width);

        let mut out = Vec::new();
        push_icon_wrapped(
            &mut out,
            THINKING_PREFIX,
            header_style,
            "Thinking...",
            header_style,
            width,
        );
        for text_line in self.text.lines() {
            push_icon_wrapped(&mut out, "  ", header_style, text_line, text_style, width);
        }
        out
    }

    fn visible(&self, ctx: &RenderCtx<'_>) -> bool {
        ctx.show_thinking
    }
}
