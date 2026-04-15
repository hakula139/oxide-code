mod highlight;
mod render;

use pulldown_cmark::{Options, Parser};
use ratatui::text::Text;

use render::MarkdownRenderer;

use super::theme::Theme;

/// Convert a markdown string to styled ratatui [`Text`].
pub(crate) fn render_markdown(input: &str, theme: &Theme) -> Text<'static> {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(input, options);
    let mut renderer = MarkdownRenderer::new(parser, *theme);
    renderer.run();
    Text::from(renderer.lines)
}
