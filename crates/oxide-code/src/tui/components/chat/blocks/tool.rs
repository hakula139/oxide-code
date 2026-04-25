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
//! is a truncated text body; tools with structured output (Edit diffs,
//! Read excerpts today; Grep / Glob later) produce richer variants via
//! [`Tool::result_view`](crate::tool::Tool::result_view). The
//! variant-specific bodies live in sibling modules under [`tool`]; this
//! file owns block types, central dispatch, and the shared border
//! helpers child renderers reuse.

mod diff;
mod read_excerpt;
mod text;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{BAR, ChatBlock, RenderCtx};
use crate::tool::ToolResultView;
use crate::tui::theme::Theme;
use crate::tui::wrap::wrap_line;

/// Maximum lines of tool output shown inline before truncation. Shared
/// between the default text body and the read excerpt; both surface
/// the same hidden-line footer when the body overflows.
const MAX_TOOL_OUTPUT_LINES: usize = 5;

/// Maximum bytes per tool output line before horizontal truncation.
/// Measured in bytes (matched against `str::len`) rather than Unicode
/// characters — display width is already gated by the terminal width
/// budget; this cap exists to avoid pathological multi-kilobyte lines
/// pasted into tool output.
const MAX_TOOL_OUTPUT_LINE_BYTES: usize = 512;

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
                old,
                new,
                replace_all,
                replacements,
            } => {
                diff::render(
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

// ── Shared Helpers ──

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
/// call site passes either [`BORDER_PREFIX`], [`STATUS_LINE_CONT`], or
/// the diff-side continuation, all of which satisfy it.
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
}
