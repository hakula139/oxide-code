//! User message block.

use ratatui::text::Line;

use super::{
    BORDER_PREFIX, ChatBlock, RenderCtx, border_continuation_prefix, push_bordered_wrapped,
};

/// First-line prefix for user messages — peach bar + chevron icon.
const USER_PREFIX: &str = "❯ ▎ ";

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
        let bar_style = ctx.theme.user();
        let text_style = ctx.theme.text();
        let cont_prefix = border_continuation_prefix(BORDER_PREFIX, bar_style);
        let width = usize::from(ctx.width);

        let mut out = Vec::new();
        for (i, text_line) in self.text.trim().lines().enumerate() {
            let prefix = if i == 0 { USER_PREFIX } else { BORDER_PREFIX };
            push_bordered_wrapped(
                &mut out,
                prefix,
                bar_style,
                text_line,
                text_style,
                width,
                &cont_prefix,
            );
        }
        out
    }
}
