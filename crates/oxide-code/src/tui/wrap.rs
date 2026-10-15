//! Word-wrap with continuation indent for styled lines.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Wraps a line to fit within `max_width`, preserving visual indentation on continuation lines.
///
/// `continuation_indent` is the column count reserved on every wrapped sub-line after the first.
/// `continuation_prefix`, when supplied, replaces the default whitespace fill with styled spans
/// (blockquote `> ` markers, nested list indentation). The first line is never indented, since
/// callers prepend the original prefix themselves. Breaks fall between grapheme clusters, so ZWJ
/// emoji and combining sequences stay whole.
///
/// `max_width == 0` passes the line through unwrapped. Deep indents that would leave less than one
/// column for content fall back to a one-column minimum so wrapping still terminates.
pub(crate) fn wrap_line(
    line: Line<'static>,
    max_width: usize,
    continuation_indent: usize,
    continuation_prefix: Option<&[Span<'static>]>,
) -> Vec<Line<'static>> {
    wrap_line_keeping_lead(line, max_width, continuation_indent, continuation_prefix, 0)
}

/// Wraps like [`wrap_line`] without breaking inside the first `lead_width` columns, so a line's
/// prefix and indentation never end up alone on a row.
pub(crate) fn wrap_line_keeping_lead(
    line: Line<'static>,
    max_width: usize,
    continuation_indent: usize,
    continuation_prefix: Option<&[Span<'static>]>,
    lead_width: usize,
) -> Vec<Line<'static>> {
    if max_width == 0 {
        return vec![line];
    }

    let total_width = line_width(&line);
    if total_width <= max_width {
        return vec![line];
    }

    let styled: Vec<(&str, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.graphemes(true).map(move |g| (g, s.style)))
        .collect();

    // Keep at least one column on continuations so deep indents still make progress.
    let cont_content_width = max_width.saturating_sub(continuation_indent).max(1);
    let wrapped = greedy_word_wrap(&styled, max_width, cont_content_width, lead_width);

    wrapped
        .into_iter()
        .enumerate()
        .map(|(i, graphemes)| {
            let mut spans = if i > 0 && continuation_indent > 0 {
                if let Some(prefix) = continuation_prefix {
                    prefix.to_vec()
                } else {
                    vec![Span::raw(" ".repeat(continuation_indent))]
                }
            } else {
                Vec::new()
            };
            spans.extend(graphemes_to_spans(graphemes));
            Line::from(spans)
        })
        .collect()
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.width()).sum()
}

// ── Word Wrap Algorithm ──

/// Greedy word-wrap over styled graphemes. The first line wraps at `first_width` without breaking
/// inside its first `lead_width` columns, and the rest wrap at `subsequent_width`, which already
/// excludes the continuation indent.
fn greedy_word_wrap<'a>(
    graphemes: &'a [(&'a str, Style)],
    first_width: usize,
    subsequent_width: usize,
    lead_width: usize,
) -> Vec<&'a [(&'a str, Style)]> {
    let mut lines = Vec::new();
    let (end, mut pos) = take_one_line(graphemes, 0, first_width, lead_width);
    lines.push(&graphemes[..end]);
    while pos < graphemes.len() {
        let (end, next) = take_one_line(graphemes, pos, subsequent_width, 0);
        lines.push(&graphemes[pos..end]);
        pos = next;
    }
    lines
}

/// Measures one visual line starting at `start`, returning its end and the next line's start.
/// Rows break after whitespace and around wide characters, never inside the first `lead_width`
/// columns.
fn take_one_line(
    graphemes: &[(&str, Style)],
    start: usize,
    max_width: usize,
    lead_width: usize,
) -> (usize, usize) {
    let mut width = 0;
    let mut last_break: Option<usize> = None;

    for (i, &(g, _)) in graphemes.iter().enumerate().skip(start) {
        let g_width = g.width();

        if i > start && width >= lead_width && breaks_around_wide(graphemes[i - 1].0, g) {
            last_break = Some(i);
        }

        if width + g_width > max_width {
            let space_break = (is_whitespace(g) && width >= lead_width).then_some(i);
            if let Some(end) = space_break.or(last_break) {
                let mut next = end;
                while next < graphemes.len() && is_whitespace(graphemes[next].0) {
                    next += 1;
                }
                return (end, next);
            }
            // No break opportunity, so split before the overflowing grapheme.
            let end = if i == start { i + 1 } else { i };
            return (end, end);
        }

        if is_whitespace(g) && width >= lead_width {
            last_break = Some(i + 1);
        }
        width += g_width;
    }

    (graphemes.len(), graphemes.len())
}

fn is_whitespace(grapheme: &str) -> bool {
    grapheme.chars().all(char::is_whitespace)
}

/// Whether a row may end between `before` and `after` with no whitespace there. CJK text has no
/// spaces, so a wide grapheme can break from either neighbor.
fn breaks_around_wide(before: &str, after: &str) -> bool {
    !is_whitespace(before) && !is_whitespace(after) && (before.width() > 1 || after.width() > 1)
}

// ── Span Reconstruction ──

fn graphemes_to_spans(graphemes: &[(&str, Style)]) -> Vec<Span<'static>> {
    let Some(&(_, first_style)) = graphemes.first() else {
        return Vec::new();
    };

    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut current_style = first_style;

    for &(g, style) in graphemes {
        if style != current_style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), current_style));
            current_style = style;
        }
        buf.push_str(g);
    }

    if !buf.is_empty() {
        spans.push(Span::styled(buf, current_style));
    }

    spans
}

// ── Tab Expansion ──

const TAB_WIDTH: usize = 4;

/// Expands `\t` to spaces, aligning to [`TAB_WIDTH`]-column stops.
pub(crate) fn expand_tabs(s: &str) -> String {
    if !s.contains('\t') {
        return s.to_owned();
    }
    expand_tabs_from(s, &mut 0)
}

/// Expands tabs across `spans`, carrying the column so stops align for the whole line.
pub(crate) fn expand_tabs_in_spans(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut col = 0;
    spans
        .into_iter()
        .map(|mut span| {
            if span.content.contains('\t') {
                span.content = expand_tabs_from(&span.content, &mut col).into();
            } else {
                col += span.content.width();
            }
            span
        })
        .collect()
}

fn expand_tabs_from(s: &str, col: &mut usize) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for ch in s.chars() {
        if ch == '\t' {
            let spaces = TAB_WIDTH - (*col % TAB_WIDTH);
            for _ in 0..spaces {
                out.push(' ');
            }
            *col += spaces;
        } else {
            out.push(ch);
            *col += ch.width().unwrap_or(0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    use super::{expand_tabs, expand_tabs_in_spans, wrap_line, wrap_line_keeping_lead};

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    // ── wrap_line ──

    #[test]
    fn short_line_unchanged() {
        let line = Line::from("Hello, world!");
        let result = wrap_line(line, 80, 4, None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].spans[0].content.as_ref(), "Hello, world!");
    }

    #[test]
    fn zero_width_passes_through_unchanged() {
        let line = Line::from("Hello, world!");
        let result = wrap_line(line, 0, 4, None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].spans[0].content.as_ref(), "Hello, world!");
    }

    #[test]
    fn wraps_at_word_boundary() {
        let line = Line::from("Hello world foo bar");
        let result = wrap_line(line, 12, 0, None);
        let texts: Vec<String> = result
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(texts[0], "Hello world ");
        assert_eq!(texts[1], "foo bar");
    }

    #[test]
    fn continuation_indent_applied() {
        // 23 chars at width 16 indent 4 → first "    Hello world ", second "    foo bar".
        let line = Line::from(vec![Span::raw("    "), Span::raw("Hello world foo bar")]);
        let result = wrap_line(line, 16, 4, None);
        assert_eq!(
            result.len(),
            2,
            "should wrap to exactly two lines: {result:?}"
        );
        let cont = &result[1];
        assert!(
            cont.spans[0].content.starts_with("    "),
            "continuation should have 4-space indent: {cont:?}"
        );
    }

    #[test]
    fn preserves_styles_across_wrap() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let line = Line::from(vec![
            Span::styled("Bold ", bold),
            Span::raw("normal text that is long enough to wrap"),
        ]);
        let result = wrap_line(line, 20, 0, None);
        // 44 chars / width 20 → "Bold normal text " | "that is long enough " | "to wrap".
        assert_eq!(
            result.len(),
            3,
            "should wrap to exactly three lines: {result:?}"
        );
        let first_span = &result[0].spans[0];
        assert!(
            first_span.style.add_modifier.contains(Modifier::BOLD),
            "bold should be preserved: {first_span:?}"
        );
    }

    #[test]
    fn force_break_on_long_word() {
        let input = "abcdefghijklmnopqrstuvwxyz";
        let result = wrap_line(Line::from(input), 10, 0, None);
        // 26 chars / width 10 → 10 + 10 + 6.
        assert_eq!(
            result.len(),
            3,
            "should force-break into three lines: {result:?}"
        );
        let texts: Vec<&str> = result.iter().map(|l| l.spans[0].content.as_ref()).collect();
        assert_eq!(texts, [&input[..10], &input[10..20], &input[20..]]);
    }

    #[test]
    fn styled_continuation_indent() {
        let code_style = Style::default().fg(Color::Green);
        let line = Line::from(vec![
            Span::raw("  "),
            Span::styled("a b c d e f g h i j k l", code_style),
        ]);
        let result = wrap_line(line, 14, 2, None);
        assert!(result.len() >= 2, "should wrap: {result:?}");
        let cont = &result[1];
        assert_eq!(
            cont.spans[0].content.as_ref(),
            "  ",
            "2-space continuation indent"
        );
        let has_green = cont.spans.iter().any(|s| s.style.fg == Some(Color::Green));
        assert!(has_green, "style should be preserved on continuation");
    }

    #[test]
    fn continuation_prefix_spans_applied() {
        let marker_style = Style::default().fg(Color::Green);
        let line = Line::from(vec![
            Span::styled("> ", marker_style),
            Span::raw("one two three four five six"),
        ]);
        let prefix = vec![Span::styled("> ", marker_style)];
        let result = wrap_line(line, 12, 2, Some(&prefix));
        assert!(result.len() >= 2, "should wrap: {result:?}");
        let cont = &result[1];
        assert_eq!(
            cont.spans[0].content.as_ref(),
            "> ",
            "continuation should start with the styled prefix"
        );
        assert_eq!(cont.spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn wrap_line_breaks_cjk_by_display_width() {
        let result = wrap_line(Line::from("你好世界再见"), 5, 0, None);
        assert_eq!(texts(&result), ["你好", "世界", "再见"]);
    }

    #[test]
    fn wrap_line_keeps_zwj_emoji_whole() {
        let family = "👨‍👩‍👧";
        let result = wrap_line(Line::from(format!("ab{family}{family}")), 3, 0, None);
        assert_eq!(texts(&result), ["ab", family, family]);
    }

    #[test]
    fn wrap_line_breaks_between_wide_characters_and_their_neighbors() {
        let wrap = |s, width| texts(&wrap_line(Line::from(s), width, 0, None));
        assert_eq!(
            wrap("中文 Rust 代码很长", 8),
            ["中文 ", "Rust 代", "码很长"]
        );
        assert_eq!(wrap("中 文abcd", 5), ["中 文", "abcd"]);
        assert_eq!(wrap("中 ab文字", 5), ["中 ab", "文字"]);
    }

    #[test]
    fn wrap_line_drops_whitespace_left_over_at_a_break() {
        let wrap = |s| texts(&wrap_line(Line::from(s), 3, 0, None));
        assert_eq!(wrap("ab   cd"), ["ab ", "cd"]);
        assert_eq!(wrap("abc de"), ["abc", "de"]);
        assert_eq!(wrap("abc   de"), ["abc", "de"]);
    }

    // ── wrap_line_keeping_lead ──

    #[test]
    fn wrap_line_keeping_lead_breaks_only_from_the_lead_column_on() {
        let wrap = |lead| {
            texts(&wrap_line_keeping_lead(
                Line::from("  ab cd ef"),
                6,
                2,
                None,
                lead,
            ))
        };
        assert_eq!(wrap(4), ["  ab ", "  cd ", "  ef"]);
        assert_eq!(wrap(5), ["  ab c", "  d ef"]);
    }

    // ── expand_tabs ──

    #[test]
    fn expand_tabs_no_tabs_unchanged() {
        assert_eq!(expand_tabs("hello world"), "hello world");
    }

    #[test]
    fn expand_tabs_line_number_format() {
        assert_eq!(expand_tabs("1\tuse std::io;"), "1   use std::io;");
        assert_eq!(expand_tabs("10\tuse std::io;"), "10  use std::io;");
        assert_eq!(expand_tabs("100\tuse std::io;"), "100 use std::io;");
    }

    #[test]
    fn expand_tabs_mid_line_aligns_to_stop() {
        assert_eq!(expand_tabs("ab\tcd"), "ab  cd");
        assert_eq!(expand_tabs("abcd\tx"), "abcd    x");
    }

    // ── expand_tabs_in_spans ──

    #[test]
    fn expand_tabs_in_spans_aligns_stops_across_span_boundaries() {
        let red = Style::default().fg(Color::Red);
        let spans = vec![Span::raw("ab"), Span::styled("\tx", red)];
        let expanded = expand_tabs_in_spans(spans);
        assert_eq!(expanded[0].content, "ab");
        assert_eq!(expanded[1].content, "  x");
        assert_eq!(expanded[1].style, red);
    }
}
