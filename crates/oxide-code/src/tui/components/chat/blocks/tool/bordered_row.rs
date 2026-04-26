//! Shared row primitive for tool result bodies that render an
//! unnumbered `[bar] [text]` row under the left-edge bar. Used by the
//! default text body, glob's path list and `pattern (X of Y)` header,
//! and grep's per-file path headers and combined footer.
//!
//! Numbered tool rows (read excerpts, grep matches, diff sides) use
//! the sibling [`super::numbered_row`] primitive instead; the column
//! shape is different enough that one renderer for both modes would
//! dilute each contract.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::super::RenderCtx;
use super::{STATUS_LINE_CONT, border_continuation_prefix};
use crate::tui::wrap::wrap_line;

/// Emits a single bar-prefixed row with `text` styled by `text_style`,
/// wrapping to `ctx.width` with a bar-aligned continuation prefix.
/// `wrap_line` is a no-op when the row fits, so single-line footers
/// pay no extra cost while long content still wraps under the bar.
pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    text: impl Into<String>,
    text_style: Style,
) {
    let cont_prefix = border_continuation_prefix(STATUS_LINE_CONT, border_style);
    let line = Line::from(vec![
        Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
        Span::styled(text.into(), text_style),
    ]);
    out.extend(wrap_line(
        line,
        usize::from(ctx.width),
        STATUS_LINE_CONT.width(),
        Some(&cont_prefix),
    ));
}

#[cfg(test)]
mod tests {
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
        assert_eq!(row.spans[0].content, STATUS_LINE_CONT);
        assert_eq!(row.spans[0].style, theme.tool_border());
        assert_eq!(row.spans[1].content, "hello");
        assert_eq!(row.spans[1].style, theme.dim());
    }

    #[test]
    fn render_wraps_long_text_under_bar() {
        // Width forces the row to wrap. Continuation lines should keep
        // the `▎` bar aligned via `border_continuation_prefix`, so the
        // bar style on a continuation row matches the leading row.
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
            cont_text.starts_with('▎'),
            "continuation must keep bar prefix: {cont_text:?}",
        );
    }

    #[test]
    fn render_carries_text_style_through_wrap() {
        // Pin that the text style is applied across wrapped fragments —
        // a regression that styled only the first row would dim the
        // first line and leave continuations un-styled.
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
