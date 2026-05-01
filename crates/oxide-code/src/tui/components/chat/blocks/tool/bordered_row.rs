//! Shared `[bar] [text]` row primitive for unnumbered chat-block body
//! rows. Numbered rows use the sibling [`super::numbered_row`].
//! Visibility is widened to `pub(in super::super)` so non-tool block
//! modules (`blocks::git_diff` for file headers / hunk headers /
//! truncation footers) reuse it without having to physically move
//! the file.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::super::RenderCtx;
use super::{TOOL_BORDER_CONT, border_continuation_prefix};
use crate::tui::wrap::wrap_line;

/// Emits a bar-prefixed row, wrapping under the bar at `ctx.width`.
pub(in super::super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    text: impl Into<String>,
    text_style: Style,
) {
    let cont_prefix = border_continuation_prefix(TOOL_BORDER_CONT, border_style);
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
    use crate::tui::glyphs::BAR;
    use crate::tui::theme::Theme;

    use super::*;

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
    fn render_wraps_long_text_under_bar() {
        // Continuation lines must keep the bar aligned via
        // `border_continuation_prefix`.
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
            theme.text(),
        );

        assert!(out.len() >= 2, "expected wrapped output: {out:#?}");
        let cont_text: String = out[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            cont_text.starts_with(BAR),
            "continuation must keep bar prefix: {cont_text:?}",
        );
    }

    #[test]
    fn render_carries_text_style_through_wrap() {
        // Text style must apply across all wrapped fragments, not
        // just the first.
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

        for line in &out {
            let last = line.spans.last().expect("each row carries a text span");
            assert_eq!(
                last.style,
                theme.dim(),
                "every wrapped row keeps the text style: {line:?}",
            );
        }
    }
}
