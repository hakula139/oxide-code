//! Tool call and result blocks. A left-edge bar couples each call to its result and takes the
//! error color when the tool fails. Each [`ToolResultView`] variant renders its body in a child
//! module, and unstructured output renders as text. The compact view caps each body and ends it
//! with an overflow footer, which expands the block when clicked.

mod diff;
mod glob;
mod grep;
mod read_excerpt;
mod tasks;
mod text;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use ox_core::tool::view::ToolResultView;

use super::rows::{body_row, wrapped_row};
use super::{BlockRows, ChatBlock, RenderCtx};
use crate::components::overflow_count;
use crate::glyphs::{TOOL_BORDER_CONT, TOOL_BORDER_PREFIX, TOOL_ERROR, TOOL_SUCCESS};

/// Body rows a tool result shows in the compact view.
const MAX_TOOL_OUTPUT_LINES: usize = 5;

// ── ToolCallBlock ──

/// One bordered line with the tool icon and input summary.
pub(crate) struct ToolCallBlock {
    icon: String,
    label: String,
}

impl ToolCallBlock {
    pub(crate) fn new(icon: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            icon: icon.into(),
            label: label.into(),
        }
    }
}

impl ChatBlock for ToolCallBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        status_row(
            &mut out,
            ctx,
            ctx.theme.tool_border(),
            Span::styled(self.icon.clone(), ctx.theme.tool_icon()),
            Span::styled(self.label.clone(), ctx.theme.text()),
        );
        out
    }

    fn standalone(&self) -> bool {
        false
    }
}

// ── ToolResultBlock ──

/// Tool-call outcome: a ✓ or ✗ indicator, the label, and the per-view body.
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
        self.render_rows(ctx).lines
    }

    fn render_rows(&self, ctx: &RenderCtx<'_>) -> BlockRows {
        let border_style = if self.is_error {
            ctx.theme.error()
        } else {
            ctx.theme.tool_border()
        };
        let mut out = BlockRows::default();
        self.render_status_line(&mut out.lines, ctx, border_style);
        match &self.view {
            ToolResultView::Text { content } => {
                text::render(&mut out, ctx, border_style, content, &self.label);
            }
            ToolResultView::ReadExcerpt {
                path,
                lines,
                total_lines,
            } => {
                read_excerpt::render(&mut out, ctx, border_style, path, lines, *total_lines);
            }
            ToolResultView::Diff(diff) => {
                diff::render(&mut out, ctx, border_style, diff);
            }
            ToolResultView::GrepMatches { groups, truncated } => {
                grep::render(&mut out, ctx, border_style, groups, *truncated);
            }
            ToolResultView::GlobFiles {
                pattern,
                files,
                total,
            } => {
                glob::render(&mut out, ctx, border_style, pattern, files, *total);
            }
            ToolResultView::Tasks { tasks } => {
                tasks::render(&mut out, ctx, border_style, tasks);
            }
        }
        out
    }

    fn standalone(&self) -> bool {
        false
    }

    fn ends_tool_group(&self) -> bool {
        true
    }
}

impl ToolResultBlock {
    fn render_status_line(
        &self,
        out: &mut Vec<Line<'static>>,
        ctx: &RenderCtx<'_>,
        border_style: Style,
    ) {
        let (indicator, indicator_style) = if self.is_error {
            (TOOL_ERROR, ctx.theme.error())
        } else {
            (TOOL_SUCCESS, ctx.theme.success())
        };
        status_row(
            out,
            ctx,
            border_style,
            Span::styled(indicator, indicator_style),
            Span::styled(self.label.clone(), ctx.theme.muted()),
        );
    }
}

// ── Helpers ──

/// Emits a tool call or result status line, whose wrapped rows continue under the body text.
fn status_row(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    marker: Span<'static>,
    label: Span<'static>,
) {
    let line = Line::from(vec![
        Span::styled(TOOL_BORDER_PREFIX, border_style),
        marker,
        Span::raw(" "),
        label,
    ]);
    wrapped_row(out, ctx, line, TOOL_BORDER_CONT, border_style);
}

/// Emits the footer of a body that hides `hidden` rows in the compact view or that a tool-side
/// result limit cut, as in `... +3 lines (limit reached) · Ctrl+O to expand`. A footer that counts
/// hidden rows expands the block when clicked.
fn overflow_footer(
    out: &mut BlockRows,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    hidden: usize,
    singular: &str,
    plural_form: &str,
    limit_reached: bool,
) {
    let style = ctx.theme.dim();
    if hidden == 0 {
        if limit_reached {
            body_row(
                &mut out.lines,
                ctx,
                border_style,
                "... limit reached",
                style,
            );
        }
        return;
    }
    let count = overflow_count(hidden, singular, plural_form);
    let text = if limit_reached {
        format!("{count} (limit reached)")
    } else {
        count
    };
    let text = ctx.with_expand_hint(&text);
    out.push_expander(|lines| body_row(lines, ctx, border_style, text, style));
}

#[cfg(test)]
mod tests {
    use ox_core::tool::tasks::TaskStatus;
    use ox_core::tool::tasks::testing::task;
    use ox_core::tool::testing::edit_diff;
    use ox_core::tool::view::{GrepFileGroup, GrepMatchLine, NumberedLine};

    use super::*;
    use crate::theme::Theme;

    mod render {
        use super::*;

        /// One view of each kind, with the body rows it renders under the status line.
        fn views() -> [(ToolResultView, &'static [&'static str]); 6] {
            [
                (
                    ToolResultView::Text {
                        content: "boom".to_owned(),
                    },
                    &["boom"],
                ),
                (
                    ToolResultView::ReadExcerpt {
                        path: "src/lib.rs".to_owned(),
                        lines: vec![NumberedLine {
                            number: 1,
                            text: "fn f() {}".to_owned(),
                        }],
                        total_lines: 1,
                    },
                    &["src/lib.rs:1", "1 │ fn f() {}"],
                ),
                (
                    ToolResultView::Diff(edit_diff("a\n", "b\n")),
                    &["1 - a", "1 + b"],
                ),
                (
                    ToolResultView::GrepMatches {
                        groups: vec![GrepFileGroup {
                            path: "src/main.rs".to_owned(),
                            lines: vec![GrepMatchLine {
                                number: 3,
                                text: "fn main() {}".to_owned(),
                                is_match: true,
                            }],
                        }],
                        truncated: true,
                    },
                    &["src/main.rs", "3 │ fn main() {}", "... limit reached"],
                ),
                (
                    ToolResultView::GlobFiles {
                        pattern: "*.rs".to_owned(),
                        files: vec!["a.rs".to_owned()],
                        total: 1,
                    },
                    &["*.rs (1 of 1)", "a.rs"],
                ),
                (
                    ToolResultView::Tasks {
                        tasks: vec![task("Ship it", TaskStatus::InProgress)],
                    },
                    &["● Ship it"],
                ),
            ]
        }

        #[test]
        fn puts_each_view_under_a_status_line_colored_by_outcome() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(80, &theme);
            let outcomes = [
                (false, theme.tool_border(), TOOL_SUCCESS, theme.success()),
                (true, theme.error(), TOOL_ERROR, theme.error()),
            ];
            for (is_error, border, indicator, indicator_style) in outcomes {
                for (view, body) in views() {
                    let block = ToolResultBlock::new("label", view, is_error);

                    let lines = block.render(&ctx);

                    let rows: Vec<String> = lines
                        .iter()
                        .map(|line| line.to_string().trim_end().to_owned())
                        .collect();
                    let mut expected = vec![format!("{TOOL_BORDER_PREFIX}{indicator} label")];
                    expected.extend(body.iter().map(|text| format!("{TOOL_BORDER_CONT}{text}")));
                    assert_eq!(rows, expected);
                    assert_eq!(lines[0].spans[1], Span::styled(indicator, indicator_style));
                    for (row, line) in rows.iter().zip(&lines) {
                        assert_eq!(line.spans[0].style, border, "bar of {row:?}");
                    }
                }
            }
        }
    }

    mod render_rows {
        use super::*;

        #[test]
        fn marks_the_overflow_footer_below_the_status_line_as_the_row_that_expands_it() {
            let theme = Theme::default();
            let ctx = RenderCtx {
                expand_hint: Some("Ctrl+O to expand"),
                ..RenderCtx::new(80, &theme)
            };
            let content = (1..=MAX_TOOL_OUTPUT_LINES + 2)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            let block = ToolResultBlock::new("label", ToolResultView::Text { content }, false);

            let compact = block.render_rows(&ctx);
            let expanded = block.render_rows(&RenderCtx {
                expanded: true,
                ..ctx
            });

            let footer = 1 + MAX_TOOL_OUTPUT_LINES;
            assert_eq!(
                compact.lines.last().map(ToString::to_string),
                Some(format!("{TOOL_BORDER_CONT}... +2 lines · Ctrl+O to expand")),
            );
            assert_eq!(compact.expander_rows(), [footer]);
            assert_eq!(
                expanded.lines.last().map(ToString::to_string),
                Some(format!(
                    "{TOOL_BORDER_CONT}line {}",
                    MAX_TOOL_OUTPUT_LINES + 2
                )),
            );
            assert_eq!(expanded.expander_rows(), Vec::<usize>::new());
        }
    }

    mod overflow_footer {
        use super::*;

        /// The footer for `hidden` lines, with the tool limit reached or not.
        fn footer(hidden: usize, limit_reached: bool, hint: Option<&str>) -> BlockRows {
            let theme = Theme::default();
            let ctx = RenderCtx {
                expand_hint: hint,
                ..RenderCtx::new(80, &theme)
            };
            let mut out = BlockRows::default();
            overflow_footer(
                &mut out,
                &ctx,
                theme.tool_border(),
                hidden,
                "line",
                "lines",
                limit_reached,
            );
            out
        }

        fn texts(out: &BlockRows) -> Vec<String> {
            out.lines.iter().map(ToString::to_string).collect()
        }

        #[test]
        fn counts_hidden_rows_and_names_the_expand_key() {
            let hint = Some("Ctrl+O to expand");
            let cases = [
                (1, false, "... +1 line · Ctrl+O to expand"),
                (3, true, "... +3 lines (limit reached) · Ctrl+O to expand"),
            ];
            for (hidden, limit_reached, text) in cases {
                let out = footer(hidden, limit_reached, hint);

                assert_eq!(texts(&out), [format!("{TOOL_BORDER_CONT}{text}")]);
                assert_eq!(out.expander_rows(), [0], "{text}");
            }
        }

        #[test]
        fn omits_the_hint_when_no_key_expands_the_chat() {
            let out = footer(2, false, None);

            assert_eq!(texts(&out), [format!("{TOOL_BORDER_CONT}... +2 lines")]);
        }

        #[test]
        fn flags_the_tool_limit_without_expanding_when_nothing_is_hidden() {
            let limited = footer(0, true, Some("Ctrl+O to expand"));
            let quiet = footer(0, false, Some("Ctrl+O to expand"));

            assert_eq!(
                texts(&limited),
                [format!("{TOOL_BORDER_CONT}... limit reached")]
            );
            assert_eq!(limited.expander_rows(), Vec::<usize>::new());
            assert_eq!(texts(&quiet), Vec::<String>::new());
        }
    }
}
