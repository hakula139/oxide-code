//! Modal overlay primitive.
//!
//! A modal is a focus-grabbing UI surface that intercepts keyboard
//! events while active and produces a typed result on submission.
//! Lives in the band between the chat scroll and the input area —
//! the same row range the slash autocomplete popup uses, but wider.
//!
//! [`Modal`] is the trait every concrete modal implements.
//! [`ModalStack`] owns the active modal(s) and is held by
//! [`crate::tui::app::App`]. The stack is `Vec`-backed so a future
//! "confirm leave?" overlay can `push` over an existing picker
//! without redesigning ownership.
//!
//! Companion design: `docs/design/slash/modals.md` (added with the
//! first concrete modal).
//!
//! Related research: `docs/research/slash/modals.md`.

pub(crate) mod list_picker;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::agent::event::UserAction;
use crate::tui::theme::Theme;

// ── Modal Trait ──

/// A focus-grabbing UI overlay. While active, the modal owns keyboard
/// focus end-to-end — App routes keys to it before any other component
/// sees them. A modal renders into a band the manager allocates above
/// the input area.
///
/// `Send` because App lives on the tokio runtime; never `Sync` —
/// modals own mutable state and are not shared across threads.
pub(crate) trait Modal: Send {
    /// Visible height in rows for the given width. The manager
    /// allocates exactly this much vertical space; the modal must
    /// render fully within it. May vary with `width` (wrap-aware
    /// modals) or stay constant (fixed-height pickers).
    fn height(&self, width: u16) -> u16;

    /// Render into `area`. Width and height match what `height` last
    /// returned for `area.width`.
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme);

    /// Process a key. The return value drives manager behavior:
    /// stay open, dismiss, or submit a typed action.
    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey;
}

// ── Outcomes ──

/// Outcome of a single key event delivered to a modal.
#[derive(Debug)]
pub(crate) enum ModalKey {
    /// Stay open. Key was consumed.
    Consumed,
    /// Close without dispatching anything.
    Cancelled,
    /// Close and apply this action.
    Submitted(ModalAction),
}

/// What a submitted modal asks the manager to do.
#[derive(Debug)]
pub(crate) enum ModalAction {
    /// Modal already applied its effect locally — no dispatch needed.
    /// Reserved for future live-preview modals (e.g. `/theme`) where
    /// arrowing through the list already mutated UI state.
    None,
    /// Forward a [`UserAction`] to the agent loop. Same channel as a
    /// keyboard-typed action, so `/model` swaps and friends share one
    /// path.
    User(UserAction),
}

// ── ModalStack ──

/// Owns the active modal(s). Single-modal-at-a-time today; the `Vec`
/// is there so a "confirm leave?" overlay inside a picker can `push`
/// without ownership rework.
#[derive(Default)]
pub(crate) struct ModalStack {
    stack: Vec<Box<dyn Modal>>,
}

impl ModalStack {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.stack.is_empty()
    }

    /// Push a modal onto the stack. The new modal receives keys until
    /// it submits or cancels; the previous top resumes.
    pub(crate) fn push(&mut self, modal: Box<dyn Modal>) {
        self.stack.push(modal);
    }

    /// Total height the stack needs above the input. Today only the
    /// top modal renders; the height reflects that. If we ever stack
    /// visually, this sums.
    pub(crate) fn height(&self, width: u16) -> u16 {
        self.stack.last().map_or(0, |m| m.height(width))
    }

    /// Render the visible modal into `area`. No-op if empty.
    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if let Some(top) = self.stack.last() {
            top.render(frame, area, theme);
        }
    }

    /// Deliver `event` to the top modal. Returns the action to dispatch
    /// (or `ModalAction::None` to indicate a silent close), or `None`
    /// if the modal stayed open (key consumed) or the stack is empty.
    pub(crate) fn handle_key(&mut self, event: &KeyEvent) -> Option<ModalAction> {
        let outcome = self.stack.last_mut()?.handle_key(event);
        match outcome {
            ModalKey::Consumed => None,
            ModalKey::Cancelled => {
                self.stack.pop();
                Some(ModalAction::None)
            }
            ModalKey::Submitted(action) => {
                self.stack.pop();
                Some(action)
            }
        }
    }
}

// ── Test Fixtures ──

#[cfg(test)]
pub(crate) mod testing {
    //! Synthetic modal for exercising the manager without coupling
    //! tests to a concrete picker.

    use super::*;

    /// Modal with scripted key handling — emits a fixed action on a
    /// sentinel key. Used to drive `ModalStack` tests and the App-side
    /// gate before any concrete modal exists.
    pub(crate) struct ScriptedModal {
        pub(crate) on_submit_key: char,
        pub(crate) on_cancel_key: char,
        pub(crate) submit_action: ModalAction,
        pub(crate) declared_height: u16,
    }

    impl ScriptedModal {
        pub(crate) fn new(submit_action: ModalAction) -> Self {
            Self {
                on_submit_key: 's',
                on_cancel_key: 'c',
                submit_action,
                declared_height: 3,
            }
        }
    }

    impl Modal for ScriptedModal {
        fn height(&self, _width: u16) -> u16 {
            self.declared_height
        }

        fn render(&self, _frame: &mut Frame<'_>, _area: Rect, _theme: &Theme) {}

        fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
            use crossterm::event::KeyCode;
            match event.code {
                KeyCode::Char(c) if c == self.on_submit_key => {
                    let mut taken = ModalAction::None;
                    std::mem::swap(&mut self.submit_action, &mut taken);
                    ModalKey::Submitted(taken)
                }
                KeyCode::Char(c) if c == self.on_cancel_key => ModalKey::Cancelled,
                _ => ModalKey::Consumed,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::testing::ScriptedModal;
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::from(KeyCode::Char(c))
    }

    // ── ModalStack ──

    #[test]
    fn empty_stack_reports_inactive_and_zero_height() {
        let stack = ModalStack::new();
        assert!(!stack.is_active());
        assert_eq!(stack.height(80), 0);
    }

    #[test]
    fn push_activates_stack_and_height_reflects_top_modal() {
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::None)));
        assert!(stack.is_active());
        assert_eq!(stack.height(80), 3);
    }

    #[test]
    fn handle_key_consumed_keeps_modal_active() {
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::None)));
        // Any key that's neither submit-sentinel nor cancel-sentinel
        // is consumed — stack stays active and returns None.
        assert!(stack.handle_key(&key('x')).is_none());
        assert!(stack.is_active());
    }

    #[test]
    fn handle_key_cancel_pops_and_yields_modal_action_none() {
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::None)));
        // Cancel must surface a `Some(ModalAction::None)` so App can
        // distinguish "modal closed silently" from "key consumed".
        let outcome = stack.handle_key(&key('c'));
        assert!(matches!(outcome, Some(ModalAction::None)));
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_submit_pops_and_yields_modal_action_user() {
        let mut stack = ModalStack::new();
        let action = UserAction::Cancel;
        stack.push(Box::new(ScriptedModal::new(ModalAction::User(
            action.clone(),
        ))));
        let outcome = stack.handle_key(&key('s'));
        assert!(
            matches!(outcome, Some(ModalAction::User(a)) if a == action),
            "submit must surface the modal's UserAction unchanged",
        );
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_on_empty_stack_returns_none_without_panicking() {
        // No active modal → no key delivery, no stack mutation.
        let mut stack = ModalStack::new();
        assert!(stack.handle_key(&key('s')).is_none());
        assert!(!stack.is_active());
    }

    #[test]
    fn nested_push_routes_keys_to_top_only() {
        // Two-deep stack: keys go to the top until it pops, then the
        // inner one resumes. Pin so a regression that fans keys to all
        // layers fails here.
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::User(
            UserAction::Clear,
        ))));
        let mut top = ScriptedModal::new(ModalAction::None);
        top.declared_height = 5;
        stack.push(Box::new(top));

        assert_eq!(stack.height(80), 5, "top modal's height wins");
        let outcome = stack.handle_key(&key('s'));
        assert!(matches!(outcome, Some(ModalAction::None)));
        assert!(stack.is_active(), "inner modal still active");
        assert_eq!(stack.height(80), 3, "inner modal's height resumes");
    }
}
