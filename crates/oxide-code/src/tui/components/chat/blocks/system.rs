//! System message block — multi-line slash-command output, rendered
//! with a `▎` left-bar in `accent` and the body in `text`.
//!
//! Used by `/help`, `/status`, `/config`, `/diff`, and `/init`
//! confirmation. Errors keep their own `ErrorBlock` styling — the
//! left-bar variant is reserved for informational output so the user
//! can scan a transcript and tell at a glance which lines are
//! agent-emitted vs. command-emitted.

use ratatui::text::{Line, Span};

use super::{ChatBlock, RenderCtx};
use crate::tui::glyphs::TOOL_BORDER_PREFIX;

/// Output from a locally-dispatched slash command.
pub(crate) struct SystemMessageBlock {
    text: String,
}

impl SystemMessageBlock {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl ChatBlock for SystemMessageBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let bar_style = ctx.theme.accent();
        let body_style = ctx.theme.text();
        let mut out = Vec::new();
        for body_line in self.text.lines() {
            out.push(Line::from(vec![
                Span::styled(TOOL_BORDER_PREFIX.to_owned(), bar_style),
                Span::styled(body_line.to_owned(), body_style),
            ]));
        }
        if out.is_empty() {
            // Empty content still gets a single bar so the block is
            // visible — better than silently dropping the call.
            out.push(Line::from(Span::styled(
                TOOL_BORDER_PREFIX.to_owned(),
                bar_style,
            )));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

    fn ctx_at(width: u16, theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            width,
            theme,
            show_thinking: false,
        }
    }

    // ── render ──

    #[test]
    fn render_each_input_line_gets_bar_prefix() {
        let theme = Theme::default();
        let block = SystemMessageBlock::new("first\nsecond\nthird");
        let lines = block.render(&ctx_at(60, &theme));
        assert_eq!(lines.len(), 3);
        for (i, expected) in ["first", "second", "third"].iter().enumerate() {
            assert_eq!(lines[i].spans.len(), 2, "row {i}: bar + body");
            assert_eq!(lines[i].spans[0].content, TOOL_BORDER_PREFIX);
            assert_eq!(lines[i].spans[0].style, theme.accent());
            assert_eq!(lines[i].spans[1].content, *expected);
            assert_eq!(lines[i].spans[1].style, theme.text());
        }
    }

    #[test]
    fn render_empty_text_still_emits_a_bar_line() {
        // Tests / edge cases that hand in an empty payload should
        // still produce a visible block — silently swallowing it
        // would hide a bug at the call site.
        let theme = Theme::default();
        let block = SystemMessageBlock::new("");
        let lines = block.render(&ctx_at(60, &theme));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].content, TOOL_BORDER_PREFIX);
    }

    #[test]
    fn render_trailing_newline_does_not_emit_extra_blank_row() {
        // `str::lines()` already drops the final empty fragment from a
        // trailing newline; pin that contract so a future switch to
        // `split('\n')` would fail visibly here.
        let theme = Theme::default();
        let block = SystemMessageBlock::new("alpha\nbeta\n");
        let lines = block.render(&ctx_at(60, &theme));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].spans[1].content, "beta");
    }
}
