//! Edit-tool diff body: a unified `-` / `+` layout of the [`DiffChunk`]s.

use ratatui::style::Style;
use ratatui::text::Line;

use crate::tool::{DiffChunk, DiffLine};
use crate::tui::components::chat::blocks::RenderCtx;
use crate::tui::components::chat::blocks::rows::{NumberedRows, body_row};
use crate::util::text::plural;

const MAX_DIFF_BODY_LINES: usize = 20;
const MAX_LOCATIONS_DISPLAYED: usize = 8;
const NO_CHANGE_MARKER: &str = "(no change)";

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    chunks: &[DiffChunk],
) {
    if !any_chunk_has_content(chunks) {
        body_row(out, ctx, border_style, NO_CHANGE_MARKER, ctx.theme.dim());
        return;
    }

    render_chunk_body(out, ctx, border_style, &chunks[0], MAX_DIFF_BODY_LINES);

    if chunks.len() > 1 {
        let locations: Vec<usize> = chunks.iter().filter_map(chunk_anchor_line).collect();
        render_locations_footer(out, ctx, border_style, &locations);
    }
}

fn render_chunk_body(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    chunk: &DiffChunk,
    budget: usize,
) {
    let number_width = number_column_width(chunk);
    let del_renderer = NumberedRows::with_style(
        ctx,
        border_style,
        number_width,
        " - ",
        ctx.theme.error(),
        Some(ctx.theme.diff_del_row()),
    );
    let add_renderer = NumberedRows::with_style(
        ctx,
        border_style,
        number_width,
        " + ",
        ctx.theme.success(),
        Some(ctx.theme.diff_add_row()),
    );
    let text_style = ctx.theme.text();

    for entry in entries(&chunk.old, &chunk.new, budget) {
        match entry {
            Entry::Line {
                side: Side::Del,
                line,
            } => del_renderer.render(out, line.number, &line.text, text_style),
            Entry::Line {
                side: Side::Add,
                line,
            } => add_renderer.render(out, line.number, &line.text, text_style),
            Entry::Ellipsis { hidden } => {
                let noun = plural(hidden, "line", "lines");
                let text = format!("... {hidden} {noun} hidden");
                body_row(out, ctx, border_style, text, ctx.theme.dim());
            }
        }
    }
}

fn number_column_width(chunk: &DiffChunk) -> usize {
    let max_old = chunk.old.iter().map(|l| l.number).max().unwrap_or(0);
    let max_new = chunk.new.iter().map(|l| l.number).max().unwrap_or(0);
    max_old.max(max_new).to_string().len().max(1)
}

fn render_locations_footer(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    locations: &[usize],
) {
    if locations.is_empty() {
        return;
    }
    let shown = locations.len().min(MAX_LOCATIONS_DISPLAYED);
    let list = locations[..shown]
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if locations.len() > MAX_LOCATIONS_DISPLAYED {
        format!(" and {} more", locations.len() - MAX_LOCATIONS_DISPLAYED)
    } else {
        String::new()
    };
    let noun = plural(locations.len(), "line", "lines");
    let text = format!("applied at {noun} {list}{suffix}");
    body_row(out, ctx, border_style, text, ctx.theme.dim());
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
    if budget == 0 {
        out.push(Entry::Ellipsis {
            hidden: lines.len(),
        });
        return;
    }
    let head = budget - 1;
    let tail = 1;
    let hidden = lines.len() - head - tail;
    for line in &lines[..head] {
        out.push(Entry::Line { side, line });
    }
    if hidden > 0 {
        out.push(Entry::Ellipsis { hidden });
    }
    for line in &lines[lines.len() - tail..] {
        out.push(Entry::Line { side, line });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

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

    // ── render ──

    fn ctx(theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            width: 80,
            theme,
            show_thinking: true,
        }
    }

    /// Each row's text without the pad that carries a diff row's bg to the chat width.
    fn rows(out: &[Line<'static>]) -> Vec<String> {
        out.iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    #[test]
    fn render_marks_chunks_without_lines_as_no_change() {
        let theme = Theme::default();
        let mut out = Vec::new();

        render(
            &mut out,
            &ctx(&theme),
            theme.tool_border(),
            &[chunk_of(&[], &[])],
        );

        assert_eq!(rows(&out), ["▎   (no change)"]);
    }

    #[test]
    fn render_single_chunk_emits_body_without_locations_footer() {
        let theme = Theme::default();
        let mut out = Vec::new();

        render(
            &mut out,
            &ctx(&theme),
            theme.tool_border(),
            &[chunk_of(&["foo"], &["bar"])],
        );

        assert_eq!(rows(&out), ["▎   1 - foo", "▎   1 + bar"]);
    }

    #[test]
    fn render_multi_chunk_emits_one_body_plus_locations_footer() {
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
        let mut out = Vec::new();

        render(
            &mut out,
            &ctx(&theme),
            theme.tool_border(),
            &[chunk_at(12), chunk_at(47)],
        );

        assert_eq!(
            rows(&out),
            [
                "▎   12 - foo",
                "▎   12 + bar",
                "▎   applied at lines 12, 47",
            ]
        );
    }

    // ── render_locations_footer ──

    #[test]
    fn render_locations_footer_caps_with_and_more_suffix_past_max() {
        let theme = Theme::default();
        let locations: Vec<usize> = (1..=10).collect();
        let mut out = Vec::new();

        render_locations_footer(&mut out, &ctx(&theme), theme.tool_border(), &locations);

        assert_eq!(
            rows(&out),
            ["▎   applied at lines 1, 2, 3, 4, 5, 6, 7, 8 and 2 more"]
        );
    }

    #[test]
    fn render_locations_footer_singular_label_for_one_location() {
        let theme = Theme::default();
        let mut out = Vec::new();

        render_locations_footer(&mut out, &ctx(&theme), theme.tool_border(), &[42]);

        assert_eq!(rows(&out), ["▎   applied at line 42"]);
    }

    #[test]
    fn render_locations_footer_empty_emits_nothing() {
        let theme = Theme::default();
        let mut out = Vec::new();

        render_locations_footer(&mut out, &ctx(&theme), theme.tool_border(), &[]);

        assert_eq!(out, []);
    }

    #[test]
    fn render_locations_footer_wraps_under_the_bar_in_a_narrow_chat() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 24,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();

        render_locations_footer(&mut out, &ctx, theme.tool_border(), &[12, 47, 200]);

        assert_eq!(rows(&out), ["▎   applied at lines 12,", "▎   47, 200"]);
    }

    // ── any_chunk_has_content ──

    #[test]
    fn any_chunk_has_content_empty_chunks_is_false() {
        let chunks = vec![chunk_of(&[], &[])];
        assert!(!any_chunk_has_content(&chunks));
    }

    #[test]
    fn any_chunk_has_content_one_side_filled_is_true() {
        let chunks = vec![chunk_of(&["a"], &[])];
        assert!(any_chunk_has_content(&chunks));
        let chunks = vec![chunk_of(&[], &["b"])];
        assert!(any_chunk_has_content(&chunks));
    }

    #[test]
    fn any_chunk_has_content_empty_vec_is_false() {
        assert!(!any_chunk_has_content(&[]));
    }

    // ── chunk_anchor_line ──

    #[test]
    fn chunk_anchor_line_uses_old_side_first_line() {
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
    fn chunk_anchor_line_falls_back_to_new_side_for_pure_insertions() {
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
    fn chunk_anchor_line_is_none_when_both_sides_empty() {
        let chunk = chunk_of(&[], &[]);
        assert_eq!(chunk_anchor_line(&chunk), None);
    }

    // ── entries ──

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
    fn entries_under_budget_shows_all_lines() {
        let old = line_numbered(&["foo", "bar"]);
        let new = line_numbered(&["baz"]);
        let entries = entries(&old, &new, 10);
        assert_eq!(render_entries(&entries), vec!["- foo", "- bar", "+ baz"]);
    }

    #[test]
    fn entries_over_budget_splits_budget_between_sides() {
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
    fn entries_pure_deletion_over_budget_truncates_old_side() {
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
    fn entries_at_budget_boundary_shows_every_line() {
        let old = line_numbered(&["a", "b", "c"]);
        let new = line_numbered(&["x", "y"]);
        let entries = entries(&old, &new, 5);
        assert_eq!(
            render_entries(&entries),
            vec!["- a", "- b", "- c", "+ x", "+ y"],
        );
    }

    #[test]
    fn entries_both_sides_overflow_split_evenly() {
        let old = line_numbered(&["a0", "a1", "a2", "a3", "a4"]);
        let new = line_numbered(&["b0", "b1", "b2", "b3", "b4"]);
        let entries = entries(&old, &new, 5);
        assert_eq!(
            render_entries(&entries),
            vec!["- a0", "- a1", "... +2", "- a4", "+ b0", "... +3", "+ b4"],
        );
    }

    #[test]
    fn entries_zero_budget_collapses_each_side_to_ellipsis() {
        let old = line_numbered(&["a", "b"]);
        let new = line_numbered(&["x"]);
        let entries = entries(&old, &new, 0);
        assert_eq!(render_entries(&entries), vec!["... +2", "... +1"]);
    }

    #[test]
    fn entries_budget_two_emits_single_ellipsis_then_tail_per_side() {
        let old = line_numbered(&["a", "b", "c"]);
        let new = line_numbered(&["x", "y", "z"]);
        let entries = entries(&old, &new, 2);
        assert_eq!(
            render_entries(&entries),
            vec!["... +2", "- c", "... +2", "+ z"],
        );
    }

    // ── split_budget ──

    #[test]
    fn split_budget_under_budget_preserves_input_lengths() {
        assert_eq!(split_budget(2, 3, 10), (2, 3));
    }

    #[test]
    fn split_budget_smaller_side_surplus_flows_to_larger() {
        assert_eq!(split_budget(1, 20, 5), (1, 4));
        assert_eq!(split_budget(20, 1, 5), (4, 1));
    }

    #[test]
    fn split_budget_both_overflow_splits_with_odd_line_to_old() {
        assert_eq!(split_budget(10, 10, 5), (3, 2));
    }
}
