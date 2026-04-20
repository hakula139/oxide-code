use crossterm::event::Event;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::agent::event::UserAction;

/// A self-contained UI component with its own state, event handling, and
/// rendering.
///
/// The root [`App`](super::app::App) dispatches events top-down and calls
/// `render` on each component with its allocated screen area.
///
/// Components return a [`UserAction`] to request behavior from the parent
/// (e.g., submitting a prompt, quitting). `None` means "event consumed,
/// no further action needed".
pub(crate) trait Component {
    /// Handle a crossterm event (keyboard, mouse, resize).
    ///
    /// Returns a [`UserAction`] when the event should reach the agent loop
    /// (e.g., a submitted prompt or quit request).
    fn handle_event(&mut self, event: &Event) -> Option<UserAction>;

    /// Render the component into the given frame area.
    fn render(&self, frame: &mut Frame, area: Rect);
}
