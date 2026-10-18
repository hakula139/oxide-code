//! Tool call and result blocks. A left-edge bar couples each call to its result and takes the
//! error color when the tool fails. Each [`ToolResultView`] variant renders its body in a child
//! module, and unstructured output renders as truncated text.

mod diff;
mod glob;
mod grep;
mod read_excerpt;
mod tasks;
mod text;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::rows::wrapped_row;
use super::{ChatBlock, RenderCtx};
use crate::tool::ToolResultView;
use crate::tui::glyphs::{TOOL_BORDER_CONT, TOOL_BORDER_PREFIX, TOOL_ERROR, TOOL_SUCCESS};
use crate::util::text::plural;

const MAX_TOOL_OUTPUT_LINES: usize = 5;

// ── Tool Call ──

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

// ── Tool Result ──

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
        let border_style = if self.is_error {
            ctx.theme.error()
        } else {
            ctx.theme.tool_border()
        };
        let mut out = Vec::new();
        self.render_status_line(&mut out, ctx, border_style);
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
            ToolResultView::Diff { chunks, .. } => {
                diff::render(&mut out, ctx, border_style, chunks);
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

// ── Shared Helpers ──

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

/// Footer counting the rows a body hides and flagging a tool-side result limit, as in
/// `... +3 lines (limit reached)`, or `None` when neither applies.
fn hidden_footer(
    hidden: usize,
    singular: &str,
    plural_form: &str,
    limit_reached: bool,
) -> Option<String> {
    let noun = plural(hidden, singular, plural_form);
    match (hidden, limit_reached) {
        (0, false) => None,
        (0, true) => Some("... limit reached".to_owned()),
        (n, false) => Some(format!("... +{n} {noun}")),
        (n, true) => Some(format!("... +{n} {noun} (limit reached)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::tasks::TaskStatus;
    use crate::tool::tasks::testing::task;
    use crate::tool::{GrepFileGroup, GrepMatchLine};
    use crate::tui::theme::Theme;

    // ── ToolResultBlock::render ──

    #[test]
    fn tool_result_block_render_puts_the_view_body_under_the_status_line() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: false,
        };
        let cases = [
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
                ["src/main.rs", "3 │ fn main() {}", "... limit reached"].as_slice(),
            ),
            (
                ToolResultView::GlobFiles {
                    pattern: "*.rs".to_owned(),
                    files: vec!["a.rs".to_owned()],
                    total: 1,
                },
                ["*.rs (1 of 1)", "a.rs"].as_slice(),
            ),
            (
                ToolResultView::Tasks {
                    tasks: vec![task("Ship it", TaskStatus::InProgress)],
                },
                ["● Ship it"].as_slice(),
            ),
        ];
        for (view, body) in cases {
            let block = ToolResultBlock::new("label", view, false);

            let rows: Vec<String> = block.render(&ctx).iter().map(ToString::to_string).collect();

            let mut expected = vec![format!("{TOOL_BORDER_PREFIX}{TOOL_SUCCESS} label")];
            expected.extend(body.iter().map(|text| format!("{TOOL_BORDER_CONT}{text}")));
            assert_eq!(rows, expected);
        }
    }

    #[test]
    fn tool_result_block_render_paints_a_failed_result_in_the_error_color() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: false,
        };
        let view = ToolResultView::Text {
            content: "boom".to_owned(),
        };
        let block = ToolResultBlock::new("bash", view, true);

        let lines = block.render(&ctx);

        let error = theme.error();
        assert_eq!(
            lines,
            [
                Line::from(vec![
                    Span::styled(TOOL_BORDER_PREFIX, error),
                    Span::styled(TOOL_ERROR, error),
                    Span::raw(" "),
                    Span::styled("bash", theme.muted()),
                ]),
                Line::from(vec![
                    Span::styled(TOOL_BORDER_CONT, error),
                    Span::styled("boom", theme.dim()),
                ]),
            ]
        );
    }

    // ── hidden_footer ──

    #[test]
    fn hidden_footer_counts_hidden_rows_and_flags_the_limit() {
        let cases = [
            (0, false, None),
            (0, true, Some("... limit reached")),
            (1, false, Some("... +1 line")),
            (3, false, Some("... +3 lines")),
            (2, true, Some("... +2 lines (limit reached)")),
        ];
        for (hidden, limit_reached, expected) in cases {
            let footer = hidden_footer(hidden, "line", "lines", limit_reached);

            assert_eq!(
                footer.as_deref(),
                expected,
                "{hidden} hidden, {limit_reached}"
            );
        }
    }
}
