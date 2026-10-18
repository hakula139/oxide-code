//! Text helpers: display-width-aware truncation, count-based pluralization, sentence joining, and
//! escaping for terminal output.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) const ELLIPSIS: &str = "...";
/// Columns [`ELLIPSIS`] fills, one per byte since it is ASCII.
pub(crate) const ELLIPSIS_WIDTH: usize = ELLIPSIS.len();
const HORIZONTAL_ELLIPSIS: char = '\u{2026}';

/// `singular` when `count` is one, `plural` otherwise.
#[expect(
    clippy::needless_pass_by_value,
    reason = "callers pass integer counts directly"
)]
pub(crate) fn plural<'a, N: PartialEq + From<u8>>(
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

/// Truncates `s` to `max_width` display columns, appending [`ELLIPSIS`] when shortened.
///
/// Width is measured in terminal columns (CJK and emoji take 2), and cuts fall between grapheme
/// clusters so ZWJ emoji and combining marks stay whole. The ellipsis is dropped when
/// `max_width < ELLIPSIS_WIDTH` because emitting it would itself overflow the budget.
pub(crate) fn truncate_to_width(s: &str, max_width: usize) -> String {
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

/// Truncates `s` to `max_width` columns by removing characters from the middle, replacing the
/// excised run with [`ELLIPSIS`].
///
/// Preserves head and tail context. Falls back to right-truncation when the budget is too small
/// for any context on either side (`max_width < ELLIPSIS_WIDTH`, or both halves drop their first
/// CJK char).
pub(crate) fn center_truncate_to_width(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_owned();
    }
    if max_width < ELLIPSIS_WIDTH {
        return truncate_to_width(s, max_width);
    }
    let budget = max_width - ELLIPSIS_WIDTH;
    let head = take_head(s, budget / 2);
    let tail = take_tail(s, budget - budget / 2);
    // Both halves dropped their first char (e.g., a 2-col CJK head into a 1-col half-budget).
    // Right-truncate so we surface at least the leading context.
    if head.is_empty() && tail.is_empty() {
        return truncate_to_width(s, max_width);
    }
    format!("{head}{ELLIPSIS}{tail}")
}

/// The longest prefix of `s` that fits `budget` display columns, cut between grapheme clusters.
pub(crate) fn take_head(s: &str, budget: usize) -> String {
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

/// Whether a terminal paints `c` as no visible cell: a control or zero-width character.
pub(crate) fn is_hidden(c: char) -> bool {
    c.is_control() || c.width().is_none_or(|w| w == 0)
}

/// `text` with each control character other than a tab or a line break escaped by
/// [`char::escape_default`], so writing it to a terminal cannot move the cursor or change the
/// terminal's state.
pub(crate) fn escape_controls(text: &str) -> String {
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

/// `text` with each [hidden](is_hidden) character other than a tab escaped by
/// [`char::escape_default`], line breaks included, so no part of it can hide from a reader.
pub(crate) fn escape_hidden(text: &str) -> String {
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

    // ── plural ──

    #[test]
    fn plural_uses_singular_only_for_one() {
        for (count, expected) in [(0, "files"), (1, "file"), (2, "files")] {
            assert_eq!(plural(count, "file", "files"), expected);
        }
    }

    // ── join_sentences ──

    #[test]
    fn join_sentences_adds_a_period_only_after_text_that_does_not_end_a_sentence() {
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

    // ── truncate_to_width ──

    #[test]
    fn truncate_to_width_passes_through_strings_that_fit() {
        assert_eq!(truncate_to_width("short", 10), "short");
        assert_eq!(truncate_to_width("exact", 5), "exact");
        assert_eq!(truncate_to_width("", 5), "");
    }

    #[test]
    fn truncate_to_width_appends_ellipsis_on_ascii_overflow() {
        assert_eq!(truncate_to_width("abcdefghij", 5), "ab...");
    }

    #[test]
    fn truncate_to_width_accounts_for_cjk_double_width() {
        // 测试文本 = 4 chars × 2 cols = 8 cols. Budget = 5 − 3 (ellipsis) = 2 cols → one char fits.
        assert_eq!(truncate_to_width("测试文本", 5), "测...");
    }

    #[test]
    fn truncate_to_width_zero_produces_empty() {
        assert_eq!(truncate_to_width("anything", 0), "");
    }

    #[test]
    fn truncate_to_width_drops_ellipsis_when_budget_below_ellipsis_width() {
        // Emitting "..." into a 1- or 2-col slot would overflow the budget, so the marker drops.
        assert_eq!(truncate_to_width("abc", 1), "a");
        assert_eq!(truncate_to_width("abc", 2), "ab");
    }

    #[test]
    fn truncate_to_width_keeps_zwj_emoji_whole() {
        assert_eq!(truncate_to_width("👨‍👩‍👧 family photo", 5), "👨‍👩‍👧...");
    }

    // ── center_truncate_to_width ──

    #[test]
    fn center_truncate_to_width_passes_through_strings_that_fit() {
        assert_eq!(center_truncate_to_width("short", 10), "short");
        assert_eq!(center_truncate_to_width("exact", 5), "exact");
        assert_eq!(center_truncate_to_width("", 5), "");
    }

    #[test]
    fn center_truncate_to_width_keeps_head_and_tail_around_ellipsis() {
        // Path use case: `~/work/project/src/x.rs` should keep the `~/` prefix and `.rs` leaf.
        assert_eq!(
            center_truncate_to_width("~/work/project/src/x.rs", 13),
            "~/wor.../x.rs",
        );
    }

    #[test]
    fn center_truncate_to_width_falls_back_to_right_truncation_when_budget_too_small() {
        // Below the ellipsis floor, center-truncate would underflow the budget split, so the
        // right-truncate fallback returns whatever fits.
        assert_eq!(center_truncate_to_width("abcdef", 2), "ab");
        assert_eq!(center_truncate_to_width("abcdef", 1), "a");
    }

    #[test]
    fn center_truncate_to_width_accounts_for_cjk_double_width() {
        // 测试文本编辑 = 6 chars × 2 cols = 12 cols. Width 9 = 3 head + `...` + 3 tail, so only one
        // 2-col CJK char fits each side.
        assert_eq!(center_truncate_to_width("测试文本编辑", 9), "测...辑");
    }

    #[test]
    fn center_truncate_to_width_falls_back_to_right_truncate_when_cjk_overflows_both_halves() {
        // 中文测试 = 8 cols. Width 5 leaves a budget of 2, so each half gets 1 and neither side
        // fits a 2-col char. Centering would yield a bare "...", so it right-truncates instead.
        assert_eq!(center_truncate_to_width("中文测试", 5), "中...");
    }

    #[test]
    fn center_truncate_to_width_keeps_zwj_emoji_whole_on_both_sides() {
        assert_eq!(center_truncate_to_width("👨‍👩‍👧 photos/2026 👨‍👩‍👧", 7), "👨‍👩‍👧...👨‍👩‍👧",);
    }

    // ── is_hidden ──

    #[test]
    fn is_hidden_flags_control_and_zero_width_characters_only() {
        for c in ['\u{1b}', '\n', '\u{200b}', '\u{200d}'] {
            assert!(is_hidden(c), "{c:?}");
        }
        for c in ['a', ' ', '中', '·'] {
            assert!(!is_hidden(c), "{c:?}");
        }
    }

    // ── escape_controls ──

    #[test]
    fn escape_controls_escapes_terminal_sequences_but_keeps_tabs_and_line_breaks() {
        assert_eq!(
            escape_controls("a\tb\r\nc\n\u{1b}[2K\rd\u{7}\u{200b}"),
            "a\tb\r\nc\n\\u{1b}[2K\\rd\\u{7}\u{200b}",
        );
    }

    // ── escape_hidden ──

    #[test]
    fn escape_hidden_escapes_controls_line_breaks_and_zero_width_characters_but_keeps_tabs() {
        assert_eq!(
            escape_hidden("a\tb\r\nc\u{1b}[2K\u{200b}d 中"),
            "a\tb\\r\\nc\\u{1b}[2K\\u{200b}d 中",
        );
    }
}
