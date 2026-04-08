use ratatui::text::Text;

/// Convert a markdown string to styled ratatui [`Text`].
///
/// Uses `tui_markdown` (pulldown-cmark + syntect) for full markdown
/// rendering including syntax-highlighted code blocks.
pub(crate) fn render_markdown(input: &str) -> Text<'_> {
    tui_markdown::from_str(input)
}
