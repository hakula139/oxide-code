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
pub(crate) fn render_markdown(input: &str, theme: &Theme, width: usize) -> Text<'static> {
    render(input, theme, width, false)
}

/// Renders `input` as the blocks after an already rendered block, so its first block opens with
/// the blank separator a single render would give it.
pub(crate) fn render_markdown_after_block(
    input: &str,
    theme: &Theme,
    width: usize,
) -> Text<'static> {
    render(input, theme, width, true)
}

fn render(input: &str, theme: &Theme, width: usize, after_block: bool) -> Text<'static> {
    let parser = Parser::new_ext(input, OPTIONS);
    let mut renderer = MarkdownRenderer::new(parser, theme, width, after_block);
    renderer.run();
    Text::from(renderer.lines)
}

/// Byte length of the leading run of complete top-level blocks in `input`.
///
/// A block is complete once another top-level block starts after it, so the run ends at the line
/// where the last block starts. Rendering the run with [`render_markdown`] and the remainder with
/// [`render_markdown_after_block`] matches a single render of `input`, except that a
/// reference-style link renders literally when its definition is on the other side of the split.
pub(crate) fn complete_blocks_len(input: &str) -> usize {
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

    // ── render_markdown_after_block ──

    #[test]
    fn render_markdown_after_block_separates_only_a_rendered_block() {
        let theme = Theme::default();
        let rows = |input| -> Vec<String> {
            let text = render_markdown_after_block(input, &theme, 0);
            text.lines.iter().map(ToString::to_string).collect()
        };
        assert_eq!(rows("para\n"), ["", "para"]);
        assert_eq!(rows("[a]: https://example.com\n"), Vec::<String>::new());
    }

    // ── complete_blocks_len ──

    #[test]
    fn complete_blocks_len_ends_at_last_block_start() {
        let input = "para one\n\npara two\n\n- tail";
        assert_eq!(complete_blocks_len(input), "para one\n\npara two\n\n".len());
    }

    #[test]
    fn complete_blocks_len_single_block_is_incomplete() {
        assert_eq!(complete_blocks_len("only paragraph\n\n"), 0);
        assert_eq!(complete_blocks_len("\n\n"), 0);
    }

    #[test]
    fn complete_blocks_len_keeps_blank_lines_inside_open_fence() {
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
    fn complete_blocks_len_keeps_loose_list_items_together() {
        assert_eq!(complete_blocks_len("- a\n\n- b\n\n  more"), 0);
    }

    #[test]
    fn complete_blocks_len_snaps_indented_block_to_line_start() {
        let input = "para\n\n    indented code";
        assert_eq!(complete_blocks_len(input), "para\n\n".len());
    }

    #[test]
    fn complete_blocks_len_counts_thematic_break_as_block() {
        assert_eq!(complete_blocks_len("---\ntext"), "---\n".len());
    }
}
