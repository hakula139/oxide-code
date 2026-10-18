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
use crate::tui::theme::Theme;

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
        let mut out = Vec::new();
        render_status_line(&mut out, ctx, &self.label, self.is_error);
        match &self.view {
            ToolResultView::Text { content } => {
                text::render(&mut out, ctx, content, &self.label, self.is_error);
            }
            ToolResultView::ReadExcerpt {
                path,
                lines,
                total_lines,
            } => {
                read_excerpt::render(&mut out, ctx, path, lines, *total_lines, self.is_error);
            }
            ToolResultView::Diff { chunks, .. } => {
                diff::render(&mut out, ctx, chunks, self.is_error);
            }
            ToolResultView::GrepMatches { groups, truncated } => {
                grep::render(&mut out, ctx, groups, *truncated, self.is_error);
            }
            ToolResultView::GlobFiles {
                pattern,
                files,
                total,
            } => {
                glob::render(&mut out, ctx, pattern, files, *total, self.is_error);
            }
            ToolResultView::Tasks { tasks } => {
                tasks::render(&mut out, ctx, tasks, self.is_error);
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

// ── Shared Helpers ──

fn render_status_line(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    label: &str,
    is_error: bool,
) {
    let (indicator, indicator_style) = if is_error {
        (TOOL_ERROR, ctx.theme.error())
    } else {
        (TOOL_SUCCESS, ctx.theme.success())
    };
    status_row(
        out,
        ctx,
        border_style_for(ctx.theme, is_error),
        Span::styled(indicator, indicator_style),
        Span::styled(label.to_owned(), ctx.theme.muted()),
    );
}

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

fn border_style_for(theme: &Theme, is_error: bool) -> Style {
    if is_error {
        theme.error()
    } else {
        theme.tool_border()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::tasks::TaskStatus;
    use crate::tool::tasks::testing::task;
    use crate::tool::{GrepFileGroup, GrepMatchLine};

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
}
