//! Markdown-to-ratatui renderer with syntax highlighting.

mod highlight;
mod render;

use pulldown_cmark::{Event, Options, Parser};
use ratatui::text::Text;

use super::theme::Theme;
use render::MarkdownRenderer;

const OPTIONS: Options = Options::ENABLE_STRIKETHROUGH.union(Options::ENABLE_TABLES);

/// Renders markdown into styled ratatui [`Text`].
///
/// `width == 0` disables wrapping entirely (useful for tests and width-agnostic callers). Any
/// non-zero value wraps each block to that column budget, preserving the enclosing block's
/// continuation indent (blockquote `>`, list-item gutter, etc.) on wrapped sub-lines.
pub(super) fn render_markdown(input: &str, theme: &Theme, width: usize) -> Text<'static> {
    render_markdown_chunk(input, theme, width, false).0
}

/// Renders `input` as the next chunk of a document, where `after_block` says whether the chunks
/// before it ended after a block. Returns the rows and that flag for the chunk after this one.
pub(super) fn render_markdown_chunk(
    input: &str,
    theme: &Theme,
    width: usize,
    after_block: bool,
) -> (Text<'static>, bool) {
    let parser = Parser::new_ext(input, OPTIONS);
    let mut renderer = MarkdownRenderer::new(parser, theme, width, after_block);
    renderer.run();
    (Text::from(renderer.lines), renderer.needs_newline)
}

/// Byte length of the leading run of complete top-level blocks in `input`.
///
/// A block is complete once another top-level block starts after it, so the run ends at the line
/// where the last block starts. Rendering the run and then the remainder with
/// [`render_markdown_chunk`], passing its flag along, matches a single render of `input`, except
/// that a reference-style link renders literally when its definition is on the other side.
pub(super) fn complete_blocks_len(input: &str) -> usize {
    let mut depth = 0_usize;
    let mut last_start = 0;
    for (event, range) in Parser::new_ext(input, OPTIONS).into_offset_iter() {
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    last_start = range.start;
                }
                depth += 1;
            }
            Event::End(_) => depth -= 1,
            _ if depth == 0 => last_start = range.start,
            _ => {}
        }
    }
    input[..last_start].rfind('\n').map_or(0, |i| i + 1)
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    mod render_markdown_chunk {
        use super::*;

        #[test]
        fn carries_the_block_flag_across_chunks() {
            let theme = Theme::default();
            let chunk = |input, after_block| {
                let (text, after) = render_markdown_chunk(input, &theme, 0, after_block);
                let rows: Vec<String> = text.lines.iter().map(ToString::to_string).collect();
                (rows, after)
            };
            assert_eq!(
                chunk("para\n", true),
                (vec![String::new(), "para".to_owned()], true)
            );
            assert_eq!(chunk("```\n```\n", false), (vec![], true));
            for after_block in [false, true] {
                let definition = chunk("[a]: https://example.com\n", after_block);
                assert_eq!(definition, (vec![], after_block));
            }
        }

        #[test]
        fn matches_a_single_render_when_rows_overflow_the_width() {
            let theme = Theme::default();
            let input = "> quote quote\n\npara text";
            let split = complete_blocks_len(input);

            let (head, after_block) = render_markdown_chunk(&input[..split], &theme, 2, false);
            let (tail, _) = render_markdown_chunk(&input[split..], &theme, 2, after_block);
            let chunked: Vec<_> = head.lines.into_iter().chain(tail.lines).collect();

            assert_eq!(split, "> quote quote\n\n".len());
            assert_eq!(chunked, render_markdown(input, &theme, 2).lines);
        }
    }

    mod complete_blocks_len {
        use super::*;

        #[test]
        fn ends_at_last_block_start() {
            let input = indoc! {"
                para one

                para two

                - tail"
            };
            assert_eq!(complete_blocks_len(input), "para one\n\npara two\n\n".len());
        }

        #[test]
        fn single_block_is_incomplete() {
            assert_eq!(complete_blocks_len("only paragraph\n\n"), 0);
            assert_eq!(complete_blocks_len("\n\n"), 0);
        }

        #[test]
        fn keeps_blank_lines_inside_open_fence() {
            let input = indoc! {"
                Here:

                ```c
                #include <a.h>

                #define X 1
                - not a bullet
            "};
            assert_eq!(complete_blocks_len(input), "Here:\n\n".len());
        }

        #[test]
        fn keeps_loose_list_items_together() {
            assert_eq!(
                complete_blocks_len(indoc! {"
                    - a

                    - b

                      more"
                }),
                0
            );
        }

        #[test]
        fn snaps_indented_block_to_line_start() {
            let input = indoc! {"
                para

                    indented code"
            };
            assert_eq!(complete_blocks_len(input), "para\n\n".len());
        }

        #[test]
        fn counts_thematic_break_as_block() {
            assert_eq!(complete_blocks_len("---\ntext"), "---\n".len());
        }
    }
}
