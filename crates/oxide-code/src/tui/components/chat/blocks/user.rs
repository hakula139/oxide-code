//! User message block.

use ratatui::text::Line;

use super::{ChatBlock, RenderCtx, push_icon_wrapped};

/// First-line prefix for user messages — chevron + space. Continuation
/// wraps to a 2-column space indent under the text.
const USER_PREFIX: &str = "❯ ";

/// A user-typed message.
pub(crate) struct UserMessage {
    text: String,
}

impl UserMessage {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl ChatBlock for UserMessage {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let icon_style = ctx.theme.user();
        let text_style = ctx.theme.text();
        let width = usize::from(ctx.width);

        let mut out = Vec::new();
        for (i, text_line) in self.text.trim().lines().enumerate() {
            let prefix = if i == 0 { USER_PREFIX } else { "  " };
            push_icon_wrapped(&mut out, prefix, icon_style, text_line, text_style, width);
        }
        out
    }
}
