//! Root TUI application.
//!
//! [`App`] owns every component, holds the cross-task channels, and runs the `tokio::select!`
//! loop multiplexing crossterm events, agent events, user actions, and a 60 FPS render tick.
//! A dirty flag coalesces redraws so renders fire per state change rather than per event.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent};
use futures::{Stream, StreamExt};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

use super::components::chat::ChatView;
use super::components::input::InputArea;
use super::components::status::{Status, StatusBar};
use super::components::welcome::{self, WelcomeSnapshot};
use super::glyphs::{NEWLINE_GLYPH, USER_PROMPT_PREFIX, USER_PROMPT_PREFIX_WIDTH};
use super::modal::{ModalAction, ModalStack};
use super::pending_calls::{PendingCall, PendingCalls, result_header};
use super::terminal::{Tui, draw_sync};
use super::theme::Theme;
use crate::agent::event::{AgentEvent, UserAction};
use crate::config::{CompactionConfig, Effort, display_auto_compaction};
use crate::message::Message;
use crate::session::entry::CompactInfo;
use crate::slash::{self, LiveSessionInfo, SlashContext, SlashKind};
use crate::tool::{ToolMetadata, ToolRegistry, ToolResultView};
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
    chat: ChatView,
    input: InputArea,
    session_info: LiveSessionInfo,
    agent_rx: mpsc::Receiver<AgentEvent>,
    user_tx: mpsc::Sender<UserAction>,
    tools: Arc<ToolRegistry>,
    /// Correlates `ToolCallStart` with its matching `ToolCallEnd`.
    pending_calls: PendingCalls,
    /// Prompt already painted for the in-flight turn. Replayed if auto-compaction clears chat.
    active_prompt: Option<String>,
    /// FIFO of prompts submitted mid-turn. Drained at turn boundaries.
    pending_prompts: VecDeque<String>,
    modals: ModalStack,
    /// Theme saved when a `/theme` picker opens. Restored if the modal cancels.
    preview_theme_snapshot: Option<Theme>,
    should_quit: bool,
    dirty: bool,
}

pub(crate) struct AppHistory<'a> {
    pub(crate) messages: &'a [Message],
    pub(crate) compact: Option<&'a CompactInfo>,
    pub(crate) tool_metadata: &'a HashMap<String, ToolMetadata>,
    pub(crate) title: Option<String>,
}

impl App {
    pub(crate) fn new(
        theme: &Theme,
        session_info: LiveSessionInfo,
        show_thinking: bool,
        agent_rx: mpsc::Receiver<AgentEvent>,
        user_tx: mpsc::Sender<UserAction>,
        tools: Arc<ToolRegistry>,
        history: AppHistory<'_>,
    ) -> Self {
        let mut chat = ChatView::new(theme, show_thinking);
        chat.load_history(
            history.messages,
            history.compact,
            history.tool_metadata,
            tools.as_ref(),
        );
        let mut status_bar = StatusBar::new(
            theme,
            session_info.config.status_line.clone(),
            session_info.short_display_name().into_owned(),
            session_info.config.effort,
            session_info.cwd.clone(),
            session_info.git_branch.clone(),
        );
        status_bar.set_title(history.title);
        Self {
            theme: theme.clone(),
            status_bar,
            chat,
            input: InputArea::new(theme),
            session_info,
            agent_rx,
            user_tx,
            tools,
            pending_calls: PendingCalls::new(),
            active_prompt: None,
            pending_prompts: VecDeque::new(),
            modals: ModalStack::new(),
            preview_theme_snapshot: None,
            should_quit: false,
            dirty: true,
        }
    }

    /// Runs until the user quits or the agent channel closes.
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
        S: Stream<Item = std::io::Result<Event>> + Unpin,
    {
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        self.render(terminal)?;

        loop {
            // Three sources can wake the loop. The tick arm is the only renderer, so handlers only
            // mark `dirty` and render work stays paced by the 60 FPS clock.
            tokio::select! {
                    event = crossterm_events.next() => {
                    if let Some(Ok(event)) = event {
                        self.handle_crossterm_event(&event);
                    }
                }
                event = self.agent_rx.recv() => {
                    match event {
                        Some(event) => self.handle_agent_event(event),
                        // Agent channel closed. Quit instead of spinning.
                        None => self.should_quit = true,
                    }
                }
                _ = tick.tick() => {
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

    fn handle_crossterm_event(&mut self, event: &Event) {
        // Modal keys belong to the top overlay. Mouse and resize events still reach chat.
        if let Event::Key(key) = event
            && self.modals.is_active()
        {
            if let Some(action) = self.modals.handle_key(key) {
                self.apply_modal_action(action);
            }
            self.dirty = true;
            return;
        }
        match event {
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            }) => {
                if self.input.popup_visible() {
                    _ = self.input.handle_event(event);
                } else {
                    self.handle_esc();
                }
            }
            Event::Key(..) => {
                if let Some(action) = self.input.handle_event(event) {
                    self.dispatch_user_action(action);
                }
                if !self.input.is_enabled() {
                    self.chat.handle_event(event);
                }
            }
            Event::Mouse(..) => {
                self.chat.handle_event(event);
            }
            Event::Resize(..) => {}
            _ => return,
        }
        self.dirty = true;
    }

    fn apply_modal_action(&mut self, action: ModalAction) {
        match action {
            // Cancel: revert any in-flight theme preview to the snapshot taken on open.
            ModalAction::None => self.cancel_theme_preview(),
            ModalAction::User(user_action) => self.dispatch_user_action(user_action),
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
        self.modals.clear();
    }

    /// Repaints every theme-styled component for a mid-session theme swap.
    fn apply_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
        self.chat.set_theme(theme);
        self.status_bar.set_theme(theme);
        self.input.set_theme(theme);
        self.dirty = true;
    }

    #[cfg(test)]
    pub(crate) fn push_modal(&mut self, modal: Box<dyn super::modal::Modal>) {
        self.modals.push(modal);
        self.dirty = true;
    }

    /// Cancels if busy, restores one queued prompt if idle, or no-ops.
    fn handle_esc(&mut self) {
        if !self.input.is_enabled() {
            self.dispatch_user_action(UserAction::Cancel);
        } else if self.input.is_empty()
            && let Some(prompt) = self.pending_prompts.pop_back()
        {
            self.input.set_text(&prompt);
        }
    }

    /// Applies UI side-effects then forwards to the agent channel.
    ///
    /// `apply_action_locally` returns `false` when the action was fully handled in the UI (slash
    /// command synthesized its own forward, prompt was queued mid-turn, exit-arm was set, etc.),
    /// gating the forward so the agent loop never sees actions that were never meant for it.
    fn dispatch_user_action(&mut self, action: UserAction) {
        if !self.apply_action_locally(&action) {
            return;
        }
        self.forward_to_agent(action);
    }

    /// Sends `action` to the agent loop. Channel errors surface as chat error blocks.
    fn forward_to_agent(&mut self, action: UserAction) {
        if let Err(e) = self.user_tx.try_send(action) {
            match e {
                mpsc::error::TrySendError::Closed(_) => {
                    self.chat
                        .push_error("agent task exited unexpectedly; restart `ox` to recover");
                    self.input.set_enabled(false);
                    self.should_quit = true;
                }
                mpsc::error::TrySendError::Full(_) => {
                    self.chat
                        .push_error("user-action channel full; prompt dropped (this is a bug)");
                }
            }
        }
    }

    /// Applies UI-state changes and returns whether to forward to the agent.
    fn apply_action_locally(&mut self, action: &UserAction) -> bool {
        match action {
            UserAction::SubmitPrompt(text) => self.handle_submit_prompt(text),
            UserAction::Cancel => {
                self.status_bar.set_status(Status::Cancelling);
                true
            }
            UserAction::ConfirmExit => {
                if let Status::ExitArmed { until } = self.status_bar.status()
                    && Instant::now() < *until
                {
                    self.should_quit = true;
                } else {
                    let until = Instant::now() + EXIT_WINDOW;
                    self.status_bar.set_status(Status::ExitArmed { until });
                }
                false
            }
            UserAction::Quit => {
                self.should_quit = true;
                true
            }
            UserAction::Clear | UserAction::Rename { .. } | UserAction::SwapConfig { .. } => true,
            UserAction::Resume { .. } => {
                self.input.set_enabled(false);
                true
            }
            UserAction::Compact { .. } => {
                self.input.set_enabled(false);
                self.status_bar.set_status(Status::Compacting);
                true
            }
            UserAction::PreviewTheme { name } => {
                if let Some(preview) = super::theme::load_builtin(name) {
                    if self.preview_theme_snapshot.is_none() {
                        self.preview_theme_snapshot = Some(self.theme.clone());
                    }
                    self.apply_theme(&preview);
                } else {
                    tracing::warn!(name, "PreviewTheme: unknown built-in; picker roster drift");
                }
                false
            }
            UserAction::SwapTheme { name } => {
                self.preview_theme_snapshot = None;
                if let Some(theme) = super::theme::load_builtin(name) {
                    self.session_info.config.theme_name.clone_from(name);
                    self.apply_theme(&theme);
                    self.chat
                        .push_system_message(format!("Theme set to {name}."));
                } else {
                    tracing::warn!(name, "SwapTheme: unknown built-in; picker roster drift");
                }
                false
            }
        }
    }

    /// Returns whether submitted text should also forward to the agent. Slash commands emit their
    /// own synthesized actions, so plain prompts are the only forwarded submissions.
    fn handle_submit_prompt(&mut self, text: &str) -> bool {
        if self.input.is_enabled() {
            if let Some(parsed) = slash::parse_slash(text) {
                if slash::echoes_input(&parsed) {
                    self.chat.push_user_message(text.to_owned());
                }
                let (synthesized, modal) = {
                    let mut ctx = SlashContext::with_title(
                        &mut self.chat,
                        &self.session_info,
                        self.status_bar.title(),
                    );
                    let action = slash::dispatch(&parsed, &mut ctx);
                    (action, ctx.take_modal())
                };
                if let Some(modal) = modal {
                    self.modals.push(modal);
                }
                if let Some(action) = synthesized {
                    if matches!(action, UserAction::SubmitPrompt(_)) {
                        if slash::echoes_input(&parsed) {
                            self.active_prompt = Some(text.to_owned());
                        }
                        self.input.set_enabled(false);
                        self.status_bar.set_status(Status::Streaming);
                        self.forward_to_agent(action);
                    } else {
                        self.dispatch_user_action(action);
                    }
                }
                return false;
            }
            self.chat.push_user_message(text.to_owned());
            self.active_prompt = Some(text.to_owned());
            self.input.set_enabled(false);
            self.status_bar.set_status(Status::Streaming);
            return true;
        }
        if let Some(parsed) = slash::parse_slash(text) {
            if slash::echoes_input(&parsed) {
                self.chat.push_user_message(text.to_owned());
            }
            match slash::classify(&parsed) {
                SlashKind::ReadOnly | SlashKind::Unknown => {
                    let modal = {
                        let mut ctx = SlashContext::new(&mut self.chat, &self.session_info);
                        _ = slash::dispatch(&parsed, &mut ctx);
                        ctx.take_modal()
                    };
                    if let Some(modal) = modal {
                        self.modals.push(modal);
                    }
                }
                SlashKind::Mutating => {
                    self.chat.push_system_message(format!(
                        "/{} runs only when idle. Try again after the turn finishes.",
                        parsed.name,
                    ));
                }
            }
            return false;
        }
        self.pending_prompts.push_back(text.to_owned());
        !matches!(
            self.status_bar.status(),
            Status::Compacting | Status::Cancelling,
        )
    }

    fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::StreamToken(token) => {
                self.chat.append_stream_token(&token);
                self.set_active_status(Status::Streaming);
                self.input.set_enabled(false);
            }
            AgentEvent::ThinkingToken(token) => {
                self.chat.append_thinking_token(&token);
                self.set_active_status(Status::Streaming);
            }
            AgentEvent::ToolCallStart { id, name, input } => {
                let icon = self.tools.icon(&name);
                let label = self.tools.label(&name, &input);
                self.chat.push_tool_call(icon, &label);
                self.set_active_status(Status::ToolRunning { name: name.clone() });
                self.pending_calls
                    .insert(id, PendingCall { label, name, input });
            }
            AgentEvent::ToolCallEnd {
                id,
                content,
                is_error,
                metadata,
            } => {
                let pending = self.pending_calls.remove(&id);
                let view = pending.as_ref().map_or_else(
                    || ToolResultView::Text {
                        content: content.clone(),
                    },
                    |p| {
                        self.tools
                            .result_view(&p.name, &p.input, &content, &metadata, is_error)
                    },
                );
                let header = result_header(&metadata, pending.as_ref().map(|p| p.label.as_str()));
                self.chat.push_tool_result_view(&header, view, is_error);
            }
            AgentEvent::PromptDrained(text) => {
                self.pending_prompts.pop_front();
                self.chat.push_user_message(text);
            }
            AgentEvent::UsageUpdated(usage) => {
                self.status_bar.set_usage(Some(usage));
            }
            AgentEvent::TurnComplete => {
                self.finish_turn();
            }
            AgentEvent::Cancelled => {
                self.chat.push_interrupted_marker();
                self.active_prompt = None;
                self.finalize_idle();
            }
            AgentEvent::AutoCompactionStarted => {
                self.set_active_status(Status::Compacting);
                self.input.set_enabled(false);
            }
            AgentEvent::SessionTitleUpdated { session_id, title } => {
                if session_id == self.session_info.session_id {
                    self.status_bar.set_title(Some(title));
                }
            }
            AgentEvent::SessionRolled { id } => {
                self.session_info.session_id = id;
                self.status_bar.set_title(None);
                self.status_bar.set_usage(None);
                self.chat.clear_history();
                self.active_prompt = None;
            }
            AgentEvent::SessionResumed {
                id,
                title,
                messages,
                compact,
                tool_metadata,
            } => self.apply_session_resumed(id, title, compact.as_ref(), &messages, &tool_metadata),
            AgentEvent::SessionCompacted {
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
            AgentEvent::ConfigChanged {
                model_id,
                effort,
                compaction,
                requested_effort,
            } => self.apply_config_changed(model_id, effort, compaction, requested_effort),
            AgentEvent::Error(msg) => {
                self.chat.push_error(&msg);
                self.finish_turn();
            }
        }
        self.dirty = true;
    }

    fn finish_turn(&mut self) {
        self.chat.commit_streaming();
        self.active_prompt = None;
        self.finalize_idle();
    }

    /// Mid-session resume: rebinds the session and rebuilds chat from the target transcript.
    fn apply_session_resumed(
        &mut self,
        id: String,
        title: Option<String>,
        compact: Option<&CompactInfo>,
        messages: &[Message],
        tool_metadata: &HashMap<String, ToolMetadata>,
    ) {
        self.session_info.session_id = id;
        self.status_bar.set_title(title);
        self.status_bar.set_usage(None);
        self.chat.clear_history();
        self.chat
            .load_history(messages, compact, tool_metadata, self.tools.as_ref());
        self.pending_calls.clear();
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
        self.pending_calls.clear();
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
        model_id: String,
        effort: Option<Effort>,
        compaction: CompactionConfig,
        requested_effort: Option<Effort>,
    ) {
        let model_changed = model_id != self.session_info.config.model_id;
        let prev_effort = self.session_info.config.effort;
        let prev_compaction = self.session_info.config.compaction;
        let confirmation = format_config_change(
            &model_id,
            model_changed,
            prev_effort,
            effort,
            requested_effort,
            prev_compaction,
            compaction,
        );
        if model_changed {
            self.status_bar
                .set_model(crate::model::short_display_name(&model_id).into_owned());
        }
        self.status_bar.set_effort(effort);
        self.session_info.config.model_id = model_id;
        self.session_info.config.effort = effort;
        self.session_info.config.compaction = compaction;
        self.chat.push_system_message(confirmation);
    }

    /// Resets to idle, clears orphan calls, re-enables input, and drains queued prompts.
    fn finalize_idle(&mut self) {
        self.pending_calls.clear();
        self.status_bar.set_status(Status::Idle);
        self.input.set_enabled(true);
        self.drain_pending_prompt();
    }

    fn drain_pending_prompt(&mut self) {
        if let Some(prompt) = self.pending_prompts.pop_front() {
            self.dispatch_user_action(UserAction::SubmitPrompt(prompt));
        }
    }

    /// Sets busy status unless a user-acknowledgement status is showing.
    fn set_active_status(&mut self, status: Status) {
        if !matches!(
            self.status_bar.status(),
            Status::Cancelling | Status::ExitArmed { .. },
        ) {
            self.status_bar.set_status(status);
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

    // ── Rendering ──

    fn render<W: std::io::Write>(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::prelude::CrosstermBackend<W>>,
    ) -> Result<()> {
        let mut chat_area = ratatui::layout::Rect::default();
        draw_sync(terminal, |frame| {
            chat_area = self.draw_frame(frame);
        })?;
        if self.chat.update_layout(chat_area) {
            draw_sync(terminal, |frame| {
                self.draw_frame(frame);
            })?;
        }
        Ok(())
    }

    /// Returns the chat area so the scroll cache can refresh its layout.
    fn draw_frame(&mut self, frame: &mut ratatui::Frame<'_>) -> ratatui::layout::Rect {
        let preview_height = self.preview_height();
        let modal_height = self.modals.height(frame.area().width);
        // Modal owns focus, so input and popup bands collapse.
        let modal_active = modal_height > 0;
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
        chunks[1]
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

/// `AgentEvent::ConfigChanged` confirmation, surfacing silent effort shifts.
fn format_config_change(
    model_id: &str,
    model_changed: bool,
    prev_effort: Option<crate::config::Effort>,
    new_effort: Option<crate::config::Effort>,
    requested_effort: Option<crate::config::Effort>,
    prev_compaction: CompactionConfig,
    new_compaction: CompactionConfig,
) -> String {
    let message = if model_changed {
        let head = format!(
            "Switched to {} ({model_id})",
            crate::model::display_name(model_id)
        );
        match (requested_effort, prev_effort, new_effort) {
            (Some(req), _, Some(eff)) if req == eff => format!("{head} · effort {eff}."),
            (Some(req), _, Some(eff)) => format!("{head} · effort {eff} (clamped from {req})."),
            (Some(req), _, None) => {
                format!("{head}. Effort unchanged — model has no effort tier (asked for {req}).")
            }
            (None, None, None) => format!("{head}."),
            (None, Some(_), None) => format!("{head}. Effort cleared (model has no effort tier)."),
            (None, None, Some(eff)) => format!("{head} · effort {eff} (model default)."),
            (None, Some(prev), Some(new)) if new < prev => {
                format!("{head} · effort {new} (clamped from {prev}).")
            }
            (None, Some(_), Some(eff)) => format!("{head} · effort {eff}."),
        }
    } else {
        match (requested_effort, new_effort) {
            (Some(req), Some(eff)) if req == eff => format!("Effort set to {eff}."),
            (Some(req), Some(eff)) => format!("Effort set to {eff} (clamped from {req})."),
            (Some(req), None) => {
                format!("Effort unchanged — model has no effort tier (asked for {req}).")
            }
            // Slash dispatch keeps this unreachable, but a clear fallback beats a panic.
            (None, _) => "Config unchanged.".to_owned(),
        }
    };
    if model_changed && prev_compaction.auto != new_compaction.auto {
        return append_sentence(
            message,
            &format!(
                "Auto compaction {}",
                display_auto_compaction(new_compaction.auto)
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
    use std::sync::Mutex;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::prelude::CrosstermBackend;
    use ratatui::{Terminal, TerminalOptions, Viewport};
    use tokio::sync::mpsc;

    use super::*;
    use crate::agent::event::UsageSnapshot;
    use crate::config::test_thresholds;
    use crate::tool::ToolRegistry;
    use crate::tui::modal::testing::ScriptedModal;

    /// Idle `App` plus the `user_tx` consumer and an `agent_tx` kept alive so `agent_rx` stays
    /// open.
    fn test_app(
        title: Option<&str>,
    ) -> (App, mpsc::Receiver<UserAction>, mpsc::Sender<AgentEvent>) {
        test_app_with_registry(title, Arc::new(ToolRegistry::new(Vec::new())))
    }

    /// Variant with the real tool catalog so `ToolCallStart` labels match production.
    fn test_app_with_tools() -> (App, mpsc::Receiver<UserAction>, mpsc::Sender<AgentEvent>) {
        let tracker = crate::file_tracker::testing::tracker();
        let tools = ToolRegistry::new(vec![
            Box::new(crate::tool::bash::BashTool),
            Box::new(crate::tool::read::ReadTool::new(Arc::clone(&tracker))),
            Box::new(crate::tool::write::WriteTool::new(Arc::clone(&tracker))),
            Box::new(crate::tool::edit::EditTool::new(tracker)),
            Box::new(crate::tool::glob::GlobTool),
            Box::new(crate::tool::grep::GrepTool),
        ]);
        test_app_with_registry(None, Arc::new(tools))
    }

    fn test_app_with_registry(
        title: Option<&str>,
        tools: Arc<ToolRegistry>,
    ) -> (App, mpsc::Receiver<UserAction>, mpsc::Sender<AgentEvent>) {
        let (agent_tx, agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (user_tx, user_rx) = mpsc::channel::<UserAction>(8);
        let app = App::new(
            &Theme::default(),
            test_session_info(),
            false,
            agent_rx,
            user_tx,
            tools,
            AppHistory {
                messages: &[],
                compact: None,
                tool_metadata: &HashMap::new(),
                title: title.map(ToOwned::to_owned),
            },
        );
        (app, user_rx, agent_tx)
    }

    fn test_session_info() -> LiveSessionInfo {
        // `test-model` is intentionally unknown so `display_name` falls back to the literal
        // id, keeping insta snapshots stable.
        use crate::config::{
            AutoCompactionConfig, CompactionConfig, ConfigSnapshot, Effort, PromptCacheTtl,
        };

        LiveSessionInfo {
            cwd: "~/test".to_owned(),
            git_branch: Some("main".to_owned()),
            version: "0.0.0-test",
            session_id: "test-session".to_owned(),
            config: ConfigSnapshot {
                auth_label: "API key",
                base_url: "https://api.test.invalid".to_owned(),
                extra_ca_certs: None,
                model_id: "test-model".to_owned(),
                effort: Some(Effort::High),
                max_tokens: 32_000,
                max_tool_rounds: None,
                prompt_cache_ttl: PromptCacheTtl::OneHour,
                compaction: CompactionConfig::resolved_for_test(AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(155_000),
                }),
                show_thinking: false,
                show_welcome: true,
                status_line: crate::config::StatusLineSegment::DEFAULT.to_vec(),
                theme_name: "mocha".to_owned(),
            },
        }
    }

    fn base_compaction() -> CompactionConfig {
        CompactionConfig::resolved_for_test(crate::config::AutoCompactionConfig {
            enabled: true,
            threshold_tokens: Some(155_000),
        })
    }

    fn usage_snapshot() -> UsageSnapshot {
        UsageSnapshot {
            context_tokens: 124_000,
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
        fn height(&self, _width: u16) -> u16 {
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
        let mut chat_area = Rect::default();
        terminal
            .draw(|frame| {
                chat_area = app.draw_frame(frame);
            })
            .unwrap();
        // Mirror `App::render`'s second-pass repaint so the captured
        // buffer matches what the user actually sees.
        if app.chat.update_layout(chat_area) {
            terminal
                .draw(|frame| {
                    app.draw_frame(frame);
                })
                .unwrap();
        }
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

    // ── App::new ──

    #[test]
    fn new_plumbs_resumed_title_into_status_bar() {
        let (app, _rx, _agent_tx) = test_app(Some("Resumed title"));
        assert_eq!(app.status_bar.title(), Some("Resumed title"));
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert_eq!(app.chat.entry_count(), 0);
        assert!(app.input.is_enabled());
        assert!(!app.should_quit);
        assert!(app.dirty, "first frame must render");
    }

    #[test]
    fn new_without_title_leaves_slot_unset() {
        let (app, _rx, _agent_tx) = test_app(None);
        assert!(app.status_bar.title().is_none());
    }

    #[test]
    fn new_whitespace_title_is_filtered_by_status_bar() {
        // Status bar filters whitespace-only titles so a resumed blank title doesn't leave a slot.
        let (app, _rx, _agent_tx) = test_app(Some("   \n "));
        assert!(app.status_bar.title().is_none());
    }

    // ── run_with_events ──

    #[tokio::test]
    async fn run_with_events_expires_armed_exit_on_tick_before_channel_close() {
        let (mut app, _rx, agent_tx) = test_app(None);
        let until = Instant::now()
            .checked_sub(EXIT_WINDOW)
            .expect("monotonic clock has run long enough for a test deadline");
        app.status_bar.set_status(Status::ExitArmed { until });
        app.dirty = false;

        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);
        let closer = tokio::spawn(async move {
            tokio::time::sleep(TICK_INTERVAL * 2).await;
            drop(agent_tx);
        });

        app.run_with_events(
            &mut terminal,
            futures::stream::pending::<std::io::Result<Event>>(),
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
        let (mut app, _rx, agent_tx) = test_app(None);
        app.status_bar.set_status(Status::Streaming);
        app.dirty = false;

        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);
        let driver = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(agent_tx);
        });

        app.run_with_events(
            &mut terminal,
            futures::stream::pending::<std::io::Result<Event>>(),
        )
        .await
        .unwrap();
        driver.await.unwrap();

        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn run_with_events_pumps_crossterm_and_agent_events_through_select() {
        // Pins both arms of `select!`: a key reaches input, a stream token disables it,
        // and closing the agent channel ends the loop.
        let (mut app, _rx, agent_tx) = test_app(None);
        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);

        let key = Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        let stream = futures::stream::iter(vec![key]).chain(futures::stream::pending());

        let agent_tx_clone = agent_tx.clone();
        let driver = tokio::spawn(async move {
            agent_tx_clone
                .send(AgentEvent::StreamToken("hi".into()))
                .await
                .unwrap();
            tokio::time::sleep(TICK_INTERVAL * 2).await;
            drop(agent_tx);
        });

        app.run_with_events(&mut terminal, stream).await.unwrap();
        driver.await.unwrap();

        assert!(app.should_quit);
        assert_eq!(app.input.lines(), vec!["x"], "crossterm key reached input");
        assert!(
            !app.input.is_enabled(),
            "agent StreamToken disabled the input",
        );
    }

    // ── handle_crossterm_event ──

    fn key_event(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[tokio::test]
    async fn handle_crossterm_key_submit_forwards_through_input_to_dispatch() {
        let (mut app, mut rx, _agent_tx) = test_app(None);
        // Type "hi" then Enter; input returns `SubmitPrompt` which must reach
        // `dispatch_user_action`.
        app.handle_crossterm_event(&key_event(KeyCode::Char('h'), KeyModifiers::NONE));
        app.handle_crossterm_event(&key_event(KeyCode::Char('i'), KeyModifiers::NONE));
        app.handle_crossterm_event(&key_event(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.dirty);
        assert_eq!(app.chat.entry_count(), 1);
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        let forwarded = rx.recv().await.unwrap();
        assert!(matches!(forwarded, UserAction::SubmitPrompt(s) if s == "hi"));
    }

    #[test]
    fn handle_crossterm_key_ctrl_c_idle_arms_exit_then_confirms() {
        // First press arms; second press inside the window confirms.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.should_quit, "first press only arms");
        assert!(matches!(app.status_bar.status(), Status::ExitArmed { .. }));

        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit, "second press within window confirms");
    }

    #[tokio::test]
    async fn handle_crossterm_key_ctrl_c_busy_forwards_cancel_without_quitting() {
        // Mid-turn Ctrl+C forwards `Cancel` to the agent loop and leaves `should_quit` clear.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::StreamToken("partial".into()));
        assert!(!app.input.is_enabled());

        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.should_quit, "busy Ctrl+C must not exit");
        let forwarded = rx.recv().await.expect("Cancel forwarded to agent loop");
        assert!(matches!(forwarded, UserAction::Cancel));
    }

    #[tokio::test]
    async fn handle_crossterm_key_esc_busy_forwards_cancel() {
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::StreamToken("partial".into()));

        app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.should_quit);
        let forwarded = rx.recv().await.expect("Cancel forwarded to agent loop");
        assert!(matches!(forwarded, UserAction::Cancel));
    }

    #[test]
    fn handle_crossterm_mouse_is_forwarded_to_chat() {
        // Mouse events reach `ChatView::handle_event` for scroll; the dirty flag must flip.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_crossterm_event(&Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.dirty);
    }

    #[test]
    fn handle_crossterm_resize_schedules_dirty_for_relayout() {
        // Resize falls through to `dirty = true` so the next tick re-splits with the new area.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dirty = false;
        app.handle_crossterm_event(&Event::Resize(80, 24));
        assert!(app.dirty, "Resize must trigger a re-layout render");
    }

    #[test]
    fn handle_crossterm_unknown_event_is_a_noop() {
        // The `_ => return` arm prevents FocusGained / FocusLost / Paste from forcing re-renders.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dirty = false;
        app.handle_crossterm_event(&Event::FocusGained);
        assert!(!app.dirty);
    }

    #[test]
    fn handle_crossterm_scroll_key_routes_to_chat_while_input_disabled() {
        // While input is disabled, arrow / page keys must still reach chat for scrolling.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.input.set_enabled(false);
        app.handle_crossterm_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(app.dirty);
    }

    #[tokio::test]
    async fn handle_crossterm_popup_enter_dispatches_canonical_command() {
        // `/h` filters to /help; Enter dispatches it. /help is read-only so chat
        // lands a system message instead of forwarding to the agent.
        let (mut app, mut rx, _agent_tx) = test_app(None);
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
        let (mut app, _rx, _agent_tx) = test_app(None);
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
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.push_modal(Box::new(ScriptedModal::new(ModalAction::User(
            UserAction::Cancel,
        ))));

        app.handle_crossterm_event(&key_event(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(
            app.input.lines().iter().all(String::is_empty),
            "input must stay empty while modal is active",
        );
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "no UserAction must reach user_tx for a key the modal consumed",
        );

        // Modal submit flows through the normal dispatch path.
        app.handle_crossterm_event(&key_event(KeyCode::Char('s'), KeyModifiers::NONE));
        let forwarded = rx.recv().await.expect("modal-submitted action forwarded");
        assert!(matches!(forwarded, UserAction::Cancel));
    }

    #[test]
    fn modal_gate_cancel_closes_modal_without_dispatching() {
        // `ModalKey::Cancelled` pops the modal without dispatching; next key reaches input.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.push_modal(Box::new(ScriptedModal::new(ModalAction::None)));
        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::NONE));

        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "Cancelled must not dispatch any UserAction",
        );
        // Modal closed; subsequent keys reach the input.
        app.handle_crossterm_event(&key_event(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(app.input.lines(), vec!["y".to_owned()]);
    }

    // ── handle_esc ──

    #[tokio::test]
    async fn handle_esc_busy_dispatches_cancel() {
        // Esc is routed by `App` because its meaning depends on queue state and run-state.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::StreamToken("partial".into()));
        app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.status_bar.status(), &Status::Cancelling);
        let forwarded = rx.recv().await.expect("Cancel forwarded");
        assert!(matches!(forwarded, UserAction::Cancel));
    }

    #[test]
    fn handle_esc_idle_with_empty_queue_is_silent() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_crossterm_event(&key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.pending_prompts.is_empty());
    }

    #[test]
    fn handle_esc_idle_with_queue_pops_most_recent_into_input() {
        // The most-recent (back of FIFO) returns to input; the rest stay queued.
        let (mut app, _rx, _agent_tx) = test_app(None);
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
        let (mut app, _rx, _agent_tx) = test_app(None);
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
        let (mut app, _rx, _agent_tx) = test_app(None);
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

    // ── dispatch_user_action ──

    #[tokio::test]
    async fn dispatch_submit_prompt_updates_chat_status_and_forwards_action() {
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("hello".to_owned()));

        assert_eq!(app.chat.entry_count(), 1);
        assert!(!app.input.is_enabled(), "streaming disables input");
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.should_quit);
        let forwarded = rx.recv().await.expect("forwarded action");
        assert!(matches!(forwarded, UserAction::SubmitPrompt(s) if s == "hello"));
    }

    #[tokio::test]
    async fn dispatch_slash_command_renders_locally_without_forwarding() {
        // Slash commands stay client-side; the agent loop never sees the prompt.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("/help".to_owned()));

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
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("/no-such".to_owned()));

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
        // `//foo` parses as "not a command", so the agent receives the bytes verbatim.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("//etc/hosts".to_owned()));

        assert_eq!(app.chat.entry_count(), 1, "only the user message");
        assert!(!app.input.is_enabled(), "streaming disables input");
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.chat.last_is_error());
        let forwarded = rx.recv().await.expect("forwarded action");
        assert!(matches!(
            forwarded,
            UserAction::SubmitPrompt(s) if s == "//etc/hosts",
        ));
    }

    #[tokio::test]
    async fn dispatch_cancel_flips_status_to_cancelling_and_forwards() {
        // Cancel flips status immediately; `AgentEvent::Cancelled` later returns to idle.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("hi".to_owned()));
        rx.recv().await.expect("submit forwarded");
        let entries_before = app.chat.entry_count();

        app.dispatch_user_action(UserAction::Cancel);

        assert_eq!(app.chat.entry_count(), entries_before, "no new chat entry");
        assert_eq!(app.status_bar.status(), &Status::Cancelling);
        assert!(!app.should_quit);
        let forwarded = rx.recv().await.expect("Cancel forwarded");
        assert!(matches!(forwarded, UserAction::Cancel));
    }

    #[test]
    fn dispatch_quit_sets_should_quit_and_leaves_chat_untouched() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::Quit);

        assert!(app.should_quit);
        assert_eq!(app.chat.entry_count(), 0);
        // Quit flows past streaming setup so tear-down doesn't have to un-spinner.
        assert_eq!(app.status_bar.status(), &Status::Idle);
    }

    #[test]
    fn dispatch_closed_channel_surfaces_error_and_quits() {
        // Dropping `user_rx` simulates the agent task exiting; the UI must announce and tear down.
        let (mut app, rx, _agent_tx) = test_app(None);
        drop(rx);

        app.dispatch_user_action(UserAction::SubmitPrompt("hi".to_owned()));

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
        // `Cancel` always goes through `try_send`, so it's the natural way to overflow
        // (busy-turn submits buffer locally instead).
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
        for _ in 0..7 {
            app.dispatch_user_action(UserAction::Cancel);
        }
        let before_overflow = app.chat.entry_count();

        app.dispatch_user_action(UserAction::Cancel);

        assert!(!app.should_quit, "Full is not fatal");
        assert_eq!(
            app.chat.entry_count(),
            before_overflow + 1,
            "exactly one error block on overflow",
        );
        assert!(app.chat.last_is_error());
    }

    #[tokio::test]
    async fn dispatch_submit_during_busy_queues_and_forwards_for_mid_turn_drain() {
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("first".to_owned()));
        rx.recv().await.expect("first submit forwarded");

        app.dispatch_user_action(UserAction::SubmitPrompt("queued".to_owned()));

        // Buffered for preview AND forwarded so `agent_turn` can splice it at the next
        // round boundary. Chat stays untouched until `PromptDrained` lands.
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["queued".to_owned()],
        );
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        let forwarded = rx.recv().await.expect("queued submit forwarded");
        assert!(matches!(forwarded, UserAction::SubmitPrompt(s) if s == "queued"));
    }

    #[tokio::test]
    async fn dispatch_swap_config_forwards_to_agent_through_user_tx() {
        // Modal-emitted SwapConfig must reach the agent loop so it can emit `ConfigChanged`.
        for action in [
            UserAction::SwapConfig {
                model: Some(crate::model::ResolvedModelId::new(
                    "claude-opus-4-7".to_owned(),
                )),
                effort: None,
            },
            UserAction::SwapConfig {
                model: None,
                effort: Some(crate::config::Effort::High),
            },
            UserAction::Clear,
        ] {
            let (mut app, mut rx, _agent_tx) = test_app(None);
            app.dispatch_user_action(action.clone());

            let forwarded = rx.recv().await.expect("action forwarded to agent");
            assert_eq!(forwarded, action);
            assert!(!app.should_quit);
            assert_eq!(app.chat.entry_count(), 0);
        }
    }

    #[tokio::test]
    async fn dispatch_resume_forwards_to_agent_and_disables_input_until_event() {
        // Pin: between forwarding `Resume` and the SessionResumed event landing, input must be
        // gated so a typed prompt doesn't push into chat just before `apply_session_resumed`'s
        // `clear_history` wipes it. `finalize_idle` inside the resumed handler re-enables input.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        let action = UserAction::Resume {
            session_id: "resume-target".to_owned(),
        };
        app.dispatch_user_action(action.clone());

        let forwarded = rx.recv().await.expect("Resume must reach the agent loop");
        assert_eq!(forwarded, action);
        assert!(
            !app.input.is_enabled(),
            "input must be gated until the resume event lands",
        );
    }

    #[tokio::test]
    async fn dispatch_compact_forwards_to_agent_and_disables_input_until_event() {
        // Mirror of the Resume gate: the chat is about to be wiped by `apply_session_compacted`,
        // so the user's input must be parked until SessionCompacted re-enables it.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        let action = UserAction::Compact {
            instructions: Some("focus".to_owned()),
        };
        app.dispatch_user_action(action.clone());

        let forwarded = rx.recv().await.expect("Compact must reach the agent loop");
        assert_eq!(forwarded, action);
        assert!(
            !app.input.is_enabled(),
            "input must be gated until the compact event lands",
        );
        assert_eq!(app.status_bar.status(), &Status::Compacting);
    }

    #[tokio::test]
    async fn dispatch_submit_during_cancelling_holds_locally_without_forwarding() {
        // Cancel-window FIFO authority: forwarding a submit during cancel could let it
        // slip ahead of `pending_prompts`. Hold locally until `Cancelled`, then drain.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.dispatch_user_action(UserAction::Cancel);
        rx.recv().await.expect("cancel forwarded");
        assert_eq!(app.status_bar.status(), &Status::Cancelling);

        app.dispatch_user_action(UserAction::SubmitPrompt("typed-during-cancel".to_owned()));

        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "submit during cancel must not reach the agent's user_rx",
        );
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["typed-during-cancel".to_owned()],
            "held locally; finalize_idle re-fires after Cancelled lands",
        );
    }

    #[tokio::test]
    async fn dispatch_submit_during_compacting_holds_locally_without_forwarding() {
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::Compact { instructions: None });
        rx.recv().await.expect("compact forwarded");
        assert_eq!(app.status_bar.status(), &Status::Compacting);

        app.dispatch_user_action(UserAction::SubmitPrompt("typed-during-compact".to_owned()));

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
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        assert_eq!(app.status_bar.status(), &Status::Streaming);

        app.dispatch_user_action(UserAction::SubmitPrompt("/help".to_owned()));

        assert!(
            app.pending_prompts.is_empty(),
            "slash command must not queue",
        );
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "slash command must not forward to user_tx",
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
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");

        app.dispatch_user_action(UserAction::SubmitPrompt("/clear".to_owned()));

        assert!(app.pending_prompts.is_empty());
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "neither SubmitPrompt nor Clear must reach user_tx",
        );
        let body = app.chat.last_system_text().expect("refusal system message");
        assert!(
            body.contains("/clear runs only when idle"),
            "refusal must name the command and gate: {body}",
        );
    }

    #[tokio::test]
    async fn dispatch_init_forwards_synthesized_prompt_and_flips_to_streaming() {
        // Chat shows only `/init`; the agent must receive the synthesized body.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("/init".to_owned()));

        assert_eq!(app.chat.entry_count(), 1, "only the typed `/init` line");
        assert!(!app.input.is_enabled(), "Streaming disables input");
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        let forwarded = rx.recv().await.expect("synthesized prompt forwarded");
        assert!(
            matches!(
                &forwarded,
                UserAction::SubmitPrompt(body) if body.contains("AGENTS.md") && body != "/init"
            ),
            "expected SubmitPrompt with expanded body, got {forwarded:?}",
        );
    }

    #[tokio::test]
    async fn dispatch_init_during_busy_refuses_with_system_message_no_forward() {
        // Mutating: refuse. Typed `/init` row still lands; only the synthesized body suppresses.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");

        app.dispatch_user_action(UserAction::SubmitPrompt("/init".to_owned()));

        assert!(app.pending_prompts.is_empty());
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "no synthesized prompt must reach user_tx mid-turn",
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
            let (mut app, mut rx, _agent_tx) = test_app(None);
            app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
            rx.recv().await.expect("active submit forwarded");

            app.dispatch_user_action(UserAction::SubmitPrompt(cmd.to_owned()));

            assert!(
                matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
                "{cmd}: action must not reach user_tx mid-turn",
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
            let (mut app, mut rx, _agent_tx) = test_app(None);
            app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
            rx.recv().await.expect("active submit forwarded");

            app.dispatch_user_action(UserAction::SubmitPrompt(cmd.to_owned()));

            assert!(
                !app.modals.is_active(),
                "{cmd}: modal must not open mid-turn"
            );
            assert!(
                matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
                "{cmd}: action must not reach user_tx mid-turn",
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
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");

        app.dispatch_user_action(UserAction::SubmitPrompt("/nope".to_owned()));

        assert!(app.pending_prompts.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
        ));
        assert!(app.chat.last_is_error());
    }

    // ── apply_action_locally / theme ──

    #[test]
    fn preview_theme_repaints_components_and_caches_original() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        let original = app.theme.clone();
        app.dispatch_user_action(UserAction::PreviewTheme {
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
        // Esc / Ctrl+C surface as ModalAction::None; the snapshot must roll back.
        let (mut app, _rx, _agent_tx) = test_app(None);
        let original = app.theme.clone();
        app.dispatch_user_action(UserAction::PreviewTheme {
            name: "latte".to_owned(),
        });
        app.apply_modal_action(ModalAction::None);
        assert!(
            app.preview_theme_snapshot.is_none(),
            "snapshot consumed on cancel",
        );
        assert_eq!(app.theme.text, original.text, "theme rolled back to mocha");
    }

    #[test]
    fn clearing_modal_stack_rolls_back_theme_preview() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        let original = app.theme.clone();
        app.push_modal(Box::new(ScriptedModal::new(ModalAction::None)));
        app.dispatch_user_action(UserAction::PreviewTheme {
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
        // ModalAction::None path; the rollback arm must skip cleanly with no snapshot.
        let (mut app, _rx, _agent_tx) = test_app(None);
        let original_text = app.theme.text;
        app.apply_modal_action(ModalAction::None);
        assert!(app.preview_theme_snapshot.is_none());
        assert_eq!(app.theme.text, original_text);
    }

    #[test]
    fn modal_system_message_action_pushes_into_chat() {
        // Destructive-action modals (e.g. confirm-delete) report success via SystemMessage so
        // the user has chat-stream evidence the action ran. Pin the body verbatim so a future
        // routing change that drops the message or pushes an empty block fails here.
        let (mut app, _rx, _agent_tx) = test_app(None);
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
        let (mut app, _rx, _agent_tx) = test_app(None);
        let original_text = app.theme.text;
        let entries_before = app.chat.entry_count();

        app.dispatch_user_action(UserAction::PreviewTheme {
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
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::PreviewTheme {
            name: "latte".to_owned(),
        });
        let previewed = app.theme.clone();
        app.dispatch_user_action(UserAction::SwapTheme {
            name: "latte".to_owned(),
        });
        assert!(app.preview_theme_snapshot.is_none());
        assert_eq!(app.session_info.config.theme_name, "latte");
        assert_eq!(app.theme.text, previewed.text);

        app.apply_modal_action(ModalAction::None);
        assert_eq!(
            app.session_info.config.theme_name, "latte",
            "post-commit cancel must not restore the pre-preview theme",
        );
    }

    #[tokio::test]
    async fn swap_theme_with_unknown_name_is_a_silent_noop() {
        // Roster drift between slash::theme::LISTED_THEMES and tui::theme::builtin::TABLE is a
        // dev bug, not user error — log via tracing and leave UI state untouched.
        let (mut app, _rx, _agent_tx) = test_app(None);
        let original = app.session_info.config.theme_name.clone();
        let entries_before = app.chat.entry_count();

        app.dispatch_user_action(UserAction::SwapTheme {
            name: "nonexistent".to_owned(),
        });

        assert_eq!(app.session_info.config.theme_name, original);
        assert_eq!(
            app.chat.entry_count(),
            entries_before,
            "no chat block — drift is dev-only",
        );
    }

    #[tokio::test]
    async fn theme_actions_are_not_forwarded_to_agent_loop() {
        // PreviewTheme / SwapTheme are TUI-only; reaching `user_tx` would race the client.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::PreviewTheme {
            name: "latte".to_owned(),
        });
        app.dispatch_user_action(UserAction::SwapTheme {
            name: "latte".to_owned(),
        });
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "theme actions must stay client-side",
        );
    }

    // ── handle_submit_prompt ──

    #[test]
    fn submit_slash_theme_pushes_picker_onto_modal_stack() {
        // Bare `/theme` is the ReadOnly modal-opening branch — `slash::dispatch` populates the
        // SlashContext modal slot, then handle_submit_prompt drains it onto `App::modals`.
        let (mut app, _rx, _agent_tx) = test_app(None);
        assert!(!app.modals.is_active(), "stack starts empty");
        app.dispatch_user_action(UserAction::SubmitPrompt("/theme".to_owned()));
        assert!(app.modals.is_active(), "bare `/theme` opens a picker modal");
    }

    #[test]
    fn slash_typed_swap_theme_routes_through_local_handler() {
        // Synthesized non-SubmitPrompt actions (e.g. `/theme latte`) must flow through
        // dispatch_user_action so the local theme arm runs, not get forwarded to the agent.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("/theme latte".to_owned()));

        assert_eq!(
            app.session_info.config.theme_name, "latte",
            "typed `/theme <name>` must mutate session theme",
        );
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "TUI-only theme swap must not leak to the agent channel",
        );
    }

    #[tokio::test]
    async fn slash_typed_model_routes_synthesized_swap_config_through_dispatch() {
        // Mirrors the typed-`/theme` regression on the agent-bound side: `/model <id>` synthesizes
        // a SwapConfig that must reach the agent via dispatch_user_action → forward_to_agent.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("/model haiku".to_owned()));

        let forwarded = rx.recv().await.expect("SwapConfig forwarded to agent");
        match forwarded {
            UserAction::SwapConfig { model, effort } => {
                assert_eq!(
                    model.as_ref().map(crate::model::ResolvedModelId::as_str),
                    Some("claude-haiku-4-5")
                );
                assert_eq!(effort, None);
            }
            other => panic!("expected SwapConfig, got {other:?}"),
        }
    }

    // ── handle_agent_event ──

    #[test]
    fn handle_session_title_updated_overwrites_existing_title_for_current_session() {
        let (mut app, _rx, _agent_tx) = test_app(Some("First prompt"));
        app.handle_agent_event(AgentEvent::SessionTitleUpdated {
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
        let (mut app, _rx, _agent_tx) = test_app(Some("First prompt"));
        app.handle_agent_event(AgentEvent::SessionTitleUpdated {
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
        // Three surfaces refresh in one shot: status-bar label,
        // `session_info` (backs `/status` / `/config`), and a chat
        // confirmation block. Display name is derived locally from `model_id`.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::ConfigChanged {
            model_id: "claude-sonnet-4-6".to_owned(),
            effort: Some(crate::config::Effort::High),
            compaction: crate::config::CompactionConfig::resolved_for_test(
                crate::config::AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(test_thresholds::WINDOW_200K),
                },
            ),
            requested_effort: None,
        });

        assert_eq!(app.session_info.config.model_id, "claude-sonnet-4-6");
        assert_eq!(
            app.session_info.config.effort,
            Some(crate::config::Effort::High),
        );
        assert_eq!(app.status_bar.model(), "Sonnet 4.6");
        assert_eq!(
            app.session_info.config.compaction.auto.threshold_tokens,
            Some(test_thresholds::WINDOW_200K),
        );
        let body = app.chat.last_system_text().expect("confirmation block");
        assert_eq!(
            body,
            format!(
                "Switched to Claude Sonnet 4.6 (claude-sonnet-4-6) · effort high. \
                 Auto compaction at {} tokens.",
                test_thresholds::WINDOW_200K,
            ),
        );
        assert!(app.dirty);
    }

    #[test]
    fn handle_config_changed_effort_only_keeps_status_bar_model_label() {
        // Effort-only swap leaves the cached model label alone; only
        // the snapshot effort and chat confirmation update.
        let (mut app, _rx, _agent_tx) = test_app(None);
        let original_model = app.status_bar.model().to_owned();
        app.handle_agent_event(AgentEvent::ConfigChanged {
            model_id: app.session_info.config.model_id.clone(),
            effort: Some(crate::config::Effort::Xhigh),
            compaction: app.session_info.config.compaction,
            requested_effort: Some(crate::config::Effort::Xhigh),
        });
        assert_eq!(
            app.session_info.config.effort,
            Some(crate::config::Effort::Xhigh),
        );
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
            "claude-haiku-4-5",
            true,
            None,
            None,
            None,
            base_compaction(),
            base_compaction(),
        );
        assert_eq!(s, "Switched to Claude Haiku 4.5 (claude-haiku-4-5).");
    }

    #[test]
    fn format_config_change_swap_clears_effort_when_new_model_drops_it() {
        // User had a tier; new model has none. Surface the change so
        // the user knows their effort just disappeared.
        let s = format_config_change(
            "claude-haiku-4-5",
            true,
            Some(crate::config::Effort::Xhigh),
            None,
            None,
            base_compaction(),
            base_compaction(),
        );
        assert_eq!(
            s,
            "Switched to Claude Haiku 4.5 (claude-haiku-4-5). Effort cleared (model has no effort tier)."
        );
    }

    #[test]
    fn format_config_change_swap_marks_default_when_previous_was_none() {
        // None → Some means the new model's default kicked in;
        // distinguishing this from "user's pick survived" prevents
        // the user from thinking they chose this tier.
        let s = format_config_change(
            "claude-opus-4-7",
            true,
            None,
            Some(crate::config::Effort::Xhigh),
            None,
            base_compaction(),
            base_compaction(),
        );
        assert_eq!(
            s,
            "Switched to Claude Opus 4.7 (claude-opus-4-7) · effort xhigh (model default)."
        );
    }

    #[test]
    fn format_config_change_swap_marks_clamp_when_new_effort_below_previous() {
        // The effective tier changed; surface the temporary clamp.
        let s = format_config_change(
            "claude-sonnet-4-6",
            true,
            Some(crate::config::Effort::Xhigh),
            Some(crate::config::Effort::High),
            None,
            base_compaction(),
            base_compaction(),
        );
        assert_eq!(
            s,
            "Switched to Claude Sonnet 4.6 (claude-sonnet-4-6) · effort high (clamped from xhigh)."
        );
    }

    #[test]
    fn format_config_change_swap_quiet_when_effort_unchanged() {
        // Same tier survives — no clamp / default annotation. Pin
        // exact format so a stray suffix (`(unchanged)`) would fail.
        let s = format_config_change(
            "claude-opus-4-7",
            true,
            Some(crate::config::Effort::High),
            Some(crate::config::Effort::High),
            None,
            base_compaction(),
            base_compaction(),
        );
        assert_eq!(
            s,
            "Switched to Claude Opus 4.7 (claude-opus-4-7) · effort high.",
        );
    }

    #[test]
    fn format_config_change_swap_with_explicit_effort_clamped_against_new_caps() {
        // Combined picker case: user asks for xhigh on Sonnet (caps at
        // high). Surface that the *requested* tier was clamped — not the previous-effort delta.
        let s = format_config_change(
            "claude-sonnet-4-6",
            true,
            Some(crate::config::Effort::Medium),
            Some(crate::config::Effort::High),
            Some(crate::config::Effort::Xhigh),
            base_compaction(),
            base_compaction(),
        );
        assert_eq!(
            s,
            "Switched to Claude Sonnet 4.6 (claude-sonnet-4-6) · effort high (clamped from xhigh)."
        );
    }

    #[test]
    fn format_config_change_effort_explicit_pick_matches_resolution() {
        let s = format_config_change(
            "claude-opus-4-7",
            false,
            Some(crate::config::Effort::High),
            Some(crate::config::Effort::Xhigh),
            Some(crate::config::Effort::Xhigh),
            base_compaction(),
            base_compaction(),
        );
        assert_eq!(s, "Effort set to xhigh.");
    }

    #[test]
    fn format_config_change_effort_clamp_surfaces_what_user_asked_for() {
        let s = format_config_change(
            "claude-sonnet-4-6",
            false,
            Some(crate::config::Effort::Medium),
            Some(crate::config::Effort::High),
            Some(crate::config::Effort::Xhigh),
            base_compaction(),
            base_compaction(),
        );
        assert_eq!(s, "Effort set to high (clamped from xhigh).");
    }

    #[test]
    fn format_config_change_effort_pick_on_no_tier_model_surfaces_loss() {
        // The slash command preflight stops this through /effort, but
        // client-driven flows could still emit it.
        let s = format_config_change(
            "claude-haiku-4-5",
            false,
            None,
            None,
            Some(crate::config::Effort::High),
            base_compaction(),
            base_compaction(),
        );
        assert_eq!(
            s,
            "Effort unchanged — model has no effort tier (asked for high)."
        );
    }

    #[test]
    fn format_config_change_model_swap_mentions_compaction_threshold_change() {
        let new_compaction =
            CompactionConfig::resolved_for_test(crate::config::AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(test_thresholds::WINDOW_200K),
            });

        let s = format_config_change(
            "claude-sonnet-4-6",
            true,
            Some(crate::config::Effort::High),
            Some(crate::config::Effort::High),
            None,
            base_compaction(),
            new_compaction,
        );

        assert_eq!(
            s,
            format!(
                "Switched to Claude Sonnet 4.6 (claude-sonnet-4-6) · effort high. \
                 Auto compaction at {} tokens.",
                test_thresholds::WINDOW_200K,
            ),
        );
    }

    #[test]
    fn handle_usage_updated_sets_status_bar_usage() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        let usage = usage_snapshot();

        app.handle_agent_event(AgentEvent::UsageUpdated(usage));

        assert_eq!(app.status_bar.usage(), Some(usage));
        assert!(app.dirty);
    }

    #[test]
    fn handle_session_rolled_clears_chat_rebinds_id_and_drops_stale_title() {
        let (mut app, _rx, _agent_tx) = test_app(Some("Old session title"));
        app.chat.push_user_message("old prompt".to_owned());
        let original_id = app.session_info.session_id.clone();
        app.status_bar.set_usage(Some(usage_snapshot()));

        app.handle_agent_event(AgentEvent::SessionRolled {
            id: "rolled-session".to_owned(),
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
        assert!(
            app.chat.is_empty(),
            "clear must drain the chat so the welcome can repaint",
        );
        assert!(app.dirty);
    }

    #[test]
    fn handle_session_resumed_replays_transcript_and_clears_pending() {
        let (mut app, _rx, _agent_tx) = test_app(Some("Old"));
        app.chat.push_user_message("live prompt".to_owned());
        app.status_bar.set_usage(Some(usage_snapshot()));
        app.pending_prompts.push_back("queued".to_owned());
        app.pending_calls.insert(
            "pending-1".to_owned(),
            PendingCall {
                label: "Bash(...)".to_owned(),
                name: "bash".to_owned(),
                input: serde_json::json!({}),
            },
        );
        let original_id = app.session_info.session_id.clone();

        let messages = vec![
            Message::user("resumed user"),
            Message::assistant("resumed assistant"),
        ];
        app.handle_agent_event(AgentEvent::SessionResumed {
            id: "resumed-session".to_owned(),
            title: Some("Resumed title".to_owned()),
            messages,
            compact: None,
            tool_metadata: HashMap::new(),
        });

        assert_eq!(app.session_info.session_id, "resumed-session");
        assert_ne!(app.session_info.session_id, original_id);
        assert_eq!(app.status_bar.title(), Some("Resumed title"));
        assert!(
            app.status_bar.usage().is_none(),
            "stale usage must be cleared on resume",
        );
        assert_eq!(
            app.chat.entry_count(),
            3,
            "chat must reflect the resumed transcript + the queued-prompt-discarded notice",
        );
        assert_eq!(
            app.pending_calls.len(),
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
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::SessionResumed {
            id: "resumed".to_owned(),
            title: None,
            messages: vec![Message::user("only msg")],
            compact: None,
            tool_metadata: HashMap::new(),
        });
        assert_eq!(
            app.chat.entry_count(),
            1,
            "no queued prompts → no discarded-prompts notice",
        );
    }

    #[test]
    fn handle_session_resumed_with_compact_replays_boundary_block() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::SessionResumed {
            id: "resumed".to_owned(),
            title: None,
            messages: vec![
                crate::agent::compaction::synthesize_post_compact_message("internal summary"),
                Message::assistant("post-compact reply"),
            ],
            compact: Some(CompactInfo {
                summary: "Compact summary".to_owned(),
                pre_message_count: 4,
                instructions: None,
            }),
            tool_metadata: HashMap::new(),
        });

        assert_eq!(app.chat.entry_count(), 2);
        let text = rendered_text(&mut app, 80, 12);
        assert!(text.contains("Compacted 4 messages"));
        assert!(text.contains("Compact summary"));
        assert!(text.contains("post-compact reply"));
        assert!(!text.contains("internal summary"));
    }

    #[test]
    fn handle_session_resumed_with_no_title_clears_stale_chrome() {
        let (mut app, _rx, _agent_tx) = test_app(Some("Stale"));
        app.handle_agent_event(AgentEvent::SessionResumed {
            id: "resumed".to_owned(),
            title: None,
            messages: Vec::new(),
            compact: None,
            tool_metadata: HashMap::new(),
        });
        assert!(
            app.status_bar.title().is_none(),
            "Some(None) title must clear the chrome",
        );
        assert!(app.chat.is_empty());
    }

    #[test]
    fn handle_session_compacted_replays_summary_and_clears_pending_calls() {
        let (mut app, _rx, _agent_tx) = test_app(Some("Pre-compact"));
        app.chat.push_user_message("pre-compact prompt".to_owned());
        app.status_bar.set_usage(Some(usage_snapshot()));
        app.pending_calls.insert(
            "pending-1".to_owned(),
            PendingCall {
                label: "Bash(...)".to_owned(),
                name: "bash".to_owned(),
                input: serde_json::json!({}),
            },
        );

        app.handle_agent_event(AgentEvent::SessionCompacted {
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
            app.pending_calls.len(),
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
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.pending_prompts
            .push_back("queued after compact".to_owned());

        app.handle_agent_event(AgentEvent::SessionCompacted {
            summary: "s".to_owned(),
            pre_count: 2,
            instructions: None,
            automatic: false,
        });

        let forwarded = rx.recv().await.expect("drained prompt reaches the agent");
        assert_eq!(
            forwarded,
            UserAction::SubmitPrompt("queued after compact".to_owned()),
            "queued prompt must drain as the next user turn",
        );
        assert!(app.pending_prompts.is_empty());
    }

    #[test]
    fn handle_session_compacted_without_instructions_renders_clean_block() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::SessionCompacted {
            summary: "summary only".to_owned(),
            pre_count: 2,
            instructions: None,
            automatic: false,
        });
        assert_eq!(app.chat.entry_count(), 1, "exactly one boundary block");
    }

    #[tokio::test]
    async fn handle_session_compacted_automatic_keeps_busy_state_and_queued_prompts() {
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.input.set_enabled(false);
        app.status_bar.set_status(Status::Compacting);
        app.pending_prompts
            .push_back("queued while busy".to_owned());

        app.handle_agent_event(AgentEvent::SessionCompacted {
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
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active question".to_owned()));

        app.handle_agent_event(AgentEvent::AutoCompactionStarted);

        assert_eq!(app.status_bar.status(), &Status::Compacting);
        assert!(!app.input.is_enabled());
    }

    #[tokio::test]
    async fn handle_session_compacted_automatic_replays_active_prompt_after_summary() {
        let (mut app, mut rx, _agent_tx) = test_app(None);

        app.dispatch_user_action(UserAction::SubmitPrompt("active question".to_owned()));
        let forwarded = rx.recv().await.expect("prompt reaches the agent");
        assert_eq!(
            forwarded,
            UserAction::SubmitPrompt("active question".to_owned())
        );
        app.handle_agent_event(AgentEvent::AutoCompactionStarted);

        app.handle_agent_event(AgentEvent::SessionCompacted {
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
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::StreamToken("partial".to_owned()));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.input.is_enabled());
    }

    #[test]
    fn handle_thinking_token_routes_to_chat_and_marks_streaming() {
        // Thinking tokens land in the chat view as a separate block
        // (not interleaved with assistant text) and must flip the bar
        // to Streaming so the user sees the agent is working even before any visible text arrives.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::ThinkingToken("planning...".to_owned()));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        // Unlike StreamToken, thinking does not disable input on its
        // own — the matching SubmitPrompt already did that.
    }

    #[test]
    fn handle_tool_call_start_switches_to_tool_running() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::ToolCallStart {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": "ls"}),
        });
        assert!(matches!(
            app.status_bar.status(),
            Status::ToolRunning { .. }
        ));
        assert_eq!(
            app.chat.entry_count(),
            1,
            "tool call renders one chat entry",
        );
    }

    #[test]
    fn handle_cancelled_commits_partial_stream_with_marker_and_becomes_idle() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("hi".to_owned()));
        app.handle_agent_event(AgentEvent::StreamToken("partial answer".into()));
        let entries_before = app.chat.entry_count();
        assert_eq!(app.status_bar.status(), &Status::Streaming);

        app.handle_agent_event(AgentEvent::Cancelled);

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
        assert!(
            text.contains(crate::agent::event::INTERRUPTED_MARKER),
            "marker present: {text}",
        );
    }

    #[test]
    fn cancelling_status_is_sticky_against_late_buffered_events() {
        // After the user dispatches Cancel, the agent channel may still
        // have queued StreamToken / ToolCallStart events the agent emitted
        // before its select arm dropped the turn future. Those buffered
        // events must not flip the bar back to Streaming / ToolRunning —
        // otherwise the cancel acknowledgement flickers off until `Cancelled` finally arrives.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("hi".to_owned()));
        app.dispatch_user_action(UserAction::Cancel);
        assert_eq!(app.status_bar.status(), &Status::Cancelling);

        app.handle_agent_event(AgentEvent::StreamToken("late".into()));
        assert_eq!(app.status_bar.status(), &Status::Cancelling);
        app.handle_agent_event(AgentEvent::ToolCallStart {
            id: "t-late".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": "ls"}),
        });
        assert_eq!(app.status_bar.status(), &Status::Cancelling);
    }

    #[test]
    fn handle_tool_call_end_with_title_pushes_result_entry() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::ToolCallStart {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": "ls"}),
        });
        let before = app.chat.entry_count();
        app.handle_agent_event(AgentEvent::ToolCallEnd {
            id: "t1".to_owned(),
            content: "file1\nfile2\n".to_owned(),
            is_error: false,
            metadata: crate::tool::ToolMetadata {
                title: Some("ls /".to_owned()),
                ..crate::tool::ToolMetadata::default()
            },
        });
        assert_eq!(app.chat.entry_count(), before + 1);
    }

    #[test]
    fn handle_tool_call_end_without_title_falls_back_to_call_label() {
        // `title: None` means the tool didn't set a result header —
        // typically a failure path (timeout, invalid input) that
        // aborted before `.with_title(...)`. The result must still be
        // pushed (silent swallow would hide the error body) and the
        // header must render the tool-call label stashed at
        // `ToolCallStart`, not a blank string or the generic `(result)` fallback.
        let (mut app, _rx, _agent_tx) = test_app_with_tools();
        app.handle_agent_event(AgentEvent::ToolCallStart {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": "distinctive_label_xyz"}),
        });
        let before = app.chat.entry_count();
        app.handle_agent_event(AgentEvent::ToolCallEnd {
            id: "t1".to_owned(),
            content: "spawn failed: permission denied".to_owned(),
            is_error: true,
            metadata: crate::tool::ToolMetadata::default(),
        });
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
        let (mut app, _rx, _agent_tx) = test_app(None);
        let before = app.chat.entry_count();
        app.handle_agent_event(AgentEvent::ToolCallEnd {
            id: "orphan".to_owned(),
            content: "stray output".to_owned(),
            is_error: false,
            metadata: crate::tool::ToolMetadata::default(),
        });
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
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.dispatch_user_action(UserAction::SubmitPrompt("queued-a".to_owned()));
        rx.recv().await.expect("queued-a submit forwarded");
        app.dispatch_user_action(UserAction::SubmitPrompt("queued-b".to_owned()));
        rx.recv().await.expect("queued-b submit forwarded");

        let chat_before = app.chat.entry_count();
        app.handle_agent_event(AgentEvent::PromptDrained("queued-a".to_owned()));

        // Head pops in dispatch order regardless of the event payload —
        // the `text` is for display and never reorders the FIFO.
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
    fn prompt_drained_with_empty_queue_still_pushes_chat_entry() {
        // Defensive: post-cancel-window-fix the agent never emits
        // `PromptDrained` for items the TUI's mirror lacks, but if it
        // ever did (agent / TUI desync), the handler must still
        // surface the text instead of swallowing the event silently.
        let (mut app, _rx, _agent_tx) = test_app(None);
        let chat_before = app.chat.entry_count();

        app.handle_agent_event(AgentEvent::PromptDrained("orphan".to_owned()));

        assert!(app.pending_prompts.is_empty());
        assert_eq!(app.chat.entry_count(), chat_before + 1);
    }

    #[test]
    fn handle_turn_complete_becomes_idle_and_reenables_input() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        // Drive into streaming first so TurnComplete has state to tear down.
        app.dispatch_user_action(UserAction::SubmitPrompt("hi".to_owned()));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.input.is_enabled());

        app.handle_agent_event(AgentEvent::TurnComplete);
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled());
    }

    #[tokio::test]
    async fn turn_complete_drains_queue_head_and_dispatches() {
        // FIFO: oldest queued prompt fires first; the rest stay queued
        // for subsequent turn boundaries.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.pending_prompts.push_back("a".into());
        app.pending_prompts.push_back("b".into());

        app.handle_agent_event(AgentEvent::TurnComplete);

        let forwarded = rx.recv().await.expect("queued head forwarded");
        assert!(matches!(forwarded, UserAction::SubmitPrompt(s) if s == "a"));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["b".to_owned()],
        );
    }

    #[test]
    fn handle_cancelled_with_no_stream_still_pushes_marker() {
        // Cancel during a tool call (or before any stream tokens) —
        // commit_streaming is a no-op, but the marker still lands so
        // the user sees where the cancel hit.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("hi".to_owned()));
        let entries_before = app.chat.entry_count();

        app.handle_agent_event(AgentEvent::Cancelled);

        assert_eq!(app.chat.entry_count(), entries_before + 1);
        let text = rendered_text(&mut app, 60, 8);
        assert!(
            text.contains(crate::agent::event::INTERRUPTED_MARKER),
            "marker present: {text}",
        );
    }

    #[tokio::test]
    async fn cancelled_drains_queue_head_to_match_completed_path() {
        // Cancellation does not auto-clear the queue — a user who
        // interrupts a stuck turn typically still wants their planned follow-up to fire.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.pending_prompts.push_back("queued".into());

        app.handle_agent_event(AgentEvent::Cancelled);

        let forwarded = rx.recv().await.expect("queued prompt forwarded");
        assert!(matches!(forwarded, UserAction::SubmitPrompt(s) if s == "queued"));
    }

    #[test]
    fn handle_error_pushes_error_entry_and_finishes_turn() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("boom".to_owned()));
        app.handle_agent_event(AgentEvent::Error("API blew up".to_owned()));

        assert!(app.chat.last_is_error(), "error entry appended");
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled());
    }

    #[tokio::test]
    async fn handle_error_drains_queue_head_only_once() {
        // Single-drain contract: `Error` is the only teardown event
        // a failed turn fires (no paired `TurnComplete`), so
        // `finalize_idle` → `drain_pending_prompt` runs exactly once
        // and pops one head — not two — when the queue is non-empty.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.pending_prompts.push_back("first".into());
        app.pending_prompts.push_back("second".into());

        app.handle_agent_event(AgentEvent::Error("API failed".to_owned()));

        let forwarded = rx.recv().await.expect("queue head forwarded");
        assert!(matches!(forwarded, UserAction::SubmitPrompt(s) if s == "first"));
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["second".to_owned()],
            "only the head fires; the tail stays queued for the next turn",
        );
    }

    #[tokio::test]
    async fn pending_queue_survives_max_tool_rounds_bail_and_drains_serially() {
        // When agent_turn hits MAX_TOOL_ROUNDS its per-turn pending
        // buffer drops with the future, but the TUI mirror is the
        // source of truth — every queued item must surface across
        // subsequent turn boundaries via finalize_idle's drain. Pin
        // it here so a future refactor that "fixes" the agent-side
        // drop without re-firing PromptDrained gets caught.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.pending_prompts.push_back("a".into());
        app.pending_prompts.push_back("b".into());
        app.pending_prompts.push_back("c".into());

        // First turn dies (cap-bail surfaces as Error); head drains.
        app.handle_agent_event(AgentEvent::Error(
            "agent stopped after MAX_TOOL_ROUNDS".to_owned(),
        ));
        let first = rx.recv().await.expect("a re-fired as fresh turn");
        assert!(matches!(first, UserAction::SubmitPrompt(s) if s == "a"));

        // Subsequent terminal events keep peeling the queue.
        app.handle_agent_event(AgentEvent::TurnComplete);
        let second = rx.recv().await.expect("b re-fired");
        assert!(matches!(second, UserAction::SubmitPrompt(s) if s == "b"));

        app.handle_agent_event(AgentEvent::TurnComplete);
        let third = rx.recv().await.expect("c re-fired");
        assert!(matches!(third, UserAction::SubmitPrompt(s) if s == "c"));

        assert!(app.pending_prompts.is_empty());
    }

    // ── finish_turn ──

    #[test]
    fn finish_turn_evicts_orphaned_pending_calls() {
        // `ToolCallStart` without a matching `ToolCallEnd` by turn-end
        // is an orphan (crashed tool, agent-loop bug, mid-turn abort).
        // The pending entry must be discarded so long sessions don't
        // accumulate stale ids across turns.
        let (mut app, _rx, _agent_tx) = test_app_with_tools();
        app.handle_agent_event(AgentEvent::ToolCallStart {
            id: "orphan".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": "ls"}),
        });
        assert_eq!(app.pending_calls.len(), 1);
        app.handle_agent_event(AgentEvent::TurnComplete);
        assert_eq!(
            app.pending_calls.len(),
            0,
            "turn end must evict calls whose result never arrived",
        );
    }

    // ── expire_armed_exit ──

    #[test]
    fn expire_armed_exit_becomes_idle_after_window() {
        // After the 1 s window the armed state evaporates so the user
        // isn't left staring at an exit hint they didn't act on.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::ConfirmExit);
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
        let (mut app, _rx, _agent_tx) = test_app(None);
        assert_eq!(app.status_bar.status(), &Status::Idle);

        assert!(
            !app.expire_armed_exit(),
            "no-op when status isn't ExitArmed",
        );
        assert_eq!(app.status_bar.status(), &Status::Idle);
    }

    // ── render ──

    const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
    const END_SYNC: &[u8] = b"\x1b[?2026l";

    fn index_of(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    #[test]
    fn render_brackets_frame_with_sync_update_bytes() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        let (mut terminal, buf) = crossterm_test_terminal(60, 8);

        app.render(&mut terminal).unwrap();

        let bytes = buf.lock().unwrap();
        let begin = index_of(&bytes, BEGIN_SYNC).expect("BeginSynchronizedUpdate emitted");
        let end = index_of(&bytes, END_SYNC).expect("EndSynchronizedUpdate emitted");
        assert!(begin < end, "sync update must bracket the rendered frame");
    }

    #[test]
    fn render_repaints_when_chat_content_grows_past_viewport() {
        use std::fmt::Write as _;

        // Content pushed in the same handler tick must land in the viewport on the first frame —
        // a post-paint re-clamp would arrive too late.
        let (mut app, _rx, _agent_tx) = test_app(None);
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

    // ── draw_frame ──

    #[test]
    fn draw_frame_lays_out_status_chat_and_input_in_order() {
        let (mut app, _rx, _agent_tx) = test_app(Some("Session title"));
        insta::assert_snapshot!(render_app(&mut app, 80, 10));
    }

    #[test]
    fn draw_frame_with_conversation_and_tool_call() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.chat.push_user_message("what files are here?".into());
        app.chat.push_tool_call("$", "ls");
        app.chat
            .push_tool_result("ran ls", "README.md\nCargo.toml", false);
        app.chat.append_stream_token("Two files.");
        app.chat.commit_streaming();
        insta::assert_snapshot!(render_app(&mut app, 60, 12));
    }

    #[test]
    fn draw_frame_streaming_shows_spinner_in_status_bar() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("working...".into()));
        app.handle_agent_event(AgentEvent::StreamToken("part".into()));
        insta::assert_snapshot!(render_app(&mut app, 60, 8));
    }

    #[test]
    fn draw_frame_auto_scroll_on_hides_jump_overlay() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());

        let text = rendered_text(&mut app, 60, 10);
        assert!(!text.contains("Jump to bottom"), "{text}");
    }

    #[test]
    fn draw_frame_scrolled_up_shows_jump_overlay() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 60, 10);

        app.chat
            .handle_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
        let text = rendered_text(&mut app, 60, 10);

        assert!(text.contains("Jump to bottom"), "{text}");
    }

    #[test]
    fn draw_frame_scrolled_up_counts_new_content() {
        let (mut app, _rx, _agent_tx) = test_app(None);
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
        let (mut app, _rx, _agent_tx) = test_app(Some("narrow"));
        app.chat.push_user_message("hi".into());
        insta::assert_snapshot!(render_app(&mut app, 40, 8));
    }

    #[test]
    fn draw_frame_renders_slash_popup_above_input_when_visible() {
        // Typing `/` opens the popup; draw_frame must reserve a band
        // above the input and paint at least one command row into it.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(app.input.popup_visible());

        let text = rendered_text(&mut app, 60, 14);
        assert!(
            text.contains("/help"),
            "popup must paint at least one canonical command row: {text}",
        );
    }

    #[test]
    fn draw_frame_preview_panel_renders_queued_prompts_and_overflow_tag() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".into()));
        for i in 0..5 {
            app.pending_prompts.push_back(format!("queued {i}"));
        }
        insta::assert_snapshot!(render_app(&mut app, 60, 14));
    }

    #[test]
    fn draw_frame_hides_input_and_popup_while_modal_active() {
        // Modal owns focus, so the layout must collapse the input + popup bands. The user-prompt
        // marker (`❯`) is the cheapest substring proof that the input got rendered.
        let (mut app, _rx, _agent_tx) = test_app(None);
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
    fn draw_frame_surface_fill_overwrites_unpainted_cells_with_surface_bg() {
        // Buffer-wide invariant: pre-stain every cell, render, and assert no sentinel survives.
        // The frame-area surface fill is the only widget that guarantees this for cells no
        // other widget covers.
        use ratatui::style::Color;

        let (mut app, _rx, _agent_tx) = test_app(None);
        let sentinel = Color::Rgb(254, 0, 254);
        let surface_bg = app.theme.surface().bg.expect("surface slot defines bg");

        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        for cell in &mut terminal.current_buffer_mut().content {
            cell.set_bg(sentinel);
        }
        terminal
            .draw(|frame| {
                _ = app.draw_frame(frame);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        for y in 0..10 {
            for x in 0..60 {
                let cell = buffer.cell((x, y)).expect("cell in bounds");
                assert_eq!(
                    cell.bg, surface_bg,
                    "cell ({x},{y}) kept the sentinel — surface fill regressed",
                );
            }
        }
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

    // ── preview_height ──

    #[test]
    fn preview_height_is_zero_when_queue_empty() {
        let (app, _rx, _agent_tx) = test_app(None);
        assert_eq!(app.preview_height(), 0);
    }

    #[test]
    fn preview_height_caps_at_visible_plus_overflow_row() {
        let (mut app, _rx, _agent_tx) = test_app(None);
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
        // A queue larger than `PREVIEW_VISIBLE` collapses the tail
        // into a single "+N more" hint so the panel never grows past
        // the cap; the user keeps the most recent items in view.
        let (mut app, _rx, _agent_tx) = test_app(None);
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
}
