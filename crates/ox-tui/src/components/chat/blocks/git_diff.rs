//! `/diff` output block, drawn like an Edit-tool diff with a line-number gutter, from the rows
//! `/diff` parsed.

use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use super::rows::{NumberedRows, body_row};
use super::{ChatBlock, RenderCtx};
use crate::slash::{DiffRow, WorkingTreeDiff};

/// Heading over the untracked file list.
const UNTRACKED_HEADING: &str = "Untracked files:";

// ── GitDiffBlock ──

/// Output of `/diff`.
pub(crate) struct GitDiffBlock {
    diff: WorkingTreeDiff,
}

impl GitDiffBlock {
    pub(crate) fn new(diff: WorkingTreeDiff) -> Self {
        Self { diff }
    }
}

impl ChatBlock for GitDiffBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let border = ctx.theme.tool_border();
        let max_number = max_line_number(&self.diff.rows);
        let context = NumberedRows::new(ctx, border, max_number);
        let removed = NumberedRows::removed(ctx, border, max_number);
        let added = NumberedRows::added(ctx, border, max_number);

        let mut out = Vec::new();
        for row in &self.diff.rows {
            match row {
                DiffRow::File(label) => {
                    let style = ctx.theme.text().add_modifier(Modifier::BOLD);
                    body_row(&mut out, ctx, border, label.clone(), style);
                }
                DiffRow::Hunk(header) => {
                    body_row(&mut out, ctx, border, header.clone(), ctx.theme.dim());
                }
                DiffRow::Removed { number, text } => {
                    removed.render(&mut out, *number, text, ctx.theme.text());
                }
                DiffRow::Added { number, text } => {
                    added.render(&mut out, *number, text, ctx.theme.text());
                }
                DiffRow::Context { number, text } => {
                    context.render(&mut out, *number, text, ctx.theme.dim());
                }
                DiffRow::Marker(text) => {
                    body_row(&mut out, ctx, border, text.clone(), ctx.theme.dim());
                }
                DiffRow::Note(text) => {
                    body_row(&mut out, ctx, border, text.clone(), ctx.theme.text());
                }
            }
        }
        if !self.diff.untracked.is_empty() {
            push_separator(&mut out, ctx, border);
            body_row(&mut out, ctx, border, UNTRACKED_HEADING, ctx.theme.dim());
            for path in &self.diff.untracked {
                body_row(&mut out, ctx, border, format!("  {path}"), ctx.theme.text());
            }
        }
        if let Some(truncation) = &self.diff.truncation {
            push_separator(&mut out, ctx, border);
            body_row(
                &mut out,
                ctx,
                border,
                truncation.to_string(),
                ctx.theme.dim(),
            );
        }
        out
    }
}

/// Leaves a blank row between sections, unless the section opens the block.
fn push_separator(out: &mut Vec<Line<'static>>, ctx: &RenderCtx<'_>, border: Style) {
    if !out.is_empty() {
        body_row(out, ctx, border, "", ctx.theme.dim());
    }
}

// ── Gutter Sizing ──

/// The largest line number the rows show, which sizes the gutter.
fn max_line_number(rows: &[DiffRow]) -> usize {
    rows.iter()
        .filter_map(|row| match row {
            DiffRow::Removed { number, .. }
            | DiffRow::Added { number, .. }
            | DiffRow::Context { number, .. } => Some(*number),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use ratatui::text::Span;

    use super::*;
    use crate::glyphs::TOOL_BORDER_CONT;
    use crate::theme::Theme;

    fn removed(number: usize, text: &str) -> DiffRow {
        let text = text.to_owned();
        DiffRow::Removed { number, text }
    }

    fn added(number: usize, text: &str) -> DiffRow {
        let text = text.to_owned();
        DiffRow::Added { number, text }
    }

    fn context(number: usize, text: &str) -> DiffRow {
        let text = text.to_owned();
        DiffRow::Context { number, text }
    }

    fn tracked(rows: Vec<DiffRow>) -> WorkingTreeDiff {
        WorkingTreeDiff {
            rows,
            ..WorkingTreeDiff::default()
        }
    }

    mod render {
        use super::*;

        fn body_rows(lines: &[Line<'_>]) -> Vec<String> {
            lines
                .iter()
                .map(|line| {
                    let row: String = line.spans[1..].iter().map(|s| s.content.as_ref()).collect();
                    row.trim_end().to_owned()
                })
                .collect()
        }

        fn plain_row(theme: &Theme, text: impl Into<String>, style: Style) -> Line<'static> {
            Line::from(vec![
                Span::styled(TOOL_BORDER_CONT, theme.tool_border()),
                Span::styled(text.into(), style),
            ])
        }

        #[test]
        fn draws_each_row_kind_in_its_style() {
            let theme = Theme::default();
            let block = GitDiffBlock::new(tracked(vec![
                DiffRow::File("foo.rs".to_owned()),
                DiffRow::Note("old mode 100644".to_owned()),
                DiffRow::Hunk("@@ -1,3 +1,3 @@".to_owned()),
                removed(1, "old line"),
                added(1, "new line"),
                context(2, "context"),
                DiffRow::Marker(r"\ No newline at end of file".to_owned()),
            ]));

            let lines = block.render(&RenderCtx::new(80, &theme));

            assert_eq!(
                body_rows(&lines),
                [
                    "foo.rs",
                    "old mode 100644",
                    "@@ -1,3 +1,3 @@",
                    "1 - old line",
                    "1 + new line",
                    "2 │ context",
                    r"\ No newline at end of file",
                ]
            );
            let bold = theme.text().add_modifier(Modifier::BOLD);
            assert_eq!(lines[0], plain_row(&theme, "foo.rs", bold));
            assert_eq!(lines[1], plain_row(&theme, "old mode 100644", theme.text()));
            assert_eq!(lines[2], plain_row(&theme, "@@ -1,3 +1,3 @@", theme.dim()));
            let marker = r"\ No newline at end of file";
            assert_eq!(lines[6], plain_row(&theme, marker, theme.dim()));
        }

        #[test]
        fn add_row_carries_diff_add_row_bg() {
            // Pin: dropping or swapping `diff_add_row` in the renderer construction trips here.
            let theme = Theme::default();
            let block = GitDiffBlock::new(tracked(vec![added(1, "only added")]));

            let lines = block.render(&RenderCtx::new(80, &theme));

            // The number, separator, and text spans carry the bg, while the bar stays clear.
            let bgs: Vec<_> = lines[0].spans.iter().map(|s| s.style.bg).collect();
            assert_eq!(bgs[0], None, "bar prefix must stay clear");
            assert!(
                bgs.iter().skip(1).any(|bg| *bg == theme.diff_add_row().bg),
                "expected diff_add_row bg on inner spans, got {bgs:?}",
            );
        }

        #[test]
        fn del_row_carries_diff_del_row_bg() {
            let theme = Theme::default();
            let block = GitDiffBlock::new(tracked(vec![removed(1, "only removed")]));

            let lines = block.render(&RenderCtx::new(80, &theme));

            let bgs: Vec<_> = lines[0].spans.iter().map(|s| s.style.bg).collect();
            assert!(
                bgs.iter().skip(1).any(|bg| *bg == theme.diff_del_row().bg),
                "expected diff_del_row bg on inner spans, got {bgs:?}",
            );
        }

        #[test]
        fn pads_every_number_to_the_widest_one() {
            let theme = Theme::default();
            let block = GitDiffBlock::new(tracked(vec![context(9, "nine"), added(10, "ten")]));

            let lines = block.render(&RenderCtx::new(80, &theme));

            assert_eq!(body_rows(&lines), [" 9 │ nine", "10 + ten"]);
        }

        #[test]
        fn lists_untracked_paths_under_a_dim_heading_after_a_blank_row() {
            let theme = Theme::default();
            let block = GitDiffBlock::new(WorkingTreeDiff {
                rows: vec![DiffRow::File("x".to_owned())],
                untracked: vec!["new.txt".to_owned(), "also-new.rs".to_owned()],
                truncation: None,
            });

            let lines = block.render(&RenderCtx::new(80, &theme));

            let bold = theme.text().add_modifier(Modifier::BOLD);
            assert_eq!(
                lines,
                [
                    plain_row(&theme, "x", bold),
                    plain_row(&theme, "", theme.dim()),
                    plain_row(&theme, UNTRACKED_HEADING, theme.dim()),
                    plain_row(&theme, "  new.txt", theme.text()),
                    plain_row(&theme, "  also-new.rs", theme.text()),
                ]
            );
        }

        #[test]
        fn opens_with_the_untracked_heading_when_nothing_is_tracked() {
            let theme = Theme::default();
            let block = GitDiffBlock::new(WorkingTreeDiff {
                untracked: vec!["new.txt".to_owned()],
                ..WorkingTreeDiff::default()
            });

            let lines = block.render(&RenderCtx::new(80, &theme));

            assert_eq!(body_rows(&lines), [UNTRACKED_HEADING, "  new.txt"]);
        }

        #[test]
        fn dims_the_truncation_note_after_a_blank_row() {
            let theme = Theme::default();
            let diff = format!(
                "diff --git a/x b/x\n@@ -0,0 +1,6000 @@\n{}",
                "+added line\n".repeat(6000),
            );
            let diff = WorkingTreeDiff::new(&[&diff], "", "`git diff HEAD`");
            let note = diff
                .truncation
                .as_ref()
                .expect("6000 lines overrun")
                .to_string();
            let block = GitDiffBlock::new(diff);

            let lines = block.render(&RenderCtx::new(80, &theme));

            assert_eq!(
                lines[lines.len() - 2..],
                [
                    plain_row(&theme, "", theme.dim()),
                    plain_row(&theme, note, theme.dim()),
                ]
            );
        }
    }

    mod max_line_number {
        use super::*;

        #[test]
        fn takes_the_largest_number_on_either_side() {
            let rows = [removed(104, "a"), added(10, "b")];
            assert_eq!(max_line_number(&rows), 104);
        }

        #[test]
        fn is_zero_without_numbered_rows() {
            assert_eq!(max_line_number(&[DiffRow::File("x".to_owned())]), 0);
        }
    }
}
