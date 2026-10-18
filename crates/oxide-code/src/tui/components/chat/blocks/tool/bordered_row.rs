//! Shared `[bar] [text]` row primitive for unnumbered chat-block body rows, such as the `/diff`
//! headers and footers. Numbered rows use the sibling [`super::numbered_row`].

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::super::RenderCtx;
use super::TOOL_BORDER_CONT;
use crate::tui::components::chat::blocks::bar_continuation_prefix;
use crate::tui::wrap::wrap_line;

/// Emits a bar-prefixed row, wrapping under the bar at `ctx.width`.
pub(in super::super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    text: impl Into<String>,
    text_style: Style,
) {
    let cont_prefix = bar_continuation_prefix(TOOL_BORDER_CONT, border_style);
    let line = Line::from(vec![
        Span::styled(TOOL_BORDER_CONT.to_owned(), border_style),
        Span::styled(text.into(), text_style),
    ]);
    out.extend(wrap_line(
        line,
        usize::from(ctx.width),
        TOOL_BORDER_CONT.width(),
        Some(&cont_prefix),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::glyphs::BAR;
    use crate::tui::theme::Theme;

    // ── render ──

    #[test]
    fn render_emits_bar_prefix_and_styled_text() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        render(&mut out, &ctx, theme.tool_border(), "hello", theme.dim());

        assert_eq!(out.len(), 1, "short row should not wrap: {out:#?}");
        let row = &out[0];
        assert_eq!(row.spans.len(), 2);
        assert_eq!(row.spans[0].content, TOOL_BORDER_CONT);
        assert_eq!(row.spans[0].style, theme.tool_border());
        assert_eq!(row.spans[1].content, "hello");
        assert_eq!(row.spans[1].style, theme.dim());
    }

    #[test]
    fn render_wraps_long_text_under_the_bar_in_the_text_style() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 12,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();

        render(
            &mut out,
            &ctx,
            theme.tool_border(),
            "alpha beta gamma",
            theme.dim(),
        );

        let (bar, dim) = (theme.tool_border(), theme.dim());
        let cont = |text| {
            Line::from(vec![
                Span::styled(BAR, bar),
                Span::raw("   "),
                Span::styled(text, dim),
            ])
        };
        assert_eq!(
            out,
            [
                Line::from(vec![
                    Span::styled(TOOL_BORDER_CONT, bar),
                    Span::styled("alpha ", dim),
                ]),
                cont("beta "),
                cont("gamma"),
            ]
        );
    }
}
