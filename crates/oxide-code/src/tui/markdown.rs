mod highlight;
mod render;

use pulldown_cmark::{Options, Parser};
use ratatui::text::Text;

use render::MarkdownRenderer;

/// Convert a markdown string to styled ratatui [`Text`].
pub(crate) fn render_markdown(input: &str) -> Text<'static> {
    let parser = Parser::new_ext(input, Options::ENABLE_STRIKETHROUGH);
    let mut renderer = MarkdownRenderer::new(parser);
    renderer.run();
    Text::from(renderer.lines)
}
