//! Assistant text, thinking, and progress-update blocks.

use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use super::{ChatBlock, RenderCtx, SectionStyles, bar_section, prepend_markdown_prefix};
use crate::tui::glyphs::{ASSISTANT_CONT, ASSISTANT_PREFIX, THINKING_PREFIX};
use crate::tui::markdown::render_markdown;

/// Header label on the first line of a thinking block.
const THINKING_LABEL: &str = "Thinking...";

// ── AssistantText ──

/// A committed assistant text response, rendered through the markdown pipeline.
pub(crate) struct AssistantText {
    text: String,
}

impl AssistantText {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl ChatBlock for AssistantText {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let text = render_markdown(&self.text, ctx.theme, assistant_markdown_width(ctx));
        with_assistant_gutter(text.lines, ctx, true)
    }

    fn continues_assistant_turn(&self) -> bool {
        true
    }
}

/// Column budget for assistant markdown, which renders behind the assistant gutter.
pub(super) fn assistant_markdown_width(ctx: &RenderCtx<'_>) -> usize {
    usize::from(ctx.width).saturating_sub(ASSISTANT_PREFIX.width())
}

/// Prefixes markdown rows with the assistant gutter, putting the icon on the first row when it
/// starts a new turn.
pub(super) fn with_assistant_gutter(
    lines: Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    starts_new_turn: bool,
) -> Vec<Line<'static>> {
    let icon_style = ctx.theme.assistant();
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let prefix = if i == 0 && starts_new_turn {
                ASSISTANT_PREFIX
            } else {
                ASSISTANT_CONT
            };
            prepend_markdown_prefix(line, prefix, icon_style)
        })
        .collect()
}

// ── AssistantThinking ──

/// Extended-thinking block. Hidden when `show_thinking` is off.
pub(crate) struct AssistantThinking {
    text: String,
}

impl AssistantThinking {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl ChatBlock for AssistantThinking {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let style = ctx.theme.thinking();
        let styles = SectionStyles {
            bar: style,
            header: style,
            body: style,
        };
        bar_section(ctx, THINKING_PREFIX, &styles, THINKING_LABEL, &self.text)
    }

    fn visible(&self, ctx: &RenderCtx<'_>) -> bool {
        ctx.show_thinking
    }
}

// ── ProgressUpdate ──

/// A note the model wrote before a tool call, rendered as assistant prose. Hidden when
/// `show_thinking` is on, since the thinking block already shows the same text.
pub(crate) struct ProgressUpdate {
    text: String,
}

impl ProgressUpdate {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl ChatBlock for ProgressUpdate {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let text = render_markdown(&self.text, ctx.theme, assistant_markdown_width(ctx));
        with_assistant_gutter(text.lines, ctx, true)
    }

    fn visible(&self, ctx: &RenderCtx<'_>) -> bool {
        !ctx.show_thinking
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ratatui::text::Span;

    use super::*;
    use crate::tui::theme::Theme;

    fn ctx_at(width: u16, theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            width,
            theme,
            show_thinking: true,
        }
    }

    mod render {
        use super::*;

        #[test]
        fn fenced_code_preserves_highlight_style() {
            // `prepend_markdown_prefix` must keep `line.style` so the whole-line fg of unknown-lang
            // fence blocks survives into the assistant render.
            let theme = Theme::default();
            let block = AssistantText::new(indoc! {"
                ```
                let x = 1;
                ```
            "});
            let ctx = ctx_at(60, &theme);
            let lines = block.render(&ctx);
            let fence_line = lines
                .iter()
                .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 1;")))
                .expect("fence body line missing from render");
            assert_eq!(fence_line.style.fg, theme.code().fg);
        }
    }

    mod assistant_thinking {
        use super::*;

        mod render {
            use super::*;

            #[test]
            fn empty_body_emits_header_only() {
                // Thinking that holds only whitespace renders as the bare header.
                let theme = Theme::default();
                let block = AssistantThinking::new("   \n  ");

                let lines = block.render(&ctx_at(60, &theme));

                let style = theme.thinking();
                assert_eq!(
                    lines,
                    [Line::from(vec![
                        Span::styled(THINKING_PREFIX, style),
                        Span::styled(THINKING_LABEL, style),
                    ])]
                );
            }

            #[test]
            fn paints_header_gutter_and_plain_body_in_the_thinking_style() {
                let theme = Theme::default();
                let block = AssistantThinking::new("hello");

                let lines = block.render(&ctx_at(60, &theme));

                let style = theme.thinking();
                assert_eq!(
                    lines,
                    [
                        Line::from(vec![
                            Span::styled(THINKING_PREFIX, style),
                            Span::styled(THINKING_LABEL, style),
                        ]),
                        Line::from(Span::styled(THINKING_PREFIX, style)),
                        Line::from(vec![
                            Span::styled(THINKING_PREFIX, style),
                            Span::styled("hello", style),
                        ]),
                    ]
                );
            }

            #[test]
            fn fenced_code_block_preserves_highlight_style() {
                // Whole-line fg on fence output must survive the bar prefix.
                let theme = Theme::default();
                let block = AssistantThinking::new(indoc! {"
                    Consider:

                    ```
                    let x = 1;
                    ```
                "});
                let lines = block.render(&ctx_at(60, &theme));

                let fence_line = lines
                    .iter()
                    .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 1;")))
                    .expect("fence body line missing from render");
                assert_eq!(fence_line.style.fg, theme.code().fg);

                let first_span = fence_line.spans.first().expect("empty fence line");
                assert_eq!(first_span.content, THINKING_PREFIX);
                assert_eq!(first_span.style, theme.thinking());
            }
        }
    }

    mod progress_update {
        use super::*;

        #[test]
        fn shows_as_assistant_text_only_while_thinking_hidden() {
            let theme = Theme::default();
            let block = ProgressUpdate::new("Found the loader.");
            let mut ctx = ctx_at(60, &theme);
            assert!(
                !block.visible(&ctx),
                "the thinking block already shows the text"
            );

            ctx.show_thinking = false;
            assert!(block.visible(&ctx));
            assert_eq!(
                block.render(&ctx),
                AssistantText::new("Found the loader.").render(&ctx),
            );
        }
    }
}
