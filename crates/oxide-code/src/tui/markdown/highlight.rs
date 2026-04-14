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

#[cfg(test)]
mod tests {
    use super::*;

    fn all_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn has_rgb_color(lines: &[Line<'_>]) -> bool {
        lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Rgb(..))))
        })
    }

    // ── highlight_code ──

    #[test]
    fn highlight_code_known_language_produces_rgb() {
        let lines = highlight_code("rust", "fn main() {}");
        assert!(has_rgb_color(&lines));
        assert!(all_text(&lines).contains("fn"));
    }

    #[test]
    fn highlight_code_info_string_with_extra_tokens() {
        let lines = highlight_code("rust no_run", "let x = 1;");
        assert!(has_rgb_color(&lines));
    }

    #[test]
    fn highlight_code_unknown_language_uses_teal_fallback() {
        let lines = highlight_code("nonexistent_lang_xyz", "hello");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].style.fg, Some(CODE_FG));
        assert_eq!(all_text(&lines), "hello");
    }

    #[test]
    fn highlight_code_empty_language_uses_fallback() {
        let lines = highlight_code("", "code here");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].style.fg, Some(CODE_FG));
    }

    #[test]
    fn highlight_code_whitespace_only_language_uses_fallback() {
        let lines = highlight_code("   ", "code");
        assert_eq!(lines[0].style.fg, Some(CODE_FG));
    }

    #[test]
    fn highlight_code_multiline_preserves_lines() {
        let lines = highlight_code("rust", "fn a() {}\nfn b() {}");
        assert_eq!(lines.len(), 2);
        assert!(all_text(&lines).contains("fn a()"));
        assert!(all_text(&lines).contains("fn b()"));
    }

    #[test]
    fn highlight_code_empty_code_returns_empty() {
        let lines = highlight_code("rust", "");
        assert!(lines.is_empty() || all_text(&lines).trim().is_empty());
    }
}
