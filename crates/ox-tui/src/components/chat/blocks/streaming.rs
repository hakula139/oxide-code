//! Streaming assistant block. Buffers tokens with a rendered-prefix cache so completed markdown
//! blocks are not re-parsed each frame. Promoted to [`super::AssistantText`] on `commit_streaming`.

use ratatui::text::Line;

use super::RenderCtx;
use super::assistant::{assistant_markdown_width, with_assistant_gutter};
use super::markdown_cache::MarkdownCache;
use crate::markdown::render_markdown_chunk;

/// In-flight assistant turn buffered between stream tokens.
#[derive(Default)]
pub(crate) struct StreamingAssistant {
    buffer: String,
    cache: MarkdownCache,
}

impl StreamingAssistant {
    pub(crate) fn append(&mut self, token: &str) {
        self.buffer.push_str(token);
    }

    /// The streamed text, ending the turn's buffering.
    pub(crate) fn into_buffer(self) -> String {
        self.buffer
    }

    pub(crate) fn invalidate_cache_for_width(&mut self, width: u16) {
        self.cache.invalidate_for_width(width);
    }

    /// Drops the prefix cache so the next render re-parses the buffer with current styles.
    pub(crate) fn invalidate_cache(&mut self) {
        self.cache.invalidate();
    }

    /// Caches rendered lines for every top-level block that is complete.
    pub(crate) fn advance_cache(&mut self, ctx: &RenderCtx<'_>, continues_turn: bool) {
        let starts_new_turn = !continues_turn && self.cache.rendered().is_empty();
        let render = chunk_renderer(ctx, starts_new_turn);
        self.cache.advance(&self.buffer, ctx.width, render);
    }

    pub(crate) fn rendered(&self) -> &[Line<'static>] {
        self.cache.rendered()
    }

    /// Renders the uncached tail in one parse so pulldown keeps its block separators.
    pub(crate) fn render_tail(
        &self,
        ctx: &RenderCtx<'_>,
        continues_turn: bool,
    ) -> Vec<Line<'static>> {
        let starts_new_turn = !continues_turn && self.cache.rendered().is_empty();
        self.cache
            .tail(&self.buffer, chunk_renderer(ctx, starts_new_turn))
    }
}

/// Paints one chunk, with the assistant icon only when the chunk opens a new turn.
fn chunk_renderer<'a>(
    ctx: &'a RenderCtx<'_>,
    starts_new_turn: bool,
) -> impl Fn(&str, bool) -> (Vec<Line<'static>>, bool) + 'a {
    let width = assistant_markdown_width(ctx);
    move |chunk, after_block| {
        let (text, after_block) = render_markdown_chunk(chunk, ctx.theme, width, after_block);
        (
            with_assistant_gutter(text.lines, ctx, starts_new_turn),
            after_block,
        )
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::components::chat::blocks::{AssistantText, ChatBlock as _};
    use crate::theme::Theme;

    /// Streams `tokens` into a new turn, advancing the cache after each one as the chat does.
    fn streamed(tokens: &[&str], ctx: &RenderCtx<'_>) -> StreamingAssistant {
        let mut streaming = StreamingAssistant::default();
        for token in tokens {
            streaming.append(token);
            streaming.advance_cache(ctx, false);
        }
        streaming
    }

    fn rows(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    mod invalidate_cache_for_width {
        use super::*;

        #[test]
        fn drops_the_cache_only_when_the_width_changes() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(80, &theme);
            for (width, cached) in [(80, &["◉ para1"][..]), (40, &[])] {
                let mut streaming = streamed(&["para1\n\npara2\n"], &ctx);

                streaming.invalidate_cache_for_width(width);

                assert_eq!(rows(streaming.rendered()), cached, "width {width}");
            }
        }
    }

    mod invalidate_cache {
        use super::*;

        #[test]
        fn leaves_the_whole_buffer_to_the_tail() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(80, &theme);
            let mut streaming = streamed(&["para1\n\npara2\n"], &ctx);

            streaming.invalidate_cache();

            assert_eq!(rows(streaming.rendered()), [] as [String; 0]);
            assert_eq!(
                rows(&streaming.render_tail(&ctx, false)),
                ["◉ para1", "  ", "  para2"]
            );
        }
    }

    mod advance_cache {
        use super::*;

        #[test]
        fn separates_each_cached_block_from_the_next() {
            let theme = Theme::default();

            let streaming = streamed(
                &["para1\n\npara2\n", "\npara3\n"],
                &RenderCtx::new(80, &theme),
            );

            assert_eq!(rows(streaming.rendered()), ["◉ para1", "  ", "  para2"]);
        }

        #[test]
        fn matches_committed_render_at_every_char() {
            let body = indoc! {"
                ```
                ```

                Here:

                | a | b |
                | - | - |
                | 1 | 2 |

                text
                #tag

                para
                ```x
                code
                ```

                ```c
                #include <a.h>

                #define X 1
                - not a bullet
                ```

                [ref]: https://example.com

                ```
                ```

                <div>
                html
                </div>

                Done.
            "};
            let theme = Theme::default();
            let ctx = RenderCtx::new(80, &theme);
            let mut streaming = StreamingAssistant::default();
            for (i, ch) in body.char_indices() {
                streaming.append(&ch.to_string());

                streaming.advance_cache(&ctx, false);

                let prefix = &body[..i + ch.len_utf8()];
                let lines = [streaming.rendered(), &streaming.render_tail(&ctx, false)].concat();
                assert_eq!(
                    lines,
                    AssistantText::new(prefix).render(&ctx),
                    "after {prefix:?}"
                );
            }
        }
    }
}
