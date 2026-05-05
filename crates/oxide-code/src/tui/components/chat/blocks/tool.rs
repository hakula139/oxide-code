//! Tool call and result blocks. The left-edge bar visually couples a call to its output and
//! color-codes success / error; this is the only chat block that keeps it, so the bar / border
//! helpers live here rather than in the trait module.
//!
//! Result rendering is per-variant via [`ToolResultView`] — default is truncated text; structured
//! tools (Edit, Read, Grep, Glob) get richer bodies in sibling modules under [`tool`].

pub(super) mod bordered_row;
mod diff;
mod glob;
mod grep;
pub(super) mod numbered_row;
mod read_excerpt;
mod text;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{BlockKind, ChatBlock, RenderCtx};
use crate::tool::ToolResultView;
use crate::tui::glyphs::{BAR, TOOL_BORDER_CONT, TOOL_BORDER_PREFIX, TOOL_ERROR, TOOL_SUCCESS};
use crate::tui::theme::Theme;
use crate::tui::wrap::wrap_line;

const MAX_TOOL_OUTPUT_LINES: usize = 5;

const MAX_TOOL_OUTPUT_LINE_BYTES: usize = 512;

// ── Tool Call ──

/// One bordered line with the tool icon and input summary.
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
        let cont_prefix = border_continuation_prefix(TOOL_BORDER_CONT, border_style);
        let line = Line::from(vec![
            Span::styled(TOOL_BORDER_PREFIX.to_owned(), border_style),
            Span::styled(self.icon.to_owned(), ctx.theme.tool_icon()),
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

/// Tool-call outcome — indicator (✓ / ✗), label, and per-view body.
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
        // Per-variant body dispatch — each tool's structured renderer owns its own gutter sizing,
        // truncation footer, and overflow rules. The `Text` arm is the catch-all fallback.
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
            ToolResultView::Diff {
                chunks,
                replace_all,
                replacements,
            } => {
                diff::render(
                    &mut out,
                    ctx,
                    chunks,
                    *replace_all,
                    *replacements,
                    self.is_error,
                );
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
    let cont_prefix = border_continuation_prefix(TOOL_BORDER_CONT, border_style);
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

/// Continuation prefix that keeps `▎` aligned under the original prefix.
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
        let spans = border_continuation_prefix(TOOL_BORDER_PREFIX, style);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "");
        assert_eq!(spans[1].content, BAR);
        assert_eq!(spans[2].content, " ");
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
        // Each `中` is 3 bytes; cutting at byte 5 without `floor_char_boundary` would split a
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
