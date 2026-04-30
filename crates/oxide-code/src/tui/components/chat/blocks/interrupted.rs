//! Cancellation marker — dim italic `(interrupted)` line shown after a
//! turn is dropped via [`UserAction::Cancel`](crate::agent::event::UserAction).

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::{ChatBlock, RenderCtx};
use crate::agent::event::INTERRUPTED_MARKER;

/// Trailing marker placed after the partial assistant block when a
/// turn is cancelled mid-flight, so the rendered transcript shows
/// where the cancel landed without a separate error treatment.
pub(crate) struct InterruptedMarker;

impl ChatBlock for InterruptedMarker {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let style = ctx.theme.dim().add_modifier(Modifier::ITALIC);
        vec![Line::from(Span::styled(INTERRUPTED_MARKER, style))]
    }
}
