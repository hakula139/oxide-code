//! Line diffs that `edit` and `write` show: hunks with context and the changed spans in each line.

use std::borrow::Cow;
use std::ops::Range;
use std::time::{Duration, Instant};

use similar::{ChangeTag, DiffOp, DiffTag, InlineChange, TextDiff};

use super::view::{Diff, DiffHunk, DiffLine, DiffLineKind};

/// Unchanged lines kept on each side of a change.
const CONTEXT_LINES: usize = 3;

/// Rows one diff keeps across its hunks, which bounds what an approval shows and what a session
/// stores for an edit that rewrites most of a large file.
const MAX_DIFF_LINES: usize = 2000;

/// How long one diff may search for the smallest edit, across its line and word passes, before
/// settling for a coarser one, which bounds a large rewrite of a large file.
const SEARCH_TIMEOUT: Duration = Duration::from_millis(500);

/// One change of a diff: the 0-based line ranges it removes from the old text and adds to the new.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Change {
    pub(super) old: Range<usize>,
    pub(super) new: Range<usize>,
}

impl Change {
    /// The 1-based line the change applies at: the first old line it removes, or else the first
    /// new line it adds.
    pub(super) fn location(&self) -> usize {
        if self.old.is_empty() {
            self.new.start + 1
        } else {
            self.old.start + 1
        }
    }
}

/// The diff of replacing `old` with `new`, numbered from line 1 on each side.
pub(super) fn diff(old: &str, new: &str) -> Diff {
    diff_with_changes(old, new).0
}

/// [`diff`] with the changes it found, in order. Lines compare without their endings. When only
/// endings differ, they compare with them, and each line shows its ending as `␍` and `↵`, so the
/// change still shows.
pub(super) fn diff_with_changes(old: &str, new: &str) -> (Diff, Vec<Change>) {
    let deadline = Instant::now() + SEARCH_TIMEOUT;
    let diffed = line_diff(old, new, Endings::Ignored, deadline);
    if diffed.1.is_empty() && old != new {
        return line_diff(old, new, Endings::Shown, deadline);
    }
    diffed
}

/// Whether lines compare with their endings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endings {
    /// Each line compares as its text and `\n`, which no row shows.
    Ignored,
    /// Each line compares as written, and its row shows the ending.
    Shown,
}

fn line_diff(old: &str, new: &str, endings: Endings, deadline: Instant) -> (Diff, Vec<Change>) {
    let (old, new) = (split_lines(old, endings), split_lines(new, endings));
    let (old, new): (Vec<&str>, Vec<&str>) = (
        old.iter().map(AsRef::as_ref).collect(),
        new.iter().map(AsRef::as_ref).collect(),
    );
    let lines = TextDiff::configure()
        .deadline(deadline)
        .diff_slices(&old, &new);
    let mut diff = Diff::default();
    let mut left = MAX_DIFF_LINES;
    for ops in lines.grouped_ops(CONTEXT_LINES) {
        let rows: usize = ops.iter().map(op_rows).sum();
        let kept = rows.min(left);
        if kept > 0 {
            diff.hunks.push(DiffHunk {
                lines: ops
                    .iter()
                    .flat_map(|op| lines.iter_inline_changes_deadline(op, Some(deadline)))
                    .take(kept)
                    .map(|change| diff_line(&change, endings))
                    .collect(),
            });
        }
        left -= kept;
        diff.omitted_lines += rows - kept;
    }
    let changes = lines
        .ops()
        .iter()
        .filter(|op| op.tag() != DiffTag::Equal)
        .map(|op| Change {
            old: op.old_range(),
            new: op.new_range(),
        })
        .collect();
    (diff, changes)
}

/// The rows `op` adds to a hunk: one per unchanged line, or one per line it removes or adds.
fn op_rows(op: &DiffOp) -> usize {
    match op.tag() {
        DiffTag::Equal => op.old_range().len(),
        DiffTag::Delete | DiffTag::Insert | DiffTag::Replace => {
            op.old_range().len() + op.new_range().len()
        }
    }
}

/// The lines of `text`, each ending in a line break unless `endings` keeps a final line without
/// one. A token-less line would drop out of a word diff, so even a blank line keeps a token.
fn split_lines(text: &str, endings: Endings) -> Vec<Cow<'_, str>> {
    match endings {
        Endings::Ignored => text
            .lines()
            .map(|line| Cow::Owned(format!("{line}\n")))
            .collect(),
        Endings::Shown => text.split_inclusive('\n').map(Cow::Borrowed).collect(),
    }
}

/// One line of a diff, its changed spans merged, and cleared when the line shares nothing but
/// whitespace with its counterpart.
fn diff_line(change: &InlineChange<'_, str>, endings: Endings) -> DiffLine {
    let (kind, index) = match change.tag() {
        ChangeTag::Equal => (DiffLineKind::Context, change.new_index()),
        ChangeTag::Delete => (DiffLineKind::Removed, change.old_index()),
        ChangeTag::Insert => (DiffLineKind::Added, change.new_index()),
    };
    let mut text = String::new();
    let mut changed: Vec<Range<usize>> = Vec::new();
    let mut shares_text = false;
    for (emphasized, value) in change.iter_strings_lossy() {
        let value = match endings {
            Endings::Ignored => value.replace('\n', ""),
            Endings::Shown => value.replace('\r', "␍").replace('\n', "↵"),
        };
        if value.is_empty() {
            continue;
        }
        let start = text.len();
        text.push_str(&value);
        if !emphasized {
            shares_text |= !value.trim().is_empty();
            continue;
        }
        match changed.last_mut() {
            Some(last) if last.end == start => last.end = text.len(),
            _ => changed.push(start..text.len()),
        }
    }
    if !shares_text {
        changed.clear();
    }
    DiffLine {
        kind,
        number: index.expect("each change sits on its own side") + 1,
        text,
        changed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testing::diff_line;

    fn kinds_and_texts(hunk: &DiffHunk) -> Vec<(DiffLineKind, usize, &str)> {
        hunk.lines
            .iter()
            .map(|line| (line.kind, line.number, line.text.as_str()))
            .collect()
    }

    mod diff {
        use super::*;

        #[test]
        fn keeps_three_lines_of_context_around_a_change() {
            let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
            let new = "1\n2\n3\n4\nfive\n6\n7\n8\n9\n";

            let diff = diff(old, new);

            assert_eq!(
                diff.hunks,
                [DiffHunk {
                    lines: vec![
                        diff_line(DiffLineKind::Context, 2, "2"),
                        diff_line(DiffLineKind::Context, 3, "3"),
                        diff_line(DiffLineKind::Context, 4, "4"),
                        diff_line(DiffLineKind::Removed, 5, "5"),
                        diff_line(DiffLineKind::Added, 5, "five"),
                        diff_line(DiffLineKind::Context, 6, "6"),
                        diff_line(DiffLineKind::Context, 7, "7"),
                        diff_line(DiffLineKind::Context, 8, "8"),
                    ],
                }],
            );
        }

        #[test]
        fn splits_changes_further_apart_than_the_context_into_hunks() {
            let old = (1..=20)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            let new = old
                .replacen("2\n", "two\n", 1)
                .replacen("19\n", "nineteen\n", 1);

            let diff = diff(&old, &new);

            assert_eq!(diff.hunks.len(), 2, "{diff:?}");
            assert_eq!(
                kinds_and_texts(&diff.hunks[0])[..3],
                [
                    (DiffLineKind::Context, 1, "1"),
                    (DiffLineKind::Removed, 2, "2"),
                    (DiffLineKind::Added, 2, "two"),
                ],
            );
            assert_eq!(
                kinds_and_texts(&diff.hunks[1])[3..],
                [
                    (DiffLineKind::Removed, 19, "19"),
                    (DiffLineKind::Added, 19, "nineteen"),
                    (DiffLineKind::Context, 20, "20"),
                ],
            );
        }

        #[test]
        fn marks_the_changed_word_of_a_paired_line() {
            let diff = diff("let total = count + 1;\n", "let total = count + 2;\n");

            let lines = &diff.hunks[0].lines;
            assert_eq!(lines[0].changed, [Range { start: 20, end: 21 }]);
            assert_eq!(lines[1].changed, [Range { start: 20, end: 21 }]);
            assert_eq!(&lines[1].text[20..21], "2");
        }

        #[test]
        fn leaves_lines_sharing_no_word_unmarked() {
            let diff = diff(
                "let total = count + 1;\nfoo\n",
                "let total = count + 2;\nbar\n",
            );

            let changed: Vec<_> = diff.hunks[0]
                .lines
                .iter()
                .map(|line| (line.text.as_str(), line.changed.clone()))
                .collect();
            assert_eq!(
                changed,
                [
                    ("let total = count + 1;", vec![Range { start: 20, end: 21 }]),
                    ("foo", vec![]),
                    ("let total = count + 2;", vec![Range { start: 20, end: 21 }]),
                    ("bar", vec![]),
                ],
            );
        }

        #[test]
        fn keeps_a_blank_line_changed_beside_another_line() {
            for (old, new, expected) in [
                (
                    "foo(1)\nbar()\n",
                    "foo(2)\n\nbar()\n",
                    vec![
                        (DiffLineKind::Removed, 1, "foo(1)"),
                        (DiffLineKind::Added, 1, "foo(2)"),
                        (DiffLineKind::Added, 2, ""),
                        (DiffLineKind::Context, 3, "bar()"),
                    ],
                ),
                (
                    "foo(1)\n\nbar()\n",
                    "foo(2)\nbar()\n",
                    vec![
                        (DiffLineKind::Removed, 1, "foo(1)"),
                        (DiffLineKind::Removed, 2, ""),
                        (DiffLineKind::Added, 1, "foo(2)"),
                        (DiffLineKind::Context, 2, "bar()"),
                    ],
                ),
            ] {
                let diff = diff(old, new);

                assert_eq!(
                    kinds_and_texts(&diff.hunks[0]),
                    expected,
                    "{old:?} -> {new:?}"
                );
            }
        }

        #[test]
        fn compares_lines_without_their_endings() {
            for (old, new, expected) in [
                (
                    "a\r\nb\r\n",
                    "a\r\nc",
                    vec![
                        (DiffLineKind::Context, 1, "a"),
                        (DiffLineKind::Removed, 2, "b"),
                        (DiffLineKind::Added, 2, "c"),
                    ],
                ),
                (
                    "fn foo()",
                    "fn foo()\n    return 42;",
                    vec![
                        (DiffLineKind::Context, 1, "fn foo()"),
                        (DiffLineKind::Added, 2, "    return 42;"),
                    ],
                ),
            ] {
                let diff = diff(old, new);

                assert_eq!(
                    kinds_and_texts(&diff.hunks[0]),
                    expected,
                    "{old:?} -> {new:?}"
                );
            }
        }

        #[test]
        fn caps_the_rows_and_counts_the_rest() {
            // Each changed region has a short old side, so the line diff finishes well inside its
            // deadline and takes the same shape on a loaded machine.
            let added = |count: usize| {
                (0..count)
                    .map(|n| format!("new {n}"))
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n"
            };
            let same = "same\n".repeat(10);

            for (case, old, new, omitted, last_kept) in [
                (
                    "inside one hunk",
                    String::new(),
                    added(MAX_DIFF_LINES + 5),
                    5,
                    MAX_DIFF_LINES - 1,
                ),
                (
                    "a later hunk",
                    format!("x\n{same}y\n"),
                    format!("{}{same}", added(MAX_DIFF_LINES)),
                    1 + MAX_DIFF_LINES + 3 + 3 + 1 - MAX_DIFF_LINES,
                    MAX_DIFF_LINES - 2,
                ),
            ] {
                let diff = diff(&old, &new);

                let lens: Vec<_> = diff.hunks.iter().map(|hunk| hunk.lines.len()).collect();
                assert_eq!(lens, [MAX_DIFF_LINES], "{case}");
                assert_eq!(diff.omitted_lines, omitted, "{case}");
                assert_eq!(
                    diff.hunks[0].lines[MAX_DIFF_LINES - 1].text,
                    format!("new {last_kept}"),
                    "{case}"
                );
            }
        }

        #[test]
        fn has_no_hunks_for_the_same_text() {
            assert_eq!(diff("fn main() {}\n", "fn main() {}\n"), Diff::default());
        }

        #[test]
        fn shows_the_endings_when_only_they_differ() {
            for (old, new, expected) in [
                (
                    "a\r\nb\r\nc\r\n",
                    "a\r\nb\nc\r\n",
                    vec![
                        (DiffLineKind::Context, 1, "a␍↵"),
                        (DiffLineKind::Removed, 2, "b␍↵"),
                        (DiffLineKind::Added, 2, "b↵"),
                        (DiffLineKind::Context, 3, "c␍↵"),
                    ],
                ),
                (
                    "a\nb",
                    "a\nb\n",
                    vec![
                        (DiffLineKind::Context, 1, "a↵"),
                        (DiffLineKind::Removed, 2, "b"),
                        (DiffLineKind::Added, 2, "b↵"),
                    ],
                ),
            ] {
                let diff = diff(old, new);

                assert_eq!(
                    kinds_and_texts(&diff.hunks[0]),
                    expected,
                    "{old:?} -> {new:?}"
                );
            }
        }
    }

    mod diff_with_changes {
        use super::*;

        #[test]
        fn lists_each_change_by_its_line_ranges() {
            let (_, changes) = diff_with_changes("a\nb\nc\nd\n", "a\nB\nc\nd\ne\n");

            assert_eq!(
                changes,
                [
                    Change {
                        old: 1..2,
                        new: 1..2
                    },
                    Change {
                        old: 4..4,
                        new: 4..5
                    },
                ],
            );
        }
    }
}
