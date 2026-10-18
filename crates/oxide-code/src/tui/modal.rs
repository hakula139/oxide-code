//! Modal stack that routes keys to the top overlay and renders it between chat and input.
//!
//! Design: `docs/design/slash/modals.md`.

pub(crate) mod approval;
pub(crate) mod chrome;
pub(crate) mod keymap;
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
use crate::tui::glyphs::HORIZONTAL_RULE;
use crate::tui::theme::Theme;

const TOP_BORDER_HEIGHT: u16 = 1;

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
    /// no-op. Pickers override it to refresh state the nested modal changed.
    fn on_focus_regained(&mut self) {}

    /// Fires when the modal leaves the stack without submitting: universal-cancel (Esc / Ctrl+C),
    /// a [`ModalKey::Cancelled`], or [`ModalStack::clear`]. Returns an action to dispatch in place
    /// of the silent [`ModalAction::Cancelled`]. The approval modal overrides this to resolve a
    /// blocked agent to `Deny`. Default no-op.
    fn on_cancel(&mut self) -> Option<ModalAction> {
        None
    }

    /// Returns whether Esc reaches [`Self::handle_key`] in place of the universal cancel, for a
    /// modal that handles Esc itself, either backing out of its current step or dismissing through
    /// its own key table. Ctrl+C still cancels. Default false.
    fn captures_esc(&self) -> bool {
        false
    }
}

// ── Outcomes ──

pub(crate) enum ModalKey {
    Consumed,
    Cancelled,
    Submitted(ModalAction),
    /// Emits `action` without popping. Live-preview modals use this so cursor moves can mutate app
    /// state without committing.
    Preview(ModalAction),
    /// Pushes `modal` onto the stack as a nested overlay. The current modal stays beneath, so a
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
    /// An [`Action`] the App dispatches through the same path as a typed one.
    Dispatch(Action),
    /// A confirmation line the App pushes into chat after the modal pops, so a destructive action
    /// leaves evidence in chat that it ran.
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
    pub(crate) fn is_active(&self) -> bool {
        !self.stack.is_empty()
    }

    pub(crate) fn push(&mut self, modal: Box<dyn Modal>) {
        self.stack.push(modal);
    }

    /// Drops every modal on the stack, firing each modal's [`Modal::on_cancel`] hook first, and
    /// returns the hooks' actions for the caller to dispatch. Session swaps call this so pickers
    /// and nested overlays stay with their session and a pending approval still reaches its agent.
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

    /// Renders the top modal into `area` below a one-row separator. No-op when empty.
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
            HORIZONTAL_RULE.repeat(usize::from(area.width)),
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

    /// Pops the top entry and notifies the new top through [`Modal::on_focus_regained`].
    fn pop_and_notify(&mut self) {
        self.stack.pop();
        self.notify_top();
    }

    /// Pops the top entry for a cancellation, firing its [`Modal::on_cancel`] hook and notifying
    /// the new top. Returns the hook's action, or [`ModalAction::Cancelled`] for a silent close.
    fn pop_cancelled(&mut self) -> ModalAction {
        let action = self.stack.pop().and_then(|mut m| m.on_cancel());
        self.notify_top();
        action.unwrap_or(ModalAction::Cancelled)
    }

    fn notify_top(&mut self) {
        if let Some(top) = self.stack.last_mut() {
            top.on_focus_regained();
        }
    }
}

/// Returns whether the key has no modifier but Shift, which terminals report on uppercase letters.
pub(crate) fn is_plain_char(event: &KeyEvent) -> bool {
    event.modifiers.is_empty() || event.modifiers == KeyModifiers::SHIFT
}

fn is_universal_cancel(event: &KeyEvent) -> bool {
    event.code == KeyCode::Esc
        || (event.code == KeyCode::Char('c') && event.modifiers.contains(KeyModifiers::CONTROL))
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    //! Row readers for rendered modals, and a synthetic modal that exercises the stack without a
    //! concrete picker.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::testing::buffer_rows;

    /// Renders `modal` at `width` and the height it requests.
    pub(crate) fn rendered_text(modal: &dyn Modal, width: u16) -> String {
        let height = modal.height(width, u16::MAX);
        render_rows(modal, width, height).join("\n")
    }

    /// Renders `modal` into a `width` x `height` buffer and returns each row as displayed.
    pub(crate) fn render_rows(modal: &dyn Modal, width: u16, height: u16) -> Vec<String> {
        render_rows_with(width, height, |frame, area, theme| {
            modal.render(frame, area, theme);
        })
    }

    /// [`render_rows`] for any render closure, such as a picker that is not a [`Modal`].
    pub(super) fn render_rows_with(
        width: u16,
        height: u16,
        render: impl FnOnce(&mut Frame<'_>, Rect, &Theme),
    ) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, width, height), &Theme::default()))
            .unwrap();
        buffer_rows(terminal.backend().buffer())
    }

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
        /// Child that `on_push_key` pushes, once. Pressing the key without a child panics.
        pub(crate) push_child: Option<Box<dyn Modal>>,
        /// Whether the modal claims Esc through [`Modal::captures_esc`].
        pub(crate) captures_esc: bool,
        pub(crate) declared_height: u16,
        /// Shared so a test can read the count after the modal moves into the stack.
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
                KeyCode::Char(c) if c == self.on_submit_key => ModalKey::Submitted(
                    std::mem::replace(&mut self.submit_action, ModalAction::Cancelled),
                ),
                KeyCode::Char(c) if c == self.on_cancel_key => ModalKey::Cancelled,
                KeyCode::Char(c) if c == self.on_preview_key => ModalKey::Preview(
                    std::mem::replace(&mut self.preview_action, ModalAction::Cancelled),
                ),
                KeyCode::Char(c) if c == self.on_push_key => {
                    let child = self
                        .push_child
                        .take()
                        .expect("push_child must be set before pressing on_push_key");
                    ModalKey::Push(child)
                }
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

    use super::testing::{ScriptedModal, render_rows_with};
    use super::*;
    use crate::protocol::Op;

    /// Modal that keeps every default hook.
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

    // ── Modal::captures_esc ──

    #[test]
    fn captures_esc_default_leaves_esc_to_close_the_modal_silently() {
        let mut stack = ModalStack::default();
        stack.push(Box::new(BareModal));

        let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Esc));

        assert!(
            matches!(outcome, Some(ModalAction::Cancelled)),
            "{outcome:?}"
        );
        assert!(!stack.is_active());
    }

    // ── ModalKey::fmt ──

    #[test]
    fn fmt_names_each_variant_and_its_action() {
        assert_eq!(format!("{:?}", ModalKey::Consumed), "Consumed");
        assert_eq!(format!("{:?}", ModalKey::Cancelled), "Cancelled");
        assert_eq!(
            format!("{:?}", ModalKey::Submitted(ModalAction::Cancelled)),
            "Submitted(Cancelled)"
        );
        assert_eq!(
            format!("{:?}", ModalKey::Preview(ModalAction::Cancelled)),
            "Preview(Cancelled)"
        );
        let push: ModalKey = ModalKey::Push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        assert_eq!(format!("{push:?}"), "Push(<modal>)");
    }

    // ── is_active ──

    #[test]
    fn is_active_empty_stack_is_inactive_with_zero_height() {
        let stack = ModalStack::default();
        assert!(!stack.is_active());
        assert_eq!(stack.height(80, 40), 0);
    }

    // ── push ──

    #[test]
    fn push_activates_stack_and_height_reflects_top_modal() {
        let mut stack = ModalStack::default();
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        assert!(stack.is_active());
        assert_eq!(stack.height(80, 40), 3 + TOP_BORDER_HEIGHT);
    }

    // ── clear ──

    #[test]
    fn clear_drops_every_modal_on_the_stack() {
        let mut stack = ModalStack::default();
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
        let mut stack = ModalStack::default();
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
        let mut stack = ModalStack::default();
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
        let mut stack = ModalStack::default();
        let modal = ScriptedModal::new(ModalAction::Cancelled);
        let body_height = modal.declared_height;
        stack.push(Box::new(modal));

        let total_height = stack.height(12, 40);
        assert_eq!(total_height, body_height + TOP_BORDER_HEIGHT);

        let rows = render_rows_with(12, total_height, |frame, area, theme| {
            stack.render(frame, area, theme);
        });

        let blank = " ".repeat(12);
        assert_eq!(
            rows,
            [
                HORIZONTAL_RULE.repeat(12),
                blank.clone(),
                blank.clone(),
                blank
            ]
        );
    }

    #[test]
    fn render_no_ops_when_stack_empty_or_area_smaller_than_body() {
        let empty = ModalStack::default();
        let mut stack = ModalStack::default();
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));

        let empty_rows = render_rows_with(8, 2, |frame, area, theme| {
            empty.render(frame, area, theme);
        });
        let zero_height_rows = render_rows_with(8, 1, |frame, area, theme| {
            stack.render(frame, Rect { height: 0, ..area }, theme);
        });
        let border_only_rows = render_rows_with(8, TOP_BORDER_HEIGHT, |frame, area, theme| {
            stack.render(frame, area, theme);
        });

        assert_eq!(empty_rows, [" ".repeat(8), " ".repeat(8)]);
        assert_eq!(zero_height_rows, [" ".repeat(8)]);
        assert_eq!(border_only_rows, [HORIZONTAL_RULE.repeat(8)]);
    }

    // ── handle_key ──

    #[test]
    fn handle_key_cancel_pops_and_yields_modal_action_cancelled() {
        // A silent close surfaces as `Some(Cancelled)` so the App can tell it from a consumed key.
        let mut stack = ModalStack::default();
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));

        let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('c')));

        assert!(matches!(outcome, Some(ModalAction::Cancelled)));
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_cancel_surfaces_the_modals_on_cancel_action() {
        let mut stack = ModalStack::default();
        let mut modal = ScriptedModal::new(ModalAction::Cancelled);
        modal.cancel_action = Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)));
        stack.push(Box::new(modal));

        let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('c')));

        assert!(matches!(
            outcome,
            Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)))
        ));
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_universal_cancel_pops_before_the_modal_sees_the_key() {
        for cancel in [
            KeyEvent::from(KeyCode::Esc),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut stack = ModalStack::default();
            let mut modal = ScriptedModal::new(ModalAction::Cancelled);
            // Moves the cancel sentinel off `c`, so a Ctrl+C reaching the modal would be consumed.
            modal.on_cancel_key = 'x';
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
        let mut stack = ModalStack::default();
        let mut modal = ScriptedModal::new(ModalAction::Cancelled);
        modal.cancel_action = Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)));
        modal.captures_esc = true;
        stack.push(Box::new(modal));

        let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Esc));
        assert!(outcome.is_none());
        assert!(stack.is_active());

        let outcome = stack.handle_key(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(
            outcome,
            Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)))
        ));
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_submit_pops_and_yields_the_dispatched_action() {
        let mut stack = ModalStack::default();
        let action = Action::Op(Op::Interrupt);
        stack.push(Box::new(ScriptedModal::new(ModalAction::Dispatch(
            action.clone(),
        ))));

        let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('s')));

        assert!(
            matches!(outcome, Some(ModalAction::Dispatch(a)) if a == action),
            "submit must surface the modal's Action unchanged",
        );
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_preview_yields_action_without_popping_stack() {
        let mut stack = ModalStack::default();
        let mut modal = ScriptedModal::new(ModalAction::Cancelled);
        modal.preview_action = ModalAction::Dispatch(Action::Op(Op::Interrupt));
        stack.push(Box::new(modal));

        let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('p')));

        assert!(
            matches!(
                outcome,
                Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)))
            ),
            "preview must surface the modal's Action unchanged",
        );
        assert!(stack.is_active(), "preview must not pop the stack");
    }

    #[test]
    fn handle_key_empty_stack_delivers_nothing() {
        let mut stack = ModalStack::default();

        let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('s')));

        assert!(outcome.is_none());
        assert!(!stack.is_active());
    }

    #[test]
    fn handle_key_modifier_less_key_routes_to_modal_unchanged() {
        // `x` because ScriptedModal consumes `c` as its own cancel sentinel.
        let mut stack = ModalStack::default();
        stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));

        let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('x')));

        assert!(outcome.is_none(), "non-cancel key must not pop the stack");
        assert!(stack.is_active());
    }

    #[test]
    fn handle_key_with_nested_stack_routes_to_top_modal_only() {
        let mut stack = ModalStack::default();
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

        let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('s')));

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
        let mut parent = ScriptedModal::new(ModalAction::Cancelled);
        let mut child = ScriptedModal::new(ModalAction::Cancelled);
        child.declared_height = 7;
        parent.push_child = Some(Box::new(child));

        let mut stack = ModalStack::default();
        stack.push(Box::new(parent));
        assert_eq!(stack.height(80, 40), 3 + TOP_BORDER_HEIGHT, "parent height");

        let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('h')));

        assert!(outcome.is_none(), "Push must not surface a ModalAction");
        assert_eq!(
            stack.height(80, 40),
            7 + TOP_BORDER_HEIGHT,
            "child becomes top"
        );
    }

    #[test]
    fn handle_key_pop_notifies_underlying_top_via_on_focus_regained() {
        use std::sync::atomic::Ordering;

        for (label, exit_key) in [
            ("Esc universal-cancel", KeyEvent::from(KeyCode::Esc)),
            (
                "Ctrl+C universal-cancel",
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ),
            ("modal Cancelled", KeyEvent::from(KeyCode::Char('c'))),
            ("modal Submitted", KeyEvent::from(KeyCode::Char('s'))),
        ] {
            let parent = ScriptedModal::new(ModalAction::Cancelled);
            let counter = Arc::clone(&parent.focus_regained_count);
            let child = ScriptedModal::new(ModalAction::Cancelled);

            let mut stack = ModalStack::default();
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
}
