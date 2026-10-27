//! Wrapping list navigation and render-sized scroll windows.

use std::cell::Cell;
use std::ops::Range;

/// Which way a keyboard step moves through a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Forward,
    Backward,
}

/// Index one step after `index` among `len` rows, wrapping past the last. Needs `len > 0`.
pub(crate) fn wrap_next(index: usize, len: usize) -> usize {
    (index + 1) % len
}

/// Index one step before `index` among `len` rows, wrapping past the first. Needs `len > 0`.
pub(crate) fn wrap_prev(index: usize, len: usize) -> usize {
    index.checked_sub(1).unwrap_or(len - 1)
}

/// Scroll window over a list. `Cell`-backed because the window settles during `render(&self)`, when
/// the granted height is finally known.
#[derive(Debug, Default)]
pub(crate) struct Viewport {
    offset: Cell<usize>,
    height: Cell<usize>,
}

impl Viewport {
    /// Visible range of `len` rows in a `height`-row window, moved as little as possible to keep
    /// `cursor` in view.
    pub(crate) fn window(&self, len: usize, height: usize, cursor: Option<usize>) -> Range<usize> {
        let mut offset = self.offset.get().min(len.saturating_sub(height));
        if let Some(cursor) = cursor {
            if cursor < offset {
                offset = cursor;
            } else if height > 0 && cursor >= offset + height {
                offset = cursor + 1 - height;
            }
        }
        self.offset.set(offset);
        self.height.set(height);
        offset..len.min(offset + height)
    }

    /// Rows the last render showed, at least one, for page-sized cursor steps.
    pub(crate) fn page(&self) -> usize {
        self.height.get().max(1)
    }

    /// Scrolls a cursor-less window by `delta` rows, stopping where the last of `len` rows shows.
    pub(crate) fn scroll_by(&self, delta: isize, len: usize) {
        let max_offset = len.saturating_sub(self.height.get());
        let offset = self.offset.get().saturating_add_signed(delta);
        self.offset.set(offset.min(max_offset));
    }

    pub(crate) fn reset(&self) {
        self.offset.set(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod wrap_next {
        use super::*;

        #[test]
        fn advances_then_wraps_to_the_first_row() {
            assert_eq!(wrap_next(0, 3), 1);
            assert_eq!(wrap_next(2, 3), 0);
            assert_eq!(wrap_next(0, 1), 0);
        }
    }

    mod wrap_prev {
        use super::*;

        #[test]
        fn retreats_then_wraps_to_the_last_row() {
            assert_eq!(wrap_prev(2, 3), 1);
            assert_eq!(wrap_prev(0, 3), 2);
            assert_eq!(wrap_prev(0, 1), 0);
        }
    }

    mod window {
        use super::*;

        #[test]
        fn follows_the_cursor_with_minimal_movement() {
            let viewport = Viewport::default();
            assert_eq!(viewport.window(10, 4, Some(0)), 0..4);
            assert_eq!(viewport.window(10, 4, Some(3)), 0..4, "still in view");
            assert_eq!(viewport.window(10, 4, Some(5)), 2..6, "bottom edge follows");
            assert_eq!(
                viewport.window(10, 4, Some(4)),
                2..6,
                "back inside the window"
            );
            assert_eq!(viewport.window(10, 4, Some(1)), 1..5, "top edge follows");
        }

        #[test]
        fn reclamps_when_the_granted_height_changes() {
            let viewport = Viewport::default();
            assert_eq!(viewport.window(10, 3, Some(9)), 7..10);
            assert_eq!(
                viewport.window(10, 6, Some(9)),
                4..10,
                "taller window shows more"
            );
            assert_eq!(
                viewport.window(10, 2, Some(9)),
                8..10,
                "shorter window keeps the cursor"
            );
        }

        #[test]
        fn on_a_short_list_shows_every_row() {
            let viewport = Viewport::default();
            assert_eq!(viewport.window(2, 5, Some(1)), 0..2);
            assert_eq!(viewport.window(0, 5, None), 0..0);
        }

        #[test]
        fn with_zero_height_is_empty_without_moving() {
            let viewport = Viewport::default();
            viewport.window(10, 3, Some(4));
            assert_eq!(viewport.window(10, 0, Some(9)), 2..2);
            assert_eq!(viewport.page(), 1, "paging still moves by at least one row");
        }
    }

    mod page {
        use super::*;

        #[test]
        fn reports_the_last_rendered_height() {
            let viewport = Viewport::default();
            assert_eq!(viewport.page(), 1, "no render yet");
            viewport.window(20, 7, Some(0));
            assert_eq!(viewport.page(), 7);
        }
    }

    mod scroll_by {
        use super::*;

        #[test]
        fn stops_at_both_ends() {
            let viewport = Viewport::default();
            viewport.window(10, 4, None);
            viewport.scroll_by(3, 10);
            assert_eq!(viewport.window(10, 4, None), 3..7);
            viewport.scroll_by(100, 10);
            assert_eq!(
                viewport.window(10, 4, None),
                6..10,
                "last row pins the bottom"
            );
            viewport.scroll_by(-100, 10);
            assert_eq!(viewport.window(10, 4, None), 0..4);
        }
    }

    mod reset {
        use super::*;

        #[test]
        fn returns_the_window_to_the_top() {
            let viewport = Viewport::default();
            viewport.window(10, 4, Some(8));
            viewport.reset();
            assert_eq!(viewport.window(10, 4, None), 0..4);
        }
    }
}
