//! Streaming assistant block. Buffers tokens with a rendered-prefix cache so completed markdown
//! blocks are not re-parsed each frame. Promoted to [`super::AssistantText`] on `commit_streaming`.

use ratatui::text::Line;

use super::RenderCtx;
use super::assistant::{Opening, render_assistant_markdown};
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
        // A partial last line can still change which block it starts, so only whole lines count.
        let Some(last_newline) = tail.rfind('\n') else {
            return;
        };
        let complete_len = complete_blocks_len(&tail[..=last_newline]);
        if complete_len == 0 {
            return;
        }

        let rendered =
            render_assistant_markdown(&tail[..complete_len], ctx, self.opening(continues_turn));
        self.rendered.extend(rendered);

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
        render_assistant_markdown(tail, ctx, self.opening(continues_turn))
    }

    fn opening(&self, continues_turn: bool) -> Opening {
        if !self.rendered.is_empty() {
            Opening::AfterBlocks
        } else if continues_turn {
            Opening::SameTurn
        } else {
            Opening::NewTurn
        }
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
