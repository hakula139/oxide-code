//! Shared heading + key-value table renderer for `/help` / `/status` / `/config`.

use std::fmt::Write as _;

const ROW_PREFIX: &str = "  ";
const COL_GAP: &str = "  ";

/// Heading + blank line + table. Successive sections are separated by a blank line.
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

/// `key  value` rows aligned to the longest key. Empty input is a no-op.
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
        let value_cols: Vec<usize> = out
            .lines()
            .map(|l| l.find(|c: char| c.is_ascii_digit()).expect("value present"))
            .collect();
        assert!(
            value_cols.windows(2).all(|w| w[0] == w[1]),
            "value columns not aligned: {value_cols:?}",
        );
        assert_eq!(value_cols[0], 10);
    }

    #[test]
    fn write_kv_table_empty_input_writes_nothing() {
        let mut out = String::from("preexisting");
        write_kv_table(&mut out, std::iter::empty::<(&str, &str)>());
        assert_eq!(out, "preexisting");
    }

    #[test]
    fn write_kv_table_single_row_uses_zero_padding() {
        let mut out = String::new();
        write_kv_table(&mut out, [("only", "value")]);
        assert_eq!(out, "  only  value\n");
    }

    #[test]
    fn write_kv_table_each_row_ends_in_newline() {
        let mut out = String::new();
        write_kv_table(&mut out, [("a", "1"), ("b", "2")]);
        assert_eq!(out.matches('\n').count(), 2);
        assert!(out.ends_with('\n'));
    }
}
