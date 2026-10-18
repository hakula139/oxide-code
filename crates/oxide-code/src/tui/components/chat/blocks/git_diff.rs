//! `/diff` output block, drawn like an Edit-tool diff with a line-number gutter. It parses the
//! raw text lazily on `render`.

use std::borrow::Cow;

use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use super::rows::{NumberedRows, body_row};
use super::{ChatBlock, RenderCtx};
use crate::slash::{TRUNCATION_FOOTER, UNTRACKED_HEADING};

// ── GitDiffBlock ──

/// Output of `/diff`, kept as raw `git diff` text.
pub(crate) struct GitDiffBlock {
    text: String,
}

impl GitDiffBlock {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl ChatBlock for GitDiffBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let border = ctx.theme.tool_border();
        let (diff, footer) = split_truncation_footer(&self.text);
        let max_number = max_line_number(diff);
        let context = NumberedRows::new(ctx, border, max_number);
        let removed = NumberedRows::removed(ctx, border, max_number);
        let added = NumberedRows::added(ctx, border, max_number);

        let lines: Vec<&str> = diff.lines().collect();
        let mut out = Vec::new();
        let mut hunk = HunkCursor::default();
        for (i, &line) in lines.iter().enumerate() {
            if line.starts_with("diff ") {
                hunk = HunkCursor::default();
                let label = file_label(line, &lines[i + 1..]);
                let style = ctx.theme.text().add_modifier(Modifier::BOLD);
                body_row(&mut out, ctx, border, label, style);
            } else if let Some((old, new)) = parse_hunk_header(line) {
                hunk = HunkCursor { old, new };
                body_row(&mut out, ctx, border, line, ctx.theme.dim());
            } else if !hunk.is_open() {
                push_outside_hunk(&mut out, ctx, border, line);
            } else if let Some(text) = line.strip_prefix('+') {
                added.render(&mut out, hunk.new.advance(), text, ctx.theme.text());
            } else if let Some(text) = line.strip_prefix('-') {
                removed.render(&mut out, hunk.old.advance(), text, ctx.theme.text());
            } else if let Some(text) = context_text(line) {
                context.render(&mut out, hunk.advance_context(), text, ctx.theme.dim());
            } else {
                body_row(&mut out, ctx, border, line, ctx.theme.dim());
            }
        }
        if let Some(footer) = footer {
            body_row(&mut out, ctx, border, "", ctx.theme.dim());
            body_row(&mut out, ctx, border, footer, ctx.theme.dim());
        }
        out
    }
}

/// Draws a line between hunks: an untracked heading or path, a blank separator, or a no-newline
/// marker. Extended file headers are skipped because the path row names the file.
fn push_outside_hunk(out: &mut Vec<Line<'static>>, ctx: &RenderCtx<'_>, border: Style, line: &str) {
    if is_file_header(line) {
        return;
    }
    let style = if line.is_empty() || line == UNTRACKED_HEADING || line.starts_with('\\') {
        ctx.theme.dim()
    } else {
        ctx.theme.text()
    };
    body_row(out, ctx, border, line, style);
}

// ── HunkCursor ──

/// The lines each side of the hunk being drawn has left.
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

// ── Diff Parsing ──

/// Splits the footer [`TRUNCATION_FOOTER`] opens off the diff above it.
fn split_truncation_footer(text: &str) -> (&str, Option<&str>) {
    match text.rfind(TRUNCATION_FOOTER) {
        Some(at) if text[..at].ends_with('\n') => {
            (text[..at].trim_end_matches('\n'), Some(&text[at..]))
        }
        _ => (text, None),
    }
}

/// The file a `diff --git` line names, or `old → new` when its extended header records a rename.
fn file_label<'a>(line: &'a str, rest: &[&'a str]) -> Cow<'a, str> {
    let header_len = rest
        .iter()
        .position(|line| !is_file_header(line))
        .unwrap_or(rest.len());
    let header = &rest[..header_len];
    let renamed = |prefix: &str| header.iter().find_map(|line| line.strip_prefix(prefix));
    match (renamed("rename from "), renamed("rename to ")) {
        (Some(from), Some(to)) => Cow::Owned(format!("{from} → {to}")),
        _ => Cow::Borrowed(parse_diff_git_path(line).unwrap_or(line)),
    }
}

/// Returns whether `line` is an extended header between `diff --git` and the first `@@`, which
/// the path row covers.
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
fn parse_diff_git_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("diff --git a/")?;
    let (path, new_side) = rest.split_at_checked(rest.len().checked_sub(3)? / 2)?;
    (new_side.strip_prefix(" b/")? == path).then_some(path)
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

/// One side of a hunk: its next line number and how many lines it has left.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct HunkRange {
    start: usize,
    count: usize,
}

impl HunkRange {
    /// Last line number in the range, or `start` when the range is empty.
    fn last(self) -> usize {
        self.start + self.count.saturating_sub(1)
    }

    /// Returns the line number of the next line and moves past it.
    fn advance(&mut self) -> usize {
        let number = self.start;
        self.start += 1;
        self.count = self.count.saturating_sub(1);
        number
    }
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

// ── Gutter Sizing ──

/// The largest line number a hunk header reaches, which sizes the gutter.
fn max_line_number(text: &str) -> usize {
    text.lines()
        .filter_map(parse_hunk_header)
        .map(|(old, new)| old.last().max(new.last()))
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use indoc::{formatdoc, indoc};
    use ratatui::text::Span;

    use super::*;
    use crate::slash::truncate_for_display;
    use crate::tui::glyphs::TOOL_BORDER_CONT;
    use crate::tui::theme::Theme;

    fn ctx_at(width: u16, theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            width,
            theme,
            show_thinking: false,
        }
    }

    fn range(start: usize, count: usize) -> HunkRange {
        HunkRange { start, count }
    }

    fn body_rows(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                let row: String = line.spans[1..].iter().map(|s| s.content.as_ref()).collect();
                row.trim_end().to_owned()
            })
            .collect()
    }

    // ── parse_diff_git_path ──

    #[test]
    fn parse_diff_git_path_extracts_a_path() {
        assert_eq!(
            parse_diff_git_path("diff --git a/src/main.rs b/src/main.rs"),
            Some("src/main.rs"),
        );
    }

    #[test]
    fn parse_diff_git_path_keeps_a_space_and_b_slash_inside_the_path() {
        assert_eq!(
            parse_diff_git_path("diff --git a/x b/y.rs b/x b/y.rs"),
            Some("x b/y.rs"),
        );
    }

    #[test]
    fn parse_diff_git_path_rejects_a_line_without_one_path_on_both_sides() {
        assert_eq!(parse_diff_git_path("@@ -1 +1 @@"), None);
        assert_eq!(parse_diff_git_path("plain text"), None);
        assert_eq!(parse_diff_git_path("diff --git a/old.rs b/new.rs"), None);
    }

    // ── parse_hunk_header ──

    #[test]
    fn parse_hunk_header_with_counts() {
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
    fn parse_hunk_header_omitted_count_spans_one_line() {
        assert_eq!(
            parse_hunk_header("@@ -42 +43 @@"),
            Some((range(42, 1), range(43, 1))),
        );
    }

    #[test]
    fn parse_hunk_header_rejects_lines_that_are_not_hunk_headers() {
        assert_eq!(parse_hunk_header("+ added line"), None);
        assert_eq!(parse_hunk_header("@@ -x +1 @@"), None);
        assert_eq!(parse_hunk_header(""), None);
    }

    // ── HunkRange::last ──

    #[test]
    fn hunk_range_last_is_start_plus_count_minus_one() {
        assert_eq!(range(27, 20).last(), 46);
        assert_eq!(range(27, 1).last(), 27);
        assert_eq!(range(5, 0).last(), 5);
    }

    // ── max_line_number ──

    #[test]
    fn max_line_number_uses_largest_hunk_extent() {
        // Asymmetric extents: dropping `.max()` would collapse to the narrower side.
        assert_eq!(max_line_number("@@ -1,1 +1,10 @@"), 10);
        assert_eq!(max_line_number("@@ -100,5 +1,1 @@"), 104);
    }

    #[test]
    fn max_line_number_ignores_text_without_hunks() {
        assert_eq!(max_line_number(""), 0);
        assert_eq!(max_line_number("Untracked files:\n  foo"), 0);
    }

    // ── render ──

    #[test]
    fn render_emits_path_header_then_hunk_then_body_rows() {
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/foo.rs b/foo.rs
            index abc..def 100644
            --- a/foo.rs
            +++ b/foo.rs
            @@ -1,3 +1,3 @@
            -old line
            +new line
             context
        "});

        let lines = block.render(&ctx_at(80, &theme));

        assert_eq!(
            body_rows(&lines),
            [
                "foo.rs",
                "@@ -1,3 +1,3 @@",
                "1 - old line",
                "1 + new line",
                "2 │ context",
            ]
        );
    }

    #[test]
    fn render_skips_index_and_marker_lines() {
        // `index`, `--- a/X`, and `+++ b/X` are skipped, since the path header names the file.
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/foo.rs b/foo.rs
            index abc..def 100644
            --- a/foo.rs
            +++ b/foo.rs
        "});

        let lines = block.render(&ctx_at(80, &theme));

        assert_eq!(body_rows(&lines), ["foo.rs"]);
    }

    #[test]
    fn render_classifies_hunk_lines_by_first_char() {
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/notes.md b/notes.md
            --- a/notes.md
            +++ b/notes.md
            @@ -10,4 +10,3 @@
             intro
            -- item
            --- yaml
            +++ counter
             outro
        "});
        let lines = block.render(&ctx_at(80, &theme));
        assert_eq!(
            body_rows(&lines[2..]),
            [
                "10 │ intro",
                "11 - - item",
                "12 - -- yaml",
                "11 + ++ counter",
                "12 │ outro",
            ],
        );
    }

    #[test]
    fn render_ends_hunks_by_count_so_unparsed_file_headers_stay_headers() {
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {r#"
            diff --git a/one.rs b/one.rs
            index 1111111..2222222 100644
            --- a/one.rs
            +++ b/one.rs
            @@ -1 +1 @@
            -old
            +new
            \ No newline at end of file
            diff --git "a/\346\226\207.txt" "b/\346\226\207.txt"
            index 3333333..4444444 100644
            --- "a/\346\226\207.txt"
            +++ "b/\346\226\207.txt"
            @@ -1 +1 @@
            -x
            +y
        "#});
        let lines = block.render(&ctx_at(80, &theme));
        assert_eq!(
            body_rows(&lines),
            [
                "one.rs",
                "@@ -1 +1 @@",
                "1 - old",
                "1 + new",
                r"\ No newline at end of file",
                r#"diff --git "a/\346\226\207.txt" "b/\346\226\207.txt""#,
                "@@ -1 +1 @@",
                "1 - x",
                "1 + y",
            ],
        );
        assert_eq!(lines[4].spans[1].style, theme.dim());
    }

    #[test]
    fn render_names_a_renamed_file_by_both_paths() {
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/old.rs b/new.rs
            similarity index 90%
            rename from old.rs
            rename to new.rs
            index 1111111..2222222 100644
            --- a/old.rs
            +++ b/new.rs
            @@ -1 +1 @@
            -a
            +b
        "});

        let lines = block.render(&ctx_at(80, &theme));

        assert_eq!(
            body_rows(&lines),
            ["old.rs → new.rs", "@@ -1 +1 @@", "1 - a", "1 + b"]
        );
    }

    #[test]
    fn render_reads_an_empty_line_inside_a_hunk_as_a_blank_context_row() {
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/x b/x
            @@ -1,4 +1,4 @@
             a

             b
            -c
            +C
        "});

        let lines = block.render(&ctx_at(80, &theme));

        assert_eq!(
            body_rows(&lines),
            [
                "x",
                "@@ -1,4 +1,4 @@",
                "1 │ a",
                "2 │",
                "3 │ b",
                "4 - c",
                "4 + C"
            ]
        );
    }

    #[test]
    fn render_closes_a_hunk_once_each_side_reaches_its_count() {
        // Uneven counts: the old side closes after two lines and the new side after one, so the
        // blank line and the indented path after them sit outside the hunk.
        let theme = Theme::default();
        let block = GitDiffBlock::new(formatdoc! {"
            diff --git a/x b/x
            @@ -1,2 +1 @@
            -a
            -b
            +c

            {UNTRACKED_HEADING}
              new.txt
        "});

        let lines = block.render(&ctx_at(80, &theme));

        assert_eq!(
            body_rows(&lines),
            [
                "x",
                "@@ -1,2 +1 @@",
                "1 - a",
                "2 - b",
                "1 + c",
                "",
                UNTRACKED_HEADING,
                "  new.txt",
            ]
        );
    }

    #[test]
    fn render_starts_each_file_outside_a_hunk_even_when_the_last_hunk_ran_short() {
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/x b/x
            @@ -1,3 +1,3 @@
            -a
            +b
            diff --git a/y b/y
            --- a/y
            +++ b/y
            @@ -1 +1 @@
            -c
            +d
        "});

        let lines = block.render(&ctx_at(80, &theme));

        assert_eq!(
            body_rows(&lines),
            [
                "x",
                "@@ -1,3 +1,3 @@",
                "1 - a",
                "1 + b",
                "y",
                "@@ -1 +1 @@",
                "1 - c",
                "1 + d",
            ]
        );
    }

    #[test]
    fn render_add_row_carries_diff_add_row_bg() {
        // Pin: dropping or swapping `diff_add_row` on the renderer construction would trip here.
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/x b/x
            @@ -1 +1 @@
            +only added
        "});
        let lines = block.render(&ctx_at(80, &theme));
        // Layout: [path header, hunk header, add row]. The number, separator, and text spans carry
        // the bg, while the bar stays clear.
        let add_row = lines.last().expect("at least one row");
        let bgs: Vec<_> = add_row.spans.iter().map(|s| s.style.bg).collect();
        assert_eq!(bgs[0], None, "bar prefix must stay clear");
        assert!(
            bgs.iter().skip(1).any(|bg| *bg == theme.diff_add_row().bg),
            "expected diff_add_row bg on inner spans, got {bgs:?}",
        );
    }

    #[test]
    fn render_del_row_carries_diff_del_row_bg() {
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/x b/x
            @@ -1 +1 @@
            -only removed
            +replacement
        "});
        let lines = block.render(&ctx_at(80, &theme));
        let del_row = lines
            .iter()
            .find(|line| line.spans.iter().any(|s| s.content.as_ref() == " - "))
            .expect("del row present");
        let bgs: Vec<_> = del_row.spans.iter().map(|s| s.style.bg).collect();
        assert!(
            bgs.iter().skip(1).any(|bg| *bg == theme.diff_del_row().bg),
            "expected diff_del_row bg on inner spans, got {bgs:?}",
        );
    }

    #[test]
    fn render_untracked_section_outside_hunks_emits_plain_rows() {
        // Indented untracked paths look like context rows but must not paint as one.
        let theme = Theme::default();
        let block = GitDiffBlock::new(formatdoc! {"
            {UNTRACKED_HEADING}
              new.txt
              also-new.rs
        "});

        let lines = block.render(&ctx_at(80, &theme));

        let row = |text, style| {
            Line::from(vec![
                Span::styled(TOOL_BORDER_CONT, theme.tool_border()),
                Span::styled(text, style),
            ])
        };
        assert_eq!(
            lines,
            [
                row(UNTRACKED_HEADING, theme.dim()),
                row("  new.txt", theme.text()),
                row("  also-new.rs", theme.text()),
            ]
        );
    }

    #[test]
    fn render_dims_the_truncation_footer_of_a_diff_cut_inside_a_hunk() {
        let theme = Theme::default();
        let diff = format!(
            "diff --git a/x b/x\n@@ -0,0 +1,6000 @@\n{}",
            "+added line\n".repeat(6000),
        );
        let text = truncate_for_display(diff);
        let footer = text.lines().last().unwrap().to_owned();
        let block = GitDiffBlock::new(text);

        let lines = block.render(&ctx_at(80, &theme));

        let row = |text| {
            Line::from(vec![
                Span::styled(TOOL_BORDER_CONT, theme.tool_border()),
                Span::styled(text, theme.dim()),
            ])
        };
        assert!(footer.starts_with(TRUNCATION_FOOTER), "{footer:?}");
        assert_eq!(lines[lines.len() - 2..], [row(String::new()), row(footer)]);
    }

    #[test]
    fn render_walks_line_numbers_per_hunk_starts() {
        // Line numbers start from the `@@` header's line.
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/x b/x
            @@ -27,2 +27,2 @@
            -alpha
            -beta
            +gamma
            +delta
        "});
        let lines = block.render(&ctx_at(80, &theme));
        let body = &lines[2..]; // skip path + hunk headers
        let numbers: Vec<String> = body
            .iter()
            .map(|line| line.spans[1].content.trim().to_owned())
            .collect();
        assert_eq!(numbers, vec!["27", "28", "27", "28"]);
    }

    #[test]
    fn render_advances_line_numbers_through_context_rows() {
        // One fixture pins both increments: `+third` takes new_ln (bumped 2 → 3 by the context
        // row), and `-fourth` takes old_ln (1 → 2).
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/x b/x
            @@ -1,3 +1,3 @@
            +first
             middle
            +third
            -fourth
        "});
        let lines = block.render(&ctx_at(80, &theme));
        let body = &lines[2..];
        let numbers: Vec<String> = body
            .iter()
            .map(|line| line.spans[1].content.trim().to_owned())
            .collect();
        assert_eq!(numbers, vec!["1", "2", "3", "2"]);
    }

    #[test]
    fn render_corrupt_hunk_body_falls_through_to_defensive_row() {
        // A corrupt body line renders as a plain bordered row with no number or bg, and it bumps
        // no number, so the rest of the hunk keeps its numbering.
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/x b/x
            @@ -1,2 +1,2 @@
            +real add
            corrupt-no-marker
            +second add
        "});
        let lines = block.render(&ctx_at(80, &theme));
        let body = &lines[2..];
        assert_eq!(body.len(), 3, "{body:#?}");

        let corrupt_row = &body[1];
        let body_text: String = corrupt_row
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(body_text.contains("corrupt-no-marker"), "{body_text:?}");
        for span in &corrupt_row.spans {
            assert!(
                span.style.bg != theme.diff_add_row().bg
                    && span.style.bg != theme.diff_del_row().bg,
                "defensive row must not paint diff bgs: {corrupt_row:?}",
            );
        }

        // The surrounding `+` rows keep 1 and 2, since the corrupt line takes no number.
        let plus_numbers: Vec<String> = [&body[0], &body[2]]
            .iter()
            .map(|line| line.spans[1].content.trim().to_owned())
            .collect();
        assert_eq!(plus_numbers, vec!["1", "2"]);
    }
}
