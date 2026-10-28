//! Modal stack that routes keys to the top overlay and renders it between chat and input.

pub(crate) mod acceptance;
pub(crate) mod approval;
pub(crate) mod chrome;
pub(crate) mod kv_overview;
pub(crate) mod line_editor;
pub(crate) mod list_picker;
pub(crate) mod searchable_list;

use std::fmt;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use ox_core::protocol::SavedTarget;
use ox_core::session::saved::{ArchiveUpdate, SavedError};
use ox_core::session::store::listing::ArchiveState;

use crate::action::Action;
use crate::glyphs::HORIZONTAL_RULE;
use crate::keymap::{CTRL_C, KeyChord, is_unmodified};
use crate::theme::Theme;

const TOP_BORDER_HEIGHT: u16 = 1;

// ── Modal Trait ──

/// Focus-grabbing UI overlay. `Send` because App lives on tokio. Not `Sync` because modals own
/// mutable state and are exclusively driven from the App task.
pub(crate) trait Modal: Send {
    /// Rows the modal wants at `width`, before the separator. Scrollable modals fit within
    /// `max_height`, and the stack clips anything taller.
    fn height(&mut self, width: u16, max_height: u16) -> u16;

    /// Updates the current view even when `area` has no cells, invalidating hidden acceptance.
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme);

    /// Routes one key event. Returns whether the modal consumed it, was cancelled, or submitted a
    /// typed action. The stack pops on cancel or submit.
    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey;

    /// Receives bracketed-paste text. Default no-op, so a paste never answers a prompt.
    fn handle_paste(&mut self, _text: &str) {}

    /// Marks a saved-session archive of `target` accepted by the engine.
    fn archive_started(&mut self, _target: &SavedTarget) {}

    /// Marks a delete of the saved session `id` accepted by the engine.
    fn delete_started(&mut self, _id: &str) {}

    /// Receives the result of a saved-session archive of `target` to `state` while this modal
    /// remains on the stack.
    fn archive_updated(
        &mut self,
        _target: &SavedTarget,
        _state: ArchiveState,
        _result: &Result<ArchiveUpdate, SavedError>,
    ) {
    }

    /// Receives the result of deleting saved session `id` while this modal remains on the stack.
    fn saved_deleted(&mut self, _id: &str, _result: &Result<(), SavedError>) -> DeleteReply {
        DeleteReply::Unclaimed
    }

    /// Fires on frame ticks while the modal is on top, for state that changes with time, such as a
    /// notice that expires or rows loading in the background. Returns whether the modal needs a
    /// repaint. An override reports when it next has work through [`Self::next_tick`], since an
    /// idle loop ticks only then. Default no-op.
    fn tick(&mut self, _now: Instant) -> bool {
        false
    }

    /// When [`Self::tick`] next has work, or `None` when only input changes the modal. A time at or
    /// before `now` asks for a tick every frame.
    fn next_tick(&self, _now: Instant) -> Option<Instant> {
        None
    }

    /// Fires when the modal leaves the stack without submitting: universal-cancel (Esc / Ctrl+C), a
    /// [`ModalKey::Cancelled`], or [`ModalStack::clear`]. Returns an action to dispatch in place of
    /// the silent [`ModalAction::Cancelled`]. The approval modal returns `Abort` to stop the
    /// blocked turn. Default no-op.
    fn on_cancel(&mut self) -> Option<ModalAction> {
        None
    }

    /// Returns whether Esc reaches [`Self::handle_key`] in place of the universal cancel, for a
    /// modal that handles Esc itself, either backing out of its current step or dismissing through
    /// its own key table. Ctrl+C still cancels. Default false.
    fn captures_esc(&self) -> bool {
        false
    }

    /// Returns whether `event`, a key held with Ctrl, Alt, or another modifier but Shift, is one of
    /// the modal's bindings, such as a text field's editing keys. The stack drops every other
    /// modified key, so a plain binding such as `j` never fires as Alt+J. Default false.
    fn binds_modified_key(&self, _event: &KeyEvent) -> bool {
        false
    }

    /// Returns whether the modal may send an op that needs an idle engine, so a queued prompt must
    /// not start while it is open. Default false.
    fn sends_idle_only_ops(&self) -> bool {
        false
    }
}

// ── Outcomes ──

pub(crate) enum ModalKey {
    Consumed,
    Cancelled,
    Submitted(ModalAction),
    /// Emits `action` without popping, for a key that changes app state without committing the
    /// modal, such as a theme preview or an archive toggle inside the `/resume` picker.
    Emit(ModalAction),
    /// Pushes `modal` onto the stack as a nested overlay. The current modal stays beneath, so a
    /// picker can open a confirm dialog without losing its own state.
    Push(Box<dyn Modal>),
}

impl fmt::Debug for ModalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Consumed => f.write_str("Consumed"),
            Self::Cancelled => f.write_str("Cancelled"),
            Self::Submitted(action) => f.debug_tuple("Submitted").field(action).finish(),
            Self::Emit(action) => f.debug_tuple("Emit").field(action).finish(),
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

/// How a modal took the result of a saved-session delete.
#[derive(Debug)]
pub(crate) enum DeleteReply {
    /// The modal leaves the result for the App to report.
    Unclaimed,
    /// The modal shows the result and stays open.
    Shown,
    /// The modal closes, replaced by the action that reports the result.
    Closed(ModalAction),
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

    /// Returns whether an open modal may send an op that needs an idle engine.
    pub(crate) fn sends_idle_only_ops(&self) -> bool {
        self.stack.iter().any(|modal| modal.sends_idle_only_ops())
    }

    pub(crate) fn push(&mut self, modal: Box<dyn Modal>) {
        self.stack.push(modal);
    }

    pub(crate) fn archive_started(&mut self, target: &SavedTarget) {
        for modal in &mut self.stack {
            modal.archive_started(target);
        }
    }

    pub(crate) fn delete_started(&mut self, id: &str) {
        for modal in &mut self.stack {
            modal.delete_started(id);
        }
    }

    pub(crate) fn archive_updated(
        &mut self,
        target: &SavedTarget,
        state: ArchiveState,
        result: &Result<ArchiveUpdate, SavedError>,
    ) {
        for modal in &mut self.stack {
            modal.archive_updated(target, state, result);
        }
    }

    /// Passes a delete result to every modal, dropping each one the result closes. Returns the
    /// closed modals' replacement actions in stack order, or `None` when no modal shows the result,
    /// so the caller reports it.
    pub(crate) fn saved_deleted(
        &mut self,
        id: &str,
        result: &Result<(), SavedError>,
    ) -> Option<Vec<ModalAction>> {
        let mut claimed = false;
        let mut actions = Vec::new();
        self.stack
            .retain_mut(|modal| match modal.saved_deleted(id, result) {
                DeleteReply::Unclaimed => true,
                DeleteReply::Shown => {
                    claimed = true;
                    true
                }
                DeleteReply::Closed(action) => {
                    claimed = true;
                    actions.push(action);
                    false
                }
            });
        claimed.then_some(actions)
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
    pub(crate) fn height(&mut self, width: u16, max_height: u16) -> u16 {
        let Some(top) = self.stack.last_mut() else {
            return 0;
        };
        let body_max = max_height.saturating_sub(TOP_BORDER_HEIGHT);
        let body = top.height(width, body_max).min(body_max);
        body.saturating_add(TOP_BORDER_HEIGHT).min(max_height)
    }

    /// Renders the top modal into `area` below a one-row separator. No-op when empty.
    pub(crate) fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let Some(top) = self.stack.last_mut() else {
            return;
        };
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
        let body_area = Rect {
            x: area.x,
            y: area.y.saturating_add(TOP_BORDER_HEIGHT),
            width: area.width,
            height: body_height,
        };
        top.render(frame, body_area, theme);
    }

    /// Routes `event` to the top modal. Returns `None` when the key was consumed, dropped, or the
    /// stack is empty, `Some(ModalAction::Cancelled)` for a silent close, and `Some(action)` for a
    /// Submit or Emit. Esc and Ctrl+C short-circuit before delegation as a universal cancel, so
    /// individual modals don't reimplement the gesture, except that Esc reaches a modal whose
    /// [`Modal::captures_esc`] is set. A modified key the modal does not bind is dropped.
    pub(crate) fn handle_key(&mut self, event: &KeyEvent) -> Option<ModalAction> {
        let top = self.stack.last_mut()?;
        if is_universal_cancel(event) && !(event.code == KeyCode::Esc && top.captures_esc()) {
            return Some(self.pop_cancelled());
        }
        if !is_unmodified(event) && !top.binds_modified_key(event) {
            return None;
        }
        let outcome = top.handle_key(event);
        match outcome {
            ModalKey::Consumed => None,
            ModalKey::Cancelled => Some(self.pop_cancelled()),
            ModalKey::Submitted(action) => {
                self.stack.pop();
                Some(action)
            }
            ModalKey::Emit(action) => Some(action),
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

    /// Ticks the top modal. Returns whether it needs a repaint.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        self.stack.last_mut().is_some_and(|top| top.tick(now))
    }

    /// When the top modal next needs a tick. See [`Modal::next_tick`].
    pub(crate) fn next_tick(&self, now: Instant) -> Option<Instant> {
        self.stack.last()?.next_tick(now)
    }

    /// Pops the top entry for a cancellation, firing its [`Modal::on_cancel`] hook. Returns the
    /// hook's action, or [`ModalAction::Cancelled`] for a silent close.
    fn pop_cancelled(&mut self) -> ModalAction {
        let action = self.stack.pop().and_then(|mut m| m.on_cancel());
        action.unwrap_or(ModalAction::Cancelled)
    }
}

/// Plain Esc or Ctrl+C. A modified Esc is an ordinary modified key, which a modal may bind.
fn is_universal_cancel(event: &KeyEvent) -> bool {
    KeyChord::plain(KeyCode::Esc).matches(event) || CTRL_C.matches(event)
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
    use crate::testing::buffer_rows;

    /// Renders `modal` at `width` and the height it requests.
    pub(crate) fn rendered_text(modal: &mut dyn Modal, width: u16) -> String {
        let height = modal.height(width, u16::MAX);
        render_rows(modal, width, height).join("\n")
    }

    /// Renders `modal` at `width` and the height it requests within `max_height`, returning each
    /// row without its trailing spaces.
    pub(crate) fn render_modal(modal: &mut dyn Modal, width: u16, max_height: u16) -> Vec<String> {
        let height = modal.height(width, max_height);
        render_rows(modal, width, height)
            .into_iter()
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    /// Renders `modal` into a `width` x `height` buffer and returns each row as displayed.
    pub(crate) fn render_rows(modal: &mut dyn Modal, width: u16, height: u16) -> Vec<String> {
        render_rows_with(width, height, |frame, area, theme| {
            modal.render(frame, area, theme);
        })
    }

    /// [`render_rows`] for any render closure, such as a picker that is not a [`Modal`].
    pub(crate) fn render_rows_with(
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
        pub(crate) on_emit_key: char,
        pub(crate) on_push_key: char,
        pub(crate) submit_action: ModalAction,
        pub(crate) emit_action: ModalAction,
        /// Take-once action returned from [`Modal::on_cancel`]. `None` leaves the default (silent
        /// close), matching pickers that don't override the hook.
        pub(crate) cancel_action: Option<ModalAction>,
        /// Child that `on_push_key` pushes, once. Pressing the key without a child panics.
        pub(crate) push_child: Option<Box<dyn Modal>>,
        /// Whether the modal claims Esc through [`Modal::captures_esc`].
        pub(crate) captures_esc: bool,
        /// Whether [`Modal::binds_modified_key`] claims every Ctrl- or Alt-modified key.
        pub(crate) binds_modified_keys: bool,
        pub(crate) declared_height: u16,
        /// Ticks received, shared so a test can read the count after the modal moves into the
        /// stack. Each tick asks for a repaint.
        pub(crate) tick_count: Arc<AtomicU32>,
        /// Deadline returned from [`Modal::next_tick`].
        pub(crate) next_tick: Option<Instant>,
        /// Take-once reply from [`Modal::saved_deleted`], which is otherwise unclaimed.
        pub(crate) delete_reply: Option<DeleteReply>,
        /// The saved-session results received, shared like `tick_count`.
        pub(crate) saved_events: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ScriptedModal {
        pub(crate) fn new(submit_action: ModalAction) -> Self {
            Self {
                on_submit_key: 's',
                on_cancel_key: 'c',
                on_emit_key: 'p',
                on_push_key: 'h',
                submit_action,
                emit_action: ModalAction::Cancelled,
                cancel_action: None,
                push_child: None,
                captures_esc: false,
                binds_modified_keys: false,
                declared_height: 3,
                tick_count: Arc::new(AtomicU32::new(0)),
                next_tick: None,
                delete_reply: None,
                saved_events: Arc::default(),
            }
        }
    }

    impl Modal for ScriptedModal {
        fn archive_started(&mut self, target: &SavedTarget) {
            self.saved_events
                .lock()
                .unwrap()
                .push(format!("started:{target:?}"));
        }

        fn delete_started(&mut self, id: &str) {
            self.saved_events
                .lock()
                .unwrap()
                .push(format!("deleting:{id}"));
        }

        fn archive_updated(
            &mut self,
            target: &SavedTarget,
            state: ArchiveState,
            result: &Result<ArchiveUpdate, SavedError>,
        ) {
            self.saved_events
                .lock()
                .unwrap()
                .push(format!("completed:{target:?}:{state:?}:{result:?}"));
        }

        fn saved_deleted(&mut self, id: &str, result: &Result<(), SavedError>) -> DeleteReply {
            self.saved_events
                .lock()
                .unwrap()
                .push(format!("deleted:{id}:{result:?}"));
            self.delete_reply.take().unwrap_or(DeleteReply::Unclaimed)
        }

        fn height(&mut self, _width: u16, _max_height: u16) -> u16 {
            self.declared_height
        }

        fn render(&mut self, _frame: &mut Frame<'_>, _area: Rect, _theme: &Theme) {}

        fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
            match event.code {
                KeyCode::Char(c) if c == self.on_submit_key => ModalKey::Submitted(
                    std::mem::replace(&mut self.submit_action, ModalAction::Cancelled),
                ),
                KeyCode::Char(c) if c == self.on_cancel_key => ModalKey::Cancelled,
                KeyCode::Char(c) if c == self.on_emit_key => ModalKey::Emit(std::mem::replace(
                    &mut self.emit_action,
                    ModalAction::Cancelled,
                )),
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

        fn tick(&mut self, _now: Instant) -> bool {
            self.tick_count.fetch_add(1, Ordering::Relaxed);
            true
        }

        fn next_tick(&self, _now: Instant) -> Option<Instant> {
            self.next_tick
        }

        fn on_cancel(&mut self) -> Option<ModalAction> {
            self.cancel_action.take()
        }

        fn captures_esc(&self) -> bool {
            self.captures_esc
        }

        fn binds_modified_key(&self, _event: &KeyEvent) -> bool {
            self.binds_modified_keys
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crossterm::event::KeyModifiers;

    use ox_core::protocol::Op;

    use super::testing::{ScriptedModal, render_rows_with};
    use super::*;

    /// Modal that keeps every default hook.
    struct BareModal;

    impl Modal for BareModal {
        fn height(&mut self, _w: u16, _max: u16) -> u16 {
            1
        }

        fn render(&mut self, _f: &mut Frame<'_>, _a: Rect, _t: &Theme) {}

        fn handle_key(&mut self, _e: &KeyEvent) -> ModalKey {
            ModalKey::Consumed
        }
    }

    #[test]
    fn empty_stack_is_inactive_with_zero_height() {
        let mut stack = ModalStack::default();

        assert!(!stack.is_active());
        assert_eq!(stack.height(80, 40), 0);
    }

    mod modal {
        use super::*;

        mod captures_esc {
            use super::*;

            #[test]
            fn default_leaves_esc_to_close_the_modal_silently() {
                let mut stack = ModalStack::default();
                stack.push(Box::new(BareModal));

                let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Esc));

                assert!(
                    matches!(outcome, Some(ModalAction::Cancelled)),
                    "{outcome:?}"
                );
                assert!(!stack.is_active());
            }
        }

        mod binds_modified_key {
            use super::*;

            #[test]
            fn default_binds_no_modified_key() {
                for event in [
                    KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT),
                    KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL),
                ] {
                    assert!(!BareModal.binds_modified_key(&event), "{event:?}");
                }
            }
        }
    }

    mod modal_key {
        use super::*;

        #[test]
        fn debug_names_each_variant_and_its_action() {
            assert_eq!(format!("{:?}", ModalKey::Consumed), "Consumed");
            assert_eq!(format!("{:?}", ModalKey::Cancelled), "Cancelled");
            assert_eq!(
                format!("{:?}", ModalKey::Submitted(ModalAction::Cancelled)),
                "Submitted(Cancelled)"
            );
            assert_eq!(
                format!("{:?}", ModalKey::Emit(ModalAction::Cancelled)),
                "Emit(Cancelled)"
            );
            let push: ModalKey =
                ModalKey::Push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
            assert_eq!(format!("{push:?}"), "Push(<modal>)");
        }
    }

    mod push {
        use super::*;

        #[test]
        fn activates_stack_and_height_reflects_top_modal() {
            let mut stack = ModalStack::default();

            stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));

            assert!(stack.is_active());
            assert_eq!(stack.height(80, 40), 3 + TOP_BORDER_HEIGHT);
        }
    }

    mod delete_started {
        use super::*;

        #[test]
        fn reaches_a_modal_below_the_top() {
            let mut stack = ModalStack::default();
            let below = ScriptedModal::new(ModalAction::Dispatch(Action::Op(Op::Clear)));
            let observed = Arc::clone(&below.saved_events);
            stack.push(Box::new(below));
            stack.push(Box::new(BareModal));

            stack.delete_started("saved");

            assert_eq!(*observed.lock().unwrap(), ["deleting:saved"]);
        }
    }

    mod archive_updated {
        use super::*;

        #[test]
        fn completes_a_covered_picker_without_dismissing_the_overlay() {
            let mut stack = ModalStack::default();
            let picker = ScriptedModal::new(ModalAction::Dispatch(Action::Op(Op::Clear)));
            let observed = Arc::clone(&picker.saved_events);
            stack.push(Box::new(picker));
            stack.push(Box::new(BareModal));
            let target = SavedTarget::Id("saved".to_owned());
            let update = ArchiveUpdate {
                id: "saved".to_owned(),
                changed: true,
            };

            stack.archive_started(&target);
            stack.archive_updated(&target, ArchiveState::Archived, &Ok(update));

            assert_eq!(
                *observed.lock().unwrap(),
                [
                    r#"started:Id("saved")"#,
                    r#"completed:Id("saved"):Archived:Ok(ArchiveUpdate { id: "saved", changed: true })"#,
                ]
            );
            assert!(
                stack
                    .handle_key(&KeyEvent::from(KeyCode::Char('s')))
                    .is_none()
            );
            assert!(matches!(
                stack.handle_key(&KeyEvent::from(KeyCode::Esc)),
                Some(ModalAction::Cancelled)
            ));
        }
    }

    mod saved_deleted {
        use super::*;

        #[test]
        fn closes_only_the_modals_the_result_closes_and_returns_their_actions() {
            let mut stack = ModalStack::default();
            let picker = ScriptedModal::new(ModalAction::Cancelled);
            let observed = Arc::clone(&picker.saved_events);
            let mut confirm = ScriptedModal::new(ModalAction::Cancelled);
            confirm.declared_height = 5;
            confirm.delete_reply = Some(DeleteReply::Closed(ModalAction::SystemMessage(
                "Deleted".to_owned(),
            )));
            stack.push(Box::new(picker));
            stack.push(Box::new(confirm));

            let actions = stack.saved_deleted("saved", &Ok(()));

            assert!(
                matches!(actions.as_deref(), Some([ModalAction::SystemMessage(msg)]) if msg == "Deleted"),
                "{actions:?}"
            );
            assert_eq!(*observed.lock().unwrap(), ["deleted:saved:Ok(())"]);
            assert_eq!(
                stack.height(80, 40),
                3 + TOP_BORDER_HEIGHT,
                "the picker is back on top"
            );
        }

        #[test]
        fn claims_the_result_only_when_a_modal_shows_or_closes_on_it() {
            let cases: [(&str, Option<DeleteReply>, Option<usize>); 3] = [
                ("unclaimed", None, None),
                ("shown", Some(DeleteReply::Shown), Some(0)),
                (
                    "closed",
                    Some(DeleteReply::Closed(ModalAction::Cancelled)),
                    Some(1),
                ),
            ];
            for (name, reply, expected) in cases {
                let mut stack = ModalStack::default();
                let mut modal = ScriptedModal::new(ModalAction::Cancelled);
                modal.delete_reply = reply;
                stack.push(Box::new(modal));

                let actions = stack.saved_deleted("saved", &Ok(()));

                assert_eq!(actions.map(|a| a.len()), expected, "{name}");
                assert_eq!(stack.is_active(), expected != Some(1), "{name}");
            }
        }
    }

    mod clear {
        use super::*;

        #[test]
        fn drops_every_modal_on_the_stack() {
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
        fn returns_each_modals_on_cancel_action() {
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
    }

    mod height {
        use super::*;

        #[test]
        fn clips_the_top_modal_and_separator_to_the_max() {
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
    }

    mod render {
        use super::*;

        #[test]
        fn paints_top_border_then_delegates_body_below_it() {
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
        fn no_ops_when_stack_empty_or_area_smaller_than_body() {
            let mut empty = ModalStack::default();
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
    }

    mod handle_key {
        use super::*;

        #[test]
        fn a_silent_close_pops_and_reports_the_cancel() {
            // A silent close yields `Some(Cancelled)`, so the App can tell it from a consumed key.
            let mut stack = ModalStack::default();
            stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));

            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('c')));

            assert!(matches!(outcome, Some(ModalAction::Cancelled)));
            assert!(!stack.is_active());
        }

        #[test]
        fn cancel_surfaces_the_modals_on_cancel_action() {
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
        fn universal_cancel_pops_before_the_modal_sees_the_key() {
            for cancel in [
                KeyEvent::from(KeyCode::Esc),
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ] {
                let mut stack = ModalStack::default();
                let mut modal = ScriptedModal::new(ModalAction::Cancelled);
                // Moves the cancel sentinel off `c`, so a Ctrl+C the modal saw would be consumed.
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
        fn esc_reaches_a_modal_that_captures_it_while_ctrl_c_still_cancels() {
            let mut stack = ModalStack::default();
            let mut modal = ScriptedModal::new(ModalAction::Cancelled);
            // Moves the cancel sentinel off `c`, so a Ctrl+C the modal saw would be consumed.
            modal.on_cancel_key = 'x';
            modal.binds_modified_keys = true;
            modal.cancel_action = Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)));
            modal.captures_esc = true;
            stack.push(Box::new(modal));

            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Esc));

            assert!(outcome.is_none());
            assert!(stack.is_active());

            let outcome =
                stack.handle_key(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

            assert!(matches!(
                outcome,
                Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)))
            ));
            assert!(!stack.is_active());
        }

        #[test]
        fn drops_modified_keys_the_modal_does_not_bind() {
            let mut stack = ModalStack::default();
            stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));

            for modifiers in [
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
                KeyModifiers::SUPER,
            ] {
                let outcome = stack.handle_key(&KeyEvent::new(KeyCode::Char('s'), modifiers));

                assert!(outcome.is_none(), "{modifiers:?}");
            }
            assert!(
                stack.is_active(),
                "no modified key reached the submit binding"
            );
        }

        #[test]
        fn treats_a_modified_esc_as_an_ordinary_modified_key() {
            let mut stack = ModalStack::default();
            stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));

            let outcome = stack.handle_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::ALT));

            assert!(outcome.is_none());
            assert!(stack.is_active(), "only a plain Esc cancels");
        }

        #[test]
        fn delivers_the_modified_keys_a_modal_binds() {
            let mut stack = ModalStack::default();
            let mut modal = ScriptedModal::new(ModalAction::Dispatch(Action::Op(Op::Interrupt)));
            modal.binds_modified_keys = true;
            stack.push(Box::new(modal));

            let outcome = stack.handle_key(&KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT));

            assert!(matches!(
                outcome,
                Some(ModalAction::Dispatch(Action::Op(Op::Interrupt)))
            ));
        }

        #[test]
        fn submit_pops_and_yields_the_dispatched_action() {
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
        fn preview_yields_action_without_popping_stack() {
            let mut stack = ModalStack::default();
            let mut modal = ScriptedModal::new(ModalAction::Cancelled);
            modal.emit_action = ModalAction::Dispatch(Action::Op(Op::Interrupt));
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
        fn empty_stack_delivers_nothing() {
            let mut stack = ModalStack::default();

            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('s')));

            assert!(outcome.is_none());
            assert!(!stack.is_active());
        }

        #[test]
        fn modifier_less_key_routes_to_modal_unchanged() {
            // `x` because ScriptedModal consumes `c` as its own cancel sentinel.
            let mut stack = ModalStack::default();
            stack.push(Box::new(ScriptedModal::new(ModalAction::Cancelled)));

            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('x')));

            assert!(outcome.is_none(), "non-cancel key must not pop the stack");
            assert!(stack.is_active());
        }

        #[test]
        fn with_nested_stack_routes_to_top_modal_only() {
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
        fn push_nests_child_modal_without_popping_parent() {
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
    }

    mod tick {
        use super::*;

        #[test]
        fn reaches_only_the_top_modal_and_reports_its_repaint() {
            let bottom = ScriptedModal::new(ModalAction::Cancelled);
            let bottom_ticks = Arc::clone(&bottom.tick_count);
            let top = ScriptedModal::new(ModalAction::Cancelled);
            let top_ticks = Arc::clone(&top.tick_count);
            let mut stack = ModalStack::default();
            stack.push(Box::new(bottom));
            stack.push(Box::new(top));

            let repaint = stack.tick(Instant::now());

            assert!(repaint);
            assert_eq!(top_ticks.load(Ordering::Relaxed), 1);
            assert_eq!(bottom_ticks.load(Ordering::Relaxed), 0);
        }

        #[test]
        fn with_no_modal_or_a_default_hook_asks_for_no_repaint() {
            let mut stack = ModalStack::default();

            let empty_repaint = stack.tick(Instant::now());

            assert!(!empty_repaint);

            stack.push(Box::new(BareModal));
            let default_repaint = stack.tick(Instant::now());

            assert!(!default_repaint);
        }
    }

    mod next_tick {
        use super::*;

        #[test]
        fn follows_only_the_top_modal() {
            let now = Instant::now();
            let mut bottom = ScriptedModal::new(ModalAction::Cancelled);
            bottom.next_tick = Some(now + Duration::from_secs(3));
            let mut stack = ModalStack::default();
            stack.push(Box::new(bottom));
            stack.push(Box::new(BareModal));
            assert_eq!(stack.next_tick(now), None);

            stack.handle_key(&KeyEvent::from(KeyCode::Esc));

            assert_eq!(stack.next_tick(now), Some(now + Duration::from_secs(3)));
        }
    }
}
