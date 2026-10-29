//! Text helpers: display-width-aware truncation and padding, count-based pluralization and
//! abbreviation, byte-size labels, sentence joining, blank-edge trimming, and terminal escaping.

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

pub const ELLIPSIS: &str = "...";
/// Columns [`ELLIPSIS`] fills, one per byte since it is ASCII.
pub(crate) const ELLIPSIS_WIDTH: usize = ELLIPSIS.len();
const HORIZONTAL_ELLIPSIS: char = '\u{2026}';

/// Returns `s` with its first character uppercased, for a headline shown as its own sentence.
pub fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `singular` when `count` is one, `plural` otherwise.
pub fn plural<'a, N: Copy + PartialEq + From<u8>>(
    count: N,
    singular: &'a str,
    plural: &'a str,
) -> &'a str {
    if count == N::from(1) {
        singular
    } else {
        plural
    }
}

/// `n` in whole thousands or millions past 999, as in `12k` or `3M`. Below a billion it takes at
/// most 4 columns.
pub fn compact_count(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// `bytes` in the largest binary unit it reaches, as in `512 B`, `1.5 KiB`, or `10 MiB`. A size
/// that is not a whole number of its unit shows one decimal, rounded down.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let exp = (bytes.checked_ilog2().unwrap_or(0) / 10).min(3);
    let name = UNITS[exp as usize];
    let unit = 1 << (10 * exp);
    let (whole, rest) = (bytes / unit, bytes % unit);
    if rest == 0 {
        format!("{whole} {name}")
    } else {
        format!("{whole}.{} {name}", rest * 10 / unit)
    }
}

/// `text`, which ends in variable external content, then the sentence `next`, with a period between
/// them unless `text` already ends a sentence. Never pass our own literal as `text`.
pub(crate) fn join_sentences(text: &str, next: &str) -> String {
    let separator = if text.ends_with(['.', '?', '!', HORIZONTAL_ELLIPSIS, '。', '？', '！']) {
        " "
    } else {
        ". "
    };
    format!("{text}{separator}{next}")
}

/// `items` as an English list: `a`, `a and b`, or `a, b, and c`.
pub(crate) fn list_with_and<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [only] => only.as_ref().to_owned(),
        [first, second] => format!("{} and {}", first.as_ref(), second.as_ref()),
        [rest @ .., last] => {
            let rest: Vec<&str> = rest.iter().map(AsRef::as_ref).collect();
            format!("{}, and {}", rest.join(", "), last.as_ref())
        }
    }
}

/// Drops `text`'s trailing whitespace and leading blank lines, keeping the first line's indent.
pub fn trim_blank_edges(text: &str) -> &str {
    let text = text.trim_end();
    let start = text.find(|c: char| !c.is_whitespace()).map_or(0, |first| {
        text[..first].rfind('\n').map_or(0, |newline| newline + 1)
    });
    &text[start..]
}

/// Truncates `s` to `max_width` display columns between grapheme clusters, appending [`ELLIPSIS`]
/// when shortened unless the ellipsis alone would overflow `max_width`.
pub fn truncate_to_width(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_owned();
    }
    if max_width < ELLIPSIS_WIDTH {
        return take_head(s, max_width);
    }
    let mut out = take_head(s, max_width - ELLIPSIS_WIDTH);
    out.push_str(ELLIPSIS);
    out
}

/// Truncates `s` to `max_width` columns by replacing its middle with [`ELLIPSIS`], falling back to
/// [`truncate_to_width`] when neither side has room for a grapheme.
pub fn center_truncate_to_width(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_owned();
    }
    if max_width < ELLIPSIS_WIDTH {
        return truncate_to_width(s, max_width);
    }
    let budget = max_width - ELLIPSIS_WIDTH;
    let head = take_head(s, budget / 2);
    let tail = take_tail(s, budget - budget / 2);
    if head.is_empty() && tail.is_empty() {
        return truncate_to_width(s, max_width);
    }
    format!("{head}{ELLIPSIS}{tail}")
}

/// The longest prefix of `s` that fits `budget` display columns, cut between grapheme clusters.
pub fn take_head(s: &str, budget: usize) -> String {
    let mut used = 0;
    s.graphemes(true)
        .take_while(|g| {
            used += g.width();
            used <= budget
        })
        .collect()
}

fn take_tail(s: &str, budget: usize) -> String {
    let mut used = 0;
    let len: usize = s
        .graphemes(true)
        .rev()
        .take_while(|g| {
            used += g.width();
            used <= budget
        })
        .map(str::len)
        .sum();
    s[s.len() - len..].to_owned()
}

/// `s` followed by spaces up to `width` display columns, unchanged when at least that wide.
pub fn pad_to_width(s: &str, width: usize) -> String {
    format!("{s}{}", fill(s, width))
}

/// `s` preceded by spaces up to `width` display columns, unchanged when at least that wide.
pub fn pad_start_to_width(s: &str, width: usize) -> String {
    format!("{}{s}", fill(s, width))
}

fn fill(s: &str, width: usize) -> String {
    " ".repeat(width.saturating_sub(s.width()))
}

/// Returns whether a terminal paints `c` as no visible cell: a control or zero-width character.
pub fn is_hidden(c: char) -> bool {
    c.is_control() || c.width().is_none_or(|w| w == 0)
}

/// `text` with each control character other than a tab or a line break escaped by
/// [`char::escape_default`], so writing it to a terminal cannot move the cursor or change the
/// terminal's state.
pub fn escape_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let line_break = c == '\n' || (c == '\r' && chars.peek() == Some(&'\n'));
        if c.is_control() && c != '\t' && !line_break {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// `text` with every control character escaped by [`char::escape_default`], tabs and line breaks
/// included, so it prints on one row of a table and cannot change the terminal's state.
pub fn escape_line(text: &str) -> String {
    escape_where(text, char::is_control)
}

/// `text` with each [hidden](is_hidden) character other than a tab escaped by
/// [`char::escape_default`], line breaks included, so no part of it can hide from a reader.
pub fn escape_hidden(text: &str) -> String {
    escape_where(text, |c| c != '\t' && is_hidden(c))
}

fn escape_where(text: &str, escape: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if escape(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    mod plural {
        use super::*;

        #[test]
        fn uses_singular_only_for_one() {
            for (count, expected) in [(0, "files"), (1, "file"), (2, "files")] {
                assert_eq!(plural(count, "file", "files"), expected);
            }
        }
    }

    mod compact_count {
        use super::*;

        #[test]
        fn rounds_down_to_whole_thousands_and_millions() {
            for (n, expected) in [
                (999, "999"),
                (1_000, "1k"),
                (1_500, "1k"),
                (12_345, "12k"),
                (999_999, "999k"),
                (1_000_000, "1M"),
                (999_999_999, "999M"),
            ] {
                assert_eq!(compact_count(n), expected);
            }
        }
    }

    mod format_size {
        use super::*;

        #[test]
        fn picks_the_largest_unit_and_rounds_down_to_one_decimal() {
            for (bytes, expected) in [
                (0, "0 B"),
                (1023, "1023 B"),
                (1024, "1 KiB"),
                (1536, "1.5 KiB"),
                ((1 << 20) - 1, "1023.9 KiB"),
                (10 << 20, "10 MiB"),
                (3 << 19, "1.5 MiB"),
                ((10 << 20) + 1, "10.0 MiB"),
                (3 << 30, "3 GiB"),
                ((5 << 30) / 2, "2.5 GiB"),
                (5000 << 30, "5000 GiB"),
            ] {
                assert_eq!(format_size(bytes), expected);
            }
        }
    }

    mod join_sentences {
        use super::*;

        #[test]
        fn adds_a_period_only_after_text_that_does_not_end_a_sentence() {
            for (text, expected) in [
                ("Upstream is down.", "Upstream is down. Retry later."),
                ("Is it down?", "Is it down? Retry later."),
                ("It is down!", "It is down! Retry later."),
                (
                    "Upstream timed out...",
                    "Upstream timed out... Retry later.",
                ),
                (
                    "Upstream timed out\u{2026}",
                    "Upstream timed out\u{2026} Retry later.",
                ),
                ("上游已断开。", "上游已断开。 Retry later."),
                ("上游断开了吗？", "上游断开了吗？ Retry later."),
                ("上游断开了！", "上游断开了！ Retry later."),
                ("Upstream is down", "Upstream is down. Retry later."),
            ] {
                assert_eq!(join_sentences(text, "Retry later."), expected);
            }
        }
    }

    mod list_with_and {
        use super::*;

        #[test]
        fn joins_two_items_with_and_and_longer_lists_with_a_serial_comma() {
            for (items, expected) in [
                (&[][..], ""),
                (&["a"][..], "a"),
                (&["a", "b"][..], "a and b"),
                (&["a", "b", "c"][..], "a, b, and c"),
            ] {
                assert_eq!(list_with_and(items), expected, "{items:?}");
            }
        }
    }

    mod trim_blank_edges {
        use super::*;

        #[test]
        fn keeps_the_indent_of_the_first_line_with_text() {
            assert_eq!(
                trim_blank_edges("\n \t\n    fn main() {}\n  done \n\n"),
                "    fn main() {}\n  done",
            );
            assert_eq!(trim_blank_edges("  single"), "  single");
        }

        #[test]
        fn empties_all_blank_text() {
            assert_eq!(trim_blank_edges(" \n\t\n "), "");
        }
    }

    mod truncate_to_width {
        use super::*;

        #[test]
        fn respects_display_width_and_grapheme_boundaries() {
            for (case, input, width, expected) in [
                ("fits", "short", 10, "short"),
                ("exact fit", "exact", 5, "exact"),
                ("empty", "", 5, ""),
                ("ASCII overflow", "abcdefghij", 5, "ab..."),
                ("CJK width", "测试文本", 5, "测..."),
                ("zero budget", "anything", 0, ""),
                ("one column", "abc", 1, "a"),
                ("two columns", "abc", 2, "ab"),
                ("whole ZWJ emoji", "👨‍👩‍👧 family photo", 5, "👨‍👩‍👧..."),
            ] {
                assert_eq!(truncate_to_width(input, width), expected, "{case}");
            }
        }
    }

    mod center_truncate_to_width {
        use super::*;

        #[test]
        fn respects_display_width_and_grapheme_boundaries() {
            for (case, input, width, expected) in [
                ("fits", "short", 10, "short"),
                ("exact fit", "exact", 5, "exact"),
                ("empty", "", 5, ""),
                (
                    "ASCII head and tail",
                    "~/work/project/src/x.rs",
                    13,
                    "~/wor.../x.rs",
                ),
                ("two-column fallback", "abcdef", 2, "ab"),
                ("one-column fallback", "abcdef", 1, "a"),
                ("CJK sides", "测试文本编辑", 9, "测...辑"),
                ("narrow CJK halves", "中文测试", 5, "中..."),
                (
                    "whole ZWJ emoji on both sides",
                    "👨‍👩‍👧 photos/2026 👨‍👩‍👧",
                    7,
                    "👨‍👩‍👧...👨‍👩‍👧",
                ),
            ] {
                assert_eq!(center_truncate_to_width(input, width), expected, "{case}");
            }
        }
    }

    mod pad_to_width {
        use super::*;

        #[test]
        fn fills_display_columns_after_wide_characters() {
            assert_eq!(pad_to_width("项目", 6), "项目  ");
            assert_eq!(pad_to_width("abc", 2), "abc");
        }
    }

    mod pad_start_to_width {
        use super::*;

        #[test]
        fn fills_display_columns_before_wide_characters() {
            assert_eq!(pad_start_to_width("项目", 6), "  项目");
            assert_eq!(pad_start_to_width("abc", 2), "abc");
        }
    }

    mod is_hidden {
        use super::*;

        #[test]
        fn flags_control_and_zero_width_characters_only() {
            for c in ['\u{1b}', '\n', '\u{200b}', '\u{200d}'] {
                assert!(is_hidden(c), "{c:?}");
            }
            for c in ['a', ' ', '中', '·'] {
                assert!(!is_hidden(c), "{c:?}");
            }
        }
    }

    mod escape_controls {
        use super::*;

        #[test]
        fn escapes_terminal_sequences_but_keeps_tabs_and_line_breaks() {
            assert_eq!(
                escape_controls("a\tb\r\nc\n\u{1b}[2K\rd\u{7}\u{200b}"),
                "a\tb\r\nc\n\\u{1b}[2K\\rd\\u{7}\u{200b}",
            );
        }
    }

    mod escape_line {
        use super::*;

        #[test]
        fn escapes_every_control_character_but_keeps_zwj_emoji() {
            assert_eq!(
                escape_line("a\tb\nc\r\n\u{1b}]52;c;eA==\u{7}👩\u{200d}💻"),
                "a\\tb\\nc\\r\\n\\u{1b}]52;c;eA==\\u{7}👩\u{200d}💻",
            );
        }
    }

    mod escape_hidden {
        use super::*;

        #[test]
        fn escapes_controls_line_breaks_and_zero_width_characters_but_keeps_tabs() {
            assert_eq!(
                escape_hidden("a\tb\r\nc\u{1b}[2K\u{200b}d 中"),
                "a\tb\\r\\nc\\u{1b}[2K\\u{200b}d 中",
            );
        }
    }

    mod capitalize_first {
        use super::*;

        #[test]
        fn uppercases_the_first_character_and_leaves_the_rest() {
            assert_eq!(capitalize_first("failed to open x"), "Failed to open x");
            assert_eq!(capitalize_first("\u{e9}"), "\u{c9}");
            assert_eq!(capitalize_first(""), "");
        }
    }
}
