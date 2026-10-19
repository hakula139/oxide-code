//! Error block: `✗ message` in the error color, with no left bar.

use ratatui::text::Line;

use super::rows::prefixed_row;
use super::{ChatBlock, RenderCtx};
use crate::tui::glyphs::{ERROR_CONT, ERROR_PREFIX};

/// An error shown in the chat, from the engine, a slash command, or the app. Multi-line messages
/// render one logical line per row, the icon on the first and a width-aligned indent on the rest.
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

        let mut out = Vec::new();
        for (i, body_line) in self.message.lines().enumerate() {
            let prefix = if i == 0 { ERROR_PREFIX } else { ERROR_CONT };
            prefixed_row(&mut out, ctx, prefix, style, body_line, style);
        }
        out
    }

    #[cfg(test)]
    fn error_text(&self) -> Option<&str> {
        Some(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ratatui::text::Span;

    use super::*;
    use crate::tui::theme::Theme;

    mod render {
        use super::*;

        fn ctx_at(width: u16, theme: &Theme) -> RenderCtx<'_> {
            RenderCtx {
                width,
                theme,
                show_thinking: false,
                expanded: false,
                expand_hint: None,
            }
        }

        #[test]
        fn single_line_message_emits_icon_then_body() {
            let theme = Theme::default();
            let block = ErrorBlock::new("boom");

            let lines = block.render(&ctx_at(60, &theme));

            let style = theme.error();
            assert_eq!(
                lines,
                [Line::from(vec![
                    Span::styled(ERROR_PREFIX, style),
                    Span::styled("boom", style),
                ])]
            );
        }

        #[test]
        fn multi_line_message_keeps_each_line_on_its_own_row() {
            // Slash-command errors emit markdown bullets (`/model gpt-4` → "Unknown model: ...").
            // Joining them at render time would collapse the bullets into one wrapped line.
            let theme = Theme::default();
            let block = ErrorBlock::new(indoc! {"
                Unknown model: `gpt-4`. Valid:

                - `claude-opus-5-5` — Claude Opus 5.5
                - `claude-haiku-5-5` — Claude Haiku 5.5"
            });
            let lines = block.render(&ctx_at(80, &theme));
            let bodies: Vec<&str> = lines
                .iter()
                .map(|l| l.spans.last().expect("non-empty").content.as_ref())
                .collect();
            assert_eq!(
                bodies,
                vec![
                    "Unknown model: `gpt-4`. Valid:",
                    "",
                    "- `claude-opus-5-5` — Claude Opus 5.5",
                    "- `claude-haiku-5-5` — Claude Haiku 5.5",
                ],
            );
        }

        #[test]
        fn continuation_rows_align_under_first_line_body() {
            // The first row starts with `✗ ` (icon and space), and later logical rows indent to the
            // same column so the message reads as one block.
            let theme = Theme::default();
            let block = ErrorBlock::new("first\nsecond");

            let lines = block.render(&ctx_at(60, &theme));

            let rows: Vec<String> = lines.iter().map(ToString::to_string).collect();
            assert_eq!(rows, ["✗ first", "  second"]);
        }
    }
}
