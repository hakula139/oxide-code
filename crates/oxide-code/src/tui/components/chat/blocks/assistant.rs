//! Assistant text and thinking blocks.

use ratatui::text::{Line, Span};

use super::{
    BORDER_PREFIX, ChatBlock, RenderCtx, border_continuation_prefix, border_markdown_line,
    push_bordered_wrapped,
};
use crate::tui::markdown::render_markdown;

/// First-line prefix for assistant messages — lavender bar + diamond icon.
pub(super) const ASSISTANT_PREFIX: &str = "⟡ ▎ ";

// ── Assistant Text ──

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

/// Render assistant prose as a bordered markdown block.
///
/// `starts_new_turn = true` emits the assistant icon on the first line
/// ([`ASSISTANT_PREFIX`]); `false` uses [`BORDER_PREFIX`] so the block
/// continues an in-progress turn (used by the streaming cache after its
/// first cached line has already been emitted).
///
/// The markdown renderer wraps to `width - BORDER_PREFIX.len()` so the
/// left border doesn't push content past the terminal edge.
pub(super) fn render_assistant_markdown(
    text: &str,
    ctx: &RenderCtx<'_>,
    starts_new_turn: bool,
) -> Vec<Line<'static>> {
    let bar_style = ctx.theme.secondary();
    let md_width = usize::from(ctx.width).saturating_sub(BORDER_PREFIX.len());
    let rendered = render_markdown(text, ctx.theme, md_width);
    rendered
        .lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let prefix = if i == 0 && starts_new_turn {
                ASSISTANT_PREFIX
            } else {
                BORDER_PREFIX
            };
            border_markdown_line(line, prefix, bar_style)
        })
        .collect()
}

// ── Assistant Thinking ──

/// Extended-thinking block, shown dimmed and italic under a "Thinking..."
/// section header. Collapses to zero lines when `show_thinking` is off.
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
        let bar_style = ctx.theme.dim();
        let text_style = ctx.theme.thinking();
        let cont_prefix = border_continuation_prefix(BORDER_PREFIX, bar_style);
        let width = usize::from(ctx.width);

        let mut out = vec![Line::from(vec![
            Span::raw("  "),
            Span::styled("Thinking...", header_style),
        ])];
        for text_line in self.text.lines() {
            push_bordered_wrapped(
                &mut out,
                BORDER_PREFIX,
                bar_style,
                text_line,
                text_style,
                width,
                &cont_prefix,
            );
        }
        out
    }

    fn visible(&self, ctx: &RenderCtx<'_>) -> bool {
        ctx.show_thinking
    }
}
