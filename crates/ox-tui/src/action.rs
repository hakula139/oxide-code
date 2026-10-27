//! What the input box, slash commands, modals, and the app's named keys ask the
//! [`App`](crate::app::App) to do.

use ox_core::protocol::Op;

use crate::keymap::{KeyAction, KeyChord, Keymap};
use crate::theme::builtin::BuiltinTheme;

/// A request from a TUI surface. [`Self::Submit`] runs as a slash command or a prompt, [`Self::Op`]
/// reaches the engine after the app applies its local state change, and [`Self::SlashPrompt`]
/// starts a turn. The other variants act on TUI state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Text submitted from the input box, which runs as a slash command or a prompt, or joins the
    /// queue while a turn runs.
    Submit(String),
    Op(Op),
    /// A prompt built from `line`, whose slash command has already echoed in chat. A fresh session
    /// takes `line` as its first-prompt title, and auto-compaction replays it. A failed turn hands the
    /// line back to an empty input, where it runs again as a command, or returns the pair to the
    /// queue head, where the built prompt runs as it is.
    SlashPrompt {
        line: String,
        prompt: String,
    },
    /// Switches to the next permission mode.
    CycleMode,
    /// A preview of the picker's highlighted theme, without committing it. Cursor moves in the
    /// `/theme` modal emit this, and cancelling the modal restores the open-time snapshot.
    PreviewTheme(&'static BuiltinTheme),
    /// A theme swap for the rest of the session. A restart returns to the configured theme.
    SwapTheme(&'static BuiltinTheme),
}

// ── App Keymap ──

/// A named action whose key works while no modal holds the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppAction {
    /// Switches the chat between its compact and expanded views.
    ToggleExpanded,
}

/// Ctrl+O, which toggles the expanded chat view.
pub(crate) const TOGGLE_EXPANDED: KeyChord = KeyChord::ctrl('o');

/// The app's key actions, which take their keys before the input box sees them.
pub(crate) const APP_KEYMAP: Keymap<AppAction, ()> = Keymap::new(&[KeyAction {
    action: AppAction::ToggleExpanded,
    key: TOGGLE_EXPANDED,
    label: |()| Some("to expand"),
}]);

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    mod app_keymap {
        use super::*;

        #[test]
        fn binds_ctrl_o_to_the_expanded_view_toggle_and_names_its_effect() {
            let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);

            assert_eq!(
                APP_KEYMAP.action_for(&ctrl_o),
                Some(AppAction::ToggleExpanded),
            );
            assert_eq!(
                APP_KEYMAP.hint(AppAction::ToggleExpanded, &()).as_deref(),
                Some("Ctrl+O to expand"),
            );
        }
    }
}
