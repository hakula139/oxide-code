//! Text helpers: display-width-aware truncation and padding, count-based pluralization and
//! abbreviation, sentence joining, blank-edge trimming, and escaping for terminal output.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const ELLIPSIS: &str = "...";
/// Columns [`ELLIPSIS`] fills, one per byte since it is ASCII.
pub(crate) const ELLIPSIS_WIDTH: usize = ELLIPSIS.len();
const HORIZONTAL_ELLIPSIS: char = '\u{2026}';

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

/// `text`, which ends in variable external content, then the sentence `next`, with a period
/// between them unless `text` already ends a sentence. Never pass our own literal as `text`.
pub(crate) fn join_sentences(text: &str, next: &str) -> String {
    let separator = if text.ends_with(['.', '?', '!', HORIZONTAL_ELLIPSIS, '。', '？', '！']) {
        " "
    } else {
        ". "
    };
    format!("{text}{separator}{next}")
}

/// Drops `text`'s trailing whitespace and leading blank lines, keeping the first line's indent.
pub fn trim_blank_edges(text: &str) -> &str {
    let text = text.trim_end();
    let start = text.find(|c: char| !c.is_whitespace()).map_or(0, |first| {
        text[..first].rfind('\n').map_or(0, |newline| newline + 1)
    });
    &text[start..]
}

/// Truncates `s` to `max_width` display columns between grapheme clusters, appending
/// [`ELLIPSIS`] when shortened unless the ellipsis alone would overflow `max_width`.
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

/// Truncates `s` to `max_width` columns by replacing its middle with [`ELLIPSIS`], falling back
/// to [`truncate_to_width`] when neither side has room for a grapheme.
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

/// `s` followed by spaces up to `width` display columns, unchanged when already that wide.
pub fn pad_to_width(s: &str, width: usize) -> String {
    format!("{s}{}", fill(s, width))
}

/// `s` preceded by spaces up to `width` display columns, unchanged when already that wide.
pub fn pad_start_to_width(s: &str, width: usize) -> String {
    format!("{}{s}", fill(s, width))
}

fn fill(s: &str, width: usize) -> String {
    " ".repeat(width.saturating_sub(s.width()))
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
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// `text` with each [hidden](is_hidden) character other than a tab escaped by
/// [`char::escape_default`], line breaks included, so no part of it can hide from a reader.
pub fn escape_hidden(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c != '\t' && is_hidden(c) {
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
        fn passes_through_strings_that_fit() {
            assert_eq!(truncate_to_width("short", 10), "short");
            assert_eq!(truncate_to_width("exact", 5), "exact");
            assert_eq!(truncate_to_width("", 5), "");
        }

        #[test]
        fn appends_ellipsis_on_ascii_overflow() {
            assert_eq!(truncate_to_width("abcdefghij", 5), "ab...");
        }

        #[test]
        fn accounts_for_cjk_double_width() {
            // Width 5 leaves 2 columns beside the ellipsis, room for one 2-column char.
            assert_eq!(truncate_to_width("测试文本", 5), "测...");
        }

        #[test]
        fn zero_produces_empty() {
            assert_eq!(truncate_to_width("anything", 0), "");
        }

        #[test]
        fn drops_ellipsis_when_budget_below_ellipsis_width() {
            assert_eq!(truncate_to_width("abc", 1), "a");
            assert_eq!(truncate_to_width("abc", 2), "ab");
        }

        #[test]
        fn keeps_zwj_emoji_whole() {
            assert_eq!(truncate_to_width("👨‍👩‍👧 family photo", 5), "👨‍👩‍👧...");
        }
    }

    mod center_truncate_to_width {
        use super::*;

        #[test]
        fn passes_through_strings_that_fit() {
            assert_eq!(center_truncate_to_width("short", 10), "short");
            assert_eq!(center_truncate_to_width("exact", 5), "exact");
            assert_eq!(center_truncate_to_width("", 5), "");
        }

        #[test]
        fn keeps_head_and_tail_around_ellipsis() {
            assert_eq!(
                center_truncate_to_width("~/work/project/src/x.rs", 13),
                "~/wor.../x.rs",
            );
        }

        #[test]
        fn falls_back_to_right_truncation_when_budget_too_small() {
            assert_eq!(center_truncate_to_width("abcdef", 2), "ab");
            assert_eq!(center_truncate_to_width("abcdef", 1), "a");
        }

        #[test]
        fn accounts_for_cjk_double_width() {
            // Width 9 leaves 3 columns per side, room for one 2-column char each.
            assert_eq!(center_truncate_to_width("测试文本编辑", 9), "测...辑");
        }

        #[test]
        fn falls_back_to_right_truncate_when_cjk_overflows_both_halves() {
            // Width 5 leaves 1 column per side, too narrow for a 2-column char.
            assert_eq!(center_truncate_to_width("中文测试", 5), "中...");
        }

        #[test]
        fn keeps_zwj_emoji_whole_on_both_sides() {
            assert_eq!(center_truncate_to_width("👨‍👩‍👧 photos/2026 👨‍👩‍👧", 7), "👨‍👩‍👧...👨‍👩‍👧",);
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
}
