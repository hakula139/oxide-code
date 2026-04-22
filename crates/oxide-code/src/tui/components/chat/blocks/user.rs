//! User message block.

use ratatui::text::{Line, Span};

use super::{BORDER_PREFIX, ChatBlock, RenderCtx, border_continuation_prefix};
use crate::tui::wrap::wrap_line;

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
        let mut is_first = true;
        for text_line in self.text.trim().lines() {
            let prefix = if is_first { USER_PREFIX } else { BORDER_PREFIX };
            is_first = false;
            let line = Line::from(vec![
                Span::styled(prefix.to_owned(), bar_style),
                Span::styled(text_line.to_owned(), text_style),
            ]);
            for wrapped in wrap_line(line, width, BORDER_PREFIX.len(), Some(&cont_prefix)) {
                out.push(wrapped);
            }
        }
        out
    }
}
