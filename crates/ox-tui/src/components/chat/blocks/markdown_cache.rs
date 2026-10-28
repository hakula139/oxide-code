//! Rendered-prefix cache for markdown that grows while a turn streams.

use ratatui::text::Line;

use crate::markdown::complete_blocks_len;

/// Caches the rows of a markdown buffer's complete top-level blocks, so a section that grows
/// re-parses only the tail after them. `render` paints one chunk with the style, gutter, or prefix
/// of the section that owns the cache.
#[derive(Default)]
pub(crate) struct MarkdownCache {
    rendered: Vec<Line<'static>>,
    boundary: usize,
    after_block: bool,
    /// The width `rendered` was produced at, or 0 before the first frame.
    width: u16,
}

impl MarkdownCache {
    pub(crate) fn rendered(&self) -> &[Line<'static>] {
        &self.rendered
    }

    pub(crate) fn invalidate(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn invalidate_for_width(&mut self, width: u16) {
        if self.width != 0 && self.width != width {
            self.invalidate();
        }
    }

    /// Caches the blocks `buffer` closed since the last call, painting each with `render`.
    pub(crate) fn advance(
        &mut self,
        buffer: &str,
        width: u16,
        render: impl Fn(&str, bool) -> (Vec<Line<'static>>, bool),
    ) {
        if width == 0 {
            return;
        }

        let tail = &buffer[self.boundary..];
        // A partial last line can still change which block it starts, so only whole lines count.
        let Some(last_newline) = tail.rfind('\n') else {
            return;
        };
        let complete = complete_blocks_len(&tail[..=last_newline]);
        if complete == 0 {
            return;
        }

        let (rows, after_block) = render(&tail[..complete], self.after_block);
        self.rendered.extend(rows);
        self.after_block = after_block;
        self.boundary += complete;
        self.width = width;
    }

    /// Renders the uncached tail after the cached blocks with `render`.
    pub(crate) fn tail(
        &self,
        buffer: &str,
        render: impl FnOnce(&str, bool) -> (Vec<Line<'static>>, bool),
    ) -> Vec<Line<'static>> {
        render(&buffer[self.boundary..], self.after_block).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row per chunk, so a test can count what rendered.
    fn row(chunk: &str, _after_block: bool) -> (Vec<Line<'static>>, bool) {
        (vec![Line::raw(chunk.to_owned())], true)
    }

    fn rendered(cache: &MarkdownCache) -> Vec<String> {
        cache.rendered().iter().map(ToString::to_string).collect()
    }

    mod advance {
        use super::*;

        #[test]
        fn caches_only_the_blocks_a_finished_line_closes() {
            for (buffer, cached) in [
                ("no newline here", ""),
                ("first line\nsecond line", ""),
                ("para1\n\npartial", ""),
                ("para1\n\npartial\n", "para1\n\n"),
                ("p1\n\np2\n\np3\n\np4\n", "p1\n\np2\n\np3\n\n"),
                // A loose list continues past a blank line until another block starts.
                ("- item 1\n\n", ""),
                (
                    "- item 1\n\n- item 2\n\nafter\n",
                    "- item 1\n\n- item 2\n\n",
                ),
            ] {
                let mut cache = MarkdownCache::default();

                cache.advance(buffer, 80, row);

                assert_eq!(cache.boundary, cached.len(), "{buffer:?}");
            }
        }

        #[test]
        fn leaves_the_whole_buffer_to_the_tail_without_a_width() {
            let mut cache = MarkdownCache::default();

            cache.advance("para1\n\n", 0, row);

            assert_eq!(cache.boundary, 0);
            assert_eq!(rendered(&cache), [] as [String; 0]);
        }

        #[test]
        fn keeps_earlier_rows_across_appends() {
            let mut cache = MarkdownCache::default();
            cache.advance("para1\n\n", 80, row);

            cache.advance("para1\n\npara2\n\npara3", 80, row);

            assert_eq!(rendered(&cache), ["para1"]);
            assert_eq!(cache.boundary, "para1\n\n".len());
        }
    }

    mod invalidate_for_width {
        use super::*;

        #[test]
        fn drops_the_cache_only_when_a_measured_width_changes() {
            for (width, kept) in [(80, true), (40, false)] {
                let mut cache = MarkdownCache::default();
                cache.advance("para1\n\npara2\n\n", 80, row);

                cache.invalidate_for_width(width);

                let expected: &[&str] = if kept { &["para1"] } else { &[] };
                assert_eq!(rendered(&cache), expected, "width {width}");
            }
        }
    }

    mod tail {
        use super::*;

        #[test]
        fn renders_from_the_last_complete_block() {
            let mut cache = MarkdownCache::default();
            cache.advance("para1\n\npara2\n\npartial", 80, row);

            let tail = cache.tail("para1\n\npara2\n\npartial", row);

            assert_eq!(tail, [Line::raw("para2\n\npartial")]);
        }
    }
}
