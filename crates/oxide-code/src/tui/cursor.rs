//! Shared terminal-cursor placement helpers — every input surface (the chat input panel and the
//! modal pickers' search bar) ends up clamping a computed `raw_x` to the right edge of its area
//! before calling `Frame::set_cursor_position`.

use ratatui::Frame;
use ratatui::layout::Rect;

/// Place the terminal-native cursor at `(raw_x, y)`, clamped to the right edge of `area` so that
/// the cursor never escapes its containing region when callers compute positions from a sum of
/// widths (prompt + query, or column + textarea origin).
pub(crate) fn place_clamped(frame: &mut Frame<'_>, raw_x: u16, y: u16, area: Rect) {
    let cursor_x = raw_x.min(area.right().saturating_sub(1));
    frame.set_cursor_position((cursor_x, y));
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    // ── place_clamped ──

    #[test]
    fn place_clamped_within_area_keeps_raw_x() {
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
    fn place_clamped_past_right_edge_pulls_back_inside() {
        // raw_x = 30 in a 20-wide area would land outside; clamp parks us at the rightmost cell.
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
    fn place_clamped_in_offset_area_anchors_relative_to_right_edge() {
        // Area starting at x=5 with width=10 → right edge column = 14. raw_x past 14 clamps there.
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
