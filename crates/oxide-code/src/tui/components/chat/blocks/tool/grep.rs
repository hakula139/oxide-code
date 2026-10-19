//! `grep` tool body in content mode: per-file groups of line-numbered match rows, where context
//! lines render dim.

use ratatui::style::Style;

use super::{MAX_TOOL_OUTPUT_LINES, overflow_footer};
use crate::tool::GrepFileGroup;
use crate::tui::components::chat::blocks::rows::{NumberedRows, body_row};
use crate::tui::components::chat::blocks::{BlockRows, RenderCtx};

pub(super) fn render(
    out: &mut BlockRows,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    groups: &[GrepFileGroup],
    truncated: bool,
) {
    if groups.is_empty() {
        return;
    }

    // Budget spans both path headers and match rows.
    let total_rows: usize = groups.iter().map(|g| 1 + g.lines.len()).sum();
    let visible_rows = total_rows.min(ctx.cap(MAX_TOOL_OUTPUT_LINES));
    let hidden = total_rows.saturating_sub(visible_rows);

    let max_number = groups
        .iter()
        .flat_map(|g| g.lines.iter())
        .map(|l| l.number)
        .max()
        .unwrap_or(0);
    let rows = NumberedRows::new(ctx, border_style, max_number);

    let mut emitted: usize = 0;
    'outer: for group in groups {
        if emitted >= visible_rows {
            break;
        }
        body_row(
            &mut out.lines,
            ctx,
            border_style,
            &group.path,
            ctx.theme.muted(),
        );
        emitted += 1;

        for line in &group.lines {
            if emitted >= visible_rows {
                break 'outer;
            }
            let text_style = if line.is_match {
                ctx.theme.text()
            } else {
                ctx.theme.dim()
            };
            rows.render(&mut out.lines, line.number, &line.text, text_style);
            emitted += 1;
        }
    }

    overflow_footer(out, ctx, border_style, hidden, "line", "lines", truncated);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::GrepMatchLine;
    use crate::tui::theme::Theme;

    mod render {
        use super::*;

        /// A file group of `(number, text, is_match)` lines.
        fn group(path: &str, lines: &[(usize, &str, bool)]) -> GrepFileGroup {
            GrepFileGroup {
                path: path.to_owned(),
                lines: lines
                    .iter()
                    .map(|&(number, text, is_match)| GrepMatchLine {
                        number,
                        text: text.to_owned(),
                        is_match,
                    })
                    .collect(),
            }
        }

        fn render_rows(groups: &[GrepFileGroup], truncated: bool) -> Vec<String> {
            let theme = Theme::default();
            let ctx = RenderCtx {
                width: 80,
                theme: &theme,
                show_thinking: true,
                expanded: false,
                expand_hint: None,
            };
            let mut out = BlockRows::default();
            render(&mut out, &ctx, theme.tool_border(), groups, truncated);
            out.lines.iter().map(ToString::to_string).collect()
        }

        #[test]
        fn groups_numbered_rows_under_each_path() {
            let theme = Theme::default();
            let ctx = RenderCtx {
                width: 80,
                theme: &theme,
                show_thinking: true,
                expanded: false,
                expand_hint: None,
            };
            let groups = [
                group(
                    "src/main.rs",
                    &[(10, "fn main() {", true), (11, "    helper();", false)],
                ),
                group("src/lib.rs", &[(5, "fn lib_func()", true)]),
            ];
            let mut out = BlockRows::default();

            render(&mut out, &ctx, theme.tool_border(), &groups, false);

            let rows: Vec<String> = out.lines.iter().map(ToString::to_string).collect();
            assert_eq!(
                rows,
                [
                    "▎   src/main.rs",
                    "▎   10 │ fn main() {",
                    "▎   11 │     helper();",
                    "▎   src/lib.rs",
                    "▎    5 │ fn lib_func()",
                ]
            );
            assert_eq!(
                out.lines[1].spans[3].style,
                theme.text(),
                "a match is full color"
            );
            assert_eq!(
                out.lines[2].spans[3].style,
                theme.dim(),
                "a context line is dim"
            );
        }

        #[test]
        fn footer_counts_hidden_rows_and_flags_the_tool_limit() {
            let hits: Vec<(usize, &str, bool)> = (1..=MAX_TOOL_OUTPUT_LINES)
                .map(|n| (n, "hit", true))
                .collect();

            let capped = render_rows(&[group("src/main.rs", &hits)], false);
            let limited = render_rows(&[group("src/main.rs", &hits[..1])], true);

            let mut expected = vec!["▎   src/main.rs".to_owned()];
            expected.extend((1..MAX_TOOL_OUTPUT_LINES).map(|n| format!("▎   {n} │ hit")));
            expected.push("▎   ... +1 line".to_owned());
            assert_eq!(capped, expected);
            assert_eq!(
                limited,
                ["▎   src/main.rs", "▎   1 │ hit", "▎   ... limit reached"]
            );
        }

        #[test]
        fn empty_groups_emits_nothing() {
            // truncated=true is intentional: empty groups short-circuit before the footer.
            assert_eq!(render_rows(&[], true), [] as [String; 0]);
        }

        #[test]
        fn stops_when_budget_fills_at_path_boundary() {
            // Six 0-line groups overflow the 5-row budget on path headers alone, hitting the outer
            // guard that `break 'outer` does not reach when groups have no match rows.
            let groups: Vec<GrepFileGroup> =
                (0..6).map(|i| group(&format!("f{i}.rs"), &[])).collect();

            let rows = render_rows(&groups, false);

            assert_eq!(
                rows,
                [
                    "▎   f0.rs",
                    "▎   f1.rs",
                    "▎   f2.rs",
                    "▎   f3.rs",
                    "▎   f4.rs",
                    "▎   ... +1 line",
                ]
            );
        }
    }
}
