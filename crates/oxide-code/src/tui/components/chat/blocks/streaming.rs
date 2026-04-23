//! Streaming assistant block.
//!
//! Holds the in-flight assistant response as tokens arrive, with a
//! per-line rendered-prefix cache so committed text is not re-parsed on
//! every frame. Promoted to a committed [`super::AssistantText`] block
//! on `commit_streaming`.

use ratatui::text::Line;

use super::RenderCtx;
use super::assistant::render_assistant_markdown;

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

    /// Advances the cache: renders newly committed paragraphs
    /// (everything up to the last `\n\n` paragraph boundary) and stores
    /// them so subsequent frames skip re-parsing the stable prefix.
    ///
    /// Splitting at `\n\n` rather than `\n` is what preserves
    /// inter-paragraph spacing: pulldown-cmark emits a blank separator
    /// between adjacent block-level elements only when it sees them in
    /// the same input. Committing chunk-by-chunk on line boundaries
    /// would feed the renderer fragments that each parse as a
    /// standalone paragraph, collapsing the gap between them.
    ///
    /// Defers until the viewport has been measured so the markdown
    /// renderer receives a real wrap width.
    pub(crate) fn advance_cache(&mut self, ctx: &RenderCtx<'_>, continues_turn: bool) {
        if ctx.width == 0 {
            return;
        }

        let boundary = self.rendered_boundary;
        let tail = &self.buffer[boundary..];
        let Some(rel_boundary) = tail.rfind("\n\n") else {
            return;
        };

        let new_committed = &self.buffer[boundary..boundary + rel_boundary];
        if !new_committed.trim().is_empty() {
            let cache_empty = self.rendered.is_empty();
            let rendered =
                render_assistant_markdown(new_committed, ctx, !continues_turn && cache_empty);
            if !self.rendered.is_empty() {
                // Paragraph break between successive commits — the
                // single-pass renderer would emit this blank line.
                self.rendered.push(Line::raw(""));
            }
            self.rendered.extend(rendered);
        }

        self.rendered_boundary = boundary + rel_boundary + 2;
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

        // Cache ends on a paragraph break; the tail is a new paragraph
        // (possibly still being typed). Insert the separator that a
        // single-pass renderer would produce between them.
        let cache_empty = self.rendered.is_empty();
        if !cache_empty {
            out.push(Line::raw(""));
        }

        // Full-tail parse: pulldown's block separators (blank-line
        // before list / heading / fence) only fire on a single-pass
        // parse. Splitting the tail off to render raw would strip
        // them — a bounded one-frame flash on unclosed inline markers
        // isn't worth that persistent bug.
        let starts_new_turn = !continues_turn && cache_empty;
        out.extend(render_assistant_markdown(tail, ctx, starts_new_turn));
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
