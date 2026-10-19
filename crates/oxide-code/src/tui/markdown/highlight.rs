//! Syntax highlighting via syntect.

use std::cell::RefCell;
use std::ptr;
use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, HighlightState, Theme as SyntaxTheme};
use syntect::parsing::{ParseState, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::tui::theme::Theme;

// Loaded once on first highlight (a few hundred ms of grammar parsing) and reused for the rest of
// the process. Keeping it lazy avoids paying that cost when no fenced block is rendered.
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Highlighter state after the complete lines of the last highlighted block, so a fenced block
/// that is still streaming in only highlights the lines added since the previous frame.
struct Resume {
    syntax: &'static SyntaxReference,
    theme: &'static SyntaxTheme,
    code: String,
    rows: Vec<Line<'static>>,
    highlight_state: HighlightState,
    parse_state: ParseState,
}

thread_local! {
    static RESUME: RefCell<Option<Resume>> = const { RefCell::new(None) };
}

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

    RESUME.with_borrow_mut(|resume| highlight_resuming(resume, syntax, theme.syntax, code))
}

/// Highlights `code`, reusing `resume` when it holds a line-aligned prefix of `code` highlighted
/// with the same syntax and theme, and leaves the state after `code`'s complete lines in it.
fn highlight_resuming(
    resume: &mut Option<Resume>,
    syntax: &'static SyntaxReference,
    theme: &'static SyntaxTheme,
    code: &str,
) -> Vec<Line<'static>> {
    let reusable = resume.take().filter(|r| {
        ptr::eq(r.syntax, syntax) && ptr::eq(r.theme, theme) && code.starts_with(&r.code)
    });
    let (done, mut rows, mut highlighter) = match reusable {
        Some(r) => (
            r.code.len(),
            r.rows,
            HighlightLines::from_state(theme, r.highlight_state, r.parse_state),
        ),
        None => (0, Vec::new(), HighlightLines::new(syntax, theme)),
    };

    let rest = &code[done..];
    let complete = rest.rfind('\n').map_or(0, |i| i + 1);
    rows.extend(
        LinesWithEndings::from(&rest[..complete])
            .map(|line| highlight_line(&mut highlighter, line)),
    );
    let (highlight_state, parse_state) = highlighter.state();

    let mut out = rows.clone();
    if complete < rest.len() {
        let mut partial =
            HighlightLines::from_state(theme, highlight_state.clone(), parse_state.clone());
        out.push(highlight_line(&mut partial, &rest[complete..]));
    }
    *resume = Some(Resume {
        syntax,
        theme,
        code: code[..done + complete].to_owned(),
        rows,
        highlight_state,
        parse_state,
    });
    out
}

// The newline grammars close line-scoped tokens such as `//` comments at the `\n`, so each line
// goes in with its ending and the ending is dropped from the output.
fn highlight_line(highlighter: &mut HighlightLines<'_>, line: &str) -> Line<'static> {
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
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::tui::theme::load_builtin;

    /// Joins each row's raw span text. `Line::to_string` would drop a line ending left in a span.
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

    mod highlight_code {
        use super::*;

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

        #[test]
        fn known_language_produces_rgb() {
            let lines = highlight_code("rust", "fn main() {}", &Theme::default());

            assert!(has_rgb_color(&lines));
            assert!(all_text(&lines).contains("fn"));
        }

        #[test]
        fn info_string_uses_first_token() {
            for info in ["rust no_run", "rust,no_run"] {
                let lines = highlight_code(info, "let x = 1;", &Theme::default());
                assert!(has_rgb_color(&lines), "{info:?} should resolve to rust");
            }
        }

        #[test]
        fn multiline_preserves_lines() {
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
        fn crlf_input_drops_both_line_end_bytes() {
            let lines = highlight_code("rust", "fn a() {}\r\nfn b() {}\r\n", &Theme::default());

            assert_eq!(all_text(&lines), "fn a() {}\nfn b() {}");
        }

        #[test]
        fn line_comment_ends_at_line_break() {
            let theme = Theme::default();

            let after_comment = highlight_code("rust", "// c\nlet x = 1;", &theme);
            let alone = highlight_code("rust", "let x = 1;", &theme);

            assert_eq!(styled_spans(&after_comment[1]), styled_spans(&alone[0]));
        }

        #[test]
        fn follows_theme_syntax_palette() {
            let dark = highlight_code("rust", "fn main() {}", &Theme::default());
            let light = highlight_code("rust", "fn main() {}", &load_builtin("latte").unwrap());

            assert_eq!(all_text(&dark), all_text(&light));
            assert_ne!(styled_spans(&dark[0]), styled_spans(&light[0]));
        }

        #[test]
        fn unknown_language_uses_fallback() {
            let theme = Theme::default();

            let lines = highlight_code("nonexistent_lang_xyz", "hello", &theme);

            assert_eq!(lines, [Line::styled("hello", theme.code())]);
        }

        #[test]
        fn empty_language_uses_fallback() {
            let theme = Theme::default();

            let lines = highlight_code("", "code here", &theme);

            assert_eq!(lines, [Line::styled("code here", theme.code())]);
        }

        #[test]
        fn empty_input_yields_empty() {
            let lines = highlight_code("rust", "", &Theme::default());

            assert_eq!(lines, Vec::<Line<'_>>::new());
        }
    }

    mod highlight_resuming {
        use super::*;

        fn syntax(token: &str) -> &'static SyntaxReference {
            SYNTAX_SET.find_syntax_by_token(token).unwrap()
        }

        /// Caches `prefix` and swaps its first row for a sentinel, so a reused row is recognizable.
        fn sentinel_resume(prefix: &str) -> Option<Resume> {
            let mut resume = None;
            _ = highlight_resuming(&mut resume, syntax("rust"), Theme::default().syntax, prefix);
            resume.as_mut().unwrap().rows[0] = Line::from("sentinel");
            resume
        }

        #[test]
        fn reuses_rows_of_the_cached_complete_lines() {
            let mut resume = sentinel_resume("fn a() {}\nfn b");
            assert_eq!(resume.as_ref().unwrap().code, "fn a() {}\n");

            let code = "fn a() {}\nfn b() {}\n";
            let lines =
                highlight_resuming(&mut resume, syntax("rust"), Theme::default().syntax, code);

            assert_eq!(lines[0], Line::from("sentinel"));
            assert_eq!(all_text(&lines[1..]), "fn b() {}");
            assert_eq!(resume.unwrap().code, code);
        }

        #[test]
        fn matches_a_fresh_highlight_at_every_char() {
            let code = indoc! {"
                /* open
                still comment */ let x = 1;
                let s = \"a
                b\";
            "};
            let (syntax, theme) = (syntax("rust"), Theme::default().syntax);
            let mut resume = None;

            for end in (1..=code.len()).filter(|&i| code.is_char_boundary(i)) {
                let prefix = &code[..end];
                assert_eq!(
                    highlight_resuming(&mut resume, syntax, theme, prefix),
                    highlight_resuming(&mut None, syntax, theme, prefix),
                    "{prefix:?}",
                );
            }
        }

        #[test]
        fn starts_over_for_another_syntax_theme_or_prefix() {
            let latte = load_builtin("latte").unwrap();
            let rust_dark = (syntax("rust"), Theme::default().syntax);

            for (syntax, theme, code) in [
                (syntax("python"), rust_dark.1, "fn a() {}\n"),
                (rust_dark.0, latte.syntax, "fn a() {}\n"),
                (rust_dark.0, rust_dark.1, "fn z() {}\n"),
            ] {
                let mut resume = sentinel_resume("fn a() {}\n");
                let lines = highlight_resuming(&mut resume, syntax, theme, code);
                assert_ne!(lines[0], Line::from("sentinel"), "{code:?}");
            }
        }
    }
}
