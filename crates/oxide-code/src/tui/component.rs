use crossterm::event::Event;
use ratatui::Frame;
use ratatui::layout::Rect;

/// A self-contained UI component with its own state, event handling, and
/// rendering.
///
/// The root [`App`](super::app::App) dispatches events top-down and calls
/// `render` on each component with its allocated screen area.
///
/// Components return an [`Action`] to request behavior from the parent
/// (e.g., submitting a prompt, quitting). `None` means "event consumed,
/// no further action needed".
pub(crate) trait Component {
    /// Handle a crossterm event (keyboard, mouse, resize).
    ///
    /// Returns an [`Action`] if the event triggers a state change that the
    /// parent needs to know about.
    fn handle_event(&mut self, event: &Event) -> Option<Action>;

    /// Render the component into the given frame area.
    fn render(&self, frame: &mut Frame, area: Rect);
}

/// Actions that components emit upward to the root [`App`](super::app::App).
#[derive(Debug, Clone)]
pub(crate) enum Action {
    /// User submitted a prompt from the input area.
    SubmitPrompt(String),
    /// User requested quit (Ctrl+C / Ctrl+D).
    Quit,
}
