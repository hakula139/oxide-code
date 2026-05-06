//! Modal overlay primitive — intercepts keys while active and emits a typed result on submission.
//! Renders between chat scroll and input. [`ModalStack`] is `Vec`-backed so a future overlay
//! (e.g. "confirm leave?") can `push` over an existing picker without ownership churn.
//!
//! Companion design: `docs/design/slash/modals.md`.

pub(crate) mod list_picker;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::agent::event::UserAction;
use crate::tui::theme::Theme;

/// One-row top separator above the modal body — visually delineates the modal from the chat.
const TOP_BORDER_HEIGHT: u16 = 1;
const TOP_BORDER_GLYPH: char = '─';

// ── Modal Trait ──

/// Focus-grabbing UI overlay. `Send` because App lives on tokio; not `Sync` — modals own mutable
/// state and are exclusively driven from the App task.
pub(crate) trait Modal: Send {
    /// Total rows the modal needs at the given terminal width, before the wrapping separator.
    fn height(&self, width: u16) -> u16;

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme);

    /// Routes one key event. Returns whether the modal consumed it, was cancelled, or submitted
    /// a typed action; the stack pops on cancel / submit.
    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey;
}

// ── Outcomes ──

#[derive(Debug)]
pub(crate) enum ModalKey {
    Consumed,
    Cancelled,
    Submitted(ModalAction),
}

#[derive(Debug)]
pub(crate) enum ModalAction {
    /// No dispatch needed (live-preview modals that already mutated UI state).
    None,
    /// Forward a [`UserAction`] to the agent loop — shares the keyboard-typed dispatch path.
    User(UserAction),
}

// ── ModalStack ──

/// LIFO stack of modal overlays. Only the top modal renders and receives keys; nested entries
/// resume in reverse `push` order on cancel / submit. Single-modal-at-a-time today; the `Vec`
/// is there so a future "confirm leave?" overlay inside a picker can `push` without ownership
/// rework.
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

    /// Height above the input — top modal's body plus a one-row separator.
    pub(crate) fn height(&self, width: u16) -> u16 {
        self.stack
            .last()
            .map_or(0, |m| m.height(width).saturating_add(TOP_BORDER_HEIGHT))
    }

    /// Render the visible modal into `area`. Paints a one-row top separator first, then delegates
    /// the remainder to the modal. No-op if empty.
    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let Some(top) = self.stack.last() else {
            return;
        };
        if area.height == 0 {
            return;
        }
        let border_area = Rect {
            height: TOP_BORDER_HEIGHT.min(area.height),
            ..area
        };
        let border = Line::from(Span::styled(
            TOP_BORDER_GLYPH.to_string().repeat(usize::from(area.width)),
            theme.dim(),
        ));
        frame.render_widget(Paragraph::new(border).style(theme.surface()), border_area);

        let body_height = area.height.saturating_sub(TOP_BORDER_HEIGHT);
        if body_height == 0 {
            return;
        }
        let body_area = Rect {
            x: area.x,
            y: area.y.saturating_add(TOP_BORDER_HEIGHT),
            width: area.width,
            height: body_height,
        };
        top.render(frame, body_area, theme);
    }

    /// Routes `event` to the top modal. `None` = key consumed or stack empty;
    /// `Some(ModalAction::None)` = silent close. Esc and Ctrl+C cancel any modal universally —
    /// short-circuit before delegation so individual modals don't reimplement the gesture.
    pub(crate) fn handle_key(&mut self, event: &KeyEvent) -> Option<ModalAction> {
        if self.stack.is_empty() {
            return None;
        }
        if is_universal_cancel(event) {
            self.stack.pop();
            return Some(ModalAction::None);
        }
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

fn is_universal_cancel(event: &KeyEvent) -> bool {
    matches!(event.code, KeyCode::Esc)
        || (event.code == KeyCode::Char('c') && event.modifiers.contains(KeyModifiers::CONTROL))
}

// ── Test Fixtures ──

#[cfg(test)]
pub(crate) mod testing {
    //! Synthetic modal for exercising the manager without coupling
    //! tests to a concrete picker.

    use super::*;

    /// Emits a fixed action on a sentinel key for exercising `ModalStack`.
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

    fn key_with_mods(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    // ── is_active ──

    #[test]
    fn empty_stack_reports_inactive_and_zero_height() {
        let stack = ModalStack::new();
        assert!(!stack.is_active());
        assert_eq!(stack.height(80), 0);
    }

    // ── push ──

    #[test]
    fn push_activates_stack_and_height_reflects_top_modal() {
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::None)));
        assert!(stack.is_active());
        // Modal body (3) + one-row top separator.
        assert_eq!(stack.height(80), 3 + TOP_BORDER_HEIGHT);
    }

    // ── render ──

    #[test]
    fn render_paints_top_border_then_delegates_body_below_it() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut stack = ModalStack::new();
        let modal = ScriptedModal::new(ModalAction::None);
        let body_height = modal.declared_height;
        stack.push(Box::new(modal));

        let theme = Theme::default();
        let width: u16 = 12;
        let total_height = stack.height(width);
        assert_eq!(total_height, body_height + TOP_BORDER_HEIGHT);

        let mut terminal = Terminal::new(TestBackend::new(width, total_height)).unwrap();
        terminal
            .draw(|frame| {
                stack.render(frame, Rect::new(0, 0, width, total_height), &theme);
            })
            .expect("render must not panic");

        let buf = terminal.backend().buffer();
        for x in 0..width {
            let symbol = buf[(x, 0)].symbol();
            assert_eq!(
                symbol,
                TOP_BORDER_GLYPH.to_string(),
                "top row col {x} must be border glyph; got {symbol:?}",
            );
        }
    }

    #[test]
    fn render_no_ops_when_stack_empty_or_area_smaller_than_body() {
        // Three short-circuit branches in `render`: empty stack, area.height == 0, and
        // body_height == 0 (area only big enough for the border row).
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();

        let empty = ModalStack::new();
        let mut t1 = Terminal::new(TestBackend::new(8, 2)).unwrap();
        t1.draw(|frame| empty.render(frame, Rect::new(0, 0, 8, 2), &theme))
            .expect("empty render");

        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::None)));
        let mut t2 = Terminal::new(TestBackend::new(8, 1)).unwrap();
        t2.draw(|frame| stack.render(frame, Rect::new(0, 0, 8, 0), &theme))
            .expect("zero-height render");

        // area.height == TOP_BORDER_HEIGHT — only the border fits; body skipped.
        let mut t3 = Terminal::new(TestBackend::new(8, TOP_BORDER_HEIGHT)).unwrap();
        t3.draw(|frame| {
            stack.render(frame, Rect::new(0, 0, 8, TOP_BORDER_HEIGHT), &theme);
        })
        .expect("border-only render");
        let buf = t3.backend().buffer();
        assert_eq!(buf[(0, 0)].symbol(), TOP_BORDER_GLYPH.to_string());
    }

    // ── handle_key ──

    #[test]
    fn handle_key_consumed_keeps_modal_active() {
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::None)));
        // Non-sentinel keys are consumed; stack stays active.
        assert!(stack.handle_key(&key('x')).is_none());
        assert!(stack.is_active());
    }

    #[test]
    fn handle_key_cancel_pops_and_yields_modal_action_none() {
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::None)));
        // Cancel surfaces `Some(ModalAction::None)` so App can distinguish "closed silently" from
        // "key consumed".
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
    fn handle_key_esc_cancels_universally_without_reaching_modal() {
        // Bypass the modal entirely — the ScriptedModal has no Esc handler, so this would
        // fall through to `Consumed` if Esc were not intercepted at the stack layer.
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::User(
            UserAction::Clear,
        ))));
        let outcome = stack.handle_key(&key_with_mods(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, Some(ModalAction::None)));
        assert!(!stack.is_active(), "Esc must pop the stack");
    }

    #[test]
    fn handle_key_ctrl_c_cancels_universally_like_esc() {
        // Pairs with the Esc test — Ctrl+C is the second universal-cancel gesture.
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::None)));
        let outcome = stack.handle_key(&key_with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(outcome, Some(ModalAction::None)));
        assert!(!stack.is_active(), "Ctrl+C must pop the stack");
    }

    #[test]
    fn handle_key_modifier_less_key_routes_to_modal_unchanged() {
        // `x` because ScriptedModal consumes `c` as its own cancel sentinel.
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::None)));
        let outcome = stack.handle_key(&key_with_mods(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(outcome.is_none(), "non-cancel key must not pop the stack");
        assert!(stack.is_active());
    }

    #[test]
    fn handle_key_with_nested_stack_routes_to_top_modal_only() {
        // Pin: a regression that fans keys to every layer must fail here.
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::User(
            UserAction::Clear,
        ))));
        let mut top = ScriptedModal::new(ModalAction::None);
        top.declared_height = 5;
        stack.push(Box::new(top));

        assert_eq!(
            stack.height(80),
            5 + TOP_BORDER_HEIGHT,
            "top modal's height wins (plus border)",
        );
        let outcome = stack.handle_key(&key('s'));
        assert!(matches!(outcome, Some(ModalAction::None)));
        assert!(stack.is_active(), "inner modal still active");
        assert_eq!(
            stack.height(80),
            3 + TOP_BORDER_HEIGHT,
            "inner modal's height resumes (plus border)",
        );
    }
}
