//! What the input box, slash commands, and modals ask the [`App`](super::app::App) to do.

use crate::protocol::Op;

/// A request from a TUI surface. [`Self::Op`] reaches the engine after the app applies its local
/// state change. The other variants act on TUI state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Text submitted from the input box, which runs as a slash command or a prompt, or joins the
    /// queue while a turn runs.
    Submit(String),
    Op(Op),
    /// Ctrl+C while idle. A second one inside the exit window quits.
    ConfirmExit,
    /// Live-preview the picker's highlighted theme without committing it. Cursor moves in the
    /// `/theme` modal emit this, and cancelling the modal restores the open-time snapshot.
    PreviewTheme {
        name: String,
    },
    /// Commit a theme swap for the rest of the session. Restart returns to user config.
    SwapTheme {
        name: String,
    },
}
