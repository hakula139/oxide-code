//! Error block — reuses the tool-result status-line visual with the
//! error indicator.

use ratatui::text::Line;

use super::tool::render_status_line;
use super::{ChatBlock, RenderCtx};

/// A fatal agent or API error, rendered as a single ✗-prefixed line.
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
        let mut out = Vec::new();
        render_status_line(&mut out, ctx, &self.message, true);
        out
    }

    fn standalone(&self) -> bool {
        false
    }
}
