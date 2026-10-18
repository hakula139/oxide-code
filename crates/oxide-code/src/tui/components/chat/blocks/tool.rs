//! Tool call and result blocks. A left-edge bar couples each call to its result and takes the
//! error color when the tool fails. Each [`ToolResultView`] variant renders its body in a child
//! module, and unstructured output renders as truncated text.

pub(super) mod bordered_row;
mod diff;
mod glob;
mod grep;
pub(super) mod numbered_row;
mod read_excerpt;
mod tasks;
mod text;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{BlockKind, ChatBlock, RenderCtx, bar_continuation_prefix};
use crate::tool::ToolResultView;
use crate::tui::glyphs::{TOOL_BORDER_CONT, TOOL_BORDER_PREFIX, TOOL_ERROR, TOOL_SUCCESS};
use crate::tui::theme::Theme;
use crate::tui::wrap::wrap_line;

const MAX_TOOL_OUTPUT_LINES: usize = 5;

const MAX_TOOL_OUTPUT_LINE_BYTES: usize = 512;

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
        let border_style = ctx.theme.tool_border();
        let cont_prefix = bar_continuation_prefix(TOOL_BORDER_CONT, border_style);
        let line = Line::from(vec![
            Span::styled(TOOL_BORDER_PREFIX.to_owned(), border_style),
            Span::styled(self.icon.clone(), ctx.theme.tool_icon()),
            Span::raw(" "),
            Span::styled(self.label.clone(), ctx.theme.text()),
        ]);
        wrap_line(
            line,
            usize::from(ctx.width),
            TOOL_BORDER_CONT.width(),
            Some(&cont_prefix),
        )
    }

    fn standalone(&self) -> bool {
        false
    }

    fn block_kind(&self) -> BlockKind {
        BlockKind::Call
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

    fn block_kind(&self) -> BlockKind {
        BlockKind::Result
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
    let border_style = border_style_for(ctx.theme, is_error);
    let cont_prefix = bar_continuation_prefix(TOOL_BORDER_CONT, border_style);
    let line = Line::from(vec![
        Span::styled(TOOL_BORDER_PREFIX.to_owned(), border_style),
        Span::styled(indicator, indicator_style),
        Span::raw(" "),
        Span::styled(label.to_owned(), ctx.theme.muted()),
    ]);
    out.extend(wrap_line(
        line,
        usize::from(ctx.width),
        TOOL_BORDER_CONT.width(),
        Some(&cont_prefix),
    ));
}

fn border_style_for(theme: &Theme, is_error: bool) -> Style {
    if is_error {
        theme.error()
    } else {
        theme.tool_border()
    }
}

fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let boundary = s.floor_char_boundary(max_bytes);
    format!("{}...", &s[..boundary])
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

    // ── truncate_to_bytes ──

    #[test]
    fn truncate_to_bytes_under_limit_preserves_input() {
        assert_eq!(truncate_to_bytes("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_bytes_over_limit_appends_ellipsis() {
        assert_eq!(truncate_to_bytes("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_to_bytes_respects_char_boundary() {
        // Each `中` is 3 bytes, so cutting at byte 5 without `floor_char_boundary` would split a
        // codepoint and panic on `&s[..5]`. Rounding down to byte 3 yields one `中` + `...`.
        let input = "中中中中";
        let result = truncate_to_bytes(input, 5);
        assert_eq!(result, "中...");
        assert!(result.is_char_boundary(result.len() - 3));
    }

    #[test]
    fn truncate_to_bytes_exact_boundary_no_split() {
        assert_eq!(truncate_to_bytes("中中", 6), "中中");
    }
}
