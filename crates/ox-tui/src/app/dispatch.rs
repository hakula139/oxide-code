//! Terminal input routing, prompt submission, and engine dispatch.

use std::time::Instant;

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Position;
use tokio::sync::mpsc::OwnedPermit;
use tokio::sync::mpsc::error::TrySendError;
use tracing::debug;

use ox_core::command::{self, Parsed};
use ox_core::protocol::{Op, SignInCode};

use super::operation::InFlight;
use super::queue::QueuedPrompt;
use super::{App, EXIT_WINDOW, SlashTask};
use crate::action::{APP_KEYMAP, Action, AppAction, next_mode};
use crate::components::input::InputOutcome;
use crate::keymap::{CTRL_C, CTRL_D};
use crate::modal::ModalAction;
use crate::now;
use crate::slash::{self, SlashContext, SlashKind, SlashOutcome};
use crate::theme::Theme;
use crate::theme::builtin::BuiltinTheme;
use crate::viewport::Direction;

const BUSY_DROPPED: &str = "ox is busy, so the request was dropped. Try again.";

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
        if let CrosstermEvent::Key(key) = event
            && let Some(action) = APP_KEYMAP.action_for(key)
        {
            self.apply_app_action(action);
            self.dirty = true;
            return;
        }
        match event {
            CrosstermEvent::Key(key) if CTRL_C.matches(key) => self.handle_ctrl_c(),
            CrosstermEvent::Key(key) if CTRL_D.matches(key) => self.handle_ctrl_d(key),
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
                    if let Some(text) = self.chat.handle_mouse(*mouse, now()) {
                        self.copy_selection(text);
                    }
                }
            }
            CrosstermEvent::Resize(..) => {}
            _ => return,
        }
        self.dirty = true;
    }

    /// Opens the status-bar link under a plain left press, returning whether it hit one. A press
    /// with modifiers stays with terminals that open OSC 8 links on Ctrl+click or Cmd+click.
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
            debug!(error = %e, "failed to open the browser");
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
        let ctrl_c = CTRL_C.matches(key);
        let Some(text) = self.chat.take_selection().filter(|_| ctrl_c) else {
            return false;
        };
        self.copy_selection(text);
        true
    }

    fn copy_selection(&mut self, text: String) {
        self.pending_clipboard = None;
        self.copy_notice = None;
        self.clipboard.copy(text);
    }

    fn apply_app_action(&mut self, action: AppAction) {
        match action {
            AppAction::ToggleExpanded => self.chat.toggle_expanded(),
        }
    }

    pub(super) fn apply_modal_action(&mut self, action: ModalAction) {
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

    /// Swaps the theme every later frame paints with and re-renders the chat's cached blocks.
    fn apply_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
        self.chat.set_theme(theme);
        self.dirty = true;
    }

    /// Arms the exit window, or quits inside it, when idle, and interrupts the running work when
    /// busy.
    fn handle_ctrl_c(&mut self) {
        if self.in_flight.is_none() {
            self.confirm_exit();
        } else {
            self.dispatch_op(Op::Interrupt);
        }
    }

    /// Deletes forward while the input holds text. An empty idle input quits as a shell does at EOF,
    /// through the same double-press window as Ctrl+C, so a habitual EOF press does not close `ox`.
    fn handle_ctrl_d(&mut self, key: &KeyEvent) {
        if self.in_flight.is_none() && self.input.is_empty() {
            self.confirm_exit();
        } else {
            _ = self.input.handle_event(&CrosstermEvent::Key(*key));
        }
    }

    /// Cancels if busy, restores one queued prompt if idle, or no-ops.
    pub(super) fn handle_esc(&mut self) {
        if self.in_flight.is_some() {
            self.dispatch_op(Op::Interrupt);
        } else if self.input.is_empty()
            && let Some(prompt) = self.queue.pop_last()
        {
            self.input.set_text(&prompt);
        }
    }

    /// Routes an input, slash-command, or modal action. A submit runs as a slash command or a
    /// prompt, a built prompt starts a turn, an op goes through [`Self::dispatch_op`], and the rest
    /// apply to TUI state.
    pub(crate) fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::Submit(text) => self.handle_submit_prompt(&text),
            Action::Op(op) => self.dispatch_op(op),
            Action::SlashPrompt { line, prompt } => {
                self.start_turn(QueuedPrompt::slash(line, prompt));
            }
            Action::CycleMode(direction) => self.cycle_mode(direction),
            Action::PreviewTheme(theme) => self.preview_theme(theme),
            Action::SwapTheme(theme) => self.swap_theme(theme),
        }
    }

    /// Reserves delivery before applying the op's local state change, and drops an idle-only op
    /// with a chat error while another operation runs, since the engine would drop it silently. An
    /// idle-only op stays in flight until its terminal event. Prompts reach the engine only through
    /// the prompt queue, by [`Self::start_turn`] and [`Self::forward_queued_prompts`].
    pub(super) fn dispatch_op(&mut self, op: Op) {
        if op == Op::Shutdown {
            self.should_quit = true;
        }
        if op.is_idle_only() && self.in_flight.is_some() {
            self.chat.push_error(BUSY_DROPPED);
            return;
        }
        let Some((permit, op)) = self.reserve_op(op) else {
            return;
        };
        match &op {
            Op::Interrupt => {
                if let Some(in_flight) = &mut self.in_flight {
                    in_flight.cancel();
                }
            }
            Op::Clear | Op::Archive | Op::Resume { .. } => self.queue.mark_switch(),
            Op::Compact { .. } => self.queue.mark_compacting(),
            Op::SetArchive { target, .. } => self.modals.archive_started(target),
            Op::DeleteSaved { session_id } => self.modals.delete_started(session_id),
            Op::SetMode(mode) => self.requested_mode = *mode,
            Op::Prompt { .. }
            | Op::Queue(_)
            | Op::Resolve { .. }
            | Op::FindSaved { .. }
            | Op::SetModel(_)
            | Op::Rename { .. }
            | Op::Login
            | Op::LoginCode(_)
            | Op::Logout
            | Op::Shutdown => {}
        }
        self.begin(&op);
        permit.send(op);
    }

    /// Puts `op` in flight when it runs until a terminal event, which also closes an exit window.
    fn begin(&mut self, op: &Op) {
        if let Some(in_flight) = InFlight::new(op) {
            self.in_flight = Some(in_flight);
            self.exit_armed = None;
        }
    }

    /// Reserves capacity for `op`. A full channel surfaces as a chat error block, and a closed
    /// channel quits. The permit keeps that capacity while local state changes.
    fn reserve_op(&mut self, op: Op) -> Option<(OwnedPermit<Op>, Op)> {
        let error = match self.ops.clone().try_reserve_owned() {
            Ok(permit) => return Some((permit, op)),
            Err(TrySendError::Full(_)) => TrySendError::Full(op),
            Err(TrySendError::Closed(_)) => TrySendError::Closed(op),
        };
        self.surface_send_error(&error);
        None
    }

    fn surface_send_error(&mut self, e: &TrySendError<Op>) {
        match e {
            // The loop quits before the next frame, and `main` reports why the engine stopped.
            TrySendError::Closed(_) => {
                debug!("the engine stopped, so the TUI quits");
                self.should_quit = true;
            }
            // An approval decision is a control-plane reply the engine is waiting for, so a dropped
            // one leaves the turn blocked until the user cancels it.
            TrySendError::Full(Op::Resolve { .. }) => {
                self.chat.push_error(
                    "Approval could not be delivered. Press Esc to cancel the blocked turn.",
                );
            }
            TrySendError::Full(Op::Prompt { .. }) => {
                self.chat.push_error(
                    "ox is busy, so the prompt stays queued. Press Esc to edit it, \
                     or submit another prompt to retry.",
                );
            }
            TrySendError::Full(_) => self.chat.push_error(BUSY_DROPPED),
        }
    }

    /// Quits on a second exit key inside the window, or arms it.
    fn confirm_exit(&mut self) {
        if self.exit_armed.is_some_and(|until| now() < until) {
            self.should_quit = true;
        } else {
            self.exit_armed = Some(now() + EXIT_WINDOW);
        }
    }

    /// Switches to the mode one step from the last requested one in the keyboard cycle.
    fn cycle_mode(&mut self, direction: Direction) {
        let next = next_mode(self.requested_mode, direction);
        self.dispatch_op(Op::SetMode(next));
    }

    /// Closes an exit window that ended by `now`. Returns whether it closed.
    pub(super) fn expire_armed_exit(&mut self, now: Instant) -> bool {
        self.exit_armed.take_if(|until| now >= *until).is_some()
    }

    fn preview_theme(&mut self, builtin: &BuiltinTheme) {
        let preview = self.with_overrides(builtin);
        self.preview_theme_snapshot
            .get_or_insert_with(|| self.theme.clone());
        self.apply_theme(&preview);
    }

    fn swap_theme(&mut self, builtin: &BuiltinTheme) {
        self.preview_theme_snapshot = None;
        let theme = self.with_overrides(builtin);
        self.session_info.config.apply_theme(builtin.name);
        self.apply_theme(&theme);
        self.chat
            .push_system_message(format!("Theme set to {}", builtin.name));
    }

    /// `builtin`'s palette with the configured `[tui.theme.overrides]` applied, as at startup.
    fn with_overrides(&self, builtin: &BuiltinTheme) -> Theme {
        let mut theme = builtin.load();
        self.theme_overrides.apply(&mut theme);
        theme
    }

    /// Queues the prompt, then starts a turn with the queue head when idle. While signing in, it
    /// takes only the sign-in code. Slash commands forward their own synthesized actions.
    fn handle_submit_prompt(&mut self, text: &str) {
        let idle = self.in_flight.is_none();
        let Some(parsed) = command::parse_slash(text) else {
            if self
                .in_flight
                .as_ref()
                .is_some_and(InFlight::awaits_sign_in_code)
            {
                self.submit_sign_in_code(text);
                return;
            }
            self.queue.push(text.to_owned());
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
        let kind = slash::classify(&parsed);
        if !self.allows_mutating(&parsed.name, kind) {
            return;
        }
        if let Some(action) = self.run_slash(&parsed, text) {
            self.dispatch_action(action);
        }
    }

    /// Sends `text` as the sign-in code, without an echo, when it has the code's `code#state`
    /// shape. Other text goes back to the input with a note, so a prompt typed during sign-in is
    /// neither lost nor sent as a code.
    fn submit_sign_in_code(&mut self, text: &str) {
        let code = text.trim();
        let shaped = !code.contains(char::is_whitespace)
            && code
                .split_once('#')
                .is_some_and(|(code, state)| !code.is_empty() && !state.is_empty());
        if shaped {
            self.dispatch_op(Op::LoginCode(SignInCode(code.to_owned())));
        } else {
            self.input.set_text(text);
            self.chat.push_system_message(
                "Sign-in is waiting for the code the sign-in page shows. \
                 Send other prompts after sign-in ends, or press Esc to cancel it.",
            );
        }
    }

    /// Starts a turn running `prompt` and forwards queued prompts to its round boundaries. A
    /// rejected prompt stays at the queue head with its pending chat echo.
    pub(super) fn start_turn(&mut self, mut prompt: QueuedPrompt) {
        let op = Op::Prompt {
            text: prompt.prompt().to_owned(),
            title: prompt.title().map(str::to_owned),
        };
        let Some((permit, op)) = self.reserve_op(op) else {
            self.queue.push_front(prompt);
            return;
        };
        if let Some(display) = prompt.take_echo() {
            self.chat.push_user_message(display);
        }
        self.queue.set_active(prompt);
        self.begin(&op);
        permit.send(op);
        self.forward_queued_prompts();
    }

    /// Forwards the queued prompts the in-flight turn lacks, in order. A prompt that a full channel
    /// turns away stays queued for a later call, and a closed channel quits. Holds them unless
    /// [`InFlight::takes_queued_prompts`].
    pub(super) fn forward_queued_prompts(&mut self) {
        if !self
            .in_flight
            .as_ref()
            .is_some_and(InFlight::takes_queued_prompts)
        {
            return;
        }
        while let Some(prompt) = self.queue.next_to_forward() {
            match self.ops.try_send(Op::Queue(prompt)) {
                Ok(()) => self.queue.mark_forwarded(),
                Err(TrySendError::Full(_)) => break,
                Err(e) => {
                    self.surface_send_error(&e);
                    break;
                }
            }
        }
    }

    /// Runs `parsed`, the command the user typed as `line`, and applies its outcome, returning the
    /// action it asks the caller to route.
    fn run_slash(&mut self, parsed: &Parsed, line: &str) -> Option<Action> {
        let ctx = SlashContext::new(&self.session_info, self.status_bar.title());
        let outcome = slash::dispatch(parsed, &ctx);
        self.apply_slash_outcome(outcome, line)
    }

    /// Refuses a `Mutating` command while the App is busy, naming it in a chat notice. Returns
    /// whether the command may run.
    fn allows_mutating(&mut self, name: &str, kind: Option<SlashKind>) -> bool {
        if self.in_flight.is_none() || kind != Some(SlashKind::Mutating) {
            return true;
        }
        self.chat.push_system_message(format!(
            "/{name} runs only when idle. Try again after the turn finishes.",
        ));
        false
    }

    /// Applies a slash command's outcome: shows its chat output or modal, starts its background
    /// task, or hands back the action it forwards or the [`Action::SlashPrompt`] that runs a built
    /// prompt under the typed `line`, unapplied. Must run inside a tokio runtime.
    pub(super) fn apply_slash_outcome(
        &mut self,
        outcome: Result<SlashOutcome, String>,
        line: &str,
    ) -> Option<Action> {
        match outcome {
            Ok(SlashOutcome::SystemMessage(text)) => self.chat.push_system_message(text),
            Ok(SlashOutcome::GitDiff(diff)) => self.chat.push_git_diff(diff),
            Ok(SlashOutcome::Forward(action)) => return Some(action),
            Ok(SlashOutcome::Prompt(prompt)) => {
                return Some(Action::SlashPrompt {
                    line: line.to_owned(),
                    prompt,
                });
            }
            Ok(SlashOutcome::Modal(modal)) => self.modals.push(modal),
            Ok(SlashOutcome::Background(task)) => {
                let line = line.to_owned();
                _ = self.slash_tasks.spawn(async move {
                    SlashTask {
                        line,
                        outcome: task.await,
                    }
                });
            }
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
    use crossterm::event::KeyModifiers;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use tokio::sync::mpsc::error::TryRecvError;

    use ox_core::config::ThemeConfig;
    use ox_core::config::provenance::Source;
    use ox_core::model::{Effort, ResolvedModelId};
    use ox_core::permission::defaults::Shipped;
    use ox_core::permission::{Basis, Mode};
    use ox_core::protocol::testing::text_result;
    use ox_core::protocol::{
        ApprovalDecision, CompactTrigger, ConfigPick, Event, ResumeTarget, SavedTarget,
    };
    use ox_core::session::entry::ArchiveState;
    use ox_core::session::saved::{ArchiveUpdate, SavedAction, SavedError};
    use ox_core::session::store::testing::{listed_session, stamped_id};
    use ox_core::tool::view::{ApprovalBody, ApprovalPreview};

    use super::*;
    use crate::app::operation::Activity;
    use crate::app::testing::{
        FakeModal, KeyLogModal, OP_CHANNEL_CAPACITY, activity, cell_of, long_chat_block,
        push_answerable_approval, render_app, rendered_text, start_op, test_app,
    };
    use crate::components::status::StatusHyperlink;
    use crate::modal::acceptance::TYPE_AHEAD_GUARD;
    use crate::modal::testing::ScriptedModal;
    use crate::terminal::testing::{BEGIN_SYNC, END_SYNC, FLUSH, FRAME, capture_terminal};
    use crate::testing::{key_event, mouse_event, mouse_event_at};
    use crate::theme::{ThemeOverrides, builtin, resolve_theme};

    const RED: Color = Color::Rgb(0xff, 0x00, 0x00);

    fn fill_channel(app: &App) -> Vec<Op> {
        (0..OP_CHANNEL_CAPACITY)
            .map(|index| {
                let op = Op::Rename {
                    title: format!("queued-{index}"),
                };
                app.ops.try_send(op.clone()).unwrap();
                op
            })
            .collect()
    }

    fn built_in_theme(name: &str) -> &'static BuiltinTheme {
        builtin::find(name).expect("a built-in theme")
    }

    fn latte() -> &'static BuiltinTheme {
        built_in_theme("latte")
    }

    /// The parsed `[tui.theme.overrides]` entry `text = "#ff0000"`.
    fn red_text_overrides() -> ThemeOverrides {
        let config = ThemeConfig {
            base: "mocha".to_owned(),
            base_source: Source::Default,
            overrides: r##"text = "#ff0000""##.parse().unwrap(),
        };
        resolve_theme(&config).unwrap().overrides
    }

    mod handle_crossterm_event {
        use super::*;

        #[tokio::test]
        async fn buffered_confirmation_keys_leave_the_saved_session_untouched() {
            for code in [KeyCode::Enter, KeyCode::Char('y')] {
                for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
                    let (mut app, mut rx, _event_tx) = test_app(None);
                    let id = stamped_id(0xab);
                    app.input.set_text("/delete abab");
                    app.handle_crossterm_event(&key_event(KeyCode::Enter, KeyModifiers::NONE));
                    assert_eq!(
                        rx.try_recv(),
                        Ok(Op::FindSaved {
                            prefix: "abab".to_owned()
                        })
                    );
                    app.handle_engine_event(Event::SavedFound(Ok(Box::new(listed_session(&id)))));
                    assert!(app.modals.is_active());

                    app.handle_crossterm_event(&CrosstermEvent::Key(KeyEvent::new_with_kind(
                        code,
                        KeyModifiers::NONE,
                        kind,
                    )));

                    assert_eq!(
                        rx.try_recv(),
                        Err(TryRecvError::Empty),
                        "{code:?} {kind:?} deleted an unseen target"
                    );
                    assert!(app.modals.is_active(), "{code:?} {kind:?}");
                }
            }
        }

        /// Double-clicks the word `needle`, which selects and copies it.
        async fn select_word(app: &mut App, needle: &str) {
            let (x, y) = cell_of(render_app(app, 60, 10).buffer(), needle);
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

        #[test]
        fn ignores_a_key_release() {
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

        #[tokio::test(start_paused = true)]
        async fn ctrl_c_copies_the_selection_without_interrupting() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.chat.push_system_message("copy this word");
            start_op(&mut app, &Op::prompt("running"));
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
            assert_eq!(activity(&app), Some(&Activity::Streaming));
        }

        #[tokio::test(start_paused = true)]
        async fn any_other_key_clears_the_selection_and_still_acts() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.chat.push_system_message("copy this word");
            select_word(&mut app, "this").await;

            app.handle_crossterm_event(&key_event(KeyCode::Char('x'), KeyModifiers::NONE));

            assert!(!app.chat.has_selection());
            assert_eq!(app.input.lines(), vec!["x"]);
        }

        #[tokio::test]
        async fn a_modal_takes_keys_before_the_input() {
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

            app.handle_crossterm_event(&key_event(KeyCode::Char('s'), KeyModifiers::NONE));

            let forwarded = rx.try_recv().expect("modal-submitted action forwarded");
            assert!(matches!(forwarded, Op::Interrupt));
        }

        #[test]
        fn a_modal_takes_both_mode_keys_before_the_input() {
            let keys = Arc::new(Mutex::new(Vec::new()));
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.input.set_text("draft");
            app.push_modal(Box::new(KeyLogModal(Arc::clone(&keys))));

            for (code, modifiers) in [
                (KeyCode::Tab, KeyModifiers::NONE),
                (KeyCode::BackTab, KeyModifiers::SHIFT),
            ] {
                app.handle_crossterm_event(&key_event(code, modifiers));
            }

            assert_eq!(*keys.lock().unwrap(), [KeyCode::Tab, KeyCode::BackTab]);
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
            assert_eq!(app.requested_mode, Mode::Manual);
            assert_eq!(app.input.lines(), ["draft"]);
        }

        #[tokio::test]
        async fn the_top_modal_takes_pastes() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("/rename".to_owned()));

            app.handle_crossterm_event(&CrosstermEvent::Paste("Fix auth\r\nflow".to_owned()));
            app.handle_crossterm_event(&key_event(KeyCode::Enter, KeyModifiers::NONE));

            let forwarded = rx.try_recv().expect("rename forwarded");
            assert_eq!(
                forwarded,
                Op::Rename {
                    title: "Fix auth flow".to_owned(),
                },
            );
            assert!(app.input.is_empty(), "the hidden input must stay empty");
        }

        #[test]
        fn a_paste_never_answers_an_approval() {
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
        fn the_wheel_steps_a_modal_and_leaves_the_chat() {
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
        fn cancelling_a_modal_closes_it_without_dispatching() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.push_modal(Box::new(ScriptedModal::new(ModalAction::Cancelled)));

            app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::NONE));

            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "Cancelled must not dispatch any Op"
            );

            app.handle_crossterm_event(&key_event(KeyCode::Char('y'), KeyModifiers::NONE));

            assert_eq!(app.input.lines(), vec!["y".to_owned()]);
        }

        #[test]
        fn ctrl_o_toggles_the_expanded_view_before_the_input_sees_it() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let output = (0..8)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            app.chat
                .push_tool_result(text_result("ls", &output, false, Some("Listed files")));
            app.input.set_text("draft");
            let compact = rendered_text(&mut app, 80, 20);
            app.dirty = false;

            app.handle_crossterm_event(&key_event(KeyCode::Char('o'), KeyModifiers::CONTROL));

            assert!(app.dirty);
            let expanded = rendered_text(&mut app, 80, 20);
            assert!(
                compact.contains("... +3 lines · Ctrl+O to expand"),
                "{compact}"
            );
            assert!(!compact.contains("line 7"), "{compact}");
            assert!(expanded.contains("line 7"), "{expanded}");
            assert_eq!(app.input.lines(), vec!["draft".to_owned()]);
        }

        #[tokio::test(start_paused = true)]
        async fn ctrl_c_while_idle_arms_exit_then_confirms() {
            let (mut app, _rx, _event_tx) = test_app(None);

            app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));

            assert!(!app.should_quit, "first press only arms");
            assert_eq!(app.exit_armed, Some(now() + EXIT_WINDOW));

            tokio::time::advance(EXIT_WINDOW.checked_sub(Duration::from_millis(1)).unwrap()).await;
            app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));

            assert!(app.should_quit, "second press within window confirms");
        }

        #[tokio::test(start_paused = true)]
        async fn ctrl_c_after_the_exit_window_arms_it_again() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));

            tokio::time::advance(EXIT_WINDOW).await;
            app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));

            assert!(!app.should_quit);
            assert_eq!(app.exit_armed, Some(now() + EXIT_WINDOW));
        }

        #[tokio::test]
        async fn ctrl_c_while_busy_interrupts_without_quitting() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            start_op(&mut app, &Op::prompt("running"));

            app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));

            assert!(!app.should_quit, "busy Ctrl+C must not exit");
            assert_eq!(rx.try_recv(), Ok(Op::Interrupt));
        }

        #[test]
        fn ctrl_d_on_an_empty_input_arms_exit_and_quits_only_when_armed() {
            for (label, exit_armed, in_flight, armed, quits) in [
                ("idle", None, None, true, false),
                ("armed", Some(now() + EXIT_WINDOW), None, true, true),
                ("busy", None, Some(Op::prompt("running")), false, false),
            ] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.exit_armed = exit_armed;
                if let Some(op) = in_flight {
                    start_op(&mut app, &op);
                }

                app.handle_crossterm_event(&key_event(KeyCode::Char('d'), KeyModifiers::CONTROL));

                assert_eq!(app.should_quit, quits, "{label}");
                assert_eq!(app.exit_armed.is_some(), armed, "{label}");
                assert_eq!(rx.try_recv().ok(), None, "{label}");
            }
        }

        #[test]
        fn ctrl_d_with_text_deletes_forward_and_stays_idle() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.input.set_text("draft");
            app.handle_crossterm_event(&key_event(KeyCode::Home, KeyModifiers::NONE));

            app.handle_crossterm_event(&key_event(KeyCode::Char('d'), KeyModifiers::CONTROL));

            assert!(!app.should_quit);
            assert_eq!(activity(&app), None);
            assert_eq!(rx.try_recv().ok(), None);
            assert_eq!(app.input.lines(), ["raft"]);
        }

        #[tokio::test]
        async fn esc_while_busy_interrupts() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            start_op(&mut app, &Op::prompt("running"));
            app.handle_engine_event(Event::StreamToken("partial".to_owned()));

            app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));

            assert!(!app.should_quit);
            let forwarded = rx.try_recv().expect("Interrupt forwarded to the engine");
            assert!(matches!(forwarded, Op::Interrupt));
        }

        #[tokio::test]
        async fn enter_submits_through_the_input() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.handle_crossterm_event(&key_event(KeyCode::Char('h'), KeyModifiers::NONE));
            app.handle_crossterm_event(&key_event(KeyCode::Char('i'), KeyModifiers::NONE));

            app.handle_crossterm_event(&key_event(KeyCode::Enter, KeyModifiers::NONE));

            assert!(app.dirty);
            assert_eq!(app.chat.entry_count(), 1);
            assert_eq!(activity(&app), Some(&Activity::Streaming));
            let forwarded = rx.try_recv().ok().unwrap();
            assert!(matches!(forwarded, Op::Prompt { text, title: None } if text == "hi"));
        }

        #[test]
        fn page_up_scrolls_chat_while_idle_and_ctrl_end_jumps_back() {
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
        fn up_scrolls_chat_once_the_input_cursor_reaches_the_top() {
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
        fn popup_arrows_while_busy_leave_chat_scroll_alone() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.chat.push_system_message(long_chat_block());
            _ = render_app(&mut app, 80, 20);
            start_op(&mut app, &Op::prompt("running"));
            app.handle_engine_event(Event::StreamToken("partial".to_owned()));
            app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
            assert!(app.input.popup_visible());

            app.handle_crossterm_event(&key_event(KeyCode::Up, KeyModifiers::NONE));

            assert!(!app.chat.is_scrolled_up(), "the popup consumed Up");
        }

        #[test]
        fn paste_fills_the_input_without_submitting() {
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
        fn popup_enter_dispatches_canonical_command() {
            // `/h` filters to /help, and Enter dispatches it. /help is read-only, so chat lands a
            // system message and the engine receives nothing.
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
            app.handle_crossterm_event(&key_event(KeyCode::Char('h'), KeyModifiers::NONE));
            assert!(app.input.popup_visible());

            app.handle_crossterm_event(&key_event(KeyCode::Enter, KeyModifiers::NONE));

            assert!(!app.input.popup_visible(), "submit clears popup");
            assert_eq!(
                app.chat.last_error_text(),
                None,
                "/help must not push an error"
            );
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "slash command stays client-side"
            );
        }

        #[test]
        fn popup_tab_completes_names_and_arguments_without_switching_modes() {
            for (typed, completed) in [("/h", "/help "), ("/mode p", "/mode plan ")] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.handle_crossterm_event(&CrosstermEvent::Paste(typed.to_owned()));
                assert!(app.input.popup_visible(), "{typed}");

                app.handle_crossterm_event(&key_event(KeyCode::Tab, KeyModifiers::NONE));

                assert!(!app.input.popup_visible(), "{typed}");
                assert_eq!(app.input.lines(), [completed], "{typed}");
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{typed}");
                assert_eq!(app.requested_mode, Mode::Manual, "{typed}");
            }
        }

        #[test]
        fn tab_during_busy_switches_mode_without_queuing_or_editing_the_draft() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            start_op(&mut app, &Op::prompt("running"));
            app.input.set_text("next task");

            app.handle_crossterm_event(&key_event(KeyCode::Tab, KeyModifiers::NONE));

            assert_eq!(rx.try_recv(), Ok(Op::SetMode(Mode::Plan)));
            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
            assert_eq!(app.input.lines(), ["next task"]);
            assert_eq!(activity(&app), Some(&Activity::Streaming));
        }

        #[test]
        fn wheel_scroll_is_forwarded_to_chat() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.chat.push_system_message(long_chat_block());
            _ = render_app(&mut app, 60, 10);

            app.handle_crossterm_event(&mouse_event(MouseEventKind::ScrollUp));

            let text = rendered_text(&mut app, 60, 10);
            assert!(text.contains("Ctrl+End to jump to bottom"), "{text}");
            assert!(app.dirty);
        }

        #[test]
        fn wheel_steps_the_slash_popup_like_arrows() {
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

        #[test]
        fn plain_click_opens_the_status_link_under_it() {
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

        #[tokio::test]
        async fn drag_release_copies_the_selection() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.chat.push_system_message("copy this word");
            let (x, y) = cell_of(render_app(&mut app, 60, 10).buffer(), "this");

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

        #[test]
        fn resize_schedules_dirty_for_relayout() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dirty = false;

            app.handle_crossterm_event(&CrosstermEvent::Resize(80, 24));

            assert!(app.dirty, "Resize must trigger a re-layout render");
        }

        #[test]
        fn unhandled_events_skip_the_rerender() {
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
    }

    mod copy_selection {
        use super::*;

        #[tokio::test]
        async fn replaces_an_earlier_copy_still_waiting_for_the_next_frame() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let mut terminal = capture_terminal(60, 8);
            app.copy_selection("old".to_owned());
            let earlier = app.clipboard.recv().await.unwrap();
            app.apply_copy_report(earlier);

            app.copy_selection("latest".to_owned());

            assert_eq!(app.pending_clipboard, None);
            assert!(app.copy_notice.is_none());

            app.render(&mut terminal).unwrap();
            let latest = app.clipboard.recv().await.unwrap();
            app.apply_copy_report(latest);
            app.render(&mut terminal).unwrap();

            assert_eq!(
                terminal.backend().bytes(),
                [
                    BEGIN_SYNC,
                    FRAME,
                    END_SYNC,
                    FLUSH,
                    BEGIN_SYNC,
                    FRAME,
                    b"\x1b]52;c;bGF0ZXN0\x07",
                    END_SYNC,
                    FLUSH,
                ]
                .concat(),
            );
            let notice = app.copy_notice.as_ref().unwrap();
            assert_eq!(
                (notice.text.as_str(), notice.failed),
                ("Copied 6 characters", false)
            );
        }
    }

    mod apply_modal_action {
        use super::*;

        #[test]
        fn cancel_without_snapshot_is_a_noop() {
            // Modals that never previewed (e.g. /model picker) cancel through the same
            // ModalAction::Cancelled path, so the rollback arm must skip cleanly with no snapshot.
            let (mut app, _rx, _event_tx) = test_app(None);
            let original_text = app.theme.text;

            app.apply_modal_action(ModalAction::Cancelled);

            assert!(app.preview_theme_snapshot.is_none());
            assert_eq!(app.theme.text, original_text);
        }

        #[test]
        fn system_message_action_pushes_into_chat() {
            // Destructive-action modals report success in chat so the user can confirm the action
            // ran.
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
    }

    mod clear_modals {
        use super::*;

        #[test]
        fn resolves_a_pending_approval_to_abort() {
            // A session swap clears the stack while an approval is outstanding. The cancel hook's
            // Abort must reach the engine, or the blocked turn is stranded waiting for a decision.
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
                    decision: ApprovalDecision::Abort,
                } if id == "call-1"
            ));
        }

        #[test]
        fn rolls_back_theme_preview() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let original = app.theme.clone();
            app.push_modal(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
            app.dispatch_action(Action::PreviewTheme(latte()));

            app.clear_modals();

            assert!(!app.modals.is_active());
            assert!(app.preview_theme_snapshot.is_none());
            assert_eq!(app.theme.text, original.text);
        }
    }

    mod handle_esc {
        use super::*;

        #[tokio::test]
        async fn busy_dispatches_interrupt() {
            // Esc is routed by `App` because its meaning depends on queue state and run-state.
            let (mut app, mut rx, _event_tx) = test_app(None);
            start_op(&mut app, &Op::prompt("running"));
            app.handle_engine_event(Event::StreamToken("partial".to_owned()));

            app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));

            assert_eq!(activity(&app), Some(&Activity::Cancelling));
            let forwarded = rx.try_recv().expect("Interrupt forwarded");
            assert!(matches!(forwarded, Op::Interrupt));
        }

        #[test]
        fn idle_with_empty_queue_is_silent() {
            let (mut app, _rx, _event_tx) = test_app(None);

            app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));

            assert_eq!(activity(&app), None);
            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
        }

        #[test]
        fn idle_with_queue_pops_most_recent_into_input() {
            // The most recent prompt (back of FIFO) returns to input, while the rest stay queued.
            let (mut app, _rx, _event_tx) = test_app(None);
            app.queue.push("first".to_owned());
            app.queue.push("second".to_owned());

            app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));

            assert_eq!(app.input.lines(), vec!["second".to_owned()]);
            assert_eq!(*app.queue.pending(), ["first"],);
        }

        #[test]
        fn idle_with_buffer_content_refuses_pop() {
            // Esc must not clobber an in-progress draft.
            let (mut app, _rx, _event_tx) = test_app(None);
            app.queue.push("queued".to_owned());
            app.input.set_text("draft");

            app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));

            assert_eq!(app.input.lines(), vec!["draft".to_owned()]);
            assert_eq!(*app.queue.pending(), ["queued"],);
        }

        #[test]
        fn with_popup_visible_dismisses_popup_and_leaves_queue_intact() {
            // The popup gate precedes Esc routing, so the popup hides and the queue stays put.
            let (mut app, _rx, _event_tx) = test_app(None);
            app.queue.push("queued".to_owned());
            app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
            assert!(app.input.popup_visible(), "/ opens the popup");

            app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));

            assert!(!app.input.popup_visible(), "Esc dismisses the popup");
            assert_eq!(
                *app.queue.pending(),
                ["queued"],
                "queue must not be peeled while popup owns Esc",
            );
        }
    }

    mod dispatch_action {
        use super::*;

        #[tokio::test]
        async fn submit_prompt_updates_chat_status_and_forwards_action() {
            let (mut app, mut rx, _event_tx) = test_app(None);

            app.dispatch_action(Action::Submit("hello".to_owned()));

            assert_eq!(app.chat.entry_count(), 1);
            assert_eq!(activity(&app), Some(&Activity::Streaming));
            assert!(!app.should_quit);
            let forwarded = rx.try_recv().expect("forwarded action");
            assert!(matches!(forwarded, Op::Prompt { text, title: None } if text == "hello"));
        }

        #[test]
        fn slash_command_renders_locally_without_forwarding() {
            let (mut app, mut rx, _event_tx) = test_app(None);

            app.dispatch_action(Action::Submit("/help".to_owned()));

            assert!(app.modals.is_active(), "/help opens a modal");
            assert_eq!(
                app.chat.entry_count(),
                0,
                "modal-only commands push no chat blocks and suppress their own echo",
            );
            assert_eq!(activity(&app), None);
            assert_eq!(app.chat.last_error_text(), None);
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[test]
        fn unknown_slash_command_renders_error_without_forwarding() {
            let (mut app, mut rx, _event_tx) = test_app(None);

            app.dispatch_action(Action::Submit("/no-such".to_owned()));

            assert_eq!(app.chat.entry_count(), 2);
            assert!(app.in_flight.is_none());
            assert_eq!(
                app.chat.last_error_text(),
                Some(
                    "Unknown command: `/no-such`. \
                     Available: /archive, /clear, /compact, /config, /delete, /diff, \
                     /effort, /exit, /help, /init, /login, /logout, /mode, /model, /rename, \
                     /resume, /status, /theme, /unarchive. \
                     Use `//no-such` to send the literal text."
                ),
            );
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[tokio::test]
        async fn double_slash_escapes_command_and_forwards_literal() {
            let (mut app, mut rx, _event_tx) = test_app(None);

            app.dispatch_action(Action::Submit("//etc/hosts".to_owned()));

            assert_eq!(app.chat.entry_count(), 1, "only the user message");
            assert_eq!(activity(&app), Some(&Activity::Streaming));
            assert_eq!(app.chat.last_error_text(), None);
            let forwarded = rx.try_recv().expect("forwarded action");
            assert!(matches!(
                forwarded,
                Op::Prompt { text, title: None } if text == "/etc/hosts",
            ));
            assert_eq!(
                app.queue.active().map(QueuedPrompt::line),
                Some("//etc/hosts"),
                "kept as typed, so a failed auto-compaction hands back the escape",
            );
            let text = rendered_text(&mut app, 60, 10);
            assert!(
                text.contains("/etc/hosts") && !text.contains("//etc"),
                "{text}"
            );
        }

        #[test]
        fn closed_channel_quits() {
            // Dropping `op_rx` simulates a stopped engine.
            let (mut app, rx, _event_tx) = test_app(None);
            drop(rx);

            app.dispatch_action(Action::Submit("hi".to_owned()));

            assert!(app.should_quit, "closed channel must trigger teardown");
            assert_eq!(app.chat.last_error_text(), None);
            assert_eq!(activity(&app), None);
            assert_eq!(app.queue.active(), None);
            assert_eq!(*app.queue.pending(), ["hi"]);
        }

        #[test]
        fn full_channel_keeps_prompts_for_one_ordered_retry() {
            for (command, line, title) in [
                ("//literal-command", "//literal-command", None),
                (
                    "/init AGENTS.md",
                    "/init AGENTS.md",
                    Some("/init AGENTS.md"),
                ),
                ("/init", "/init AGENTS.md", Some("/init AGENTS.md")),
            ] {
                let root = tempfile::tempdir().unwrap();
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.session_info.project_root = root.path().to_owned();
                let prompt = title.map_or_else(
                    || "/literal-command".to_owned(),
                    |_| command::init::create_prompt(root.path(), "AGENTS.md"),
                );
                let buffered = fill_channel(&app);

                app.dispatch_action(Action::Submit(command.to_owned()));
                if command == "/init" {
                    assert!(app.modals.is_active());
                    app.handle_crossterm_event(&key_event(KeyCode::Enter, KeyModifiers::NONE));
                }
                app.dispatch_action(Action::Submit("next prompt".to_owned()));

                assert!(!app.should_quit, "{command}");
                assert_eq!(activity(&app), None, "{command}");
                assert_eq!(app.queue.active(), None, "{command}");
                assert_eq!(*app.queue.pending(), [line, "next prompt"], "{command}");
                assert_eq!(app.queue.pending()[0].prompt(), prompt, "{command}");
                assert_eq!(app.queue.pending()[0].title(), title, "{command}");
                assert_eq!(app.queue.forwarded(), 0, "{command}");
                assert_eq!(app.chat.entry_count(), 2 + usize::from(title.is_some()));
                assert_eq!(
                    app.chat.last_error_text(),
                    Some(
                        "ox is busy, so the prompt stays queued. Press Esc to edit it, \
                         or submit another prompt to retry."
                    ),
                    "{command}",
                );
                for op in buffered {
                    assert_eq!(rx.try_recv(), Ok(op), "{command}");
                }
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{command}");

                std::fs::write(root.path().join("AGENTS.md"), "# Instructions\n").unwrap();

                app.dispatch_action(Action::Submit("retry prompt".to_owned()));

                assert_eq!(activity(&app), Some(&Activity::Streaming), "{command}");
                assert_eq!(app.queue.active().map(QueuedPrompt::line), Some(line));
                assert_eq!(
                    rx.try_recv(),
                    Ok(Op::Prompt {
                        text: prompt,
                        title: title.map(str::to_owned),
                    }),
                    "{command}",
                );
                assert_eq!(rx.try_recv(), Ok(Op::Queue("next prompt".to_owned())));
                assert_eq!(rx.try_recv(), Ok(Op::Queue("retry prompt".to_owned())));
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{command}");
                let text = rendered_text(&mut app, 100, 20);
                assert_eq!(
                    text.matches(command.trim_start_matches('/')).count(),
                    1,
                    "{text}"
                );

                app.handle_engine_event(Event::PromptDrained("next prompt".to_owned()));
                app.handle_engine_event(Event::PromptDrained("retry prompt".to_owned()));
                app.handle_engine_event(Event::TurnComplete);

                assert_eq!(activity(&app), None, "{command}");
                assert_eq!(app.queue.active(), None, "{command}");
                assert_eq!(*app.queue.pending(), [] as [&str; 0], "{command}");
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{command}");
            }
        }

        #[test]
        fn rejected_slash_prompt_echo_survives_forwarding_and_compaction() {
            for compacted in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.session_info.project_root = root.path().to_owned();
                let buffered = fill_channel(&app);
                let queued = command::init::create_prompt(root.path(), "AGENTS.md");
                app.dispatch_action(Action::Submit("/init AGENTS.md".to_owned()));
                for op in buffered {
                    assert_eq!(rx.try_recv(), Ok(op));
                }

                app.dispatch_action(Action::Submit("/init CLAUDE.md".to_owned()));

                assert_eq!(
                    rx.try_recv(),
                    Ok(Op::Prompt {
                        text: command::init::create_prompt(root.path(), "CLAUDE.md"),
                        title: Some("/init CLAUDE.md".to_owned()),
                    }),
                );
                assert_eq!(rx.try_recv(), Ok(Op::Queue(queued.clone())));

                if compacted {
                    app.handle_engine_event(Event::SessionCompacted {
                        summary: "Earlier work".to_owned(),
                        pre_count: 2,
                        instructions: None,
                        trigger: CompactTrigger::Auto,
                    });
                }
                app.handle_engine_event(Event::PromptDrained(queued));

                assert_eq!(*app.queue.pending(), [] as [&str; 0]);
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
                let text = rendered_text(&mut app, 100, 20);
                assert_eq!(text.matches("/init AGENTS.md").count(), 1, "{text}");
                assert_eq!(text.matches("/init CLAUDE.md").count(), 1, "{text}");
            }
        }

        #[test]
        fn full_channel_on_approval_points_to_the_cancel_recovery() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            for _ in 1..OP_CHANNEL_CAPACITY {
                app.dispatch_action(Action::Op(Op::Interrupt));
            }

            app.dispatch_action(Action::Op(Op::Resolve {
                id: "call-1".to_owned(),
                decision: ApprovalDecision::Approve,
            }));

            assert!(!app.should_quit, "Full is not fatal");
            assert_eq!(
                app.chat.last_error_text(),
                Some("Approval could not be delivered. Press Esc to cancel the blocked turn."),
            );
        }

        #[tokio::test]
        async fn submit_during_busy_queues_and_forwards_for_mid_turn_drain() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("first".to_owned()));
            rx.try_recv().expect("first submit forwarded");

            app.dispatch_action(Action::Submit("queued".to_owned()));

            // Buffered for preview and forwarded so the turn can splice it in at its next round
            // boundary. Chat stays untouched until `PromptDrained` lands.
            assert_eq!(*app.queue.pending(), ["queued"],);
            assert_eq!(activity(&app), Some(&Activity::Streaming));
            let forwarded = rx.try_recv().expect("queued submit forwarded");
            assert_eq!(forwarded, Op::Queue("queued".to_owned()));
        }

        #[tokio::test]
        async fn submit_during_cancelling_holds_locally_without_forwarding() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");
            app.dispatch_action(Action::Op(Op::Interrupt));
            rx.try_recv().expect("cancel forwarded");
            assert_eq!(activity(&app), Some(&Activity::Cancelling));

            app.dispatch_action(Action::Submit("typed-during-cancel".to_owned()));

            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "submit during cancel must not reach the engine"
            );
            assert_eq!(*app.queue.pending(), ["typed-during-cancel"],);
        }

        #[tokio::test]
        async fn submit_during_compacting_holds_locally_without_forwarding() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Op(Op::Compact { instructions: None }));
            rx.try_recv().expect("compact forwarded");
            assert_eq!(activity(&app), Some(&Activity::Compacting));

            app.dispatch_action(Action::Submit("typed-during-compact".to_owned()));

            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "submit during compact must wait for SessionCompacted to drain it"
            );
            assert_eq!(*app.queue.pending(), ["typed-during-compact"],);
        }

        #[tokio::test]
        async fn read_only_slash_during_busy_runs_client_side_without_queueing() {
            // Read-only slash commands run client-side immediately during a busy turn, since
            // queueing them would make the LLM answer `/help` literally on drain.
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");
            assert_eq!(activity(&app), Some(&Activity::Streaming));

            app.dispatch_action(Action::Submit("/help".to_owned()));

            assert_eq!(
                *app.queue.pending(),
                [] as [&str; 0],
                "slash command must not queue"
            );
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "slash command must not forward to ops"
            );
            // Only the original "active" prompt: /help is modal-only and suppresses its own echo.
            assert_eq!(app.chat.entry_count(), 1);
            assert_eq!(app.chat.last_error_text(), None);
            assert!(app.modals.is_active(), "/help opens a modal");
        }

        #[tokio::test]
        async fn clear_during_busy_refuses_with_system_message_no_dispatch() {
            // State-mutating commands refuse mid-turn, since rolling while `messages` is still
            // draining would race the in-flight turn into the wrong JSONL.
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");

            app.dispatch_action(Action::Submit("/clear".to_owned()));

            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
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
        async fn init_forwards_synthesized_prompt_and_flips_to_streaming() {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("AGENTS.md"), "# AGENTS.md\n").unwrap();
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.session_info.project_root = root.path().to_owned();

            app.dispatch_action(Action::Submit("/init".to_owned()));

            assert_eq!(app.chat.entry_count(), 1, "only the typed `/init` line");
            assert_eq!(activity(&app), Some(&Activity::Streaming));
            assert_eq!(
                app.queue.active().map(QueuedPrompt::line),
                Some("/init"),
                "the line auto-compaction replays"
            );
            let forwarded = rx.try_recv().expect("synthesized prompt forwarded");
            assert!(
                matches!(
                    &forwarded,
                    Op::Prompt { text, title: Some(title) }
                        if title == "/init"
                            && text.contains("- `AGENTS.md`")
                            && text.contains("unified diff")
                ),
                "expected the update prompt, got {forwarded:?}",
            );
        }

        #[tokio::test]
        async fn init_without_an_instruction_file_starts_the_turn_from_the_picker() {
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
            assert_eq!(activity(&app), Some(&Activity::Streaming));
            assert_eq!(
                app.queue.active().map(QueuedPrompt::line),
                Some("/init CLAUDE.md"),
                "the line auto-compaction replays"
            );
            let forwarded = rx.try_recv().expect("create prompt forwarded");
            assert!(
                matches!(
                    &forwarded,
                    Op::Prompt { text, title: Some(title) }
                        if title == "/init CLAUDE.md" && text.contains("create `CLAUDE.md`")
                ),
                "expected the create prompt for the second file, got {forwarded:?}",
            );
        }

        #[test]
        fn init_picker_cancel_keeps_the_typed_command_in_chat() {
            let root = tempfile::tempdir().unwrap();
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.session_info.project_root = root.path().to_owned();
            app.dispatch_action(Action::Submit("/init".to_owned()));
            assert!(app.modals.is_active());

            app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));

            assert!(!app.modals.is_active());
            assert_eq!(activity(&app), None);
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
            let text = rendered_text(&mut app, 80, 10);
            assert_eq!(text.matches("/init").count(), 1, "{text}");
        }

        #[tokio::test]
        async fn init_during_busy_refuses_with_system_message_no_forward() {
            // Mutating: refuse. The typed `/init` row lands, but its synthesized body doesn't.
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");

            app.dispatch_action(Action::Submit("/init".to_owned()));

            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
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
        async fn arg_bearing_slash_during_busy_refuses_locally() {
            for (cmd, gate_phrase) in [
                ("/model opus", "/model runs only when idle"),
                ("/effort xhigh", "/effort runs only when idle"),
            ] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.dispatch_action(Action::Submit("active".to_owned()));
                rx.try_recv().expect("active submit forwarded");

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
        async fn config_modal_slash_during_busy_refuses_locally() {
            for (cmd, gate_phrase) in [
                ("/model", "/model runs only when idle"),
                ("/effort", "/effort runs only when idle"),
            ] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.dispatch_action(Action::Submit("active".to_owned()));
                rx.try_recv().expect("active submit forwarded");

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
        async fn unknown_slash_during_busy_renders_error_no_queue() {
            // An unknown command routes through `dispatch`, which shows its error with recovery
            // hints, such as alternatives and the `//` escape, and never sends it to the model.
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");

            app.dispatch_action(Action::Submit("/nope".to_owned()));

            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
            assert_eq!(
                app.chat.last_error_text(),
                Some(
                    "Unknown command: `/nope`. \
                     Available: /archive, /clear, /compact, /config, /delete, /diff, \
                     /effort, /exit, /help, /init, /login, /logout, /mode, /model, /rename, \
                     /resume, /status, /theme, /unarchive. \
                     Use `//nope` to send the literal text."
                ),
            );
        }

        #[tokio::test]
        async fn mode_during_busy_reaches_the_engine_mid_turn() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");

            app.dispatch_action(Action::Submit("/mode plan".to_owned()));
            app.handle_crossterm_event(&key_event(KeyCode::BackTab, KeyModifiers::SHIFT));
            app.dispatch_action(Action::Submit("/mode".to_owned()));

            assert_eq!(rx.try_recv().ok(), Some(Op::SetMode(Mode::Plan)));
            assert_eq!(rx.try_recv().ok(), Some(Op::SetMode(Mode::Manual)));
            assert!(app.modals.is_active(), "the bare form opens the picker");
            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
        }

        #[tokio::test]
        async fn exit_during_busy_quits_and_shuts_the_engine_down() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.recv().await.expect("active submit forwarded");

            app.dispatch_action(Action::Submit("/quit".to_owned()));

            assert!(app.should_quit);
            assert_eq!(rx.try_recv().ok(), Some(Op::Shutdown));
        }
    }

    mod dispatch_op {
        use super::*;

        #[tokio::test]
        async fn interrupt_flips_status_to_cancelling_and_forwards() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("hi".to_owned()));
            rx.try_recv().expect("submit forwarded");
            let entries_before = app.chat.entry_count();

            app.dispatch_action(Action::Op(Op::Interrupt));

            assert_eq!(app.chat.entry_count(), entries_before, "no new chat entry");
            assert_eq!(activity(&app), Some(&Activity::Cancelling));
            assert!(!app.should_quit);
            let forwarded = rx.try_recv().expect("Interrupt forwarded");
            assert!(matches!(forwarded, Op::Interrupt));
        }

        #[test]
        fn shutdown_sets_should_quit_and_leaves_chat_untouched() {
            let (mut app, _rx, _event_tx) = test_app(None);

            app.dispatch_action(Action::Op(Op::Shutdown));

            assert!(app.should_quit);
            assert_eq!(app.chat.entry_count(), 0);
            assert_eq!(activity(&app), None);
        }

        #[test]
        fn shutdown_quits_when_delivery_is_full_or_closed() {
            for closed in [false, true] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                let buffered = fill_channel(&app);
                if closed {
                    rx.close();
                }

                app.dispatch_op(Op::Shutdown);

                assert!(app.should_quit, "closed={closed}");
                assert_eq!(activity(&app), None, "closed={closed}");
                for op in buffered {
                    assert_eq!(rx.try_recv(), Ok(op), "closed={closed}");
                }
                assert_eq!(
                    rx.try_recv(),
                    Err(if closed {
                        TryRecvError::Disconnected
                    } else {
                        TryRecvError::Empty
                    }),
                    "closed={closed}",
                );
            }
        }

        #[tokio::test]
        async fn resume_forwards_to_the_engine_and_stays_busy_until_event() {
            // Between forwarding `Resume` and the `SessionResumed` event, a submit only queues, so
            // a typed prompt doesn't push into chat just before `apply_session_resumed` clears it.
            let (mut app, mut rx, _event_tx) = test_app(None);
            let action = Op::Resume {
                target: ResumeTarget::Path("resume-target.jsonl".into()),
            };

            app.dispatch_op(action.clone());

            let forwarded = rx.try_recv().expect("Resume must reach the engine");
            assert_eq!(forwarded, action);
            assert_eq!(activity(&app), Some(&Activity::Resuming));
        }

        #[tokio::test]
        async fn compact_forwards_to_the_engine_and_stays_busy_until_event() {
            // Mirrors the Resume gate: `apply_session_compacted` is about to wipe the chat, so a
            // submit only queues until the compaction ends.
            let (mut app, mut rx, _event_tx) = test_app(None);
            let action = Op::Compact {
                instructions: Some("focus".to_owned()),
            };

            app.dispatch_op(action.clone());

            let forwarded = rx.try_recv().expect("Compact must reach the engine");
            assert_eq!(forwarded, action);
            assert_eq!(activity(&app), Some(&Activity::Compacting));
        }

        #[tokio::test]
        async fn login_goes_busy() {
            let (mut app, mut rx, _event_tx) = test_app(None);

            app.dispatch_op(Op::Login);

            assert_eq!(rx.try_recv().ok(), Some(Op::Login));
            assert_eq!(activity(&app), Some(&Activity::SigningIn));
        }

        #[tokio::test]
        async fn a_switch_holds_prompts_typed_during_it_for_the_new_session() {
            let landed = |id: &str| Event::SessionResumed {
                id: id.to_owned(),
                title: None,
                prior: ArchiveState::Active,
                history: Vec::new(),
            };
            let rolled = |id: &str, old_session| Event::SessionRolled {
                id: id.to_owned(),
                old_session,
            };
            for (command, switched, swapped) in [
                (
                    "/clear",
                    Activity::Clearing,
                    rolled("rolled-session", ArchiveState::Active),
                ),
                (
                    "/archive",
                    Activity::Archiving,
                    rolled("rolled-session", ArchiveState::Archived),
                ),
                (
                    "/resume abcd",
                    Activity::Resuming,
                    landed("resumed-session"),
                ),
            ] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.queue.push("left by a cancel".to_owned());
                app.dispatch_action(Action::Submit(command.to_owned()));
                rx.try_recv().expect("switch forwarded");

                app.dispatch_action(Action::Submit("after the switch".to_owned()));

                assert_eq!(activity(&app), Some(&switched), "{command}");
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{command}");

                app.handle_engine_event(swapped);
                app.handle_engine_event(Event::SessionReady);

                assert_eq!(activity(&app), Some(&Activity::Streaming), "{command}");
                assert_eq!(
                    rx.try_recv().ok(),
                    Some(Op::prompt("after the switch")),
                    "{command}",
                );
                let text = rendered_text(&mut app, 80, 12);
                assert!(
                    text.contains("Discarded 1 queued prompt from the previous session"),
                    "{command}: {text}"
                );
            }
        }

        #[tokio::test]
        async fn a_saved_session_delete_holds_prompts_typed_during_its_lookup_and_delete() {
            let delete = Op::DeleteSaved {
                session_id: "saved".to_owned(),
            };
            for (action, busy, sent) in [
                (
                    Action::Submit("/delete abcd".to_owned()),
                    Activity::Searching,
                    Op::FindSaved {
                        prefix: "abcd".to_owned(),
                    },
                ),
                (
                    Action::Op(delete.clone()),
                    Activity::Deleting,
                    delete.clone(),
                ),
            ] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.dispatch_action(action);
                assert_eq!(rx.try_recv(), Ok(sent), "{busy:?}");

                app.dispatch_action(Action::Submit("typed meanwhile".to_owned()));

                assert_eq!(activity(&app), Some(&busy), "{busy:?}");
                assert_eq!(*app.queue.pending(), ["typed meanwhile"], "{busy:?}");
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{busy:?}");
            }
        }

        #[tokio::test]
        async fn forwards_engine_ops_unchanged() {
            // A modal-emitted `SetModel` must reach the engine so it can emit `ConfigChanged`.
            for op in [
                Op::SetModel(ConfigPick::Model {
                    id: ResolvedModelId::new("claude-opus-5-5"),
                    effort: None,
                }),
                Op::SetModel(ConfigPick::Effort(Effort::High)),
            ] {
                let (mut app, mut rx, _event_tx) = test_app(None);

                app.dispatch_op(op.clone());

                let forwarded = rx.try_recv().expect("op forwarded to the engine");
                assert_eq!(forwarded, op);
                assert!(!app.should_quit);
                assert_eq!(app.chat.entry_count(), 0);
            }
        }

        #[test]
        fn saved_archive_retains_ownership_and_reports_after_the_picker_closes() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            let modal = ScriptedModal::new(ModalAction::Cancelled);
            let observed = Arc::clone(&modal.saved_events);
            app.push_modal(Box::new(modal));
            let target = SavedTarget::Prefix("saved".to_owned());
            let op = Op::SetArchive {
                target: target.clone(),
                state: ArchiveState::Archived,
            };
            let buffered = fill_channel(&app);

            app.dispatch_op(op.clone());

            assert_eq!(activity(&app), None);
            assert_eq!(*observed.lock().unwrap(), [] as [String; 0]);
            for op in buffered {
                assert_eq!(rx.try_recv(), Ok(op));
            }

            app.dispatch_op(op.clone());

            assert_eq!(rx.try_recv(), Ok(op.clone()));
            assert_eq!(activity(&app), Some(&Activity::Archiving));
            let started = r#"started:Prefix("saved")"#;
            assert_eq!(*observed.lock().unwrap(), [started]);

            app.handle_engine_event(Event::ArchiveUpdated {
                target: target.clone(),
                state: ArchiveState::Archived,
                result: Ok(ArchiveUpdate {
                    id: "saved".to_owned(),
                    changed: true,
                }),
            });

            assert_eq!(activity(&app), None);
            let completed = r#"completed:Prefix("saved"):Archived:Ok(ArchiveUpdate { id: "saved", changed: true })"#;
            assert_eq!(*observed.lock().unwrap(), [started, completed]);
            assert_eq!(
                app.chat.last_system_text(),
                Some(
                    ox_core::session::display::archive_notice("saved", ArchiveState::Archived)
                        .as_str()
                )
            );

            app.dispatch_op(op.clone());
            assert_eq!(rx.try_recv(), Ok(op));
            app.modals.clear();
            app.handle_engine_event(Event::ArchiveUpdated {
                target,
                state: ArchiveState::Archived,
                result: Err(SavedError::Failed {
                    action: SavedAction::Archive,
                    id: "saved".to_owned(),
                    cause: "disk full".to_owned(),
                }),
            });

            assert_eq!(activity(&app), None);
            assert_eq!(
                app.chat.last_error_text(),
                Some("/archive: Failed to archive session saved: disk full")
            );
            assert_eq!(*observed.lock().unwrap(), [started, completed, started]);
        }

        #[test]
        fn drops_an_idle_only_op_while_another_operation_runs() {
            for (op, running) in [
                (
                    Op::Resume {
                        target: ResumeTarget::Id("saved".to_owned()),
                    },
                    Op::DeleteSaved {
                        session_id: "other".to_owned(),
                    },
                ),
                (
                    Op::SetArchive {
                        target: SavedTarget::Id("saved".to_owned()),
                        state: ArchiveState::Archived,
                    },
                    Op::Rename {
                        title: "Renamed".to_owned(),
                    },
                ),
                (
                    Op::DeleteSaved {
                        session_id: "saved".to_owned(),
                    },
                    Op::prompt("running"),
                ),
            ] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                start_op(&mut app, &running);
                let busy = activity(&app).cloned();

                app.dispatch_op(op.clone());

                assert_eq!(activity(&app).cloned(), busy, "{op:?}");
                assert_eq!(app.chat.last_error_text(), Some(BUSY_DROPPED), "{op:?}");
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{op:?}");
            }
        }

        #[test]
        fn full_channel_preserves_state_until_delivery_can_be_reserved() {
            let running = Some(Op::prompt("running"));
            for (op, before, after) in [
                (Op::Interrupt, running, Some(Activity::Cancelling)),
                (Op::Clear, None, Some(Activity::Clearing)),
                (
                    Op::Resume {
                        target: ResumeTarget::Prefix("saved-session".to_owned()),
                    },
                    None,
                    Some(Activity::Resuming),
                ),
                (
                    Op::Compact {
                        instructions: Some("focus on the API".to_owned()),
                    },
                    None,
                    Some(Activity::Compacting),
                ),
                (Op::Login, None, Some(Activity::SigningIn)),
                (Op::Logout, None, Some(Activity::SigningOut)),
                (Op::Archive, None, Some(Activity::Archiving)),
                (
                    Op::FindSaved {
                        prefix: "saved".to_owned(),
                    },
                    None,
                    Some(Activity::Searching),
                ),
                (
                    Op::DeleteSaved {
                        session_id: "saved".to_owned(),
                    },
                    None,
                    Some(Activity::Deleting),
                ),
                (
                    Op::Rename {
                        title: "Renamed".to_owned(),
                    },
                    None,
                    Some(Activity::Renaming),
                ),
                (
                    Op::SetModel(ConfigPick::Effort(Effort::High)),
                    None,
                    Some(Activity::UpdatingModel),
                ),
                (Op::SetMode(Mode::Plan), None, None),
            ] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                if let Some(running) = &before {
                    start_op(&mut app, running);
                }
                let before = activity(&app).cloned();
                app.requested_mode = Mode::Manual;
                app.queue.push("old session".to_owned());
                app.queue.mark_switch();
                app.queue.push("current session".to_owned());
                let buffered = fill_channel(&app);

                app.dispatch_op(op.clone());

                assert!(!app.should_quit, "{op:?}");
                assert_eq!(activity(&app).cloned(), before, "{op:?}");
                assert_eq!(app.requested_mode, Mode::Manual, "{op:?}");
                assert_eq!(
                    app.chat.last_error_text(),
                    Some("ox is busy, so the request was dropped. Try again."),
                    "{op:?}",
                );
                app.queue.discard_left_behind();
                assert_eq!(*app.queue.pending(), ["current session"], "{op:?}");
                for buffered_op in buffered {
                    assert_eq!(rx.try_recv(), Ok(buffered_op), "{op:?}");
                }
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{op:?}");

                app.dispatch_op(op.clone());

                assert_eq!(activity(&app).cloned(), after, "{op:?}");
                let mode = if let Op::SetMode(mode) = &op {
                    *mode
                } else {
                    Mode::Manual
                };
                assert_eq!(app.requested_mode, mode, "{op:?}");
                assert_eq!(rx.try_recv(), Ok(op.clone()), "{op:?}");
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{op:?}");
            }
        }

        #[tokio::test(start_paused = true)]
        async fn a_dropped_delete_leaves_the_confirm_modal_taking_y_again() {
            for full in [false, true] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.handle_engine_event(Event::SavedFound(Ok(Box::new(listed_session("saved")))));
                render_app(&mut app, 60, 12);
                tokio::time::advance(TYPE_AHEAD_GUARD).await;
                let buffered = if full {
                    fill_channel(&app)
                } else {
                    app.set_status(Status::Archiving);
                    Vec::new()
                };
                let y = key_event(KeyCode::Char('y'), KeyModifiers::NONE);

                app.handle_crossterm_event(&y);

                assert_eq!(
                    app.chat.last_error_text(),
                    Some(BUSY_DROPPED),
                    "full={full}"
                );
                for op in buffered {
                    assert_eq!(rx.try_recv(), Ok(op), "full={full}");
                }
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "full={full}");

                app.set_status(Status::Idle);
                app.handle_crossterm_event(&y);

                let delete = Op::DeleteSaved {
                    session_id: "saved".to_owned(),
                };
                assert_eq!(rx.try_recv(), Ok(delete), "full={full}");
                assert_eq!(app.status, Status::Deleting, "full={full}");

                let entries = app.chat.entry_count();
                app.handle_crossterm_event(&y);

                assert_eq!(app.chat.entry_count(), entries, "full={full}");
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "full={full}");
            }
        }
    }

    mod cycle_mode {
        use super::*;

        #[test]
        fn tab_cycles_when_a_slash_command_has_no_completion() {
            for draft in ["/no-such-command", "/compact focus"] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.handle_crossterm_event(&CrosstermEvent::Paste(draft.to_owned()));
                assert!(!app.input.popup_visible(), "{draft:?}");

                app.handle_crossterm_event(&key_event(KeyCode::Tab, KeyModifiers::NONE));

                assert_eq!(rx.try_recv(), Ok(Op::SetMode(Mode::Plan)), "{draft:?}");
                assert_eq!(app.input.lines(), [draft], "{draft:?}");
            }
        }

        #[test]
        fn shift_tab_steps_back_while_a_completion_stays_open() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Op(Op::SetMode(Mode::Plan)));
            assert_eq!(rx.try_recv(), Ok(Op::SetMode(Mode::Plan)));
            app.handle_crossterm_event(&CrosstermEvent::Paste("/mode p".to_owned()));
            assert!(app.input.popup_visible());

            app.handle_crossterm_event(&key_event(KeyCode::BackTab, KeyModifiers::SHIFT));

            assert_eq!(rx.try_recv(), Ok(Op::SetMode(Mode::Manual)));
            assert_eq!(app.input.lines(), ["/mode p"]);
            assert!(app.input.popup_visible());
        }

        #[test]
        fn both_keys_step_the_mode_and_an_unconfirmed_press_advances() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.input.set_text("draft\nsecond line");

            app.handle_crossterm_event(&key_event(KeyCode::Tab, KeyModifiers::NONE));
            assert_eq!(rx.try_recv(), Ok(Op::SetMode(Mode::Plan)));
            assert_eq!(app.input.lines(), ["draft", "second line"]);

            app.handle_crossterm_event(&key_event(KeyCode::Tab, KeyModifiers::NONE));
            assert_eq!(rx.try_recv(), Ok(Op::SetMode(Mode::Yolo)));

            app.handle_crossterm_event(&key_event(KeyCode::BackTab, KeyModifiers::SHIFT));
            assert_eq!(rx.try_recv(), Ok(Op::SetMode(Mode::Plan)));

            app.handle_crossterm_event(&key_event(KeyCode::BackTab, KeyModifiers::SHIFT));
            assert_eq!(rx.try_recv(), Ok(Op::SetMode(Mode::Manual)));

            app.handle_crossterm_event(&key_event(KeyCode::BackTab, KeyModifiers::SHIFT));
            assert_eq!(rx.try_recv(), Ok(Op::SetMode(Mode::Yolo)));
        }
    }

    mod expire_armed_exit {
        use super::*;

        #[test]
        fn becomes_idle_once_the_window_ends() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
            let until = app.exit_armed.expect("Ctrl+C arms the exit window");
            let before = until.checked_sub(Duration::from_millis(1)).unwrap();

            let expired_early = app.expire_armed_exit(before);

            assert!(!expired_early);
            assert_eq!(app.exit_armed, Some(until));

            let expired = app.expire_armed_exit(until);

            assert!(expired);
            assert_eq!(app.exit_armed, None);
        }

        #[test]
        fn when_not_armed_leaves_the_status() {
            let (mut app, _rx, _event_tx) = test_app(None);
            start_op(&mut app, &Op::prompt("running"));

            let expired = app.expire_armed_exit(Instant::now());

            assert!(!expired);
            assert_eq!(activity(&app), Some(&Activity::Streaming));
        }
    }

    mod preview_theme {
        use super::*;

        #[test]
        fn repaints_components_and_caches_original() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let original = app.theme.clone();

            app.dispatch_action(Action::PreviewTheme(latte()));

            assert!(
                app.preview_theme_snapshot.is_some(),
                "first PreviewTheme must cache the original",
            );
            assert_ne!(app.theme.text, original.text, "live theme must change");
            assert_eq!(
                app.session_info.config.theme_name, "mocha",
                "a preview keeps the theme name until commit",
            );
        }

        #[test]
        fn then_modal_cancel_restores_the_theme_from_before_the_first_preview() {
            // Esc / Ctrl+C surface as ModalAction::Cancelled, so the snapshot must roll back.
            let (mut app, _rx, _event_tx) = test_app(None);
            let original = app.theme.clone();
            app.dispatch_action(Action::PreviewTheme(latte()));
            app.dispatch_action(Action::PreviewTheme(built_in_theme("frappe")));

            app.apply_modal_action(ModalAction::Cancelled);

            assert!(
                app.preview_theme_snapshot.is_none(),
                "snapshot consumed on cancel",
            );
            assert_eq!(app.theme.text, original.text, "theme rolled back to mocha");
        }

        #[test]
        fn keeps_the_configured_overrides() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.theme_overrides = red_text_overrides();

            app.dispatch_action(Action::PreviewTheme(latte()));

            assert_eq!(app.theme.text.fg, Some(RED));
            assert_ne!(app.theme.accent, Theme::default().accent, "latte applies");
        }
    }

    mod swap_theme {
        use super::*;

        #[test]
        fn commits_and_clears_snapshot() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::PreviewTheme(latte()));
            let previewed = app.theme.clone();

            app.dispatch_action(Action::SwapTheme(latte()));

            assert!(app.preview_theme_snapshot.is_none());
            assert_eq!(app.session_info.config.theme_name, "latte");
            assert_eq!(app.session_info.config.provenance.theme, Source::Session);
            assert_eq!(app.theme.text, previewed.text);

            app.apply_modal_action(ModalAction::Cancelled);

            assert_eq!(
                app.session_info.config.theme_name, "latte",
                "post-commit cancel must not restore the pre-preview theme",
            );
        }

        #[test]
        fn keeps_the_configured_overrides_away_and_back() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.theme_overrides = red_text_overrides();

            app.dispatch_action(Action::SwapTheme(latte()));
            let away = app.theme.text.fg;
            app.dispatch_action(Action::SwapTheme(built_in_theme("mocha")));

            assert_eq!(away, Some(RED));
            assert_eq!(app.theme.text.fg, Some(RED));
            assert_eq!(app.theme.accent, Theme::default().accent, "mocha applies");
        }
    }

    mod handle_submit_prompt {
        use super::*;

        /// An app signing in, with the `Login` op already taken off `rx`.
        fn signing_in() -> (
            App,
            tokio::sync::mpsc::Receiver<Op>,
            tokio::sync::mpsc::UnboundedSender<Event>,
        ) {
            let (mut app, mut rx, event_tx) = test_app(None);
            app.dispatch_op(Op::Login);
            rx.try_recv().expect("login forwarded");
            (app, rx, event_tx)
        }

        #[tokio::test]
        async fn sends_a_code_typed_while_signing_in_without_an_echo() {
            let (mut app, mut rx, _event_tx) = signing_in();
            let entries = app.chat.entry_count();

            app.dispatch_action(Action::Submit(" auth-code#state\n".to_owned()));

            assert_eq!(
                rx.try_recv().ok(),
                Some(Op::LoginCode(SignInCode("auth-code#state".to_owned())))
            );
            assert_eq!(activity(&app), Some(&Activity::SigningIn));
            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
            assert_eq!(
                app.chat.entry_count(),
                entries,
                "a sign-in code is never echoed"
            );
        }

        #[tokio::test]
        async fn returns_a_prompt_typed_while_signing_in_to_the_input() {
            let (mut app, mut rx, _event_tx) = signing_in();

            app.dispatch_action(Action::Submit("fix the #42 bug".to_owned()));

            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
            assert_eq!(app.input.lines(), ["fix the #42 bug"]);
            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
            assert_eq!(
                app.chat.last_system_text(),
                Some(
                    "Sign-in is waiting for the code the sign-in page shows. \
                     Send other prompts after sign-in ends, or press Esc to cancel it."
                ),
            );
        }

        #[tokio::test]
        async fn runs_a_slash_command_typed_while_signing_in() {
            let (mut app, mut rx, _event_tx) = signing_in();

            app.dispatch_action(Action::Submit("/theme latte".to_owned()));

            assert_eq!(app.session_info.config.theme_name, "latte");
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[test]
        fn slash_theme_opens_the_picker() {
            let (mut app, _rx, _event_tx) = test_app(None);
            assert!(!app.modals.is_active(), "stack starts empty");

            app.dispatch_action(Action::Submit("/theme".to_owned()));

            assert!(app.modals.is_active(), "bare `/theme` opens a picker modal");
        }

        #[test]
        fn slash_typed_swap_theme_routes_through_local_handler() {
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
        async fn slash_typed_theme_swaps_mid_turn() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");

            app.dispatch_action(Action::Submit("/theme latte".to_owned()));

            assert_eq!(app.session_info.config.theme_name, "latte");
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[tokio::test]
        async fn slash_typed_model_routes_synthesized_set_model_through_dispatch() {
            let (mut app, mut rx, _event_tx) = test_app(None);

            app.dispatch_action(Action::Submit("/model haiku".to_owned()));

            let forwarded = rx.try_recv().expect("SetModel forwarded to the engine");
            assert_eq!(
                forwarded,
                Op::SetModel(ConfigPick::Model {
                    id: ResolvedModelId::new("claude-haiku-5-5"),
                    effort: None,
                }),
            );
        }

        #[test]
        fn after_a_cancel_runs_the_held_queue_before_the_new_prompt() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            app.dispatch_action(Action::Submit("held".to_owned()));
            app.handle_esc();
            app.handle_engine_event(Event::Cancelled);
            let mut forwarded = || std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
            assert_eq!(
                forwarded(),
                [
                    Op::prompt("active"),
                    Op::Queue("held".to_owned()),
                    Op::Interrupt,
                ],
            );

            app.dispatch_action(Action::Submit("new".to_owned()));

            assert_eq!(
                forwarded(),
                [Op::prompt("held"), Op::Queue("new".to_owned())],
            );
            assert_eq!(*app.queue.pending(), ["new"],);
        }

        #[test]
        fn double_slash_while_busy_queues_the_literal_and_keeps_the_escape() {
            let (mut app, mut rx, _event_tx) = test_app(None);

            app.dispatch_action(Action::Submit("active".to_owned()));
            app.dispatch_action(Action::Submit("//etc/hosts".to_owned()));
            app.dispatch_action(Action::Submit("//tmp".to_owned()));

            let mut forwarded = || std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
            assert_eq!(
                forwarded(),
                [
                    Op::prompt("active"),
                    Op::Queue("/etc/hosts".to_owned()),
                    Op::Queue("/tmp".to_owned()),
                ],
            );

            app.handle_engine_event(Event::PromptDrained("/etc/hosts".to_owned()));

            assert_eq!(
                *app.queue.pending(),
                ["//tmp"],
                "the queue keeps the typed escape so Esc restores it",
            );

            app.handle_engine_event(Event::Cancelled);
            app.dispatch_action(Action::Submit("new".to_owned()));

            assert_eq!(
                forwarded(),
                [Op::prompt("/tmp"), Op::Queue("new".to_owned())],
            );
            assert_eq!(app.queue.active().map(QueuedPrompt::line), Some("//tmp"));
        }
    }

    mod forward_queued_prompts {
        use super::*;

        #[test]
        fn keeps_a_prompt_the_full_channel_turned_away_queued() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            while app.ops.try_send(Op::Interrupt).is_ok() {}
            let entries_before = app.chat.entry_count();

            app.dispatch_action(Action::Submit("a".to_owned()));

            assert_eq!(*app.queue.pending(), ["a"]);
            assert_eq!(app.queue.forwarded(), 0);
            assert_eq!(app.chat.entry_count(), entries_before, "no error block");
        }

        #[test]
        fn resends_a_prompt_the_full_channel_turned_away() {
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
            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
        }

        #[test]
        fn stops_at_a_closed_channel_and_quits() {
            let (mut app, rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            app.queue.push("a".to_owned());
            app.queue.push("b".to_owned());
            drop(rx);

            app.forward_queued_prompts();

            assert!(app.should_quit);
            assert_eq!(app.queue.forwarded(), 0);
        }
    }

    mod apply_slash_outcome {
        use super::*;

        #[test]
        fn hands_back_forwards_and_prompts_and_opens_modals_whether_idle_or_busy() {
            for busy in [false, true] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                if busy {
                    start_op(&mut app, &Op::prompt("running"));
                    app.handle_engine_event(Event::StreamToken("partial".to_owned()));
                }

                let handed_back = app.apply_slash_outcome(Err("/diff: boom".to_owned()), "/diff");

                assert_eq!(handed_back, None);
                assert_eq!(app.chat.last_error_text(), Some("/diff: boom"));

                let modal = SlashOutcome::Modal(Box::new(FakeModal::new("picker")));
                let handed_back = app.apply_slash_outcome(Ok(modal), "/theme");

                assert_eq!(handed_back, None);
                assert!(app.modals.is_active());

                let forward = SlashOutcome::Forward(Action::Op(Op::Clear));
                let handed_back = app.apply_slash_outcome(Ok(forward), "/clear");

                assert_eq!(handed_back, Some(Action::Op(Op::Clear)));

                let prompt = SlashOutcome::Prompt("Create AGENTS.md".to_owned());
                let handed_back = app.apply_slash_outcome(Ok(prompt), "/init AGENTS.md");

                assert_eq!(
                    handed_back,
                    Some(Action::SlashPrompt {
                        line: "/init AGENTS.md".to_owned(),
                        prompt: "Create AGENTS.md".to_owned(),
                    }),
                );
                assert_eq!(
                    rx.try_recv(),
                    Err(TryRecvError::Empty),
                    "the caller routes the returned actions"
                );
            }
        }

        #[tokio::test]
        async fn runs_a_background_task_and_prints_nothing_until_it_finishes() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            let task = SlashOutcome::Background(Box::pin(async move {
                Ok(SlashOutcome::SystemMessage(rx.await.unwrap()))
            }));

            let handed_back = app.apply_slash_outcome(Ok(task), "/diff");

            assert_eq!(handed_back, None);
            assert_eq!(app.chat.entry_count(), 0);

            tx.send("done".to_owned()).unwrap();
            let finished = app.slash_tasks.join_next().await.expect("the task runs");
            app.apply_slash_task(finished);

            assert_eq!(app.chat.last_system_text(), Some("done"));
        }
    }
}
