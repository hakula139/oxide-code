//! Edit-tool diff body — `-` / `+` unified diff layout from [`DiffChunk`]s.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::RenderCtx;
use super::numbered_row;
use super::{TOOL_BORDER_CONT, border_style_for};
use crate::tool::{DiffChunk, DiffLine};

const MAX_DIFF_BODY_LINES: usize = 20;
const MAX_LOCATIONS_DISPLAYED: usize = 8;
const NO_CHANGE_MARKER: &str = "(no change)";

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    chunks: &[DiffChunk],
    replace_all: bool,
    replacements: usize,
    is_error: bool,
) {
    let border_style = border_style_for(ctx.theme, is_error);

    if !any_chunk_has_content(chunks) {
        out.push(Line::from(vec![
            Span::styled(TOOL_BORDER_CONT.to_owned(), border_style),
            Span::styled(NO_CHANGE_MARKER, ctx.theme.dim()),
        ]));
        return;
    }

    render_chunk_body(out, ctx, &chunks[0], border_style, MAX_DIFF_BODY_LINES);

    if chunks.len() > 1 {
        let locations: Vec<usize> = chunks.iter().filter_map(chunk_anchor_line).collect();
        render_locations_footer(out, ctx, &locations, border_style);
        return;
    }

    if replace_all && replacements > 1 {
        out.push(Line::from(vec![
            Span::styled(TOOL_BORDER_CONT.to_owned(), border_style),
            Span::styled(
                format!("{replacements} occurrences replaced"),
                ctx.theme.dim(),
            ),
        ]));
    }
}

fn render_chunk_body(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    chunk: &DiffChunk,
    border_style: Style,
    budget: usize,
) {
    let number_width = number_column_width(chunk);
    let del_renderer = numbered_row::Renderer::with_style(
        ctx,
        border_style,
        number_width,
        " - ",
        ctx.theme.error(),
        Some(ctx.theme.diff_del_row()),
    );
    let add_renderer = numbered_row::Renderer::with_style(
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
                let noun = if hidden == 1 { "line" } else { "lines" };
                out.push(Line::from(vec![
                    Span::styled(TOOL_BORDER_CONT.to_owned(), border_style),
                    Span::styled(format!("... {hidden} {noun} hidden"), ctx.theme.dim()),
                ]));
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
    locations: &[usize],
    border_style: Style,
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
    let label = if locations.len() == 1 {
        "line"
    } else {
        "lines"
    };
    out.push(Line::from(vec![
        Span::styled(TOOL_BORDER_CONT.to_owned(), border_style),
        Span::styled(
            format!("applied at {label} {list}{suffix}"),
            ctx.theme.dim(),
        ),
    ]));
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

/// Splits budget between sides; surplus flows from the smaller side to the larger.
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
    use crate::tool::DiffLine;
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

    fn rendered_texts(out: &[Line<'static>]) -> Vec<String> {
        out.iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn render_no_change_marker_when_all_chunks_empty_after_trim() {
        let theme = Theme::default();
        let mut out = Vec::new();
        render(
            &mut out,
            &ctx(&theme),
            &[chunk_of(&[], &[])],
            false,
            0,
            false,
        );
        assert_eq!(out.len(), 1);
        let text: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with("(no change)"), "unexpected body: {text:?}");
    }

    #[test]
    fn render_single_chunk_emits_body_without_locations_footer() {
        let theme = Theme::default();
        let mut out = Vec::new();
        render(
            &mut out,
            &ctx(&theme),
            &[chunk_of(&["foo"], &["bar"])],
            false,
            1,
            false,
        );
        let texts = rendered_texts(&out);
        assert!(
            texts.iter().any(|t| t.contains("1 - foo")),
            "missing del line with number column: {texts:?}",
        );
        assert!(
            texts.iter().any(|t| t.contains("1 + bar")),
            "missing add line with number column: {texts:?}",
        );
        assert!(
            !texts.iter().any(|t| t.contains("applied at")),
            "single-chunk render must not emit a locations footer: {texts:?}",
        );
    }

    #[test]
    fn render_multi_chunk_emits_one_body_plus_locations_footer() {
        let theme = Theme::default();
        let chunk_a = DiffChunk {
            old: vec![DiffLine {
                number: 12,
                text: "foo".to_owned(),
            }],
            new: vec![DiffLine {
                number: 12,
                text: "bar".to_owned(),
            }],
        };
        let chunk_b = DiffChunk {
            old: vec![DiffLine {
                number: 47,
                text: "foo".to_owned(),
            }],
            new: vec![DiffLine {
                number: 47,
                text: "bar".to_owned(),
            }],
        };
        let mut out = Vec::new();
        render(&mut out, &ctx(&theme), &[chunk_a, chunk_b], true, 2, false);
        let texts = rendered_texts(&out);
        assert_eq!(
            texts.iter().filter(|t| t.contains("12 - foo")).count(),
            1,
            "body must appear once at the first chunk's line: {texts:?}",
        );
        assert_eq!(
            texts.iter().filter(|t| t.contains("12 + bar")).count(),
            1,
            "body must appear once at the first chunk's line: {texts:?}",
        );
        assert!(
            !texts.iter().any(|t| t.contains("47 - foo")),
            "second chunk's body must not render: {texts:?}",
        );
        assert!(
            texts.iter().any(|t| t.contains("applied at lines 12, 47")),
            "missing locations footer: {texts:?}",
        );
        assert!(
            !texts.iter().any(|t| t.contains("occurrences replaced")),
            "multi-chunk render replaces the legacy count footer: {texts:?}",
        );
    }

    #[test]
    fn render_single_chunk_replace_all_keeps_legacy_count_footer() {
        let theme = Theme::default();
        let mut out = Vec::new();
        render(
            &mut out,
            &ctx(&theme),
            &[chunk_of(&["a"], &["b"])],
            true,
            7,
            false,
        );
        let texts = rendered_texts(&out);
        assert!(
            texts.iter().any(|t| t.contains("7 occurrences replaced")),
            "missing legacy count footer: {texts:?}",
        );
    }

    // ── render_locations_footer ──

    #[test]
    fn render_locations_footer_lists_each_line_number() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        render_locations_footer(&mut out, &ctx, &[12, 47, 200], theme.tool_border());
        assert_eq!(out.len(), 1);
        let text: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("applied at lines 12, 47, 200"),
            "footer text mismatch: {text:?}",
        );
    }

    #[test]
    fn render_locations_footer_caps_with_and_more_suffix_past_max() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let locations: Vec<usize> = (1..=10).collect();
        let mut out = Vec::new();
        render_locations_footer(&mut out, &ctx, &locations, theme.tool_border());
        let text: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("applied at lines 1, 2, 3, 4, 5, 6, 7, 8 and 2 more"),
            "footer should cap and append remainder: {text:?}",
        );
    }

    #[test]
    fn render_locations_footer_singular_label_for_one_location() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        render_locations_footer(&mut out, &ctx, &[42], theme.tool_border());
        let text: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("applied at line 42") && !text.contains("lines"),
            "singular label expected: {text:?}",
        );
    }

    #[test]
    fn render_locations_footer_empty_emits_nothing() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        render_locations_footer(&mut out, &ctx, &[], theme.tool_border());
        assert!(out.is_empty());
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
            "old fits in its budget; new uses head + ellipsis + tail",
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
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e, Entry::Ellipsis { hidden: 0 })),
            "Entry::Ellipsis {{ hidden: 0 }} must never be emitted",
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
        assert!(entries.iter().all(|e| matches!(e, Entry::Line { .. })));
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
