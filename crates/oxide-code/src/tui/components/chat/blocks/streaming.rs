//! Streaming assistant block.
//!
//! Holds the in-flight assistant response as tokens arrive, with a
//! per-line rendered-prefix cache so committed text is not re-parsed on
//! every frame. Promoted to a committed [`super::AssistantText`] block
//! on `commit_streaming`.

use ratatui::text::{Line, Span};

use super::RenderCtx;
use super::assistant::{ASSISTANT_CONT, ASSISTANT_PREFIX, render_assistant_markdown};

/// Mutable streaming state for the current assistant response. Not a
/// [`ChatBlock`](super::ChatBlock) — its render path needs context from
/// the surrounding chat (whether it continues the prior turn) that the
/// trait doesn't carry.
pub(crate) struct StreamingAssistant {
    /// Full buffered text as it arrives.
    buffer: String,
    /// Rendered lines for the stable prefix of the buffer. Avoids
    /// re-parsing committed text on every frame.
    rendered: Vec<Line<'static>>,
    /// Byte offset in `buffer` up to which `rendered` is current. Text
    /// before this offset is cached; text past it needs parsing.
    rendered_boundary: usize,
    /// Viewport width at which `rendered` was produced. When the
    /// viewport resizes mid-stream, the cache must be cleared so lines
    /// re-wrap to the new width.
    cached_width: u16,
}

impl StreamingAssistant {
    pub(crate) fn new() -> Self {
        Self {
            buffer: String::new(),
            rendered: Vec::new(),
            rendered_boundary: 0,
            cached_width: 0,
        }
    }

    pub(crate) fn append(&mut self, token: &str) {
        self.buffer.push_str(token);
    }

    /// Take ownership of the accumulated buffer, leaving the streaming
    /// state empty. The caller promotes the buffer into an
    /// [`AssistantText`](super::AssistantText) block.
    pub(crate) fn take_buffer(&mut self) -> String {
        self.rendered.clear();
        self.rendered_boundary = 0;
        self.cached_width = 0;
        std::mem::take(&mut self.buffer)
    }

    /// Drop the cache when the viewport changes width so lines re-wrap.
    pub(crate) fn invalidate_cache_for_width(&mut self, width: u16) {
        if self.cached_width != 0 && self.cached_width != width {
            self.rendered.clear();
            self.rendered_boundary = 0;
            self.cached_width = 0;
        }
    }

    /// Advances the cache: renders newly committed lines (everything up
    /// to the last `\n`) and stores them so subsequent frames skip
    /// re-parsing the stable prefix.
    ///
    /// Defers until the viewport has been measured so the markdown
    /// renderer receives a real wrap width.
    pub(crate) fn advance_cache(&mut self, ctx: &RenderCtx<'_>, continues_turn: bool) {
        if ctx.width == 0 {
            return;
        }

        let boundary = self.rendered_boundary;
        let tail = &self.buffer[boundary..];
        let Some(rel_boundary) = tail.rfind('\n') else {
            return;
        };

        let new_committed = &self.buffer[boundary..boundary + rel_boundary];
        if !new_committed.is_empty() {
            let cache_empty = self.rendered.is_empty();
            let rendered =
                render_assistant_markdown(new_committed, ctx, !continues_turn && cache_empty);
            self.rendered.extend(rendered);
        }

        self.rendered_boundary = boundary + rel_boundary + 1;
        self.cached_width = ctx.width;
    }

    /// Render the streaming state into `out`.
    ///
    /// `continues_turn` is `true` when the preceding block is committed
    /// assistant text (same logical turn); it suppresses the leading
    /// icon and blank-line gap so streaming tokens flow into the block
    /// above.
    pub(crate) fn render_into(
        &self,
        out: &mut Vec<Line<'static>>,
        ctx: &RenderCtx<'_>,
        continues_turn: bool,
    ) {
        // Leading gap when starting a fresh turn and the transcript
        // already has content.
        if !continues_turn && super::last_has_width(out) {
            out.push(Line::raw(""));
        }

        out.extend(self.rendered.iter().cloned());

        let tail = &self.buffer[self.rendered_boundary..];
        if tail.is_empty() {
            return;
        }

        let cache_empty = self.rendered.is_empty();
        let (committed, trailing) = match tail.rfind('\n') {
            Some(nl) => (&tail[..nl], &tail[nl + 1..]),
            None => ("", tail),
        };

        if !committed.is_empty() {
            out.extend(render_assistant_markdown(
                committed,
                ctx,
                !continues_turn && cache_empty,
            ));
        }

        if !trailing.is_empty() {
            let starts_here = !continues_turn && cache_empty && committed.is_empty();
            let prefix = if starts_here {
                ASSISTANT_PREFIX
            } else {
                ASSISTANT_CONT
            };
            out.push(Line::from(vec![
                Span::styled(prefix.to_owned(), ctx.theme.secondary()),
                Span::styled(trailing.to_owned(), ctx.theme.text()),
            ]));
        }
    }

    // Test-only accessors for sibling-module tests that assert on
    // cache bookkeeping. Gated to keep the API surface minimal in
    // production builds.
    #[cfg(test)]
    pub(crate) fn rendered_boundary(&self) -> usize {
        self.rendered_boundary
    }

    #[cfg(test)]
    pub(crate) fn rendered_len(&self) -> usize {
        self.rendered.len()
    }

    #[cfg(test)]
    pub(crate) fn cached_width(&self) -> u16 {
        self.cached_width
    }
}
