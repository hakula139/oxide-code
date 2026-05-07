//! Error block — `✗ message` in the error color, no left bar.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{ChatBlock, RenderCtx};
use crate::tui::glyphs::ERROR_PREFIX;
use crate::tui::wrap::wrap_line;

/// A fatal agent or API error. Multi-line messages render one logical line per row, the icon
/// on the first and a width-aligned indent on the rest.
pub(crate) struct ErrorBlock {
    message: String,
}

impl ErrorBlock {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl ChatBlock for ErrorBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let style = ctx.theme.error();
        let width = usize::from(ctx.width);
        let indent = ERROR_PREFIX.width();
        let cont_prefix = vec![Span::raw(" ".repeat(indent))];
        let mut out = Vec::new();
        for (i, body_line) in self.message.lines().enumerate() {
            let prefix = if i == 0 {
                ERROR_PREFIX.to_owned()
            } else {
                " ".repeat(indent)
            };
            let line = Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(body_line.to_owned(), style),
            ]);
            out.extend(wrap_line(line, width, indent, Some(&cont_prefix)));
        }
        out
    }

    fn is_error_marker(&self) -> bool {
        true
    }

    #[cfg(test)]
    fn error_text(&self) -> Option<&str> {
        Some(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

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
    fn render_single_line_message_emits_icon_then_body() {
        let theme = Theme::default();
        let block = ErrorBlock::new("boom");
        let lines = block.render(&ctx_at(60, &theme));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, ERROR_PREFIX);
        assert_eq!(lines[0].spans[1].content, "boom");
    }

    #[test]
    fn render_multi_line_message_keeps_each_line_on_its_own_row() {
        // Slash-command errors emit markdown bullets (`/model gpt-4` → "Unknown model: ...").
        // Joining them at render time would collapse the bullets into one wrapped line.
        let theme = Theme::default();
        let block = ErrorBlock::new(indoc! {"
            Unknown model: `gpt-4`. Supported models:

            - `claude-opus-4-7` — Claude Opus 4.7
            - `claude-haiku-4-5` — Claude Haiku 4.5"
        });
        let lines = block.render(&ctx_at(80, &theme));
        let bodies: Vec<&str> = lines
            .iter()
            .map(|l| l.spans.last().expect("non-empty").content.as_ref())
            .collect();
        assert_eq!(
            bodies,
            vec![
                "Unknown model: `gpt-4`. Supported models:",
                "",
                "- `claude-opus-4-7` — Claude Opus 4.7",
                "- `claude-haiku-4-5` — Claude Haiku 4.5",
            ],
        );
    }

    #[test]
    fn render_continuation_rows_align_under_first_line_body() {
        // First-row prefix is `✗ ` (icon + space); follow-up logical rows must indent to the
        // same column so the message reads as one block.
        let theme = Theme::default();
        let block = ErrorBlock::new("first\nsecond");
        let lines = block.render(&ctx_at(60, &theme));
        let indent = ERROR_PREFIX.width();
        assert_eq!(lines[0].spans[0].content, ERROR_PREFIX);
        assert_eq!(lines[1].spans[0].content, " ".repeat(indent));
    }
}
