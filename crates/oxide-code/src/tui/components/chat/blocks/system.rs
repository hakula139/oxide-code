//! Slash-command output block: an `accent` `▎` bar beside the body in `text`. Errors use
//! [`super::ErrorBlock`], which keeps failures visually distinct from this output.

use ratatui::text::{Line, Span};

use super::rows::prefixed_row;
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
    #[cfg(test)]
    fn system_text(&self) -> Option<&str> {
        Some(&self.text)
    }

    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let bar_style = ctx.theme.accent();
        let mut out = Vec::new();
        for body_line in self.text.lines() {
            prefixed_row(
                &mut out,
                ctx,
                TOOL_BORDER_PREFIX,
                bar_style,
                body_line,
                ctx.theme.text(),
            );
        }
        if out.is_empty() {
            // Empty content renders a bare bar, so the block stays visible.
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
    use indoc::indoc;

    use super::*;
    use crate::tui::glyphs::BAR;
    use crate::tui::theme::Theme;

    mod render {
        use super::*;

        fn ctx_at(width: u16, theme: &Theme) -> RenderCtx<'_> {
            RenderCtx {
                width,
                theme,
                show_thinking: false,
            }
        }

        fn rows(lines: &[Line<'_>]) -> Vec<String> {
            lines.iter().map(ToString::to_string).collect()
        }

        #[test]
        fn each_input_line_gets_bar_prefix() {
            let theme = Theme::default();
            let block = SystemMessageBlock::new(indoc! {"
                first
                second
                third
            "});

            let lines = block.render(&ctx_at(60, &theme));

            let row = |text| {
                Line::from(vec![
                    Span::styled(TOOL_BORDER_PREFIX, theme.accent()),
                    Span::styled(text, theme.text()),
                ])
            };
            assert_eq!(lines, [row("first"), row("second"), row("third")]);
        }

        #[test]
        fn empty_text_still_emits_a_bar_line() {
            // Silently dropping empty payloads would mask call-site bugs.
            let theme = Theme::default();
            let block = SystemMessageBlock::new("");

            let lines = block.render(&ctx_at(60, &theme));

            assert_eq!(
                lines,
                [Line::from(Span::styled(TOOL_BORDER_PREFIX, theme.accent()))]
            );
        }

        #[test]
        fn trailing_newline_does_not_emit_extra_blank_row() {
            // Pins `str::lines()` semantics, where `split('\n')` would add an empty last row.
            let theme = Theme::default();
            let block = SystemMessageBlock::new(indoc! {"
                alpha
                beta
            "});

            let lines = block.render(&ctx_at(60, &theme));

            assert_eq!(rows(&lines), ["▎ alpha", "▎ beta"]);
        }

        #[test]
        fn wraps_long_body_under_bar_at_viewport_width() {
            // Continuation lines must re-emit the bar so the block reads as one visual unit.
            let theme = Theme::default();
            let block = SystemMessageBlock::new("alpha beta gamma delta epsilon zeta");

            let lines = block.render(&ctx_at(16, &theme));

            assert_eq!(
                rows(&lines),
                ["▎ alpha beta ", "▎ gamma delta ", "▎ epsilon zeta"]
            );
            for (i, line) in lines.iter().enumerate() {
                let head = &line.spans[0];
                let content = head.content.as_ref();
                assert!(
                    content.starts_with(BAR),
                    "row {i} bar prefix missing: {content:?}",
                );
                assert_eq!(head.style, theme.accent());
            }
        }

        #[test]
        fn per_logical_line_wraps_independently() {
            // Joining input lines would render two source lines as one paragraph.
            let theme = Theme::default();
            let block = SystemMessageBlock::new(
                "first really long line of text\nsecond really long line of text",
            );

            let lines = block.render(&ctx_at(20, &theme));

            assert_eq!(
                rows(&lines),
                [
                    "▎ first really long ",
                    "▎ line of text",
                    "▎ second really long",
                    "▎ line of text",
                ]
            );
        }
    }
}
