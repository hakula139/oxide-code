//! Edit-tool diff body: the hunks of an [`EditDiff`] in a unified layout, with context rows and the
//! changed words of each paired line marked.

use ratatui::style::Style;

use ox_core::tool::view::{Diff, DiffLineKind, DiffRow, EditDiff};
use ox_core::util::text::plural;

use super::overflow_footer;
use crate::components::chat::blocks::rows::{NumberedRows, body_row};
use crate::components::chat::blocks::{BlockRows, RenderCtx};
use crate::glyphs::{HUNK_GAP, diff_cap_row};

/// Diff line rows the compact view shows.
const MAX_DIFF_BODY_LINES: usize = 20;
/// Line numbers the locations footer lists in the compact view.
const MAX_LOCATIONS_DISPLAYED: usize = 8;
const NO_CHANGE_MARKER: &str = "(no change)";

pub(super) fn render(
    out: &mut BlockRows,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    diff: &EditDiff,
) {
    if diff.diff.hunks.is_empty() {
        body_row(
            &mut out.lines,
            ctx,
            border_style,
            NO_CHANGE_MARKER,
            ctx.theme.dim(),
        );
        return;
    }

    render_hunks(out, ctx, border_style, &diff.diff);

    if diff.locations.len() > 1 {
        render_locations_footer(out, ctx, border_style, &diff.locations);
    }
}

/// Draws context rows as `/diff` does, and removed and added rows over their tints.
fn render_hunks(out: &mut BlockRows, ctx: &RenderCtx<'_>, border_style: Style, diff: &Diff) {
    let max_number = diff.max_number();
    let context = NumberedRows::new(ctx, border_style, max_number);
    let removed = NumberedRows::removed(ctx, border_style, max_number);
    let added = NumberedRows::added(ctx, border_style, max_number);

    for row in rows(diff, ctx.cap(MAX_DIFF_BODY_LINES)) {
        match row {
            Row::Diff(DiffRow::Line(line)) => {
                let (rows, text_style) = match line.kind {
                    DiffLineKind::Context => (&context, ctx.theme.dim()),
                    DiffLineKind::Removed => (&removed, ctx.theme.text()),
                    DiffLineKind::Added => (&added, ctx.theme.text()),
                };
                rows.render_segments(&mut out.lines, line.number, line.segments(), text_style);
            }
            Row::Diff(DiffRow::Gap) => {
                let gap = format!("{HUNK_GAP:>width$}", width = max_number.to_string().len());
                body_row(&mut out.lines, ctx, border_style, gap, ctx.theme.dim());
            }
            Row::Diff(DiffRow::Omitted(lines)) => {
                let text = diff_cap_row(lines);
                body_row(&mut out.lines, ctx, border_style, text, ctx.theme.dim());
            }
            Row::Elided(lines) => {
                overflow_footer(out, ctx, border_style, lines, "line", "lines", false);
            }
            Row::FoldedHunks(hunks) => {
                overflow_footer(out, ctx, border_style, hunks, "hunk", "hunks", false);
            }
        }
    }
}

/// One row of a diff body as [`rows`] lays it out.
enum Row<'a> {
    Diff(DiffRow<'a>),
    /// The middle lines of an over-budget hunk.
    Elided(usize),
    /// The hunks left out once the budget filled.
    FoldedHunks(usize),
}

/// The rows of [`Diff::rows`] that fit `budget` line rows, which must be at least 2. Hunks keep
/// their rows while they fit, and the rest fold into one [`Row::FoldedHunks`]. A first hunk longer
/// than the budget keeps a half at each end around a [`Row::Elided`]. The cap row always shows,
/// since expanding cannot reveal the lines it counts.
fn rows(diff: &Diff, budget: usize) -> Vec<Row<'_>> {
    let mut lens = diff.hunks.iter().map(|hunk| hunk.lines.len());
    let first = lens.next().unwrap_or(0);
    let shown = if first > budget {
        1
    } else {
        1 + lens
            .scan(budget - first, |left, len| {
                (len <= *left).then(|| *left -= len)
            })
            .count()
    };

    let mut rows = Vec::new();
    let mut hunk = 0;
    for row in diff.rows() {
        if row == DiffRow::Gap {
            hunk += 1;
            if hunk == shown {
                rows.push(Row::FoldedHunks(diff.hunks.len() - shown));
            }
        }
        if hunk < shown || matches!(row, DiffRow::Omitted(_)) {
            rows.push(Row::Diff(row));
        }
    }
    if first > budget {
        let head = budget.div_ceil(2);
        let tail = first - (budget - head);
        rows.splice(head..tail, [Row::Elided(tail - head)]);
    }
    rows
}

/// Lists the lines each replacement applied at. A compact list that leaves some out ends with the
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

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use ox_core::tool::testing::diff_line;
    use ox_core::tool::view::{DiffHunk, DiffLine};
    use ratatui::text::Line;

    use super::*;
    use crate::theme::Theme;

    fn edit_diff(hunks: Vec<Vec<DiffLine>>, locations: Vec<usize>) -> EditDiff {
        EditDiff {
            diff: Diff {
                hunks: hunks.into_iter().map(|lines| DiffHunk { lines }).collect(),
                omitted_lines: 0,
            },
            locations,
        }
    }

    fn expanded_ctx(theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            expanded: true,
            ..RenderCtx::new(80, theme)
        }
    }

    fn render_compact(theme: &Theme, diff: &EditDiff) -> BlockRows {
        let mut out = BlockRows::default();
        render(
            &mut out,
            &RenderCtx::new(80, theme),
            theme.tool_border(),
            diff,
        );
        out
    }

    /// Each row's text without the pad that carries a diff row's bg to the chat width.
    fn text_rows(out: &[Line<'static>]) -> Vec<String> {
        out.iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    mod render {
        use super::*;

        #[test]
        fn marks_a_diff_without_hunks_as_no_change() {
            let theme = Theme::default();

            let out = render_compact(&theme, &edit_diff(Vec::new(), Vec::new()));

            assert_eq!(text_rows(&out.lines), ["▎   (no change)"]);
        }

        #[test]
        fn draws_context_around_the_change_without_a_locations_footer_for_one_location() {
            let theme = Theme::default();
            let diff = edit_diff(
                vec![vec![
                    diff_line(DiffLineKind::Context, 9, "fn main() {"),
                    diff_line(DiffLineKind::Removed, 10, "    foo();"),
                    diff_line(DiffLineKind::Added, 10, "    bar();"),
                    diff_line(DiffLineKind::Context, 11, "}"),
                ]],
                vec![10],
            );

            let out = render_compact(&theme, &diff);

            assert_eq!(
                text_rows(&out.lines),
                [
                    "▎    9 │ fn main() {",
                    "▎   10 -     foo();",
                    "▎   10 +     bar();",
                    "▎   11 │ }",
                ]
            );
            assert_eq!(out.lines[0].spans[3].style, theme.dim());
            assert_eq!(out.lines[0].spans[3].style.bg, None);
        }

        #[test]
        fn ends_with_the_locations_of_several_replacements() {
            let theme = Theme::default();
            let diff = edit_diff(
                vec![vec![
                    diff_line(DiffLineKind::Removed, 12, "foo"),
                    diff_line(DiffLineKind::Added, 12, "bar"),
                ]],
                vec![12, 47],
            );

            let out = render_compact(&theme, &diff);

            assert_eq!(
                text_rows(&out.lines),
                [
                    "▎   12 - foo",
                    "▎   12 + bar",
                    "▎   applied at lines 12, 47",
                ]
            );
        }
    }

    mod render_hunks {
        use super::*;

        fn render_rows(ctx: &RenderCtx<'_>, diff: &EditDiff) -> BlockRows {
            let mut out = BlockRows::default();
            render_hunks(&mut out, ctx, ctx.theme.tool_border(), &diff.diff);
            out
        }

        #[test]
        fn marks_the_changed_words_over_the_word_bg() {
            let theme = Theme::default();
            let changed = |kind, text: &str, range: Range<usize>| DiffLine {
                changed: vec![range],
                ..diff_line(kind, 1, text)
            };
            let diff = edit_diff(
                vec![vec![
                    changed(DiffLineKind::Removed, "let a = 1;", 8..9),
                    changed(DiffLineKind::Added, "let a = 2;", 8..9),
                ]],
                vec![1],
            );

            let out = render_rows(&RenderCtx::new(80, &theme), &diff);

            for (row, word, row_bg, word_bg) in [
                (&out.lines[0], "1", theme.diff_del(), theme.diff_del_word()),
                (&out.lines[1], "2", theme.diff_add(), theme.diff_add_word()),
            ] {
                let text: Vec<_> = row.spans[3..6]
                    .iter()
                    .map(|span| (span.content.as_ref(), span.style.bg))
                    .collect();
                assert_eq!(
                    text,
                    [
                        ("let a = ", row_bg.bg),
                        (word, word_bg.bg),
                        (";", row_bg.bg),
                    ],
                );
            }
        }

        #[test]
        fn separates_hunks_with_a_gap_and_folds_the_hunks_past_the_budget() {
            let theme = Theme::default();
            let hunk = |number: usize| {
                vec![
                    diff_line(DiffLineKind::Removed, number, "old"),
                    diff_line(DiffLineKind::Added, number, "new"),
                ]
            };
            let diff = edit_diff(
                (1..=MAX_DIFF_BODY_LINES / 2 + 2)
                    .map(|n| hunk(n * 10))
                    .collect(),
                Vec::new(),
            );

            let compact = render_rows(&RenderCtx::new(80, &theme), &diff);
            let expanded = render_rows(&expanded_ctx(&theme), &diff);

            let compact_rows = text_rows(&compact.lines);
            assert_eq!(
                compact_rows[..5],
                [
                    "▎    10 - old",
                    "▎    10 + new",
                    "▎     ⋮",
                    "▎    20 - old",
                    "▎    20 + new",
                ],
            );
            assert_eq!(compact_rows.last().unwrap(), "▎   ... +2 hunks");
            assert_eq!(compact.expander_rows(), [compact_rows.len() - 1]);
            assert_eq!(text_rows(&expanded.lines).last().unwrap(), "▎   120 + new");
            assert_eq!(expanded.expander_rows(), Vec::<usize>::new());
        }

        #[test]
        fn elides_an_overlong_first_hunk_and_folds_the_rest_until_expanded() {
            let theme = Theme::default();
            let numbers = 1..=MAX_DIFF_BODY_LINES + 2;
            let added = numbers
                .clone()
                .map(|n| diff_line(DiffLineKind::Added, n, &format!("line {n}")))
                .collect();
            let diff = edit_diff(
                vec![added, vec![diff_line(DiffLineKind::Added, 40, "line 40")]],
                vec![1, 40],
            );
            let row = |n: usize| format!("▎   {n:>2} + line {n}");

            let compact = render_rows(&RenderCtx::new(80, &theme), &diff);
            let expanded = render_rows(&expanded_ctx(&theme), &diff);

            let half = MAX_DIFF_BODY_LINES / 2;
            let mut expected: Vec<String> = (1..=half).map(row).collect();
            expected.push("▎   ... +2 lines".to_owned());
            expected.extend((half + 3..=MAX_DIFF_BODY_LINES + 2).map(row));
            expected.push("▎   ... +1 hunk".to_owned());
            assert_eq!(text_rows(&compact.lines), expected);
            assert_eq!(compact.expander_rows(), [half, MAX_DIFF_BODY_LINES + 1]);
            let mut expected: Vec<String> = numbers.map(row).collect();
            expected.extend(["▎    ⋮".to_owned(), row(40)]);
            assert_eq!(text_rows(&expanded.lines), expected);
            assert_eq!(expanded.expander_rows(), Vec::<usize>::new());
        }

        #[test]
        fn ends_a_capped_diff_with_a_plain_count_of_the_lines_it_left_out() {
            let theme = Theme::default();
            let hunk = |number: usize| vec![diff_line(DiffLineKind::Added, number, "new")];
            let diff = EditDiff {
                diff: Diff {
                    omitted_lines: 5,
                    ..edit_diff(
                        (1..=MAX_DIFF_BODY_LINES + 2)
                            .map(|n| hunk(n * 10))
                            .collect(),
                        Vec::new(),
                    )
                    .diff
                },
                locations: Vec::new(),
            };
            let hint = Some("Ctrl+O to expand");

            let compact = render_rows(
                &RenderCtx {
                    expand_hint: hint,
                    ..RenderCtx::new(80, &theme)
                },
                &diff,
            );
            let expanded = render_rows(
                &RenderCtx {
                    expand_hint: hint,
                    ..expanded_ctx(&theme)
                },
                &diff,
            );

            let compact_rows = text_rows(&compact.lines);
            assert_eq!(
                compact_rows[compact_rows.len() - 2..],
                [
                    "▎   ... +2 hunks · Ctrl+O to expand",
                    "▎   ... +5 lines past the diff limit",
                ],
            );
            assert_eq!(compact.expander_rows(), [compact_rows.len() - 2]);
            assert_eq!(
                text_rows(&expanded.lines).last().unwrap(),
                "▎   ... +5 lines past the diff limit"
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
                text_rows(&out.lines),
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
                text_rows(&out.lines),
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

            assert_eq!(text_rows(&out.lines), ["▎   applied at line 42"]);
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
                text_rows(&out.lines),
                ["▎   applied at lines 12,", "▎   47, 200"]
            );
        }
    }
}
