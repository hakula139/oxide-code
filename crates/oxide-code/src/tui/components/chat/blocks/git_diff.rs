//! `/diff` output block, drawn like an Edit-tool diff with a line-number gutter. It reuses the
//! tool tree's `numbered_row` and `bordered_row`, and parses the raw text lazily on `render`.

use ratatui::style::Modifier;
use ratatui::text::Line;

use super::tool::{bordered_row, numbered_row};
use super::{ChatBlock, RenderCtx};

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
        let number_width = max_line_number_width(&self.text);
        let context = numbered_row::Renderer::new(ctx, border, number_width);
        let del = numbered_row::Renderer::with_style(
            ctx,
            border,
            number_width,
            " - ",
            ctx.theme.error(),
            Some(ctx.theme.diff_del_row()),
        );
        let add = numbered_row::Renderer::with_style(
            ctx,
            border,
            number_width,
            " + ",
            ctx.theme.success(),
            Some(ctx.theme.diff_add_row()),
        );

        let mut out = Vec::new();
        let mut old_ln: usize = 0;
        let mut new_ln: usize = 0;
        let mut old_left: usize = 0;
        let mut new_left: usize = 0;

        for line in self.text.lines() {
            if line.starts_with("diff ") {
                let header = parse_diff_git_path(line).unwrap_or(line);
                bordered_row::render(
                    &mut out,
                    ctx,
                    border,
                    header.to_owned(),
                    ctx.theme.text().add_modifier(Modifier::BOLD),
                );
                old_left = 0;
                new_left = 0;
            } else if let Some((old, new)) = parse_hunk_header(line) {
                old_ln = old.start;
                new_ln = new.start;
                old_left = old.count;
                new_left = new.count;
                bordered_row::render(&mut out, ctx, border, line.to_owned(), ctx.theme.dim());
            } else if line.is_empty() {
                old_left = 0;
                new_left = 0;
                bordered_row::render(&mut out, ctx, border, String::new(), ctx.theme.dim());
            } else if old_left == 0 && new_left == 0 {
                if is_file_header(line) {
                    continue;
                }
                // Untracked heading and paths, the truncation footer, or a no-newline marker.
                let style = if line.starts_with("Untracked")
                    || line.starts_with("(truncated")
                    || line.starts_with('\\')
                {
                    ctx.theme.dim()
                } else {
                    ctx.theme.text()
                };
                bordered_row::render(&mut out, ctx, border, line.to_owned(), style);
            } else if let Some(text) = line.strip_prefix('+') {
                add.render(&mut out, new_ln, text, ctx.theme.text());
                new_ln += 1;
                new_left = new_left.saturating_sub(1);
            } else if let Some(text) = line.strip_prefix('-') {
                del.render(&mut out, old_ln, text, ctx.theme.text());
                old_ln += 1;
                old_left = old_left.saturating_sub(1);
            } else if let Some(text) = line.strip_prefix(' ') {
                context.render(&mut out, new_ln, text, ctx.theme.dim());
                old_ln += 1;
                new_ln += 1;
                old_left = old_left.saturating_sub(1);
                new_left = new_left.saturating_sub(1);
            } else {
                bordered_row::render(&mut out, ctx, border, line.to_owned(), ctx.theme.dim());
            }
        }
        out
    }
}

// ── Diff Parsing ──

/// Extended header lines between `diff --git` and the first `@@`, covered by the path row.
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

fn parse_diff_git_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("diff --git ")?;
    let after_a = rest.strip_prefix("a/")?;
    let space = after_a.find(" b/")?;
    Some(&after_a[..space])
}

/// One side of a hunk header: the first line number and the number of lines the hunk spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HunkRange {
    start: usize,
    count: usize,
}

impl HunkRange {
    /// Last line number in the range, or `start` when the range is empty.
    fn last(self) -> usize {
        self.start + self.count.saturating_sub(1)
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

fn max_line_number_width(text: &str) -> usize {
    let max_ln = text
        .lines()
        .filter_map(parse_hunk_header)
        .map(|(old, new)| old.last().max(new.last()))
        .max()
        .unwrap_or(0);
    max_ln.to_string().len().max(1)
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ratatui::text::Span;

    use super::*;
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
    fn parse_diff_git_path_is_none_for_unrelated_line() {
        assert_eq!(parse_diff_git_path("@@ -1 +1 @@"), None);
        assert_eq!(parse_diff_git_path("plain text"), None);
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
    fn parse_hunk_header_is_none_for_non_hunk() {
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

    // ── max_line_number_width ──

    #[test]
    fn max_line_number_width_uses_largest_hunk_extent() {
        // Asymmetric extents: dropping `.max()` would collapse to the narrower side.
        assert_eq!(max_line_number_width("@@ -1,1 +1,10 @@"), 2);
        assert_eq!(max_line_number_width("@@ -100,5 +1,1 @@"), 3);
    }

    #[test]
    fn max_line_number_width_floors_at_one_when_no_hunks() {
        // Width 0 would butt the separator against the bar.
        assert_eq!(max_line_number_width(""), 1);
        assert_eq!(max_line_number_width("Untracked files:\n  foo"), 1);
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
        let block = GitDiffBlock::new(indoc! {"
            Untracked files:
              new.txt
              also-new.rs
        "});
        let lines = block.render(&ctx_at(80, &theme));
        for line in &lines {
            for span in &line.spans {
                assert!(
                    span.style.bg != theme.diff_add_row().bg
                        && span.style.bg != theme.diff_del_row().bg,
                    "untracked section must not paint diff bgs: {line:?}",
                );
            }
        }
    }

    #[test]
    fn render_truncation_footer_after_hunks_renders_dim() {
        // The truncation footer reads as a dim row, outside any hunk.
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/x b/x
            @@ -1 +1 @@
            +added

            (truncated: 12.3 KB more)
        "});

        let lines = block.render(&ctx_at(80, &theme));

        assert_eq!(
            lines.last(),
            Some(&Line::from(vec![
                Span::styled(TOOL_BORDER_CONT, theme.tool_border()),
                Span::styled("(truncated: 12.3 KB more)", theme.dim()),
            ]))
        );
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
