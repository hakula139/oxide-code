//! Cancellation marker: a dim italic `(interrupted)` line after a turn that
//! [`Op::Interrupt`](crate::protocol::Op::Interrupt) cut short.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::{ChatBlock, RenderCtx};
use crate::tui::glyphs::INTERRUPTED_MARKER;

/// Trails a cancelled turn's partial assistant block, marking where the cancel landed.
pub(crate) struct InterruptedMarker;

impl ChatBlock for InterruptedMarker {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let style = ctx.theme.dim().add_modifier(Modifier::ITALIC);
        vec![Line::from(Span::styled(INTERRUPTED_MARKER, style))]
    }
}
