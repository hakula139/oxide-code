//! `GitDiffBlock` — `git diff` rendered with the Edit-tool diff
//! aesthetic (red `-` rows, green `+` rows, dim hunk headers, line-
//! number gutter). Reuses `numbered_row` / `bordered_row` from the tool
//! tree. The block owns raw text and parses it lazily on `render`.

use ratatui::style::Modifier;
use ratatui::text::Line;

use super::tool::{bordered_row, numbered_row};
use super::{BlockKind, ChatBlock, RenderCtx};

// ── GitDiffBlock ──

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
            } else if let Some((o, n)) = parse_hunk_starts(line) {
                old_ln = o;
                new_ln = n;
                in_hunk = true;
                bordered_row::render(&mut out, ctx, border, line.to_owned(), ctx.theme.dim());
            } else if line.is_empty() {
                in_hunk = false;
                bordered_row::render(&mut out, ctx, border, String::new(), ctx.theme.dim());
            } else if !in_hunk {
                // Untracked heading / paths / truncation footer.
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
                bordered_row::render(&mut out, ctx, border, line.to_owned(), ctx.theme.dim());
            }
        }
        out
    }

    fn block_kind(&self) -> BlockKind {
        BlockKind::Other
    }
}

// ── Diff Parsing ──

fn parse_diff_git_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("diff --git ")?;
    let after_a = rest.strip_prefix("a/")?;
    let space = after_a.find(" b/")?;
    Some(&after_a[..space])
}

fn parse_hunk_starts(line: &str) -> Option<(usize, usize)> {
    let after_minus = line.strip_prefix("@@ -")?;
    let old_start = parse_first_number(after_minus)?;
    let plus_at = line.find(" +")?;
    let after_plus = &line[plus_at + 2..];
    let new_start = parse_first_number(after_plus)?;
    Some((old_start, new_start))
}

fn parse_first_number(s: &str) -> Option<usize> {
    let stop = s.find([',', ' ']).unwrap_or(s.len());
    s[..stop].parse().ok()
}

fn strip_marker(line: &str, marker: char) -> Option<&str> {
    line.strip_prefix(marker)
        .filter(|rest| !rest.starts_with(marker))
}

// ── Gutter Sizing ──

fn max_line_number_width(text: &str) -> usize {
    let mut max_ln: usize = 0;
    for line in text.lines() {
        if let Some(extents) = parse_hunk_extents(line) {
            max_ln = max_ln.max(extents);
        }
    }
    max_ln.to_string().len().max(1)
}

fn parse_hunk_extents(line: &str) -> Option<usize> {
    let after_minus = line.strip_prefix("@@ -")?;
    let old_extent = parse_range_extent(after_minus)?;
    let plus_at = line.find(" +")?;
    let after_plus = &line[plus_at + 2..];
    let new_extent = parse_range_extent(after_plus)?;
    Some(old_extent.max(new_extent))
}

fn parse_range_extent(s: &str) -> Option<usize> {
    let start = parse_first_number(s)?;
    let Some(comma) = s.find(',') else {
        return Some(start);
    };
    let count = parse_first_number(&s[comma + 1..])?;
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

    // ── parse_hunk_starts ──

    #[test]
    fn parse_hunk_starts_with_counts() {
        assert_eq!(parse_hunk_starts("@@ -27,20 +27,20 @@"), Some((27, 27)));
        assert_eq!(parse_hunk_starts("@@ -10,5 +12,7 @@ ctx"), Some((10, 12)));
    }

    #[test]
    fn parse_hunk_starts_without_counts() {
        // Single-line hunks omit the ",B" suffix.
        assert_eq!(parse_hunk_starts("@@ -42 +43 @@"), Some((42, 43)));
    }

    #[test]
    fn parse_hunk_starts_is_none_for_non_hunk() {
        assert_eq!(parse_hunk_starts("+ added line"), None);
        assert_eq!(parse_hunk_starts(""), None);
    }

    // ── strip_marker ──

    #[test]
    fn strip_marker_extracts_body_for_real_diff_lines() {
        assert_eq!(strip_marker("+added", '+'), Some("added"));
        assert_eq!(strip_marker("-removed", '-'), Some("removed"));
    }

    #[test]
    fn strip_marker_rejects_diff_metadata_double_marker() {
        // Safety net: if the renderer's metadata-skip branch ever
        // drops, `+++ b/X` / `--- a/X` must not render as green/red.
        assert_eq!(strip_marker("+++ b/path", '+'), None);
        assert_eq!(strip_marker("--- a/path", '-'), None);
    }

    // ── max_line_number_width ──

    #[test]
    fn max_line_number_width_uses_largest_hunk_extent() {
        // Asymmetric extents: dropping `.max()` would collapse to 1.
        let text = "@@ -1,1 +1,10 @@";
        assert_eq!(max_line_number_width(text), 2);
    }

    #[test]
    fn max_line_number_width_floors_at_one_when_no_hunks() {
        // Floor at 1 — width 0 would butt the separator against the bar.
        assert_eq!(max_line_number_width(""), 1);
        assert_eq!(max_line_number_width("Untracked files:\n  foo"), 1);
    }

    // ── parse_hunk_extents ──

    #[test]
    fn parse_hunk_extents_produces_max_of_old_and_new_sides() {
        // Pin `.max()` directly; the integration test above only
        // catches losses that change the rendered gutter width.
        assert_eq!(parse_hunk_extents("@@ -1,1 +1,10 @@"), Some(10));
        assert_eq!(parse_hunk_extents("@@ -100,5 +1,1 @@"), Some(104));
    }

    #[test]
    fn parse_hunk_extents_handles_omitted_counts() {
        assert_eq!(parse_hunk_extents("@@ -42 +43 @@"), Some(43));
    }

    #[test]
    fn parse_hunk_extents_is_none_for_non_hunk() {
        assert_eq!(parse_hunk_extents("plain"), None);
    }

    // ── parse_range_extent ──

    #[test]
    fn parse_range_extent_with_count_is_start_plus_count_minus_one() {
        // count=1 must give extent=start, not start+1.
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
        // `index`, `--- a/X`, `+++ b/X` are all skipped — the path header already names the file.
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
        // Pin: dropping or swapping `diff_add_row` on the renderer construction would trip here.
        let theme = Theme::default();
        let block = GitDiffBlock::new(indoc! {"
            diff --git a/x b/x
            @@ -1 +1 @@
            +only added
        "});
        let lines = block.render(&ctx_at(80, &theme));
        // Layout: [path header, hunk header, add row]. Inner spans
        // (number, separator, text) carry the bg; the bar stays clear.
        let add_row = lines.last().expect("at least one row");
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
        // Indented untracked paths must not paint as context rows
        // even though their leading spaces look like one.
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
                    span.style.bg != theme.diff_add.bg && span.style.bg != theme.diff_del.bg,
                    "untracked section must not paint diff bgs: {line:?}",
                );
            }
        }
    }

    #[test]
    fn render_truncation_footer_after_hunks_renders_dim() {
        // Truncation footer must read as a dim row, not +/-/ctx.
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
        // Numbers come from the `@@` header, not 1-based per chunk.
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
        // Pin both increments via one fixture: `+third` rides new_ln
        // (bumped 2→3 by context), `-fourth` rides old_ln (1→2).
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
        // Corrupt body line → plain bordered row, no number, no bg,
        // and no number-bump (else the rest of the hunk drifts).
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

        // Surrounding `+` rows keep 1 and 2 — corrupt line consumed no number slot on either side.
        let plus_numbers: Vec<String> = [&body[0], &body[2]]
            .iter()
            .map(|line| line.spans[1].content.trim().to_owned())
            .collect();
        assert_eq!(plus_numbers, vec!["1", "2"]);
    }

    // ── block_kind ──

    #[test]
    fn block_kind_is_other() {
        // `Result` kind would force blank-before spacing; pin `Other`.
        let block = GitDiffBlock::new("diff --git a/x b/x\n");
        assert!(matches!(block.block_kind(), BlockKind::Other));
    }
}
