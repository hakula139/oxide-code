use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout};
use tokio::sync::mpsc;

use super::component::{Action, Component};
use super::components::chat::ChatView;
use super::components::input::InputArea;
use super::components::status::{Status, StatusBar};
use super::event::{AgentEvent, UserAction};
use super::terminal::{Tui, draw_sync};

/// Tick interval for animation frames and render coalescing (~60 FPS).
const TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Root application state. Owns all components and drives the render loop.
pub struct App {
    status_bar: StatusBar,
    chat: ChatView,
    input: InputArea,
    agent_rx: mpsc::UnboundedReceiver<AgentEvent>,
    user_tx: mpsc::UnboundedSender<UserAction>,
    should_quit: bool,
    /// Whether state has changed since the last render.
    dirty: bool,
}

impl App {
    pub fn new(
        model: String,
        agent_rx: mpsc::UnboundedReceiver<AgentEvent>,
        user_tx: mpsc::UnboundedSender<UserAction>,
    ) -> Self {
        Self {
            status_bar: StatusBar::new(model),
            chat: ChatView::new(),
            input: InputArea::new(),
            agent_rx,
            user_tx,
            should_quit: false,
            dirty: true,
        }
    }

    /// Main event loop. Runs until the user quits or the agent channel closes.
    pub async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
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
                // Tick — coalesce renders.
                _ = tick.tick() => {
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
                    self.handle_action(action);
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

    fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::StreamToken(token) => {
                self.chat.append_stream_token(&token);
                self.status_bar.set_status(Status::Streaming);
                self.input.set_enabled(false);
            }
            AgentEvent::ThinkingToken(_) => {
                // TODO(PR 3.2): Render thinking in a dimmed/collapsible block.
                self.status_bar.set_status(Status::Streaming);
            }
            AgentEvent::ToolCallStart { name, .. } => {
                self.chat.commit_streaming();
                self.chat.push_tool_call(&name, None);
                self.status_bar.set_status(Status::ToolRunning);
            }
            AgentEvent::ToolCallEnd { title, .. } => {
                // Update the last tool call with its title if available.
                if let Some(title) = &title {
                    // For now, push a result line. PR 3.4 will render this
                    // as a collapsible tool result block.
                    self.chat.push_tool_call("result", Some(title));
                }
            }
            AgentEvent::TurnComplete => {
                self.chat.commit_streaming();
                self.status_bar.set_status(Status::Idle);
                self.input.set_enabled(true);
            }
            AgentEvent::Error(msg) => {
                self.chat.commit_streaming();
                self.chat.push_tool_call("error", Some(&msg));
                self.status_bar.set_status(Status::Idle);
                self.input.set_enabled(true);
            }
        }
        self.dirty = true;
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::SubmitPrompt(text) => {
                self.chat.push_user_message(text.clone());
                self.input.set_enabled(false);
                _ = self.user_tx.send(UserAction::SubmitPrompt(text));
            }
            Action::Quit => {
                _ = self.user_tx.send(UserAction::Quit);
                self.should_quit = true;
            }
        }
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
