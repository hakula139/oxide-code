//! Assistant text, thinking, and progress-update blocks.

use ratatui::style::Style;
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr as _;

use super::rows::prefixed_row;
use super::{
    BlockRows, ChatBlock, LiveMarkdown, RenderCtx, SectionStyles, bar_section,
    prepend_markdown_prefix, streaming_bar_section,
};
use crate::glyphs::{ASSISTANT_CONT, ASSISTANT_PREFIX, THINKING_PREFIX};
use crate::markdown::render_markdown;

/// Header of thinking that is still streaming, after the spinner.
const THINKING_LABEL: &str = "Thinking";

/// Header of committed thinking.
const THOUGHT_LABEL: &str = "Thought";

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
        assistant_markdown(&self.text, ctx)
    }

    fn continues_assistant_turn(&self) -> bool {
        true
    }
}

/// Renders `text` as markdown that opens an assistant turn behind the assistant gutter.
fn assistant_markdown(text: &str, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
    let width = assistant_markdown_width(ctx);
    let text = render_markdown(text, ctx.theme, Style::default(), width);
    with_assistant_gutter(text.lines, ctx, true)
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

/// Committed extended thinking, headed `Thought`. The compact view collapses it to the header.
/// Hidden when `show_thinking` is off.
pub(crate) struct AssistantThinking {
    text: String,
}

impl AssistantThinking {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// Renders thinking still streaming in, headed by `spinner` and `Thinking`, with its body in
    /// either view.
    pub(crate) fn render_streaming(
        text: &mut LiveMarkdown,
        spinner: char,
        ctx: &RenderCtx<'_>,
    ) -> Vec<Line<'static>> {
        let header = format!("{spinner} {THINKING_LABEL}");
        streaming_bar_section(ctx, THINKING_PREFIX, &thinking_styles(ctx), &header, text)
    }
}

impl ChatBlock for AssistantThinking {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        self.render_rows(ctx).lines
    }

    fn render_rows(&self, ctx: &RenderCtx<'_>) -> BlockRows {
        let styles = thinking_styles(ctx);
        let mut out = BlockRows::default();
        if ctx.expanded || self.text.trim().is_empty() {
            out.lines = bar_section(ctx, THINKING_PREFIX, &styles, THOUGHT_LABEL, &self.text);
            return out;
        }
        let header = ctx.with_expand_hint(THOUGHT_LABEL);
        out.push_expander(|lines| {
            prefixed_row(
                lines,
                ctx,
                THINKING_PREFIX,
                styles.bar,
                header,
                styles.header,
            );
        });
        out
    }

    fn visible(&self, show_thinking: bool) -> bool {
        show_thinking
    }
}

fn thinking_styles(ctx: &RenderCtx<'_>) -> SectionStyles {
    let style = ctx.theme.thinking();
    SectionStyles {
        bar: style,
        header: style,
        body: style,
    }
}

// ── ProgressUpdate ──

/// A note the model wrote before a tool call, rendered as assistant prose. Hidden when
/// `show_thinking` is on, since the thinking block then holds the same text.
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
        assistant_markdown(&self.text, ctx)
    }

    fn visible(&self, show_thinking: bool) -> bool {
        !show_thinking
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ratatui::text::Span;

    use super::*;
    use crate::theme::Theme;

    fn expanded_at(width: u16, theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            expanded: true,
            ..RenderCtx::new(width, theme)
        }
    }

    mod assistant_text {
        use super::*;

        mod render {
            use super::*;

            #[test]
            fn fenced_code_preserves_highlight_style() {
                // `prepend_markdown_prefix` must keep `line.style` so the whole-line fg of
                // unknown-lang fence blocks survives into the assistant render.
                let theme = Theme::default();
                let block = AssistantText::new(indoc! {"
                    ```
                    let x = 1;
                    ```
                "});
                let ctx = RenderCtx::new(60, &theme);
                let lines = block.render(&ctx);
                let fence_line = lines
                    .iter()
                    .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 1;")))
                    .expect("fence body line missing from render");
                assert_eq!(fence_line.style.fg, theme.code().fg);
            }
        }
    }

    mod render_streaming {
        use super::*;

        #[test]
        fn heads_the_body_with_the_spinner_in_either_view() {
            let theme = Theme::default();
            let style = theme.thinking();

            for ctx in [RenderCtx::new(60, &theme), expanded_at(60, &theme)] {
                let mut text = LiveMarkdown::default();
                text.push_str("hello");

                let lines = AssistantThinking::render_streaming(&mut text, '⣷', &ctx);

                assert_eq!(
                    lines,
                    [
                        Line::from(vec![
                            Span::styled(THINKING_PREFIX, style),
                            Span::styled("⣷ Thinking", style),
                        ]),
                        Line::from(Span::styled(THINKING_PREFIX, style)),
                        Line::from(vec![
                            Span::styled(THINKING_PREFIX, style),
                            Span::styled("hello", style),
                        ]),
                    ]
                );
            }
        }

        #[test]
        fn matches_the_committed_body_at_every_char() {
            let body = indoc! {"
                para one
                #tag

                - item 1
                - item 2

                ```c
                #include <a.h>
                ```

                | a | b |
                | - | - |
                | 1 | 2 |

                Done.
            "};
            let theme = Theme::default();
            let ctx = expanded_at(60, &theme);
            let mut text = LiveMarkdown::default();
            for ch in body.chars() {
                text.push_str(ch.encode_utf8(&mut [0; 4]));
                let prefix = text.text().to_owned();

                let streaming = AssistantThinking::render_streaming(&mut text, '⣷', &ctx);
                let committed = AssistantThinking::new(prefix.as_str()).render(&ctx);

                assert_eq!(streaming[1..], committed[1..], "after {prefix:?}");
            }
        }
    }

    mod render_rows {
        use super::*;

        #[test]
        fn collapses_to_a_thought_header_that_expands_it_in_the_compact_view() {
            let theme = Theme::default();
            let ctx = RenderCtx {
                expand_hint: Some("Ctrl+O to expand"),
                ..RenderCtx::new(60, &theme)
            };
            let block = AssistantThinking::new("hello");

            let rows = block.render_rows(&ctx);

            let style = theme.thinking();
            assert_eq!(
                rows.lines,
                [Line::from(vec![
                    Span::styled(THINKING_PREFIX, style),
                    Span::styled("Thought · Ctrl+O to expand", style),
                ])]
            );
            assert_eq!(rows.expander_rows(), [0]);
        }

        #[test]
        fn empty_body_emits_a_header_that_expands_nothing() {
            let theme = Theme::default();
            let ctx = RenderCtx {
                expand_hint: Some("Ctrl+O to expand"),
                ..RenderCtx::new(60, &theme)
            };
            let block = AssistantThinking::new("   \n  ");

            let rows = block.render_rows(&ctx);

            let style = theme.thinking();
            assert_eq!(
                rows.lines,
                [Line::from(vec![
                    Span::styled(THINKING_PREFIX, style),
                    Span::styled(THOUGHT_LABEL, style),
                ])]
            );
            assert_eq!(rows.expander_rows(), Vec::<usize>::new());
        }

        #[test]
        fn paints_header_gutter_and_plain_body_in_the_thinking_style_when_expanded() {
            let theme = Theme::default();
            let block = AssistantThinking::new("hello");

            let lines = block.render(&expanded_at(60, &theme));

            let style = theme.thinking();
            assert_eq!(
                lines,
                [
                    Line::from(vec![
                        Span::styled(THINKING_PREFIX, style),
                        Span::styled(THOUGHT_LABEL, style),
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
            let lines = block.render(&expanded_at(60, &theme));

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

    mod progress_update {
        use super::*;

        #[test]
        fn shows_as_assistant_text_only_while_thinking_is_hidden() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(60, &theme);
            let block = ProgressUpdate::new("Found the loader.");

            let visible = [true, false].map(|show_thinking| block.visible(show_thinking));
            let rows = block.render(&ctx);

            assert_eq!(visible, [false, true], "the thinking block holds the text");
            assert_eq!(rows, AssistantText::new("Found the loader.").render(&ctx));
        }
    }
}
