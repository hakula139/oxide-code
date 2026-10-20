//! Terminal-cursor placement for the chat input and the modal line editors.

use ratatui::Frame;
use ratatui::layout::Rect;

/// Places the terminal cursor at `(raw_x, y)`, clamped inside the right edge of `area`.
pub(crate) fn place_clamped(frame: &mut Frame<'_>, raw_x: u16, y: u16, area: Rect) {
    let cursor_x = raw_x.min(area.right().saturating_sub(1));
    frame.set_cursor_position((cursor_x, y));
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    mod place_clamped {
        use super::*;

        #[test]
        fn within_area_keeps_raw_x() {
            let mut terminal = Terminal::new(TestBackend::new(20, 1)).unwrap();

            terminal
                .draw(|frame| {
                    place_clamped(frame, 5, 0, Rect::new(0, 0, 20, 1));
                })
                .unwrap();

            let pos = terminal.get_cursor_position().unwrap();
            assert_eq!((pos.x, pos.y), (5, 0));
        }

        #[test]
        fn past_right_edge_pulls_back_inside() {
            let mut terminal = Terminal::new(TestBackend::new(20, 1)).unwrap();

            terminal
                .draw(|frame| {
                    place_clamped(frame, 30, 0, Rect::new(0, 0, 20, 1));
                })
                .unwrap();

            let pos = terminal.get_cursor_position().unwrap();
            assert_eq!((pos.x, pos.y), (19, 0));
        }

        #[test]
        fn in_offset_area_anchors_relative_to_right_edge() {
            // The area spans columns 5 through 14.
            let mut terminal = Terminal::new(TestBackend::new(20, 1)).unwrap();

            terminal
                .draw(|frame| {
                    place_clamped(frame, 100, 0, Rect::new(5, 0, 10, 1));
                })
                .unwrap();

            let pos = terminal.get_cursor_position().unwrap();
            assert_eq!((pos.x, pos.y), (14, 0));
        }
    }
}
