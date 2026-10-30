//! What the input box, slash commands, modals, and the app's named keys ask the
//! [`App`](crate::app::App) to do.

use strum::VariantArray as _;

use ox_core::permission::Mode;
use ox_core::protocol::Op;

use crate::keymap::{KeyAction, KeyChord, Keymap};
use crate::theme::builtin::BuiltinTheme;
use crate::viewport::{Direction, wrap_next, wrap_prev};

/// A request from a TUI surface. [`Self::Submit`] runs as a slash command or a prompt, [`Self::Op`]
/// reaches the engine after the app applies its local state change, and [`Self::SlashPrompt`]
/// starts a turn. The other variants act on TUI state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Text submitted from the input box, which runs as a slash command or a prompt, or joins the
    /// queue while a turn runs.
    Submit(String),
    /// Replaces the draft without submitting it or changing queued prompts.
    ReplaceInput(String),
    Op(Op),
    /// A prompt built from `line`, whose slash command has already echoed in chat. A fresh session
    /// takes `line` as its first-prompt title, and auto-compaction replays it. A failed turn hands
    /// the line back to an empty input, where it runs again as a command, or returns the pair to
    /// the queue head, where the built prompt runs as it is.
    SlashPrompt {
        line: String,
        prompt: String,
    },
    /// Steps to the permission mode on `direction`.
    CycleMode(Direction),
    /// A preview of the picker's highlighted theme, without committing it. Cursor moves in the
    /// `/theme` modal emit this, and cancelling the modal restores the open-time snapshot.
    PreviewTheme(&'static BuiltinTheme),
    /// Restores the theme from before the first preview, which the `/theme` modal sends when it
    /// closes without committing.
    RevertThemePreview,
    /// A theme swap for the rest of the session. A restart returns to the configured theme.
    SwapTheme(&'static BuiltinTheme),
}

// ── Mode Cycle ──

/// The mode one step from `current` in `direction`, wrapping at both ends.
pub(crate) fn next_mode(current: Mode, direction: Direction) -> Mode {
    let index = Mode::VARIANTS
        .iter()
        .position(|&mode| mode == current)
        .expect("Mode::VARIANTS lists every permission mode");
    let next = match direction {
        Direction::Forward => wrap_next(index, Mode::VARIANTS.len()),
        Direction::Backward => wrap_prev(index, Mode::VARIANTS.len()),
    };
    Mode::VARIANTS[next]
}

// ── App Keymap ──

/// A named action whose key works while no modal holds the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppAction {
    /// Switches the chat between its compact and expanded views.
    ToggleExpanded,
    SearchPrompts,
}

/// Ctrl+O toggles the expanded chat view.
pub(crate) const TOGGLE_EXPANDED: KeyChord = KeyChord::ctrl('o');

/// The app's key actions, which take their keys before the input box sees them.
pub(crate) const APP_KEYMAP: Keymap<AppAction, ()> = Keymap::new(&[
    KeyAction {
        action: AppAction::ToggleExpanded,
        key: TOGGLE_EXPANDED,
        effect: |()| Some("expand"),
    },
    KeyAction {
        action: AppAction::SearchPrompts,
        key: KeyChord::ctrl('r'),
        effect: |()| Some("search prompts"),
    },
]);

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

    mod next_mode {
        use super::*;

        #[test]
        fn steps_through_every_mode_and_wraps_at_both_ends() {
            for (start, forward, backward) in [
                (Mode::Manual, Mode::Plan, Mode::Yolo),
                (Mode::Plan, Mode::Yolo, Mode::Manual),
                (Mode::Yolo, Mode::Manual, Mode::Plan),
            ] {
                assert_eq!(
                    next_mode(start, Direction::Forward),
                    forward,
                    "{start} forward",
                );
                assert_eq!(
                    next_mode(start, Direction::Backward),
                    backward,
                    "{start} backward",
                );
            }
        }
    }
}
