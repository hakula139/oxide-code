//! Streaming assistant block. Buffers tokens with a rendered-prefix cache so committed text is not
//! re-parsed each frame; promoted to [`super::AssistantText`] on `commit_streaming`.

use ratatui::text::Line;

use super::RenderCtx;
use super::assistant::render_assistant_markdown;

/// In-flight assistant turn buffered between stream tokens.
///
/// Tokens append to `buffer`; everything up to the most recent paragraph break (`\n\n`) is
/// pre-rendered into `rendered` so each frame only re-parses the unstable tail. The cache is
/// keyed by viewport width and invalidated on resize.
pub(crate) struct StreamingAssistant {
    buffer: String,
    /// Rendered lines for the stable prefix up to `rendered_boundary`.
    rendered: Vec<Line<'static>>,
    rendered_boundary: usize,
    /// Width at which `rendered` was produced; cache invalidates on resize.
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
        self.rendered.clear();
        self.rendered_boundary = 0;
        self.cached_width = 0;
        std::mem::take(&mut self.buffer)
    }

    pub(crate) fn invalidate_cache_for_width(&mut self, width: u16) {
        if self.cached_width != 0 && self.cached_width != width {
            self.rendered.clear();
            self.rendered_boundary = 0;
            self.cached_width = 0;
        }
    }

    /// Caches rendered lines up to the last `\n\n` boundary.
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
                // Paragraph break that the single-pass renderer would emit between commits.
                self.rendered.push(Line::raw(""));
            }
            self.rendered.extend(rendered);
        }

        self.rendered_boundary = boundary + rel_boundary + 2;
        self.cached_width = ctx.width;
    }

    /// Renders streaming content; `continues_turn` suppresses the leading gap.
    pub(crate) fn render_into(
        &self,
        out: &mut Vec<Line<'static>>,
        ctx: &RenderCtx<'_>,
        continues_turn: bool,
    ) {
        if !continues_turn && super::last_has_width(out) {
            out.push(Line::raw(""));
        }

        out.extend(self.rendered.iter().cloned());

        let tail = &self.buffer[self.rendered_boundary..];
        if tail.is_empty() {
            return;
        }

        let cache_empty = self.rendered.is_empty();
        if !cache_empty {
            out.push(Line::raw(""));
        }

        // Full-tail parse: splitting would lose pulldown's block separators.
        let starts_new_turn = !continues_turn && cache_empty;
        out.extend(render_assistant_markdown(tail, ctx, starts_new_turn));
    }

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
