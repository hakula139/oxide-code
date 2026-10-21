//! Edit-tool diff body: a unified `-` / `+` layout of the [`DiffChunk`]s.

use ratatui::style::Style;

use ox_core::tool::{DiffChunk, DiffLine};
use ox_core::util::text::plural;

use super::overflow_footer;
use crate::components::chat::blocks::rows::{NumberedRows, body_row};
use crate::components::chat::blocks::{BlockRows, RenderCtx};

/// Body rows a diff shows in the compact view, split between its sides.
const MAX_DIFF_BODY_LINES: usize = 20;
/// Line numbers the locations footer lists in the compact view.
const MAX_LOCATIONS_DISPLAYED: usize = 8;
const NO_CHANGE_MARKER: &str = "(no change)";

pub(super) fn render(
    out: &mut BlockRows,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    chunks: &[DiffChunk],
) {
    if !any_chunk_has_content(chunks) {
        body_row(
            &mut out.lines,
            ctx,
            border_style,
            NO_CHANGE_MARKER,
            ctx.theme.dim(),
        );
        return;
    }

    render_chunk_body(out, ctx, border_style, &chunks[0]);

    if chunks.len() > 1 {
        let locations: Vec<usize> = chunks.iter().filter_map(chunk_anchor_line).collect();
        render_locations_footer(out, ctx, border_style, &locations);
    }
}

fn render_chunk_body(
    out: &mut BlockRows,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    chunk: &DiffChunk,
) {
    let max_number = max_line_number(chunk);
    let removed = NumberedRows::removed(ctx, border_style, max_number);
    let added = NumberedRows::added(ctx, border_style, max_number);
    let text_style = ctx.theme.text();
    let budget = ctx.cap(MAX_DIFF_BODY_LINES);

    for entry in entries(&chunk.old, &chunk.new, budget) {
        match entry {
            Entry::Line {
                side: Side::Del,
                line,
            } => removed.render(&mut out.lines, line.number, &line.text, text_style),
            Entry::Line {
                side: Side::Add,
                line,
            } => added.render(&mut out.lines, line.number, &line.text, text_style),
            Entry::Ellipsis { hidden } => {
                overflow_footer(out, ctx, border_style, hidden, "line", "lines", false);
            }
        }
    }
}

fn max_line_number(chunk: &DiffChunk) -> usize {
    chunk
        .old
        .iter()
        .chain(&chunk.new)
        .map(|line| line.number)
        .max()
        .unwrap_or(0)
}

/// Lists the lines each chunk applied at. A compact list that leaves some out ends with the
/// expand hint and expands the block when clicked.
fn render_locations_footer(
    out: &mut BlockRows,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    locations: &[usize],
) {
    if locations.is_empty() {
        return;
    }
    let shown = locations.len().min(ctx.cap(MAX_LOCATIONS_DISPLAYED));
    let list = locations[..shown]
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let noun = plural(locations.len(), "line", "lines");
    let text = format!("applied at {noun} {list}");
    let style = ctx.theme.dim();
    let hidden = locations.len() - shown;
    if hidden == 0 {
        body_row(&mut out.lines, ctx, border_style, text, style);
        return;
    }
    let text = ctx.with_expand_hint(&format!("{text} and {hidden} more"));
    out.push_expander(|lines| body_row(lines, ctx, border_style, text, style));
}

fn any_chunk_has_content(chunks: &[DiffChunk]) -> bool {
    chunks
        .iter()
        .any(|c| !c.old.is_empty() || !c.new.is_empty())
}

fn chunk_anchor_line(chunk: &DiffChunk) -> Option<usize> {
    chunk
        .old
        .first()
        .or_else(|| chunk.new.first())
        .map(|l| l.number)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Del,
    Add,
}

enum Entry<'a> {
    Line { side: Side, line: &'a DiffLine },
    Ellipsis { hidden: usize },
}

/// Lays out both sides within `budget` rows, which must be at least 2 so each side keeps a row.
fn entries<'a>(
    old_lines: &'a [DiffLine],
    new_lines: &'a [DiffLine],
    budget: usize,
) -> Vec<Entry<'a>> {
    let (old_budget, new_budget) = split_budget(old_lines.len(), new_lines.len(), budget);
    let mut out = Vec::with_capacity(old_lines.len() + new_lines.len() + 2);
    emit_side(&mut out, Side::Del, old_lines, old_budget);
    emit_side(&mut out, Side::Add, new_lines, new_budget);
    out
}

/// Splits the budget between the sides, and the smaller side's surplus goes to the larger one.
fn split_budget(old_len: usize, new_len: usize, budget: usize) -> (usize, usize) {
    if old_len + new_len <= budget {
        return (old_len, new_len);
    }
    let half = budget.div_ceil(2);
    if old_len <= half {
        return (old_len, budget - old_len);
    }
    if new_len <= budget - half {
        return (budget - new_len, new_len);
    }
    (half, budget - half)
}

fn emit_side<'a>(out: &mut Vec<Entry<'a>>, side: Side, lines: &'a [DiffLine], budget: usize) {
    if lines.is_empty() {
        return;
    }
    if lines.len() <= budget {
        for line in lines {
            out.push(Entry::Line { side, line });
        }
        return;
    }
    let head = budget - 1;
    let tail = 1;
    for line in &lines[..head] {
        out.push(Entry::Line { side, line });
    }
    out.push(Entry::Ellipsis {
        hidden: lines.len() - budget,
    });
    for line in &lines[lines.len() - tail..] {
        out.push(Entry::Line { side, line });
    }
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;

    use super::*;
    use crate::theme::Theme;

    fn line_numbered(side: &[&str]) -> Vec<DiffLine> {
        side.iter()
            .enumerate()
            .map(|(i, t)| DiffLine {
                number: i + 1,
                text: (*t).to_owned(),
            })
            .collect()
    }

    fn chunk_of(old: &[&str], new: &[&str]) -> DiffChunk {
        DiffChunk {
            old: line_numbered(old),
            new: line_numbered(new),
        }
    }

    fn expanded_ctx(theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            expanded: true,
            ..RenderCtx::new(80, theme)
        }
    }

    /// Each row's text without the pad that carries a diff row's bg to the chat width.
    fn rows(out: &[Line<'static>]) -> Vec<String> {
        out.iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    mod render {
        use super::*;

        #[test]
        fn marks_chunks_without_lines_as_no_change() {
            let theme = Theme::default();
            let mut out = BlockRows::default();

            render(
                &mut out,
                &RenderCtx::new(80, &theme),
                theme.tool_border(),
                &[chunk_of(&[], &[])],
            );

            assert_eq!(rows(&out.lines), ["▎   (no change)"]);
        }

        #[test]
        fn single_chunk_emits_body_without_locations_footer() {
            let theme = Theme::default();
            let mut out = BlockRows::default();

            render(
                &mut out,
                &RenderCtx::new(80, &theme),
                theme.tool_border(),
                &[chunk_of(&["foo"], &["bar"])],
            );

            assert_eq!(rows(&out.lines), ["▎   1 - foo", "▎   1 + bar"]);
        }

        #[test]
        fn multi_chunk_emits_one_body_plus_locations_footer() {
            let theme = Theme::default();
            let chunk_at = |number| DiffChunk {
                old: vec![DiffLine {
                    number,
                    text: "foo".to_owned(),
                }],
                new: vec![DiffLine {
                    number,
                    text: "bar".to_owned(),
                }],
            };
            let mut out = BlockRows::default();

            render(
                &mut out,
                &RenderCtx::new(80, &theme),
                theme.tool_border(),
                &[chunk_at(12), chunk_at(47)],
            );

            assert_eq!(
                rows(&out.lines),
                [
                    "▎   12 - foo",
                    "▎   12 + bar",
                    "▎   applied at lines 12, 47",
                ]
            );
        }
    }

    mod render_chunk_body {
        use super::*;

        #[test]
        fn caps_a_long_side_with_a_clickable_footer_until_expanded() {
            let theme = Theme::default();
            let added: Vec<String> = (1..=MAX_DIFF_BODY_LINES + 2)
                .map(|n| format!("line {n}"))
                .collect();
            let added: Vec<&str> = added.iter().map(String::as_str).collect();
            let chunk = chunk_of(&[], &added);
            let row = |n: usize| format!("▎   {n:>2} + line {n}");
            let mut compact = BlockRows::default();
            let mut expanded = BlockRows::default();

            render_chunk_body(
                &mut compact,
                &RenderCtx::new(80, &theme),
                theme.tool_border(),
                &chunk,
            );
            render_chunk_body(
                &mut expanded,
                &expanded_ctx(&theme),
                theme.tool_border(),
                &chunk,
            );

            let head = MAX_DIFF_BODY_LINES - 1;
            let mut expected: Vec<String> = (1..=head).map(row).collect();
            expected.push("▎   ... +2 lines".to_owned());
            expected.push(row(MAX_DIFF_BODY_LINES + 2));
            assert_eq!(rows(&compact.lines), expected);
            assert_eq!(compact.expander_rows(), [head]);
            assert_eq!(
                rows(&expanded.lines),
                (1..=MAX_DIFF_BODY_LINES + 2).map(row).collect::<Vec<_>>(),
            );
            assert_eq!(expanded.expander_rows(), Vec::<usize>::new());
        }
    }

    mod render_locations_footer {
        use super::*;

        #[test]
        fn caps_with_and_more_suffix_past_max() {
            let theme = Theme::default();
            let ctx = RenderCtx {
                expand_hint: Some("Ctrl+O to expand"),
                ..RenderCtx::new(100, &theme)
            };
            let locations: Vec<usize> = (1..=10).collect();
            let mut out = BlockRows::default();

            render_locations_footer(&mut out, &ctx, theme.tool_border(), &locations);

            assert_eq!(
                rows(&out.lines),
                ["▎   applied at lines 1, 2, 3, 4, 5, 6, 7, 8 and 2 more · Ctrl+O to expand"]
            );
            assert_eq!(out.expander_rows(), [0]);
        }

        #[test]
        fn lists_every_location_when_expanded() {
            let theme = Theme::default();
            let locations: Vec<usize> = (1..=10).collect();
            let mut out = BlockRows::default();

            render_locations_footer(
                &mut out,
                &expanded_ctx(&theme),
                theme.tool_border(),
                &locations,
            );

            assert_eq!(
                rows(&out.lines),
                ["▎   applied at lines 1, 2, 3, 4, 5, 6, 7, 8, 9, 10"]
            );
            assert_eq!(out.expander_rows(), Vec::<usize>::new());
        }

        #[test]
        fn singular_label_for_one_location() {
            let theme = Theme::default();
            let mut out = BlockRows::default();

            render_locations_footer(
                &mut out,
                &RenderCtx::new(80, &theme),
                theme.tool_border(),
                &[42],
            );

            assert_eq!(rows(&out.lines), ["▎   applied at line 42"]);
        }

        #[test]
        fn empty_emits_nothing() {
            let theme = Theme::default();
            let mut out = BlockRows::default();

            render_locations_footer(
                &mut out,
                &RenderCtx::new(80, &theme),
                theme.tool_border(),
                &[],
            );

            assert_eq!(out.lines, []);
        }

        #[test]
        fn wraps_under_the_bar_in_a_narrow_chat() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(24, &theme);
            let mut out = BlockRows::default();

            render_locations_footer(&mut out, &ctx, theme.tool_border(), &[12, 47, 200]);

            assert_eq!(
                rows(&out.lines),
                ["▎   applied at lines 12,", "▎   47, 200"]
            );
        }
    }

    mod any_chunk_has_content {
        use super::*;

        #[test]
        fn empty_chunks_is_false() {
            let chunks = vec![chunk_of(&[], &[])];
            assert!(!any_chunk_has_content(&chunks));
        }

        #[test]
        fn one_side_filled_is_true() {
            let chunks = vec![chunk_of(&["a"], &[])];
            assert!(any_chunk_has_content(&chunks));
            let chunks = vec![chunk_of(&[], &["b"])];
            assert!(any_chunk_has_content(&chunks));
        }

        #[test]
        fn empty_vec_is_false() {
            assert!(!any_chunk_has_content(&[]));
        }
    }

    mod chunk_anchor_line {
        use super::*;

        #[test]
        fn uses_old_side_first_line() {
            let chunk = DiffChunk {
                old: vec![
                    DiffLine {
                        number: 47,
                        text: "a".to_owned(),
                    },
                    DiffLine {
                        number: 48,
                        text: "b".to_owned(),
                    },
                ],
                new: vec![DiffLine {
                    number: 47,
                    text: "X".to_owned(),
                }],
            };
            assert_eq!(chunk_anchor_line(&chunk), Some(47));
        }

        #[test]
        fn falls_back_to_new_side_for_pure_insertions() {
            let chunk = DiffChunk {
                old: vec![],
                new: vec![DiffLine {
                    number: 99,
                    text: "added".to_owned(),
                }],
            };
            assert_eq!(chunk_anchor_line(&chunk), Some(99));
        }

        #[test]
        fn leaves_an_empty_chunk_unanchored() {
            let chunk = chunk_of(&[], &[]);
            assert_eq!(chunk_anchor_line(&chunk), None);
        }
    }

    mod entries {
        use super::*;

        fn render_entries(entries: &[Entry<'_>]) -> Vec<String> {
            entries
                .iter()
                .map(|e| match e {
                    Entry::Line { side, line } => {
                        let sign = match side {
                            Side::Del => '-',
                            Side::Add => '+',
                        };
                        format!("{sign} {}", line.text)
                    }
                    Entry::Ellipsis { hidden } => format!("... +{hidden}"),
                })
                .collect()
        }

        #[test]
        fn under_budget_shows_all_lines() {
            let old = line_numbered(&["foo", "bar"]);
            let new = line_numbered(&["baz"]);
            let entries = entries(&old, &new, 10);
            assert_eq!(render_entries(&entries), vec!["- foo", "- bar", "+ baz"]);
        }

        #[test]
        fn over_budget_splits_budget_between_sides() {
            let old = line_numbered(&["a", "b"]);
            let new = line_numbered(&["c", "d", "e", "f", "g"]);
            let entries = entries(&old, &new, 5);
            assert_eq!(
                render_entries(&entries),
                vec!["- a", "- b", "+ c", "+ d", "... +2", "+ g"],
                "old fits in its budget, while new uses head + ellipsis + tail",
            );
        }

        #[test]
        fn pure_deletion_over_budget_truncates_old_side() {
            let old = line_numbered(&["a", "b", "c", "d", "e", "f", "g"]);
            let new: Vec<DiffLine> = Vec::new();
            let entries = entries(&old, &new, 5);
            assert_eq!(
                render_entries(&entries),
                vec!["- a", "- b", "- c", "- d", "... +2", "- g"],
                "old gets the full budget when new is empty",
            );
        }

        #[test]
        fn at_budget_boundary_shows_every_line() {
            let old = line_numbered(&["a", "b", "c"]);
            let new = line_numbered(&["x", "y"]);
            let entries = entries(&old, &new, 5);
            assert_eq!(
                render_entries(&entries),
                vec!["- a", "- b", "- c", "+ x", "+ y"],
            );
        }

        #[test]
        fn both_sides_overflow_split_evenly() {
            let old = line_numbered(&["a0", "a1", "a2", "a3", "a4"]);
            let new = line_numbered(&["b0", "b1", "b2", "b3", "b4"]);
            let entries = entries(&old, &new, 5);
            assert_eq!(
                render_entries(&entries),
                vec!["- a0", "- a1", "... +2", "- a4", "+ b0", "... +3", "+ b4"],
            );
        }

        #[test]
        fn budget_two_emits_single_ellipsis_then_tail_per_side() {
            let old = line_numbered(&["a", "b", "c"]);
            let new = line_numbered(&["x", "y", "z"]);
            let entries = entries(&old, &new, 2);
            assert_eq!(
                render_entries(&entries),
                vec!["... +2", "- c", "... +2", "+ z"],
            );
        }
    }

    mod split_budget {
        use super::*;

        #[test]
        fn under_budget_preserves_input_lengths() {
            assert_eq!(split_budget(2, 3, 10), (2, 3));
        }

        #[test]
        fn smaller_side_surplus_flows_to_larger() {
            assert_eq!(split_budget(1, 20, 5), (1, 4));
            assert_eq!(split_budget(20, 1, 5), (4, 1));
        }

        #[test]
        fn both_overflow_splits_with_odd_line_to_old() {
            assert_eq!(split_budget(10, 10, 5), (3, 2));
        }
    }
}
