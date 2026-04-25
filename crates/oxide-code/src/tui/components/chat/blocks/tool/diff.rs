//! `edit` tool body — `-` old / `+` new unified diff with boundary
//! trimming and per-side budget truncation.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::super::RenderCtx;
use super::{
    MAX_TOOL_OUTPUT_LINE_BYTES, STATUS_LINE_CONT, border_continuation_prefix, border_style_for,
    truncate_to_bytes,
};
use crate::tui::wrap::{expand_tabs, wrap_line};

/// Maximum lines of diff body (combined `-` + `+`) shown before
/// truncation. Set higher than text output because diffs pair every
/// old line with its new counterpart, doubling the natural line
/// count before the user learns anything new — a 6-line → 7-line
/// function replacement (common in real edits) already sits at 13
/// combined lines. 20 covers roughly the 95th-percentile edit
/// without hiding the change's middle behind an ellipsis.
const MAX_DIFF_BODY_LINES: usize = 20;

/// Continuation prefix for wrapped diff-body lines. Hangs under the
/// text column of a `- ` / `+ ` line (col 6) — two columns right of
/// [`STATUS_LINE_CONT`] — so wrapped content keeps reading as a
/// continuation of the same diff line rather than dropping back to
/// the outer body indent.
const DIFF_LINE_CONT: &str = "▎     ";

/// Renders a unified-diff-style body: every line of `old` prefixed with
/// `- ` in red, followed by every line of `new` prefixed with `+ ` in
/// green.
///
/// Lines that are identical at the leading or trailing boundary are
/// dropped — a pure tail insertion like `fn foo()` → `fn foo()\n  42`
/// renders as a single `+ 42`, not as `- fn foo()` / `+ fn foo()` /
/// `+ 42`. Empty strings on either side — a pure deletion or insertion
/// — render only the non-empty half. Trailing blank lines (a common
/// artefact of line-ended old/new strings) are stripped so the
/// rendered body matches what the user sees when they open the file.
///
/// Long diffs are truncated per side (`MAX_DIFF_BODY_LINES` split
/// between `old` and `new`) with a head + ellipsis + tail shape so the
/// first and last lines of each side stay visible.
pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    old: &str,
    new: &str,
    replace_all: bool,
    replacements: usize,
    is_error: bool,
) {
    let border_style = border_style_for(ctx.theme, is_error);
    let diff_cont_prefix = border_continuation_prefix(DIFF_LINE_CONT, border_style);
    let width = usize::from(ctx.width);

    let old_lines = split_side(old);
    let new_lines = split_side(new);
    let (old_lines, new_lines) = trim_common_boundaries(&old_lines, &new_lines);
    if old_lines.is_empty() && new_lines.is_empty() {
        // Both sides collapsed to empty — `old == new` entirely.
        // `edit_file` rejects no-op edits live, but a resumed
        // transcript (or a malformed input) can reach this branch.
        // Emit a single dim marker so the result row doesn't render
        // as a bare success header with no body, which reads as
        // \"edit applied, diff scrolled off\" instead of \"no change\".
        out.push(Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled("(no change)", ctx.theme.dim()),
        ]));
        return;
    }

    let entries = entries(
        old_lines,
        new_lines,
        MAX_DIFF_BODY_LINES,
        ctx.theme.error(),
        ctx.theme.success(),
    );
    for entry in entries {
        match entry {
            Entry::Line { sign, text, style } => {
                let expanded = expand_tabs(text);
                let display_text = truncate_to_bytes(&expanded, MAX_TOOL_OUTPUT_LINE_BYTES);
                let line = Line::from(vec![
                    Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
                    Span::styled(format!("{sign} "), style),
                    Span::styled(display_text, style),
                ]);
                out.extend(wrap_line(
                    line,
                    width,
                    DIFF_LINE_CONT.width(),
                    Some(&diff_cont_prefix),
                ));
            }
            Entry::Ellipsis { hidden } => {
                let noun = if hidden == 1 { "line" } else { "lines" };
                out.push(Line::from(vec![
                    Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
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

    if replace_all && replacements > 1 {
        out.push(Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            // Mirror the model-facing "Replaced N occurrences in ..."
            // wording rather than inventing a new shape. Past tense
            // matches the ✓/✗ indicator in the status row above.
            Span::styled(
                format!("{replacements} occurrences replaced"),
                ctx.theme.dim(),
            ),
        ]));
    }
}

/// Splits one side of a diff into displayable lines. A trailing
/// newline is dropped so `"a\nb\n"` renders as two lines, not three
/// with a blank tail — common when `old_string` / `new_string` span
/// whole lines. Empty input yields an empty `Vec`, which the caller
/// treats as "render nothing for this side".
fn split_side(side: &str) -> Vec<&str> {
    side.lines().collect()
}

/// Drops lines that are identical at the leading or trailing boundary
/// of `old` and `new`. Single-line edits are unaffected — line 0 of
/// each side differs — but pure tail insertions like
/// `fn foo()` → `fn foo()\n    return 42;` collapse the anchor line
/// so only the real delta renders. Returns borrowed slice views into
/// the input; the callers already own the `Vec`s.
fn trim_common_boundaries<'a, 'b>(
    old: &'a [&'b str],
    new: &'a [&'b str],
) -> (&'a [&'b str], &'a [&'b str]) {
    let max_prefix = old.len().min(new.len());
    let mut prefix = 0;
    while prefix < max_prefix && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let mut suffix = 0;
    while suffix < max_suffix && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
        suffix += 1;
    }
    (
        &old[prefix..old.len() - suffix],
        &new[prefix..new.len() - suffix],
    )
}

/// A single rendered entry in a diff body.
enum Entry<'a> {
    Line {
        sign: char,
        text: &'a str,
        style: Style,
    },
    Ellipsis {
        hidden: usize,
    },
}

/// Builds the ordered entry stream for a diff body, capping the total
/// line count at `budget`. Each side is allotted its fair share via
/// [`split_budget`] and renders through [`emit_side`], which preserves
/// the first and last line of the side while collapsing the middle
/// into a single `Ellipsis`.
fn entries<'a>(
    old_lines: &[&'a str],
    new_lines: &[&'a str],
    budget: usize,
    del_style: Style,
    add_style: Style,
) -> Vec<Entry<'a>> {
    let (old_budget, new_budget) = split_budget(old_lines.len(), new_lines.len(), budget);
    let mut out = Vec::with_capacity(old_lines.len() + new_lines.len() + 2);
    emit_side(&mut out, '-', old_lines, old_budget, del_style);
    emit_side(&mut out, '+', new_lines, new_budget, add_style);
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

/// Emits `lines` entries under `sign`, capped at `budget`. Over
/// budget, shows the first `budget - 1` lines, a single `Ellipsis` for
/// the collapsed middle, then the final line — so both the leading
/// and trailing shape stay visible. `budget == 0` collapses the entire
/// side into one `Ellipsis`. `Ellipsis { hidden: 0 }` is never emitted
/// — a bare `... +0 lines` footer would be a contradiction.
fn emit_side<'a>(
    out: &mut Vec<Entry<'a>>,
    sign: char,
    lines: &[&'a str],
    budget: usize,
    style: Style,
) {
    if lines.is_empty() {
        return;
    }
    if lines.len() <= budget {
        for line in lines {
            out.push(Entry::Line {
                sign,
                text: line,
                style,
            });
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
        out.push(Entry::Line {
            sign,
            text: line,
            style,
        });
    }
    if hidden > 0 {
        out.push(Entry::Ellipsis { hidden });
    }
    for line in &lines[lines.len() - tail..] {
        out.push(Entry::Line {
            sign,
            text: line,
            style,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── split_side ──

    #[test]
    fn split_side_empty_yields_empty_slice() {
        assert!(split_side("").is_empty());
    }

    #[test]
    fn split_side_drops_trailing_newline() {
        // `"a\nb\n"` → two displayable lines, not three with a blank
        // tail. Needed because `old_string` / `new_string` often end
        // in newlines when the edit spans full lines.
        assert_eq!(split_side("a\nb\n"), vec!["a", "b"]);
    }

    // ── trim_common_boundaries ──

    #[test]
    fn trim_common_boundaries_drops_matching_prefix_and_suffix() {
        let old = vec!["a", "b", "c", "d"];
        let new = vec!["a", "X", "Y", "d"];
        let (o, n) = trim_common_boundaries(&old, &new);
        assert_eq!(o, &["b", "c"]);
        assert_eq!(n, &["X", "Y"]);
    }

    #[test]
    fn trim_common_boundaries_pure_tail_insertion_strips_anchor() {
        // The canonical Edit case: `old` is an anchor line,
        // `new` is the anchor plus added lines below. The diff
        // should show only the added lines, not `- anchor / + anchor`.
        let old = vec!["fn foo()"];
        let new = vec!["fn foo()", "    return 42;"];
        let (o, n) = trim_common_boundaries(&old, &new);
        assert!(o.is_empty(), "anchor line dropped on old side");
        assert_eq!(n, &["    return 42;"]);
    }

    #[test]
    fn trim_common_boundaries_pure_head_insertion_strips_anchor() {
        let old = vec!["fn foo()"];
        let new = vec!["// new doc", "fn foo()"];
        let (o, n) = trim_common_boundaries(&old, &new);
        assert!(o.is_empty());
        assert_eq!(n, &["// new doc"]);
    }

    #[test]
    fn trim_common_boundaries_single_line_edit_is_untouched() {
        // Line 0 differs on both sides — no boundary to trim. This
        // preserves the snapshot for single-line word changes.
        let old = vec!["fn foo() {}"];
        let new = vec!["fn bar() {}"];
        let (o, n) = trim_common_boundaries(&old, &new);
        assert_eq!(o, &["fn foo() {}"]);
        assert_eq!(n, &["fn bar() {}"]);
    }

    #[test]
    fn trim_common_boundaries_fully_identical_yields_empty_slices() {
        // Not reachable via EditTool (no-op edits are rejected), but
        // the helper must still terminate and return empty slices.
        let old = vec!["a", "b"];
        let new = vec!["a", "b"];
        let (o, n) = trim_common_boundaries(&old, &new);
        assert!(o.is_empty());
        assert!(n.is_empty());
    }

    #[test]
    fn trim_common_boundaries_empty_input_is_idempotent() {
        let empty: Vec<&str> = Vec::new();
        let (o, n) = trim_common_boundaries(&empty, &empty);
        assert!(o.is_empty());
        assert!(n.is_empty());
    }

    // ── entries ──

    /// Render an `Entry` stream as `Vec<String>` so assertions read
    /// like the actual rendered diff body.
    fn render_entries(entries: &[Entry<'_>]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                Entry::Line { sign, text, .. } => format!("{sign} {text}"),
                Entry::Ellipsis { hidden } => format!("... +{hidden}"),
            })
            .collect()
    }

    #[test]
    fn entries_under_budget_shows_all_lines() {
        let old = vec!["foo", "bar"];
        let new = vec!["baz"];
        let entries = entries(&old, &new, 10, Style::default(), Style::default());
        assert_eq!(render_entries(&entries), vec!["- foo", "- bar", "+ baz"]);
    }

    #[test]
    fn entries_over_budget_splits_budget_between_sides() {
        // Symmetric policy: when combined length exceeds the budget,
        // each side gets a fair share with head + ellipsis + tail.
        // Here old (2) fits entirely, so its surplus shifts to new (5
        // lines under a budget of 5 - 2 = 3: 2 head + tail? no —
        // head = budget - 1 = 2, tail = 1, hidden = 5 - 3 = 2).
        let old = vec!["a", "b"];
        let new = vec!["c", "d", "e", "f", "g"];
        let entries = entries(&old, &new, 5, Style::default(), Style::default());
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
        let old = vec!["a", "b", "c", "d", "e", "f", "g"];
        let new: Vec<&str> = Vec::new();
        let entries = entries(&old, &new, 5, Style::default(), Style::default());
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
        let old = vec!["a", "b", "c"];
        let new = vec!["x", "y"];
        let entries = entries(&old, &new, 5, Style::default(), Style::default());
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
        let old = vec!["a0", "a1", "a2", "a3", "a4"];
        let new = vec!["b0", "b1", "b2", "b3", "b4"];
        let entries = entries(&old, &new, 5, Style::default(), Style::default());
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
        let old = vec!["a", "b"];
        let new = vec!["x"];
        let entries = entries(&old, &new, 0, Style::default(), Style::default());
        assert_eq!(render_entries(&entries), vec!["... +2", "... +1"]);
    }

    #[test]
    fn entries_budget_two_emits_single_ellipsis_then_tail_per_side() {
        // `split_budget(n, n, 2)` → `(1, 1)`, which exercises the
        // `budget == 1` branch in `emit_side` — previously untested
        // via `entries`. Head = 0, Ellipsis{hidden: n-1}, tail.
        // Regresses if a mutation flips `head = budget - 1` to
        // `head = budget` (then the ellipsis would vanish).
        let old = vec!["a", "b", "c"];
        let new = vec!["x", "y", "z"];
        let entries = entries(&old, &new, 2, Style::default(), Style::default());
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
