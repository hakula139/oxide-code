//! `edit` tool body — `-` old / `+` new unified diff. Operates on the
//! pre-trimmed [`DiffChunk`]s the producer emits in
//! `crate::tool::edit`, so this module is concerned only with layout
//! (entry stream, budget split, location footer) — not with finding
//! match positions or trimming common anchor lines.
//!
//! Diff rows ride the shared [`numbered_row::Renderer`] with the `- `
//! / `+ ` sign as separator and a Catppuccin red / green row bg, so the
//! line-number column aligns with `read` / `grep` while the bg tint
//! reads as a contiguous GitHub-style block per row.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::RenderCtx;
use super::numbered_row;
use super::{TOOL_BORDER_CONT, border_style_for};
use crate::tool::{DiffChunk, DiffLine};

/// Maximum lines of diff body (combined `-` + `+`) shown before
/// truncation. Set higher than text output because diffs pair every
/// old line with its new counterpart, doubling the natural line
/// count before the user learns anything new — a 6-line → 7-line
/// function replacement (common in real edits) already sits at 13
/// combined lines. 20 covers roughly the 95th-percentile edit
/// without hiding the change's middle behind an ellipsis.
const MAX_DIFF_BODY_LINES: usize = 20;

/// Maximum line numbers listed inline in the "applied at lines ..."
/// footer for a deduplicated multi-chunk render. Beyond this, the
/// list collapses to "...and N more locations" so a 50-hit
/// `replace_all` doesn't produce a 50-number footer.
const MAX_LOCATIONS_DISPLAYED: usize = 8;

/// Body shown when the diff resolves to a no-op (defensive — the live
/// producer rejects no-op edits, so this only triggers from corrupt
/// resumed transcripts).
const NO_CHANGE_MARKER: &str = "(no change)";

/// Renders the body of an edit-tool diff result.
///
/// Each [`DiffChunk`] is shown as `- ` (red) old lines and `+ ` (green)
/// new lines; pure insertions / deletions render only the non-empty
/// half. Long bodies are head + ellipsis + tail truncated per side.
///
/// Live producer always emits chunks of identical trimmed content, so:
///
/// - Single chunk → body alone, with a legacy
///   `{N} occurrences replaced` footer when a resumed transcript has
///   `replace_all` and N > 1 but no structured chunks.
/// - Multi-chunk → body shown once, plus an "applied at lines X, Y, ..."
///   footer naming each site.
pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    chunks: &[DiffChunk],
    replace_all: bool,
    replacements: usize,
    is_error: bool,
) {
    let border_style = border_style_for(ctx.theme, is_error);

    // Defends corrupt resumed transcripts — the live producer rejects
    // no-op edits, so empty chunks only reach the renderer via bad
    // JSONL.
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

    // Resume fallback: single synthesized chunk with N>1 replacements
    // — keep the legacy footer so the count stays visible without
    // inventing fake locations.
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

/// Renders a single chunk's `- ` / `+ ` entries under `border_style`,
/// using `budget` to cap combined line count. Each side gets its own
/// [`numbered_row::Renderer`] preconfigured with the matching sign and
/// row bg; ellipsis rows render neutrally (no number, no bg).
fn render_chunk_body(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    chunk: &DiffChunk,
    border_style: Style,
    budget: usize,
) {
    let number_width = number_column_width(chunk);
    // Separator width matches read / grep's `" │ "` (3 cols) so the
    // text column lands at the same offset across every tool body.
    // The leading space gives the number column breathing room — `1 -`
    // reads more naturally than `1-` for single-digit line numbers.
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
    // White text on the muted bg keeps content readable; the saturated
    // sign in the separator slot still carries the side semantic.
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
                    // Inside a diff body the `+` glyph already means
                    // "added line" on an adjacent row; using `+N` on
                    // an ellipsis would smuggle addition semantics
                    // into a collapsed region that might be a pure
                    // deletion. Render as neutral "N lines hidden".
                    Span::styled(format!("... {hidden} {noun} hidden"), ctx.theme.dim()),
                ]));
            }
        }
    }
}

/// Width of the line-number column for a chunk — the widest line
/// number across both sides, with a minimum of 1 so the column never
/// collapses to zero (which would push the separator flush against
/// the bar prefix).
fn number_column_width(chunk: &DiffChunk) -> usize {
    let max_old = chunk.old.iter().map(|l| l.number).max().unwrap_or(0);
    let max_new = chunk.new.iter().map(|l| l.number).max().unwrap_or(0);
    max_old.max(max_new).to_string().len().max(1)
}

/// Renders the deduplicated multi-chunk footer naming every location.
/// `locations` carries one anchor line per chunk (old-side first line,
/// falling back to new-side for pure insertions). Caps at
/// [`MAX_LOCATIONS_DISPLAYED`] with a "...and N more locations" suffix.
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

/// Returns true if any chunk has a non-empty `old` or `new` side.
/// Used as the empty-render guard so chunks where both sides trimmed
/// to nothing (no-op edits) still surface a `(no change)` marker.
fn any_chunk_has_content(chunks: &[DiffChunk]) -> bool {
    chunks
        .iter()
        .any(|c| !c.old.is_empty() || !c.new.is_empty())
}

/// Anchor line number for a chunk in the original file — old-side
/// first line, falling back to new-side first line for pure
/// insertions (no `-` content). Used by the locations footer to
/// describe where each `replace_all` match landed.
fn chunk_anchor_line(chunk: &DiffChunk) -> Option<usize> {
    chunk
        .old
        .first()
        .or_else(|| chunk.new.first())
        .map(|l| l.number)
}

/// Which side of the diff a [`Entry::Line`] belongs to. Drives
/// renderer dispatch (sign, color, bg) without burdening every entry
/// with a copy of the per-side style state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Side {
    Del,
    Add,
}

/// A single rendered entry in a diff body.
enum Entry<'a> {
    Line { side: Side, line: &'a DiffLine },
    Ellipsis { hidden: usize },
}

/// Builds the ordered entry stream for a diff body, capping the total
/// line count at `budget`. Each side is allotted its fair share via
/// [`split_budget`] and renders through [`emit_side`], which preserves
/// the first and last line of the side while collapsing the middle
/// into a single `Ellipsis`.
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

/// Distributes `budget` between the `-` and `+` sides. Each side gets
/// capped at its own length; any surplus on the smaller side flows to
/// the other so single-line edits aren't starved. When both sides
/// overflow an even split, the budget splits (with an odd line going
/// to `old` since deletions anchor the diff).
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

/// Emits `lines` entries under `side`, capped at `budget`. Over
/// budget, shows the first `budget - 1` lines, a single `Ellipsis` for
/// the collapsed middle, then the final line — so both the leading
/// and trailing shape stay visible. `budget == 0` collapses the entire
/// side into one `Ellipsis`. `Ellipsis { hidden: 0 }` is never emitted
/// — a bare `... +0 lines` footer would be a contradiction.
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

    /// Helper: build a single-chunk diff body with line numbers
    /// starting at 1, mirroring how `synthesize_chunk` shapes resume-
    /// fallback chunks. Tests reach for this rather than constructing
    /// `DiffChunk` literals to keep focus on the chunk-level logic
    /// under test.
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

    // ── any_chunk_has_content ──

    #[test]
    fn any_chunk_has_content_empty_chunks_returns_false() {
        // No-change guard input: every chunk has both sides empty
        // (e.g., a malformed transcript reaching the renderer).
        // Used to short-circuit to the "(no change)" marker.
        let chunks = vec![chunk_of(&[], &[])];
        assert!(!any_chunk_has_content(&chunks));
    }

    #[test]
    fn any_chunk_has_content_one_side_filled_returns_true() {
        let chunks = vec![chunk_of(&["a"], &[])];
        assert!(any_chunk_has_content(&chunks));
        let chunks = vec![chunk_of(&[], &["b"])];
        assert!(any_chunk_has_content(&chunks));
    }

    #[test]
    fn any_chunk_has_content_empty_vec_returns_false() {
        // No chunks at all — cannot happen in the live path but the
        // renderer must still degrade gracefully.
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
        // Pure tail insertion: producer trim collapsed the old anchor.
        // Anchor line falls back to the new side so the locations
        // footer still names a meaningful position.
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
    fn chunk_anchor_line_returns_none_when_both_sides_empty() {
        let chunk = chunk_of(&[], &[]);
        assert_eq!(chunk_anchor_line(&chunk), None);
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
        // Exactly one row — a regression that double-emitted would
        // pass a `contains` check.
        assert_eq!(out.len(), 1);
        let text: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("applied at lines 12, 47, 200"),
            "footer text mismatch: {text:?}",
        );
    }

    #[test]
    fn render_locations_footer_caps_with_and_more_suffix_past_max() {
        // Past `MAX_LOCATIONS_DISPLAYED` (8), the footer truncates and
        // adds "and N more" so a 50-hit `replace_all` doesn't sprawl.
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
        // The singular branch is unreachable in production (the dedup
        // path needs `chunks.len() > 1`), but the helper must produce
        // the right grammar if a future caller ever passes a single
        // location.
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
        // Defensive guard: if every chunk lacked an anchor line (both
        // sides empty after trim), the footer must skip rather than
        // emit a malformed "applied at lines " row.
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
        // Resume / corrupt-input guard: a chunk list that trims to
        // nothing on every entry surfaces as a dim "(no change)" row,
        // not an empty body the user would read as a missing render.
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
        // Live single-edit case: one chunk, no `replace_all` count
        // footer. The body alone reaches the user.
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
        // Require the number column too — `contains("- foo")` alone
        // would still pass if a regression dropped the gutter.
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
        // Live `replace_all` case: N chunks of identical trimmed
        // content collapse into one body (rendered once) plus an
        // "applied at lines ..." footer naming each match site.
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
        // Body rendered once at the *first* chunk's line number — the
        // other chunks contribute only to the locations footer. A
        // regression that re-rendered every chunk would emit two `12 -`
        // rows or one `12 -` and one `47 -`.
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
        // Resume fallback: JSONL pre-`diff_chunks` recorded a
        // single-chunk synthesized view with `replacements > 1`. The
        // count footer is the only signal of the multi-match nature.
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

    // ── entries ──

    /// Render an `Entry` stream as `Vec<String>` so assertions read
    /// like the actual rendered diff body.
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
        // Symmetric policy: when combined length exceeds the budget,
        // each side gets a fair share with head + ellipsis + tail.
        // Here old (2) fits entirely, so its surplus shifts to new (5
        // lines under a budget of 5 - 2 = 3: 2 head + tail? no —
        // head = budget - 1 = 2, tail = 1, hidden = 5 - 3 = 2).
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
        // Regression: previously `old` had no budget cap, so a 20-line
        // pure deletion rendered 20 `-` lines followed by a bogus
        // `... +0 lines` footer. Now old is budgeted like any side
        // and the zero-hidden Ellipsis is suppressed.
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
        // old.len() + new.len() == budget: both sides render in full,
        // no ellipsis — the off-by-one guard against a spurious footer.
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
        // When neither side fits in an even split, each gets half the
        // budget — head-ellipsis-tail on both. Odd leftover goes to old.
        let old = line_numbered(&["a0", "a1", "a2", "a3", "a4"]);
        let new = line_numbered(&["b0", "b1", "b2", "b3", "b4"]);
        let entries = entries(&old, &new, 5);
        assert_eq!(
            render_entries(&entries),
            // old budget 3 (half of 5 rounded up): head 2, tail 1,
            // hidden 2; new budget 2: head 1, tail 1, hidden 3.
            vec!["- a0", "- a1", "... +2", "- a4", "+ b0", "... +3", "+ b4"],
        );
    }

    #[test]
    fn entries_zero_budget_collapses_each_side_to_ellipsis() {
        // Pathological budget == 0 must still terminate cleanly — each
        // non-empty side emits a single Ellipsis rather than panicking
        // on an underflowed head / tail split.
        let old = line_numbered(&["a", "b"]);
        let new = line_numbered(&["x"]);
        let entries = entries(&old, &new, 0);
        assert_eq!(render_entries(&entries), vec!["... +2", "... +1"]);
    }

    #[test]
    fn entries_budget_two_emits_single_ellipsis_then_tail_per_side() {
        // `split_budget(n, n, 2)` → `(1, 1)`, which exercises the
        // `budget == 1` branch in `emit_side` — previously untested
        // via `entries`. Head = 0, Ellipsis{hidden: n-1}, tail.
        // Regresses if a mutation flips `head = budget - 1` to
        // `head = budget` (then the ellipsis would vanish).
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
    fn split_budget_under_budget_returns_input_lengths() {
        assert_eq!(split_budget(2, 3, 10), (2, 3));
    }

    #[test]
    fn split_budget_smaller_side_surplus_flows_to_larger() {
        // old fits entirely; remaining budget (4) goes to new.
        assert_eq!(split_budget(1, 20, 5), (1, 4));
        // Symmetric case: new fits, old absorbs the surplus.
        assert_eq!(split_budget(20, 1, 5), (4, 1));
    }

    #[test]
    fn split_budget_both_overflow_splits_with_odd_line_to_old() {
        // Budget 5: half rounds up to 3 for old, 2 for new — the
        // extra line anchors the deletion side since `-` is the
        // "what used to be here" context readers look for first.
        assert_eq!(split_budget(10, 10, 5), (3, 2));
    }
}
