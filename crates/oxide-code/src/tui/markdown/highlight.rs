use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

/// Fallback foreground for code when no syntax is recognized.
pub(super) const CODE_FG: Color = Color::Rgb(148, 226, 213); // Catppuccin Teal

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Highlight `code` using syntect for the given language.
///
/// The language token is extracted from the first word of `lang` (so info
/// strings like `rust,no_run` still work). Falls back to plain teal-colored
/// lines when the language is unrecognized.
pub(super) fn highlight_code(lang: &str, code: &str) -> Vec<Line<'static>> {
    let syntax = lang
        .split_ascii_whitespace()
        .next()
        .filter(|s| !s.is_empty())
        .and_then(|token| SYNTAX_SET.find_syntax_by_token(token));

    let Some(syntax) = syntax else {
        return code
            .lines()
            .map(|l| Line::styled(l.to_owned(), Style::new().fg(CODE_FG)))
            .collect();
    };

    let theme = &THEME_SET.themes["base16-ocean.dark"];
    let mut highlighter = HighlightLines::new(syntax, theme);
    code.lines()
        .map(|line| {
            let spans: Vec<Span<'static>> = highlighter
                .highlight_line(line, &SYNTAX_SET)
                .unwrap_or_default()
                .into_iter()
                .map(|(style, text)| {
                    let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                    let mut rs = Style::new().fg(fg);
                    if style
                        .font_style
                        .contains(syntect::highlighting::FontStyle::BOLD)
                    {
                        rs = rs.add_modifier(Modifier::BOLD);
                    }
                    Span::styled(text.to_owned(), rs)
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}
