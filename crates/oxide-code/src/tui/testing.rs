//! Test-only input and render fixtures shared across the TUI's test modules.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::{Buffer, CellWidth};

/// Returns the crossterm `Event` the event loop receives for `code` pressed with `modifiers`.
pub(super) fn key_event(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

pub(super) fn mouse_event(kind: MouseEventKind) -> Event {
    mouse_event_at(kind, 0, 0)
}

pub(super) fn mouse_event_at(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

/// Returns each row of `buffer` as displayed, skipping the placeholder cells a wide symbol covers.
pub(super) fn buffer_rows(buffer: &Buffer) -> Vec<String> {
    (buffer.area.top()..buffer.area.bottom())
        .map(|y| {
            let mut row = String::new();
            let mut covered = 0;
            for x in buffer.area.left()..buffer.area.right() {
                let cell = &buffer[(x, y)];
                if covered > 0 {
                    covered -= 1;
                } else {
                    row.push_str(cell.symbol());
                    covered = cell.cell_width().saturating_sub(1);
                }
            }
            row
        })
        .collect()
}

mod tests {
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    use super::*;

    // ── buffer_rows ──

    #[test]
    fn buffer_rows_read_an_offset_area_and_skip_the_cells_wide_symbols_cover() {
        let mut buffer = Buffer::empty(Rect::new(3, 1, 6, 2));
        buffer.set_string(3, 1, "界ｶﾞa", Style::default());

        let rows = buffer_rows(&buffer);

        assert_eq!(rows, ["界ｶﾞa ", "      "]);
    }
}
