//! Prefixed rows shared by the chat blocks. [`prefixed_row`] wraps text under its prefix, and
//! [`body_row`] indents it as a tool or `/diff` body row. [`wrapped_row`] wraps a prebuilt line
//! under a continuation prefix, and [`NumberedRows`] puts a line number and a separator between the
//! bar and the text.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use ox_core::util::text::ELLIPSIS;

use super::RenderCtx;
use crate::glyphs::{BAR, TOOL_BORDER_CONT};
use crate::wrap::{expand_tabs, wrap_line};

const DEFAULT_SEPARATOR: &str = " │ ";

// ── Prefixed Rows ──

/// Emits `text` behind `prefix`, wrapping under the prefix at `ctx.width`.
pub(super) fn prefixed_row(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    prefix: &str,
    prefix_style: Style,
    text: impl Into<String>,
    text_style: Style,
) {
    let line = Line::from(vec![
        Span::styled(prefix.to_owned(), prefix_style),
        Span::styled(text.into(), text_style),
    ]);
    wrapped_row(out, ctx, line, prefix, prefix_style);
}

/// Emits a tool or `/diff` body row, with its text under the status line's label.
pub(super) fn body_row(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    text: impl Into<String>,
    text_style: Style,
) {
    prefixed_row(out, ctx, TOOL_BORDER_CONT, border_style, text, text_style);
}

/// Emits `line`, wrapping it at `ctx.width`. Each continuation row starts under `cont_prefix`,
/// repeating its bar in `bar_style` and blanking its other cells.
pub(super) fn wrapped_row(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    line: Line<'static>,
    cont_prefix: &str,
    bar_style: Style,
) {
    let cont_spans = continuation_spans(cont_prefix, bar_style);
    out.extend(wrap_line(
        line,
        usize::from(ctx.width),
        cont_prefix.width(),
        Some(&cont_spans),
    ));
}

fn continuation_spans(prefix: &str, bar_style: Style) -> Vec<Span<'static>> {
    let blank = |cells: &str| Span::raw(" ".repeat(cells.width()));
    let Some((left, right)) = prefix.split_once(BAR) else {
        return vec![blank(prefix)];
    };
    [blank(left), Span::styled(BAR, bar_style), blank(right)]
        .into_iter()
        .filter(|span| span.width() > 0)
        .collect()
}

// ── NumberedRows ──

/// Renders numbered rows under a tool-result body bar, with a gutter wide enough for the largest
/// line number. Read and grep rows use the default pipe separator, while Edit-tool diff sides and
/// `/diff` sign each row and tint its bg through [`NumberedRows::removed`] and
/// [`NumberedRows::added`].
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
    /// Plain numbered rows with no row bg, as read and grep draw them. The pipe separator is a
    /// column rule in the tool border's color, whatever outcome `border_style` shows.
    pub(super) fn new(ctx: &'a RenderCtx<'a>, border_style: Style, max_number: usize) -> Self {
        Self::with_style(
            ctx,
            border_style,
            max_number,
            DEFAULT_SEPARATOR,
            ctx.theme.tool_border(),
            None,
        )
    }

    /// Removed diff lines, signed `-` over the removed-row bg.
    pub(super) fn removed(ctx: &'a RenderCtx<'a>, border_style: Style, max_number: usize) -> Self {
        Self::with_style(
            ctx,
            border_style,
            max_number,
            " - ",
            ctx.theme.error(),
            Some(ctx.theme.diff_del()),
        )
    }

    /// Added diff lines, signed `+` over the added-row bg.
    pub(super) fn added(ctx: &'a RenderCtx<'a>, border_style: Style, max_number: usize) -> Self {
        Self::with_style(
            ctx,
            border_style,
            max_number,
            " + ",
            ctx.theme.success(),
            Some(ctx.theme.diff_add()),
        )
    }

    /// Rows with `separator` between the number and the text, and a `row_bg` whose tint spans the
    /// full terminal width.
    fn with_style(
        ctx: &'a RenderCtx<'a>,
        border_style: Style,
        max_number: usize,
        separator: &'static str,
        separator_style: Style,
        row_bg: Option<Style>,
    ) -> Self {
        let number_width = max_number.to_string().len();
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
        let display_text = self.ctx.cap_row(&expand_tabs(text));
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

/// The continuation prefix of a wrapped numbered row: the body bar, then a pad under the number and
/// separator that carries the row bg.
fn make_cont_spans(
    border_style: Style,
    number_width: usize,
    separator_width: usize,
    row_bg: Option<Style>,
) -> Vec<Span<'static>> {
    let mut spans = continuation_spans(TOOL_BORDER_CONT, border_style);
    spans.push(Span::styled(
        " ".repeat(number_width + separator_width),
        row_bg.unwrap_or_default(),
    ));
    spans
}

fn pad_to_width(line: &mut Line<'static>, target_width: usize, bg: Style) {
    let current = line.width();
    if current >= target_width {
        return;
    }
    line.spans
        .push(Span::styled(" ".repeat(target_width - current), bg));
}

// ── Truncation ──

/// Byte budget for one row's text in the compact view, past which [`truncate_to_bytes`] cuts it.
pub(super) const MAX_ROW_BYTES: usize = 512;

/// Cuts `s` to at most `max_bytes` between grapheme clusters and marks the cut with [`ELLIPSIS`].
pub(super) fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let boundary = s
        .grapheme_indices(true)
        .map(|(byte, grapheme)| byte + grapheme.len())
        .take_while(|&end| end <= max_bytes)
        .last()
        .unwrap_or(0);
    format!("{}{ELLIPSIS}", &s[..boundary])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::USER_PROMPT_PREFIX;
    use crate::theme::Theme;

    mod prefixed_row {
        use super::*;

        #[test]
        fn emits_the_prefix_and_text_on_one_row() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(80, &theme);
            let mut out = Vec::new();

            prefixed_row(
                &mut out,
                &ctx,
                TOOL_BORDER_CONT,
                theme.tool_border(),
                "hello",
                theme.dim(),
            );

            assert_eq!(
                out,
                [Line::from(vec![
                    Span::styled(TOOL_BORDER_CONT, theme.tool_border()),
                    Span::styled("hello", theme.dim()),
                ])]
            );
        }

        #[test]
        fn wraps_long_text_under_the_bar_in_the_text_style() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(12, &theme);
            let mut out = Vec::new();

            prefixed_row(
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

        #[test]
        fn wraps_under_the_indent_of_an_icon_prefix() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(16, &theme);
            let mut out = Vec::new();

            prefixed_row(
                &mut out,
                &ctx,
                USER_PROMPT_PREFIX,
                theme.user(),
                "one two three four five six seven",
                theme.text(),
            );

            let text = theme.text();
            let cont = |words| Line::from(vec![Span::raw("  "), Span::styled(words, text)]);
            assert_eq!(
                out,
                [
                    Line::from(vec![
                        Span::styled(USER_PROMPT_PREFIX, theme.user()),
                        Span::styled("one two three ", text),
                    ]),
                    cont("four five six "),
                    cont("seven"),
                ]
            );
        }

        #[test]
        fn wraps_by_display_width() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(12, &theme);
            let mut out = Vec::new();

            prefixed_row(
                &mut out,
                &ctx,
                USER_PROMPT_PREFIX,
                theme.user(),
                "你好 世界 再见",
                theme.text(),
            );

            let rows: Vec<String> = out.iter().map(ToString::to_string).collect();
            assert_eq!(rows, ["❯ 你好 世界 ", "  再见"]);
        }
    }

    mod continuation_spans {
        use super::*;

        #[test]
        fn keep_the_bar_in_place_and_blank_the_cells_around_it() {
            let style = Theme::default().tool_border();

            let spans = continuation_spans("  ▎   ", style);

            assert_eq!(
                spans,
                [Span::raw("  "), Span::styled(BAR, style), Span::raw("   ")]
            );
        }
    }

    mod with_style {
        use super::*;

        #[test]
        fn patches_row_bg_onto_content_spans() {
            // The bar prefix stays transparent while the number, separator, and text inherit the
            // row bg, so the chrome column never tints and the band has no holes.
            let theme = Theme::default();
            let ctx = RenderCtx::new(80, &theme);
            let renderer = NumberedRows::with_style(
                &ctx,
                theme.tool_border(),
                99,
                " + ",
                theme.success(),
                Some(theme.diff_add()),
            );
            let mut out = Vec::new();
            renderer.render(&mut out, 7, "x", theme.success());

            let row = &out[0];
            assert_eq!(row.spans[0].style.bg, None, "bar prefix must stay clear");
            assert_eq!(row.spans[1].style.bg, theme.diff_add().bg);
            assert_eq!(row.spans[2].style.bg, theme.diff_add().bg);
            assert_eq!(row.spans[3].style.bg, theme.diff_add().bg);
        }

        #[test]
        fn pads_to_full_width_with_row_bg() {
            // Trailing pad fills remaining columns so the bg tint reaches `ctx.width`.
            let theme = Theme::default();
            let ctx = RenderCtx::new(40, &theme);
            let renderer = NumberedRows::with_style(
                &ctx,
                theme.tool_border(),
                99,
                " - ",
                theme.error(),
                Some(theme.diff_del()),
            );
            let mut out = Vec::new();
            renderer.render(&mut out, 1, "abc", theme.error());

            let row = &out[0];
            assert_eq!(row.width(), 40, "padded row must reach ctx.width");

            let last = row.spans.last().expect("trailing pad span");
            assert!(
                last.content.chars().all(|c| c == ' '),
                "trailing pad span must be spaces only, got {:?}",
                last.content,
            );
            assert_eq!(last.style.bg, theme.diff_del().bg);
        }

        #[test]
        fn wrapped_continuation_keeps_bg_under_text_column() {
            // Every wrapped line pads to `ctx.width` so the bg stays contiguous, and the bar prefix
            // on continuations stays transparent like the header line.
            let theme = Theme::default();
            let ctx = RenderCtx::new(20, &theme);
            let renderer = NumberedRows::with_style(
                &ctx,
                theme.tool_border(),
                99,
                " - ",
                theme.error(),
                Some(theme.diff_del()),
            );
            let mut out = Vec::new();
            renderer.render(&mut out, 1, "alpha beta gamma delta", theme.error());

            let rows: Vec<String> = out
                .iter()
                .map(|line| line.to_string().trim_end().to_owned())
                .collect();
            assert_eq!(rows, ["▎    1 - alpha beta", "▎        gamma delta"]);
            for line in &out {
                assert_eq!(line.width(), 20, "every wrapped row must reach ctx.width");
                assert_eq!(
                    line.spans.last().unwrap().style.bg,
                    theme.diff_del().bg,
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

    mod render {
        use super::*;

        #[test]
        fn emits_bar_number_separator_and_text() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(80, &theme);
            let renderer = NumberedRows::new(&ctx, theme.error(), 100);
            let mut out = Vec::new();
            renderer.render(&mut out, 7, "hello", theme.text());

            assert_eq!(
                out,
                [Line::from(vec![
                    Span::styled(TOOL_BORDER_CONT, theme.error()),
                    Span::styled("  7", theme.muted()),
                    Span::styled(" │ ", theme.tool_border()),
                    Span::styled("hello", theme.text()),
                ])]
            );
        }

        #[test]
        fn cuts_overlong_text_to_the_byte_budget_until_expanded() {
            let theme = Theme::default();
            let long = "a".repeat(MAX_ROW_BYTES + 50);
            let text_in = |expanded| {
                let ctx = RenderCtx {
                    expanded,
                    ..RenderCtx::new(4096, &theme)
                };
                let mut out = Vec::new();
                NumberedRows::new(&ctx, theme.tool_border(), 1).render(
                    &mut out,
                    1,
                    &long,
                    theme.text(),
                );
                out[0].spans[3].content.to_string()
            };

            let (compact, expanded) = (text_in(false), text_in(true));

            assert_eq!(compact, format!("{}...", "a".repeat(MAX_ROW_BYTES)));
            assert_eq!(expanded, long);
        }

        #[test]
        fn uses_a_custom_separator_in_place_of_the_pipe() {
            // On the diff path the separator carries the `- ` or `+ ` sign, and the text stays a
            // separate span so the sign can take its own color.
            let theme = Theme::default();
            let ctx = RenderCtx::new(30, &theme);
            let renderer = NumberedRows::with_style(
                &ctx,
                theme.tool_border(),
                99,
                " - ",
                theme.error(),
                Some(theme.diff_del()),
            );
            let mut out = Vec::new();

            renderer.render(&mut out, 14, r#"println!("x");"#, theme.error());

            let row = out.first().expect("renders one line");
            let contents: Vec<&str> = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(
                contents,
                [
                    TOOL_BORDER_CONT,
                    "14",
                    " - ",
                    r#"println!("x");"#,
                    "       "
                ]
            );
        }

        #[test]
        fn wraps_with_aligned_continuation_prefix() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(14, &theme);
            let renderer = NumberedRows::new(&ctx, theme.tool_border(), 10);
            let mut out = Vec::new();
            renderer.render(&mut out, 1, "alpha ab cde", theme.text());

            let rows: Vec<String> = out.iter().map(ToString::to_string).collect();
            assert_eq!(rows, ["▎    1 │ alpha", "▎        ab ", "▎        cde"]);
        }
    }

    mod truncate_to_bytes {
        use super::*;

        #[test]
        fn preserves_character_boundaries_at_the_byte_limit() {
            for (case, input, max_bytes, expected) in [
                ("fits", "hello", 10, "hello"),
                ("overflow", "hello world", 5, "hello..."),
                ("inside a multibyte character", "中中中中", 5, "中..."),
                ("exact multibyte boundary", "中中", 6, "中中"),
                ("inside a family emoji", "a👨‍👩‍👧b", 8, "a..."),
                ("inside a combining mark", "e\u{301}x", 2, "..."),
            ] {
                assert_eq!(truncate_to_bytes(input, max_bytes), expected, "{case}");
            }
        }
    }
}
