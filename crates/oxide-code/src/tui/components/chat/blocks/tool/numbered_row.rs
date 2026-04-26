//! Shared row primitive for tool result bodies that render a
//! `[bar] [number] │ [text]` row shape. Today: `read_excerpt` and
//! `grep`. The diff redesign (`tui-visual-polish.md` item 3) joins
//! once `Edit::result_view` plumbs file line numbers through.
//!
//! The renderer captures per-call state — border style, number column
//! width, continuation prefix — once at construction so each row only
//! carries its own number, text, and text style.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::super::RenderCtx;
use super::{
    MAX_TOOL_OUTPUT_LINE_BYTES, STATUS_LINE_CONT, border_continuation_prefix, truncate_to_bytes,
};
use crate::tui::wrap::{expand_tabs, wrap_line};

/// Renders numbered rows under a shared bordered tool-result body.
pub(super) struct Renderer<'a> {
    ctx: &'a RenderCtx<'a>,
    border_style: Style,
    number_width: usize,
    cont_prefix: String,
    cont_spans: Vec<Span<'static>>,
}

impl<'a> Renderer<'a> {
    pub(super) fn new(ctx: &'a RenderCtx<'a>, border_style: Style, number_width: usize) -> Self {
        let cont_prefix = format!("{STATUS_LINE_CONT}{}   ", " ".repeat(number_width));
        let cont_spans = border_continuation_prefix(&cont_prefix, border_style);
        Self {
            ctx,
            border_style,
            number_width,
            cont_prefix,
            cont_spans,
        }
    }

    pub(super) fn render(
        &self,
        out: &mut Vec<Line<'static>>,
        number: usize,
        text: &str,
        text_style: Style,
    ) {
        let expanded = expand_tabs(text);
        let display_text = truncate_to_bytes(&expanded, MAX_TOOL_OUTPUT_LINE_BYTES);
        let line_number = format!("{:>width$}", number, width = self.number_width);
        let rendered = Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), self.border_style),
            Span::styled(line_number, self.ctx.theme.muted()),
            Span::styled(" │ ", self.ctx.theme.dim()),
            Span::styled(display_text, text_style),
        ]);
        out.extend(wrap_line(
            rendered,
            usize::from(self.ctx.width),
            self.cont_prefix.width(),
            Some(&self.cont_spans),
        ));
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::theme::Theme;

    use super::*;

    // ── Renderer::render ──

    #[test]
    fn render_emits_bar_number_separator_and_text() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = Renderer::new(&ctx, theme.tool_border(), 3);
        let mut out = Vec::new();
        renderer.render(&mut out, 7, "hello", theme.text());

        let row = out.first().expect("renders one line");
        let spans: Vec<&str> = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(spans, vec![STATUS_LINE_CONT, "  7", " │ ", "hello"]);
    }

    #[test]
    fn render_pads_number_to_column_width() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = Renderer::new(&ctx, theme.tool_border(), 4);
        let mut out = Vec::new();
        renderer.render(&mut out, 12, "x", theme.text());

        let number_span = &out[0].spans[1];
        assert_eq!(number_span.content, "  12");
    }

    #[test]
    fn render_truncates_overlong_text_to_byte_budget() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 4096,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = Renderer::new(&ctx, theme.tool_border(), 1);
        let mut out = Vec::new();
        let long = "a".repeat(MAX_TOOL_OUTPUT_LINE_BYTES + 50);
        renderer.render(&mut out, 1, &long, theme.text());

        let text_span = &out[0].spans[3];
        assert!(
            text_span.content.ends_with("..."),
            "expected ellipsis, got {:?}",
            text_span.content,
        );
        assert!(text_span.content.len() <= MAX_TOOL_OUTPUT_LINE_BYTES + 3);
    }

    #[test]
    fn render_wraps_with_aligned_continuation_prefix() {
        // Width forces the row to wrap. Continuation lines should
        // carry the `▎` bar plus padding aligned under the text column
        // (4 cols of bar prefix + 2 cols of number column + 3 cols of
        // ` │ ` = 9 cols).
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 14,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = Renderer::new(&ctx, theme.tool_border(), 2);
        let mut out = Vec::new();
        renderer.render(&mut out, 1, "alpha beta gamma", theme.text());

        assert!(out.len() >= 2, "expected wrapped output, got {out:#?}");
        let cont_text: String = out[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            cont_text.starts_with("▎     "),
            "continuation should align under text column: {cont_text:?}",
        );
    }
}
