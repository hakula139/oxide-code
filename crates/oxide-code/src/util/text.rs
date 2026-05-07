//! Text utilities — display-width-aware operations.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) const ELLIPSIS: &str = "...";

pub(crate) const ELLIPSIS_WIDTH: usize = 3;

/// Truncates `s` to `max_width` display columns, appending [`ELLIPSIS`] when shortened.
///
/// Width is measured in terminal columns (CJK and emoji = 2, zero-width = 0), not bytes or
/// `char` count. The ellipsis is dropped when `max_width < ELLIPSIS_WIDTH` because emitting it
/// would itself overflow the budget.
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

fn take_head(s: &str, budget: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

fn take_tail(s: &str, budget: usize) -> String {
    let mut chars: Vec<char> = Vec::new();
    let mut used = 0;
    for ch in s.chars().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        chars.push(ch);
        used += w;
    }
    chars.into_iter().rev().collect()
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
        // Below the ellipsis floor center-truncate would underflow the budget split; the
        // right-truncate fallback returns whatever fits.
        assert_eq!(center_truncate_to_width("abcdef", 2), "ab");
        assert_eq!(center_truncate_to_width("abcdef", 1), "a");
    }

    #[test]
    fn center_truncate_to_width_accounts_for_cjk_double_width() {
        // 测试文本编辑 = 6 chars × 2 cols = 12 cols; budget 9 = 3 head + 3 tail around `...`. Only
        // one CJK char fits each side at 2 cols.
        assert_eq!(center_truncate_to_width("测试文本编辑", 9), "测...辑");
    }

    #[test]
    fn center_truncate_to_width_falls_back_to_right_truncate_when_cjk_overflows_both_halves() {
        // 中文测试 = 8 cols; max=5 → budget=2, half=1 — neither side fits a 2-col char. Centering
        // would yield bare "..." (3 cols of 5), so fall back to right-truncate ("中...", 5 cols).
        assert_eq!(center_truncate_to_width("中文测试", 5), "中...");
    }
}
