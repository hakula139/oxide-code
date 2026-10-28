//! Markdown that grows while a turn streams, with a rendered-prefix cache.

use ratatui::text::Line;

use crate::markdown::complete_blocks_len;

/// Markdown text still streaming in, with the rows of its complete top-level blocks cached, so a
/// section that grows re-parses only the tail after them. Each `render` closure paints one chunk
/// with the style, gutter, or prefix of the section that owns the text.
#[derive(Default)]
pub(crate) struct LiveMarkdown {
    text: String,
    rendered: Vec<Line<'static>>,
    /// The length of the prefix of `text` that `rendered` covers.
    boundary: usize,
    after_block: bool,
    /// The width `rendered` was produced at, or 0 before the first frame.
    width: u16,
}

impl LiveMarkdown {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn push_str(&mut self, token: &str) {
        self.text.push_str(token);
    }

    /// Takes the text, leaving the stream empty.
    pub(crate) fn take(&mut self) -> String {
        std::mem::take(self).text
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Drops the cached rows, so the next render re-parses the whole text with current styles.
    pub(crate) fn invalidate(&mut self) {
        *self = Self {
            text: std::mem::take(&mut self.text),
            ..Self::default()
        };
    }

    pub(crate) fn rendered(&self) -> &[Line<'static>] {
        &self.rendered
    }

    /// Returns whether the next render at `width` paints from the start of the text, keeping no
    /// cached rows.
    pub(crate) fn starts_over_at(&self, width: u16) -> bool {
        self.rendered.is_empty() || self.width != width
    }

    /// Caches the blocks the text closed since the last call, painting each with `render`. Rows
    /// cached at another width are dropped first, since they no longer wrap the same.
    pub(crate) fn advance(
        &mut self,
        width: u16,
        render: impl Fn(&str, bool) -> (Vec<Line<'static>>, bool),
    ) {
        if self.width != width {
            self.invalidate();
        }
        if width == 0 {
            return;
        }

        let tail = &self.text[self.boundary..];
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
        render: impl FnOnce(&str, bool) -> (Vec<Line<'static>>, bool),
    ) -> Vec<Line<'static>> {
        render(&self.text[self.boundary..], self.after_block).0
    }

    /// Advances the cache, then returns every row: the cached blocks followed by the tail.
    pub(crate) fn render(
        &mut self,
        width: u16,
        render: impl Fn(&str, bool) -> (Vec<Line<'static>>, bool),
    ) -> Vec<Line<'static>> {
        self.advance(width, &render);
        let mut rows = self.rendered.clone();
        rows.extend(self.tail(render));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row per chunk, with each newline shown as `/`, so a test can see what each chunk held.
    fn row(chunk: &str, _after_block: bool) -> (Vec<Line<'static>>, bool) {
        (vec![Line::raw(chunk.replace('\n', "/"))], true)
    }

    fn live(text: &str) -> LiveMarkdown {
        let mut live = LiveMarkdown::default();
        live.push_str(text);
        live
    }

    fn rows(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    mod take {
        use super::*;

        #[test]
        fn leaves_an_empty_stream_that_caches_from_the_start() {
            let mut live = live("p1\n\np2\n");
            live.advance(80, row);

            assert_eq!(live.take(), "p1\n\np2\n");

            assert!(live.is_empty());
            assert_eq!(rows(live.rendered()), [] as [String; 0]);
            live.push_str("p3\n\np4\n");
            live.advance(80, row);
            assert_eq!(rows(live.rendered()), ["p3//"]);
        }
    }

    mod clear {
        use super::*;

        #[test]
        fn drops_the_text_and_its_cached_rows() {
            let mut live = live("p1\n\np2\n");
            live.advance(80, row);

            live.clear();

            assert_eq!(live.text(), "");
            assert_eq!(rows(live.rendered()), [] as [String; 0]);
        }
    }

    mod invalidate {
        use super::*;

        #[test]
        fn keeps_the_text_for_the_tail() {
            let mut live = live("p1\n\np2\n");
            live.advance(80, row);

            live.invalidate();

            assert_eq!(rows(live.rendered()), [] as [String; 0]);
            assert_eq!(rows(&live.tail(row)), ["p1//p2/"]);
        }
    }

    mod starts_over_at {
        use super::*;

        #[test]
        fn holds_until_rows_are_cached_at_the_same_width() {
            let mut live = live("p1\n\np2\n");
            assert!(live.starts_over_at(80));

            live.advance(80, row);

            assert!(!live.starts_over_at(80));
            assert!(live.starts_over_at(40));
        }
    }

    mod advance {
        use super::*;

        #[test]
        fn caches_only_the_blocks_a_finished_line_closes() {
            for (text, cached) in [
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
                let mut live = live(text);

                live.advance(80, row);

                assert_eq!(live.boundary, cached.len(), "{text:?}");
            }
        }

        #[test]
        fn leaves_the_whole_text_to_the_tail_without_a_width() {
            let mut live = live("p1\n\np2\n");

            live.advance(0, row);

            assert_eq!(live.boundary, 0);
            assert_eq!(rows(live.rendered()), [] as [String; 0]);
        }

        #[test]
        fn keeps_earlier_rows_across_appends() {
            let mut live = live("p1\n\np2\n");
            live.advance(80, row);
            live.push_str("\np3\n");

            live.advance(80, row);

            assert_eq!(rows(live.rendered()), ["p1//", "p2//"]);
            assert_eq!(live.boundary, "p1\n\np2\n\n".len());
        }

        #[test]
        fn recaches_from_the_start_only_when_a_measured_width_changes() {
            for (width, cached) in [
                (80, &["p1//", "p2//"][..]),
                (40, &["p1//p2//"][..]),
                (0, &[][..]),
            ] {
                let mut live = live("p1\n\np2\n");
                live.advance(80, row);
                live.push_str("\np3\n");

                live.advance(width, row);

                assert_eq!(rows(live.rendered()), cached, "width {width}");
            }
        }
    }

    mod tail {
        use super::*;

        #[test]
        fn renders_from_the_last_complete_block() {
            let mut live = live("p1\n\np2\n\npartial");
            live.advance(80, row);

            let tail = live.tail(row);

            assert_eq!(rows(&tail), ["p2//partial"]);
        }
    }

    mod render {
        use super::*;

        #[test]
        fn follows_the_cached_blocks_with_the_tail() {
            let mut live = live("p1\n\np2\n\npartial");

            let rendered = live.render(80, row);

            assert_eq!(rows(&rendered), ["p1//", "p2//partial"]);
            assert_eq!(rows(live.rendered()), ["p1//"]);
        }
    }
}
