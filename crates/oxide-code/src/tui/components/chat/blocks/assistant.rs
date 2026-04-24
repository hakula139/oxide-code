//! Assistant text and thinking blocks.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{ChatBlock, RenderCtx, prepend_markdown_prefix};
use crate::tui::markdown::render_markdown;
use crate::tui::theme::Theme;
use crate::tui::wrap::wrap_line;

/// First-line prefix for assistant text — diamond + space. Continuation
/// (and all lines when the streaming block is continuing a turn) uses a
/// 2-column space indent.
pub(super) const ASSISTANT_PREFIX: &str = "◉ ";

/// Continuation prefix for assistant markdown — two spaces matching the
/// visual width of [`ASSISTANT_PREFIX`].
pub(super) const ASSISTANT_CONT: &str = "  ";

/// Per-line prefix for thinking blocks — shares [`BAR`] so bars align.
const THINKING_PREFIX: &str = "▎ ";

/// Header label on the first line of a thinking block.
const THINKING_LABEL: &str = "Thinking...";

// ── AssistantText ──

/// A committed assistant text response, rendered through the markdown
/// pipeline.
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
        render_assistant_markdown(&self.text, ctx, true)
    }

    fn continues_assistant_turn(&self) -> bool {
        true
    }
}

/// Render assistant prose as markdown with a first-line icon and a
/// matching-width space indent on every subsequent line.
///
/// `starts_new_turn = true` emits [`ASSISTANT_PREFIX`] on the first line
/// (a fresh turn). `false` uses [`ASSISTANT_CONT`] on every line so the
/// block flows into an existing assistant turn (used by the streaming
/// cache after its first line has already been emitted).
///
/// The markdown renderer wraps to `width - 2` so the 2-column lead-in
/// never pushes content past the terminal edge.
pub(super) fn render_assistant_markdown(
    text: &str,
    ctx: &RenderCtx<'_>,
    starts_new_turn: bool,
) -> Vec<Line<'static>> {
    let icon_style = ctx.theme.secondary();
    let md_width = usize::from(ctx.width).saturating_sub(ASSISTANT_PREFIX.width());
    let rendered = render_markdown(text, ctx.theme, md_width);
    rendered
        .lines
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

/// Extended-thinking block — bar-prefixed quote with a `Thinking...`
/// header and markdown-rendered body. Hidden when `show_thinking` is off.
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
        let theme = ctx.theme;
        let width = usize::from(ctx.width);
        let style = theme.thinking();

        let bar_width = THINKING_PREFIX.width();
        let inner_width = width.saturating_sub(bar_width);
        let bar_spans = vec![Span::styled(THINKING_PREFIX, style)];

        let mut out = Vec::new();

        let header = Line::from(vec![
            Span::styled(THINKING_PREFIX, style),
            Span::styled(THINKING_LABEL, style),
        ]);
        out.extend(wrap_line(header, width, bar_width, Some(&bar_spans)));

        if !self.text.trim().is_empty() {
            let rendered = render_markdown(&self.text, theme, inner_width);
            for line in rendered.lines {
                let dimmed = apply_thinking_style(line, theme);
                let mut spans = bar_spans.clone();
                spans.extend(dimmed.spans);
                let mut out_line = Line::from(spans);
                out_line.style = dimmed.style;
                out.push(out_line);
            }
        }

        out
    }

    fn visible(&self, ctx: &RenderCtx<'_>) -> bool {
        ctx.show_thinking
    }
}

/// Dims plain spans; leaves explicitly-colored spans (inline code,
/// links, highlighted fences) at full color.
fn apply_thinking_style(mut line: Line<'static>, theme: &Theme) -> Line<'static> {
    if line.style.fg.is_some() {
        return line;
    }
    let base = theme.thinking();
    for span in &mut line.spans {
        if span.style.fg.is_none() {
            span.style = span.style.patch(base);
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ratatui::style::Style;

    use super::super::BAR;
    use super::*;
    use crate::tui::theme::Theme;

    fn ctx_at(width: u16, theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            width,
            theme,
            show_thinking: true,
        }
    }

    #[test]
    fn thinking_prefix_shares_bar_glyph_with_tool_blocks() {
        assert!(
            THINKING_PREFIX.starts_with(BAR),
            "THINKING_PREFIX ({THINKING_PREFIX:?}) must start with BAR ({BAR:?})",
        );
    }

    // ── AssistantThinking::render ──

    #[test]
    fn render_empty_body_emits_header_only() {
        // Exercised by the zero-delta frame before the first thinking chunk.
        let theme = Theme::default();
        let block = AssistantThinking::new("   \n  ");
        let lines = block.render(&ctx_at(60, &theme));
        assert_eq!(lines.len(), 1, "only the header should render: {lines:?}");
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header.starts_with(THINKING_PREFIX));
        assert!(header.contains("Thinking..."));
    }

    #[test]
    fn render_fenced_code_block_preserves_highlight_style() {
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
        assert_eq!(fence_line.style.fg, Some(theme.code));

        let first_span = fence_line.spans.first().expect("empty fence line");
        assert_eq!(first_span.content, THINKING_PREFIX);
        assert_eq!(first_span.style, theme.thinking());
    }

    // ── apply_thinking_style ──

    #[test]
    fn apply_thinking_style_dims_plain_spans_only() {
        let theme = Theme::default();
        let line = Line::from(vec![
            Span::raw("plain "),
            Span::styled("code", Style::default().fg(theme.code)),
        ]);
        let out = apply_thinking_style(line, &theme);
        assert_eq!(out.spans[0].style.fg, theme.thinking().fg);
        assert_eq!(out.spans[1].style.fg, Some(theme.code));
    }
}
