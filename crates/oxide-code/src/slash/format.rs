//! Shared rendering helpers for the slash-command output blocks.
//!
//! Every read-only command (`/help`, `/status`, `/config`) prints a
//! key-value table aligned to a shared gutter — this module owns the
//! single implementation. Keys are byte-aligned; that's correct today
//! because every key in the slash module is ASCII. If a future key
//! lands with multi-width chars, switch to `unicode_width::UnicodeWidthStr`.

use std::fmt::Write as _;

/// Two-space prefix on every row, leaving room for the eventual
/// `▎` left-bar that `SystemMessageBlock` prepends.
const ROW_PREFIX: &str = "  ";

/// Two-space gap between the key column and the value column —
/// wider than one space so the eye sees them as separate columns,
/// narrower than four so the value column doesn't drift right on
/// long keys.
const COL_GAP: &str = "  ";

/// Append a `key  value` table to `out`, aligning every value
/// column to a gutter wide enough for the longest key. Empty input
/// is a no-op so callers can branch on whether to emit a heading.
pub(super) fn write_kv_table<'a>(
    out: &mut String,
    rows: impl IntoIterator<Item = (&'a str, &'a str)> + Clone,
) {
    let gutter = rows.clone().into_iter().map(|(k, _)| k.len()).max();
    let Some(gutter) = gutter else {
        return;
    };
    for (key, value) in rows {
        let pad = gutter.saturating_sub(key.len());
        _ = writeln!(
            out,
            "{ROW_PREFIX}{key}{spaces}{COL_GAP}{value}",
            spaces = " ".repeat(pad),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── write_kv_table ──

    #[test]
    fn write_kv_table_aligns_value_column_to_longest_key() {
        let mut out = String::new();
        write_kv_table(&mut out, [("a", "1"), ("longer", "2"), ("mid", "3")]);
        // Each row's value column starts at the same byte offset —
        // assert that offset directly so a regression that loses the
        // padding fails here, not silently in a higher-level
        // snapshot.
        let value_cols: Vec<usize> = out
            .lines()
            .map(|l| l.find(|c: char| c.is_ascii_digit()).expect("value present"))
            .collect();
        assert!(
            value_cols.windows(2).all(|w| w[0] == w[1]),
            "value columns not aligned: {value_cols:?}",
        );
        // The shared offset is `prefix(2) + longest_key(6) + gap(2) = 10`.
        assert_eq!(value_cols[0], 10);
    }

    #[test]
    fn write_kv_table_empty_input_writes_nothing() {
        // Empty `rows` short-circuits before any allocation. Pin the
        // contract so a future "always emit a header row" tweak
        // can't regress callers that branch on `out.is_empty()`.
        let mut out = String::from("preexisting");
        write_kv_table(&mut out, std::iter::empty::<(&str, &str)>());
        assert_eq!(out, "preexisting");
    }

    #[test]
    fn write_kv_table_single_row_uses_zero_padding() {
        // `gutter == key.len()` ⇒ no padding spaces. Pin so a
        // refactor that always pads at least one space fails here.
        let mut out = String::new();
        write_kv_table(&mut out, [("only", "value")]);
        assert_eq!(out, "  only  value\n");
    }

    #[test]
    fn write_kv_table_each_row_ends_in_newline() {
        // `writeln!` adds a trailing `\n`; verify the contract so a
        // switch to `write!` would fail visibly here.
        let mut out = String::new();
        write_kv_table(&mut out, [("a", "1"), ("b", "2")]);
        assert_eq!(out.matches('\n').count(), 2);
        assert!(out.ends_with('\n'));
    }
}
