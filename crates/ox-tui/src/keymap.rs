//! Key chords, the shape of every key hint, and the named actions a surface routes through a
//! table. Each surface matches its other keys beside the state they change.

use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// ── KeyChord ──

/// Ctrl+C, shared so the modal stack, selection copy, and the app agree on which keys count.
pub(crate) const CTRL_C: KeyChord = KeyChord::ctrl('c');

/// Ctrl+D, which deletes forward or exits from an empty input.
pub(crate) const CTRL_D: KeyChord = KeyChord::ctrl('d');

/// Ctrl+End, which returns the chat to the newest output.
pub(crate) const CTRL_END: KeyChord = KeyChord::ctrl_code(KeyCode::End);

/// Returns whether `event` holds no modifier but Shift, which terminals report on uppercase
/// letters.
pub(crate) fn is_unmodified(event: &KeyEvent) -> bool {
    (event.modifiers - KeyModifiers::SHIFT).is_empty()
}

/// The `{key} to {effect}` shape every key hint the TUI paints takes.
pub(crate) fn hint(key: impl fmt::Display, effect: &str) -> String {
    format!("{key} to {effect}")
}

/// A key, optionally held with Ctrl.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyChord {
    code: KeyCode,
    ctrl: bool,
}

impl KeyChord {
    pub(crate) const fn plain(code: KeyCode) -> Self {
        Self { code, ctrl: false }
    }

    pub(crate) const fn ctrl(c: char) -> Self {
        Self::ctrl_code(KeyCode::Char(c))
    }

    pub(crate) const fn ctrl_code(code: KeyCode) -> Self {
        Self { code, ctrl: true }
    }

    /// Returns whether `event` presses this key with Ctrl held exactly when the chord holds it and
    /// no other modifier but Shift. A Ctrl letter matches in either case, since Windows reports
    /// Ctrl+C as `C` while Caps Lock or Shift is on.
    pub(crate) fn matches(self, event: &KeyEvent) -> bool {
        if !self.ctrl {
            return event.code == self.code && is_unmodified(event);
        }
        let same_key = match (event.code, self.code) {
            (KeyCode::Char(pressed), KeyCode::Char(bound)) => pressed.eq_ignore_ascii_case(&bound),
            (pressed, bound) => pressed == bound,
        };
        same_key && event.modifiers - KeyModifiers::SHIFT == KeyModifiers::CONTROL
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

/// A named action bound to a key, with the hint it shows.
pub(crate) struct KeyAction<A, V> {
    pub(crate) action: A,
    pub(crate) key: KeyChord,
    /// What the key does in the state `V`, such as `resume`, which the hint names after the key.
    /// `None` hides the hint while the action has nothing to act on, such as a row action over an
    /// empty list.
    pub(crate) effect: fn(&V) -> Option<&'static str>,
}

impl<A, V> KeyAction<A, V> {
    /// The [`hint`] in `view`, or `None` while the action has no effect there.
    fn hint(&self, view: &V) -> Option<String> {
        (self.effect)(view).map(|effect| hint(self.key, effect))
    }
}

/// A modal's or the app's key actions, in hint order.
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

    /// Each action with an effect in `view` and its [`hint`], in table order.
    pub(crate) fn hints<'a>(&'a self, view: &'a V) -> impl Iterator<Item = (A, String)> + 'a {
        self.actions
            .iter()
            .filter_map(move |entry| Some((entry.action, entry.hint(view)?)))
    }

    /// The [`hint`] of `action` in `view`, or `None` while it has no effect there.
    pub(crate) fn hint(&self, action: A, view: &V) -> Option<String>
    where
        A: PartialEq,
    {
        self.actions
            .iter()
            .filter(|entry| entry.action == action)
            .find_map(|entry| entry.hint(view))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestAction {
        Toggle,
        Reset,
        Confirm,
    }

    /// `Toggle` names the state it switches to, and `Reset` shows only while the state is on.
    const ACTIONS: &[KeyAction<TestAction, bool>] = &[
        KeyAction {
            action: TestAction::Toggle,
            key: KeyChord::ctrl('x'),
            effect: |on| Some(if *on { "stop" } else { "start" }),
        },
        KeyAction {
            action: TestAction::Reset,
            key: KeyChord::plain(KeyCode::Enter),
            effect: |on| on.then_some("reset"),
        },
        KeyAction {
            action: TestAction::Confirm,
            key: KeyChord::plain(KeyCode::Char('y')),
            effect: |_| Some("confirm"),
        },
    ];

    fn keymap() -> Keymap<TestAction, bool> {
        Keymap::new(ACTIONS)
    }

    mod hint {
        use super::*;

        #[test]
        fn names_the_key_then_its_effect() {
            assert_eq!(
                hint(KeyChord::plain(KeyCode::Esc), "cancel"),
                "Esc to cancel"
            );
            assert_eq!(
                hint(CTRL_END, "jump to bottom"),
                "Ctrl+End to jump to bottom"
            );
        }
    }

    mod key_chord {
        use super::*;

        #[test]
        fn display_names_ctrl_chords_and_plain_keys() {
            assert_eq!(KeyChord::ctrl('x').to_string(), "Ctrl+X");
            assert_eq!(KeyChord::plain(KeyCode::Char('y')).to_string(), "Y");
            assert_eq!(KeyChord::plain(KeyCode::Enter).to_string(), "Enter");
            assert_eq!(KeyChord::plain(KeyCode::Esc).to_string(), "Esc");
            assert_eq!(KeyChord::plain(KeyCode::Tab).to_string(), "Tab");
            assert_eq!(CTRL_END.to_string(), "Ctrl+End");
        }
    }

    mod keymap {
        use super::*;

        mod action_for {
            use super::*;

            #[test]
            fn matches_ctrl_and_alt_exactly_while_ignoring_shift_and_case() {
                let keymap = keymap();
                let press = |code, modifiers| keymap.action_for(&KeyEvent::new(code, modifiers));

                assert_eq!(
                    press(KeyCode::Char('x'), KeyModifiers::CONTROL),
                    Some(TestAction::Toggle),
                );
                assert_eq!(
                    press(KeyCode::Char('X'), KeyModifiers::CONTROL),
                    Some(TestAction::Toggle),
                    "Windows reports Ctrl+X as X under Caps Lock",
                );
                assert_eq!(press(KeyCode::Char('x'), KeyModifiers::NONE), None);
                assert_eq!(
                    press(KeyCode::Char('y'), KeyModifiers::NONE),
                    Some(TestAction::Confirm),
                );
                assert_eq!(press(KeyCode::Char('y'), KeyModifiers::CONTROL), None);
                assert_eq!(
                    press(
                        KeyCode::Char('x'),
                        KeyModifiers::CONTROL | KeyModifiers::SHIFT
                    ),
                    Some(TestAction::Toggle),
                );
                assert_eq!(
                    press(
                        KeyCode::Char('x'),
                        KeyModifiers::CONTROL | KeyModifiers::ALT
                    ),
                    None,
                );
                assert_eq!(
                    press(KeyCode::Enter, KeyModifiers::SHIFT),
                    Some(TestAction::Reset),
                );
                assert_eq!(press(KeyCode::Enter, KeyModifiers::CONTROL), None);
                assert_eq!(press(KeyCode::Enter, KeyModifiers::ALT), None);
                assert_eq!(press(KeyCode::Tab, KeyModifiers::NONE), None);
            }
        }

        mod hints {
            use super::*;

            #[test]
            fn follow_the_view_and_skip_actions_without_an_effect() {
                let keymap = keymap();

                assert_eq!(
                    keymap.hints(&false).collect::<Vec<_>>(),
                    [
                        (TestAction::Toggle, "Ctrl+X to start".to_owned()),
                        (TestAction::Confirm, "Y to confirm".to_owned()),
                    ],
                );
                assert_eq!(
                    keymap.hints(&true).collect::<Vec<_>>(),
                    [
                        (TestAction::Toggle, "Ctrl+X to stop".to_owned()),
                        (TestAction::Reset, "Enter to reset".to_owned()),
                        (TestAction::Confirm, "Y to confirm".to_owned()),
                    ],
                );
            }
        }

        mod hint {
            use super::*;

            #[test]
            fn names_one_action_in_the_view_and_hides_it_without_an_effect() {
                let keymap = keymap();

                assert_eq!(
                    keymap.hint(TestAction::Toggle, &true).as_deref(),
                    Some("Ctrl+X to stop"),
                );
                assert_eq!(keymap.hint(TestAction::Reset, &false), None);
            }
        }
    }
}
