//! Root TUI application.
//!
//! [`App`] owns every component (chat, input, status), holds the
//! cross-task channels, and runs the `tokio::select!` loop that
//! multiplexes crossterm events, agent events, user actions, and a
//! 60 FPS render tick. Render coalescing (dirty flag + timer) keeps
//! redraw work proportional to state change rather than event throughput.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent};
use futures::{Stream, StreamExt};
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use tokio::sync::mpsc;

use super::components::chat::ChatView;
use super::components::input::InputArea;
use super::components::status::{Status, StatusBar};
use super::glyphs::{NEWLINE_GLYPH, USER_PROMPT_PREFIX, USER_PROMPT_PREFIX_WIDTH};
use super::modal::{ModalAction, ModalStack};
use super::pending_calls::{PendingCall, PendingCalls, result_header};
use super::terminal::{Tui, draw_sync};
use super::theme::Theme;
use crate::agent::event::{AgentEvent, UserAction};
use crate::message::Message;
use crate::slash::{self, SessionInfo, SlashContext, SlashKind};
use crate::tool::{ToolMetadata, ToolRegistry, ToolResultView};
use crate::util::text::truncate_to_width;

/// Tick interval for animation frames and render coalescing (~60 FPS).
const TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Window in which a second Ctrl+C confirms exit.
const EXIT_WINDOW: Duration = Duration::from_secs(1);

/// Maximum queued prompts shown in the preview before collapsing into `+N more`.
const PREVIEW_VISIBLE: usize = 3;

/// Root application state. Owns all components and drives the render loop.
pub(crate) struct App {
    theme: Theme,
    status_bar: StatusBar,
    chat: ChatView,
    input: InputArea,
    session_info: SessionInfo,
    agent_rx: mpsc::Receiver<AgentEvent>,
    user_tx: mpsc::Sender<UserAction>,
    tools: Arc<ToolRegistry>,
    /// Correlates `ToolCallStart` with its matching `ToolCallEnd`.
    pending_calls: PendingCalls,
    /// FIFO of prompts submitted mid-turn; drained at turn boundaries.
    pending_prompts: VecDeque<String>,
    /// Active modal overlay(s). Empty when no modal is on screen.
    modals: ModalStack,
    should_quit: bool,
    /// Whether state has changed since the last render.
    dirty: bool,
}

impl App {
    #[expect(
        clippy::too_many_arguments,
        reason = "ctor wires the full surface (display config, IPC channels, resumed state, tool registry); a builder would obscure which dependencies App owns"
    )]
    pub(crate) fn new(
        theme: &Theme,
        session_info: SessionInfo,
        show_thinking: bool,
        title: Option<String>,
        agent_rx: mpsc::Receiver<AgentEvent>,
        user_tx: mpsc::Sender<UserAction>,
        history: &[Message],
        history_metadata: &HashMap<String, ToolMetadata>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        let mut chat = ChatView::new(theme, show_thinking);
        chat.load_history(history, history_metadata, tools.as_ref());
        let mut status_bar = StatusBar::new(
            theme,
            session_info.marketing_name().into_owned(),
            session_info.cwd.clone(),
        );
        status_bar.set_title(title);
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
            pending_prompts: VecDeque::new(),
            modals: ModalStack::new(),
            should_quit: false,
            dirty: true,
        }
    }

    /// Main event loop. Runs until the user quits or the agent channel closes.
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
            tokio::select! {
                    event = crossterm_events.next() => {
                    if let Some(Ok(event)) = event {
                        self.handle_crossterm_event(&event);
                    }
                }
                event = self.agent_rx.recv() => {
                    match event {
                        Some(event) => self.handle_agent_event(event),
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

    /// Dispatcher for actions emitted by a closed modal.
    fn apply_modal_action(&mut self, action: ModalAction) {
        match action {
            ModalAction::None => {}
            ModalAction::User(user_action) => self.dispatch_user_action(user_action),
        }
    }

    #[cfg(test)]
    pub(crate) fn push_modal(&mut self, modal: Box<dyn super::modal::Modal>) {
        self.modals.push(modal);
        self.dirty = true;
    }

    /// Routes Esc: cancel if busy, pop queue if idle+empty, else no-op.
    fn handle_esc(&mut self) {
        if !self.input.is_enabled() {
            self.dispatch_user_action(UserAction::Cancel);
        } else if self.input.is_empty()
            && let Some(prompt) = self.pending_prompts.pop_back()
        {
            self.input.set_text(&prompt);
            self.sync_input_queue_hint();
        }
    }

    /// Applies UI side-effects then forwards to the agent channel.
    fn dispatch_user_action(&mut self, action: UserAction) {
        if !self.apply_action_locally(&action) {
            return;
        }
        self.forward_to_agent(action);
    }

    /// Send `action` to the agent loop; channel errors land as a chat
    /// error block. Reused by the slash `Action(_)` branch — both
    /// `Action(SubmitPrompt(_))` and `Action(Clear)` flow through here.
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

    /// Applies UI-state changes; returns whether to forward to the agent.
    fn apply_action_locally(&mut self, action: &UserAction) -> bool {
        match action {
            UserAction::SubmitPrompt(text) => {
                if self.input.is_enabled() {
                    if let Some(parsed) = slash::parse_slash(text) {
                        self.chat.push_user_message(text.clone());
                        let (synthesized, modal) = {
                            let mut ctx = SlashContext::new(&mut self.chat, &self.session_info);
                            let action = slash::dispatch(&parsed, &mut ctx);
                            (action, ctx.take_modal())
                        };
                        if let Some(modal) = modal {
                            self.modals.push(modal);
                        }
                        if let Some(action) = synthesized {
                            if matches!(action, UserAction::SubmitPrompt(_)) {
                                self.input.set_enabled(false);
                                self.status_bar.set_status(Status::Streaming);
                            }
                            self.forward_to_agent(action);
                        }
                        return false;
                    }
                    self.chat.push_user_message(text.clone());
                    self.input.set_enabled(false);
                    self.status_bar.set_status(Status::Streaming);
                    true
                } else {
                    if let Some(parsed) = slash::parse_slash(text) {
                        self.chat.push_user_message(text.clone());
                        match slash::classify(&parsed) {
                            SlashKind::ReadOnly | SlashKind::Unknown => {
                                let modal = {
                                    let mut ctx =
                                        SlashContext::new(&mut self.chat, &self.session_info);
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
                    self.pending_prompts.push_back(text.clone());
                    self.sync_input_queue_hint();
                    !matches!(self.status_bar.status(), Status::Cancelling)
                }
            }
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
            UserAction::Clear | UserAction::SwapConfig { .. } => true,
        }
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
                self.sync_input_queue_hint();
            }
            AgentEvent::TurnComplete => {
                self.finish_turn();
            }
            AgentEvent::Cancelled => {
                self.chat.push_interrupted_marker();
                self.finalize_idle();
            }
            AgentEvent::SessionTitleUpdated { session_id, title } => {
                if session_id == self.session_info.session_id {
                    self.status_bar.set_title(Some(title));
                }
            }
            AgentEvent::SessionRolled { id } => {
                self.session_info.session_id = id;
                self.status_bar.set_title(None);
                self.chat.clear_history();
                self.chat
                    .push_system_message("Conversation cleared. Next message starts fresh.");
            }
            AgentEvent::ConfigChanged {
                model_id,
                effort,
                requested_effort,
            } => {
                let model_changed = model_id != self.session_info.config.model_id;
                let prev_effort = self.session_info.config.effort;
                let marketing = crate::model::marketing_or_id(&model_id);
                let confirmation = format_config_change(
                    &marketing,
                    &model_id,
                    model_changed,
                    prev_effort,
                    effort,
                    requested_effort,
                );
                if model_changed {
                    self.status_bar.set_model(marketing.into_owned());
                }
                self.session_info.config.model_id = model_id;
                self.session_info.config.effort = effort;
                self.chat.push_system_message(confirmation);
            }
            AgentEvent::Error(msg) => {
                self.chat.push_error(&msg);
                self.finish_turn();
            }
        }
        self.dirty = true;
    }

    fn finish_turn(&mut self) {
        self.chat.commit_streaming();
        self.finalize_idle();
    }

    /// Resets to idle: clears orphan calls, re-enables input, drains queued prompts.
    fn finalize_idle(&mut self) {
        self.pending_calls.clear();
        self.status_bar.set_status(Status::Idle);
        self.input.set_enabled(true);
        self.drain_pending_prompt();
    }

    /// Pops the front of the queue and dispatches as a fresh submit.
    fn drain_pending_prompt(&mut self) {
        if let Some(prompt) = self.pending_prompts.pop_front() {
            self.dispatch_user_action(UserAction::SubmitPrompt(prompt));
        }
        self.sync_input_queue_hint();
    }

    fn sync_input_queue_hint(&mut self) {
        self.input.set_has_queued(!self.pending_prompts.is_empty());
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

    /// Returns `true` when an [`Status::ExitArmed`] window has elapsed
    /// and the bar was reset to idle.
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

    /// Draws all components and returns the chat area for scroll-cache bookkeeping.
    fn draw_frame(&mut self, frame: &mut ratatui::Frame<'_>) -> ratatui::layout::Rect {
        let input_height = self.input.height();
        let preview_height = self.preview_height();
        let popup_height = self.input.popup_height();
        let modal_height = self.modals.height(frame.area().width);
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
        self.chat.render(frame, chunks[1]);
        if preview_height > 0 {
            self.render_preview(frame, chunks[2]);
        }
        if modal_height > 0 {
            self.modals.render(frame, chunks[3], &self.theme);
        }
        if popup_height > 0 {
            self.input.render_popup(frame, chunks[4]);
        }
        self.input.render(frame, chunks[5]);
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
}

/// Renders a single queued prompt as a dim user-message ghost, capped at `body_width` columns.
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
    marketing: &str,
    model_id: &str,
    model_changed: bool,
    prev_effort: Option<crate::config::Effort>,
    new_effort: Option<crate::config::Effort>,
    requested_effort: Option<crate::config::Effort>,
) -> String {
    if !model_changed {
        return match (requested_effort, new_effort) {
            (Some(req), Some(eff)) if req == eff => format!("Effort set to {eff}."),
            (Some(req), Some(eff)) => format!("Effort set to {eff} (clamped from {req})."),
            (Some(req), None) => {
                format!("Effort unchanged — model has no effort tier (asked for {req}).")
            }
            // No-op SwapConfig — slash dispatch keeps this unreachable
            // in practice, but a clear fallback beats a panic.
            (None, _) => "Config unchanged.".to_owned(),
        };
    }
    let head = format!("Switched to {marketing} ({model_id})");
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
    use crate::tool::ToolRegistry;

    /// Fresh idle `App` plus the `user_tx` consumer (for forwarded-action
    /// assertions) and the `agent_tx` producer (kept alive so the
    /// `agent_rx` side doesn't close on construction).
    fn test_app(
        title: Option<&str>,
    ) -> (App, mpsc::Receiver<UserAction>, mpsc::Sender<AgentEvent>) {
        test_app_with_registry(title, Arc::new(ToolRegistry::new(Vec::new())))
    }

    /// Variant that plumbs the real tool catalog into the `App` so
    /// `ToolCallStart` label lookups match what production would render.
    /// Used by tool-event tests that exercise the Start → End flow.
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
            title.map(ToOwned::to_owned),
            agent_rx,
            user_tx,
            &[],
            &HashMap::new(),
            tools,
        );
        (app, user_rx, agent_tx)
    }

    fn test_session_info() -> SessionInfo {
        // `model_id = "test-model"` is intentionally unknown so
        // `marketing_or_id` falls back to the literal id, keeping
        // every TUI insta snapshot stable as `test-model`.
        use crate::config::{ConfigSnapshot, Effort, PromptCacheTtl};

        SessionInfo {
            cwd: "~/test".to_owned(),
            version: "0.0.0-test",
            session_id: "test-session".to_owned(),
            config: ConfigSnapshot {
                auth_label: "API key",
                base_url: "https://api.test.invalid".to_owned(),
                model_id: "test-model".to_owned(),
                effort: Some(Effort::High),
                max_tokens: 32_000,
                prompt_cache_ttl: PromptCacheTtl::OneHour,
                show_thinking: false,
            },
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
        // Status bar filters whitespace-only titles, so plumbing such a
        // value from `SessionData` won't leave a blank slot in the bar.
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
        // Streaming spinner flips frames after 5 * 16ms = 80ms, so
        // sleep past that to drive `status_bar.tick()` truthy.
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
        // Pin the crossterm and agent arms of the select! loop:
        // a key reaches the input, a stream token disables it, and
        // closing the agent channel ends the loop.
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
        // Simulate typing "hi" then Enter — the input area composes the
        // prompt and returns `SubmitPrompt`, which `handle_crossterm_event`
        // must pipe into `dispatch_user_action`.
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
        // Mid-turn Ctrl+C must reach the agent loop as `Cancel` so it
        // can drop the future, not flip `should_quit` and tear down
        // the TUI. Drive the app into the streaming state first to
        // mirror production: input disabled => cancel branch fires.
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
        // Mouse events reach `ChatView::handle_event` which consumes them
        // for scroll. Assert the dirty flag flips so the next tick renders.
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
        // Resize matches the arm that does no per-component work but still
        // falls through to `self.dirty = true`, so the next tick re-runs
        // the layout split with the new `frame.area()`.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dirty = false;
        app.handle_crossterm_event(&Event::Resize(80, 24));
        assert!(app.dirty, "Resize must trigger a re-layout render");
    }

    #[test]
    fn handle_crossterm_unknown_event_is_a_noop() {
        // The `_ => return` arm covers FocusGained/FocusLost/Paste — the
        // early return here is significant: every other arm falls through
        // to `self.dirty = true`, so without this branch a stream of
        // unhandled terminal events would cause continuous re-renders.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dirty = false;
        app.handle_crossterm_event(&Event::FocusGained);
        assert!(!app.dirty);
    }

    #[test]
    fn handle_crossterm_scroll_key_routes_to_chat_while_input_disabled() {
        // When the input is disabled (mid-stream), arrow / page keys
        // must still reach chat so the user can scroll through output.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.input.set_enabled(false);
        app.handle_crossterm_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(app.dirty);
    }

    #[tokio::test]
    async fn handle_crossterm_popup_enter_dispatches_canonical_command() {
        // Typing `/h` filters the popup to /help; Enter on the
        // highlighted row submits it through the existing dispatch
        // path. /help is read-only, so the chat lands a system
        // message instead of forwarding to the agent.
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
        // Tab on a popup row inserts `/{name} ` and hides the popup —
        // the user is now in args-typing mode. Filter to /help first
        // so the test pins the completion shape independent of the BUILT_INS-ordered first row.
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
        // While a modal is on screen, any key event lands on the modal
        // first — the input area must NOT receive them. Pin so a
        // regression that fans keys to both surfaces (double-handling
        // a single keystroke) fails here.
        use crate::tui::modal::ModalAction;
        use crate::tui::modal::testing::ScriptedModal;

        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.push_modal(Box::new(ScriptedModal::new(ModalAction::User(
            UserAction::Cancel,
        ))));

        // Type a printable that the input area would otherwise capture.
        app.handle_crossterm_event(&key_event(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(
            app.input.lines().iter().all(String::is_empty),
            "input must stay empty while modal is active",
        );
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "no UserAction must reach user_tx for a key the modal consumed",
        );

        // Submit the modal — its action flows through the normal
        // dispatch path, just like a keyboard-typed UserAction.
        app.handle_crossterm_event(&key_event(KeyCode::Char('s'), KeyModifiers::NONE));
        let forwarded = rx.recv().await.expect("modal-submitted action forwarded");
        assert!(matches!(forwarded, UserAction::Cancel));
    }

    #[test]
    fn modal_gate_cancel_closes_modal_without_dispatching() {
        // `ModalKey::Cancelled` pops the modal but does not dispatch a
        // UserAction. The next key must reach the input area.
        use crate::tui::modal::ModalAction;
        use crate::tui::modal::testing::ScriptedModal;

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
        // Esc is routed by `App` because its meaning depends on queue
        // state and run-state; the input component can't see those.
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
        // The most-recent (back of the FIFO) returns to the input for
        // editing; the rest stay queued so the user can keep peeling.
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
        // Esc must not clobber an in-progress draft. The user has to
        // clear the buffer (or submit) before peeling a queued prompt.
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
        // The popup gate sits in front of App's Esc routing so a
        // visible popup can swallow Esc before queue / cancel logic
        // fires. Open the popup by typing `/`, queue a prompt, then
        // press Esc — popup hides, queue stays put.
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
        // Slash commands must stay client-side: user message + command
        // output land in chat, agent loop never sees the prompt.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("/help".to_owned()));

        assert_eq!(
            app.chat.entry_count(),
            2,
            "user-message + system-message blocks expected",
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
        // `//foo` parses as "not a command", so the agent receives the
        // bytes verbatim — pin so a future prefix check can't swallow it.
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
        // Cancel acknowledges the user request immediately by flipping
        // the status; the matching `AgentEvent::Cancelled` returns to
        // idle once the agent loop has actually dropped the future.
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("hi".to_owned()));
        // Drain the SubmitPrompt to isolate the Cancel payload.
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
        // Status bar stays idle — Quit flows past the streaming setup so
        // the tear-down path doesn't have to un-spinner it.
        assert_eq!(app.status_bar.status(), &Status::Idle);
    }

    #[test]
    fn dispatch_closed_channel_surfaces_error_and_quits() {
        // Dropping `user_rx` simulates the agent task exiting — try_send
        // returns `Closed`. The UI must announce the failure and tear
        // itself down so the user isn't left staring at a stuck spinner.
        let (mut app, rx, _agent_tx) = test_app(None);
        drop(rx);

        app.dispatch_user_action(UserAction::SubmitPrompt("hi".to_owned()));

        assert!(app.should_quit, "closed channel must trigger teardown");
        assert!(
            !app.input.is_enabled(),
            "input stays disabled during teardown"
        );
        // User message pushed before try_send, error block after — two entries.
        assert_eq!(app.chat.entry_count(), 2);
        assert!(
            app.chat.last_is_error(),
            "closed-channel error should be the final block"
        );
    }

    #[test]
    fn dispatch_full_channel_surfaces_error_but_keeps_app_alive() {
        // Fill the 8-slot channel without draining, then overflow.
        // `Cancel` still goes through `try_send` (the queue only routes
        // submits) so it's the natural way to overflow now that submits
        // during a busy turn buffer locally instead of forwarding.
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

        // Buffered for the preview pane AND forwarded so `agent_turn`
        // can splice it into the same multi-step turn at the next
        // round boundary. The chat history stays untouched until the
        // matching `PromptDrained` event lands.
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
        // Modal-emitted SwapConfig must reach the agent loop so it can call
        // `apply_swap_config` and emit `ConfigChanged`. The earlier `=> false` arm in
        // `apply_action_locally` swallowed it silently — caused empty title bar updates after
        // picker submit. Pin both axes.
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
    async fn dispatch_submit_during_cancelling_holds_locally_without_forwarding() {
        // Cancel-window FIFO authority: between the user pressing Esc
        // and the matching `AgentEvent::Cancelled` arriving, the agent's
        // outer `recv` is racing for the next signal. Forwarding a fresh
        // submit here lets it slip ahead of `pending_prompts`'s existing
        // head — the agent picks it up and starts a new turn while older
        // queued items fall behind. Hold mid-turn submits locally;
        // `finalize_idle`'s drain re-fires them in order after `Cancelled` lands.
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
    async fn dispatch_read_only_slash_during_busy_runs_client_side_without_queueing() {
        // Read-only slash commands typed during a busy turn must run
        // immediately. Otherwise the queue-drain path persists them as
        // user prompts and the LLM ends up answering `/help` literally.
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
        // User-message + dispatched help block, on top of the active prompt.
        assert_eq!(app.chat.entry_count(), 3);
        assert!(!app.chat.last_is_error());
    }

    #[tokio::test]
    async fn dispatch_clear_during_busy_refuses_with_system_message_no_dispatch() {
        // State-mutating commands must refuse mid-turn — rolling the
        // session while `messages` is still draining would race the
        // in-flight turn into the wrong JSONL.
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
        // The chat shows only `/init`; the agent must receive the body.
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
        // Mutating ⇒ refuse. The typed `/init` row still lands; only
        // the synthesized body is suppressed.
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
    async fn dispatch_arg_bearing_slash_during_busy_refuses_with_system_message_no_forward() {
        // `/model <id>` and `/effort <level>` both mutate the live
        // Client and must wait for idle. Pinning both so a regression
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
    async fn dispatch_bare_slash_during_busy_opens_modal_picker() {
        // Bare `/model` classifies as ReadOnly so it dispatches mid-turn and opens the picker
        // modal instead of printing a list. (`/effort` is Mutating regardless of args after the
        // typed-arg-only refactor — its bare-form busy path is covered by
        // `dispatch_arg_bearing_slash_during_busy_refuses_with_system_message_no_forward`.)
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("active".to_owned()));

        app.dispatch_user_action(UserAction::SubmitPrompt("/model".to_owned()));

        assert!(
            app.modals.is_active(),
            "bare /model must push a modal mid-turn",
        );
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
    fn handle_config_changed_with_model_swap_refreshes_status_bar_session_info_and_chat() {
        // Three surfaces refresh in one shot: status-bar label,
        // `session_info` (backs `/status` / `/config`), and a chat
        // confirmation block. Marketing name is derived locally from `model_id`.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::ConfigChanged {
            model_id: "claude-sonnet-4-6".to_owned(),
            effort: Some(crate::config::Effort::High),
            requested_effort: None,
        });

        assert_eq!(app.session_info.config.model_id, "claude-sonnet-4-6");
        assert_eq!(
            app.session_info.config.effort,
            Some(crate::config::Effort::High),
        );
        assert_eq!(app.status_bar.model(), "Claude Sonnet 4.6");
        let body = app.chat.last_system_text().expect("confirmation block");
        assert_eq!(
            body,
            "Switched to Claude Sonnet 4.6 (claude-sonnet-4-6) · effort high.",
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
            "Claude Haiku 4.5",
            "claude-haiku-4-5",
            true,
            None,
            None,
            None,
        );
        assert_eq!(s, "Switched to Claude Haiku 4.5 (claude-haiku-4-5).");
    }

    #[test]
    fn format_config_change_swap_clears_effort_when_new_model_drops_it() {
        // User had a tier; new model has none. Surface the change so
        // the user knows their effort just disappeared.
        let s = format_config_change(
            "Claude Haiku 4.5",
            "claude-haiku-4-5",
            true,
            Some(crate::config::Effort::Xhigh),
            None,
            None,
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
            "Claude Opus 4.7",
            "claude-opus-4-7",
            true,
            None,
            Some(crate::config::Effort::Xhigh),
            None,
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
            "Claude Sonnet 4.6",
            "claude-sonnet-4-6",
            true,
            Some(crate::config::Effort::Xhigh),
            Some(crate::config::Effort::High),
            None,
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
            "Claude Opus 4.7",
            "claude-opus-4-7",
            true,
            Some(crate::config::Effort::High),
            Some(crate::config::Effort::High),
            None,
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
            "Claude Sonnet 4.6",
            "claude-sonnet-4-6",
            true,
            Some(crate::config::Effort::Medium),
            Some(crate::config::Effort::High),
            Some(crate::config::Effort::Xhigh),
        );
        assert_eq!(
            s,
            "Switched to Claude Sonnet 4.6 (claude-sonnet-4-6) · effort high (clamped from xhigh)."
        );
    }

    #[test]
    fn format_config_change_effort_explicit_pick_matches_resolution() {
        let s = format_config_change(
            "Claude Opus 4.7",
            "claude-opus-4-7",
            false,
            Some(crate::config::Effort::High),
            Some(crate::config::Effort::Xhigh),
            Some(crate::config::Effort::Xhigh),
        );
        assert_eq!(s, "Effort set to xhigh.");
    }

    #[test]
    fn format_config_change_effort_clamp_surfaces_what_user_asked_for() {
        let s = format_config_change(
            "Claude Sonnet 4.6",
            "claude-sonnet-4-6",
            false,
            Some(crate::config::Effort::Medium),
            Some(crate::config::Effort::High),
            Some(crate::config::Effort::Xhigh),
        );
        assert_eq!(s, "Effort set to high (clamped from xhigh).");
    }

    #[test]
    fn format_config_change_effort_pick_on_no_tier_model_surfaces_loss() {
        // The slash command preflight stops this through /effort, but
        // client-driven flows could still emit it.
        let s = format_config_change(
            "Claude Haiku 4.5",
            "claude-haiku-4-5",
            false,
            None,
            None,
            Some(crate::config::Effort::High),
        );
        assert_eq!(
            s,
            "Effort unchanged — model has no effort tier (asked for high)."
        );
    }

    #[test]
    fn handle_session_rolled_clears_chat_rebinds_id_and_drops_stale_title() {
        let (mut app, _rx, _agent_tx) = test_app(Some("Old session title"));
        app.chat.push_user_message("old prompt".to_owned());
        let original_id = app.session_info.session_id.clone();

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
        assert_eq!(
            app.chat.entry_count(),
            1,
            "only the confirmation message remains after clear",
        );
        assert_eq!(
            app.chat.last_system_text(),
            Some("Conversation cleared. Next message starts fresh."),
        );
        assert!(app.dirty);
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
    fn render_repaints_when_slash_dispatch_grows_content_past_viewport() {
        // Regression: pre-fix, slash output landed below the viewport
        // until the user scrolled — the post-paint `update_layout`
        // re-clamp arrived too late for the same frame.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("/help".to_owned()));
        // Tight viewport guarantees /help overflows.
        let text = rendered_text(&mut app, 60, 12);
        assert!(
            text.contains("//etc/hosts"),
            "tail of /help body must be in the viewport after the first render, got:\n{text}",
        );
    }

    // ── draw_frame ──

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
        // The matching input-border style change is validated in
        // `input::render_disabled_applies_dim_foreground_to_text` — a
        // text-only snapshot would render identically either way.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("working...".into()));
        app.handle_agent_event(AgentEvent::StreamToken("part".into()));
        insta::assert_snapshot!(render_app(&mut app, 60, 8));
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
