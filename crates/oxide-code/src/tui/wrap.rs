//! Word-wrap with continuation indent for styled lines.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Wraps a line to fit within `max_width`, preserving visual indentation
/// on continuation lines.
///
/// `continuation_indent` is the number of leading columns consumed by
/// the continuation prefix. Each continuation line is prefixed with the
/// spans in `continuation_prefix` (if provided) or plain spaces.
///
/// Returns the original line unchanged when it fits within `max_width`.
pub(crate) fn wrap_line(
    line: Line<'static>,
    max_width: usize,
    continuation_indent: usize,
    continuation_prefix: Option<&[Span<'static>]>,
) -> Vec<Line<'static>> {
    if max_width == 0 {
        return vec![line];
    }

    let total_width = line_width(&line);
    if total_width <= max_width {
        return vec![line];
    }

    // Flatten spans into (char, style) pairs for character-level wrapping.
    let styled: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |ch| (ch, s.style)))
        .collect();

    // Ensure at least 1 column for content on continuation lines so that
    // deeply nested indents don't produce degenerate one-char-per-line output.
    let cont_content_width = max_width.saturating_sub(continuation_indent).max(1);
    let wrapped = greedy_word_wrap(&styled, max_width, cont_content_width);

    wrapped
        .into_iter()
        .enumerate()
        .map(|(i, chars)| {
            let mut spans = if i > 0 && continuation_indent > 0 {
                if let Some(prefix) = continuation_prefix {
                    prefix.to_vec()
                } else {
                    vec![Span::raw(" ".repeat(continuation_indent))]
                }
            } else {
                Vec::new()
            };
            spans.extend(chars_to_spans(&chars));
            Line::from(spans)
        })
        .collect()
}

/// Measure the display width of a line's spans.
fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.width()).sum()
}

// ── Word Wrap Algorithm ──

/// Greedy word-wrap over styled characters.
///
/// The first line wraps at `first_width`. Subsequent lines wrap at
/// `subsequent_width` (the caller has already subtracted the
/// continuation indent from the total budget).
fn greedy_word_wrap(
    chars: &[(char, Style)],
    first_width: usize,
    subsequent_width: usize,
) -> Vec<Vec<(char, Style)>> {
    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut pos = 0;
    let mut is_first = true;

    while pos < chars.len() {
        let budget = if is_first {
            first_width
        } else {
            subsequent_width
        };
        let (line, next) = take_one_line(chars, pos, budget);
        lines.push(line);
        pos = next;
        is_first = false;
    }

    if lines.is_empty() {
        lines.push(Vec::new());
    }

    lines
}

/// Consumes one visual line's worth of characters starting at `start`.
///
/// Returns the characters for this line and the index of the first
/// character of the next line.
fn take_one_line(
    chars: &[(char, Style)],
    start: usize,
    max_width: usize,
) -> (Vec<(char, Style)>, usize) {
    let mut width = 0;
    let mut last_break = None;

    for i in start..chars.len() {
        let ch_width = chars[i].0.width().unwrap_or(0);

        if width + ch_width > max_width {
            if let Some(bp) = last_break {
                // Break at the last whitespace.
                let line = chars[start..=bp].to_vec();
                let mut next = bp + 1;
                // Skip leading whitespace on the new line.
                while next < chars.len() && chars[next].0.is_whitespace() {
                    next += 1;
                }
                return (line, next);
            }
            // No whitespace break point — force break at the current position.
            let end = if i == start { i + 1 } else { i };
            return (chars[start..end].to_vec(), end);
        }

        width += ch_width;

        if chars[i].0.is_whitespace() {
            last_break = Some(i);
        }
    }

    // Everything from `start` fits on one line.
    (chars[start..].to_vec(), chars.len())
}

// ── Span Reconstruction ──

/// Group consecutive characters with the same style back into `Span`s.
fn chars_to_spans(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    if chars.is_empty() {
        return Vec::new();
    }

    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut current_style = chars[0].1;

    for &(ch, style) in chars {
        if style != current_style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), current_style));
            current_style = style;
        }
        buf.push(ch);
    }

    if !buf.is_empty() {
        spans.push(Span::styled(buf, current_style));
    }

    spans
}

// ── Tab Expansion ──

/// Tab stop width for expanding `\t` in tool output. Ratatui renders each
/// character into fixed-width cells, so tabs must be expanded to spaces.
const TAB_WIDTH: usize = 4;

/// Expand tab characters to spaces, aligning to [`TAB_WIDTH`]-column stops.
pub(crate) fn expand_tabs(s: &str) -> String {
    if !s.contains('\t') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 16);
    let mut col = 0;
    for ch in s.chars() {
        if ch == '\t' {
            let spaces = TAB_WIDTH - (col % TAB_WIDTH);
            for _ in 0..spaces {
                out.push(' ');
            }
            col += spaces;
        } else {
            out.push(ch);
            col += ch.width().unwrap_or(0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    use super::{expand_tabs, wrap_line};

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
        // "    Hello world foo bar" (23 chars), width 16, indent 4.
        // First line fits "    Hello world "; the break lands there and
        // the continuation carries the 4-space indent plus "foo bar".
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
        // 44 chars at width 20 wraps at the two word boundaries:
        // "Bold normal text " | "that is long enough " | "to wrap".
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
        // 26 chars at width 10 must produce three segments: 10 + 10 + 6.
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
        // Check that continuation has 2-space prefix.
        let cont = &result[1];
        assert_eq!(
            cont.spans[0].content.as_ref(),
            "  ",
            "2-space continuation indent"
        );
        // The remaining spans should still have green color.
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
}
