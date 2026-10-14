//! Syntax highlighting via syntect.

use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::FontStyle;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::tui::theme::Theme;

// Loaded once on first highlight (a few hundred ms of grammar parsing) and reused for the rest of
// the process. Keeping it lazy avoids paying that cost when no fenced block is rendered.
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Highlights `code` with the theme's syntax colors, returning one [`Line`] per source line.
///
/// `lang` is a fenced-block info string whose first token, ending at whitespace or a comma, names
/// the language (`rust,no_run` and `rust ignore` both resolve to `rust`). Unknown or empty
/// languages fall back to the theme's `code` style with no per-token coloring so plain fenced
/// blocks render uniformly without ragged background fills.
pub(super) fn highlight_code(lang: &str, code: &str, theme: &Theme) -> Vec<Line<'static>> {
    let syntax = lang
        .split(|c: char| c.is_whitespace() || c == ',')
        .next()
        .filter(|s| !s.is_empty())
        .and_then(|token| SYNTAX_SET.find_syntax_by_token(token));

    let Some(syntax) = syntax else {
        let fallback = theme.code();
        return code
            .lines()
            .map(|l| Line::styled(l.to_owned(), fallback))
            .collect();
    };

    // The newline grammars close line-scoped tokens such as `//` comments at the `\n`, so each line
    // goes in with its ending and the ending is dropped from the output.
    let mut highlighter = HighlightLines::new(syntax, theme.syntax);
    LinesWithEndings::from(code)
        .map(|line| {
            let spans: Vec<Span<'static>> = highlighter
                .highlight_line(line, &SYNTAX_SET)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(style, text)| {
                    let text = text.trim_end_matches(['\n', '\r']);
                    if text.is_empty() {
                        return None;
                    }
                    let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                    let mut rs = Style::new().fg(fg);
                    if style.font_style.contains(FontStyle::BOLD) {
                        rs = rs.add_modifier(Modifier::BOLD);
                    }
                    Some(Span::styled(text.to_owned(), rs))
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::tui::theme::load_builtin;

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

    fn styled_spans(line: &Line<'_>) -> Vec<(String, Option<Color>)> {
        line.spans
            .iter()
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect()
    }

    // ── highlight_code ──

    #[test]
    fn highlight_code_known_language_produces_rgb() {
        let lines = highlight_code("rust", "fn main() {}", &Theme::default());
        assert!(has_rgb_color(&lines));
        assert!(all_text(&lines).contains("fn"));
    }

    #[test]
    fn highlight_code_info_string_uses_first_token() {
        for info in ["rust no_run", "rust,no_run"] {
            let lines = highlight_code(info, "let x = 1;", &Theme::default());
            assert!(has_rgb_color(&lines), "{info:?} should resolve to rust");
        }
    }

    #[test]
    fn highlight_code_multiline_preserves_lines() {
        let lines = highlight_code(
            "rust",
            indoc! {"
                fn a() {}
                fn b() {}
            "},
            &Theme::default(),
        );
        assert_eq!(all_text(&lines), "fn a() {}\nfn b() {}");
    }

    #[test]
    fn highlight_code_line_comment_ends_at_line_break() {
        let theme = Theme::default();
        let after_comment = highlight_code("rust", "// c\nlet x = 1;", &theme);
        let alone = highlight_code("rust", "let x = 1;", &theme);
        assert_eq!(styled_spans(&after_comment[1]), styled_spans(&alone[0]));
    }

    #[test]
    fn highlight_code_follows_theme_syntax_palette() {
        let dark = highlight_code("rust", "fn main() {}", &Theme::default());
        let light = highlight_code("rust", "fn main() {}", &load_builtin("latte").unwrap());
        assert_eq!(all_text(&dark), all_text(&light));
        assert_ne!(styled_spans(&dark[0]), styled_spans(&light[0]));
    }

    #[test]
    fn highlight_code_unknown_language_uses_fallback() {
        let theme = Theme::default();
        let lines = highlight_code("nonexistent_lang_xyz", "hello", &theme);
        assert_eq!(lines, [Line::styled("hello", theme.code())]);
    }

    #[test]
    fn highlight_code_empty_language_uses_fallback() {
        let theme = Theme::default();
        let lines = highlight_code("", "code here", &theme);
        assert_eq!(lines, [Line::styled("code here", theme.code())]);
    }

    #[test]
    fn highlight_code_empty_input_yields_empty() {
        let lines = highlight_code("rust", "", &Theme::default());
        assert_eq!(lines, Vec::<Line<'_>>::new());
    }
}
