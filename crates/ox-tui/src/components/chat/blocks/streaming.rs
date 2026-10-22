//! Streaming assistant block. Buffers tokens with a rendered-prefix cache so completed markdown
//! blocks are not re-parsed each frame. Promoted to [`super::AssistantText`] on `commit_streaming`.

use ratatui::text::Line;

use super::RenderCtx;
use super::assistant::{assistant_markdown_width, with_assistant_gutter};
use crate::markdown::{complete_blocks_len, render_markdown_chunk};

/// In-flight assistant turn buffered between stream tokens.
///
/// Complete top-level markdown blocks are pre-rendered into `rendered`, so each frame only
/// re-parses the tail from the last block start. The cache is keyed by viewport width.
#[derive(Default)]
pub(crate) struct StreamingAssistant {
    buffer: String,
    /// Rendered lines for the complete blocks before `rendered_boundary`.
    rendered: Vec<Line<'static>>,
    rendered_boundary: usize,
    /// Whether the cached chunks end after a block, so the next chunk opens with a separator.
    after_block: bool,
    /// Width at which `rendered` was produced.
    cached_width: u16,
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
        if self.cached_width != 0 && self.cached_width != width {
            self.invalidate_cache();
        }
    }

    /// Drops the prefix cache so the next render re-parses the buffer with current styles.
    pub(crate) fn invalidate_cache(&mut self) {
        self.rendered.clear();
        self.rendered_boundary = 0;
        self.after_block = false;
        self.cached_width = 0;
    }

    /// Caches rendered lines for every top-level block that is complete.
    pub(crate) fn advance_cache(&mut self, ctx: &RenderCtx<'_>, continues_turn: bool) {
        if ctx.width == 0 {
            return;
        }

        let boundary = self.rendered_boundary;
        let tail = &self.buffer[boundary..];
        // A partial last line can still change which block it starts, so only whole lines count.
        let Some(last_newline) = tail.rfind('\n') else {
            return;
        };
        let complete_len = complete_blocks_len(&tail[..=last_newline]);
        if complete_len == 0 {
            return;
        }

        let (rows, after_block) = self.render_chunk(&tail[..complete_len], ctx, continues_turn);
        self.rendered.extend(rows);
        self.after_block = after_block;

        self.rendered_boundary = boundary + complete_len;
        self.cached_width = ctx.width;
    }

    pub(crate) fn rendered(&self) -> &[Line<'static>] {
        &self.rendered
    }

    /// Renders the uncached tail in one parse so pulldown keeps its block separators.
    pub(crate) fn render_tail(
        &self,
        ctx: &RenderCtx<'_>,
        continues_turn: bool,
    ) -> Vec<Line<'static>> {
        let tail = &self.buffer[self.rendered_boundary..];
        self.render_chunk(tail, ctx, continues_turn).0
    }

    /// Renders the chunk after the cached rows, returning its rows and whether it ends after a
    /// block. The chunk takes the assistant icon only when it opens a new turn.
    fn render_chunk(
        &self,
        chunk: &str,
        ctx: &RenderCtx<'_>,
        continues_turn: bool,
    ) -> (Vec<Line<'static>>, bool) {
        let width = assistant_markdown_width(ctx);
        let (text, after_block) = render_markdown_chunk(chunk, ctx.theme, width, self.after_block);
        let starts_new_turn = !continues_turn && self.rendered.is_empty();
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
        fn waits_for_a_measured_width() {
            let theme = Theme::default();

            let streaming = streamed(&["first paragraph\n\nsecond\n"], &RenderCtx::new(0, &theme));

            assert_eq!(streaming.rendered_boundary, 0);
            assert_eq!(streaming.cached_width, 0);
        }

        #[test]
        fn caches_only_the_blocks_a_finished_line_closes() {
            let theme = Theme::default();
            let ctx = RenderCtx::new(80, &theme);
            for (buffer, cached) in [
                ("no newline here", ""),
                ("first line\nsecond line", ""),
                ("para1\n\npartial", ""),
                ("para1\n\npartial\n", "para1\n\n"),
                ("p1\n\np2\n\np3\n\np4\n", "p1\n\np2\n\np3\n\n"),
                // A loose list continues past a blank line until another block starts.
                ("- item 1\n\n", ""),
                (
                    "- item 1\n\n- item 2\n\nafter\n",
                    "- item 1\n\n- item 2\n\n",
                ),
            ] {
                let streaming = streamed(&[buffer], &ctx);

                assert_eq!(
                    &streaming.buffer[..streaming.rendered_boundary],
                    cached,
                    "{buffer:?}"
                );
            }
        }

        #[test]
        fn separates_each_cached_block_from_the_next() {
            let theme = Theme::default();

            let streaming = streamed(
                &["para1\n\npara2\n", "\npara3\n"],
                &RenderCtx::new(80, &theme),
            );

            assert_eq!(rows(streaming.rendered()), ["◉ para1", "  ", "  para2"]);
            assert_eq!(streaming.cached_width, 80);
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
