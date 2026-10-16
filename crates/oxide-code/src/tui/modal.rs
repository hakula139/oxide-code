//! Modal overlay primitive — intercepts keys while active and emits a typed result on submission.
//! Renders between chat scroll and input. [`ModalStack`] is `Vec`-backed so a future overlay
//! (e.g. "confirm leave?") can `push` over an existing picker without ownership churn.
//!
//! Companion design: `docs/design/slash/modals.md`.

pub(crate) mod approval;
pub(crate) mod chrome;
pub(crate) mod kv_overview;
pub(crate) mod line_editor;
pub(crate) mod list_picker;
pub(crate) mod searchable_list;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::action::Action;
use crate::tui::theme::Theme;

const TOP_BORDER_HEIGHT: u16 = 1;
const TOP_BORDER_GLYPH: char = '─';

// ── Modal Trait ──

/// Focus-grabbing UI overlay. `Send` because App lives on tokio. Not `Sync` because modals own
/// mutable state and are exclusively driven from the App task.
pub(crate) trait Modal: Send {
    /// Rows the modal wants at `width`, before the separator. Scrollable modals fit within
    /// `max_height`, and the stack clips anything taller.
    fn height(&self, width: u16, max_height: u16) -> u16;

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme);

    /// Routes one key event. Returns whether the modal consumed it, was cancelled, or submitted
    /// a typed action. The stack pops on cancel or submit.
    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey;

    /// Receives bracketed-paste text. Default no-op, so a paste never answers a prompt.
    fn handle_paste(&mut self, _text: &str) {}

    /// Fires when this modal returns to the top of the stack after a nested modal pops. Default
    /// no-op; pickers override to refresh state mutated by the sub-modal.
    fn on_focus_regained(&mut self) {}

    /// Fires when the modal leaves the stack without submitting: universal-cancel (Esc / Ctrl+C),
    /// a [`ModalKey::Cancelled`], or [`ModalStack::clear`]. Returns an action to dispatch in place
    /// of the silent [`ModalAction::Cancelled`]. The approval modal overrides this to resolve a
    /// blocked agent to `Deny`. Default no-op.
    fn on_cancel(&mut self) -> Option<ModalAction> {
        None
    }

    /// Whether Esc reaches [`Self::handle_key`] in place of the universal cancel, for a modal whose
    /// current step backs out on Esc. Ctrl+C still cancels. Default false.
    fn captures_esc(&self) -> bool {
        false
    }
}

// ── Outcomes ──

pub(crate) enum ModalKey {
    Consumed,
    Cancelled,
    Submitted(ModalAction),
    /// Emit `action` without popping. Live-preview modals use this so cursor moves can mutate app
    /// state without committing.
    Preview(ModalAction),
    /// Push `modal` onto the stack as a nested overlay. The current modal stays beneath, so a
    /// picker can open a confirm dialog without losing its own state.
    Push(Box<dyn Modal>),
}

/// Hand-rolled because `Box<dyn Modal>` can't derive `Debug` without forcing every impl to.
impl std::fmt::Debug for ModalKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Consumed => f.write_str("Consumed"),
            Self::Cancelled => f.write_str("Cancelled"),
            Self::Submitted(action) => f.debug_tuple("Submitted").field(action).finish(),
            Self::Preview(action) => f.debug_tuple("Preview").field(action).finish(),
            Self::Push(_) => f.write_str("Push(<modal>)"),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ModalAction {
    /// The modal closed without an action of its own. The App reverts any live preview.
    Cancelled,
    /// Dispatch an [`Action`] through the same path as a keyboard-typed one.
    Dispatch(Action),
    /// Push a confirmation line into chat after the modal pops. Used by destructive-action
    /// modals so the user has chat-stream evidence the action ran.
    SystemMessage(String),
}

// ── ModalStack ──

/// LIFO stack of modal overlays. Only the top modal renders and receives keys. Nested entries
/// resume in reverse push order on cancel or submit.
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

    /// Push a modal onto the stack.
    pub(crate) fn push(&mut self, modal: Box<dyn Modal>) {
        self.stack.push(modal);
    }

    /// Drop every modal on the stack, firing each modal's [`Modal::on_cancel`] hook first. Called
    /// on session swap so picker and nested overlays don't leak across sessions, and so a pending
    /// approval resolves to a decision instead of stranding the agent. Returns the hooks' actions
    /// for the caller to dispatch.
    pub(crate) fn clear(&mut self) -> Vec<ModalAction> {
        let actions = self
            .stack
            .iter_mut()
            .filter_map(|m| m.on_cancel())
            .collect();
        self.stack.clear();
        actions
    }

    /// Rows above the input: the top modal's body plus a one-row separator, within `max_height`.
    pub(crate) fn height(&self, width: u16, max_height: u16) -> u16 {
        let Some(top) = self.stack.last() else {
            return 0;
        };
        let body_max = max_height.saturating_sub(TOP_BORDER_HEIGHT);
        let body = top.height(width, body_max).min(body_max);
        body.saturating_add(TOP_BORDER_HEIGHT).min(max_height)
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

    /// Routes `event` to the top modal. Returns `None` when the key was consumed or the stack is
    /// empty, `Some(ModalAction::Cancelled)` for a silent close, and `Some(action)` for a Submit or
    /// Preview. Esc and Ctrl+C short-circuit before delegation as a universal cancel, so
    /// individual modals don't reimplement the gesture, except that Esc reaches a modal whose
    /// [`Modal::captures_esc`] is set.
    pub(crate) fn handle_key(&mut self, event: &KeyEvent) -> Option<ModalAction> {
        let top = self.stack.last_mut()?;
        if is_universal_cancel(event) && !(event.code == KeyCode::Esc && top.captures_esc()) {
            return Some(self.pop_cancelled());
        }
        let outcome = top.handle_key(event);
        match outcome {
            ModalKey::Consumed => None,
            ModalKey::Cancelled => Some(self.pop_cancelled()),
            ModalKey::Submitted(action) => {
                self.pop_and_notify();
                Some(action)
            }
            ModalKey::Preview(action) => Some(action),
            ModalKey::Push(modal) => {
                self.stack.push(modal);
                None
            }
        }
    }

    /// Routes bracketed-paste `text` to the top modal.
    pub(crate) fn handle_paste(&mut self, text: &str) {
        if let Some(top) = self.stack.last_mut() {
            top.handle_paste(text);
        }
    }

    /// Pop the top entry and notify the new top via [`Modal::on_focus_regained`]. Pickers
    /// override the hook to refresh after a nested confirm modal mutated shared state.
    fn pop_and_notify(&mut self) {
        self.stack.pop();
        if let Some(top) = self.stack.last_mut() {
            top.on_focus_regained();
        }
    }

    /// Pop the top entry for a cancellation, firing its [`Modal::on_cancel`] hook and notifying the
    /// new top. Returns the hook's action, or [`ModalAction::Cancelled`] for a silent close.
    fn pop_cancelled(&mut self) -> ModalAction {
        let action = self.stack.pop().and_then(|mut m| m.on_cancel());
        if let Some(top) = self.stack.last_mut() {
            top.on_focus_regained();
        }
        action.unwrap_or(ModalAction::Cancelled)
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

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Emits a fixed action on a sentinel key for exercising `ModalStack`.
    pub(crate) struct ScriptedModal {
        pub(crate) on_submit_key: char,
        pub(crate) on_cancel_key: char,
        pub(crate) on_preview_key: char,
        pub(crate) on_push_key: char,
        pub(crate) submit_action: ModalAction,
        pub(crate) preview_action: ModalAction,
        /// Take-once action returned from [`Modal::on_cancel`]. `None` leaves the default (silent
        /// close), matching pickers that don't override the hook.
        pub(crate) cancel_action: Option<ModalAction>,
        /// `Some` to make `on_push_key` emit `ModalKey::Push(<child>)`. Take-once: subsequent
        /// presses fall through to `Consumed` so a single test step can't redouble-push.
        pub(crate) push_child: Option<Box<dyn Modal>>,
        /// Returned from [`Modal::captures_esc`].
        pub(crate) captures_esc: bool,
        pub(crate) declared_height: u16,
        /// Shared so tests can read the counter through a sibling clone — the modal itself lives
        /// inside `Box<dyn Modal>` after pushing onto the stack and isn't directly inspectable.
        pub(crate) focus_regained_count: Arc<AtomicU32>,
    }

    impl ScriptedModal {
        pub(crate) fn new(submit_action: ModalAction) -> Self {
            Self {
                on_submit_key: 's',
                on_cancel_key: 'c',
                on_preview_key: 'p',
                on_push_key: 'h',
                submit_action,
                preview_action: ModalAction::Cancelled,
                cancel_action: None,
                push_child: None,
                captures_esc: false,
                declared_height: 3,
                focus_regained_count: Arc::new(AtomicU32::new(0)),
            }
        }
    }

    impl Modal for ScriptedModal {
        fn height(&self, _width: u16, _max_height: u16) -> u16 {
            self.declared_height
        }

        fn render(&self, _frame: &mut Frame<'_>, _area: Rect, _theme: &Theme) {}

        fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
            use crossterm::event::KeyCode;
            match event.code {
                KeyCode::Char(c) if c == self.on_submit_key => {
                    let mut taken = ModalAction::Cancelled;
                    std::mem::swap(&mut self.submit_action, &mut taken);
                    ModalKey::Submitted(taken)
                }
                KeyCode::Char(c) if c == self.on_cancel_key => ModalKey::Cancelled,
                KeyCode::Char(c) if c == self.on_preview_key => {
                    let mut taken = ModalAction::Cancelled;
                    std::mem::swap(&mut self.preview_action, &mut taken);
                    ModalKey::Preview(taken)
                }
                KeyCode::Char(c) if c == self.on_push_key => match self.push_child.take() {
                    Some(child) => ModalKey::Push(child),
                    None => ModalKey::Consumed,
                },
                _ => ModalKey::Consumed,
            }
        }

        fn on_focus_regained(&mut self) {
            self.focus_regained_count.fetch_add(1, Ordering::Relaxed);
        }

        fn on_cancel(&mut self) -> Option<ModalAction> {
            self.cancel_action.take()
        }

        fn captures_esc(&self) -> bool {
            self.captures_esc
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::testing::ScriptedModal;
    use super::*;
    use crate::protocol::Op;

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
        assert_eq!(stack.height(80, 40), 0);
    }

    // ── push ──

    #[test]
    fn push_activates_stack_and_height_reflects_top_modal() {
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        assert!(stack.is_active());
        assert_eq!(stack.height(80, 40), 3 + TOP_BORDER_HEIGHT);
    }

    // ── clear ──

    #[test]
    fn clear_drops_every_modal_on_the_stack() {
        // Session swap calls clear and expects every nested modal gone, not just the top.
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        assert!(stack.is_active());
        let actions = stack.clear();
        assert!(!stack.is_active(), "clear must empty the stack");
        assert_eq!(stack.height(80, 40), 0);
        assert!(
            actions.is_empty(),
            "modals without an on_cancel override surface no action",
        );
    }

    #[test]
    fn clear_returns_each_modals_on_cancel_action() {
        // Session swap over a pending approval must drain the modal's deny decision so the agent
        // isn't stranded. The hook runs per modal, top-down, before the stack empties.
        let mut stack = ModalStack::new();
        let mut bottom = ScriptedModal::new(ModalAction::Cancelled);
        bottom.cancel_action = Some(ModalAction::Dispatch(Action::Op(Op::Clear)));
        let mut top = ScriptedModal::new(ModalAction::Cancelled);
        top.cancel_action = Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)));
        stack.push(Box::new(bottom));
        stack.push(Box::new(top));

        let actions = stack.clear();
        assert!(!stack.is_active());
        assert!(
            matches!(
                actions.as_slice(),
                [
                    ModalAction::Dispatch(Action::Op(Op::Clear)),
                    ModalAction::Dispatch(Action::Op(Op::Interrupt)),
                ],
            ),
            "clear must surface each modal's cancel action in stack order: {actions:?}",
        );
    }

    // ── height ──

    #[test]
    fn height_clips_the_top_modal_and_separator_to_the_max() {
        let mut stack = ModalStack::new();
        let mut modal = ScriptedModal::new(ModalAction::Cancelled);
        modal.declared_height = 30;
        stack.push(Box::new(modal));

        assert_eq!(
            stack.height(80, 12),
            12,
            "body clipped to fit beside the separator"
        );
        assert_eq!(stack.height(80, 1), 1, "only the separator fits");
        assert_eq!(stack.height(80, 0), 0);
    }

    // ── render ──

    #[test]
    fn render_paints_top_border_then_delegates_body_below_it() {
        let mut stack = ModalStack::new();
        let modal = ScriptedModal::new(ModalAction::Cancelled);
        let body_height = modal.declared_height;
        stack.push(Box::new(modal));

        let theme = Theme::default();
        let width: u16 = 12;
        let total_height = stack.height(width, 40);
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
        let theme = Theme::default();

        let empty = ModalStack::new();
        let mut t1 = Terminal::new(TestBackend::new(8, 2)).unwrap();
        t1.draw(|frame| empty.render(frame, Rect::new(0, 0, 8, 2), &theme))
            .expect("empty render");

        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
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
    fn handle_key_cancel_pops_and_yields_modal_action_cancelled() {
        // A silent close surfaces as `Some(Cancelled)` so the App can tell it from a consumed key.
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        let outcome = stack.handle_key(&key('c'));
        assert!(matches!(outcome, Some(ModalAction::Cancelled)));
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_cancel_surfaces_the_modals_on_cancel_action() {
        // A modal that overrides on_cancel (the approval modal) must have its deny decision
        // surfaced on a ModalKey::Cancelled, not swallowed as a silent close.
        let mut stack = ModalStack::new();
        let mut modal = ScriptedModal::new(ModalAction::Cancelled);
        modal.cancel_action = Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)));
        stack.push(Box::new(modal));
        let outcome = stack.handle_key(&key('c'));
        assert!(matches!(
            outcome,
            Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)))
        ));
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_universal_cancel_surfaces_the_modals_on_cancel_action() {
        // Esc / Ctrl+C bypass handle_key, so the deny decision must come from the on_cancel hook
        // the stack invokes on the outgoing modal.
        for cancel in [
            key_with_mods(KeyCode::Esc, KeyModifiers::NONE),
            key_with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut stack = ModalStack::new();
            let mut modal = ScriptedModal::new(ModalAction::Cancelled);
            modal.cancel_action = Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)));
            stack.push(Box::new(modal));
            let outcome = stack.handle_key(&cancel);
            assert!(
                matches!(
                    outcome,
                    Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)))
                ),
                "{cancel:?} must surface the on_cancel action",
            );
            assert!(!stack.is_active());
        }
    }

    #[test]
    fn handle_key_esc_reaches_a_modal_that_captures_it_while_ctrl_c_still_cancels() {
        let mut stack = ModalStack::new();
        let mut modal = ScriptedModal::new(ModalAction::Cancelled);
        modal.cancel_action = Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)));
        modal.captures_esc = true;
        stack.push(Box::new(modal));

        assert!(
            stack
                .handle_key(&key_with_mods(KeyCode::Esc, KeyModifiers::NONE))
                .is_none()
        );
        assert!(stack.is_active());
        let outcome = stack.handle_key(&key_with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(
            outcome,
            Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)))
        ));
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_submit_pops_and_yields_modal_action_user() {
        let mut stack = ModalStack::new();
        let action = Action::Op(Op::Interrupt);
        stack.push(Box::new(ScriptedModal::new(ModalAction::Dispatch(
            action.clone(),
        ))));
        let outcome = stack.handle_key(&key('s'));
        assert!(
            matches!(outcome, Some(ModalAction::Dispatch(a)) if a == action),
            "submit must surface the modal's Action unchanged",
        );
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_preview_yields_action_without_popping_stack() {
        // Live-preview modals (theme picker) emit a `User` action on cursor moves so the App can
        // repaint, but the modal must stay on screen until Enter or Esc.
        let mut stack = ModalStack::new();
        let mut modal = ScriptedModal::new(ModalAction::Cancelled);
        modal.preview_action = ModalAction::Dispatch(Action::Op(Op::Interrupt));
        stack.push(Box::new(modal));

        let outcome = stack.handle_key(&key('p'));
        assert!(
            matches!(
                outcome,
                Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)))
            ),
            "preview must surface the modal's Action unchanged",
        );
        assert!(stack.is_active(), "preview must NOT pop the stack");
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
        stack.push(Box::new(ScriptedModal::new(ModalAction::Dispatch(
            Action::Op(Op::Clear),
        ))));
        let outcome = stack.handle_key(&key_with_mods(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, Some(ModalAction::Cancelled)));
        assert!(!stack.is_active(), "Esc must pop the stack");
    }

    #[test]
    fn handle_key_ctrl_c_cancels_universally_like_esc() {
        // Pairs with the Esc test — Ctrl+C is the second universal-cancel gesture.
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        let outcome = stack.handle_key(&key_with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(outcome, Some(ModalAction::Cancelled)));
        assert!(!stack.is_active(), "Ctrl+C must pop the stack");
    }

    #[test]
    fn handle_key_modifier_less_key_routes_to_modal_unchanged() {
        // `x` because ScriptedModal consumes `c` as its own cancel sentinel.
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        let outcome = stack.handle_key(&key_with_mods(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(outcome.is_none(), "non-cancel key must not pop the stack");
        assert!(stack.is_active());
    }

    #[test]
    fn handle_key_with_nested_stack_routes_to_top_modal_only() {
        // Pin: a regression that fans keys to every layer must fail here.
        let mut stack = ModalStack::new();
        stack.push(Box::new(ScriptedModal::new(ModalAction::Dispatch(
            Action::Op(Op::Clear),
        ))));
        let mut top = ScriptedModal::new(ModalAction::Cancelled);
        top.declared_height = 5;
        stack.push(Box::new(top));

        assert_eq!(
            stack.height(80, 40),
            5 + TOP_BORDER_HEIGHT,
            "top modal's height wins (plus border)",
        );
        let outcome = stack.handle_key(&key('s'));
        assert!(matches!(outcome, Some(ModalAction::Cancelled)));
        assert!(stack.is_active(), "inner modal still active");
        assert_eq!(
            stack.height(80, 40),
            3 + TOP_BORDER_HEIGHT,
            "inner modal's height resumes (plus border)",
        );
    }

    #[test]
    fn handle_key_push_nests_child_modal_without_popping_parent() {
        // ModalKey::Push must push the child as a new top while the parent stays beneath. Height
        // flips to the child's; key routing flips to the child too.
        let mut parent = ScriptedModal::new(ModalAction::Cancelled);
        let mut child = ScriptedModal::new(ModalAction::Cancelled);
        child.declared_height = 7;
        parent.push_child = Some(Box::new(child));

        let mut stack = ModalStack::new();
        stack.push(Box::new(parent));
        assert_eq!(stack.height(80, 40), 3 + TOP_BORDER_HEIGHT, "parent height");

        let outcome = stack.handle_key(&key('h'));
        assert!(outcome.is_none(), "Push must not surface a ModalAction");
        assert_eq!(
            stack.height(80, 40),
            7 + TOP_BORDER_HEIGHT,
            "child becomes top"
        );
    }

    #[test]
    fn pop_notifies_underlying_top_via_on_focus_regained() {
        // After a nested modal pops (cancel / submit / universal-cancel), the parent regains
        // focus and gets exactly one on_focus_regained call so it can refresh stale data.
        use std::sync::atomic::Ordering;

        for (label, exit_key) in [
            (
                "Esc universal-cancel",
                key_with_mods(KeyCode::Esc, KeyModifiers::NONE),
            ),
            (
                "Ctrl+C universal-cancel",
                key_with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ),
            ("modal Cancelled", key('c')),
            ("modal Submitted", key('s')),
        ] {
            let parent = ScriptedModal::new(ModalAction::Cancelled);
            let counter = Arc::clone(&parent.focus_regained_count);
            let child = ScriptedModal::new(ModalAction::Cancelled);

            let mut stack = ModalStack::new();
            stack.push(Box::new(parent));
            stack.push(Box::new(child));
            assert_eq!(
                counter.load(Ordering::Relaxed),
                0,
                "{label}: pre-push baseline"
            );

            stack.handle_key(&exit_key);
            assert_eq!(
                counter.load(Ordering::Relaxed),
                1,
                "{label}: parent must be notified exactly once after child pops",
            );
            assert!(stack.is_active(), "{label}: parent stays on the stack");
        }
    }

    #[test]
    fn empty_stack_pop_and_notify_is_a_noop() {
        // No active modal → pop on universal-cancel is short-circuited at the empty-stack guard,
        // so the helper is never reached. Cover the helper's "no top to notify" branch directly.
        let mut stack = ModalStack::new();
        stack.pop_and_notify();
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_push_take_once_falls_through_to_consumed_after_first_push() {
        // ScriptedModal's push_child is take-once. A second 'h' press after the child has been
        // pushed and popped must not double-push.
        let mut parent = ScriptedModal::new(ModalAction::Cancelled);
        let child = ScriptedModal::new(ModalAction::Cancelled);
        parent.push_child = Some(Box::new(child));

        let mut stack = ModalStack::new();
        stack.push(Box::new(parent));
        stack.handle_key(&key('h'));
        stack.handle_key(&key('c')); // cancel the child, parent regains focus.
        assert_eq!(
            stack.height(80, 40),
            3 + TOP_BORDER_HEIGHT,
            "back to parent"
        );

        // Second 'h' press: push_child is now None, so the parent consumes silently rather than
        // pushing a second nested modal.
        let outcome = stack.handle_key(&key('h'));
        assert!(
            outcome.is_none(),
            "second push attempt is consumed: got {outcome:?}"
        );
        assert_eq!(
            stack.height(80, 40),
            3 + TOP_BORDER_HEIGHT,
            "no nested modal"
        );
    }

    // ── Debug for ModalKey ──

    #[test]
    fn debug_format_distinguishes_each_variant() {
        // The Debug impl is hand-rolled because Box<dyn Modal> can't derive it. Pin each arm
        // directly so test panic messages remain useful even if no test actually fires them.
        assert_eq!(format!("{:?}", ModalKey::Consumed), "Consumed");
        assert_eq!(format!("{:?}", ModalKey::Cancelled), "Cancelled");
        assert!(
            format!("{:?}", ModalKey::Submitted(ModalAction::Cancelled)).starts_with("Submitted(")
        );
        assert!(format!("{:?}", ModalKey::Preview(ModalAction::Cancelled)).starts_with("Preview("));
        let push: ModalKey = ModalKey::Push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        assert_eq!(format!("{push:?}"), "Push(<modal>)");
    }

    // ── Modal::on_focus_regained default ──

    /// Modal that doesn't override `on_focus_regained`, so the default empty body fires on pop.
    struct BareModal;

    impl Modal for BareModal {
        fn height(&self, _w: u16, _max: u16) -> u16 {
            1
        }
        fn render(&self, _f: &mut Frame<'_>, _a: Rect, _t: &Theme) {}
        fn handle_key(&mut self, _e: &KeyEvent) -> ModalKey {
            ModalKey::Consumed
        }
    }

    #[test]
    fn default_on_focus_regained_is_a_noop_and_does_not_panic() {
        // Pin that the trait's default empty body is safe to invoke. ScriptedModal overrides the
        // hook for assertion purposes, so the default arm needs separate coverage.
        let mut stack = ModalStack::new();
        stack.push(Box::new(BareModal));
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        stack.handle_key(&key('c'));
        assert_eq!(
            stack.height(80, 40),
            1 + TOP_BORDER_HEIGHT,
            "BareModal is back on top after its child cancels"
        );
    }
}
