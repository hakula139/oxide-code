//! Shared `[bar] [number] [separator] [text]` row renderer. Read / grep use the default pipe
//! separator; Edit-tool diff sides and `/diff` pass `- ` / `+ ` plus a row bg via
//! [`Renderer::with_style`]. Visibility widened to `pub(in super::super)` so `blocks::git_diff`
//! can reuse it without relocating.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::super::RenderCtx;
use super::{MAX_TOOL_OUTPUT_LINE_BYTES, TOOL_BORDER_CONT, truncate_to_bytes};
use crate::tui::glyphs::BAR;
use crate::tui::wrap::{expand_tabs, wrap_line};

const DEFAULT_SEPARATOR: &str = " │ ";

/// Renders numbered rows under a shared bordered tool-result body.
pub(in super::super) struct Renderer<'a> {
    ctx: &'a RenderCtx<'a>,
    border_style: Style,
    number_width: usize,
    separator: &'static str,
    separator_style: Style,
    row_bg: Option<Style>,
    cont_indent: usize,
    cont_spans: Vec<Span<'static>>,
}

impl<'a> Renderer<'a> {
    /// Plain numbered rows — pipe separator, no row bg. Used by read / grep.
    pub(in super::super) fn new(
        ctx: &'a RenderCtx<'a>,
        border_style: Style,
        number_width: usize,
    ) -> Self {
        Self::with_style(
            ctx,
            border_style,
            number_width,
            DEFAULT_SEPARATOR,
            ctx.theme.dim(),
            None,
        )
    }

    /// Diff-side variant — caller passes the `- ` / `+ ` sign as separator and a row bg so the
    /// tint extends across the full terminal width.
    pub(in super::super) fn with_style(
        ctx: &'a RenderCtx<'a>,
        border_style: Style,
        number_width: usize,
        separator: &'static str,
        separator_style: Style,
        row_bg: Option<Style>,
    ) -> Self {
        let separator_width = separator.width();
        let cont_indent = TOOL_BORDER_CONT.width() + number_width + separator_width;
        let cont_spans = make_cont_spans(border_style, number_width, separator_width, row_bg);
        Self {
            ctx,
            border_style,
            number_width,
            separator,
            separator_style,
            row_bg,
            cont_indent,
            cont_spans,
        }
    }

    pub(in super::super) fn render(
        &self,
        out: &mut Vec<Line<'static>>,
        number: usize,
        text: &str,
        text_style: Style,
    ) {
        let expanded = expand_tabs(text);
        let display_text = truncate_to_bytes(&expanded, MAX_TOOL_OUTPUT_LINE_BYTES);
        let line_number = format!("{:>width$}", number, width = self.number_width);
        let bg = self.row_bg.unwrap_or_default();
        let rendered = Line::from(vec![
            Span::styled(TOOL_BORDER_CONT.to_owned(), self.border_style),
            Span::styled(line_number, self.ctx.theme.muted().patch(bg)),
            Span::styled(self.separator.to_owned(), self.separator_style.patch(bg)),
            Span::styled(display_text, text_style.patch(bg)),
        ]);
        let mut wrapped = wrap_line(
            rendered,
            usize::from(self.ctx.width),
            self.cont_indent,
            Some(&self.cont_spans),
        );
        if let Some(row_bg) = self.row_bg {
            for line in &mut wrapped {
                pad_to_width(line, usize::from(self.ctx.width), row_bg);
            }
        }
        out.extend(wrapped);
    }
}

/// Builds the wrapped-line continuation prefix (bar area stays transparent, text area gets bg).
fn make_cont_spans(
    border_style: Style,
    number_width: usize,
    separator_width: usize,
    row_bg: Option<Style>,
) -> Vec<Span<'static>> {
    let bar_prefix_padding = TOOL_BORDER_CONT.width().saturating_sub(BAR.width());
    let text_col_padding = number_width + separator_width;
    let bg = row_bg.unwrap_or_default();
    vec![
        Span::styled(BAR, border_style),
        Span::raw(" ".repeat(bar_prefix_padding)),
        Span::styled(" ".repeat(text_col_padding), bg),
    ]
}

fn pad_to_width(line: &mut Line<'static>, target_width: usize, bg: Style) {
    let current: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if current >= target_width {
        return;
    }
    line.spans
        .push(Span::styled(" ".repeat(target_width - current), bg));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

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
        assert_eq!(spans, vec![TOOL_BORDER_CONT, "  7", " │ ", "hello"]);
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
        // Continuation lines carry the bar plus padding aligned under the text column.
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
            cont_text.starts_with(TOOL_BORDER_CONT),
            "continuation must keep tool border continuation prefix: {cont_text:?}",
        );
    }

    // ── Renderer::with_style ──

    #[test]
    fn with_style_render_uses_custom_separator_in_place_of_pipe() {
        // Diff path: separator carries `- ` / `+ ` instead of the pipe; text stays a separate
        // span so the sign can take its own color.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = Renderer::with_style(
            &ctx,
            theme.tool_border(),
            2,
            " - ",
            theme.error(),
            Some(theme.diff_del_row()),
        );
        let mut out = Vec::new();
        renderer.render(&mut out, 14, r#"println!("x");"#, theme.error());

        let row = out.first().expect("renders one line");
        let contents: Vec<&str> = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(contents[0], TOOL_BORDER_CONT);
        assert_eq!(contents[1], "14");
        assert_eq!(contents[2], " - ");
        assert_eq!(contents[3], r#"println!("x");"#);
    }

    #[test]
    fn with_style_patches_row_bg_onto_content_spans() {
        // Bar prefix stays transparent; number / separator / text inherit the row bg. Regression
        // would either tint the chrome column or leave a hole in the band.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = Renderer::with_style(
            &ctx,
            theme.tool_border(),
            2,
            " + ",
            theme.success(),
            Some(theme.diff_add_row()),
        );
        let mut out = Vec::new();
        renderer.render(&mut out, 7, "x", theme.success());

        let row = &out[0];
        assert_eq!(row.spans[0].style.bg, None, "bar prefix must stay clear");
        assert_eq!(row.spans[1].style.bg, theme.diff_add.bg);
        assert_eq!(row.spans[2].style.bg, theme.diff_add.bg);
        assert_eq!(row.spans[3].style.bg, theme.diff_add.bg);
    }

    #[test]
    fn with_style_pads_to_full_width_with_row_bg() {
        // Trailing pad fills remaining columns so the bg tint reaches `ctx.width`.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 40,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = Renderer::with_style(
            &ctx,
            theme.tool_border(),
            2,
            " - ",
            theme.error(),
            Some(theme.diff_del_row()),
        );
        let mut out = Vec::new();
        renderer.render(&mut out, 1, "abc", theme.error());

        let row = &out[0];
        let total: usize = row.spans.iter().map(|s| s.content.width()).sum();
        assert_eq!(total, 40, "padded row must reach ctx.width");

        let last = row.spans.last().expect("trailing pad span");
        assert!(
            last.content.chars().all(|c| c == ' '),
            "trailing pad span must be spaces only, got {:?}",
            last.content,
        );
        assert_eq!(last.style.bg, theme.diff_del.bg);
    }

    #[test]
    fn with_style_no_bg_skips_padding() {
        // No row bg → no trailing pad. Read / grep rely on this so a transparent terminal
        // does not paint a phantom band.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = Renderer::with_style(&ctx, theme.tool_border(), 2, " │ ", theme.dim(), None);
        let mut out = Vec::new();
        renderer.render(&mut out, 1, "abc", theme.text());

        let row = &out[0];
        let total: usize = row.spans.iter().map(|s| s.content.width()).sum();
        assert!(
            total < 80,
            "no-bg row must not pad to width, got total={total}",
        );
    }

    #[test]
    fn with_style_wrapped_continuation_keeps_bg_under_text_column() {
        // Every wrapped line pads to `ctx.width` so the bg stays contiguous; the bar prefix
        // on continuations stays transparent like the header line.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 20,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = Renderer::with_style(
            &ctx,
            theme.tool_border(),
            2,
            " - ",
            theme.error(),
            Some(theme.diff_del_row()),
        );
        let mut out = Vec::new();
        renderer.render(&mut out, 1, "alpha beta gamma delta", theme.error());

        assert!(out.len() >= 2, "expected wrapped output: {out:#?}");
        for line in &out {
            let total: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert_eq!(total, 20, "every wrapped row must reach ctx.width");
            assert_eq!(
                line.spans.last().unwrap().style.bg,
                theme.diff_del.bg,
                "trailing span on every line must carry row bg",
            );
        }
        let cont = &out[1];
        assert_eq!(
            cont.spans[0].style.bg, None,
            "continuation bar prefix must stay clear"
        );
    }
}
