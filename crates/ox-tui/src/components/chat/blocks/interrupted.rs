//! Cancellation marker: a dim italic `(interrupted)` line after a turn that
//! [`Op::Interrupt`](ox_core::protocol::Op::Interrupt) cut short.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::{ChatBlock, RenderCtx};
use crate::glyphs::INTERRUPTED_MARKER;

/// Trails a cancelled turn's partial assistant block, marking where the cancel landed.
pub(crate) struct InterruptedMarker;

impl ChatBlock for InterruptedMarker {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let style = ctx.theme.dim().add_modifier(Modifier::ITALIC);
        vec![Line::from(Span::styled(INTERRUPTED_MARKER, style))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    mod render {
        use super::*;

        #[test]
        fn emits_one_dim_italic_marker_row() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(80, &theme);

            let lines = InterruptedMarker.render(&ctx);

            assert_eq!(
                lines,
                [Line::from(Span::styled(
                    "(interrupted)",
                    theme.dim().add_modifier(Modifier::ITALIC),
                ))]
            );
        }
    }
}
