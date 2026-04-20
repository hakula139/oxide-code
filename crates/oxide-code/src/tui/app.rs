use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout};
use tokio::sync::mpsc;

use super::component::Component;
use super::components::chat::ChatView;
use super::components::input::InputArea;
use super::components::status::{Status, StatusBar};
use super::terminal::{Tui, draw_sync};
use super::theme::Theme;
use crate::agent::event::{AgentEvent, UserAction};
use crate::message::Message;
use crate::tool::ToolRegistry;

/// Tick interval for animation frames and render coalescing (~60 FPS).
const TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Root application state. Owns all components and drives the render loop.
pub(crate) struct App {
    status_bar: StatusBar,
    chat: ChatView,
    input: InputArea,
    agent_rx: mpsc::Receiver<AgentEvent>,
    user_tx: mpsc::Sender<UserAction>,
    tools: Arc<ToolRegistry>,
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
        model: String,
        show_thinking: bool,
        cwd: String,
        title: Option<String>,
        agent_rx: mpsc::Receiver<AgentEvent>,
        user_tx: mpsc::Sender<UserAction>,
        history: &[Message],
        tools: Arc<ToolRegistry>,
    ) -> Self {
        let theme = Theme::default();
        let mut chat = ChatView::new(theme, show_thinking);
        chat.load_history(history, tools.as_ref());
        let mut status_bar = StatusBar::new(theme, model, cwd);
        status_bar.set_title(title);
        Self {
            status_bar,
            chat,
            input: InputArea::new(theme),
            agent_rx,
            user_tx,
            tools,
            should_quit: false,
            dirty: true,
        }
    }

    /// Main event loop. Runs until the user quits or the agent channel closes.
    pub(crate) async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        let mut crossterm_events = EventStream::new();
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Initial render.
        self.render(terminal)?;

        loop {
            tokio::select! {
                // Crossterm events (keyboard, mouse, resize).
                event = crossterm_events.next() => {
                    if let Some(Ok(event)) = event {
                        self.handle_crossterm_event(&event);
                    }
                }
                // Agent events (stream tokens, tool calls, etc.).
                event = self.agent_rx.recv() => {
                    match event {
                        Some(event) => self.handle_agent_event(event),
                        None => {
                            // Agent channel closed — agent loop exited.
                            self.should_quit = true;
                        }
                    }
                }
                // Tick — coalesce renders and advance spinner.
                _ = tick.tick() => {
                    if self.status_bar.tick() {
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
        match event {
            Event::Key(..) => {
                // Input area handles typing, submit, and quit.
                if let Some(action) = self.input.handle_event(event) {
                    self.dispatch_user_action(action);
                }
                // When input is disabled (streaming), scroll keys go to chat.
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

    /// Translate a user action into UI state changes, then forward it to the
    /// agent loop over the bounded channel. `try_send` would only fail if the
    /// agent task has died; in that case `should_quit` tears down the TUI on
    /// the next iteration so nothing is lost.
    fn dispatch_user_action(&mut self, action: UserAction) {
        match &action {
            UserAction::SubmitPrompt(text) => {
                self.chat.push_user_message(text.clone());
                self.input.set_enabled(false);
                self.status_bar.set_status(Status::Streaming);
            }
            UserAction::Quit => {
                self.should_quit = true;
            }
        }
        _ = self.user_tx.try_send(action);
    }

    fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::StreamToken(token) => {
                self.chat.append_stream_token(&token);
                self.status_bar.set_status(Status::Streaming);
                self.input.set_enabled(false);
            }
            AgentEvent::ThinkingToken(token) => {
                self.chat.append_thinking_token(&token);
                self.status_bar.set_status(Status::Streaming);
            }
            AgentEvent::ToolCallStart { name, input, .. } => {
                self.chat.commit_streaming();
                let icon = self.tools.icon(&name);
                let label = self
                    .tools
                    .summarize_input(&name, &input)
                    .map_or_else(|| name.clone(), str::to_owned);
                self.chat.push_tool_call(icon, &label);
                self.status_bar.set_status(Status::ToolRunning);
            }
            AgentEvent::ToolCallEnd {
                title,
                content,
                is_error,
                ..
            } => {
                if let Some(title) = &title {
                    self.chat.push_tool_result(title, &content, is_error);
                }
            }
            AgentEvent::TurnComplete => {
                self.finish_turn();
            }
            AgentEvent::SessionTitleUpdated(title) => {
                self.status_bar.set_title(Some(title));
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
        self.status_bar.set_status(Status::Idle);
        self.input.set_enabled(true);
    }

    // ── Rendering ──

    fn render(&mut self, terminal: &mut Tui) -> Result<()> {
        let input_height = self.input.height();

        // Capture areas for post-render layout update.
        let mut chat_area = ratatui::layout::Rect::default();

        draw_sync(terminal, |frame| {
            let chunks = Layout::vertical([
                Constraint::Length(2),            // status bar (content + border)
                Constraint::Min(1),               // chat view
                Constraint::Length(input_height), // input area
            ])
            .split(frame.area());

            self.status_bar.render(frame, chunks[0]);
            self.chat.render(frame, chunks[1]);
            self.input.render(frame, chunks[2]);

            chat_area = chunks[1];
        })?;

        // Update layout bookkeeping outside the render closure.
        self.chat.update_layout(chat_area);

        Ok(())
    }
}
