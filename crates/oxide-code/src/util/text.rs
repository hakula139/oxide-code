//! Text utilities — display-width-aware operations.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) const ELLIPSIS: &str = "...";

pub(crate) const ELLIPSIS_WIDTH: usize = 3;

/// Truncates `s` to `max_width` display columns, appending [`ELLIPSIS`] when shortened.
pub(crate) fn truncate_to_width(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_owned();
    }
    let (budget, tail) = if max_width >= ELLIPSIS_WIDTH {
        (max_width - ELLIPSIS_WIDTH, ELLIPSIS)
    } else {
        (max_width, "")
    };
    let mut out = String::with_capacity(s.len());
    let mut used = 0;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push_str(tail);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // 测试文本 = 4 chars × 2 cols = 8 cols; budget = 5 − 3 (ellipsis) = 2 cols → one char fits.
        assert_eq!(truncate_to_width("测试文本", 5), "测...");
    }

    #[test]
    fn truncate_to_width_zero_produces_empty() {
        assert_eq!(truncate_to_width("anything", 0), "");
    }

    #[test]
    fn truncate_to_width_drops_ellipsis_when_budget_below_ellipsis_width() {
        // Drop the marker — emitting "..." into a 1- or 2-col slot would overflow the budget.
        assert_eq!(truncate_to_width("abc", 1), "a");
        assert_eq!(truncate_to_width("abc", 2), "ab");
    }
}
