//! The structured shapes frontends render for a tool call: result views and approval previews.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use super::tasks::Task;

// ── Tool Result View ──

/// Per-tool structured shape of a completed tool call's body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ToolResultView {
    Text {
        content: String,
    },
    ReadExcerpt {
        path: String,
        lines: Vec<NumberedLine>,
        total_lines: usize,
    },
    Diff(EditDiff),
    GrepMatches {
        groups: Vec<GrepFileGroup>,
        truncated: bool,
    },
    GlobFiles {
        pattern: String,
        files: Vec<String>,
        total: usize,
    },
    /// The task list a `tasks` call sent, before a finished list clears.
    Tasks {
        tasks: Vec<Task>,
    },
}

/// One line of a file with its 1-based line number, as a read excerpt shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NumberedLine {
    pub number: usize,
    pub text: String,
}

/// A line diff as hunks in file order, each holding its changed lines with up to three unchanged
/// lines around them. No two hunks are adjacent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub hunks: Vec<DiffHunk>,
    /// The lines past the row cap that the hunks leave out, which bounds the diff of a large
    /// rewrite. The last hunk may stop partway.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_lines: usize,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl Diff {
    /// The largest line number on either side, which sets the width of the number column.
    pub fn max_number(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| line.number)
            .max()
            .unwrap_or(0)
    }

    /// Every line in order, with a [`DiffRow::Gap`] between two hunks and a closing
    /// [`DiffRow::Omitted`] when the cap left lines out.
    pub fn rows(&self) -> impl Iterator<Item = DiffRow<'_>> {
        let lines = self.hunks.iter().enumerate().flat_map(|(index, hunk)| {
            (index > 0)
                .then_some(DiffRow::Gap)
                .into_iter()
                .chain(hunk.lines.iter().map(DiffRow::Line))
        });
        let omitted = (self.omitted_lines > 0).then_some(DiffRow::Omitted(self.omitted_lines));
        lines.chain(omitted)
    }
}

/// One row of a [`Diff`] as [`Diff::rows`] lays it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRow<'a> {
    Line(&'a DiffLine),
    /// The break between two hunks, standing for the unchanged lines they skip.
    Gap,
    /// The count of lines the cap left out.
    Omitted(usize),
}

/// One run of changed lines and their context, in the order a unified diff shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub lines: Vec<DiffLine>,
}

/// One row of a [`DiffHunk`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// The 1-based line number, in the old text for a removed line and in the new text otherwise.
    pub number: usize,
    pub text: String,
    /// Byte ranges of `text` that differ from the line it pairs with on the other side, ascending
    /// and disjoint. Empty for a context line and for a line that shares no text beyond whitespace
    /// with a counterpart.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<Range<usize>>,
}

impl DiffLine {
    /// `text` as consecutive runs, each paired with whether it falls in `changed`. A range out of
    /// order or off a character boundary, as a corrupt saved session could hold, stays unmarked.
    pub fn segments(&self) -> Vec<(&str, bool)> {
        let mut segments = Vec::new();
        let mut at = 0;
        for range in &self.changed {
            let (Some(before), Some(changed)) =
                (self.text.get(at..range.start), self.text.get(range.clone()))
            else {
                continue;
            };
            if !before.is_empty() {
                segments.push((before, false));
            }
            segments.push((changed, true));
            at = range.end;
        }
        if at < self.text.len() || segments.is_empty() {
            segments.push((&self.text[at..], false));
        }
        segments
    }
}

/// Which side of a diff a [`DiffLine`] belongs to, which picks the text its `number` counts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Removed,
    Added,
}

/// What an edit changed: its diff, and the line each replacement applied at, which is its first
/// removed line, or for a pure insertion its first added line. A replacement whose change the diff
/// places past it applies at the first line it writes, or at its first old line if it writes none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditDiff {
    #[serde(flatten)]
    pub diff: Diff,
    pub locations: Vec<usize>,
}

/// One file's match block in a grep result view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepFileGroup {
    pub path: String,
    pub lines: Vec<GrepMatchLine>,
}

/// One row in a [`GrepFileGroup`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatchLine {
    pub number: usize,
    pub text: String,
    pub is_match: bool,
}

// ── Approval Preview ──

/// What a pending approval shows the user, rendered by the tool so the modal only displays it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPreview {
    /// Tool display label, such as `Bash` or `Write(src/main.rs)`.
    pub title: String,
    pub body: ApprovalBody,
}

/// The renderable body of an [`ApprovalPreview`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalBody {
    /// The call's label, which for `bash` is the whole command and may span lines.
    Command(String),
    /// The change an `edit` or `write` would make, numbered by its place in the file when the
    /// preview could read the file, and otherwise from line 1 of the call's own text.
    Diff(Diff),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testing::diff_line;

    mod segments {
        use super::*;

        #[test]
        fn splits_the_text_at_each_valid_changed_range() {
            for (case, changed, expected) in [
                ("unchanged", vec![], vec![("let é = 1;", false)]),
                (
                    "inner",
                    vec![Range { start: 9, end: 10 }],
                    vec![("let é = ", false), ("1", true), (";", false)],
                ),
                (
                    "at both ends",
                    vec![Range { start: 0, end: 3 }, Range { start: 10, end: 11 }],
                    vec![("let", true), (" é = 1", false), (";", true)],
                ),
                (
                    "off a character boundary",
                    vec![Range { start: 5, end: 6 }],
                    vec![("let é = 1;", false)],
                ),
                (
                    "out of order",
                    vec![Range { start: 9, end: 10 }, Range { start: 0, end: 3 }],
                    vec![("let é = ", false), ("1", true), (";", false)],
                ),
            ] {
                let line = DiffLine {
                    changed,
                    ..diff_line(DiffLineKind::Added, 1, "let é = 1;")
                };

                assert_eq!(line.segments(), expected, "{case}");
            }
        }
    }

    mod diff {
        use super::*;

        fn two_hunks() -> Diff {
            Diff {
                hunks: vec![
                    DiffHunk {
                        lines: vec![
                            diff_line(DiffLineKind::Removed, 12, "a"),
                            diff_line(DiffLineKind::Added, 9, "b"),
                        ],
                    },
                    DiffHunk {
                        lines: vec![diff_line(DiffLineKind::Context, 10, "c")],
                    },
                ],
                omitted_lines: 0,
            }
        }

        #[test]
        fn max_number_takes_the_largest_on_either_side() {
            assert_eq!(two_hunks().max_number(), 12);
            assert_eq!(Diff::default().max_number(), 0);
        }

        #[test]
        fn rows_put_a_gap_between_hunks_only() {
            let diff = two_hunks();

            let rows: Vec<_> = diff.rows().collect();

            let lines: Vec<_> = diff.hunks.iter().flat_map(|hunk| &hunk.lines).collect();
            assert_eq!(
                rows,
                [
                    DiffRow::Line(lines[0]),
                    DiffRow::Line(lines[1]),
                    DiffRow::Gap,
                    DiffRow::Line(lines[2]),
                ],
            );
        }
    }
}
