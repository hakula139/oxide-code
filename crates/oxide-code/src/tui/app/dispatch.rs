//! Terminal input and the actions it produces: modal and Esc routing, prompt submission, slash
//! commands, and the ops sent to the engine.

use std::time::Instant;

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use ratatui::layout::Position;
use tokio::sync::mpsc;

use super::{App, EXIT_WINDOW};
use crate::protocol::Op;
use crate::slash::{self, Parsed, SlashContext, SlashKind, SlashOutcome};
use crate::tui::action::Action;
use crate::tui::components::input::InputOutcome;
use crate::tui::components::status::Status;
use crate::tui::modal::ModalAction;
use crate::tui::theme::{self, Theme};
use crate::tui::viewport::wrap_next;

impl App {
    pub(super) fn handle_crossterm_event(&mut self, event: &CrosstermEvent) {
        // Windows also reports key releases, which would otherwise act as a second press.
        if let CrosstermEvent::Key(key) = event
            && key.kind == KeyEventKind::Release
        {
            return;
        }
        if let CrosstermEvent::Key(key) = event
            && self.release_selection(key)
        {
            return;
        }
        // The top overlay owns keyboard input, pastes, and the wheel, which steps its selection or
        // scroll like Up / Down. Clicks and drags still reach chat and the status bar, and resize
        // events still reach chat.
        if self.modals.is_active() {
            match event {
                CrosstermEvent::Key(key) => {
                    self.handle_modal_key(key);
                    return;
                }
                CrosstermEvent::Mouse(mouse) => {
                    if let Some(key) = wheel_arrow(mouse.kind) {
                        self.handle_modal_key(&key);
                        return;
                    }
                }
                CrosstermEvent::Paste(text) => {
                    self.modals.handle_paste(text);
                    self.dirty = true;
                    return;
                }
                _ => {}
            }
        }
        match event {
            CrosstermEvent::Key(KeyEvent {
                code: KeyCode::Esc, ..
            }) => {
                if self.input.popup_visible() {
                    _ = self.input.handle_event(event);
                } else {
                    self.handle_esc();
                }
            }
            CrosstermEvent::Key(..) | CrosstermEvent::Paste(..) => {
                match self.input.handle_event(event) {
                    InputOutcome::Action(action) => self.dispatch_action(action),
                    InputOutcome::Consumed => {}
                    InputOutcome::Ignored => self.chat.handle_event(event),
                }
            }
            CrosstermEvent::Mouse(mouse) => {
                if let Some(key) = wheel_arrow(mouse.kind) {
                    if self.input.popup_visible() {
                        _ = self.input.handle_event(&CrosstermEvent::Key(key));
                    } else {
                        self.chat.handle_event(event);
                    }
                } else if !self.open_status_link(*mouse) {
                    if !matches!(
                        mouse.kind,
                        MouseEventKind::Down(MouseButton::Left)
                            | MouseEventKind::Drag(MouseButton::Left)
                            | MouseEventKind::Up(MouseButton::Left)
                    ) {
                        return;
                    }
                    if let Some(text) = self.chat.handle_mouse(*mouse, Instant::now()) {
                        self.clipboard.copy(text);
                    }
                }
            }
            CrosstermEvent::Resize(..) => {}
            _ => return,
        }
        self.dirty = true;
    }

    /// Opens the status-bar link under a plain left press, returning whether it hit one. A press
    /// with modifiers stays with terminals that open OSC 8 links on Ctrl / Cmd-click themselves.
    fn open_status_link(&mut self, mouse: MouseEvent) -> bool {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) || !mouse.modifiers.is_empty() {
            return false;
        }
        let position = Position::new(mouse.column, mouse.row);
        let Some(link) = self
            .status_links
            .iter()
            .find(|link| link.rect.contains(position))
        else {
            return false;
        };
        if let Err(e) = (self.open_browser)(&link.url) {
            tracing::debug!("failed to open the browser: {e}");
            let message = format!(
                "Could not open the browser. Open {} in a browser.",
                link.url
            );
            self.chat.push_system_message(message);
        }
        true
    }

    fn handle_modal_key(&mut self, key: &KeyEvent) {
        if let Some(action) = self.modals.handle_key(key) {
            self.apply_modal_action(action);
        }
        self.dirty = true;
    }

    /// Clears the chat selection on a keystroke, copying it first on Ctrl+C. Returns whether the
    /// key was spent, which only a Ctrl+C that copied is, so it neither interrupts nor exits.
    fn release_selection(&mut self, key: &KeyEvent) -> bool {
        if !self.chat.has_selection() {
            return false;
        }
        self.dirty = true;
        let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL;
        let Some(text) = self.chat.take_selection().filter(|_| ctrl_c) else {
            return false;
        };
        self.clipboard.copy(text);
        true
    }

    fn apply_modal_action(&mut self, action: ModalAction) {
        match action {
            ModalAction::Cancelled => self.cancel_theme_preview(),
            ModalAction::Dispatch(action) => self.dispatch_action(action),
            ModalAction::SystemMessage(msg) => self.chat.push_system_message(msg),
        }
    }

    fn cancel_theme_preview(&mut self) {
        if let Some(theme) = self.preview_theme_snapshot.take() {
            self.apply_theme(&theme);
        }
    }

    pub(super) fn clear_modals(&mut self) {
        self.cancel_theme_preview();
        for action in self.modals.clear() {
            self.apply_modal_action(action);
        }
    }

    /// Repaints every theme-styled component for a mid-session theme swap.
    fn apply_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
        self.chat.set_theme(theme);
        self.status_bar.set_theme(theme);
        self.input.set_theme(theme);
        self.dirty = true;
    }

    /// Cancels if busy, restores one queued prompt if idle, or no-ops.
    pub(super) fn handle_esc(&mut self) {
        if !self.input.is_enabled() {
            self.dispatch_op(Op::Interrupt);
        } else if self.input.is_empty()
            && let Some(prompt) = self.pending_prompts.pop_back()
        {
            self.input.set_text(&prompt);
        }
    }

    /// Routes an input, slash-command, or modal action. TUI-only actions apply here, and an op goes
    /// through [`Self::dispatch_op`].
    pub(super) fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::Submit(text) => self.handle_submit_prompt(&text),
            Action::Op(op) => self.dispatch_op(op),
            Action::ConfirmExit => self.confirm_exit(),
            Action::CycleMode => self.cycle_mode(),
            Action::PreviewTheme { name } => self.preview_theme(&name),
            Action::SwapTheme { name } => self.swap_theme(&name),
        }
    }

    /// Applies the op's local state change, then sends it to the engine. A prompt starts a turn.
    pub(super) fn dispatch_op(&mut self, op: Op) {
        match &op {
            Op::Prompt(_) => return self.start_turn(op),
            Op::Interrupt => self.status_bar.set_status(Status::Cancelling),
            Op::Shutdown => self.should_quit = true,
            Op::Resume { .. } => {
                self.input.set_enabled(false);
                self.status_bar.set_status(Status::Resuming);
            }
            Op::Compact { .. } => {
                self.input.set_enabled(false);
                self.status_bar.set_status(Status::Compacting);
            }
            Op::Login => {
                self.input.set_enabled(false);
                self.status_bar.set_status(Status::SigningIn);
            }
            Op::SetMode(mode) => self.requested_mode = *mode,
            Op::Queue(_)
            | Op::Resolve { .. }
            | Op::SetModel { .. }
            | Op::Clear
            | Op::Archive
            | Op::Rename { .. }
            | Op::Logout => {}
        }
        self.send_op(op);
    }

    /// Sends `op` to the engine. Channel errors surface as chat error blocks.
    fn send_op(&mut self, op: Op) {
        if let Err(e) = self.ops.try_send(op) {
            self.surface_send_error(&e);
        }
    }

    fn surface_send_error(&mut self, e: &mpsc::error::TrySendError<Op>) {
        match e {
            mpsc::error::TrySendError::Closed(_) => {
                self.chat
                    .push_error("The engine stopped unexpectedly. Restart `ox` to recover.");
                self.input.set_enabled(false);
                self.should_quit = true;
            }
            // An approval decision is a control-plane reply the engine is waiting for, so a
            // dropped one leaves the turn blocked until the user cancels it.
            mpsc::error::TrySendError::Full(Op::Resolve { .. }) => {
                self.chat.push_error(
                    "Approval could not be delivered. Press Esc to cancel the blocked turn.",
                );
            }
            mpsc::error::TrySendError::Full(_) => {
                self.chat
                    .push_error("The engine is busy, so the request was dropped. Try again.");
            }
        }
    }

    /// Quits on a second Ctrl+C inside the exit window, or arms the window.
    fn confirm_exit(&mut self) {
        if let Status::ExitArmed { until } = self.status_bar.status()
            && Instant::now() < *until
        {
            self.should_quit = true;
        } else {
            let until = Instant::now() + EXIT_WINDOW;
            self.status_bar.set_status(Status::ExitArmed { until });
        }
    }

    /// Switches to the mode after the last requested one in the Shift+Tab cycle, or to the cycle's
    /// first mode from one outside it.
    fn cycle_mode(&mut self) {
        let next = self
            .mode_cycle
            .iter()
            .position(|&mode| mode == self.requested_mode)
            .map_or(self.mode_cycle[0], |index| {
                self.mode_cycle[wrap_next(index, self.mode_cycle.len())]
            });
        self.dispatch_op(Op::SetMode(next));
    }

    /// Clears an [`Status::ExitArmed`] window that ended by `now`. Returns whether it cleared.
    pub(super) fn expire_armed_exit(&mut self, now: Instant) -> bool {
        if let Status::ExitArmed { until } = self.status_bar.status()
            && now >= *until
        {
            self.status_bar.set_status(Status::Idle);
            return true;
        }
        false
    }

    fn preview_theme(&mut self, name: &str) {
        if let Some(preview) = theme::load_builtin(name) {
            if self.preview_theme_snapshot.is_none() {
                self.preview_theme_snapshot = Some(self.theme.clone());
            }
            self.apply_theme(&preview);
        } else {
            tracing::warn!(
                name,
                "PreviewTheme: unknown built-in, so the picker roster drifted"
            );
        }
    }

    fn swap_theme(&mut self, name: &str) {
        self.preview_theme_snapshot = None;
        if let Some(theme) = theme::load_builtin(name) {
            name.clone_into(&mut self.session_info.config.theme_name);
            self.apply_theme(&theme);
            self.chat
                .push_system_message(format!("Theme set to {name}"));
        } else {
            tracing::warn!(
                name,
                "SwapTheme: unknown built-in, so the picker roster drifted"
            );
        }
    }

    /// Queues the prompt, then starts a turn with the queue head when idle. Slash commands forward
    /// their own synthesized actions.
    fn handle_submit_prompt(&mut self, text: &str) {
        let idle = self.input.is_enabled();
        let Some(parsed) = slash::parse_slash(text) else {
            self.pending_prompts.push_back(text.to_owned());
            if idle {
                // Prompts a cancel left queued run before this one.
                self.drain_pending_prompt();
            } else {
                self.forward_queued_prompts();
            }
            return;
        };
        let echoes = slash::echoes_input(&parsed);
        if echoes {
            self.chat.push_user_message(text.to_owned());
        }
        if idle {
            if let Some(action) = self.run_slash(&parsed) {
                if echoes && matches!(action, Action::Op(Op::Prompt(_))) {
                    self.active_prompt = Some(text.to_owned());
                }
                self.dispatch_action(action);
            }
            return;
        }
        match slash::classify(&parsed) {
            SlashKind::ReadOnly | SlashKind::Unknown => _ = self.run_slash(&parsed),
            SlashKind::Steering => {
                if let Some(action) = self.run_slash(&parsed) {
                    self.dispatch_action(action);
                }
            }
            SlashKind::Mutating => {
                self.chat.push_system_message(format!(
                    "/{} runs only when idle. Try again after the turn finishes.",
                    parsed.name,
                ));
            }
        }
    }

    /// Starts a turn, then hands it the queued prompts so they splice in at its round boundaries.
    pub(super) fn start_turn(&mut self, prompt: Op) {
        self.input.set_enabled(false);
        self.status_bar.set_status(Status::Streaming);
        self.send_op(prompt);
        self.forward_queued_prompts();
    }

    /// Forwards the queued prompts the in-flight turn lacks, in order. A prompt that a full channel
    /// turns away stays queued for a later call, and a closed channel surfaces one error. Holds the
    /// prompts while compacting, signing in, resuming, or cancelling, when no turn may be left to
    /// splice them in.
    pub(super) fn forward_queued_prompts(&mut self) {
        if matches!(
            self.status_bar.status(),
            Status::Compacting | Status::SigningIn | Status::Resuming | Status::Cancelling,
        ) {
            return;
        }
        while let Some(queued) = self.pending_prompts.get(self.forwarded_prompts) {
            let prompt = slash::unescape_literal(queued).to_owned();
            match self.ops.try_send(Op::Queue(prompt)) {
                Ok(()) => self.forwarded_prompts += 1,
                Err(mpsc::error::TrySendError::Full(_)) => break,
                Err(e) => {
                    self.surface_send_error(&e);
                    break;
                }
            }
        }
    }

    /// Runs `parsed` and applies its outcome, returning the action it asks the caller to route.
    fn run_slash(&mut self, parsed: &Parsed) -> Option<Action> {
        let ctx = SlashContext::new(&self.session_info, self.status_bar.title());
        let outcome = slash::dispatch(parsed, &ctx);
        self.apply_slash_outcome(outcome)
    }

    /// Shows a slash command's chat output or modal, returning a forwarded action unapplied.
    fn apply_slash_outcome(&mut self, outcome: Result<SlashOutcome, String>) -> Option<Action> {
        match outcome {
            Ok(SlashOutcome::Forward(action)) => return Some(action),
            Ok(SlashOutcome::Modal(modal)) => self.modals.push(modal),
            Ok(SlashOutcome::SystemMessage(text)) => self.chat.push_system_message(text),
            Ok(SlashOutcome::GitDiff(diff)) => self.chat.push_git_diff(diff),
            Err(msg) => self.chat.push_error(&msg),
        }
        None
    }
}

/// The arrow key a wheel notch stands in for where a list owns the selection.
fn wheel_arrow(kind: MouseEventKind) -> Option<KeyEvent> {
    match kind {
        MouseEventKind::ScrollUp => Some(KeyEvent::from(KeyCode::Up)),
        MouseEventKind::ScrollDown => Some(KeyEvent::from(KeyCode::Down)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use base64::Engine as _;
    use indoc::indoc;
    use ratatui::layout::Rect;
    use tokio::sync::mpsc::error::TryRecvError;

    use super::*;
    use crate::config::Effort;
    use crate::permission::defaults::Shipped;
    use crate::permission::{Basis, Mode};
    use crate::protocol::{Decision, Event, ResumeTarget};
    use crate::tool::{ApprovalBody, ApprovalPreview};
    use crate::tui::app::testing::{
        FakeModal, KeyLogModal, OP_CHANNEL_CAPACITY, key_event, long_chat_block, mouse_event,
        mouse_event_at, push_answerable_approval, render_app, rendered_text, test_app,
        test_app_with, test_session_info,
    };
    use crate::tui::components::status::StatusHyperlink;
    use crate::tui::modal::testing::ScriptedModal;

    // ── Fixtures ──

    /// Screen cell where `needle` first shows in a 60x10 frame.
    fn cell_of(app: &mut App, needle: &str) -> (u16, u16) {
        let text = rendered_text(app, 60, 10);
        let (row, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(needle))
            .unwrap();
        let x = line[..line.find(needle).unwrap()].chars().count();
        (u16::try_from(x).unwrap(), u16::try_from(row).unwrap())
    }

    /// Double-clicks the word `needle`, which selects and copies it.
    async fn select_word(app: &mut App, needle: &str) {
        let (x, y) = cell_of(app, needle);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ]
        .repeat(2)
        {
            app.handle_crossterm_event(&mouse_event_at(kind, x, y));
        }
        app.clipboard.recv().await.expect("the release copied");
    }

    // ── handle_crossterm_event ──

    #[tokio::test]
    async fn handle_crossterm_key_submit_forwards_through_input_to_dispatch() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        // Type "hi" then Enter. Input returns `Action::Submit`, which must reach `dispatch_action`.
        app.handle_crossterm_event(&key_event(KeyCode::Char('h'), KeyModifiers::NONE));
        app.handle_crossterm_event(&key_event(KeyCode::Char('i'), KeyModifiers::NONE));
        app.handle_crossterm_event(&key_event(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.dirty);
        assert_eq!(app.chat.entry_count(), 1);
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        let forwarded = rx.recv().await.unwrap();
        assert!(matches!(forwarded, Op::Prompt(s) if s == "hi"));
    }

    #[test]
    fn handle_crossterm_key_ctrl_c_idle_arms_exit_then_confirms() {
        // The first press arms, and a second press inside the window confirms.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.should_quit, "first press only arms");
        assert!(matches!(app.status_bar.status(), Status::ExitArmed { .. }));

        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit, "second press within window confirms");
    }

    #[tokio::test]
    async fn handle_crossterm_key_ctrl_c_busy_forwards_interrupt_without_quitting() {
        // Mid-turn Ctrl+C forwards `Interrupt` to the engine and leaves `should_quit` clear.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::StreamToken("partial".to_owned()));
        assert!(!app.input.is_enabled());

        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.should_quit, "busy Ctrl+C must not exit");
        let forwarded = rx.recv().await.expect("Interrupt forwarded to the engine");
        assert!(matches!(forwarded, Op::Interrupt));
    }

    #[tokio::test]
    async fn handle_crossterm_key_esc_busy_forwards_interrupt() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::StreamToken("partial".to_owned()));

        app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.should_quit);
        let forwarded = rx.recv().await.expect("Interrupt forwarded to the engine");
        assert!(matches!(forwarded, Op::Interrupt));
    }

    #[test]
    fn handle_crossterm_event_wheel_scroll_is_forwarded_to_chat() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 60, 10);

        app.handle_crossterm_event(&mouse_event(MouseEventKind::ScrollUp));
        let text = rendered_text(&mut app, 60, 10);

        assert!(text.contains("Jump to bottom"), "{text}");
        assert!(app.dirty);
    }

    #[test]
    fn handle_crossterm_event_plain_click_opens_the_status_link_under_it() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.status_links = vec![StatusHyperlink {
            rect: Rect::new(10, 0, 3, 1),
            url: "https://example.com/pull/7".to_owned(),
            cells: Vec::new(),
        }];
        let press = |column, modifiers| {
            CrosstermEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row: 0,
                modifiers,
            })
        };

        app.open_browser = |_| panic!("only a plain click on the link opens it");
        app.handle_crossterm_event(&press(9, KeyModifiers::NONE));
        app.handle_crossterm_event(&press(10, KeyModifiers::CONTROL));
        app.handle_crossterm_event(&press(12, KeyModifiers::SUPER));
        app.open_browser = |_| Ok(());
        app.handle_crossterm_event(&press(10, KeyModifiers::NONE));
        assert_eq!(
            app.chat.last_system_text(),
            None,
            "an opened link needs no note"
        );
        app.open_browser = |url| {
            assert_eq!(url, "https://example.com/pull/7");
            Err(io::Error::other("no browser"))
        };
        app.handle_crossterm_event(&press(12, KeyModifiers::NONE));

        assert_eq!(
            app.chat.last_system_text(),
            Some("Could not open the browser. Open https://example.com/pull/7 in a browser."),
        );
    }

    #[test]
    fn handle_crossterm_event_wheel_steps_the_slash_popup_like_arrows() {
        let open_popup = |app: &mut App| {
            app.chat.push_system_message(long_chat_block());
            _ = render_app(app, 80, 20);
            app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
            assert!(app.input.popup_visible());
        };
        let (mut by_key, _rx, _event_tx) = test_app(None);
        let (mut by_wheel, _rx2, _event_tx2) = test_app(None);
        open_popup(&mut by_key);
        open_popup(&mut by_wheel);

        by_key.handle_crossterm_event(&key_event(KeyCode::Down, KeyModifiers::NONE));
        by_wheel.handle_crossterm_event(&mouse_event(MouseEventKind::ScrollDown));
        for app in [&mut by_key, &mut by_wheel] {
            app.handle_crossterm_event(&key_event(KeyCode::Tab, KeyModifiers::NONE));
        }

        assert_eq!(by_wheel.input.lines(), by_key.input.lines());
        assert_ne!(
            by_wheel.input.lines(),
            vec!["/archive ".to_owned()],
            "moved off the first"
        );
        assert!(!by_wheel.chat.is_scrolled_up(), "the popup took the wheel");
    }

    #[tokio::test]
    async fn handle_crossterm_event_drag_release_copies_the_selection() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message("copy this word");
        let (x, y) = cell_of(&mut app, "this");

        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), x),
            (MouseEventKind::Drag(MouseButton::Left), x + 3),
            (MouseEventKind::Up(MouseButton::Left), x + 3),
        ] {
            app.handle_crossterm_event(&mouse_event_at(kind, column, y));
        }
        let report = app.clipboard.recv().await.expect("the copy reports back");
        app.apply_copy_report(report);

        let encoded = base64::engine::general_purpose::STANDARD.encode("this");
        assert_eq!(
            app.pending_clipboard,
            Some(format!("\x1b]52;c;{encoded}\x07")),
        );
        let notice = app.copy_notice.as_ref().expect("copy noted");
        assert_eq!(
            (notice.text.as_str(), notice.failed),
            ("Copied 4 characters", false)
        );
        assert!(app.dirty);
    }

    #[tokio::test]
    async fn handle_crossterm_event_ctrl_c_copies_the_selection_without_interrupting() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.chat.push_system_message("copy this word");
        app.handle_engine_event(Event::StreamToken("partial".to_owned()));
        select_word(&mut app, "this").await;

        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let report = app.clipboard.recv().await.expect("Ctrl+C copied");

        assert_eq!(report.chars, 4);
        assert!(!app.chat.has_selection(), "the copy clears the selection");
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "a copying Ctrl+C must not interrupt"
        );
        assert_eq!(app.status_bar.status(), &Status::Streaming);
    }

    #[tokio::test]
    async fn handle_crossterm_event_any_other_key_clears_the_selection_and_still_acts() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message("copy this word");
        select_word(&mut app, "this").await;

        app.handle_crossterm_event(&key_event(KeyCode::Char('x'), KeyModifiers::NONE));

        assert!(!app.chat.has_selection());
        assert_eq!(app.input.lines(), vec!["x"]);
    }

    #[test]
    fn handle_crossterm_resize_schedules_dirty_for_relayout() {
        // Resize falls through to `dirty = true` so the next tick re-splits with the new area.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dirty = false;
        app.handle_crossterm_event(&CrosstermEvent::Resize(80, 24));
        assert!(app.dirty, "Resize must trigger a re-layout render");
    }

    #[test]
    fn handle_crossterm_unhandled_events_skip_the_rerender() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dirty = false;

        for event in [
            CrosstermEvent::FocusGained,
            mouse_event(MouseEventKind::Moved),
            mouse_event(MouseEventKind::Down(MouseButton::Right)),
            mouse_event(MouseEventKind::Up(MouseButton::Middle)),
        ] {
            app.handle_crossterm_event(&event);
            assert!(!app.dirty, "{event:?}");
        }
    }

    #[test]
    fn handle_crossterm_key_release_is_ignored() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.input.set_text("hi");
        app.dirty = false;

        app.handle_crossterm_event(&CrosstermEvent::Key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        )));

        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "a release must not submit"
        );
        assert_eq!(app.input.lines(), vec!["hi".to_owned()]);
        assert!(!app.dirty);
    }

    #[test]
    fn handle_crossterm_page_up_scrolls_chat_while_idle_and_ctrl_end_jumps_back() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 80, 20);
        app.input.set_text("draft");

        app.handle_crossterm_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(app.chat.is_scrolled_up(), "idle PageUp reaches chat");

        app.handle_crossterm_event(&key_event(KeyCode::End, KeyModifiers::CONTROL));
        assert!(
            !app.chat.is_scrolled_up(),
            "Ctrl+End jumps chat to the bottom"
        );
        assert_eq!(app.input.lines(), vec!["draft".to_owned()]);
    }

    #[test]
    fn handle_crossterm_up_scrolls_chat_once_the_input_cursor_reaches_the_top() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 80, 20);
        app.input.set_text("one\ntwo");

        app.handle_crossterm_event(&key_event(KeyCode::Up, KeyModifiers::NONE));
        assert!(
            !app.chat.is_scrolled_up(),
            "first Up moves the input cursor"
        );

        app.handle_crossterm_event(&key_event(KeyCode::Up, KeyModifiers::NONE));
        assert!(app.chat.is_scrolled_up(), "Up on the top row scrolls chat");
    }

    #[test]
    fn handle_crossterm_popup_arrows_while_busy_leave_chat_scroll_alone() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 80, 20);
        app.handle_engine_event(Event::StreamToken("partial".to_owned()));
        app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(app.input.popup_visible());

        app.handle_crossterm_event(&key_event(KeyCode::Up, KeyModifiers::NONE));

        assert!(!app.chat.is_scrolled_up(), "the popup consumed Up");
    }

    #[test]
    fn handle_crossterm_paste_fills_the_input_without_submitting() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dirty = false;

        app.handle_crossterm_event(&CrosstermEvent::Paste(
            "first line\r\nsecond line".to_owned(),
        ));

        assert_eq!(
            app.input.lines(),
            vec!["first line".to_owned(), "second line".to_owned()],
        );
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "a paste never submits"
        );
        assert_eq!(app.chat.entry_count(), 0);
        assert!(app.dirty);
    }

    #[test]
    fn handle_crossterm_popup_enter_dispatches_canonical_command() {
        // `/h` filters to /help, and Enter dispatches it. /help is read-only, so chat lands a
        // system message and the engine receives nothing.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
        app.handle_crossterm_event(&key_event(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(app.input.popup_visible());

        app.handle_crossterm_event(&key_event(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!app.input.popup_visible(), "submit clears popup");
        assert!(!app.chat.last_is_error(), "/help must not push an error");
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "slash command stays client-side"
        );
    }

    #[test]
    fn handle_crossterm_popup_tab_completes_canonical_name_into_buffer() {
        // Tab inserts `/{name} ` and hides the popup. Filter to /help so the test pins
        // the completion shape independent of BUILT_INS ordering.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
        app.handle_crossterm_event(&key_event(KeyCode::Char('h'), KeyModifiers::NONE));

        app.handle_crossterm_event(&key_event(KeyCode::Tab, KeyModifiers::NONE));

        assert!(!app.input.popup_visible(), "Tab hides the popup");
        assert_eq!(
            app.input.lines(),
            vec!["/help ".to_owned()],
            "buffer reflects the completed canonical name + space",
        );
    }

    // ── modal gate ──

    #[tokio::test]
    async fn modal_gate_intercepts_keys_before_input_sees_them() {
        // Modal keys must not reach input.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.push_modal(Box::new(ScriptedModal::new(ModalAction::Dispatch(
            Action::Op(Op::Interrupt),
        ))));

        app.handle_crossterm_event(&key_event(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(
            app.input.lines().iter().all(String::is_empty),
            "input must stay empty while modal is active",
        );
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "no Op must reach ops for a key the modal consumed"
        );

        // Modal submit flows through the normal dispatch path.
        app.handle_crossterm_event(&key_event(KeyCode::Char('s'), KeyModifiers::NONE));
        let forwarded = rx.recv().await.expect("modal-submitted action forwarded");
        assert!(matches!(forwarded, Op::Interrupt));
    }

    #[tokio::test]
    async fn modal_gate_routes_pastes_to_the_top_modal() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("/rename".to_owned()));

        app.handle_crossterm_event(&CrosstermEvent::Paste("Fix auth\r\nflow".to_owned()));
        app.handle_crossterm_event(&key_event(KeyCode::Enter, KeyModifiers::NONE));

        let forwarded = rx.recv().await.expect("rename forwarded");
        assert_eq!(
            forwarded,
            Op::Rename {
                title: "Fix auth flow".to_owned(),
            },
        );
        assert!(app.input.is_empty(), "the hidden input must stay empty");
    }

    #[test]
    fn modal_gate_paste_never_answers_an_approval() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        push_answerable_approval(&mut app, "rm -rf build");
        _ = render_app(&mut app, 60, 20);

        app.handle_crossterm_event(&CrosstermEvent::Paste("y\n".to_owned()));

        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "a paste must not decide the call"
        );
        assert!(app.modals.is_active());
        assert!(app.input.is_empty());
    }

    #[test]
    fn modal_gate_steps_the_modal_with_the_wheel_instead_of_chat() {
        let keys = Arc::new(Mutex::new(Vec::new()));
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        app.push_modal(Box::new(KeyLogModal(Arc::clone(&keys))));
        _ = render_app(&mut app, 60, 20);

        for kind in [
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::Moved,
        ] {
            app.handle_crossterm_event(&mouse_event(kind));
        }

        assert_eq!(*keys.lock().unwrap(), [KeyCode::Up, KeyCode::Down]);
        assert!(!app.chat.is_scrolled_up(), "chat under the modal stays put");
        assert!(app.modals.is_active());
    }

    #[test]
    fn modal_gate_cancel_closes_modal_without_dispatching() {
        // `ModalKey::Cancelled` pops the modal without dispatching, so the next key reaches input.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.push_modal(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::NONE));

        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "Cancelled must not dispatch any Op"
        );
        // With the modal closed, later keys reach the input.
        app.handle_crossterm_event(&key_event(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(app.input.lines(), vec!["y".to_owned()]);
    }

    #[test]
    fn clear_modals_resolves_a_pending_approval_to_abort() {
        // A session swap clears the stack while an approval is outstanding. The cancel hook's Abort
        // must reach the engine so the blocked turn is not stranded waiting for a decision.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::ApprovalRequested {
            id: "call-1".to_owned(),
            preview: ApprovalPreview {
                title: "Bash(rm -rf /)".to_owned(),
                body: ApprovalBody::Command("rm -rf /".to_owned()),
            },
            basis: Basis::Builtin(Shipped::Rm),
            grant: None,
        });

        app.clear_modals();

        let forwarded = rx
            .try_recv()
            .expect("clear must forward the cancel-hook abort");
        assert!(matches!(
            forwarded,
            Op::Resolve {
                id,
                decision: Decision::Abort,
            } if id == "call-1"
        ));
    }

    // ── handle_esc ──

    #[tokio::test]
    async fn handle_esc_busy_dispatches_interrupt() {
        // Esc is routed by `App` because its meaning depends on queue state and run-state.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::StreamToken("partial".to_owned()));
        app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.status_bar.status(), &Status::Cancelling);
        let forwarded = rx.recv().await.expect("Interrupt forwarded");
        assert!(matches!(forwarded, Op::Interrupt));
    }

    #[test]
    fn handle_esc_idle_with_empty_queue_is_silent() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.pending_prompts.is_empty());
    }

    #[test]
    fn handle_esc_idle_with_queue_pops_most_recent_into_input() {
        // The most recent prompt (back of FIFO) returns to input, while the rest stay queued.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.pending_prompts.push_back("first".to_owned());
        app.pending_prompts.push_back("second".to_owned());

        app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.input.lines(), vec!["second".to_owned()]);
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["first".to_owned()],
        );
    }

    #[test]
    fn handle_esc_idle_with_buffer_content_refuses_pop() {
        // Esc must not clobber an in-progress draft.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.pending_prompts.push_back("queued".to_owned());
        app.input.set_text("draft");

        app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.input.lines(), vec!["draft".to_owned()]);
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["queued".to_owned()],
        );
    }

    #[test]
    fn handle_esc_with_popup_visible_dismisses_popup_and_leaves_queue_intact() {
        // The popup gate runs before App's Esc routing, so the popup hides and the queue stays put.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.pending_prompts.push_back("queued".to_owned());
        app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(app.input.popup_visible(), "/ opens the popup");

        app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!app.input.popup_visible(), "Esc dismisses the popup");
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["queued".to_owned()],
            "queue must not be peeled while popup owns Esc",
        );
    }

    // ── dispatch_action ──

    #[tokio::test]
    async fn dispatch_submit_prompt_updates_chat_status_and_forwards_action() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("hello".to_owned()));

        assert_eq!(app.chat.entry_count(), 1);
        assert!(!app.input.is_enabled(), "streaming disables input");
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.should_quit);
        let forwarded = rx.recv().await.expect("forwarded action");
        assert!(matches!(forwarded, Op::Prompt(s) if s == "hello"));
    }

    #[test]
    fn dispatch_slash_command_renders_locally_without_forwarding() {
        // Slash commands stay client-side, so the engine never sees the prompt.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("/help".to_owned()));

        assert!(app.modals.is_active(), "/help opens a modal");
        assert_eq!(
            app.chat.entry_count(),
            0,
            "modal-only commands push no chat blocks and suppress their own echo",
        );
        assert!(
            app.input.is_enabled(),
            "slash command must not flip input to streaming",
        );
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(
            !app.chat.last_is_error(),
            "/help must not produce an error block",
        );
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn dispatch_unknown_slash_command_renders_error_without_forwarding() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("/no-such".to_owned()));

        assert_eq!(app.chat.entry_count(), 2);
        assert!(app.input.is_enabled());
        assert!(app.chat.last_is_error());
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[tokio::test]
    async fn dispatch_double_slash_escapes_command_and_forwards_literal() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("//etc/hosts".to_owned()));

        assert_eq!(app.chat.entry_count(), 1, "only the user message");
        assert!(!app.input.is_enabled(), "streaming disables input");
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.chat.last_is_error());
        let forwarded = rx.recv().await.expect("forwarded action");
        assert!(matches!(
            forwarded,
            Op::Prompt(s) if s == "/etc/hosts",
        ));
        assert_eq!(
            app.active_prompt.as_deref(),
            Some("//etc/hosts"),
            "kept as typed, so a failed auto-compaction hands back the escape",
        );
        let text = rendered_text(&mut app, 60, 10);
        assert!(
            text.contains("/etc/hosts") && !text.contains("//etc"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn dispatch_op_interrupt_flips_status_to_cancelling_and_forwards() {
        // An interrupt flips status immediately, and `Event::Cancelled` later returns to idle.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("hi".to_owned()));
        rx.recv().await.expect("submit forwarded");
        let entries_before = app.chat.entry_count();

        app.dispatch_action(Action::Op(Op::Interrupt));

        assert_eq!(app.chat.entry_count(), entries_before, "no new chat entry");
        assert_eq!(app.status_bar.status(), &Status::Cancelling);
        assert!(!app.should_quit);
        let forwarded = rx.recv().await.expect("Interrupt forwarded");
        assert!(matches!(forwarded, Op::Interrupt));
    }

    #[test]
    fn dispatch_op_shutdown_sets_should_quit_and_leaves_chat_untouched() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Op(Op::Shutdown));

        assert!(app.should_quit);
        assert_eq!(app.chat.entry_count(), 0);
        // Shutdown skips streaming setup so tear-down doesn't have to un-spinner.
        assert_eq!(app.status_bar.status(), &Status::Idle);
    }

    #[test]
    fn dispatch_closed_channel_surfaces_error_and_quits() {
        // Dropping `op_rx` simulates a stopped engine, which the UI announces before teardown.
        let (mut app, rx, _event_tx) = test_app(None);
        drop(rx);

        app.dispatch_action(Action::Submit("hi".to_owned()));

        assert!(app.should_quit, "closed channel must trigger teardown");
        assert!(
            !app.input.is_enabled(),
            "input stays disabled during teardown"
        );
        // User message pushed before try_send, error block after.
        assert_eq!(app.chat.entry_count(), 2);
        assert!(
            app.chat.last_is_error(),
            "closed-channel error should be the final block"
        );
    }

    #[test]
    fn dispatch_full_channel_surfaces_error_but_keeps_app_alive() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        for _ in 1..OP_CHANNEL_CAPACITY {
            app.dispatch_action(Action::Op(Op::Interrupt));
        }
        let before_overflow = app.chat.entry_count();

        app.dispatch_action(Action::Op(Op::Interrupt));

        assert!(!app.should_quit, "Full is not fatal");
        assert_eq!(
            app.chat.entry_count(),
            before_overflow + 1,
            "exactly one error block on overflow",
        );
        assert_eq!(
            app.chat.last_error_text(),
            Some("The engine is busy, so the request was dropped. Try again."),
        );
    }

    #[test]
    fn dispatch_full_channel_on_approval_points_to_the_cancel_recovery() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        for _ in 1..OP_CHANNEL_CAPACITY {
            app.dispatch_action(Action::Op(Op::Interrupt));
        }

        app.dispatch_action(Action::Op(Op::Resolve {
            id: "call-1".to_owned(),
            decision: Decision::Approve,
        }));

        assert!(!app.should_quit, "Full is not fatal");
        assert_eq!(
            app.chat.last_error_text(),
            Some("Approval could not be delivered. Press Esc to cancel the blocked turn."),
        );
    }

    #[tokio::test]
    async fn dispatch_submit_during_busy_queues_and_forwards_for_mid_turn_drain() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("first".to_owned()));
        rx.recv().await.expect("first submit forwarded");

        app.dispatch_action(Action::Submit("queued".to_owned()));

        // Buffered for preview and forwarded so the turn can splice it in at its next round
        // boundary. Chat stays untouched until `PromptDrained` lands.
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["queued".to_owned()],
        );
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        let forwarded = rx.recv().await.expect("queued submit forwarded");
        assert_eq!(forwarded, Op::Queue("queued".to_owned()));
    }

    #[tokio::test]
    async fn dispatch_op_forwards_engine_ops_unchanged() {
        // A modal-emitted `SetModel` must reach the engine so it can emit `ConfigChanged`.
        for op in [
            Op::SetModel {
                model: Some(crate::model::ResolvedModelId::new(
                    "claude-opus-5-5".to_owned(),
                )),
                effort: None,
            },
            Op::SetModel {
                model: None,
                effort: Some(Effort::High),
            },
            Op::Clear,
        ] {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_op(op.clone());

            let forwarded = rx.recv().await.expect("op forwarded to the engine");
            assert_eq!(forwarded, op);
            assert!(!app.should_quit);
            assert_eq!(app.chat.entry_count(), 0);
        }
    }

    #[tokio::test]
    async fn dispatch_resume_forwards_to_the_engine_and_disables_input_until_event() {
        // Between forwarding `Resume` and the `SessionResumed` event, input stays gated so a typed
        // prompt doesn't push into chat just before `apply_session_resumed` clears it.
        let (mut app, mut rx, _event_tx) = test_app(None);
        let action = Op::Resume {
            target: ResumeTarget::Path("resume-target.jsonl".into()),
        };
        app.dispatch_op(action.clone());

        let forwarded = rx.recv().await.expect("Resume must reach the engine");
        assert_eq!(forwarded, action);
        assert!(
            !app.input.is_enabled(),
            "input must be gated until the resume event lands",
        );
        assert_eq!(app.status_bar.status(), &Status::Resuming);
    }

    #[tokio::test]
    async fn dispatch_compact_forwards_to_the_engine_and_disables_input_until_event() {
        // Mirror of the Resume gate: the chat is about to be wiped by `apply_session_compacted`,
        // so the user's input must be parked until SessionCompacted re-enables it.
        let (mut app, mut rx, _event_tx) = test_app(None);
        let action = Op::Compact {
            instructions: Some("focus".to_owned()),
        };
        app.dispatch_op(action.clone());

        let forwarded = rx.recv().await.expect("Compact must reach the engine");
        assert_eq!(forwarded, action);
        assert!(
            !app.input.is_enabled(),
            "input must be gated until the compact event lands",
        );
        assert_eq!(app.status_bar.status(), &Status::Compacting);
    }

    #[tokio::test]
    async fn dispatch_login_disables_input_and_holds_prompts_typed_while_signing_in() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_op(Op::Login);

        assert_eq!(rx.recv().await, Some(Op::Login));
        assert!(!app.input.is_enabled());
        assert_eq!(app.status_bar.status(), &Status::SigningIn);

        app.dispatch_action(Action::Submit("typed-while-signing-in".to_owned()));

        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "no turn exists to splice a prompt into while signing in"
        );
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["typed-while-signing-in".to_owned()],
        );
    }

    #[tokio::test]
    async fn dispatch_submit_during_cancelling_holds_locally_without_forwarding() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.dispatch_action(Action::Op(Op::Interrupt));
        rx.recv().await.expect("cancel forwarded");
        assert_eq!(app.status_bar.status(), &Status::Cancelling);

        app.dispatch_action(Action::Submit("typed-during-cancel".to_owned()));

        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "submit during cancel must not reach the engine"
        );
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["typed-during-cancel".to_owned()],
        );
    }

    #[tokio::test]
    async fn dispatch_submit_during_compacting_holds_locally_without_forwarding() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Op(Op::Compact { instructions: None }));
        rx.recv().await.expect("compact forwarded");
        assert_eq!(app.status_bar.status(), &Status::Compacting);

        app.dispatch_action(Action::Submit("typed-during-compact".to_owned()));

        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "submit during compact must wait for SessionCompacted to drain it"
        );
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["typed-during-compact".to_owned()],
        );
    }

    #[tokio::test]
    async fn dispatch_read_only_slash_during_busy_runs_client_side_without_queueing() {
        // Read-only slash commands run client-side immediately during a busy turn, since queueing
        // them would make the LLM answer `/help` literally on drain.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        assert_eq!(app.status_bar.status(), &Status::Streaming);

        app.dispatch_action(Action::Submit("/help".to_owned()));

        assert!(
            app.pending_prompts.is_empty(),
            "slash command must not queue",
        );
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "slash command must not forward to ops"
        );
        // Only the original "active" prompt: /help is modal-only and suppresses its own echo.
        assert_eq!(app.chat.entry_count(), 1);
        assert!(!app.chat.last_is_error());
        assert!(app.modals.is_active(), "/help opens a modal");
    }

    #[tokio::test]
    async fn dispatch_clear_during_busy_refuses_with_system_message_no_dispatch() {
        // State-mutating commands refuse mid-turn, since rolling while `messages` is still draining
        // would race the in-flight turn into the wrong JSONL.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");

        app.dispatch_action(Action::Submit("/clear".to_owned()));

        assert!(app.pending_prompts.is_empty());
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "neither a prompt nor Clear may reach the engine"
        );
        let body = app.chat.last_system_text().expect("refusal system message");
        assert!(
            body.contains("/clear runs only when idle"),
            "refusal must name the command and gate: {body}",
        );
    }

    #[tokio::test]
    async fn dispatch_init_forwards_synthesized_prompt_and_flips_to_streaming() {
        // Chat shows only `/init`, while the engine must receive the synthesized body.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "# AGENTS.md\n").unwrap();
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.session_info.project_root = root.path().to_owned();

        app.dispatch_action(Action::Submit("/init".to_owned()));

        assert_eq!(app.chat.entry_count(), 1, "only the typed `/init` line");
        assert!(!app.input.is_enabled(), "Streaming disables input");
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        let forwarded = rx.recv().await.expect("synthesized prompt forwarded");
        assert!(
            matches!(
                &forwarded,
                Op::Prompt(body) if body.contains("- `AGENTS.md`") && body.contains("unified diff")
            ),
            "expected the update prompt, got {forwarded:?}",
        );
    }

    #[tokio::test]
    async fn dispatch_init_without_an_instruction_file_starts_the_turn_from_the_picker() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.session_info.project_root = root.path().to_owned();

        app.dispatch_action(Action::Submit("/init".to_owned()));
        assert!(app.modals.is_active(), "the file picker opens first");
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "no turn before the pick"
        );
        app.handle_crossterm_event(&key_event(KeyCode::Down, KeyModifiers::NONE));
        app.handle_crossterm_event(&key_event(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!app.modals.is_active());
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        let forwarded = rx.recv().await.expect("create prompt forwarded");
        assert!(
            matches!(&forwarded, Op::Prompt(body) if body.contains("create `CLAUDE.md`")),
            "expected the create prompt for the second file, got {forwarded:?}",
        );
    }

    #[tokio::test]
    async fn dispatch_init_during_busy_refuses_with_system_message_no_forward() {
        // Mutating: refuse. The typed `/init` row still lands, but its synthesized body does not.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");

        app.dispatch_action(Action::Submit("/init".to_owned()));

        assert!(app.pending_prompts.is_empty());
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "no synthesized prompt must reach ops mid-turn"
        );
        assert_eq!(
            app.chat.entry_count(),
            3,
            "active prompt + typed /init + system refusal",
        );
        let body = app.chat.last_system_text().expect("refusal system message");
        assert!(
            body.contains("/init runs only when idle"),
            "refusal must name the command and gate: {body}",
        );
    }

    #[tokio::test]
    async fn dispatch_arg_bearing_slash_during_busy_refuses_locally() {
        for (cmd, gate_phrase) in [
            ("/model opus", "/model runs only when idle"),
            ("/effort xhigh", "/effort runs only when idle"),
        ] {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.recv().await.expect("active submit forwarded");

            app.dispatch_action(Action::Submit(cmd.to_owned()));

            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "{cmd}: action must not reach ops mid-turn"
            );
            let body = app.chat.last_system_text().expect("refusal system message");
            assert!(body.contains(gate_phrase), "{cmd}: refusal: {body}");
        }
    }

    #[tokio::test]
    async fn dispatch_config_modal_slash_during_busy_refuses_locally() {
        for (cmd, gate_phrase) in [
            ("/model", "/model runs only when idle"),
            ("/effort", "/effort runs only when idle"),
        ] {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.recv().await.expect("active submit forwarded");

            app.dispatch_action(Action::Submit(cmd.to_owned()));

            assert!(
                !app.modals.is_active(),
                "{cmd}: modal must not open mid-turn"
            );
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "{cmd}: action must not reach ops mid-turn"
            );
            let body = app.chat.last_system_text().expect("refusal system message");
            assert!(body.contains(gate_phrase), "{cmd}: refusal: {body}");
        }
    }

    #[tokio::test]
    async fn dispatch_unknown_slash_during_busy_renders_error_no_queue() {
        // Unknown commands route through `dispatch`, so the user sees the canonical
        // "unknown command" error with recovery hints (alternatives and the `//` escape),
        // and the prompt never reaches the LLM.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");

        app.dispatch_action(Action::Submit("/nope".to_owned()));

        assert!(app.pending_prompts.is_empty());
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        assert!(app.chat.last_is_error());
    }

    #[tokio::test]
    async fn dispatch_mode_during_busy_reaches_the_engine_mid_turn() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");

        app.dispatch_action(Action::Submit("/mode plan".to_owned()));
        app.handle_crossterm_event(&key_event(KeyCode::BackTab, KeyModifiers::SHIFT));
        app.dispatch_action(Action::Submit("/mode".to_owned()));

        assert_eq!(rx.try_recv().ok(), Some(Op::SetMode(Mode::Plan)));
        assert_eq!(rx.try_recv().ok(), Some(Op::SetMode(Mode::Manual)));
        assert!(app.modals.is_active(), "the bare form opens the picker");
        assert!(app.pending_prompts.is_empty());
    }

    // ── cycle_mode ──

    #[test]
    fn cycle_mode_steps_through_manual_and_plan_plus_yolo_for_a_yolo_start() {
        for (start, expected) in [
            (Mode::Manual, [Mode::Plan, Mode::Manual, Mode::Plan]),
            (Mode::Plan, [Mode::Manual, Mode::Plan, Mode::Manual]),
            (Mode::Yolo, [Mode::Manual, Mode::Plan, Mode::Yolo]),
        ] {
            let mut info = test_session_info();
            info.config.permission_mode = start;
            let (mut app, mut rx, _event_tx) = test_app_with(info, None);

            let sent = expected.map(|_| {
                app.handle_crossterm_event(&key_event(KeyCode::BackTab, KeyModifiers::SHIFT));
                let Some(Op::SetMode(mode)) = rx.try_recv().ok() else {
                    panic!("Shift+Tab sends a mode switch");
                };
                app.handle_engine_event(Event::ModeChanged(mode));
                mode
            });

            assert_eq!(sent, expected, "{start}");
        }
    }

    #[test]
    fn cycle_mode_steps_on_from_a_switch_awaiting_its_confirmation() {
        let (mut app, mut rx, _event_tx) = test_app(None);

        app.handle_crossterm_event(&key_event(KeyCode::BackTab, KeyModifiers::SHIFT));
        app.handle_crossterm_event(&key_event(KeyCode::BackTab, KeyModifiers::SHIFT));

        assert_eq!(rx.try_recv().ok(), Some(Op::SetMode(Mode::Plan)));
        assert_eq!(rx.try_recv().ok(), Some(Op::SetMode(Mode::Manual)));
    }

    #[test]
    fn cycle_mode_from_a_mode_outside_the_cycle_returns_to_manual() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("/mode yolo".to_owned()));
        assert_eq!(rx.try_recv().ok(), Some(Op::SetMode(Mode::Yolo)));

        app.handle_crossterm_event(&key_event(KeyCode::BackTab, KeyModifiers::SHIFT));

        assert_eq!(rx.try_recv().ok(), Some(Op::SetMode(Mode::Manual)));
    }

    // ── expire_armed_exit ──

    #[test]
    fn expire_armed_exit_becomes_idle_once_the_window_ends() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::ConfirmExit);
        let Status::ExitArmed { until } = *app.status_bar.status() else {
            panic!("Ctrl+C arms the exit window");
        };
        let before = until.checked_sub(Duration::from_millis(1)).unwrap();

        assert!(!app.expire_armed_exit(before));
        assert_eq!(app.status_bar.status(), &Status::ExitArmed { until });
        assert!(app.expire_armed_exit(until));
        assert_eq!(app.status_bar.status(), &Status::Idle);
    }

    #[test]
    fn expire_armed_exit_when_not_armed_leaves_the_status() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.status_bar.set_status(Status::Streaming);

        assert!(!app.expire_armed_exit(Instant::now()));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
    }

    // ── dispatch_action: theme ──

    #[test]
    fn preview_theme_repaints_components_and_caches_original() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let original = app.theme.clone();
        app.dispatch_action(Action::PreviewTheme {
            name: "latte".to_owned(),
        });
        assert!(
            app.preview_theme_snapshot.is_some(),
            "first PreviewTheme must cache the original",
        );
        assert_ne!(app.theme.text, original.text, "live theme must change");
        // preview leaves session_info.config.theme_name as-is until commit.
        assert_eq!(app.session_info.config.theme_name, "mocha");
    }

    #[test]
    fn preview_theme_then_modal_cancel_restores_original() {
        // Esc / Ctrl+C surface as ModalAction::Cancelled, so the snapshot must roll back.
        let (mut app, _rx, _event_tx) = test_app(None);
        let original = app.theme.clone();
        app.dispatch_action(Action::PreviewTheme {
            name: "latte".to_owned(),
        });
        app.apply_modal_action(ModalAction::Cancelled);
        assert!(
            app.preview_theme_snapshot.is_none(),
            "snapshot consumed on cancel",
        );
        assert_eq!(app.theme.text, original.text, "theme rolled back to mocha");
    }

    #[test]
    fn clearing_modal_stack_rolls_back_theme_preview() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let original = app.theme.clone();
        app.push_modal(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        app.dispatch_action(Action::PreviewTheme {
            name: "latte".to_owned(),
        });

        app.clear_modals();

        assert!(!app.modals.is_active());
        assert!(app.preview_theme_snapshot.is_none());
        assert_eq!(app.theme.text, original.text);
    }

    #[test]
    fn modal_cancel_without_snapshot_is_a_noop() {
        // Modals that never previewed (e.g. /model picker) cancel through the same
        // ModalAction::Cancelled path, so the rollback arm must skip cleanly with no snapshot.
        let (mut app, _rx, _event_tx) = test_app(None);
        let original_text = app.theme.text;
        app.apply_modal_action(ModalAction::Cancelled);
        assert!(app.preview_theme_snapshot.is_none());
        assert_eq!(app.theme.text, original_text);
    }

    #[test]
    fn modal_system_message_action_pushes_into_chat() {
        // Destructive-action modals (e.g. confirm-delete) report success via SystemMessage so
        // the user has chat-stream evidence the action ran. Pin the body verbatim so a future
        // routing change that drops the message or pushes an empty block fails here.
        let (mut app, _rx, _event_tx) = test_app(None);
        let entries_before = app.chat.entry_count();
        let body = "Deleted session abc12345: Fix auth flow".to_owned();
        app.apply_modal_action(ModalAction::SystemMessage(body.clone()));
        assert_eq!(
            app.chat.entry_count(),
            entries_before + 1,
            "SystemMessage must push exactly one block",
        );
        assert_eq!(app.chat.last_system_text(), Some(body.as_str()));
    }

    #[test]
    fn preview_theme_with_unknown_name_does_nothing_and_keeps_active_theme() {
        // Roster drift between the slash and built-in tables only logs, so user state stays put.
        let (mut app, _rx, _event_tx) = test_app(None);
        let original_text = app.theme.text;
        let entries_before = app.chat.entry_count();

        app.dispatch_action(Action::PreviewTheme {
            name: "nonexistent".to_owned(),
        });

        assert!(
            app.preview_theme_snapshot.is_none(),
            "no snapshot when load_builtin returns None",
        );
        assert_eq!(app.theme.text, original_text, "theme stays put on drift");
        assert_eq!(
            app.chat.entry_count(),
            entries_before,
            "preview must never push a chat block",
        );
    }

    #[test]
    fn swap_theme_commits_and_clears_snapshot() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::PreviewTheme {
            name: "latte".to_owned(),
        });
        let previewed = app.theme.clone();
        app.dispatch_action(Action::SwapTheme {
            name: "latte".to_owned(),
        });
        assert!(app.preview_theme_snapshot.is_none());
        assert_eq!(app.session_info.config.theme_name, "latte");
        assert_eq!(app.theme.text, previewed.text);

        app.apply_modal_action(ModalAction::Cancelled);
        assert_eq!(
            app.session_info.config.theme_name, "latte",
            "post-commit cancel must not restore the pre-preview theme",
        );
    }

    #[test]
    fn swap_theme_with_unknown_name_is_a_silent_noop() {
        // Only a dev bug can name an unknown built-in, so log via tracing and leave UI state alone.
        let (mut app, _rx, _event_tx) = test_app(None);
        let original = app.session_info.config.theme_name.clone();
        let entries_before = app.chat.entry_count();

        app.dispatch_action(Action::SwapTheme {
            name: "nonexistent".to_owned(),
        });

        assert_eq!(app.session_info.config.theme_name, original);
        assert_eq!(
            app.chat.entry_count(),
            entries_before,
            "dev-only drift adds no chat block",
        );
    }

    // ── handle_submit_prompt ──

    #[test]
    fn submit_slash_theme_pushes_picker_onto_modal_stack() {
        // Bare `/theme` is the ReadOnly modal-opening branch, so its `SlashOutcome::Modal` must
        // land on `App::modals`.
        let (mut app, _rx, _event_tx) = test_app(None);
        assert!(!app.modals.is_active(), "stack starts empty");
        app.dispatch_action(Action::Submit("/theme".to_owned()));
        assert!(app.modals.is_active(), "bare `/theme` opens a picker modal");
    }

    #[test]
    fn slash_typed_swap_theme_routes_through_local_handler() {
        // A synthesized TUI-only action like `/theme latte` runs its local `dispatch_action` arm.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("/theme latte".to_owned()));

        assert_eq!(
            app.session_info.config.theme_name, "latte",
            "typed `/theme <name>` must mutate session theme",
        );
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "TUI-only theme swap must not leak to the engine"
        );
    }

    #[tokio::test]
    async fn slash_typed_model_routes_synthesized_set_model_through_dispatch() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("/model haiku".to_owned()));

        let forwarded = rx.recv().await.expect("SetModel forwarded to the engine");
        match forwarded {
            Op::SetModel { model, effort } => {
                assert_eq!(
                    model.as_ref().map(crate::model::ResolvedModelId::as_str),
                    Some("claude-haiku-5-5")
                );
                assert_eq!(effort, None);
            }
            other => panic!("expected SetModel, got {other:?}"),
        }
    }

    #[test]
    fn submit_after_a_cancel_runs_the_held_queue_before_the_new_prompt() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        app.dispatch_action(Action::Submit("held".to_owned()));
        app.handle_esc();
        app.handle_engine_event(Event::Cancelled);
        let mut forwarded = || std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(forwarded().len(), 3, "submit, queue, and cancel");

        app.dispatch_action(Action::Submit("new".to_owned()));

        assert_eq!(
            forwarded(),
            [Op::Prompt("held".to_owned()), Op::Queue("new".to_owned())],
        );
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["new".to_owned()],
        );
    }

    #[test]
    fn submit_busy_double_slash_queues_the_literal_and_keeps_the_escape() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        app.dispatch_action(Action::Submit("//etc/hosts".to_owned()));
        app.dispatch_action(Action::Submit("//tmp".to_owned()));
        let mut forwarded = || std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(
            forwarded(),
            [
                Op::Prompt("active".to_owned()),
                Op::Queue("/etc/hosts".to_owned()),
                Op::Queue("/tmp".to_owned()),
            ],
        );

        app.handle_engine_event(Event::PromptDrained("/etc/hosts".to_owned()));
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["//tmp".to_owned()],
            "the queue keeps the typed escape so Esc restores it",
        );

        app.handle_engine_event(Event::Cancelled);
        app.dispatch_action(Action::Submit("new".to_owned()));
        assert_eq!(
            forwarded(),
            [Op::Prompt("/tmp".to_owned()), Op::Queue("new".to_owned())],
        );
        assert_eq!(app.active_prompt.as_deref(), Some("//tmp"));
    }

    // ── forward_queued_prompts ──

    #[test]
    fn forward_queued_prompts_keeps_a_prompt_the_full_channel_turned_away_queued() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        while app.ops.try_send(Op::Interrupt).is_ok() {}
        let entries_before = app.chat.entry_count();

        app.dispatch_action(Action::Submit("a".to_owned()));

        assert_eq!(app.pending_prompts, ["a"]);
        assert_eq!(app.forwarded_prompts, 0);
        assert_eq!(app.chat.entry_count(), entries_before, "no error block");
    }

    #[test]
    fn forward_queued_prompts_resends_a_prompt_the_full_channel_turned_away() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        while app.ops.try_send(Op::Interrupt).is_ok() {}
        app.dispatch_action(Action::Submit("a".to_owned()));
        while rx.try_recv().is_ok() {}

        app.dispatch_action(Action::Submit("b".to_owned()));
        let mut sent = Vec::new();
        while let Ok(op) = rx.try_recv() {
            if let Op::Queue(prompt) = &op {
                app.handle_engine_event(Event::PromptDrained(prompt.clone()));
            }
            sent.push(op);
        }
        app.handle_engine_event(Event::TurnComplete);
        sent.extend(std::iter::from_fn(|| rx.try_recv().ok()));

        assert_eq!(sent, [Op::Queue("a".to_owned()), Op::Queue("b".to_owned())]);
        assert!(app.pending_prompts.is_empty());
    }

    #[test]
    fn forward_queued_prompts_reports_a_closed_channel_once() {
        let (mut app, rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        app.pending_prompts.extend(["a".to_owned(), "b".to_owned()]);
        drop(rx);
        let entries_before = app.chat.entry_count();

        app.forward_queued_prompts();

        assert_eq!(app.chat.entry_count(), entries_before + 1);
        assert_eq!(
            app.chat.last_error_text(),
            Some("The engine stopped unexpectedly. Restart `ox` to recover."),
        );
        assert!(app.should_quit);
        assert_eq!(app.forwarded_prompts, 0);
    }

    // ── apply_slash_outcome ──

    #[test]
    fn apply_slash_outcome_shows_output_and_hands_back_forwards_whether_idle_or_busy() {
        let diff = indoc! {"
            diff --git a/src/lib.rs b/src/lib.rs
            --- a/src/lib.rs
            +++ b/src/lib.rs
            @@ -1 +1 @@
            -old line
            +new line
        "};
        for busy in [false, true] {
            let (mut app, mut rx, _event_tx) = test_app(None);
            if busy {
                app.handle_engine_event(Event::StreamToken("partial".to_owned()));
            }

            let clean = SlashOutcome::SystemMessage("Working tree clean".to_owned());
            assert_eq!(app.apply_slash_outcome(Ok(clean)), None);
            assert_eq!(app.chat.last_system_text(), Some("Working tree clean"));

            assert_eq!(
                app.apply_slash_outcome(Ok(SlashOutcome::GitDiff(diff.to_owned()))),
                None
            );
            let text = rendered_text(&mut app, 60, 24);
            assert!(text.contains("new line"), "busy = {busy}: {text}");

            assert_eq!(app.apply_slash_outcome(Err("/diff: boom".to_owned())), None);
            assert_eq!(app.chat.last_error_text(), Some("/diff: boom"));

            let modal = SlashOutcome::Modal(Box::new(FakeModal::new("picker")));
            assert_eq!(app.apply_slash_outcome(Ok(modal)), None);
            assert!(app.modals.is_active());

            let forward = SlashOutcome::Forward(Action::Op(Op::Clear));
            assert_eq!(
                app.apply_slash_outcome(Ok(forward)),
                Some(Action::Op(Op::Clear)),
            );
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "the caller routes a forwarded action"
            );
        }
    }
}
