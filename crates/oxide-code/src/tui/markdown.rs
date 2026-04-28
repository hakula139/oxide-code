mod highlight;
mod render;

use pulldown_cmark::{Options, Parser};
use ratatui::text::Text;

use render::MarkdownRenderer;

use super::theme::Theme;

/// Converts a markdown string to styled ratatui [`Text`].
///
/// When `width` is non-zero, long lines are word-wrapped to fit within
/// the given column budget. Continuation lines preserve the current
/// indent level (list markers, blockquote prefixes, etc.).
pub(crate) fn render_markdown(input: &str, theme: &Theme, width: usize) -> Text<'static> {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(input, options);
    let mut renderer = MarkdownRenderer::new(parser, theme, width);
    renderer.run();
    Text::from(renderer.lines)
}
