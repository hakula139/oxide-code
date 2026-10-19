//! What the input box, slash commands, modals, and the app's named keys ask the
//! [`App`](super::app::App) to do.

use crate::protocol::Op;
use crate::tui::keymap::{KeyAction, KeyChord, Keymap};

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
    /// Shift+Tab, which switches to the next permission mode.
    CycleMode,
    /// A preview of the picker's highlighted theme, without committing it. Cursor moves in the
    /// `/theme` modal emit this, and cancelling the modal restores the open-time snapshot.
    PreviewTheme {
        name: String,
    },
    /// A theme swap for the rest of the session. A restart returns to the configured theme.
    SwapTheme {
        name: String,
    },
}

// ── App Keymap ──

/// A named action whose key works while no modal holds the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AppAction {
    /// Switches the chat between its compact and expanded views.
    ToggleExpanded,
}

/// The app state that [`APP_KEYMAP`]'s hints read.
pub(super) struct AppView {
    /// Whether the chat shows every block in full.
    pub(super) expanded: bool,
}

/// The app's key actions, which take their keys before the input box sees them.
pub(super) const APP_KEYMAP: Keymap<AppAction, AppView> = Keymap::new(&[KeyAction {
    action: AppAction::ToggleExpanded,
    key: KeyChord::ctrl('o'),
    label: |view| {
        Some(if view.expanded {
            "to collapse"
        } else {
            "to expand"
        })
    },
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
            for (expanded, hint) in [(false, "Ctrl+O to expand"), (true, "Ctrl+O to collapse")] {
                assert_eq!(
                    APP_KEYMAP
                        .hint(AppAction::ToggleExpanded, &AppView { expanded })
                        .as_deref(),
                    Some(hint),
                );
            }
        }
    }
}
