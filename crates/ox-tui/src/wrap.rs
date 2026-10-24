//! Word-wrap with continuation indent for styled lines.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

/// Wraps a line to fit within `max_width`, preserving visual indentation on continuation lines.
///
/// `continuation_indent` is the column count reserved on every wrapped sub-line after the first.
/// `continuation_prefix`, when supplied, replaces the default whitespace fill with styled spans
/// (blockquote `│ ` bars, nested list indentation). The first line is never indented, since
/// callers prepend the original prefix themselves. Breaks fall between grapheme clusters, so ZWJ
/// emoji and combining sequences stay whole. Every row keeps the line's style and alignment.
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

    let total_width = line.width();
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
            Line {
                style: line.style,
                alignment: line.alignment,
                spans,
            }
        })
        .collect()
}

/// The width of the widest run [`wrap_line`] never breaks inside, so the narrowest width that wraps
/// `line` without splitting a word. CJK text can break between any two characters.
pub(crate) fn longest_word_width(line: &Line<'_>) -> usize {
    let mut longest = 0;
    let mut run = 0;
    let mut prev = None;
    for g in line.spans.iter().flat_map(|s| s.content.graphemes(true)) {
        if is_whitespace(g) {
            run = 0;
        } else {
            if prev.is_some_and(|prev| breaks_around_wide(prev, g)) {
                run = 0;
            }
            run += g.width();
            longest = longest.max(run);
        }
        prev = Some(g);
    }
    longest
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

/// Measures one visual line starting at `start`, returning its end and the next line's start. Rows
/// break after whitespace and around wide characters, never in the first `lead_width` columns.
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
            // Break at the last opportunity, or else split before the overflowing grapheme, one
            // earlier when it is closing punctuation or the row would end on opening punctuation.
            let end = space_break.or(last_break).unwrap_or(if i == start {
                i + 1
            } else if i - 1 > start
                && (is_punctuation_in(g, CLOSING_PUNCTUATION)
                    || is_punctuation_in(graphemes[i - 1].0, OPENING_PUNCTUATION))
            {
                i - 1
            } else {
                i
            });
            let mut next = end;
            while next < graphemes.len() && is_whitespace(graphemes[next].0) {
                next += 1;
            }
            return (end, next);
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

/// Returns whether a row may end between `before` and `after` without whitespace. CJK text has no
/// spaces, so a wide grapheme can break from either neighbor unless opening punctuation precedes or
/// closing punctuation follows.
fn breaks_around_wide(before: &str, after: &str) -> bool {
    !is_whitespace(before)
        && !is_whitespace(after)
        && (before.width() > 1 || after.width() > 1)
        && !is_punctuation_in(before, OPENING_PUNCTUATION)
        && !is_punctuation_in(after, CLOSING_PUNCTUATION)
}

/// Punctuation that must not end a row.
const OPENING_PUNCTUATION: &str = "「『（［｛〈《【〔“‘([{";

/// Punctuation that must not start a row.
const CLOSING_PUNCTUATION: &str = "、。，．：；！？）］｝〉》」』】〕”’,.:;!?)]}";

fn is_punctuation_in(grapheme: &str, set: &str) -> bool {
    grapheme.chars().all(|c| set.contains(c))
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
    for g in s.graphemes(true) {
        if g == "\t" {
            let spaces = TAB_WIDTH - (*col % TAB_WIDTH);
            out.extend(std::iter::repeat_n(' ', spaces));
            *col += spaces;
        } else {
            out.push_str(g);
            *col += g.width();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    mod wrap_line {
        use super::*;

        #[test]
        fn short_line_is_unchanged() {
            let line = Line::from("Hello, world!");
            assert_eq!(wrap_line(line.clone(), 80, 4, None), [line]);
        }

        #[test]
        fn zero_width_passes_through_unchanged() {
            let line = Line::from("Hello, world!");
            assert_eq!(wrap_line(line.clone(), 0, 4, None), [line]);
        }

        #[test]
        fn wraps_at_word_boundary() {
            let result = wrap_line(Line::from("Hello world foo bar"), 12, 0, None);
            assert_eq!(texts(&result), ["Hello world ", "foo bar"]);
        }

        #[test]
        fn indents_continuation_rows() {
            let line = Line::from(vec![Span::raw("    "), Span::raw("Hello world foo bar")]);
            let result = wrap_line(line, 16, 4, None);
            assert_eq!(texts(&result), ["    Hello world ", "    foo bar"]);
        }

        #[test]
        fn preserves_styles_across_wrap() {
            let bold = Style::default().add_modifier(Modifier::BOLD);
            let line = Line::from(vec![
                Span::styled("Bold ", bold),
                Span::raw("normal text that is long enough to wrap"),
            ]);
            let result = wrap_line(line, 20, 0, None);
            assert_eq!(
                result,
                [
                    Line::from(vec![Span::styled("Bold ", bold), Span::raw("normal text ")]),
                    Line::from("that is long enough "),
                    Line::from("to wrap"),
                ]
            );
        }

        #[test]
        fn keeps_line_style_and_alignment_on_every_row() {
            let style = Style::default().fg(Color::Green);
            let line = Line::from("one two three").style(style).right_aligned();

            let result = wrap_line(line, 8, 2, None);

            let row = |spans: Vec<Span<'static>>| Line::from(spans).style(style).right_aligned();
            assert_eq!(
                result,
                [
                    row(vec![Span::raw("one two ")]),
                    row(vec![Span::raw("  "), Span::raw("three")]),
                ]
            );
        }

        #[test]
        fn force_breaks_a_word_wider_than_the_row() {
            let input = "abcdefghijklmnopqrstuvwxyz";
            let result = wrap_line(Line::from(input), 10, 0, None);
            assert_eq!(texts(&result), [&input[..10], &input[10..20], &input[20..]]);
        }

        #[test]
        fn keeps_span_style_on_indented_continuations() {
            let code_style = Style::default().fg(Color::Green);
            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled("a b c d e f g h i j k l", code_style),
            ]);
            let result = wrap_line(line, 14, 2, None);
            assert_eq!(
                result,
                [
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled("a b c d e f ", code_style)
                    ]),
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled("g h i j k l", code_style)
                    ]),
                ]
            );
        }

        #[test]
        fn repeats_styled_prefix_on_continuations() {
            let marker_style = Style::default().fg(Color::Green);
            let line = Line::from(vec![
                Span::styled("> ", marker_style),
                Span::raw("one two three four five six"),
            ]);
            let prefix = vec![Span::styled("> ", marker_style)];
            let result = wrap_line(line, 12, 2, Some(&prefix));
            let row = |text| Line::from(vec![Span::styled("> ", marker_style), Span::raw(text)]);
            assert_eq!(
                result,
                [row("one two "), row("three four"), row("five six")]
            );
        }

        #[test]
        fn breaks_cjk_by_display_width() {
            let result = wrap_line(Line::from("你好世界再见"), 5, 0, None);
            assert_eq!(texts(&result), ["你好", "世界", "再见"]);
            let too_narrow = wrap_line(Line::from("你好"), 1, 0, None);
            assert_eq!(texts(&too_narrow), ["你", "好"]);
        }

        #[test]
        fn keeps_zwj_emoji_whole() {
            let family = "👨‍👩‍👧";
            let result = wrap_line(Line::from(format!("ab{family}{family}")), 3, 0, None);
            assert_eq!(texts(&result), ["ab", family, family]);
        }

        #[test]
        fn breaks_between_wide_characters_and_their_neighbors() {
            let wrap = |s, width| texts(&wrap_line(Line::from(s), width, 0, None));
            assert_eq!(
                wrap("中文 Rust 代码很长", 8),
                ["中文 ", "Rust 代", "码很长"]
            );
            assert_eq!(wrap("中 文abcd", 5), ["中 文", "abcd"]);
            assert_eq!(wrap("中 ab文字", 5), ["中 ab", "文字"]);
        }

        #[test]
        fn keeps_closing_punctuation_off_the_row_start() {
            let wrap = |s, width| texts(&wrap_line(Line::from(s), width, 0, None));
            assert_eq!(wrap("中文字，好", 6), ["中文", "字，好"]);
            assert_eq!(wrap("中文字.", 6), ["中文", "字."]);
            assert_eq!(wrap("abcde，", 5), ["abcd", "e，"]);
        }

        #[test]
        fn keeps_opening_punctuation_off_the_row_end() {
            let wrap = |s, width| texts(&wrap_line(Line::from(s), width, 0, None));
            assert_eq!(
                wrap("这是「测试」文本", 6),
                ["这是", "「测", "试」文", "本"]
            );
            assert_eq!(wrap("看看（例子）吧", 6), ["看看", "（例", "子）吧"]);
            assert_eq!(wrap("abcd（e", 5), ["abcd", "（e"]);
        }

        #[test]
        fn drops_whitespace_left_over_at_a_break() {
            let wrap = |s| texts(&wrap_line(Line::from(s), 3, 0, None));
            assert_eq!(wrap("ab   cd"), ["ab ", "cd"]);
            assert_eq!(wrap("abc de"), ["abc", "de"]);
            assert_eq!(wrap("abc   de"), ["abc", "de"]);
            assert_eq!(
                texts(&wrap_line(Line::from("✅ ab"), 1, 0, None)),
                ["✅", "a", "b"],
                "a grapheme wider than the row splits off without a blank row after it"
            );
        }
    }

    mod wrap_line_keeping_lead {
        use super::*;

        #[test]
        fn breaks_only_from_the_lead_column_on() {
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
    }

    mod longest_word_width {
        use super::*;

        #[test]
        fn spans_style_changes_and_stops_at_wrap_breaks() {
            let bold = Style::default().add_modifier(Modifier::BOLD);
            let styled = Line::from(vec![
                Span::raw("ab cd"),
                Span::styled("ef ", bold),
                Span::raw("g"),
            ]);

            assert_eq!(longest_word_width(&styled), 4);
            assert_eq!(longest_word_width(&Line::from("中文，好")), 4);
            assert_eq!(longest_word_width(&Line::from("✅ a")), 2);
            assert_eq!(longest_word_width(&Line::default()), 0);
        }
    }

    mod expand_tabs {
        use super::*;

        #[test]
        fn leaves_text_without_tabs_unchanged() {
            assert_eq!(expand_tabs("hello world"), "hello world");
        }

        #[test]
        fn aligns_line_number_prefixes_to_the_first_stop() {
            assert_eq!(expand_tabs("1\tuse std::io;"), "1   use std::io;");
            assert_eq!(expand_tabs("10\tuse std::io;"), "10  use std::io;");
            assert_eq!(expand_tabs("100\tuse std::io;"), "100 use std::io;");
        }

        #[test]
        fn mid_line_aligns_to_stop() {
            assert_eq!(expand_tabs("ab\tcd"), "ab  cd");
            assert_eq!(expand_tabs("abcd\tx"), "abcd    x");
        }

        #[test]
        fn measures_emoji_sequences_as_whole_clusters() {
            assert_eq!(expand_tabs("❤️\tx"), "❤️  x");
            assert_eq!(expand_tabs("👨‍👩‍👧\tx"), "👨‍👩‍👧  x");
        }
    }

    mod expand_tabs_in_spans {
        use super::*;

        #[test]
        fn aligns_stops_across_span_boundaries() {
            let red = Style::default().fg(Color::Red);
            let spans = vec![Span::raw("ab"), Span::styled("\tx", red)];
            let expanded = expand_tabs_in_spans(spans);
            assert_eq!(expanded[0].content, "ab");
            assert_eq!(expanded[1].content, "  x");
            assert_eq!(expanded[1].style, red);
        }
    }
}
