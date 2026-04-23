//! Tool call and tool result blocks.
//!
//! The tool group is the only chat block that keeps a left-edge bar —
//! it visually couples a call to its output and color-codes success /
//! error at the same time. Every other block (user, assistant, error)
//! uses the bar-less icon-prefix helpers in [`super`] and flushes to
//! col 0. The bar / border machinery therefore lives here, not in the
//! trait module, so it scopes to exactly the blocks that use it.
//!
//! Result rendering is per-variant via [`ToolResultView`]: the default
//! is a truncated text body; tools with structured inputs (Edit today;
//! Read / Grep / Glob later) produce richer variants via
//! [`Tool::result_view`](crate::tool::Tool::result_view). The enum
//! itself lives in `crate::tool` — rendering stays here, so adding a
//! new variant touches the tool module + this file + the renderer.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{ChatBlock, RenderCtx};
use crate::tool::ToolResultView;
use crate::tui::theme::Theme;
use crate::tui::wrap::{expand_tabs, wrap_line};

/// Maximum lines of tool output shown inline before truncation.
const MAX_TOOL_OUTPUT_LINES: usize = 5;

/// Maximum lines of diff body (combined `-` + `+`) shown before
/// truncation. Set higher than text output because diffs pair every
/// old line with its new counterpart, doubling the natural line
/// count before the user learns anything new.
const MAX_DIFF_BODY_LINES: usize = 10;

/// Maximum bytes per tool output line before horizontal truncation.
/// Measured in bytes (matched against `str::len`) rather than Unicode
/// characters — display width is already gated by the terminal width
/// budget; this cap exists to avoid pathological multi-kilobyte lines
/// pasted into tool output.
const MAX_TOOL_OUTPUT_LINE_BYTES: usize = 512;

/// Left bar character for tool blocks.
const BAR: &str = "▎";

/// First-line prefix for tool-call and tool-result status lines — bar +
/// space. Content sits at col 2.
const BORDER_PREFIX: &str = "▎ ";

/// Prefix for lines subordinate to the status header — wrapped tool
/// name / result label (when the header overflows) and tool output body
/// lines. Aligns content at col 4, past the `✓` / `✗` indicator, so the
/// body reads as a child of the status header rather than a peer.
const STATUS_LINE_CONT: &str = "▎   ";

// ── Tool Call ──

/// A running or completed tool invocation — one bordered line with the
/// tool icon and input summary.
pub(crate) struct ToolCallBlock {
    icon: &'static str,
    label: String,
}

impl ToolCallBlock {
    pub(crate) fn new(icon: &'static str, label: impl Into<String>) -> Self {
        Self {
            icon,
            label: label.into(),
        }
    }
}

impl ChatBlock for ToolCallBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let border_style = ctx.theme.tool_border();
        let cont_prefix = border_continuation_prefix(STATUS_LINE_CONT, border_style);
        let line = Line::from(vec![
            Span::styled(BORDER_PREFIX.to_owned(), border_style),
            Span::styled(self.icon.to_owned(), ctx.theme.tool_icon()),
            Span::raw(" "),
            Span::styled(self.label.clone(), ctx.theme.text()),
        ]);
        wrap_line(
            line,
            usize::from(ctx.width),
            STATUS_LINE_CONT.width(),
            Some(&cont_prefix),
        )
    }

    fn standalone(&self) -> bool {
        false
    }
}

// ── Tool Result ──

/// The outcome of a tool call — indicator (✓ / ✗), label, and a
/// per-view body (truncated text by default; richer shapes for tools
/// with structured inputs).
pub(crate) struct ToolResultBlock {
    label: String,
    view: ToolResultView,
    is_error: bool,
}

impl ToolResultBlock {
    pub(crate) fn new(label: impl Into<String>, view: ToolResultView, is_error: bool) -> Self {
        Self {
            label: label.into(),
            view,
            is_error,
        }
    }
}

impl ChatBlock for ToolResultBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        render_status_line(&mut out, ctx, &self.label, self.is_error);
        match &self.view {
            ToolResultView::Text { content } => {
                render_text_body(&mut out, ctx, content, &self.label, self.is_error);
            }
            ToolResultView::Diff {
                old,
                new,
                replace_all,
                replacements,
            } => {
                render_diff_body(
                    &mut out,
                    ctx,
                    old,
                    new,
                    *replace_all,
                    *replacements,
                    self.is_error,
                );
            }
        }
        out
    }

    fn standalone(&self) -> bool {
        false
    }
}

fn render_text_body(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    content: &str,
    label: &str,
    is_error: bool,
) {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return;
    }

    let border_style = border_style_for(ctx.theme, is_error);
    let text_style = ctx.theme.dim();
    let cont_prefix = border_continuation_prefix(STATUS_LINE_CONT, border_style);
    let width = usize::from(ctx.width);

    // Tools (grep, glob) commonly use their own summary line as both
    // the `title` metadata (shown in the status line) and the first
    // line of `content` (shown in the body) — the model needs the
    // summary to parse counts, but rendering both duplicates it on
    // screen. Skip the first body line when it matches the label
    // verbatim.
    let mut output_lines: Vec<&str> = trimmed.lines().collect();
    if output_lines
        .first()
        .is_some_and(|l| l.trim() == label.trim())
    {
        output_lines.remove(0);
    }
    if output_lines.is_empty() {
        return;
    }
    let truncated = output_lines.len() > MAX_TOOL_OUTPUT_LINES;
    let visible = if truncated {
        &output_lines[..MAX_TOOL_OUTPUT_LINES]
    } else {
        &output_lines
    };

    for text_line in visible {
        let expanded = expand_tabs(text_line);
        let display_text = truncate_to_bytes(&expanded, MAX_TOOL_OUTPUT_LINE_BYTES);
        let line = Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled(display_text, text_style),
        ]);
        out.extend(wrap_line(
            line,
            width,
            STATUS_LINE_CONT.width(),
            Some(&cont_prefix),
        ));
    }

    if truncated {
        let n = output_lines.len() - MAX_TOOL_OUTPUT_LINES;
        let label = if n == 1 { "line" } else { "lines" };
        out.push(Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled(format!("... +{n} {label}"), ctx.theme.dim()),
        ]));
    }
}

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
fn render_diff_body(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    old: &str,
    new: &str,
    replace_all: bool,
    replacements: usize,
    is_error: bool,
) {
    let border_style = border_style_for(ctx.theme, is_error);
    let cont_prefix = border_continuation_prefix(STATUS_LINE_CONT, border_style);
    let width = usize::from(ctx.width);

    let old_lines = split_diff_side(old);
    let new_lines = split_diff_side(new);
    let (old_lines, new_lines) = trim_common_boundaries(&old_lines, &new_lines);
    if old_lines.is_empty() && new_lines.is_empty() {
        return;
    }

    let entries = diff_entries(
        old_lines,
        new_lines,
        MAX_DIFF_BODY_LINES,
        ctx.theme.error(),
        ctx.theme.success(),
    );
    for entry in entries {
        match entry {
            DiffEntry::Line { sign, text, style } => {
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
                    STATUS_LINE_CONT.width(),
                    Some(&cont_prefix),
                ));
            }
            DiffEntry::Ellipsis { hidden } => {
                let noun = if hidden == 1 { "line" } else { "lines" };
                out.push(Line::from(vec![
                    Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
                    Span::styled(format!("... +{hidden} {noun}"), ctx.theme.dim()),
                ]));
            }
        }
    }

    if replace_all && replacements > 1 {
        out.push(Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled(
                format!("applied to {replacements} matches"),
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
fn split_diff_side(side: &str) -> Vec<&str> {
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
enum DiffEntry<'a> {
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
fn diff_entries<'a>(
    old_lines: &[&'a str],
    new_lines: &[&'a str],
    budget: usize,
    del_style: Style,
    add_style: Style,
) -> Vec<DiffEntry<'a>> {
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
    out: &mut Vec<DiffEntry<'a>>,
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
            out.push(DiffEntry::Line {
                sign,
                text: line,
                style,
            });
        }
        return;
    }
    if budget == 0 {
        out.push(DiffEntry::Ellipsis {
            hidden: lines.len(),
        });
        return;
    }
    let head = budget - 1;
    let tail = 1;
    let hidden = lines.len() - head - tail;
    for line in &lines[..head] {
        out.push(DiffEntry::Line {
            sign,
            text: line,
            style,
        });
    }
    if hidden > 0 {
        out.push(DiffEntry::Ellipsis { hidden });
    }
    for line in &lines[lines.len() - tail..] {
        out.push(DiffEntry::Line {
            sign,
            text: line,
            style,
        });
    }
}

/// Renders the tool-result header line — success / error indicator,
/// styled label, and wrapped continuation under the bar.
fn render_status_line(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    label: &str,
    is_error: bool,
) {
    let (indicator, indicator_style) = if is_error {
        ("✗", ctx.theme.error())
    } else {
        ("✓", ctx.theme.success())
    };
    let border_style = border_style_for(ctx.theme, is_error);
    let cont_prefix = border_continuation_prefix(STATUS_LINE_CONT, border_style);
    let line = Line::from(vec![
        Span::styled(BORDER_PREFIX.to_owned(), border_style),
        Span::styled(indicator, indicator_style),
        Span::raw(" "),
        Span::styled(label.to_owned(), ctx.theme.muted()),
    ]);
    out.extend(wrap_line(
        line,
        usize::from(ctx.width),
        STATUS_LINE_CONT.width(),
        Some(&cont_prefix),
    ));
}

/// Builds a continuation prefix that keeps the `▎` bar aligned under
/// the original prefix. For a prefix like `"▎   "` (4 cols), produces
/// `["", "▎", "   "]` where the bar span is styled.
///
/// Precondition: `prefix` must contain [`BAR`] — every tool-rendering
/// call site passes either [`BORDER_PREFIX`] or [`STATUS_LINE_CONT`],
/// both of which satisfy it.
fn border_continuation_prefix(prefix: &str, bar_style: Style) -> Vec<Span<'static>> {
    let bar_pos = prefix.find(BAR).expect("prefix must contain ▎ bar");
    let left = &prefix[..bar_pos];
    let right = &prefix[bar_pos + BAR.len()..];
    vec![
        Span::raw(left.to_owned()),
        Span::styled(BAR, bar_style),
        Span::raw(right.to_owned()),
    ]
}

fn border_style_for(theme: &Theme, is_error: bool) -> Style {
    if is_error {
        theme.error()
    } else {
        theme.tool_border()
    }
}

/// Truncates a string to `max_bytes` bytes, appending `...` if cut.
/// Falls back to the nearest char boundary at or before `max_bytes` to
/// avoid splitting multi-byte UTF-8 sequences.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let boundary = s.floor_char_boundary(max_bytes);
    format!("{}...", &s[..boundary])
}

#[cfg(test)]
mod tests {
    use ratatui::style::Style;

    use super::*;

    // ── border_continuation_prefix ──

    #[test]
    fn border_continuation_prefix_preserves_bar_position() {
        let style = Style::default();
        let spans = border_continuation_prefix(BORDER_PREFIX, style);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "");
        assert_eq!(spans[1].content, BAR);
        assert_eq!(spans[2].content, " ");
    }

    // ── truncate_to_bytes ──

    #[test]
    fn truncate_to_bytes_under_limit_returns_input() {
        assert_eq!(truncate_to_bytes("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_bytes_over_limit_appends_ellipsis() {
        assert_eq!(truncate_to_bytes("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_to_bytes_respects_char_boundary() {
        // Each `中` is 3 bytes in UTF-8. If floor_char_boundary wasn't used,
        // cutting at byte 5 would split the second `中` mid-codepoint and
        // produce invalid UTF-8 (panic on `&s[..5]`). Boundary fallback
        // rounds down to byte 3, yielding one clean `中` + `...`.
        let input = "中中中中";
        let result = truncate_to_bytes(input, 5);
        assert_eq!(result, "中...");
        assert!(result.is_char_boundary(result.len() - 3));
    }

    #[test]
    fn truncate_to_bytes_exact_boundary_no_split() {
        // 6 bytes = exactly two `中`s; result stays untouched.
        assert_eq!(truncate_to_bytes("中中", 6), "中中");
    }

    // ── diff_entries ──

    /// Render a `DiffEntry` stream as `Vec<String>` so assertions read
    /// like the actual rendered diff body.
    fn render_entries(entries: &[DiffEntry<'_>]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                DiffEntry::Line { sign, text, .. } => format!("{sign} {text}"),
                DiffEntry::Ellipsis { hidden } => format!("... +{hidden}"),
            })
            .collect()
    }

    #[test]
    fn diff_entries_under_budget_shows_all_lines() {
        let old = vec!["foo", "bar"];
        let new = vec!["baz"];
        let entries = diff_entries(&old, &new, 10, Style::default(), Style::default());
        assert_eq!(render_entries(&entries), vec!["- foo", "- bar", "+ baz"]);
    }

    #[test]
    fn diff_entries_over_budget_splits_budget_between_sides() {
        // Symmetric policy: when combined length exceeds the budget,
        // each side gets a fair share with head + ellipsis + tail.
        // Here old (2) fits entirely, so its surplus shifts to new (5
        // lines under a budget of 5 - 2 = 3: 2 head + tail? no —
        // head = budget - 1 = 2, tail = 1, hidden = 5 - 3 = 2).
        let old = vec!["a", "b"];
        let new = vec!["c", "d", "e", "f", "g"];
        let entries = diff_entries(&old, &new, 5, Style::default(), Style::default());
        assert_eq!(
            render_entries(&entries),
            vec!["- a", "- b", "+ c", "+ d", "... +2", "+ g"],
            "old fits in its budget; new uses head + ellipsis + tail",
        );
    }

    #[test]
    fn diff_entries_pure_deletion_over_budget_truncates_old_side() {
        // Regression: previously `old` had no budget cap, so a 20-line
        // pure deletion rendered 20 `-` lines followed by a bogus
        // `... +0 lines` footer. Now old is budgeted like any side
        // and the zero-hidden Ellipsis is suppressed.
        let old = vec!["a", "b", "c", "d", "e", "f", "g"];
        let new: Vec<&str> = Vec::new();
        let entries = diff_entries(&old, &new, 5, Style::default(), Style::default());
        assert_eq!(
            render_entries(&entries),
            vec!["- a", "- b", "- c", "- d", "... +2", "- g"],
            "old gets the full budget when new is empty",
        );
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e, DiffEntry::Ellipsis { hidden: 0 })),
            "Ellipsis {{ hidden: 0 }} must never be emitted",
        );
    }

    #[test]
    fn diff_entries_at_budget_boundary_shows_every_line() {
        // old.len() + new.len() == budget: both sides render in full,
        // no ellipsis — the off-by-one guard against a spurious footer.
        let old = vec!["a", "b", "c"];
        let new = vec!["x", "y"];
        let entries = diff_entries(&old, &new, 5, Style::default(), Style::default());
        assert_eq!(
            render_entries(&entries),
            vec!["- a", "- b", "- c", "+ x", "+ y"],
        );
        assert!(entries.iter().all(|e| matches!(e, DiffEntry::Line { .. })));
    }

    #[test]
    fn diff_entries_both_sides_overflow_split_evenly() {
        // When neither side fits in an even split, each gets half the
        // budget — head-ellipsis-tail on both. Odd leftover goes to old.
        let old = vec!["a0", "a1", "a2", "a3", "a4"];
        let new = vec!["b0", "b1", "b2", "b3", "b4"];
        let entries = diff_entries(&old, &new, 5, Style::default(), Style::default());
        assert_eq!(
            render_entries(&entries),
            // old budget 3 (half of 5 rounded up): head 2, tail 1,
            // hidden 2; new budget 2: head 1, tail 1, hidden 3.
            vec!["- a0", "- a1", "... +2", "- a4", "+ b0", "... +3", "+ b4"],
        );
    }

    #[test]
    fn diff_entries_zero_budget_collapses_each_side_to_ellipsis() {
        // Pathological budget == 0 must still terminate cleanly — each
        // non-empty side emits a single Ellipsis rather than panicking
        // on an underflowed head / tail split.
        let old = vec!["a", "b"];
        let new = vec!["x"];
        let entries = diff_entries(&old, &new, 0, Style::default(), Style::default());
        assert_eq!(render_entries(&entries), vec!["... +2", "... +1"]);
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

    // ── split_diff_side ──

    #[test]
    fn split_diff_side_empty_yields_empty_slice() {
        assert!(split_diff_side("").is_empty());
    }

    #[test]
    fn split_diff_side_drops_trailing_newline() {
        // `"a\nb\n"` → two displayable lines, not three with a blank
        // tail. Needed because `old_string` / `new_string` often end
        // in newlines when the edit spans full lines.
        assert_eq!(split_diff_side("a\nb\n"), vec!["a", "b"]);
    }
}
