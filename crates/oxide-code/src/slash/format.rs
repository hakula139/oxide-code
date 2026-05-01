//! Shared rendering helpers for the slash-command output blocks.
//!
//! `/help`, `/status`, and `/config` all render a heading followed by
//! a key-value table; this module owns the single implementation of
//! both pieces so the three commands stay visually parallel without
//! re-rolling spacing rules per-call. Keys are byte-aligned because
//! every key in the slash module is ASCII.

use std::fmt::Write as _;

/// Two-space prefix on every row, leaving room for the eventual
/// `▎` left-bar that `SystemMessageBlock` prepends.
const ROW_PREFIX: &str = "  ";

/// Two-space gap between the key column and the value column —
/// wider than one space so the eye sees them as separate columns,
/// narrower than four so the value column doesn't drift right on
/// long keys.
const COL_GAP: &str = "  ";

/// Append a `Heading` line + blank separator + key-value table to
/// `out`. When `out` already has content, prepends a blank line so
/// successive sections sit one blank apart — the shape `/config`
/// uses for its `Resolved config` / `Source files` pair.
pub(super) fn write_kv_section<'a>(
    out: &mut String,
    heading: &str,
    rows: impl IntoIterator<Item = (&'a str, &'a str)> + Clone,
) {
    if !out.is_empty() {
        out.push('\n');
    }
    _ = writeln!(out, "{heading}");
    out.push('\n');
    write_kv_table(out, rows);
}

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

    // ── write_kv_section ──

    #[test]
    fn write_kv_section_first_section_starts_with_heading() {
        let mut out = String::new();
        write_kv_section(&mut out, "Heading", [("k", "v")]);
        assert!(out.starts_with("Heading\n\n"), "{out:?}");
        assert!(out.contains('k'), "{out:?}");
        assert!(out.contains('v'), "{out:?}");
    }

    #[test]
    fn write_kv_section_second_section_inserts_blank_separator() {
        // A `/config`-style two-section render: the second call must
        // leave exactly one blank line between the prior table and
        // the next heading. Pin the byte sequence so a regression
        // that drops or doubles the blank fails here.
        let mut out = String::new();
        write_kv_section(&mut out, "First", [("a", "1")]);
        write_kv_section(&mut out, "Second", [("b", "2")]);
        assert!(
            out.contains("\n\nSecond\n\n"),
            "expected blank-line separator before second heading: {out:?}",
        );
        assert_eq!(out.matches("\n\n").count(), 3, "{out:?}");
    }

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
