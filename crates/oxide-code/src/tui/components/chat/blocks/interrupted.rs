//! Cancellation marker — dim italic `(interrupted)` line shown after a
//! turn is dropped via [`UserAction::Cancel`](crate::agent::event::UserAction).

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::{ChatBlock, RenderCtx};
use crate::agent::event::INTERRUPTED_MARKER;

/// Trails a cancelled turn's partial assistant block so the transcript shows where the cancel
/// landed.
pub(crate) struct InterruptedMarker;

impl ChatBlock for InterruptedMarker {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let style = ctx.theme.dim().add_modifier(Modifier::ITALIC);
        vec![Line::from(Span::styled(INTERRUPTED_MARKER, style))]
    }
}
