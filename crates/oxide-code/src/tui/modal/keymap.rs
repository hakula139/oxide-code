//! Named key actions for a modal. Each entry binds an action to its key and footer hint, so the
//! modal dispatches keys and renders hints from one table and the two cannot drift apart.

use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// ── KeyChord ──

/// A key, optionally held with Ctrl.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyChord {
    code: KeyCode,
    ctrl: bool,
}

impl KeyChord {
    pub(crate) const fn plain(code: KeyCode) -> Self {
        Self { code, ctrl: false }
    }

    pub(crate) const fn ctrl(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            ctrl: true,
        }
    }

    /// Whether `event` presses this key with Ctrl held exactly when the chord needs it. Other
    /// modifiers are ignored.
    fn matches(self, event: &KeyEvent) -> bool {
        event.code == self.code && event.modifiers.contains(KeyModifiers::CONTROL) == self.ctrl
    }

    #[cfg(test)]
    pub(crate) fn event(self) -> KeyEvent {
        let modifiers = if self.ctrl {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::NONE
        };
        KeyEvent::new(self.code, modifiers)
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        match self.code {
            KeyCode::Char(c) => write!(f, "{}", c.to_ascii_uppercase()),
            // crossterm names Enter "Return" on macOS.
            KeyCode::Enter => f.write_str("Enter"),
            code => write!(f, "{code}"),
        }
    }
}

// ── Keymap ──

/// A named action bound to a key, with the footer hint it shows.
pub(crate) struct KeyAction<A, V> {
    pub(crate) action: A,
    pub(crate) key: KeyChord,
    /// Hint text after the key, such as `to resume`, for the modal state `V`. `None` leaves the
    /// action out of the footer while it has nothing to act on.
    pub(crate) label: fn(&V) -> Option<&'static str>,
}

/// A modal's key actions, in footer order.
pub(crate) struct Keymap<A: 'static, V: 'static> {
    actions: &'static [KeyAction<A, V>],
}

impl<A: Copy, V> Keymap<A, V> {
    pub(crate) const fn new(actions: &'static [KeyAction<A, V>]) -> Self {
        Self { actions }
    }

    /// The action `event` triggers, if any.
    pub(crate) fn action_for(&self, event: &KeyEvent) -> Option<A> {
        self.actions
            .iter()
            .find(|entry| entry.key.matches(event))
            .map(|entry| entry.action)
    }

    /// `{key} {label}` for each action labeled in `view`, in table order.
    pub(crate) fn hints<'a>(&'a self, view: &'a V) -> impl Iterator<Item = String> + 'a {
        self.actions.iter().filter_map(move |entry| {
            (entry.label)(view).map(|label| format!("{} {label}", entry.key))
        })
    }

    #[cfg(test)]
    pub(crate) fn actions(&self) -> &[KeyAction<A, V>] {
        self.actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test fixture ──

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum TestAction {
        Toggle,
        Reset,
    }

    /// `Toggle` names the state it switches to, and `Reset` shows only while the state is on.
    const ACTIONS: &[KeyAction<TestAction, bool>] = &[
        KeyAction {
            action: TestAction::Toggle,
            key: KeyChord::ctrl('x'),
            label: |on| Some(if *on { "to stop" } else { "to start" }),
        },
        KeyAction {
            action: TestAction::Reset,
            key: KeyChord::plain(KeyCode::Enter),
            label: |on| on.then_some("to reset"),
        },
    ];

    fn keymap() -> Keymap<TestAction, bool> {
        Keymap::new(ACTIONS)
    }

    // ── KeyChord::fmt ──

    #[test]
    fn fmt_names_ctrl_chords_and_plain_keys() {
        assert_eq!(KeyChord::ctrl('x').to_string(), "Ctrl+X");
        assert_eq!(KeyChord::plain(KeyCode::Char('y')).to_string(), "Y");
        assert_eq!(KeyChord::plain(KeyCode::Enter).to_string(), "Enter");
        assert_eq!(KeyChord::plain(KeyCode::Esc).to_string(), "Esc");
        assert_eq!(KeyChord::plain(KeyCode::Tab).to_string(), "Tab");
    }

    // ── Keymap::action_for ──

    #[test]
    fn action_for_requires_ctrl_exactly_when_the_chord_has_it() {
        let keymap = keymap();
        let press = |code, modifiers| keymap.action_for(&KeyEvent::new(code, modifiers));

        assert_eq!(
            press(KeyCode::Char('x'), KeyModifiers::CONTROL),
            Some(TestAction::Toggle),
        );
        assert_eq!(press(KeyCode::Char('x'), KeyModifiers::NONE), None);
        assert_eq!(
            press(KeyCode::Enter, KeyModifiers::SHIFT),
            Some(TestAction::Reset),
        );
        assert_eq!(press(KeyCode::Enter, KeyModifiers::CONTROL), None);
        assert_eq!(press(KeyCode::Tab, KeyModifiers::NONE), None);
    }

    // ── Keymap::hints ──

    #[test]
    fn hints_follow_the_view_and_skip_unlabeled_actions() {
        let keymap = keymap();

        assert_eq!(
            keymap.hints(&false).collect::<Vec<_>>(),
            ["Ctrl+X to start"]
        );
        assert_eq!(
            keymap.hints(&true).collect::<Vec<_>>(),
            ["Ctrl+X to stop", "Enter to reset"],
        );
    }
}
