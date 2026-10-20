//! User message block.

use ratatui::text::Line;

use ox_core::util::text::trim_blank_edges;

use super::rows::prefixed_row;
use super::{ChatBlock, RenderCtx};
use crate::tui::glyphs::{USER_PROMPT_CONT, USER_PROMPT_PREFIX};
use crate::tui::wrap::expand_tabs;

/// A user-typed message.
pub(crate) struct UserMessage {
    text: String,
}

impl UserMessage {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl ChatBlock for UserMessage {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let icon_style = ctx.theme.user();
        let text_style = ctx.theme.text();

        let mut out = Vec::new();
        for (i, text_line) in trim_blank_edges(&self.text).lines().enumerate() {
            let prefix = if i == 0 {
                USER_PROMPT_PREFIX
            } else {
                USER_PROMPT_CONT
            };
            let text_line = expand_tabs(text_line);
            prefixed_row(&mut out, ctx, prefix, icon_style, text_line, text_style);
        }
        out
    }
}

#[cfg(test)]
mod tests {
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
        fn expands_tabs_to_stops() {
            let theme = Theme::default();
            let lines = UserMessage::new("a\tb\n\tc").render(&ctx_at(40, &theme));
            let rows: Vec<String> = lines.iter().map(ToString::to_string).collect();
            assert_eq!(rows, ["❯ a   b", "      c"]);
        }

        #[test]
        fn keeps_the_first_line_indent_and_drops_blank_edges() {
            let theme = Theme::default();

            let lines = UserMessage::new("\n    fn main() {\n        body\n    }\n\n")
                .render(&ctx_at(40, &theme));

            let rows: Vec<String> = lines.iter().map(ToString::to_string).collect();
            assert_eq!(rows, ["❯     fn main() {", "          body", "      }"]);
        }
    }
}
