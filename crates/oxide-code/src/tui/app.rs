//! Root TUI application.
//!
//! [`App`] owns every component, holds the cross-task channels, and runs the `tokio::select!`
//! loop multiplexing crossterm events, engine events, git probe results, and a 60 FPS render tick.
//! A dirty flag coalesces redraws so each state change renders once.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind};
use futures::{Stream, StreamExt};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

use super::action::Action;
use super::components::chat::ChatView;
use super::components::input::{InputArea, InputOutcome};
use super::components::status::{Status, StatusBar};
use super::components::welcome::{self, WelcomeSnapshot};
use super::git_probes::{GitProbes, GitUpdate};
use super::glyphs::{NEWLINE_GLYPH, USER_PROMPT_PREFIX, USER_PROMPT_PREFIX_WIDTH};
use super::modal::{Modal, ModalAction, ModalStack};
use super::terminal::{Tui, synchronized_update, write_status_hyperlinks};
use super::theme::Theme;
use crate::config::{AutoCompactionConfig, Effort, StatusLineSegment, display_auto_compaction};
use crate::model::ModelSpec;
use crate::permission::SessionGrant;
use crate::protocol::{Event, Interaction, Op};
use crate::session::display::archive_notice;
use crate::slash::{self, LiveSessionInfo, Parsed, SlashContext, SlashKind, SlashOutcome};
use crate::tool::ApprovalPreview;
use crate::util::text::{center_truncate_to_width, truncate_to_width};

/// Tick interval for animation frames and render coalescing (~60 FPS).
const TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Window in which a second Ctrl+C confirms exit.
const EXIT_WINDOW: Duration = Duration::from_secs(1);

/// Maximum queued prompts shown in the preview before collapsing into `+N more`.
const PREVIEW_VISIBLE: usize = 3;

pub(crate) struct App {
    theme: Theme,
    status_bar: StatusBar,
    git_probes: GitProbes,
    chat: ChatView,
    input: InputArea,
    session_info: LiveSessionInfo,
    events: mpsc::UnboundedReceiver<Event>,
    ops: mpsc::Sender<Op>,
    /// Prompt already painted for the in-flight turn. Replayed if auto-compaction clears chat.
    active_prompt: Option<String>,
    /// FIFO of prompts submitted mid-turn, kept as typed so Esc restores a `//` escape. The TUI owns
    /// each one until the engine confirms it with `PromptDrained` or it starts a turn of its own.
    pending_prompts: VecDeque<String>,
    /// How many leading `pending_prompts` the in-flight turn already holds.
    forwarded_prompts: usize,
    modals: ModalStack,
    /// Theme saved when a `/theme` picker opens. Restored if the modal cancels.
    preview_theme_snapshot: Option<Theme>,
    should_quit: bool,
    dirty: bool,
}

pub(crate) struct AppHistory {
    pub(crate) interactions: Vec<Interaction>,
    pub(crate) title: Option<String>,
    /// Resuming the session unarchived it, so the chat opens with a note saying so.
    pub(crate) unarchived: bool,
}

impl App {
    pub(crate) fn new(
        theme: &Theme,
        session_info: LiveSessionInfo,
        show_thinking: bool,
        events: mpsc::UnboundedReceiver<Event>,
        ops: mpsc::Sender<Op>,
        history: AppHistory,
    ) -> Self {
        let mut chat = ChatView::new(theme, show_thinking);
        chat.load_history(history.interactions);
        if history.unarchived {
            chat.push_system_message(archive_notice(&session_info.session_id, false));
        }
        let mut status_bar = StatusBar::new(
            theme,
            session_info.config.status_line.clone(),
            session_info.config.model.short_display_name().to_owned(),
            session_info.config.effort,
            session_info.cwd.clone(),
            session_info.git_branch.clone(),
        );
        status_bar.set_title(history.title);
        let git_probes = GitProbes::new(
            session_info.git_cwd.clone(),
            session_info
                .config
                .status_line
                .contains(&StatusLineSegment::PullRequest),
            Instant::now(),
        );
        Self {
            theme: theme.clone(),
            status_bar,
            git_probes,
            chat,
            input: InputArea::new(theme),
            session_info,
            events,
            ops,
            active_prompt: None,
            pending_prompts: VecDeque::new(),
            forwarded_prompts: 0,
            modals: ModalStack::new(),
            preview_theme_snapshot: None,
            should_quit: false,
            dirty: true,
        }
    }

    /// Appends a system message ahead of the first frame, for warnings raised at startup.
    pub(crate) fn push_system_message(&mut self, body: String) {
        self.chat.push_system_message(body);
    }

    /// Opens a modal ahead of the first frame, for overlays raised at startup.
    pub(crate) fn push_modal(&mut self, modal: Box<dyn Modal>) {
        self.modals.push(modal);
        self.dirty = true;
    }

    /// Runs until the user quits or the engine's event channel closes.
    pub(crate) async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        self.run_with_events(terminal, EventStream::new()).await
    }

    async fn run_with_events<W, S>(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::prelude::CrosstermBackend<W>>,
        mut crossterm_events: S,
    ) -> Result<()>
    where
        W: std::io::Write,
        S: Stream<Item = std::io::Result<CrosstermEvent>> + Unpin,
    {
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        self.render(terminal)?;

        loop {
            // Four sources can wake the loop. The tick arm is the only renderer, so handlers only
            // mark `dirty` and render work stays paced by the 60 FPS clock.
            tokio::select! {
                event = crossterm_events.next() => {
                    match event {
                        Some(Ok(event)) => self.handle_crossterm_event(&event),
                        Some(Err(e)) => return Err(e).context("cannot read terminal input"),
                        None => self.should_quit = true,
                    }
                }
                event = self.events.recv() => {
                    match event {
                        Some(event) => self.handle_engine_event(event),
                        // The engine stopped, and `recv` would keep returning `None`, so quit.
                        None => self.should_quit = true,
                    }
                }
                Some(update) = self.git_probes.recv() => self.apply_git_update(update),
                _ = tick.tick() => {
                    self.git_probes.poll(Instant::now());
                    if self.status_bar.tick() {
                        self.dirty = true;
                    }
                    if self.expire_armed_exit() {
                        self.dirty = true;
                    }
                    if self.dirty {
                        self.render(terminal)?;
                        self.dirty = false;
                    }
                }
            }

            if self.should_quit {
                break;
            }
        }

        Ok(())
    }

    // ── Event Handling ──

    fn handle_crossterm_event(&mut self, event: &CrosstermEvent) {
        // Windows also reports key releases, which would otherwise act as a second press.
        if let CrosstermEvent::Key(key) = event
            && key.kind == KeyEventKind::Release
        {
            return;
        }
        // The top overlay owns keyboard input, including pastes. Mouse and resize events still
        // reach chat.
        if self.modals.is_active() {
            match event {
                CrosstermEvent::Key(key) => {
                    if let Some(action) = self.modals.handle_key(key) {
                        self.apply_modal_action(action);
                    }
                    self.dirty = true;
                    return;
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
            CrosstermEvent::Mouse(_) => {
                self.chat.handle_event(event);
            }
            CrosstermEvent::Resize(..) => {}
            _ => return,
        }
        self.dirty = true;
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

    fn clear_modals(&mut self) {
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
    fn handle_esc(&mut self) {
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
    fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::Submit(text) => self.handle_submit_prompt(&text),
            Action::Op(op) => self.dispatch_op(op),
            Action::ConfirmExit => self.confirm_exit(),
            Action::PreviewTheme { name } => self.preview_theme(&name),
            Action::SwapTheme { name } => self.swap_theme(&name),
        }
    }

    /// Applies the op's local state change, then sends it to the engine. A prompt starts a turn.
    fn dispatch_op(&mut self, op: Op) {
        match &op {
            Op::Prompt(_) => return self.start_turn(op),
            Op::Interrupt => self.status_bar.set_status(Status::Cancelling),
            Op::Shutdown => self.should_quit = true,
            Op::Resume { .. } => self.input.set_enabled(false),
            Op::Compact { .. } => {
                self.input.set_enabled(false);
                self.status_bar.set_status(Status::Compacting);
            }
            Op::Queue(_)
            | Op::Resolve { .. }
            | Op::SetModel { .. }
            | Op::Clear
            | Op::Archive
            | Op::Rename { .. } => {}
        }
        self.send_op(op);
    }

    /// Sends `op` to the engine. Channel errors surface as chat error blocks.
    fn send_op(&mut self, op: Op) {
        if let Err(e) = self.ops.try_send(op) {
            match e {
                mpsc::error::TrySendError::Closed(_) => {
                    self.chat
                        .push_error("the engine stopped unexpectedly; restart `ox` to recover");
                    self.input.set_enabled(false);
                    self.should_quit = true;
                }
                // An approval decision is a control-plane reply the engine is waiting for, so a
                // dropped one leaves the turn blocked until the user cancels it.
                mpsc::error::TrySendError::Full(Op::Resolve { .. }) => {
                    self.chat.push_error(
                        "approval could not be delivered; press Esc to cancel the blocked turn",
                    );
                }
                mpsc::error::TrySendError::Full(_) => {
                    self.chat
                        .push_error("op channel full, so the request was dropped (this is a bug)");
                }
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

    fn preview_theme(&mut self, name: &str) {
        if let Some(preview) = super::theme::load_builtin(name) {
            if self.preview_theme_snapshot.is_none() {
                self.preview_theme_snapshot = Some(self.theme.clone());
            }
            self.apply_theme(&preview);
        } else {
            tracing::warn!(name, "PreviewTheme: unknown built-in; picker roster drift");
        }
    }

    fn swap_theme(&mut self, name: &str) {
        self.preview_theme_snapshot = None;
        if let Some(theme) = super::theme::load_builtin(name) {
            name.clone_into(&mut self.session_info.config.theme_name);
            self.apply_theme(&theme);
            self.chat
                .push_system_message(format!("Theme set to {name}."));
        } else {
            tracing::warn!(name, "SwapTheme: unknown built-in; picker roster drift");
        }
    }

    /// Queues the prompt, then starts a turn with the queue head when idle. Slash commands forward
    /// their own synthesized actions.
    fn handle_submit_prompt(&mut self, text: &str) {
        if self.input.is_enabled() {
            if let Some(parsed) = slash::parse_slash(text) {
                if slash::echoes_input(&parsed) {
                    self.chat.push_user_message(text.to_owned());
                }
                if let Some(action) = self.run_slash(&parsed) {
                    if matches!(action, Action::Op(Op::Prompt(_))) && slash::echoes_input(&parsed) {
                        self.active_prompt = Some(text.to_owned());
                    }
                    self.dispatch_action(action);
                }
                return;
            }
            // Prompts a cancel left queued run before this one.
            self.pending_prompts.push_back(text.to_owned());
            self.drain_pending_prompt();
            return;
        }
        if let Some(parsed) = slash::parse_slash(text) {
            if slash::echoes_input(&parsed) {
                self.chat.push_user_message(text.to_owned());
            }
            match slash::classify(&parsed) {
                SlashKind::ReadOnly | SlashKind::Unknown => _ = self.run_slash(&parsed),
                SlashKind::Mutating => {
                    self.chat.push_system_message(format!(
                        "/{} runs only when idle. Try again after the turn finishes.",
                        parsed.name,
                    ));
                }
            }
            return;
        }
        self.pending_prompts.push_back(text.to_owned());
        self.forward_queued_prompts();
    }

    /// Starts a turn, then hands it the queued prompts so they splice in at its round boundaries.
    fn start_turn(&mut self, prompt: Op) {
        self.input.set_enabled(false);
        self.status_bar.set_status(Status::Streaming);
        self.send_op(prompt);
        self.forward_queued_prompts();
    }

    /// Forwards the queued prompts the in-flight turn lacks. Holds them while compacting or
    /// cancelling, when no turn may be left to splice them in.
    fn forward_queued_prompts(&mut self) {
        if matches!(
            self.status_bar.status(),
            Status::Compacting | Status::Cancelling,
        ) {
            return;
        }
        while let Some(queued) = self.pending_prompts.get(self.forwarded_prompts) {
            let prompt = slash::unescape_literal(queued).to_owned();
            self.send_op(Op::Queue(prompt));
            self.forwarded_prompts += 1;
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

    fn handle_engine_event(&mut self, event: Event) {
        match event {
            Event::StreamToken(token) => {
                self.chat.append_stream_token(&token);
                self.set_active_status(Status::Streaming);
                self.input.set_enabled(false);
            }
            Event::ThinkingToken(token) => {
                self.chat.append_thinking_token(&token);
                self.set_active_status(Status::Streaming);
            }
            Event::ProgressUpdate(text) => self.chat.push_progress_update(text),
            Event::Retrying(notice) => self.chat.push_notice(notice),
            Event::ToolCallStart(call) => {
                self.set_active_status(Status::ToolRunning {
                    name: call.name.clone(),
                });
                self.chat.push_tool_call(call);
            }
            Event::ToolCallEnd(result) => self.chat.push_tool_result(result),
            Event::PromptDrained(text) => self.apply_prompt_drained(text),
            Event::UsageUpdated(usage) => {
                self.status_bar.set_usage(Some(usage));
            }
            Event::TurnComplete => {
                self.finish_turn();
            }
            Event::Cancelled => {
                self.chat.push_interrupted_marker();
                self.active_prompt = None;
                self.enter_idle();
            }
            Event::AutoCompactionStarted => {
                self.set_active_status(Status::Compacting);
                self.input.set_enabled(false);
            }
            Event::SessionTitleUpdated { session_id, title } => {
                if session_id == self.session_info.session_id {
                    self.status_bar.set_title(Some(title));
                }
            }
            Event::SessionRolled { id, archived } => {
                let old_id = std::mem::replace(&mut self.session_info.session_id, id);
                self.session_info.session_allow.clear();
                self.status_bar.set_title(None);
                self.status_bar.set_usage(None);
                self.chat.clear_history();
                self.active_prompt = None;
                if archived {
                    self.chat.push_system_message(archive_notice(&old_id, true));
                }
            }
            Event::SessionResumed {
                id,
                title,
                unarchived,
                history,
            } => self.apply_session_resumed(id, title, unarchived, history),
            Event::SessionCompacted {
                summary,
                pre_count,
                instructions,
                automatic,
            } => self.apply_session_compacted(
                &summary,
                pre_count,
                instructions.as_deref(),
                automatic,
            ),
            Event::ConfigChanged {
                model,
                effort,
                requested_effort,
                auto_compaction,
            } => self.apply_config_changed(model, effort, requested_effort, auto_compaction),
            Event::TurnFailed(msg) => {
                self.chat.push_error(&msg);
                self.finish_turn();
            }
            Event::Error(msg) => self.chat.push_error(&msg),
            Event::ApprovalRequested { id, preview, grant } => {
                self.push_approval_modal(id, preview, grant);
            }
            Event::SessionRulesUpdated(rules) => self.session_info.session_allow = rules,
        }
        self.dirty = true;
    }

    fn apply_prompt_drained(&mut self, text: String) {
        if self.forwarded_prompts > 0
            && self
                .pending_prompts
                .front()
                .is_some_and(|queued| slash::unescape_literal(queued) == text)
        {
            self.pending_prompts.pop_front();
            self.forwarded_prompts -= 1;
        } else {
            tracing::warn!("drained prompt {text:?} does not lead the forwarded queue");
        }
        self.chat.push_user_message(text);
    }

    /// Opens the approve-or-deny overlay for a gated tool call. Pushing onto the stack lets it nest
    /// over any open picker, and the modal's cancel hook resolves the blocked turn on dismissal.
    fn push_approval_modal(
        &mut self,
        id: String,
        preview: ApprovalPreview,
        grant: Option<Box<SessionGrant>>,
    ) {
        self.modals
            .push(Box::new(super::modal::approval::ApprovalModal::new(
                id,
                preview,
                grant.map(|grant| *grant),
            )));
    }

    fn finish_turn(&mut self) {
        self.chat.commit_streaming();
        self.active_prompt = None;
        // A cancel that lost the race to the turn's end still leaves the next step to the user.
        if matches!(self.status_bar.status(), Status::Cancelling) {
            self.enter_idle();
        } else {
            self.finalize_idle();
        }
    }

    /// Mid-session resume: rebinds the session and rebuilds chat from the target transcript.
    fn apply_session_resumed(
        &mut self,
        id: String,
        title: Option<String>,
        unarchived: bool,
        history: Vec<Interaction>,
    ) {
        self.session_info.session_id = id;
        self.session_info.session_allow.clear();
        self.status_bar.set_title(title);
        self.status_bar.set_usage(None);
        self.chat.clear_history();
        self.chat.load_history(history);
        if unarchived {
            self.chat
                .push_system_message(archive_notice(&self.session_info.session_id, false));
        }
        if let Some(notice) = self.session_info.config.model.unsupported_notice() {
            self.chat.push_system_message(notice);
        }
        self.active_prompt = None;
        // Queued prompts belonged to the previous thread, so resume drops them.
        let dropped = self.pending_prompts.len();
        self.pending_prompts.clear();
        if dropped > 0 {
            self.chat.push_system_message(format!(
                "{dropped} queued prompt{plural} discarded. Typed for the previous session.",
                plural = if dropped == 1 { "" } else { "s" },
            ));
        }
        self.clear_modals();
        self.finalize_idle();
    }

    /// Repaints chat after `/compact` with one boundary block. Queued prompts survive because the
    /// session identity stays the same.
    fn apply_session_compacted(
        &mut self,
        summary: &str,
        pre_count: u32,
        instructions: Option<&str>,
        automatic: bool,
    ) {
        self.chat.clear_history();
        self.status_bar.set_usage(None);
        self.chat
            .push_compacted_block(pre_count, instructions, summary.to_owned());
        if automatic && let Some(prompt) = &self.active_prompt {
            self.chat.push_user_message(prompt.clone());
        }
        self.clear_modals();
        if !automatic {
            self.active_prompt = None;
            self.finalize_idle();
        }
    }

    fn apply_config_changed(
        &mut self,
        model: ModelSpec,
        effort: Option<Effort>,
        requested_effort: Option<Effort>,
        auto_compaction: AutoCompactionConfig,
    ) {
        let model_changed = model.id() != self.session_info.config.model.id();
        let prev_effort = self.session_info.config.effort;
        let prev_auto_compaction = self.session_info.config.compaction.auto;
        let confirmation = format_config_change(
            &model,
            model_changed,
            prev_effort,
            effort,
            requested_effort,
            prev_auto_compaction,
            auto_compaction,
        );
        if model_changed {
            self.status_bar
                .set_model(model.short_display_name().to_owned());
        }
        self.status_bar.set_effort(effort);
        self.session_info
            .config
            .apply_swap(model, effort, requested_effort, auto_compaction);
        self.chat.push_system_message(confirmation);
    }

    /// Goes idle, then submits the next queued prompt.
    fn finalize_idle(&mut self) {
        self.enter_idle();
        self.drain_pending_prompt();
    }

    /// Resets to idle, clears orphan calls, and re-enables input. Queued prompts stay queued, and
    /// the ended turn no longer holds any of them.
    fn enter_idle(&mut self) {
        self.chat.clear_pending_calls();
        self.forwarded_prompts = 0;
        self.status_bar.set_status(Status::Idle);
        self.input.set_enabled(true);
    }

    /// Starts a turn with the oldest queued prompt.
    fn drain_pending_prompt(&mut self) {
        if let Some(queued) = self.pending_prompts.pop_front() {
            let prompt = slash::unescape_literal(&queued).to_owned();
            self.chat.push_user_message(prompt.clone());
            self.active_prompt = Some(prompt.clone());
            self.start_turn(Op::Prompt(prompt));
        }
    }

    /// Sets busy status unless a user-acknowledgement status is showing. Leaving `Compacting`
    /// releases the prompts held during an automatic compaction.
    fn set_active_status(&mut self, status: Status) {
        match self.status_bar.status() {
            Status::Cancelling | Status::ExitArmed { .. } => {}
            Status::Compacting => {
                self.status_bar.set_status(status);
                self.forward_queued_prompts();
            }
            _ => self.status_bar.set_status(status),
        }
    }

    /// Returns `true` when an [`Status::ExitArmed`] window has elapsed and the bar was reset.
    fn expire_armed_exit(&mut self) -> bool {
        if let Status::ExitArmed { until } = self.status_bar.status()
            && Instant::now() >= *until
        {
            self.status_bar.set_status(Status::Idle);
            return true;
        }
        false
    }

    fn apply_git_update(&mut self, update: GitUpdate) {
        let changed = match update {
            GitUpdate::Branch(branch) => self.status_bar.set_git_branch(branch),
            GitUpdate::PullRequest(pull_request) => self.status_bar.set_pull_request(pull_request),
        };
        if changed {
            self.dirty = true;
        }
    }

    // ── Rendering ──

    fn render<W: std::io::Write>(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::prelude::CrosstermBackend<W>>,
    ) -> Result<()> {
        synchronized_update(terminal, |terminal| {
            terminal.draw(|frame| self.draw_frame(frame))?;
            self.emit_status_hyperlinks(terminal)
        })
    }

    /// Replays captured status links as OSC 8 envelopes over the drawn frame.
    fn emit_status_hyperlinks<W: std::io::Write>(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::prelude::CrosstermBackend<W>>,
    ) -> Result<()> {
        let links = self.status_bar.take_pending_hyperlinks();
        if links.is_empty() {
            return Ok(());
        }
        write_status_hyperlinks(terminal.backend_mut(), &links)?;
        Ok(())
    }

    fn draw_frame(&mut self, frame: &mut ratatui::Frame<'_>) {
        // The status bar and one chat row stay visible above a modal, which then outranks the
        // queued-prompt preview.
        let modal_max = frame.area().height.saturating_sub(2 + 1);
        let modal_height = self.modals.height(frame.area().width, modal_max);
        let preview_height = self.preview_height().min(modal_max - modal_height);
        // Modal owns focus, so input and popup bands collapse.
        let modal_active = self.modals.is_active();
        let popup_height = if modal_active {
            0
        } else {
            self.input.popup_height()
        };
        let input_height = if modal_active { 0 } else { self.input.height() };
        // Pre-fill with surface bg so unpainted gaps inherit the theme.
        frame.render_widget(
            ratatui::widgets::Block::default().style(self.theme.surface()),
            frame.area(),
        );
        let chunks = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(preview_height),
            Constraint::Length(modal_height),
            Constraint::Length(popup_height),
            Constraint::Length(input_height),
        ])
        .split(frame.area());

        self.status_bar.render(frame, chunks[0]);
        if self.chat.is_empty() && self.session_info.config.show_welcome {
            let snap = WelcomeSnapshot::from_live(&self.session_info);
            welcome::paint(frame, chunks[1], &self.theme, &snap);
        } else {
            self.chat.render(frame, chunks[1]);
            self.render_jump_overlay(frame, chunks[1]);
        }
        if preview_height > 0 {
            self.render_preview(frame, chunks[2]);
        }
        if modal_active {
            self.modals.render(frame, chunks[3], &self.theme);
        } else {
            if popup_height > 0 {
                self.input.render_popup(frame, chunks[4]);
            }
            self.input.render(frame, chunks[5]);
        }
    }

    fn preview_height(&self) -> u16 {
        if self.pending_prompts.is_empty() {
            return 0;
        }
        let visible = self.pending_prompts.len().min(PREVIEW_VISIBLE);
        let overflow = usize::from(self.pending_prompts.len() > PREVIEW_VISIBLE);
        u16::try_from(visible + overflow).unwrap_or(u16::MAX)
    }

    fn render_preview(&self, frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect) {
        let body_width = usize::from(area.width)
            .saturating_sub(usize::from(USER_PROMPT_PREFIX_WIDTH))
            .saturating_sub(1);

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(PREVIEW_VISIBLE + 1);
        for prompt in self.pending_prompts.iter().take(PREVIEW_VISIBLE) {
            lines.push(preview_line(prompt, &self.theme, body_width));
        }
        if self.pending_prompts.len() > PREVIEW_VISIBLE {
            let extra = self.pending_prompts.len() - PREVIEW_VISIBLE;
            let gutter = " ".repeat(usize::from(USER_PROMPT_PREFIX_WIDTH));
            lines.push(Line::from(Span::styled(
                format!("{gutter}+{extra} more"),
                self.theme.dim(),
            )));
        }
        frame.render_widget(
            ratatui::widgets::Paragraph::new(lines).style(self.theme.surface()),
            area,
        );
    }

    fn render_jump_overlay(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        if !self.chat.is_scrolled_up() || area.width < 25 || area.height == 0 {
            return;
        }

        let new_count = self.chat.new_content_since_pause();
        let label = jump_overlay_label(new_count, usize::from(area.width));
        let style = if new_count == 0 {
            self.theme.dim()
        } else {
            self.theme.accent()
        };
        // Pill sized to label + 1-cell padding per side, anchored at the right edge with an
        // opaque surface bg so the chat content underneath stays readable.
        let pill_width = u16::try_from(label.width().saturating_add(2)).unwrap_or(u16::MAX);
        if pill_width > area.width {
            return;
        }
        let pill = Rect {
            x: area.x + area.width.saturating_sub(pill_width),
            y: area.y + area.height.saturating_sub(1),
            width: pill_width,
            height: 1,
        };
        let block = Block::default().style(self.theme.surface());
        let inner = block.inner(pill);
        frame.render_widget(block, pill);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(label, style),
                Span::raw(" "),
            ])),
            inner,
        );
    }
}

fn jump_overlay_label(new_count: u32, width: usize) -> String {
    if width < 40 {
        return "↓ (ctrl+End)".to_owned();
    }
    let label = match new_count {
        0 => "Jump to bottom (ctrl+End) ↓".to_owned(),
        1 => "1 new message (ctrl+End) ↓".to_owned(),
        n => format!("{n} new messages (ctrl+End) ↓"),
    };
    center_truncate_to_width(&label, width.saturating_sub(2))
}

/// Renders a queued prompt as a dim ghost, capped at `body_width` columns.
fn preview_line(prompt: &str, theme: &Theme, body_width: usize) -> Line<'static> {
    use ratatui::style::Modifier;

    let flat = prompt.replace('\n', NEWLINE_GLYPH);
    let display = truncate_to_width(&flat, body_width);
    let style = theme.queued().add_modifier(Modifier::DIM);
    Line::from(vec![
        Span::styled(USER_PROMPT_PREFIX, style),
        Span::styled(display, style),
    ])
}

/// `Event::ConfigChanged` confirmation, surfacing silent effort shifts.
fn format_config_change(
    model: &ModelSpec,
    model_changed: bool,
    prev_effort: Option<Effort>,
    new_effort: Option<Effort>,
    requested_effort: Option<Effort>,
    prev_auto_compaction: AutoCompactionConfig,
    new_auto_compaction: AutoCompactionConfig,
) -> String {
    let message = if model_changed {
        let head = format!("Switched to {} ({})", model.display_name(), model.id());
        match (requested_effort, prev_effort, new_effort) {
            (Some(req), _, None) => {
                format!("{head}. Effort unchanged: model has no effort tier (asked for {req}).")
            }
            (None, None, None) => format!("{head}."),
            (None, Some(_), None) => format!("{head}. Effort cleared (model has no effort tier)."),
            (None, None, Some(eff)) => format!("{head} · effort {eff} (model default)."),
            (_, _, Some(eff)) => format!("{head} · effort {eff}."),
        }
    } else {
        match (requested_effort, new_effort) {
            (Some(req), None) => {
                format!("Effort unchanged: model has no effort tier (asked for {req}).")
            }
            (_, Some(eff)) => format!("Effort set to {eff}."),
            // Slash dispatch keeps this unreachable, but a clear fallback beats a panic.
            (None, None) => "Config unchanged.".to_owned(),
        }
    };
    if model_changed && prev_auto_compaction != new_auto_compaction {
        return append_sentence(
            message,
            &format!(
                "Auto compaction {}",
                display_auto_compaction(new_auto_compaction)
            ),
        );
    }
    message
}

fn append_sentence(mut message: String, sentence: &str) -> String {
    if message.ends_with('.') {
        message.pop();
    }
    format!("{message}. {sentence}.")
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::prelude::CrosstermBackend;
    use ratatui::style::Color;
    use ratatui::{Terminal, TerminalOptions, Viewport};
    use tokio::sync::mpsc;

    use super::*;
    use crate::config::test_thresholds;
    use crate::permission::rule::Rule;
    use crate::protocol::{Decision, ToolCall, ToolResult, UsageSnapshot};
    use crate::tool::{ApprovalBody, ToolResultView};
    use crate::tui::glyphs::INTERRUPTED_MARKER;
    use crate::tui::modal::testing::ScriptedModal;

    /// Idle `App` plus the `ops` consumer and an `event_tx` kept alive so `events` stays open.
    fn test_app(title: Option<&str>) -> (App, mpsc::Receiver<Op>, mpsc::UnboundedSender<Event>) {
        let (event_tx, events) = mpsc::unbounded_channel::<Event>();
        let (ops, op_rx) = mpsc::channel::<Op>(8);
        let app = App::new(
            &Theme::default(),
            test_session_info(),
            false,
            events,
            ops,
            AppHistory {
                interactions: Vec::new(),
                title: title.map(ToOwned::to_owned),
                unarchived: false,
            },
        );
        (app, op_rx, event_tx)
    }

    fn bash_call_start(id: &str, command: &str) -> Event {
        Event::ToolCallStart(ToolCall {
            id: id.to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": command}),
            icon: "$".to_owned(),
            label: command.to_owned(),
        })
    }

    fn text_call_end(id: &str, content: &str, is_error: bool, title: Option<&str>) -> Event {
        Event::ToolCallEnd(ToolResult {
            id: id.to_owned(),
            content: content.to_owned(),
            is_error,
            title: title.map(ToOwned::to_owned),
            view: ToolResultView::Text {
                content: content.to_owned(),
            },
        })
    }

    fn test_session_info() -> LiveSessionInfo {
        // `test-model` is intentionally unknown so `display_name` falls back to the literal
        // id, keeping insta snapshots stable.
        use crate::config::{
            AutoCompactionConfig, CompactionConfig, ConfigSnapshot, Effort, PromptCacheTtl,
        };

        LiveSessionInfo {
            cwd: "~/test".to_owned(),
            git_cwd: None,
            git_branch: Some("main".to_owned()),
            version: "0.0.0-test",
            session_id: "test-session".to_owned(),
            session_allow: Vec::new(),
            config: ConfigSnapshot {
                auth: crate::config::auth::AuthSource::ApiKeyEnv,
                base_url: "https://api.test.invalid".to_owned(),
                extra_ca_certs: None,
                gateway: crate::config::gateway::GatewaySnapshot {
                    custom_header_names: Vec::new(),
                    attribution: true,
                    structured_outputs: true,
                },
                model: ModelSpec::resolve("test-model".to_owned()),
                effort: Some(Effort::High),
                max_tokens: None,
                max_tool_rounds: None,
                prompt_cache_ttl: PromptCacheTtl::OneHour,
                compaction: CompactionConfig::resolved_for_test(AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(155_000),
                }),
                permission_mode: crate::permission::Mode::Auto,
                show_thinking: false,
                show_welcome: true,
                status_line: crate::config::StatusLineSegment::DEFAULT.to_vec(),
                theme_name: "mocha".to_owned(),
                provenance: crate::config::provenance::Provenance::default(),
            },
        }
    }

    fn spec(id: &str) -> ModelSpec {
        ModelSpec::resolve(id.to_owned())
    }

    fn base_auto_compaction() -> AutoCompactionConfig {
        AutoCompactionConfig {
            enabled: true,
            threshold_tokens: Some(155_000),
        }
    }

    fn usage_snapshot() -> UsageSnapshot {
        UsageSnapshot {
            context_tokens: Some(124_000),
            context_window: Some(1_000_000),
            estimated_cost_usd: Some(0.4321),
        }
    }

    /// Minimal modal for layout tests: paints `title` on its only row, ignores keys.
    struct FakeModal {
        title: String,
    }

    impl FakeModal {
        fn new(title: &str) -> Self {
            Self {
                title: title.to_owned(),
            }
        }
    }

    impl crate::tui::modal::Modal for FakeModal {
        fn height(&self, _width: u16, _max_height: u16) -> u16 {
            1
        }

        fn render(
            &self,
            frame: &mut ratatui::Frame<'_>,
            area: Rect,
            theme: &crate::tui::theme::Theme,
        ) {
            use ratatui::widgets::Paragraph;
            let line = Line::from(Span::styled(self.title.clone(), theme.text()));
            frame.render_widget(Paragraph::new(line).style(theme.surface()), area);
        }

        fn handle_key(&mut self, _event: &KeyEvent) -> crate::tui::modal::ModalKey {
            crate::tui::modal::ModalKey::Consumed
        }
    }

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn crossterm_test_terminal(
        width: u16,
        height: u16,
    ) -> (
        Terminal<CrosstermBackend<SharedWriter>>,
        Arc<Mutex<Vec<u8>>>,
    ) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let backend = CrosstermBackend::new(SharedWriter(buf.clone()));
        let opts = TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, width, height)),
        };
        (Terminal::with_options(backend, opts).unwrap(), buf)
    }

    fn render_app(app: &mut App, width: u16, height: u16) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw_frame(frame)).unwrap();
        terminal.backend().clone()
    }

    /// Renders the app and returns the buffer as a newline-joined
    /// string. Use when substring-asserting on the rendered UI is more
    /// readable than a full `insta::assert_snapshot!`.
    fn rendered_text(app: &mut App, width: u16, height: u16) -> String {
        let backend = render_app(app, width, height);
        let buffer = backend.buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn long_chat_block() -> String {
        use std::fmt::Write as _;

        let mut body = String::new();
        for i in 0..30 {
            _ = writeln!(body, "line {i:02} of a long chat block");
        }
        body
    }

    /// Pushes a Bash approval past its type-ahead guard, so it answers keys at once.
    fn push_answerable_approval(app: &mut App, command: &str) {
        let mut modal = crate::tui::modal::approval::ApprovalModal::new(
            "call-1".to_owned(),
            ApprovalPreview {
                title: "Bash".to_owned(),
                body: ApprovalBody::Command(command.to_owned()),
            },
            None,
        );
        modal.expire_guard();
        app.push_modal(Box::new(modal));
    }

    // ── App::new ──

    #[test]
    fn new_plumbs_resumed_title_into_status_bar() {
        let (app, _rx, _event_tx) = test_app(Some("Resumed title"));
        assert_eq!(app.status_bar.title(), Some("Resumed title"));
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert_eq!(app.chat.entry_count(), 0);
        assert!(app.input.is_enabled());
        assert!(!app.should_quit);
        assert!(app.dirty, "first frame must render");
    }

    #[test]
    fn new_without_title_leaves_slot_unset() {
        let (app, _rx, _event_tx) = test_app(None);
        assert!(app.status_bar.title().is_none());
    }

    #[test]
    fn new_whitespace_title_is_filtered_by_status_bar() {
        // Status bar filters whitespace-only titles so a resumed blank title doesn't leave a slot.
        let (app, _rx, _event_tx) = test_app(Some("   \n "));
        assert!(app.status_bar.title().is_none());
    }

    #[test]
    fn new_after_resume_unarchived_the_session_opens_with_a_notice() {
        let (_event_tx, events) = mpsc::unbounded_channel::<Event>();
        let (ops, _op_rx) = mpsc::channel::<Op>(1);
        let mut app = App::new(
            &Theme::default(),
            test_session_info(),
            false,
            events,
            ops,
            AppHistory {
                interactions: vec![Interaction::UserText("earlier".to_owned())],
                title: None,
                unarchived: true,
            },
        );

        assert_eq!(app.chat.entry_count(), 2);
        let text = rendered_text(&mut app, 80, 12);
        assert!(text.contains("Unarchived session test-ses."), "{text}");
    }

    // ── push_system_message ──

    #[test]
    fn push_system_message_replaces_welcome_with_notice() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.push_system_message("Model not supported.".to_owned());
        assert!(rendered_text(&mut app, 80, 24).contains("Model not supported."));
        assert!(!app.chat.is_empty());
    }

    // ── run_with_events ──

    #[tokio::test]
    async fn run_with_events_expires_armed_exit_on_tick_before_channel_close() {
        let (mut app, _rx, event_tx) = test_app(None);
        let until = Instant::now()
            .checked_sub(EXIT_WINDOW)
            .expect("monotonic clock has run long enough for a test deadline");
        app.status_bar.set_status(Status::ExitArmed { until });
        app.dirty = false;

        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);
        let closer = tokio::spawn(async move {
            tokio::time::sleep(TICK_INTERVAL * 2).await;
            drop(event_tx);
        });

        app.run_with_events(
            &mut terminal,
            futures::stream::pending::<std::io::Result<CrosstermEvent>>(),
        )
        .await
        .unwrap();
        closer.await.unwrap();

        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn run_with_events_marks_dirty_when_spinner_frame_advances() {
        // Sleep past the spinner interval so `tick()` reports a visible frame change.
        let (mut app, _rx, event_tx) = test_app(None);
        app.status_bar.set_status(Status::Streaming);
        app.dirty = false;

        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);
        let driver = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(event_tx);
        });

        app.run_with_events(
            &mut terminal,
            futures::stream::pending::<std::io::Result<CrosstermEvent>>(),
        )
        .await
        .unwrap();
        driver.await.unwrap();

        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn run_with_events_pumps_crossterm_and_engine_events_through_select() {
        // Pins both arms of `select!`: a key reaches input, a stream token disables it,
        // and closing the engine's event channel ends the loop.
        let (mut app, _rx, event_tx) = test_app(None);
        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);

        let key = Ok(CrosstermEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        let stream = futures::stream::iter(vec![key]).chain(futures::stream::pending());

        let driver = tokio::spawn(async move {
            event_tx.send(Event::StreamToken("hi".into())).unwrap();
            tokio::time::sleep(TICK_INTERVAL * 2).await;
            drop(event_tx);
        });

        app.run_with_events(&mut terminal, stream).await.unwrap();
        driver.await.unwrap();

        assert!(app.should_quit);
        assert_eq!(app.input.lines(), vec!["x"], "crossterm key reached input");
        assert!(
            !app.input.is_enabled(),
            "engine StreamToken disabled the input",
        );
    }

    #[tokio::test]
    async fn run_with_events_applies_git_probe_results() {
        let (mut app, _rx, event_tx) = test_app(None);
        app.git_probes
            .inject(GitUpdate::Branch(Some("feat/x".to_owned())));

        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);
        let closer = tokio::spawn(async move {
            tokio::time::sleep(TICK_INTERVAL * 2).await;
            drop(event_tx);
        });

        app.run_with_events(
            &mut terminal,
            futures::stream::pending::<std::io::Result<CrosstermEvent>>(),
        )
        .await
        .unwrap();
        closer.await.unwrap();

        assert_eq!(app.status_bar.git_branch(), Some("feat/x"));
    }

    #[tokio::test]
    async fn run_with_events_returns_terminal_read_errors() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);
        let stream = futures::stream::iter(vec![Err(io::Error::other("tty gone"))]);

        let err = app
            .run_with_events(&mut terminal, stream)
            .await
            .unwrap_err();

        assert!(format!("{err:#}").contains("tty gone"), "{err:#}");
    }

    #[tokio::test]
    async fn run_with_events_quits_when_the_terminal_stream_ends() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);

        tokio::time::timeout(
            Duration::from_secs(5),
            app.run_with_events(&mut terminal, futures::stream::empty()),
        )
        .await
        .expect("an ended stream must stop the loop")
        .unwrap();

        assert!(app.should_quit);
    }

    // ── handle_crossterm_event ──

    fn key_event(code: KeyCode, modifiers: KeyModifiers) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(code, modifiers))
    }

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
        // First press arms; second press inside the window confirms.
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
        app.handle_engine_event(Event::StreamToken("partial".into()));
        assert!(!app.input.is_enabled());

        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.should_quit, "busy Ctrl+C must not exit");
        let forwarded = rx.recv().await.expect("Interrupt forwarded to the engine");
        assert!(matches!(forwarded, Op::Interrupt));
    }

    #[tokio::test]
    async fn handle_crossterm_key_esc_busy_forwards_interrupt() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::StreamToken("partial".into()));

        app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.should_quit);
        let forwarded = rx.recv().await.expect("Interrupt forwarded to the engine");
        assert!(matches!(forwarded, Op::Interrupt));
    }

    #[test]
    fn handle_crossterm_event_mouse_is_forwarded_to_chat() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 60, 10);

        app.handle_crossterm_event(&CrosstermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        let text = rendered_text(&mut app, 60, 10);

        assert!(text.contains("Jump to bottom"), "{text}");
        assert!(app.dirty);
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
    fn handle_crossterm_unknown_event_is_a_noop() {
        // The `_ => return` arm prevents FocusGained / FocusLost from forcing re-renders.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dirty = false;
        app.handle_crossterm_event(&CrosstermEvent::FocusGained);
        assert!(!app.dirty);
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

        assert!(rx.try_recv().is_err(), "a release must not submit");
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
        // Alternate-scroll turns wheel ticks into Up, so an idle prompt must still scroll chat.
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
        app.handle_engine_event(Event::StreamToken("partial".into()));
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
        assert!(rx.try_recv().is_err(), "a paste never submits");
        assert_eq!(app.chat.entry_count(), 0);
        assert!(app.dirty);
    }

    #[tokio::test]
    async fn handle_crossterm_popup_enter_dispatches_canonical_command() {
        // `/h` filters to /help; Enter dispatches it. /help is read-only so chat
        // lands a system message and the engine receives nothing.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
        app.handle_crossterm_event(&key_event(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(app.input.popup_visible());

        app.handle_crossterm_event(&key_event(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!app.input.popup_visible(), "submit clears popup");
        assert!(
            !app.chat.last_is_error(),
            "/help must produce a system message, not an error",
        );
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "slash command stays client-side",
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
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "no Op must reach ops for a key the modal consumed",
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

        assert!(rx.try_recv().is_err(), "a paste must not decide the call");
        assert!(app.modals.is_active());
        assert!(app.input.is_empty());
    }

    #[test]
    fn modal_gate_cancel_closes_modal_without_dispatching() {
        // `ModalKey::Cancelled` pops the modal without dispatching; next key reaches input.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.push_modal(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::NONE));

        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "Cancelled must not dispatch any Op",
        );
        // Modal closed; subsequent keys reach the input.
        app.handle_crossterm_event(&key_event(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(app.input.lines(), vec!["y".to_owned()]);
    }

    #[test]
    fn handle_engine_event_approval_with_a_grant_offers_allow_for_session() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let grant = crate::permission::Policy::default()
            .session_grant(
                crate::permission::PermissionClass::Bash,
                &crate::permission::GateTarget::Command("cargo test".to_owned()),
            )
            .unwrap();
        app.handle_engine_event(Event::ApprovalRequested {
            id: "call-1".to_owned(),
            preview: ApprovalPreview {
                title: "Bash".to_owned(),
                body: ApprovalBody::Command("cargo test".to_owned()),
            },
            grant: Some(Box::new(grant)),
        });

        let text = rendered_text(&mut app, 100, 24);
        assert!(text.contains("[A] allow for session"), "{text}");
    }

    #[test]
    fn clear_modals_resolves_a_pending_approval_to_deny() {
        // A session swap clears the stack while an approval is outstanding. The cancel hook's Deny
        // must reach the engine so the blocked turn is not stranded waiting for a decision.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::ApprovalRequested {
            id: "call-1".to_owned(),
            preview: ApprovalPreview {
                title: "Bash(rm -rf /)".to_owned(),
                body: ApprovalBody::Command("rm -rf /".to_owned()),
            },
            grant: None,
        });

        app.clear_modals();

        let forwarded = rx
            .try_recv()
            .expect("clear must forward the cancel-hook deny");
        assert!(matches!(
            forwarded,
            Op::Resolve {
                id,
                decision: Decision::Deny,
            } if id == "call-1"
        ));
    }

    // ── handle_esc ──

    #[tokio::test]
    async fn handle_esc_busy_dispatches_interrupt() {
        // Esc is routed by `App` because its meaning depends on queue state and run-state.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::StreamToken("partial".into()));
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
        // The most-recent (back of FIFO) returns to input; the rest stay queued.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.pending_prompts.push_back("first".into());
        app.pending_prompts.push_back("second".into());

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
        app.pending_prompts.push_back("queued".into());
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
        // The popup gate sits in front of App's Esc routing — popup hides, queue stays put.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.pending_prompts.push_back("queued".into());
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

    #[tokio::test]
    async fn dispatch_slash_command_renders_locally_without_forwarding() {
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
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn dispatch_unknown_slash_command_renders_error_without_forwarding() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("/no-such".to_owned()));

        assert_eq!(app.chat.entry_count(), 2);
        assert!(app.input.is_enabled());
        assert!(app.chat.last_is_error());
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
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
            Some("/etc/hosts"),
            "a post-compaction re-echo shows the literal text",
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
        // `Interrupt` always goes through `try_send`, so it's the natural way to overflow
        // (busy-turn submits buffer locally instead).
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        for _ in 0..7 {
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
        assert!(app.chat.last_is_error());
    }

    #[test]
    fn dispatch_full_channel_on_approval_points_to_the_cancel_recovery() {
        // A dropped approval decision blocks the turn, unlike a dropped prompt, so the error must
        // name the recovery path rather than the generic prompt-dropped message.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        for _ in 0..8 {
            app.dispatch_action(Action::Op(Op::Interrupt));
        }

        app.dispatch_action(Action::Op(Op::Resolve {
            id: "call-1".to_owned(),
            decision: Decision::Approve,
        }));

        assert!(!app.should_quit, "Full is not fatal");
        assert!(app.chat.last_is_error());
        assert!(
            app.chat
                .last_error_text()
                .is_some_and(|t| t.contains("Esc to cancel")),
            "approval-drop error should name the cancel recovery",
        );
    }

    #[tokio::test]
    async fn dispatch_submit_during_busy_queues_and_forwards_for_mid_turn_drain() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("first".to_owned()));
        rx.recv().await.expect("first submit forwarded");

        app.dispatch_action(Action::Submit("queued".to_owned()));

        // Buffered for preview AND forwarded so `agent_turn` can splice it at the next
        // round boundary. Chat stays untouched until `PromptDrained` lands.
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
        // Pin: between forwarding `Resume` and the SessionResumed event landing, input must be
        // gated so a typed prompt doesn't push into chat just before `apply_session_resumed`'s
        // `clear_history` wipes it. `finalize_idle` inside the resumed handler re-enables input.
        let (mut app, mut rx, _event_tx) = test_app(None);
        let action = Op::Resume {
            path: "resume-target.jsonl".into(),
        };
        app.dispatch_op(action.clone());

        let forwarded = rx.recv().await.expect("Resume must reach the engine");
        assert_eq!(forwarded, action);
        assert!(
            !app.input.is_enabled(),
            "input must be gated until the resume event lands",
        );
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
    async fn dispatch_submit_during_cancelling_holds_locally_without_forwarding() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.dispatch_action(Action::Op(Op::Interrupt));
        rx.recv().await.expect("cancel forwarded");
        assert_eq!(app.status_bar.status(), &Status::Cancelling);

        app.dispatch_action(Action::Submit("typed-during-cancel".to_owned()));

        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "submit during cancel must not reach the engine",
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

        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "submit during compact must wait for SessionCompacted to drain it",
        );
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["typed-during-compact".to_owned()],
        );
    }

    #[tokio::test]
    async fn dispatch_read_only_slash_during_busy_runs_client_side_without_queueing() {
        // Read-only slash commands during busy run client-side immediately; queueing them
        // would make the LLM answer `/help` literally on drain.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        assert_eq!(app.status_bar.status(), &Status::Streaming);

        app.dispatch_action(Action::Submit("/help".to_owned()));

        assert!(
            app.pending_prompts.is_empty(),
            "slash command must not queue",
        );
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "slash command must not forward to ops",
        );
        // Only the original "active" prompt: /help is modal-only and suppresses its own echo.
        assert_eq!(app.chat.entry_count(), 1);
        assert!(!app.chat.last_is_error());
        assert!(app.modals.is_active(), "/help opens a modal");
    }

    #[tokio::test]
    async fn dispatch_clear_during_busy_refuses_with_system_message_no_dispatch() {
        // State-mutating commands refuse mid-turn — rolling while `messages` is still draining
        // would race the in-flight turn into the wrong JSONL.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");

        app.dispatch_action(Action::Submit("/clear".to_owned()));

        assert!(app.pending_prompts.is_empty());
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "neither a prompt nor Clear may reach the engine",
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
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("/init".to_owned()));

        assert_eq!(app.chat.entry_count(), 1, "only the typed `/init` line");
        assert!(!app.input.is_enabled(), "Streaming disables input");
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        let forwarded = rx.recv().await.expect("synthesized prompt forwarded");
        assert!(
            matches!(
                &forwarded,
                Op::Prompt(body) if body.contains("AGENTS.md") && body != "/init"
            ),
            "expected a prompt with the expanded body, got {forwarded:?}",
        );
    }

    #[tokio::test]
    async fn dispatch_init_during_busy_refuses_with_system_message_no_forward() {
        // Mutating: refuse. Typed `/init` row still lands; only the synthesized body suppresses.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");

        app.dispatch_action(Action::Submit("/init".to_owned()));

        assert!(app.pending_prompts.is_empty());
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "no synthesized prompt must reach ops mid-turn",
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
        // `/model <id>` and `/effort <level>` both mutate Client; pin both so a regression
        // that special-cases only one leaks the other.
        for (cmd, gate_phrase) in [
            ("/model opus", "/model runs only when idle"),
            ("/effort xhigh", "/effort runs only when idle"),
        ] {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.recv().await.expect("active submit forwarded");

            app.dispatch_action(Action::Submit(cmd.to_owned()));

            assert!(
                matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
                "{cmd}: action must not reach ops mid-turn",
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
            assert!(
                matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
                "{cmd}: action must not reach ops mid-turn",
            );
            let body = app.chat.last_system_text().expect("refusal system message");
            assert!(body.contains(gate_phrase), "{cmd}: refusal: {body}");
        }
    }

    #[tokio::test]
    async fn dispatch_unknown_slash_during_busy_renders_error_no_queue() {
        // Unknown commands route through `dispatch` so the user sees
        // the canonical "unknown command" error with recovery hints
        // (alternatives + `//` escape) instead of the prompt being silently sent to the LLM.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");

        app.dispatch_action(Action::Submit("/nope".to_owned()));

        assert!(app.pending_prompts.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
        ));
        assert!(app.chat.last_is_error());
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
        // Roster drift between slash + builtin tables logs via tracing only — user state stays put.
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

    #[tokio::test]
    async fn swap_theme_with_unknown_name_is_a_silent_noop() {
        // Only a dev bug can name an unknown built-in, so log via tracing and leave UI state
        // untouched.
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
            "no chat block — drift is dev-only",
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
        // A synthesized TUI-only action (e.g. `/theme latte`) must run the local theme arm in
        // `dispatch_action`.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("/theme latte".to_owned()));

        assert_eq!(
            app.session_info.config.theme_name, "latte",
            "typed `/theme <name>` must mutate session theme",
        );
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "TUI-only theme swap must not leak to the engine",
        );
    }

    #[tokio::test]
    async fn slash_typed_model_routes_synthesized_set_model_through_dispatch() {
        // Mirrors the typed-`/theme` regression on the engine-bound side: `/model <id>` synthesizes
        // a `SetModel` that must reach the engine via dispatch_action → dispatch_op.
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

    #[tokio::test]
    async fn submit_after_a_cancel_runs_the_held_queue_before_the_new_prompt() {
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

    #[tokio::test]
    async fn submit_busy_double_slash_queues_the_literal_and_keeps_the_escape() {
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
        assert_eq!(app.active_prompt.as_deref(), Some("/tmp"));
    }

    // ── apply_slash_outcome ──

    #[test]
    fn apply_slash_outcome_shows_output_and_hands_back_forwards_whether_idle_or_busy() {
        let diff = indoc::indoc! {"
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
                app.handle_engine_event(Event::StreamToken("partial".into()));
            }

            let clean = SlashOutcome::SystemMessage("Working tree clean.".to_owned());
            assert_eq!(app.apply_slash_outcome(Ok(clean)), None);
            assert_eq!(app.chat.last_system_text(), Some("Working tree clean."));

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
            assert!(
                rx.try_recv().is_err(),
                "the caller routes a forwarded action"
            );
        }
    }

    // ── handle_engine_event ──

    #[test]
    fn handle_session_title_updated_overwrites_existing_title_for_current_session() {
        let (mut app, _rx, _event_tx) = test_app(Some("First prompt"));
        app.handle_engine_event(Event::SessionTitleUpdated {
            session_id: app.session_info.session_id.clone(),
            title: "AI-generated".to_owned(),
        });
        assert_eq!(app.status_bar.title(), Some("AI-generated"));
        assert!(app.dirty);
    }

    #[test]
    fn handle_session_title_updated_drops_event_for_stale_session_id() {
        // Title task spawned before `/clear` finishes after the roll;
        // its event must not paint the old session's title onto the current one.
        let (mut app, _rx, _event_tx) = test_app(Some("First prompt"));
        app.handle_engine_event(Event::SessionTitleUpdated {
            session_id: "different-session".to_owned(),
            title: "Stale title".to_owned(),
        });
        assert_eq!(
            app.status_bar.title(),
            Some("First prompt"),
            "current-session title must survive a stale event",
        );
    }

    #[test]
    fn handle_config_changed_model_swap_refreshes_status_and_chat() {
        // Three surfaces refresh in one shot: status-bar label, `session_info` (backs `/status` /
        // `/config`), and a chat confirmation block.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::ConfigChanged {
            model: spec("claude-sonnet-5-5"),
            effort: Some(Effort::High),
            requested_effort: None,
            auto_compaction: AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(test_thresholds::WINDOW_1M),
            },
        });

        assert_eq!(app.session_info.config.model.id(), "claude-sonnet-5-5");
        assert_eq!(app.session_info.config.effort, Some(Effort::High),);
        assert_eq!(app.status_bar.model(), "Sonnet 5.5");
        assert_eq!(
            app.session_info.config.compaction.auto.threshold_tokens,
            Some(test_thresholds::WINDOW_1M),
        );
        let body = app.chat.last_system_text().expect("confirmation block");
        assert_eq!(
            body,
            format!(
                "Switched to Claude Sonnet 5.5 (claude-sonnet-5-5) · effort high. \
                 Auto compaction at {} tokens.",
                test_thresholds::WINDOW_1M,
            ),
        );
        assert!(app.dirty);
    }

    #[test]
    fn handle_config_changed_effort_only_keeps_status_bar_model_label() {
        // Effort-only swap leaves the cached model label alone; only
        // the snapshot effort and chat confirmation update.
        let (mut app, _rx, _event_tx) = test_app(None);
        let original_model = app.status_bar.model().to_owned();
        app.handle_engine_event(Event::ConfigChanged {
            model: app.session_info.config.model.clone(),
            effort: Some(Effort::Xhigh),
            requested_effort: Some(Effort::Xhigh),
            auto_compaction: app.session_info.config.compaction.auto,
        });
        assert_eq!(app.session_info.config.effort, Some(Effort::Xhigh),);
        assert_eq!(app.status_bar.model(), original_model);
        let body = app.chat.last_system_text().expect("confirmation block");
        assert_eq!(body, "Effort set to xhigh.");
        assert!(app.dirty);
    }

    // ── format_config_change ──

    #[test]
    fn format_config_change_swap_both_none_omits_effort_clause() {
        // Pin: no `effort` substring at all, never a stray "none"
        // word. Mutation that prints `effort none.` would surface here.
        let s = format_config_change(
            &spec("custom-model"),
            true,
            None,
            None,
            None,
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(s, "Switched to custom-model (custom-model).");
    }

    #[test]
    fn format_config_change_swap_to_no_tier_model_surfaces_requested_effort() {
        // Model swap onto a no-tier model with an explicit effort ask. The user needs to know
        // the requested tier didn't apply, otherwise a silent drop reads as a config change
        // landing.
        let s = format_config_change(
            &spec("custom-model"),
            true,
            None,
            None,
            Some(Effort::High),
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(
            s,
            "Switched to custom-model (custom-model). \
             Effort unchanged: model has no effort tier (asked for high).",
        );
    }

    #[test]
    fn format_config_change_swap_clears_effort_when_new_model_drops_it() {
        // User had a tier; new model has none. Surface the change so
        // the user knows their effort just disappeared.
        let s = format_config_change(
            &spec("custom-model"),
            true,
            Some(Effort::Xhigh),
            None,
            None,
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(
            s,
            "Switched to custom-model (custom-model). Effort cleared (model has no effort tier)."
        );
    }

    #[test]
    fn format_config_change_swap_marks_default_when_previous_was_none() {
        // None → Some means the new model's default kicked in;
        // distinguishing this from "user's pick survived" prevents
        // the user from thinking they chose this tier.
        let s = format_config_change(
            &spec("claude-opus-5-5"),
            true,
            None,
            Some(Effort::Medium),
            None,
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(
            s,
            "Switched to Claude Opus 5.5 (claude-opus-5-5) · effort medium (model default)."
        );
    }

    #[test]
    fn format_config_change_swap_reports_carried_or_picked_effort_plainly() {
        // A carried-over tier and an explicit pick both read as the plain effort clause. Pin
        // exact format so a stray suffix (`(unchanged)`) would fail.
        for (prev, requested, new) in [
            (Some(Effort::High), None, Effort::High),
            (Some(Effort::Medium), Some(Effort::Max), Effort::Max),
        ] {
            let s = format_config_change(
                &spec("claude-opus-5-5"),
                true,
                prev,
                Some(new),
                requested,
                base_auto_compaction(),
                base_auto_compaction(),
            );
            assert_eq!(
                s,
                format!("Switched to Claude Opus 5.5 (claude-opus-5-5) · effort {new}."),
            );
        }
    }

    #[test]
    fn format_config_change_effort_explicit_pick_matches_resolution() {
        let s = format_config_change(
            &spec("claude-opus-5-5"),
            false,
            Some(Effort::High),
            Some(Effort::Xhigh),
            Some(Effort::Xhigh),
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(s, "Effort set to xhigh.");
    }

    #[test]
    fn format_config_change_effort_pick_on_no_tier_model_surfaces_loss() {
        // The slash command preflight stops this through /effort, but
        // client-driven flows could still emit it.
        let s = format_config_change(
            &spec("custom-model"),
            false,
            None,
            None,
            Some(Effort::High),
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(
            s,
            "Effort unchanged: model has no effort tier (asked for high)."
        );
    }

    #[test]
    fn format_config_change_without_model_or_effort_reports_no_change() {
        let s = format_config_change(
            &spec("custom-model"),
            false,
            None,
            None,
            None,
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(s, "Config unchanged.");
    }

    #[test]
    fn format_config_change_model_swap_mentions_compaction_threshold_change() {
        let new_auto_compaction = AutoCompactionConfig {
            enabled: true,
            threshold_tokens: Some(test_thresholds::WINDOW_1M),
        };

        let s = format_config_change(
            &spec("claude-sonnet-5-5"),
            true,
            Some(Effort::High),
            Some(Effort::High),
            None,
            base_auto_compaction(),
            new_auto_compaction,
        );

        assert_eq!(
            s,
            format!(
                "Switched to Claude Sonnet 5.5 (claude-sonnet-5-5) · effort high. \
                 Auto compaction at {} tokens.",
                test_thresholds::WINDOW_1M,
            ),
        );
    }

    #[test]
    fn handle_usage_updated_sets_status_bar_usage() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let usage = usage_snapshot();

        app.handle_engine_event(Event::UsageUpdated(usage));

        assert_eq!(app.status_bar.usage(), Some(usage));
        assert!(app.dirty);
    }

    #[test]
    fn handle_session_rules_updated_replaces_the_rules_status_lists() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        let rules = ["bash(cargo test:*)", "edit(out/**)"].map(|text| Rule::parse(text).unwrap());

        app.handle_engine_event(Event::SessionRulesUpdated(vec![rules[0].clone()]));
        app.handle_engine_event(Event::SessionRulesUpdated(rules.to_vec()));

        assert_eq!(app.session_info.session_allow, rules);
        assert!(app.dirty);
    }

    #[test]
    fn handle_session_rolled_clears_chat_rebinds_id_and_drops_stale_title() {
        let (mut app, _rx, _event_tx) = test_app(Some("Old session title"));
        app.chat.push_user_message("old prompt".to_owned());
        let original_id = app.session_info.session_id.clone();
        app.status_bar.set_usage(Some(usage_snapshot()));
        app.session_info.session_allow = vec![Rule::parse("bash(ls)").unwrap()];

        app.handle_engine_event(Event::SessionRolled {
            id: "rolled-session".to_owned(),
            archived: false,
        });

        assert_eq!(
            app.session_info.session_id, "rolled-session",
            "session id must rebind to the rolled session",
        );
        assert_ne!(app.session_info.session_id, original_id);
        assert!(
            app.status_bar.title().is_none(),
            "stale AI title must be cleared on roll",
        );
        assert!(
            app.status_bar.usage().is_none(),
            "stale usage must be cleared on roll",
        );
        assert_eq!(app.session_info.session_allow, []);
        assert!(
            app.chat.is_empty(),
            "clear must drain the chat so the welcome can repaint",
        );
        assert!(app.dirty);
    }

    #[test]
    fn handle_session_rolled_after_archive_names_the_archived_session() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_user_message("old prompt".to_owned());
        app.session_info.session_id = "aaaa1111-archived".to_owned();

        app.handle_engine_event(Event::SessionRolled {
            id: "rolled-session".to_owned(),
            archived: true,
        });

        assert_eq!(app.session_info.session_id, "rolled-session");
        assert_eq!(app.chat.entry_count(), 1);
        let text = rendered_text(&mut app, 100, 12);
        assert!(text.contains("Archived session aaaa1111."), "{text}");
        assert!(!text.contains("old prompt"), "{text}");
    }

    #[test]
    fn handle_session_resumed_replays_transcript_and_clears_pending() {
        let (mut app, _rx, _event_tx) = test_app(Some("Old"));
        app.session_info.config.model = spec("claude-opus-5-5");
        app.chat.push_user_message("live prompt".to_owned());
        app.status_bar.set_usage(Some(usage_snapshot()));
        app.pending_prompts.push_back("queued".to_owned());
        app.handle_engine_event(bash_call_start("pending-1", "ls"));
        app.session_info.session_allow = vec![Rule::parse("bash(ls)").unwrap()];
        let original_id = app.session_info.session_id.clone();

        app.handle_engine_event(Event::SessionResumed {
            id: "resumed-session".to_owned(),
            title: Some("Resumed title".to_owned()),
            unarchived: false,
            history: vec![
                Interaction::UserText("resumed user".to_owned()),
                Interaction::AssistantText("resumed assistant".to_owned()),
            ],
        });

        assert_eq!(app.session_info.session_id, "resumed-session");
        assert_ne!(app.session_info.session_id, original_id);
        assert_eq!(app.status_bar.title(), Some("Resumed title"));
        assert!(
            app.status_bar.usage().is_none(),
            "stale usage must be cleared on resume",
        );
        assert_eq!(app.session_info.session_allow, []);
        assert_eq!(
            app.chat.entry_count(),
            3,
            "chat must reflect the resumed transcript + the queued-prompt-discarded notice",
        );
        assert_eq!(
            app.chat.pending_call_count(),
            0,
            "pending tool calls must drop on resume",
        );
        assert!(
            app.pending_prompts.is_empty(),
            "queued prompts must drop on resume",
        );
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled(), "resume returns to idle input");
        assert!(app.dirty);
    }

    #[test]
    fn handle_session_resumed_with_no_queued_prompts_is_silent() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.session_info.config.model = spec("claude-opus-5-5");
        app.handle_engine_event(Event::SessionResumed {
            id: "resumed".to_owned(),
            title: None,
            unarchived: false,
            history: vec![Interaction::UserText("only msg".to_owned())],
        });
        assert_eq!(
            app.chat.entry_count(),
            1,
            "no queued prompts → no discarded-prompts notice",
        );
    }

    #[test]
    fn handle_session_resumed_after_unarchiving_appends_a_notice() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.session_info.config.model = spec("claude-opus-5-5");
        app.handle_engine_event(Event::SessionResumed {
            id: "bbbb2222-resumed".to_owned(),
            title: None,
            unarchived: true,
            history: vec![Interaction::UserText("only msg".to_owned())],
        });

        assert_eq!(app.chat.entry_count(), 2);
        let text = rendered_text(&mut app, 80, 12);
        assert!(text.contains("Unarchived session bbbb2222."), "{text}");
    }

    #[test]
    fn handle_session_resumed_with_compact_replays_boundary_block() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.session_info.config.model = spec("claude-opus-5-5");
        app.handle_engine_event(Event::SessionResumed {
            id: "resumed".to_owned(),
            title: None,
            unarchived: false,
            history: vec![
                Interaction::Compacted {
                    summary: "Compact summary".to_owned(),
                    pre_count: 4,
                    instructions: None,
                },
                Interaction::AssistantText("post-compact reply".to_owned()),
            ],
        });

        assert_eq!(app.chat.entry_count(), 2);
        let text = rendered_text(&mut app, 80, 12);
        assert!(text.contains("Compacted 4 messages"));
        assert!(text.contains("Compact summary"));
        assert!(text.contains("post-compact reply"));
    }

    #[test]
    fn handle_session_resumed_with_no_title_clears_stale_chrome() {
        let (mut app, _rx, _event_tx) = test_app(Some("Stale"));
        app.session_info.config.model = spec("claude-opus-5-5");
        app.handle_engine_event(Event::SessionResumed {
            id: "resumed".to_owned(),
            title: None,
            unarchived: false,
            history: Vec::new(),
        });
        assert!(
            app.status_bar.title().is_none(),
            "Some(None) title must clear the chrome",
        );
        assert!(app.chat.is_empty());
    }

    #[test]
    fn handle_session_resumed_with_an_unsupported_model_repeats_its_notice() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::SessionResumed {
            id: "resumed".to_owned(),
            title: None,
            unarchived: false,
            history: vec![Interaction::UserText("only msg".to_owned())],
        });

        assert_eq!(app.chat.entry_count(), 2);
        let text = rendered_text(&mut app, 120, 16);
        let prompt_at = text.find("only msg").unwrap();
        let notice_at = text.find("`test-model` is not a supported model").unwrap();
        assert!(prompt_at < notice_at, "{text}");
    }

    #[test]
    fn handle_session_compacted_replays_summary_and_clears_pending_calls() {
        let (mut app, _rx, _event_tx) = test_app(Some("Pre-compact"));
        app.chat.push_user_message("pre-compact prompt".to_owned());
        app.status_bar.set_usage(Some(usage_snapshot()));
        app.handle_engine_event(bash_call_start("pending-1", "ls"));

        app.handle_engine_event(Event::SessionCompacted {
            summary: "## Recap\n\nDid the thing.".to_owned(),
            pre_count: 4,
            instructions: Some("focus on auth".to_owned()),
            automatic: false,
        });

        assert_eq!(
            app.chat.entry_count(),
            1,
            "chat must collapse to the single CompactedBlock",
        );
        assert_eq!(
            app.chat.pending_call_count(),
            0,
            "pending tool calls must drop on compact",
        );
        assert!(
            app.status_bar.usage().is_none(),
            "stale usage must be cleared on compact",
        );
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled(), "compact returns to idle input");
        assert!(app.dirty);
    }

    #[tokio::test]
    async fn handle_session_compacted_drains_queued_prompts_unlike_resume() {
        // Compact preserves user intent: a queued prompt becomes the first post-compact turn
        // rather than being dropped (the way `/resume` does, since `/resume` swaps identity).
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.pending_prompts
            .push_back("queued after compact".to_owned());

        app.handle_engine_event(Event::SessionCompacted {
            summary: "s".to_owned(),
            pre_count: 2,
            instructions: None,
            automatic: false,
        });

        let forwarded = rx.recv().await.expect("drained prompt reaches the engine");
        assert_eq!(
            forwarded,
            Op::Prompt("queued after compact".to_owned()),
            "queued prompt must drain as the next user turn",
        );
        assert!(app.pending_prompts.is_empty());
    }

    #[test]
    fn handle_session_compacted_without_instructions_renders_clean_block() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::SessionCompacted {
            summary: "summary only".to_owned(),
            pre_count: 2,
            instructions: None,
            automatic: false,
        });
        assert_eq!(app.chat.entry_count(), 1, "exactly one boundary block");
    }

    #[tokio::test]
    async fn handle_session_compacted_automatic_keeps_busy_state_and_queued_prompts() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.input.set_enabled(false);
        app.status_bar.set_status(Status::Compacting);
        app.pending_prompts
            .push_back("queued while busy".to_owned());

        app.handle_engine_event(Event::SessionCompacted {
            summary: "auto summary".to_owned(),
            pre_count: 4,
            instructions: None,
            automatic: true,
        });

        assert_eq!(app.chat.entry_count(), 1);
        assert_eq!(app.status_bar.status(), &Status::Compacting);
        assert!(!app.input.is_enabled());
        assert_eq!(app.pending_prompts.len(), 1);
        assert!(
            rx.try_recv().is_err(),
            "automatic compact must not drain early"
        );
    }

    #[test]
    fn handle_auto_compaction_started_sets_compacting_status() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active question".to_owned()));

        app.handle_engine_event(Event::AutoCompactionStarted);

        assert_eq!(app.status_bar.status(), &Status::Compacting);
        assert!(!app.input.is_enabled());
    }

    #[tokio::test]
    async fn handle_session_compacted_automatic_replays_active_prompt_after_summary() {
        let (mut app, mut rx, _event_tx) = test_app(None);

        app.dispatch_action(Action::Submit("active question".to_owned()));
        let forwarded = rx.recv().await.expect("prompt reaches the engine");
        assert_eq!(forwarded, Op::Prompt("active question".to_owned()));
        app.handle_engine_event(Event::AutoCompactionStarted);

        app.handle_engine_event(Event::SessionCompacted {
            summary: "auto summary".to_owned(),
            pre_count: 4,
            instructions: None,
            automatic: true,
        });

        assert_eq!(app.chat.entry_count(), 2);
        assert_eq!(app.status_bar.status(), &Status::Compacting);
        assert!(!app.input.is_enabled());
        let text = rendered_text(&mut app, 80, 10);
        assert!(text.contains("auto summary"));
        assert!(text.contains("active question"));
    }

    #[test]
    fn handle_stream_token_switches_to_streaming_and_disables_input() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::StreamToken("partial".to_owned()));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.input.is_enabled());
    }

    #[test]
    fn handle_thinking_token_routes_to_chat_and_marks_streaming() {
        // Thinking tokens land in the chat view as a separate block
        // (not interleaved with assistant text) and must flip the bar
        // to Streaming so the user sees the engine is working even before any visible text arrives.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::ThinkingToken("planning...".to_owned()));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        // Unlike StreamToken, thinking does not disable input on its own, since the matching
        // prompt already did that.
    }

    #[test]
    fn handle_progress_update_renders_note_in_chat() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::ProgressUpdate("Running the suite.".to_owned()));
        assert!(rendered_text(&mut app, 80, 24).contains("Running the suite."));
    }

    #[test]
    fn handle_tool_call_start_switches_to_tool_running() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(bash_call_start("t1", "ls"));
        assert_eq!(
            app.status_bar.status(),
            &Status::ToolRunning {
                name: "bash".to_owned()
            },
        );
        assert_eq!(
            app.chat.entry_count(),
            1,
            "tool call renders one chat entry",
        );
        assert!(rendered_text(&mut app, 60, 8).contains("$ ls"));
    }

    #[test]
    fn handle_cancelled_commits_partial_stream_with_marker_and_becomes_idle() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("hi".to_owned()));
        app.handle_engine_event(Event::StreamToken("partial answer".into()));
        let entries_before = app.chat.entry_count();
        assert_eq!(app.status_bar.status(), &Status::Streaming);

        app.handle_engine_event(Event::Cancelled);

        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled());
        // commit_streaming pushes the partial text as an AssistantText
        // block; push_interrupted_marker pushes the marker — two new
        // entries on top of whatever was there.
        assert_eq!(
            app.chat.entry_count(),
            entries_before + 2,
            "partial assistant text + interrupted marker",
        );
        let text = rendered_text(&mut app, 60, 12);
        assert!(text.contains("partial answer"), "stream tail kept: {text}");
        assert!(text.contains(INTERRUPTED_MARKER), "marker present: {text}");
    }

    #[test]
    fn cancelling_status_is_sticky_against_late_buffered_events() {
        // After the user dispatches Interrupt, the event channel may still
        // have queued StreamToken / ToolCallStart events the engine emitted
        // before its select arm dropped the turn future. Those buffered
        // events must not flip the bar back to Streaming / ToolRunning —
        // otherwise the cancel acknowledgement flickers off until `Cancelled` finally arrives.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("hi".to_owned()));
        app.dispatch_action(Action::Op(Op::Interrupt));
        assert_eq!(app.status_bar.status(), &Status::Cancelling);

        app.handle_engine_event(Event::StreamToken("late".into()));
        assert_eq!(app.status_bar.status(), &Status::Cancelling);
        app.handle_engine_event(bash_call_start("t-late", "ls"));
        assert_eq!(app.status_bar.status(), &Status::Cancelling);
    }

    #[test]
    fn handle_tool_call_end_renders_the_view_under_the_tool_title() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(bash_call_start("t1", "ls"));
        let before = app.chat.entry_count();
        app.handle_engine_event(Event::ToolCallEnd(ToolResult {
            id: "t1".to_owned(),
            content: "raw output".to_owned(),
            is_error: false,
            title: Some("Listed files".to_owned()),
            view: ToolResultView::Text {
                content: "viewed output".to_owned(),
            },
        }));
        assert_eq!(app.chat.entry_count(), before + 1);
        let text = rendered_text(&mut app, 60, 8);
        assert!(text.contains("Listed files"), "{text}");
        assert!(text.contains("viewed output"), "{text}");
        assert!(!text.contains("raw output"), "{text}");
    }

    #[test]
    fn handle_tool_call_end_without_title_falls_back_to_call_label() {
        // `title: None` means the tool didn't set a result header —
        // typically a failure path (timeout, invalid input) that
        // aborted before `.with_title(...)`. The result must still be
        // pushed (silent swallow would hide the error body) and the
        // header must render the tool-call label stashed at
        // `ToolCallStart`, not a blank string or the generic `(result)` fallback.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(bash_call_start("t1", "distinctive_label_xyz"));
        let before = app.chat.entry_count();
        app.handle_engine_event(text_call_end(
            "t1",
            "spawn failed: permission denied",
            true,
            None,
        ));
        assert_eq!(
            app.chat.entry_count(),
            before + 1,
            "result must render even when the tool did not set a title",
        );
        // The result header must be the stashed call label (the bash
        // command). It appears twice in the rendered view — once for
        // the tool call line, once for the result header — which is
        // what we want to confirm: both the call row and the result row carry the same label.
        let text = rendered_text(&mut app, 60, 8);
        let occurrences = text.matches("distinctive_label_xyz").count();
        assert_eq!(
            occurrences, 2,
            "expected call label on both the call and result rows, got {occurrences}:\n{text}",
        );
        assert!(
            !text.contains("(result)"),
            "generic fallback must not leak through when the pending call label is known, got:\n{text}",
        );
    }

    #[test]
    fn handle_tool_call_end_without_start_uses_generic_fallback_header() {
        // Defensive: an End without a matching Start (agent-layer bug
        // or dropped event) must still render so the user sees the
        // output. No pending entry → header falls back to `(result)`.
        let (mut app, _rx, _event_tx) = test_app(None);
        let before = app.chat.entry_count();
        app.handle_engine_event(text_call_end("orphan", "stray output", false, None));
        assert_eq!(app.chat.entry_count(), before + 1);
        // Viewport must fit header + body or auto-scroll hides the header.
        let text = rendered_text(&mut app, 60, 8);
        assert!(
            text.contains("(result)"),
            "orphan End with no pending call should use the generic fallback, got:\n{text}",
        );
    }

    #[tokio::test]
    async fn prompt_drained_pops_queue_head_and_pushes_user_message() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.dispatch_action(Action::Submit("queued-a".to_owned()));
        rx.recv().await.expect("queued-a submit forwarded");
        app.dispatch_action(Action::Submit("queued-b".to_owned()));
        rx.recv().await.expect("queued-b submit forwarded");

        let chat_before = app.chat.entry_count();
        app.handle_engine_event(Event::PromptDrained("queued-a".to_owned()));

        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["queued-b".to_owned()],
        );
        assert_eq!(
            app.chat.entry_count(),
            chat_before + 1,
            "drained prompt must push exactly one new user-message block",
        );
    }

    #[test]
    fn prompt_drained_outside_the_forwarded_queue_keeps_the_queue_and_shows_the_text() {
        // The engine only drains prompts it was handed, but a desync must still show the text.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.pending_prompts.push_back("held".into());
        let chat_before = app.chat.entry_count();

        app.handle_engine_event(Event::PromptDrained("held".to_owned()));

        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["held".to_owned()],
        );
        assert_eq!(app.chat.entry_count(), chat_before + 1);
    }

    #[tokio::test]
    async fn prompt_held_during_auto_compaction_forwards_ahead_of_later_submits() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("first".to_owned()));
        rx.recv().await.expect("first submit forwarded");
        app.handle_engine_event(Event::AutoCompactionStarted);
        app.dispatch_action(Action::Submit("held".to_owned()));
        assert!(rx.try_recv().is_err(), "held while compacting");

        app.handle_engine_event(Event::StreamToken("x".to_owned()));
        app.dispatch_action(Action::Submit("later".to_owned()));
        app.handle_engine_event(Event::PromptDrained("held".to_owned()));

        let forwarded: Vec<Op> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            forwarded,
            [Op::Queue("held".to_owned()), Op::Queue("later".to_owned())],
        );
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["later".to_owned()],
        );
    }

    #[test]
    fn handle_turn_complete_becomes_idle_and_reenables_input() {
        let (mut app, _rx, _event_tx) = test_app(None);
        // Drive into streaming first so TurnComplete has state to tear down.
        app.dispatch_action(Action::Submit("hi".to_owned()));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.input.is_enabled());

        app.handle_engine_event(Event::TurnComplete);
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled());
    }

    #[tokio::test]
    async fn turn_complete_starts_a_turn_with_the_queue_head_and_hands_it_the_rest() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.pending_prompts.push_back("a".into());
        app.pending_prompts.push_back("b".into());

        app.handle_engine_event(Event::TurnComplete);

        let forwarded: Vec<Op> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            forwarded,
            [Op::Prompt("a".to_owned()), Op::Queue("b".to_owned())],
        );
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["b".to_owned()],
        );
    }

    #[tokio::test]
    async fn turn_complete_after_a_cancel_request_leaves_the_queue_to_the_user() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.dispatch_action(Action::Submit("queued".to_owned()));
        rx.recv().await.expect("queued submit forwarded");
        app.handle_esc();
        assert_eq!(rx.recv().await, Some(Op::Interrupt));

        app.handle_engine_event(Event::TurnComplete);

        assert!(rx.try_recv().is_err(), "nothing resubmits after a cancel");
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["queued".to_owned()],
        );
    }

    #[test]
    fn handle_cancelled_with_no_stream_still_pushes_marker() {
        // Cancel during a tool call (or before any stream tokens) —
        // commit_streaming is a no-op, but the marker still lands so
        // the user sees where the cancel hit.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("hi".to_owned()));
        let entries_before = app.chat.entry_count();

        app.handle_engine_event(Event::Cancelled);

        assert_eq!(app.chat.entry_count(), entries_before + 1);
        let text = rendered_text(&mut app, 60, 8);
        assert!(text.contains(INTERRUPTED_MARKER), "marker present: {text}");
    }

    #[tokio::test]
    async fn cancelled_goes_idle_and_esc_recalls_the_queued_prompt() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("first".to_owned()));
        rx.recv().await.expect("first submit forwarded");
        app.handle_engine_event(Event::StreamToken("x".to_owned()));
        app.dispatch_action(Action::Submit("second".to_owned()));
        rx.recv().await.expect("second submit forwarded");
        app.handle_esc();
        assert_eq!(rx.recv().await, Some(Op::Interrupt));

        app.handle_engine_event(Event::Cancelled);

        assert!(rx.try_recv().is_err(), "nothing resubmits after a cancel");
        assert_eq!(app.status_bar.status(), &Status::Idle);
        app.handle_esc();
        assert_eq!(app.input.lines(), ["second".to_owned()]);
        assert!(app.pending_prompts.is_empty());
    }

    #[tokio::test]
    async fn handle_retrying_and_error_notices_keep_the_turn_running() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.dispatch_action(Action::Submit("queued".to_owned()));
        rx.recv().await.expect("queued submit forwarded");

        app.handle_engine_event(Event::Retrying(
            "HTTP 529 overloaded_error. Retrying in 1s (retry 1 of 5).".to_owned(),
        ));
        assert!(!app.chat.last_is_error(), "a retry is not an error block");
        app.handle_engine_event(Event::Error("Session write failed: disk full".to_owned()));

        assert!(app.chat.last_is_error(), "error entry appended");
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.input.is_enabled());
        assert!(rx.try_recv().is_err(), "nothing resubmits mid-turn");
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["queued".to_owned()],
        );
    }

    #[test]
    fn handle_turn_failed_shows_the_error_and_finishes_the_turn() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("boom".to_owned()));
        app.handle_engine_event(Event::TurnFailed("API blew up".to_owned()));

        assert!(app.chat.last_is_error(), "error entry appended");
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled());
    }

    #[tokio::test]
    async fn handle_turn_failed_shows_the_error_and_starts_the_queue_head_once() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.pending_prompts.push_back("first".into());
        app.pending_prompts.push_back("second".into());
        let chat_before = app.chat.entry_count();

        app.handle_engine_event(Event::TurnFailed("API failed".to_owned()));

        assert_eq!(
            app.chat.entry_count(),
            chat_before + 2,
            "error block, then the next prompt"
        );
        let forwarded: Vec<Op> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            forwarded,
            [
                Op::Prompt("first".to_owned()),
                Op::Queue("second".to_owned()),
            ],
        );
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["second".to_owned()],
        );
    }

    #[tokio::test]
    async fn pending_queue_resubmits_prompts_each_ended_turn_dropped() {
        // A turn that ends before its next round boundary drops the queued prompts it held, so
        // each one must come back as a later turn.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        for prompt in ["a", "b", "c"] {
            app.dispatch_action(Action::Submit(prompt.to_owned()));
        }
        let mut forwarded = || std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(forwarded().len(), 3);

        app.handle_engine_event(Event::TurnFailed(
            "agent stopped after 3 tool rounds".to_owned(),
        ));
        assert_eq!(
            forwarded(),
            [
                Op::Prompt("a".to_owned()),
                Op::Queue("b".to_owned()),
                Op::Queue("c".to_owned()),
            ],
        );

        app.handle_engine_event(Event::TurnComplete);
        assert_eq!(
            forwarded(),
            [Op::Prompt("b".to_owned()), Op::Queue("c".to_owned())],
        );

        app.handle_engine_event(Event::TurnComplete);
        assert_eq!(forwarded(), [Op::Prompt("c".to_owned())]);
        assert!(app.pending_prompts.is_empty());
    }

    // ── finish_turn ──

    #[test]
    fn finish_turn_evicts_orphaned_pending_calls() {
        // `ToolCallStart` without a matching `ToolCallEnd` by turn-end
        // is an orphan (crashed tool, agent-loop bug, mid-turn abort).
        // The pending entry must be discarded so long sessions don't
        // accumulate stale ids across turns.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(bash_call_start("orphan", "ls"));
        assert_eq!(app.chat.pending_call_count(), 1);
        app.handle_engine_event(Event::TurnComplete);
        assert_eq!(
            app.chat.pending_call_count(),
            0,
            "turn end must evict calls whose result never arrived",
        );
    }

    // ── expire_armed_exit ──

    #[test]
    fn expire_armed_exit_becomes_idle_after_window() {
        // After the 1 s window the armed state evaporates so the user
        // isn't left staring at an exit hint they didn't act on.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::ConfirmExit);
        // Force an expired deadline so the test doesn't sleep.
        let until = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .expect("monotonic clock has run for at least one second since boot");
        app.status_bar.set_status(Status::ExitArmed { until });

        assert!(app.expire_armed_exit(), "stale armed state must clear");
        assert_eq!(app.status_bar.status(), &Status::Idle);
    }

    #[test]
    fn expire_armed_exit_when_not_armed_is_a_noop() {
        // Tick path calls `expire_armed_exit` every frame regardless of
        // status; the false branch must leave the bar untouched and
        // skip the dirty bump that would otherwise wake the renderer.
        let (mut app, _rx, _event_tx) = test_app(None);
        assert_eq!(app.status_bar.status(), &Status::Idle);

        assert!(
            !app.expire_armed_exit(),
            "no-op when status isn't ExitArmed",
        );
        assert_eq!(app.status_bar.status(), &Status::Idle);
    }

    // ── apply_git_update ──

    #[test]
    fn apply_git_update_marks_dirty_only_when_the_status_bar_changes() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dirty = false;

        app.apply_git_update(GitUpdate::Branch(Some("main".to_owned())));
        assert!(!app.dirty, "unchanged branch skips the repaint");

        app.apply_git_update(GitUpdate::PullRequest(Some(
            crate::util::git::PullRequest {
                number: 7,
                url: "https://github.com/o/r/pull/7".to_owned(),
            },
        )));
        assert!(app.dirty, "new pull request repaints");
    }

    // ── render ──

    const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
    const END_SYNC: &[u8] = b"\x1b[?2026l";

    fn index_of(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    #[test]
    fn render_brackets_frame_with_sync_update_bytes() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let (mut terminal, buf) = crossterm_test_terminal(60, 8);

        app.render(&mut terminal).unwrap();

        let bytes = buf.lock().unwrap();
        let begin = index_of(&bytes, BEGIN_SYNC).expect("BeginSynchronizedUpdate emitted");
        let end = index_of(&bytes, END_SYNC).expect("EndSynchronizedUpdate emitted");
        assert!(begin < end, "sync update must bracket the rendered frame");
    }

    #[test]
    fn render_replays_status_hyperlinks_inside_the_synchronized_update() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.apply_git_update(GitUpdate::PullRequest(Some(
            crate::util::git::PullRequest {
                number: 7,
                url: "https://example.com/pull/7".to_owned(),
            },
        )));
        let (mut terminal, buf) = crossterm_test_terminal(120, 8);

        app.render(&mut terminal).unwrap();

        let bytes = buf.lock().unwrap();
        let link = index_of(&bytes, b"\x1b]8;;https://example.com/pull/7").expect("OSC 8 emitted");
        let end = index_of(&bytes, END_SYNC).expect("EndSynchronizedUpdate emitted");
        assert!(link < end, "sync update must bracket the status links");
    }

    #[test]
    fn render_repaints_when_chat_content_grows_past_viewport() {
        use std::fmt::Write as _;

        // Content pushed in the same handler tick must land before a post-paint re-clamp.
        let (mut app, _rx, _event_tx) = test_app(None);
        let mut body = String::new();
        for i in 0..40 {
            _ = writeln!(body, "line {i:02} of a long system block");
        }
        app.chat.push_system_message(body);
        let text = rendered_text(&mut app, 60, 12);
        assert!(
            text.contains("line 39"),
            "tail of overflowing block must be in the viewport after the first render, got:\n{text}",
        );
    }

    // ── emit_status_hyperlinks ──

    #[test]
    fn emit_status_hyperlinks_is_a_noop_when_no_links_pending() {
        #[derive(Clone)]
        struct SharedSink(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for SharedSink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (mut app, _rx, _event_tx) = test_app(None);
        let bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
        let backend = CrosstermBackend::new(SharedSink(bytes.clone()));
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();
        app.emit_status_hyperlinks(&mut terminal).unwrap();
        assert!(
            bytes.lock().unwrap().is_empty(),
            "no bytes written for a no-link frame",
        );
    }

    // ── draw_frame ──

    #[test]
    fn draw_frame_lays_out_status_chat_and_input_in_order() {
        let (mut app, _rx, _event_tx) = test_app(Some("Session title"));
        insta::assert_snapshot!(render_app(&mut app, 80, 10));
    }

    #[test]
    fn draw_frame_with_conversation_and_tool_call() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_user_message("what files are here?".into());
        app.chat.push_tool_call(ToolCall {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": "ls"}),
            icon: "$".to_owned(),
            label: "ls".to_owned(),
        });
        app.chat.push_tool_result(ToolResult {
            id: "t1".to_owned(),
            content: "README.md\nCargo.toml".to_owned(),
            is_error: false,
            title: Some("ran ls".to_owned()),
            view: ToolResultView::Text {
                content: "README.md\nCargo.toml".to_owned(),
            },
        });
        app.chat.append_stream_token("Two files.");
        app.chat.commit_streaming();
        insta::assert_snapshot!(render_app(&mut app, 60, 12));
    }

    #[test]
    fn draw_frame_streaming_shows_spinner_in_status_bar() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("working...".into()));
        app.handle_engine_event(Event::StreamToken("part".into()));
        insta::assert_snapshot!(render_app(&mut app, 60, 8));
    }

    #[test]
    fn draw_frame_auto_scroll_on_hides_jump_overlay() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());

        let text = rendered_text(&mut app, 60, 10);
        assert!(!text.contains("Jump to bottom"), "{text}");
    }

    #[test]
    fn draw_frame_scrolled_up_shows_jump_overlay() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 60, 10);

        app.chat
            .handle_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
        let text = rendered_text(&mut app, 60, 10);

        assert!(text.contains("Jump to bottom"), "{text}");
    }

    #[test]
    fn draw_frame_scrolled_up_counts_new_content() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 60, 10);
        app.chat
            .handle_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));

        app.chat.push_error("background update");
        let text = rendered_text(&mut app, 60, 10);

        assert!(text.contains("1 new message"), "{text}");
    }

    #[test]
    fn draw_frame_narrow_width_still_renders_all_three_panels() {
        let (mut app, _rx, _event_tx) = test_app(Some("narrow"));
        app.chat.push_user_message("hi".into());
        insta::assert_snapshot!(render_app(&mut app, 40, 8));
    }

    #[test]
    fn draw_frame_renders_slash_popup_above_input_when_visible() {
        // Typing `/` opens the popup; draw_frame must reserve a band
        // above the input and paint at least one command row into it.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(app.input.popup_visible());

        let text = rendered_text(&mut app, 60, 14);
        assert!(
            text.contains("/archive"),
            "popup must paint at least one canonical command row: {text}",
        );
    }

    #[test]
    fn draw_frame_preview_panel_renders_queued_prompts_and_overflow_tag() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".into()));
        for i in 0..5 {
            app.pending_prompts.push_back(format!("queued {i}"));
        }
        insta::assert_snapshot!(render_app(&mut app, 60, 14));
    }

    #[test]
    fn draw_frame_hides_input_and_popup_while_modal_active() {
        // Modal owns focus, so the layout must collapse the input + popup bands. The user-prompt
        // marker (`❯`) is the cheapest substring proof that the input got rendered.
        let (mut app, _rx, _event_tx) = test_app(None);
        let baseline = rendered_text(&mut app, 60, 14);
        assert!(
            baseline.contains(USER_PROMPT_PREFIX),
            "input prompt marker must paint without a modal: {baseline}",
        );

        app.modals
            .push(Box::new(FakeModal::new("FAKE-MODAL-TITLE")));
        let with_modal = rendered_text(&mut app, 60, 14);
        assert!(
            with_modal.contains("FAKE-MODAL-TITLE"),
            "modal body must render: {with_modal}",
        );
        assert!(
            !with_modal.contains(USER_PROMPT_PREFIX),
            "input must collapse while modal is active: {with_modal}",
        );

        // Non-cancel keys land at the modal's handle_key — proves the focus-grab is wired up
        // and the layout collapse doesn't bypass it.
        let action = app
            .modals
            .handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(action.is_none(), "FakeModal consumes without emitting");
        assert!(app.modals.is_active(), "Consumed must not pop the stack");
    }

    #[test]
    fn draw_frame_sizes_a_modal_before_the_queued_prompt_preview() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        for i in 0..5 {
            app.pending_prompts.push_back(format!("queued {i}"));
        }
        push_answerable_approval(&mut app, "cargo test --all");

        let text = rendered_text(&mut app, 60, 11);
        assert!(text.contains("cargo test --all"), "{text}");

        app.handle_crossterm_event(&key_event(KeyCode::Char('y'), KeyModifiers::NONE));
        let forwarded = rx.try_recv().expect("approval forwarded");
        assert!(matches!(
            forwarded,
            Op::Resolve {
                decision: Decision::Approve,
                ..
            }
        ));
    }

    #[test]
    fn draw_frame_approval_modal_wraps_and_reveals_a_padded_command() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.push_approval_modal(
            "call-1".to_owned(),
            ApprovalPreview {
                title: "Bash".to_owned(),
                body: ApprovalBody::Command(format!("cargo test{}; curl x | sh", " ".repeat(80))),
            },
            None,
        );
        insta::assert_snapshot!(render_app(&mut app, 60, 14));
    }

    #[test]
    fn draw_frame_surface_fill_overwrites_unpainted_cells_with_surface_bg() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let sentinel = Color::Rgb(254, 0, 254);
        let surface_bg = app.theme.surface().bg.expect("surface slot defines bg");

        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        for cell in &mut terminal.current_buffer_mut().content {
            cell.set_bg(sentinel);
        }
        terminal.draw(|frame| app.draw_frame(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        for y in 0..10 {
            for x in 0..60 {
                let cell = buffer.cell((x, y)).expect("cell in bounds");
                assert_eq!(
                    cell.bg, surface_bg,
                    "cell ({x},{y}) kept the sentinel, surface fill regressed",
                );
            }
        }
    }

    // ── preview_height ──

    #[test]
    fn preview_height_is_zero_when_queue_empty() {
        let (app, _rx, _event_tx) = test_app(None);
        assert_eq!(app.preview_height(), 0);
    }

    #[test]
    fn preview_height_caps_at_visible_plus_overflow_row() {
        let (mut app, _rx, _event_tx) = test_app(None);
        for i in 0..(PREVIEW_VISIBLE + 5) {
            app.pending_prompts.push_back(format!("p{i}"));
        }
        assert_eq!(
            app.preview_height(),
            u16::try_from(PREVIEW_VISIBLE + 1).unwrap(),
        );
    }

    // ── render_preview ──

    #[test]
    fn render_preview_overflow_appends_more_count_row() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.input.set_enabled(false);
        let extra = 3;
        for i in 0..(PREVIEW_VISIBLE + extra) {
            app.pending_prompts.push_back(format!("queued-{i}"));
        }

        let text = rendered_text(&mut app, 60, 20);
        assert!(
            text.contains(&format!("+{extra} more")),
            "overflow hint must show exact extra count: {text}",
        );
    }

    // ── jump_overlay_label ──

    #[test]
    fn jump_overlay_label_idle_reads_jump_to_bottom() {
        assert_eq!(jump_overlay_label(0, 60), "Jump to bottom (ctrl+End) ↓");
    }

    #[test]
    fn jump_overlay_label_pluralizes_new_message_count() {
        assert_eq!(jump_overlay_label(1, 60), "1 new message (ctrl+End) ↓");
        assert_eq!(jump_overlay_label(3, 60), "3 new messages (ctrl+End) ↓");
    }

    #[test]
    fn jump_overlay_label_uses_short_form_below_full_width() {
        assert_eq!(jump_overlay_label(3, 30), "↓ (ctrl+End)");
    }
}
