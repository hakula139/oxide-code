//! `GitDiffBlock` — render a `git diff` output with the same visual
//! treatment as the Edit-tool diff body.
//!
//! Reuses `numbered_row::Renderer` and `bordered_row::render` from the
//! tool block tree so a slash `/diff` and an Edit tool result share
//! one aesthetic: red row bg on `-` lines, green row bg on `+` lines,
//! dim hunk headers, line numbers in a left gutter. The block is
//! parsed lazily on `render`; `GitDiffBlock` itself just owns the raw
//! text plus the optional truncation footer the producer appended.

use ratatui::style::Modifier;
use ratatui::text::Line;

use super::tool::{bordered_row, numbered_row};
use super::{BlockKind, ChatBlock, RenderCtx};

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
        let mut in_hunk = false;

        for line in self.text.lines() {
            if let Some(path) = parse_diff_git_path(line) {
                bordered_row::render(
                    &mut out,
                    ctx,
                    border,
                    path.to_owned(),
                    ctx.theme.text().add_modifier(Modifier::BOLD),
                );
                in_hunk = false;
            } else if line.starts_with("index ")
                || line.starts_with("--- ")
                || line.starts_with("+++ ")
                || line.starts_with("similarity ")
                || line.starts_with("rename ")
                || line.starts_with("new file ")
                || line.starts_with("deleted file ")
            {
                // Metadata git emits between "diff --git" and the first
                // hunk; the path header already names the file, so
                // these rows are noise.
            } else if let Some((o, n)) = parse_hunk_starts(line) {
                old_ln = o;
                new_ln = n;
                in_hunk = true;
                bordered_row::render(&mut out, ctx, border, line.to_owned(), ctx.theme.dim());
            } else if line.is_empty() {
                // A truly empty line (no leading space) ends the
                // current hunk — well-formed unified diffs prefix
                // context with " ", so a zero-byte line is a section
                // break before the untracked heading or the
                // truncation footer.
                in_hunk = false;
                bordered_row::render(&mut out, ctx, border, String::new(), ctx.theme.dim());
            } else if !in_hunk {
                // Outside a hunk: untracked-files heading, untracked
                // file paths, truncation footer.
                let style = if line.starts_with("Untracked") || line.starts_with("(truncated") {
                    ctx.theme.dim()
                } else {
                    ctx.theme.text()
                };
                bordered_row::render(&mut out, ctx, border, line.to_owned(), style);
            } else if let Some(text) = strip_marker(line, '+') {
                add.render(&mut out, new_ln, text, ctx.theme.text());
                new_ln += 1;
            } else if let Some(text) = strip_marker(line, '-') {
                del.render(&mut out, old_ln, text, ctx.theme.text());
                old_ln += 1;
            } else if let Some(text) = line.strip_prefix(' ') {
                context.render(&mut out, new_ln, text, ctx.theme.dim());
                old_ln += 1;
                new_ln += 1;
            } else {
                // Defensive — well-formed unified diffs always prefix
                // hunk-body lines with `+`, `-`, or ` `. Render as a
                // plain bordered row so corrupt input still surfaces
                // visually instead of silently misaligning numbers.
                bordered_row::render(&mut out, ctx, border, line.to_owned(), ctx.theme.dim());
            }
        }
        out
    }

    fn block_kind(&self) -> BlockKind {
        BlockKind::Other
    }
}

/// Returns the "a" path of a `diff --git a/X b/Y` line, or `None` when
/// the line isn't a file header. The "a" / "b" prefixes are git's
/// standard markers; for new / deleted files git emits `/dev/null` on
/// one side, but the other side still carries the real path so we
/// always have something to show.
fn parse_diff_git_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("diff --git ")?;
    let after_a = rest.strip_prefix("a/")?;
    let space = after_a.find(" b/")?;
    Some(&after_a[..space])
}

/// Parses `@@ -A,B +C,D @@ ...` (or `@@ -A +C @@` when count is 1) and
/// returns the starting line numbers of the two sides. `None` when the
/// line isn't a hunk header.
fn parse_hunk_starts(line: &str) -> Option<(usize, usize)> {
    let after_minus = line.strip_prefix("@@ -")?;
    let old_start = parse_first_number(after_minus)?;
    let plus_at = line.find(" +")?;
    let after_plus = &line[plus_at + 2..];
    let new_start = parse_first_number(after_plus)?;
    Some((old_start, new_start))
}

/// Parses the leading integer up to the next `,` or ` `. Used by both
/// the start-only branch (`@@ -A`) and the count branch (`@@ -A,B`).
fn parse_first_number(s: &str) -> Option<usize> {
    let stop = s.find([',', ' ']).unwrap_or(s.len());
    s[..stop].parse().ok()
}

/// Returns the body of a `+`/`-` line *unless* it's the diff metadata
/// `+++ b/X` / `--- a/X`. Walking the metadata branch first keeps the
/// caller from having to track which `+`/`-` lines are real content.
fn strip_marker(line: &str, marker: char) -> Option<&str> {
    line.strip_prefix(marker)
        .filter(|rest| !rest.starts_with(marker))
}

/// Width of the line-number column — the highest line number across
/// every hunk's `(start + count - 1)`. Computed globally so the gutter
/// stays at one width across the whole diff (consistent with the
/// Edit-tool diff body, which renders one chunk at a time but pins
/// the column to that chunk's max).
fn max_line_number_width(text: &str) -> usize {
    let mut max_ln: usize = 0;
    for line in text.lines() {
        if let Some(extents) = parse_hunk_extents(line) {
            max_ln = max_ln.max(extents);
        }
    }
    max_ln.to_string().len().max(1)
}

/// Highest line number a hunk header refers to on either side —
/// `start + count - 1` for whichever is larger. `count` defaults to 1
/// when the header omits the `,B` suffix.
fn parse_hunk_extents(line: &str) -> Option<usize> {
    let after_minus = line.strip_prefix("@@ -")?;
    let old_extent = parse_range_extent(after_minus)?;
    let plus_at = line.find(" +")?;
    let after_plus = &line[plus_at + 2..];
    let new_extent = parse_range_extent(after_plus)?;
    Some(old_extent.max(new_extent))
}

/// `start[,count]` → `start + count - 1`. Mirrors the unified-diff
/// `@@` convention: `@@ -27,20` means lines 27..=46. A bare `@@ -27`
/// means a single line at 27.
fn parse_range_extent(s: &str) -> Option<usize> {
    let stop = s.find([',', ' ']).unwrap_or(s.len());
    let start: usize = s[..stop].parse().ok()?;
    if s.as_bytes().get(stop) != Some(&b',') {
        return Some(start);
    }
    let rest = &s[stop + 1..];
    let stop2 = rest.find(' ').unwrap_or(rest.len());
    let count: usize = rest[..stop2].parse().ok()?;
    Some(start + count.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::tui::theme::Theme;

    fn ctx_at(width: u16, theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            width,
            theme,
            show_thinking: false,
        }
    }

    // ── parse_diff_git_path ──

    #[test]
    fn parse_diff_git_path_returns_a_path() {
        assert_eq!(
            parse_diff_git_path("diff --git a/src/main.rs b/src/main.rs"),
            Some("src/main.rs"),
        );
    }

    #[test]
    fn parse_diff_git_path_returns_none_for_unrelated_line() {
        assert_eq!(parse_diff_git_path("@@ -1 +1 @@"), None);
        assert_eq!(parse_diff_git_path("plain text"), None);
    }

    // ── parse_hunk_starts ──

    #[test]
    fn parse_hunk_starts_with_counts() {
        assert_eq!(parse_hunk_starts("@@ -27,20 +27,20 @@"), Some((27, 27)));
        assert_eq!(parse_hunk_starts("@@ -10,5 +12,7 @@ ctx"), Some((10, 12)));
    }

    #[test]
    fn parse_hunk_starts_without_counts() {
        // Single-line hunks omit the ",B" suffix — `@@ -42 +43 @@`.
        assert_eq!(parse_hunk_starts("@@ -42 +43 @@"), Some((42, 43)));
    }

    #[test]
    fn parse_hunk_starts_returns_none_for_non_hunk() {
        assert_eq!(parse_hunk_starts("+ added line"), None);
        assert_eq!(parse_hunk_starts(""), None);
    }

    // ── strip_marker ──

    #[test]
    fn strip_marker_returns_body_for_real_diff_lines() {
        assert_eq!(strip_marker("+added", '+'), Some("added"));
        assert_eq!(strip_marker("-removed", '-'), Some("removed"));
    }

    #[test]
    fn strip_marker_rejects_diff_metadata_double_marker() {
        // `+++ b/path` and `--- a/path` would otherwise look like add /
        // del lines if the renderer didn't filter them out before
        // reaching this branch — pin the safety net so a refactor that
        // dropped the metadata-skip branch above doesn't render them as
        // green/red rows.
        assert_eq!(strip_marker("+++ b/path", '+'), None);
        assert_eq!(strip_marker("--- a/path", '-'), None);
    }

    // ── max_line_number_width ──

    #[test]
    fn max_line_number_width_uses_largest_hunk_extent() {
        // Asymmetric hunk: old extent 1, new extent 10. Dropping the
        // `.max()` in `parse_hunk_extents` would return the smaller
        // side and collapse the gutter to width 1.
        let text = "@@ -1,1 +1,10 @@";
        assert_eq!(max_line_number_width(text), 2);
    }

    #[test]
    fn max_line_number_width_floors_at_one_when_no_hunks() {
        // The renderer never collapses the gutter to zero; otherwise the
        // separator would butt against the bar prefix.
        assert_eq!(max_line_number_width(""), 1);
        assert_eq!(max_line_number_width("Untracked files:\n  foo"), 1);
    }

    // ── parse_hunk_extents ──

    #[test]
    fn parse_hunk_extents_returns_max_of_old_and_new_sides() {
        // Pin the `.max()` directly so a future refactor can't drop it
        // silently — the integration test above only catches the case
        // where the loss changes the rendered gutter width.
        assert_eq!(parse_hunk_extents("@@ -1,1 +1,10 @@"), Some(10));
        assert_eq!(parse_hunk_extents("@@ -100,5 +1,1 @@"), Some(104));
    }

    #[test]
    fn parse_hunk_extents_handles_omitted_counts() {
        assert_eq!(parse_hunk_extents("@@ -42 +43 @@"), Some(43));
    }

    #[test]
    fn parse_hunk_extents_returns_none_for_non_hunk() {
        assert_eq!(parse_hunk_extents("plain"), None);
    }

    // ── parse_range_extent ──

    #[test]
    fn parse_range_extent_with_count_is_start_plus_count_minus_one() {
        // `27,20` covers lines 27..=46. The `saturating_sub(1)` is
        // load-bearing: count=1 must give extent=27, not 28.
        assert_eq!(parse_range_extent("27,20"), Some(46));
        assert_eq!(parse_range_extent("27,1"), Some(27));
    }

    #[test]
    fn parse_range_extent_without_count_is_just_start() {
        assert_eq!(parse_range_extent("42"), Some(42));
        assert_eq!(parse_range_extent("42 @@"), Some(42));
    }

    // ── render ──

    #[test]
    fn render_emits_path_header_then_hunk_then_body_rows() {
        // Use `indoc!` so the single leading space on " context" survives
        // — `\<newline>` line continuation would strip it and route the
        // line through the defensive branch instead of strip_prefix(' ').
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
        // path + hunk header + del + add + ctx = 5
        assert_eq!(lines.len(), 5, "{lines:#?}");
        let ctx_row = &lines[4];
        let body: String = ctx_row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(body.contains("context"), "ctx row missing body: {body:?}");
    }

    #[test]
    fn render_skips_index_and_marker_lines() {
        // `index ...`, `--- a/path`, `+++ b/path` are all noise; the
        // path header from `diff --git` already names the file.
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/foo.rs b/foo.rs
            index abc..def 100644
            --- a/foo.rs
            +++ b/foo.rs
        "});
        let lines = block.render(&ctx_at(80, &theme));
        assert_eq!(lines.len(), 1, "only path header should remain: {lines:#?}");
    }

    #[test]
    fn render_add_row_carries_diff_add_row_bg() {
        // Pin the green-row-bg path so a refactor that dropped the
        // diff_add_row argument from the add renderer construction
        // (or swapped it with del) trips here. The bg slot reaches
        // the rendered span via `Renderer::with_style`'s patch.
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/x b/x
            @@ -1 +1 @@
            +only added
        "});
        let lines = block.render(&ctx_at(80, &theme));
        // Layout: [path header, hunk header, add row].
        let add_row = lines.last().expect("at least one row");
        // Inner spans (number, separator, text) carry diff_add_row bg
        // patched via `Style::patch`. The bar prefix span stays clear.
        let bgs: Vec<_> = add_row.spans.iter().map(|s| s.style.bg).collect();
        assert_eq!(bgs[0], None, "bar prefix must stay clear");
        assert!(
            bgs.iter().skip(1).any(|bg| *bg == theme.diff_add.bg),
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
        // Find the del row (the `-` content).
        let del_row = lines
            .iter()
            .find(|line| line.spans.iter().any(|s| s.content.as_ref() == " - "))
            .expect("del row present");
        let bgs: Vec<_> = del_row.spans.iter().map(|s| s.style.bg).collect();
        assert!(
            bgs.iter().skip(1).any(|bg| *bg == theme.diff_del.bg),
            "expected diff_del_row bg on inner spans, got {bgs:?}",
        );
    }

    #[test]
    fn render_untracked_section_outside_hunks_emits_plain_rows() {
        // After all hunks the producer appends "Untracked files:" plus
        // indented file paths. None of those should land as green / red
        // rows, even though the indented lines start with two spaces
        // (which looks like a context line).
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            Untracked files:
              new.txt
              also-new.rs
        "});
        let lines = block.render(&ctx_at(80, &theme));
        for line in &lines {
            // No row should carry the diff_add or diff_del row bg.
            for span in &line.spans {
                assert!(
                    span.style.bg != theme.diff_add.bg && span.style.bg != theme.diff_del.bg,
                    "untracked section must not paint diff bgs: {line:?}",
                );
            }
        }
    }

    #[test]
    fn render_truncation_footer_after_hunks_renders_dim() {
        // The producer appends "(truncated: N KB more)" as a separate
        // paragraph. It must read as a dim footer, not a +/-/ctx row.
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/x b/x
            @@ -1 +1 @@
            +added

            (truncated: 12.3 KB more)
        "});
        let lines = block.render(&ctx_at(80, &theme));
        let footer = lines.last().expect("non-empty render");
        let text: String = footer.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("truncated"), "footer text: {text:?}");
    }

    #[test]
    fn render_walks_line_numbers_per_hunk_starts() {
        // Numbers must come from the @@ header, not be 1-based per
        // chunk index. A hunk starting at -27 / +27 must render `27`
        // for the first - and + lines, then `28` for the second, etc.
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
        // Body rows are after path header + hunk header (2 entries).
        let body = &lines[2..];
        let numbers: Vec<String> = body
            .iter()
            .map(|line| line.spans[1].content.trim().to_owned())
            .collect();
        assert_eq!(numbers, vec!["27", "28", "27", "28"]);
    }

    #[test]
    fn render_advances_line_numbers_through_context_rows() {
        // Context rows advance both old_ln and new_ln. Pin both
        // directions in one fixture: `+third` rides new_ln (which the
        // context bumped from 2 to 3), `-fourth` rides old_ln (which
        // the context bumped from 1 to 2). Dropping either increment
        // would mis-number one of the surrounding marker rows.
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
        // Inside a hunk, a line without `+`, `-`, or ` ` is malformed.
        // It must render as a plain bordered row (no number gutter, no
        // add / del bg) and must not bump line numbers — otherwise the
        // gutter drifts on the rest of the hunk.
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
                span.style.bg != theme.diff_add.bg && span.style.bg != theme.diff_del.bg,
                "defensive row must not paint diff bgs: {corrupt_row:?}",
            );
        }

        // Surrounding `+` rows keep numbers 1 and 2 — the corrupt line
        // consumed no slot on either side.
        let plus_numbers: Vec<String> = [&body[0], &body[2]]
            .iter()
            .map(|line| line.spans[1].content.trim().to_owned())
            .collect();
        assert_eq!(plus_numbers, vec!["1", "2"]);
    }

    // ── block_kind ──

    #[test]
    fn block_kind_is_other() {
        // `Result` kind forces blank-before spacing in chat view; pin
        // `Other` so a copy-paste from `ToolResultBlock` doesn't drift.
        let block = GitDiffBlock::new("diff --git a/x b/x\n");
        assert!(matches!(block.block_kind(), BlockKind::Other));
    }
}
