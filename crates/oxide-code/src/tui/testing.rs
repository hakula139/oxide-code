//! Test-only input fixtures shared across the TUI's test modules.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

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
