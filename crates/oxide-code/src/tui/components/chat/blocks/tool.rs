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
    use crate::tool::{DiffChunk, DiffLine, GrepFileGroup, GrepMatchLine, ReadExcerptLine};
    use crate::tui::theme::Theme;

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
                        lines: vec![ReadExcerptLine {
                            number: 1,
                            text: "fn f() {}".to_owned(),
                        }],
                        total_lines: 1,
                    },
                    &["src/lib.rs:1", "1 │ fn f() {}"],
                ),
                (
                    ToolResultView::Diff {
                        chunks: vec![DiffChunk {
                            old: vec![DiffLine {
                                number: 1,
                                text: "a".to_owned(),
                            }],
                            new: vec![DiffLine {
                                number: 1,
                                text: "b".to_owned(),
                            }],
                        }],
                        replace_all: false,
                        replacements: 1,
                    },
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
            let ctx = RenderCtx {
                width: 80,
                theme: &theme,
                show_thinking: false,
            };
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
}
