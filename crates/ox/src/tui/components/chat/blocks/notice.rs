//! Notice block: a dim status line, such as a scheduled API retry.

use ratatui::text::{Line, Span};

use super::{ChatBlock, RenderCtx};
use crate::tui::wrap::wrap_line;

/// A transient status update that leaves the turn running.
pub(crate) struct NoticeBlock {
    text: String,
}

impl NoticeBlock {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl ChatBlock for NoticeBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let line = Line::from(Span::styled(self.text.clone(), ctx.theme.dim()));
        wrap_line(line, usize::from(ctx.width), 0, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

    mod render {
        use super::*;

        #[test]
        fn wraps_the_text_in_the_dim_style() {
            let theme = Theme::default();
            let text = "HTTP 529 overloaded_error. Retrying in 4 s (retry 2 of 5).";
            let block = NoticeBlock::new(text);

            let lines = block.render(&RenderCtx {
                width: 30,
                theme: &theme,
                show_thinking: false,
                expanded: false,
                expand_hint: None,
            });

            assert!(
                lines.len() > 1,
                "a narrow width wraps the notice: {lines:?}"
            );
            let spans: Vec<&Span<'_>> = lines.iter().flat_map(|line| &line.spans).collect();
            assert!(
                spans
                    .iter()
                    .all(|span| span.style == theme.dim() || span.content.trim().is_empty()),
                "{spans:?}",
            );
            let words: Vec<&str> = spans
                .iter()
                .flat_map(|span| span.content.split_whitespace())
                .collect();
            assert_eq!(words, text.split_whitespace().collect::<Vec<_>>());
        }
    }
}
