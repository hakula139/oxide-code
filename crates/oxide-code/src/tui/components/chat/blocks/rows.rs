//! Bar-prefixed rows shared by the chat blocks. [`bar_row`] wraps text under its bar,
//! [`body_row`] indents it as a tool or `/diff` body row, and [`NumberedRows`] puts a line
//! number and a separator between the bar and the text.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{RenderCtx, bar_continuation_prefix};
use crate::tui::glyphs::{BAR, TOOL_BORDER_CONT};
use crate::tui::wrap::{expand_tabs, wrap_line};
use crate::util::text::ELLIPSIS;

/// Byte budget for the text of one row, past which [`truncate_to_bytes`] cuts it.
pub(super) const MAX_ROW_BYTES: usize = 512;

const DEFAULT_SEPARATOR: &str = " │ ";

// ── Bar Rows ──

/// Emits `text` behind `prefix`, which holds the bar, wrapping under the prefix at `ctx.width`.
pub(super) fn bar_row(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    prefix: &str,
    bar_style: Style,
    text: impl Into<String>,
    text_style: Style,
) {
    let cont_prefix = bar_continuation_prefix(prefix, bar_style);
    let line = Line::from(vec![
        Span::styled(prefix.to_owned(), bar_style),
        Span::styled(text.into(), text_style),
    ]);
    out.extend(wrap_line(
        line,
        usize::from(ctx.width),
        prefix.width(),
        Some(&cont_prefix),
    ));
}

/// Emits a tool or `/diff` body row, indented two columns past the status line text.
pub(super) fn body_row(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    text: impl Into<String>,
    text_style: Style,
) {
    bar_row(out, ctx, TOOL_BORDER_CONT, border_style, text, text_style);
}

/// Cuts `s` to at most `max_bytes` on a char boundary and marks the cut with [`ELLIPSIS`].
pub(super) fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let boundary = s.floor_char_boundary(max_bytes);
    format!("{}{ELLIPSIS}", &s[..boundary])
}

// ── NumberedRows ──

/// Renders numbered rows under a tool-result body bar. Read and grep rows use the default pipe
/// separator, while Edit-tool diff sides and `/diff` pass a `- ` or `+ ` sign and a row bg through
/// [`NumberedRows::with_style`].
pub(super) struct NumberedRows<'a> {
    ctx: &'a RenderCtx<'a>,
    border_style: Style,
    number_width: usize,
    separator: &'static str,
    separator_style: Style,
    row_bg: Option<Style>,
    cont_indent: usize,
    cont_spans: Vec<Span<'static>>,
}

impl<'a> NumberedRows<'a> {
    /// Plain numbered rows with the pipe separator and no row bg, as read and grep draw them.
    pub(super) fn new(ctx: &'a RenderCtx<'a>, border_style: Style, number_width: usize) -> Self {
        Self::with_style(
            ctx,
            border_style,
            number_width,
            DEFAULT_SEPARATOR,
            ctx.theme.dim(),
            None,
        )
    }

    /// Diff-side rows, with the `- ` or `+ ` sign as the separator and a row bg whose tint spans
    /// the full terminal width.
    pub(super) fn with_style(
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

    pub(super) fn render(
        &self,
        out: &mut Vec<Line<'static>>,
        number: usize,
        text: &str,
        text_style: Style,
    ) {
        let expanded = expand_tabs(text);
        let display_text = truncate_to_bytes(&expanded, MAX_ROW_BYTES);
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
    let current = line.width();
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

    // ── bar_row ──

    #[test]
    fn bar_row_emits_bar_prefix_and_styled_text() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        bar_row(
            &mut out,
            &ctx,
            TOOL_BORDER_CONT,
            theme.tool_border(),
            "hello",
            theme.dim(),
        );

        assert_eq!(out.len(), 1, "short row should not wrap: {out:#?}");
        let row = &out[0];
        assert_eq!(row.spans.len(), 2);
        assert_eq!(row.spans[0].content, TOOL_BORDER_CONT);
        assert_eq!(row.spans[0].style, theme.tool_border());
        assert_eq!(row.spans[1].content, "hello");
        assert_eq!(row.spans[1].style, theme.dim());
    }

    #[test]
    fn bar_row_wraps_long_text_under_the_bar_in_the_text_style() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 12,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();

        bar_row(
            &mut out,
            &ctx,
            TOOL_BORDER_CONT,
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

    // ── truncate_to_bytes ──

    #[test]
    fn truncate_to_bytes_under_limit_preserves_input() {
        assert_eq!(truncate_to_bytes("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_bytes_over_limit_appends_ellipsis() {
        assert_eq!(truncate_to_bytes("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_to_bytes_respects_char_boundary() {
        // Each `中` is 3 bytes, so cutting at byte 5 without `floor_char_boundary` would split a
        // codepoint and panic on `&s[..5]`. Rounding down to byte 3 yields one `中` + `...`.
        let input = "中中中中";
        let result = truncate_to_bytes(input, 5);
        assert_eq!(result, "中...");
        assert!(result.is_char_boundary(result.len() - 3));
    }

    #[test]
    fn truncate_to_bytes_exact_boundary_no_split() {
        assert_eq!(truncate_to_bytes("中中", 6), "中中");
    }

    // ── NumberedRows::render ──

    #[test]
    fn render_emits_bar_number_separator_and_text() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = NumberedRows::new(&ctx, theme.tool_border(), 3);
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
        let renderer = NumberedRows::new(&ctx, theme.tool_border(), 4);
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
        let renderer = NumberedRows::new(&ctx, theme.tool_border(), 1);
        let mut out = Vec::new();
        let long = "a".repeat(MAX_ROW_BYTES + 50);
        renderer.render(&mut out, 1, &long, theme.text());

        assert_eq!(
            out[0].spans[3].content,
            format!("{}...", "a".repeat(MAX_ROW_BYTES)),
        );
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
        let renderer = NumberedRows::new(&ctx, theme.tool_border(), 2);
        let mut out = Vec::new();
        renderer.render(&mut out, 1, "alpha beta gamma", theme.text());

        let rows: Vec<String> = out.iter().map(ToString::to_string).collect();
        assert_eq!(rows, ["▎    1 │ alpha", "▎        beta ", "▎        gamma"]);
    }

    // ── NumberedRows::with_style ──

    #[test]
    fn with_style_render_uses_custom_separator_in_place_of_pipe() {
        // On the diff path the separator carries the `- ` or `+ ` sign, and the text stays a
        // separate span so the sign can take its own color.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = NumberedRows::with_style(
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
        // The bar prefix stays transparent while the number, separator, and text inherit the row
        // bg, so the chrome column never tints and the band has no holes.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = NumberedRows::with_style(
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
        assert_eq!(row.spans[1].style.bg, theme.diff_add_row().bg);
        assert_eq!(row.spans[2].style.bg, theme.diff_add_row().bg);
        assert_eq!(row.spans[3].style.bg, theme.diff_add_row().bg);
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
        let renderer = NumberedRows::with_style(
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
        assert_eq!(last.style.bg, theme.diff_del_row().bg);
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
        let renderer =
            NumberedRows::with_style(&ctx, theme.tool_border(), 2, " │ ", theme.dim(), None);
        let mut out = Vec::new();
        renderer.render(&mut out, 1, "abc", theme.text());

        let spans: Vec<&str> = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(spans, [TOOL_BORDER_CONT, " 1", " │ ", "abc"]);
    }

    #[test]
    fn with_style_wrapped_continuation_keeps_bg_under_text_column() {
        // Every wrapped line pads to `ctx.width` so the bg stays contiguous, and the bar prefix on
        // continuations stays transparent like the header line.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 20,
            theme: &theme,
            show_thinking: true,
        };
        let renderer = NumberedRows::with_style(
            &ctx,
            theme.tool_border(),
            2,
            " - ",
            theme.error(),
            Some(theme.diff_del_row()),
        );
        let mut out = Vec::new();
        renderer.render(&mut out, 1, "alpha beta gamma delta", theme.error());

        let rows: Vec<String> = out
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect();
        assert_eq!(rows, ["▎    1 - alpha beta", "▎        gamma delta"]);
        for line in &out {
            let total: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert_eq!(total, 20, "every wrapped row must reach ctx.width");
            assert_eq!(
                line.spans.last().unwrap().style.bg,
                theme.diff_del_row().bg,
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
