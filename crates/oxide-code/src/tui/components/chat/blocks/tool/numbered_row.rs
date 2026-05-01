//! Shared row primitive for chat blocks that render a
//! `[bar] [number] [separator] [text]` row shape. Read / grep use the
//! default `" │ "` pipe separator with no row background; Edit-tool
//! diff sides and the slash `/diff` `GitDiffBlock` pass the `- ` / `+ `
//! sign as separator and a Catppuccin red / green row bg via
//! [`Renderer::with_style`]. Visibility is widened to `pub(in
//! super::super)` so non-tool block modules (`blocks::git_diff`) reuse
//! the renderer without having to physically move it.
//!
//! The renderer captures per-call state — border style, separator,
//! optional row bg, number column width, continuation prefix — once at
//! construction so each row only carries its own number, text, and text
//! style.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::super::RenderCtx;
use super::{MAX_TOOL_OUTPUT_LINE_BYTES, TOOL_BORDER_CONT, truncate_to_bytes};
use crate::tui::glyphs::BAR;
use crate::tui::wrap::{expand_tabs, wrap_line};

/// Default separator between the line-number column and the text. Read
/// / grep render this as a dim pipe; diff sides override via
/// [`Renderer::with_style`].
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
    /// Default constructor for plain numbered rows — pipe separator,
    /// no row bg. Used by read / grep.
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

    /// Customized constructor — diff sides pass their `- ` / `+ ` sign
    /// as separator and a [`Theme::diff_add_row`] / [`Theme::diff_del_row`]
    /// bg style so the row tint extends across the full terminal width.
    ///
    /// [`Theme::diff_add_row`]: crate::tui::theme::Theme::diff_add_row
    /// [`Theme::diff_del_row`]: crate::tui::theme::Theme::diff_del_row
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

/// Builds the wrapped-line continuation prefix. The bar prefix area
/// (`▎` + `TOOL_BORDER_CONT` padding) stays transparent so it doesn't
/// inherit the row tint; the text-column padding (number + separator
/// width) carries `row_bg` so the tint visually starts under the
/// number column on every wrapped row.
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

/// Extends `line` to `target_width` cols with a trailing styled
/// space-fill so the `row_bg` tint reads as a contiguous block instead
/// of a ragged content-width band. No-op when the line already fills
/// or exceeds the target.
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

    // ── Renderer::with_style ──

    #[test]
    fn with_style_render_uses_custom_separator_in_place_of_pipe() {
        // Diff-side path: the separator slot carries the `- ` / `+ `
        // sign instead of the dim pipe. The text is still emitted as a
        // separate styled span so the sign can take its own (red /
        // green) color independent of the row text.
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
        renderer.render(&mut out, 14, "println!(\"x\");", theme.error());

        let row = out.first().expect("renders one line");
        let contents: Vec<&str> = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(contents[0], TOOL_BORDER_CONT);
        assert_eq!(contents[1], "14");
        assert_eq!(contents[2], " - ");
        assert_eq!(contents[3], "println!(\"x\");");
    }

    #[test]
    fn with_style_patches_row_bg_onto_content_spans() {
        // The bar prefix span must stay transparent (bg=None) while the
        // number / separator / text spans inherit the row bg via patch.
        // A regression here would either tint the bar (visual noise
        // bleeding into the chrome column) or leave the number column
        // bare (breaking the contiguous block effect).
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
        // Trailing pad span fills the remaining columns so the bg tint
        // reaches ctx.width — without it, the row would be a ragged
        // band ending at the text width.
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
        // When row_bg is None, no trailing pad is emitted — the row
        // ends at its natural content width. Read / grep rely on this:
        // a transparent terminal must not paint a phantom band.
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
        // After wrap, every continuation line must also pad to ctx.width
        // so the bg block stays contiguous across wraps. The bar prefix
        // area on continuations stays transparent (mirrors the header
        // line).
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

    // ── Renderer::render ──

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
            cont_text.starts_with(TOOL_BORDER_CONT),
            "continuation must keep tool border continuation prefix: {cont_text:?}",
        );
    }
}
