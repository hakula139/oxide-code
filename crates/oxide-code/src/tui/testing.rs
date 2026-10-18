//! Test-only input fixtures shared by the TUI's app, component, and modal tests.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

pub(super) fn key_event(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}
