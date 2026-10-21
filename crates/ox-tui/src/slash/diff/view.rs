//! [`WorkingTreeDiff`], the typed view `/diff` parses once from git's output for the chat block to
//! draw, kept within a byte budget so a runaway diff can't stall rendering.

use std::fmt;

// ── Constants ──

/// Bytes of git output a view keeps.
const MAX_BYTES: usize = 64 * 1024;

// ── WorkingTreeDiff ──

/// Uncommitted changes in a repository, parsed from `git diff` and `git ls-files` output.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkingTreeDiff {
    pub(crate) rows: Vec<DiffRow>,
    /// Untracked paths, relative to the repository root.
    pub(crate) untracked: Vec<String>,
    /// What the byte budget left out, when the output ran past it.
    pub(crate) truncation: Option<Truncation>,
}

/// One drawn row of a tracked diff.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DiffRow {
    /// A file's path, or `old → new` when its extended header records a rename.
    File(String),
    /// An `@@ -a,b +c,d @@` hunk header, kept as git wrote it.
    Hunk(String),
    Removed {
        number: usize,
        text: String,
    },
    Added {
        number: usize,
        text: String,
    },
    /// A line both sides share, numbered on the new side.
    Context {
        number: usize,
        text: String,
    },
    /// A `\ No newline at end of file` marker, a blank line between hunks, or a hunk line no diff
    /// prefix opens.
    Marker(String),
    /// Any other line outside a hunk, such as a mode change or a binary-file notice.
    Note(String),
}

/// The part of git's output a [`WorkingTreeDiff`] dropped.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Truncation {
    dropped_bytes: usize,
    /// The git commands that print what was cut.
    full_command: &'static str,
}

impl WorkingTreeDiff {
    /// Parses the `tracked` diff sections and the NUL-separated `untracked` paths in order, until
    /// the byte budget runs out. `diff_command` names the git commands that print the full diff,
    /// for a truncation note on a cut inside it.
    pub(crate) fn new(tracked: &[&str], untracked: &str, diff_command: &'static str) -> Self {
        let total = tracked.iter().map(|s| s.len()).sum::<usize>() + untracked.len();
        let mut budget = Budget::default();
        let mut parser = Parser::default();
        for section in tracked {
            for line in section.split_inclusive('\n') {
                if !budget.take(line.len()) {
                    break;
                }
                parser.push(strip_line_ending(line));
            }
            parser.end_file();
        }
        let full_command = if budget.exhausted {
            diff_command
        } else {
            "`git status`"
        };
        let untracked = untracked
            .split_terminator('\0')
            .take_while(|path| budget.take(path.len() + 1))
            .map(str::to_owned)
            .collect();
        let truncation = (budget.used < total).then(|| Truncation {
            dropped_bytes: total - budget.used,
            full_command,
        });
        Self {
            rows: parser.rows,
            untracked,
            truncation,
        }
    }

    /// Returns whether the tree has no change to show.
    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.untracked.is_empty() && self.truncation.is_none()
    }
}

impl fmt::Display for Truncation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "(Truncated {}. Run {} for the full output.)",
            format_size(self.dropped_bytes),
            self.full_command,
        )
    }
}

// ── Budget ──

/// The bytes of [`MAX_BYTES`] spent so far. Once a piece does not fit, every later one is refused,
/// so the view keeps a prefix of the output.
#[derive(Default)]
struct Budget {
    used: usize,
    exhausted: bool,
}

impl Budget {
    /// Spends `len` bytes when they fit. Returns whether they did.
    fn take(&mut self, len: usize) -> bool {
        self.exhausted |= self.used + len > MAX_BYTES;
        if !self.exhausted {
            self.used += len;
        }
        !self.exhausted
    }
}

// ── Diff Parsing ──

/// Turns diff lines into [`DiffRow`]s, tracking the open file and hunk.
#[derive(Default)]
struct Parser {
    rows: Vec<DiffRow>,
    /// Index of the open file's [`DiffRow::File`] row, which a rename header relabels.
    file: Option<usize>,
    rename_from: Option<String>,
    hunk: HunkCursor,
}

impl Parser {
    fn push(&mut self, line: &str) {
        if line.starts_with("diff ") {
            self.end_file();
            self.file = Some(self.rows.len());
            let label = parse_diff_git_path(line).unwrap_or_else(|| line.to_owned());
            self.rows.push(DiffRow::File(label));
        } else if let Some((old, new)) = parse_hunk_header(line) {
            self.hunk = HunkCursor { old, new };
            self.rows.push(DiffRow::Hunk(line.to_owned()));
        } else if !self.hunk.is_open() {
            self.push_outside_hunk(line);
        } else {
            let row = if let Some(text) = line.strip_prefix('-') {
                DiffRow::Removed {
                    number: self.hunk.old.advance(),
                    text: text.to_owned(),
                }
            } else if let Some(text) = line.strip_prefix('+') {
                DiffRow::Added {
                    number: self.hunk.new.advance(),
                    text: text.to_owned(),
                }
            } else if let Some(text) = context_text(line) {
                DiffRow::Context {
                    number: self.hunk.advance_context(),
                    text: text.to_owned(),
                }
            } else {
                DiffRow::Marker(line.to_owned())
            };
            self.rows.push(row);
        }
    }

    /// Reads a line between hunks. Extended headers yield no row because the file row names the
    /// file, though a rename relabels it.
    fn push_outside_hunk(&mut self, line: &str) {
        if let Some(from) = line.strip_prefix("rename from ") {
            self.rename_from = Some(header_path(from));
        } else if let Some(to) = line.strip_prefix("rename to ")
            && let Some(from) = self.rename_from.take()
            && let Some(file) = self.file
        {
            self.rows[file] = DiffRow::File(format!("{from} → {}", header_path(to)));
        } else if !is_file_header(line) {
            let line = line.to_owned();
            let row = if line.is_empty() || line.starts_with('\\') {
                DiffRow::Marker(line)
            } else {
                DiffRow::Note(line)
            };
            self.rows.push(row);
        }
    }

    /// Closes the open file, so the next section starts outside any hunk.
    fn end_file(&mut self) {
        self.file = None;
        self.rename_from = None;
        self.hunk = HunkCursor::default();
    }
}

/// The lines each side of the hunk being parsed has left.
#[derive(Debug, Default)]
struct HunkCursor {
    old: HunkRange,
    new: HunkRange,
}

impl HunkCursor {
    /// Returns whether either side has body lines left, so the next line belongs to the hunk.
    fn is_open(&self) -> bool {
        self.old.count > 0 || self.new.count > 0
    }

    /// Moves both sides past a context line and returns its new-side number.
    fn advance_context(&mut self) -> usize {
        self.old.advance();
        self.new.advance()
    }
}

/// One side of a hunk: its next line number and how many lines it has left.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct HunkRange {
    start: usize,
    count: usize,
}

impl HunkRange {
    /// Returns the line number of the next line and moves past it.
    fn advance(&mut self) -> usize {
        let number = self.start;
        self.start += 1;
        self.count = self.count.saturating_sub(1);
        number
    }
}

/// Drops the `\n` or `\r\n` that ends `line`, as [`str::lines`] does.
fn strip_line_ending(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

/// Returns whether `line` is an extended header between `diff --git` and the first `@@`, which
/// the file row covers.
fn is_file_header(line: &str) -> bool {
    [
        "index ",
        "--- ",
        "+++ ",
        "similarity ",
        "rename ",
        "new file ",
        "deleted file ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

/// The path of a `diff --git a/P b/P` line, whose two halves match unless the file was renamed.
/// Git quotes both halves when the path holds a control character, `"`, or `\`.
fn parse_diff_git_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    if rest.starts_with('"') {
        let (old, rest) = take_quoted(rest)?;
        let (new, rest) = take_quoted(rest.strip_prefix(' ')?)?;
        let path = old.strip_prefix("a/")?;
        return (rest.is_empty() && new.strip_prefix("b/")? == path).then(|| path.to_owned());
    }
    let rest = rest.strip_prefix("a/")?;
    let (path, new_side) = rest.split_at_checked(rest.len().checked_sub(3)? / 2)?;
    (new_side.strip_prefix(" b/")? == path).then(|| path.to_owned())
}

/// The path a `rename from` or `rename to` header names, decoded when git quoted it.
fn header_path(path: &str) -> String {
    match take_quoted(path) {
        Some((decoded, "")) => decoded,
        _ => path.to_owned(),
    }
}

/// Decodes the C-quoted string that opens `s`, as in `"a \"b\""`, and returns it with the text
/// after its closing quote. Octal escapes of non-ASCII bytes decode as UTF-8, while escapes of
/// control characters, such as `\t`, stay as written, since the terminal would not draw them.
fn take_quoted(s: &str) -> Option<(String, &str)> {
    let mut bytes = s.strip_prefix('"')?.bytes().enumerate();
    let mut decoded = Vec::new();
    while let Some((i, byte)) = bytes.next() {
        match byte {
            b'"' => {
                let decoded = String::from_utf8_lossy(&decoded).into_owned();
                return Some((decoded, &s[i + 2..]));
            }
            b'\\' => match bytes.next()?.1 {
                escaped @ (b'"' | b'\\') => decoded.push(escaped),
                digit @ b'0'..=b'3' => {
                    let mut value = digit - b'0';
                    for _ in 0..2 {
                        let next = bytes.next()?.1;
                        if !next.is_ascii_digit() || next > b'7' {
                            return None;
                        }
                        value = value * 8 + (next - b'0');
                    }
                    if value.is_ascii() {
                        decoded.extend(format!("\\{value:03o}").bytes());
                    } else {
                        decoded.push(value);
                    }
                }
                escaped => decoded.extend([b'\\', escaped]),
            },
            byte => decoded.push(byte),
        }
    }
    None
}

/// Parses the old and new ranges of an `@@ -a,b +c,d @@` header.
fn parse_hunk_header(line: &str) -> Option<(HunkRange, HunkRange)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let new = rest.split(' ').next()?;
    Some((parse_hunk_range(old)?, parse_hunk_range(new)?))
}

/// Parses `start[,count]`, where an omitted count means one line.
fn parse_hunk_range(s: &str) -> Option<HunkRange> {
    let (start, count) = match s.split_once(',') {
        Some((start, count)) => (start.parse().ok()?, count.parse().ok()?),
        None => (s.parse().ok()?, 1),
    };
    Some(HunkRange { start, count })
}

/// The text of a context line. Under `diff.suppressBlankEmpty`, git writes a blank context line
/// as an empty line.
fn context_text(line: &str) -> Option<&str> {
    if line.is_empty() {
        Some("")
    } else {
        line.strip_prefix(' ')
    }
}

// ── Helpers ──

/// `"N B"`, `"N.N KiB"`, or `"N.N MiB"`, truncating to one decimal.
fn format_size(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = KIB * 1024;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        let whole = bytes / KIB;
        let tenth = (bytes % KIB) * 10 / KIB;
        format!("{whole}.{tenth} KiB")
    } else {
        let whole = bytes / MIB;
        let tenth = (bytes % MIB) * 10 / MIB;
        format!("{whole}.{tenth} MiB")
    }
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use super::DiffRow;

    pub(crate) fn removed(number: usize, text: &str) -> DiffRow {
        let text = text.to_owned();
        DiffRow::Removed { number, text }
    }

    pub(crate) fn added(number: usize, text: &str) -> DiffRow {
        let text = text.to_owned();
        DiffRow::Added { number, text }
    }

    pub(crate) fn context(number: usize, text: &str) -> DiffRow {
        let text = text.to_owned();
        DiffRow::Context { number, text }
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::testing::{added, context, removed};
    use super::*;

    const HINT: &str = "`git diff HEAD`";

    fn file(label: &str) -> DiffRow {
        DiffRow::File(label.to_owned())
    }

    fn hunk(header: &str) -> DiffRow {
        DiffRow::Hunk(header.to_owned())
    }

    fn rows_of(diff: &str) -> Vec<DiffRow> {
        WorkingTreeDiff::new(&[diff], "", HINT).rows
    }

    mod working_tree_diff {
        use super::*;

        mod new {
            use super::*;

            #[test]
            fn reads_files_hunks_and_numbered_lines() {
                let rows = rows_of(indoc! {"
                    diff --git a/foo.rs b/foo.rs
                    index abc..def 100644
                    --- a/foo.rs
                    +++ b/foo.rs
                    @@ -27,3 +27,3 @@
                    -old line
                    +new line
                     context
                "});

                assert_eq!(
                    rows,
                    [
                        file("foo.rs"),
                        hunk("@@ -27,3 +27,3 @@"),
                        removed(27, "old line"),
                        added(27, "new line"),
                        context(28, "context"),
                    ]
                );
            }

            #[test]
            fn classifies_hunk_lines_by_their_first_character() {
                let rows = rows_of(indoc! {"
                    diff --git a/notes.md b/notes.md
                    @@ -10,4 +10,3 @@
                     intro
                    -- item
                    --- yaml
                    +++ counter
                     outro
                "});

                assert_eq!(
                    rows[2..],
                    [
                        context(10, "intro"),
                        removed(11, "- item"),
                        removed(12, "-- yaml"),
                        added(11, "++ counter"),
                        context(12, "outro"),
                    ]
                );
            }

            #[test]
            fn advances_both_sides_through_context_lines() {
                let rows = rows_of(indoc! {"
                    diff --git a/x b/x
                    @@ -1,3 +1,3 @@
                    +first
                     middle
                    +third
                    -fourth
                "});

                assert_eq!(
                    rows[2..],
                    [
                        added(1, "first"),
                        context(2, "middle"),
                        added(3, "third"),
                        removed(2, "fourth"),
                    ]
                );
            }

            #[test]
            fn reads_an_empty_line_inside_a_hunk_as_a_blank_context_line() {
                let rows = rows_of(indoc! {"
                    diff --git a/x b/x
                    @@ -1,3 +1,3 @@
                     a

                    -c
                    +C
                "});

                assert_eq!(
                    rows[2..],
                    [
                        context(1, "a"),
                        context(2, ""),
                        removed(3, "c"),
                        added(3, "C")
                    ]
                );
            }

            #[test]
            fn ends_hunks_by_count_so_later_headers_stay_headers() {
                let rows = rows_of(indoc! {r"
                    diff --git a/one.rs b/one.rs
                    @@ -1,2 +1 @@
                    -a
                    -b
                    +c
                    \ No newline at end of file
                    diff --git a/two.rs b/two.rs
                    index 3333333..4444444 100644
                    --- a/two.rs
                    +++ b/two.rs
                    @@ -1 +1 @@
                    -x
                    +y
                "});

                assert_eq!(
                    rows,
                    [
                        file("one.rs"),
                        hunk("@@ -1,2 +1 @@"),
                        removed(1, "a"),
                        removed(2, "b"),
                        added(1, "c"),
                        DiffRow::Marker(r"\ No newline at end of file".to_owned()),
                        file("two.rs"),
                        hunk("@@ -1 +1 @@"),
                        removed(1, "x"),
                        added(1, "y"),
                    ]
                );
            }

            #[test]
            fn starts_each_file_outside_a_hunk_even_when_the_last_hunk_ran_short() {
                let rows = rows_of(indoc! {"
                    diff --git a/x b/x
                    @@ -1,3 +1,3 @@
                    -a
                    +b
                    diff --git a/y b/y
                    --- a/y
                    +++ b/y
                "});

                assert_eq!(
                    rows,
                    [
                        file("x"),
                        hunk("@@ -1,3 +1,3 @@"),
                        removed(1, "a"),
                        added(1, "b"),
                        file("y"),
                    ]
                );
            }

            #[test]
            fn starts_each_section_outside_a_hunk() {
                let cached = "@@ -0,0 +1,3 @@\n+a\n";
                let unstaged = "--- a/x\n+++ b/x\n";

                let diff = WorkingTreeDiff::new(&[cached, unstaged], "", HINT);

                assert_eq!(diff.rows, [hunk("@@ -0,0 +1,3 @@"), added(1, "a")]);
            }

            #[test]
            fn marks_a_corrupt_hunk_line_without_taking_a_number() {
                let rows = rows_of(indoc! {"
                    diff --git a/x b/x
                    @@ -1,2 +1,2 @@
                    +real add
                    corrupt-no-marker
                    +second add
                "});

                assert_eq!(
                    rows[2..],
                    [
                        added(1, "real add"),
                        DiffRow::Marker("corrupt-no-marker".to_owned()),
                        added(2, "second add"),
                    ]
                );
            }

            #[test]
            fn names_a_renamed_file_by_both_paths_and_keeps_its_mode_change() {
                let rows = rows_of(indoc! {r#"
                    diff --git a/old.sh b/new.sh
                    old mode 100644
                    new mode 100755
                    similarity index 100%
                    rename from old.sh
                    rename to "new\tname.sh"
                "#});

                assert_eq!(
                    rows,
                    [
                        file(r"old.sh → new\tname.sh"),
                        DiffRow::Note("old mode 100644".to_owned()),
                        DiffRow::Note("new mode 100755".to_owned()),
                    ]
                );
            }

            #[test]
            fn decodes_a_quoted_path() {
                let rows = rows_of(indoc! {r#"
                    diff --git "a/\346\226\207 \"x\".txt" "b/\346\226\207 \"x\".txt"
                    new file mode 100644
                "#});

                assert_eq!(rows, [file(r#"文 "x".txt"#)]);
            }

            #[test]
            fn keeps_a_line_ending_carriage_return_out_of_the_text() {
                let rows = rows_of("@@ -1 +1 @@\r\n-a\r\n+b\r\n");
                assert_eq!(rows[1..], [removed(1, "a"), added(1, "b")]);
            }

            #[test]
            fn splits_untracked_paths_on_nul() {
                let diff = WorkingTreeDiff::new(&[], "new.txt\0dir/a b.rs\0", HINT);
                assert_eq!(diff.untracked, ["new.txt", "dir/a b.rs"]);
                assert_eq!(diff.truncation, None);
            }

            #[test]
            fn keeps_output_that_fills_the_budget_exactly() {
                let header = "@@ -0,0 +1 @@\n";
                let diff = format!("{header}+{}\n", "a".repeat(MAX_BYTES - header.len() - 2));
                assert_eq!(diff.len(), MAX_BYTES);

                let view = WorkingTreeDiff::new(&[&diff], "", HINT);

                assert_eq!(view.truncation, None);
                assert_eq!(view.rows.len(), 2);
            }

            #[test]
            fn cuts_at_a_line_and_drops_every_later_piece() {
                let header = "@@ -0,0 +1,6000 @@\n";
                let line = "+added line\n";
                let diff = format!("{header}{}", line.repeat(6000));
                let (later, untracked) = ("@@ -1 +1 @@\n", "late.txt\0");

                let view = WorkingTreeDiff::new(&[&diff, later], untracked, HINT);

                let kept = (MAX_BYTES - header.len()) / line.len();
                assert_eq!(view.rows.len(), 1 + kept);
                assert_eq!(view.rows.last(), Some(&added(kept, "added line")));
                assert_eq!(view.untracked, Vec::<String>::new());
                let total = diff.len() + later.len() + untracked.len();
                assert_eq!(
                    view.truncation,
                    Some(Truncation {
                        dropped_bytes: total - (header.len() + kept * line.len()),
                        full_command: HINT,
                    })
                );
            }

            #[test]
            fn names_git_status_when_the_cut_lands_in_the_untracked_paths() {
                let path = format!("{}\0", "a".repeat(1023));
                let untracked = path.repeat(65);

                let view = WorkingTreeDiff::new(&["@@ -1 +1 @@\n"], &untracked, HINT);

                assert_eq!(view.untracked.len(), 63);
                assert_eq!(
                    view.truncation,
                    Some(Truncation {
                        dropped_bytes: 2 * 1024,
                        full_command: "`git status`",
                    })
                );
            }
        }

        mod is_empty {
            use super::*;

            #[test]
            fn holds_only_without_rows_paths_or_truncation() {
                assert!(WorkingTreeDiff::new(&["", ""], "", HINT).is_empty());
                assert!(!WorkingTreeDiff::new(&[], "a\0", HINT).is_empty());
                assert!(!WorkingTreeDiff::new(&["@@ -1 +1 @@\n"], "", HINT).is_empty());
            }
        }
    }

    mod truncation {
        use super::*;

        #[test]
        fn display_names_the_dropped_size_and_the_full_command() {
            let truncation = Truncation {
                dropped_bytes: 1536,
                full_command: "`git diff --cached` and `git diff`",
            };
            assert_eq!(
                truncation.to_string(),
                "(Truncated 1.5 KiB. Run `git diff --cached` and `git diff` for the full output.)"
            );
        }
    }

    mod parse_diff_git_path {
        use super::*;

        #[test]
        fn extracts_a_path() {
            assert_eq!(
                parse_diff_git_path("diff --git a/src/main.rs b/src/main.rs").as_deref(),
                Some("src/main.rs"),
            );
        }

        #[test]
        fn keeps_a_space_and_b_slash_inside_the_path() {
            assert_eq!(
                parse_diff_git_path("diff --git a/x b/y.rs b/x b/y.rs").as_deref(),
                Some("x b/y.rs"),
            );
        }

        #[test]
        fn decodes_quoted_halves() {
            assert_eq!(
                parse_diff_git_path(r#"diff --git "a/tab\there" "b/tab\there""#).as_deref(),
                Some(r"tab\there"),
            );
        }

        #[test]
        fn rejects_a_line_without_one_path_on_both_sides() {
            assert_eq!(parse_diff_git_path("@@ -1 +1 @@"), None);
            assert_eq!(parse_diff_git_path("diff --git a/old.rs b/new.rs"), None);
            assert_eq!(parse_diff_git_path(r#"diff --git "a/x" "b/y""#), None);
            assert_eq!(parse_diff_git_path(r#"diff --git "a/x" "b/x" tail"#), None);
            assert_eq!(parse_diff_git_path("diff --cc file.rs"), None);
        }
    }

    mod take_quoted {
        use super::*;

        #[test]
        fn decodes_quotes_and_backslashes_and_keeps_control_escapes() {
            assert_eq!(
                take_quoted(r#""\a\t\"\\\001\177" rest"#),
                Some((r#"\a\t"\\001\177"#.to_owned(), " rest")),
            );
        }

        #[test]
        fn decodes_octal_bytes_as_utf8() {
            let replacement = char::REPLACEMENT_CHARACTER;
            assert_eq!(
                take_quoted(r#""\346\226\207\303""#),
                Some((format!("文{replacement}"), "")),
            );
        }

        #[test]
        fn rejects_unquoted_unterminated_or_malformed_text() {
            assert_eq!(take_quoted("plain"), None);
            assert_eq!(take_quoted(r#""open"#), None);
            assert_eq!(take_quoted(r#""\38""#), None);
            assert_eq!(take_quoted(r#""\3"#), None);
        }
    }

    mod parse_hunk_header {
        use super::*;

        fn range(start: usize, count: usize) -> HunkRange {
            HunkRange { start, count }
        }

        #[test]
        fn with_counts() {
            assert_eq!(
                parse_hunk_header("@@ -27,20 +27,20 @@"),
                Some((range(27, 20), range(27, 20))),
            );
            assert_eq!(
                parse_hunk_header("@@ -10,5 +12,0 @@ fn ctx()"),
                Some((range(10, 5), range(12, 0))),
            );
        }

        #[test]
        fn omitted_count_spans_one_line() {
            assert_eq!(
                parse_hunk_header("@@ -42 +43 @@"),
                Some((range(42, 1), range(43, 1))),
            );
        }

        #[test]
        fn rejects_lines_that_are_not_hunk_headers() {
            assert_eq!(parse_hunk_header("+ added line"), None);
            assert_eq!(parse_hunk_header("@@ -x +1 @@"), None);
            assert_eq!(parse_hunk_header(""), None);
        }
    }

    mod format_size {
        use super::*;

        #[test]
        fn picks_the_binary_unit_and_truncates_to_one_decimal() {
            assert_eq!(format_size(0), "0 B");
            assert_eq!(format_size(1023), "1023 B");
            assert_eq!(format_size(1024), "1.0 KiB");
            assert_eq!(format_size(1536), "1.5 KiB");
            assert_eq!(format_size(1024 * 1024 - 1), "1023.9 KiB");
            let mib = 1024 * 1024;
            assert_eq!(format_size(mib), "1.0 MiB");
            assert_eq!(format_size(mib * 3 / 2), "1.5 MiB");
        }
    }
}
