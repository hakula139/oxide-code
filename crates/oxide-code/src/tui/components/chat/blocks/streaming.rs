//! Streaming assistant block. Buffers tokens with a rendered-prefix cache so completed markdown
//! blocks are not re-parsed each frame. Promoted to [`super::AssistantText`] on `commit_streaming`.

use ratatui::text::Line;

use super::RenderCtx;
use super::assistant::{assistant_blank_line, render_assistant_markdown};
use crate::tui::markdown::complete_blocks_len;

/// In-flight assistant turn buffered between stream tokens.
///
/// Complete top-level markdown blocks are pre-rendered into `rendered`, so each frame only
/// re-parses the tail from the last block start. The cache is keyed by viewport width.
pub(crate) struct StreamingAssistant {
    buffer: String,
    /// Rendered lines for the complete blocks before `rendered_boundary`.
    rendered: Vec<Line<'static>>,
    rendered_boundary: usize,
    /// Width at which `rendered` was produced.
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

    pub(crate) fn take_buffer(&mut self) -> String {
        self.invalidate_cache();
        std::mem::take(&mut self.buffer)
    }

    pub(crate) fn invalidate_cache_for_width(&mut self, width: u16) {
        if self.cached_width != 0 && self.cached_width != width {
            self.invalidate_cache();
        }
    }

    /// Drop the prefix cache so the next render re-parses the buffer with current styles.
    pub(crate) fn invalidate_cache(&mut self) {
        self.rendered.clear();
        self.rendered_boundary = 0;
        self.cached_width = 0;
    }

    /// Caches rendered lines for every top-level block that is complete.
    pub(crate) fn advance_cache(&mut self, ctx: &RenderCtx<'_>, continues_turn: bool) {
        if ctx.width == 0 {
            return;
        }

        let boundary = self.rendered_boundary;
        let tail = &self.buffer[boundary..];
        // A second block can only start after a line break.
        if !tail.contains('\n') {
            return;
        }
        let complete_len = complete_blocks_len(tail);
        if complete_len == 0 {
            return;
        }

        let complete = &tail[..complete_len];
        if !complete.trim().is_empty() {
            let cache_empty = self.rendered.is_empty();
            let rendered = render_assistant_markdown(complete, ctx, !continues_turn && cache_empty);
            if !cache_empty {
                // Block separator that the single-pass renderer would emit between commits.
                self.rendered.push(assistant_blank_line(ctx));
            }
            self.rendered.extend(rendered);
        }

        self.rendered_boundary = boundary + complete_len;
        self.cached_width = ctx.width;
    }

    pub(crate) fn rendered(&self) -> &[Line<'static>] {
        &self.rendered
    }

    /// Renders the uncached tail, led by a separator when blocks are cached. The tail takes the
    /// assistant icon only when it opens a new turn.
    pub(crate) fn render_tail(
        &self,
        ctx: &RenderCtx<'_>,
        continues_turn: bool,
    ) -> Vec<Line<'static>> {
        let tail = &self.buffer[self.rendered_boundary..];
        if tail.is_empty() {
            return Vec::new();
        }

        let cache_empty = self.rendered.is_empty();
        let mut out = Vec::new();
        if !cache_empty {
            out.push(assistant_blank_line(ctx));
        }

        // Full-tail parse: splitting would lose pulldown's block separators.
        let starts_new_turn = !continues_turn && cache_empty;
        out.extend(render_assistant_markdown(tail, ctx, starts_new_turn));
        out
    }

    #[cfg(test)]
    pub(crate) fn rendered_boundary(&self) -> usize {
        self.rendered_boundary
    }

    #[cfg(test)]
    pub(crate) fn cached_width(&self) -> u16 {
        self.cached_width
    }
}
