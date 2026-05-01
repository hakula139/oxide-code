//! Error block — flat `✗ message` in the error color, no left bar.

use ratatui::text::Line;

use super::{ChatBlock, RenderCtx, push_icon_wrapped};
use crate::tui::glyphs::ERROR_PREFIX;

/// A fatal agent or API error, rendered as a single red line.
pub(crate) struct ErrorBlock {
    message: String,
}

impl ErrorBlock {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl ChatBlock for ErrorBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let style = ctx.theme.error();
        let mut out = Vec::new();
        push_icon_wrapped(
            &mut out,
            ERROR_PREFIX,
            style,
            &self.message,
            style,
            usize::from(ctx.width),
        );
        out
    }

    fn is_error_marker(&self) -> bool {
        true
    }
}
