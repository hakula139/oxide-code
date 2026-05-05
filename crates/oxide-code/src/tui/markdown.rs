//! Markdown-to-ratatui renderer with syntax highlighting.

mod highlight;
mod render;

use pulldown_cmark::{Options, Parser};
use ratatui::text::Text;

use super::theme::Theme;
use render::MarkdownRenderer;

/// Renders markdown into styled ratatui [`Text`]; non-zero `width` wraps with indent-preserving
/// continuation lines.
pub(crate) fn render_markdown(input: &str, theme: &Theme, width: usize) -> Text<'static> {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(input, options);
    let mut renderer = MarkdownRenderer::new(parser, theme, width);
    renderer.run();
    Text::from(renderer.lines)
}
