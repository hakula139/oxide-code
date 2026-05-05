//! Markdown-to-ratatui renderer with syntax highlighting.

mod highlight;
mod render;

use pulldown_cmark::{Options, Parser};
use ratatui::text::Text;

use super::theme::Theme;
use render::MarkdownRenderer;

/// Renders markdown into styled ratatui [`Text`].
///
/// `width == 0` disables wrapping entirely (useful for tests and width-agnostic callers); any
/// non-zero value wraps each block to that column budget, preserving the enclosing block's
/// continuation indent (blockquote `>`, list-item gutter, etc.) on wrapped sub-lines.
///
/// Tables and `GFM` strikethrough are enabled; other extensions stay off.
pub(crate) fn render_markdown(input: &str, theme: &Theme, width: usize) -> Text<'static> {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(input, options);
    let mut renderer = MarkdownRenderer::new(parser, theme, width);
    renderer.run();
    Text::from(renderer.lines)
}
