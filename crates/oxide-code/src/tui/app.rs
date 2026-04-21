//! Root TUI application.
//!
//! [`App`] owns every component (chat, input, status), holds the
//! cross-task channels, and runs the `tokio::select!` loop that
//! multiplexes crossterm events, agent events, user actions, and a
//! 60 FPS render tick. Render coalescing (dirty flag + timer) keeps
//! redraw work proportional to state change rather than event
//! throughput.

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
        let mut chat_area = ratatui::layout::Rect::default();
        draw_sync(terminal, |frame| {
            chat_area = self.draw_frame(frame);
        })?;
        // Layout bookkeeping lives outside the render closure.
        self.chat.update_layout(chat_area);
        Ok(())
    }

    /// Lays out the three panels (status bar, chat, input) into the
    /// frame and dispatches to each component's `render`. Returns the
    /// chat area so the caller can update scroll-cache bookkeeping.
    /// Backend-agnostic (takes `&mut Frame`) so `TestBackend` tests can
    /// exercise the same layout logic as the live crossterm path.
    fn draw_frame(&mut self, frame: &mut ratatui::Frame<'_>) -> ratatui::layout::Rect {
        let input_height = self.input.height();
        let chunks = Layout::vertical([
            Constraint::Length(2),            // status bar (content + border)
            Constraint::Min(1),               // chat view
            Constraint::Length(input_height), // input area
        ])
        .split(frame.area());

        self.status_bar.render(frame, chunks[0]);
        self.chat.render(frame, chunks[1]);
        self.input.render(frame, chunks[2]);
        chunks[1]
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::tool::ToolRegistry;

    // `App::run` / `App::render` need a real terminal and stay untested
    // here; every other method is pure state mutation over `App`, which
    // these tests drive by constructing one without a terminal and
    // asserting on the observable side effects.

    /// Fresh idle `App` plus the `user_tx` consumer (for forwarded-action
    /// assertions) and the `agent_tx` producer (kept alive so the
    /// `agent_rx` side doesn't close on construction).
    fn test_app(
        title: Option<&str>,
    ) -> (App, mpsc::Receiver<UserAction>, mpsc::Sender<AgentEvent>) {
        let (agent_tx, agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (user_tx, user_rx) = mpsc::channel::<UserAction>(8);
        let app = App::new(
            "test-model".to_owned(),
            false,
            "~/test".to_owned(),
            title.map(ToOwned::to_owned),
            agent_rx,
            user_tx,
            &[],
            Arc::new(ToolRegistry::new(Vec::new())),
        );
        (app, user_rx, agent_tx)
    }

    // ── App::new ──

    #[test]
    fn new_plumbs_resumed_title_into_status_bar() {
        let (app, _rx, _agent_tx) = test_app(Some("Resumed title"));
        assert_eq!(app.status_bar.title(), Some("Resumed title"));
        assert_eq!(app.status_bar.status(), Status::Idle);
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

    // ── dispatch_user_action ──

    #[tokio::test]
    async fn dispatch_submit_prompt_updates_chat_status_and_forwards_action() {
        let (mut app, mut rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("hello".to_owned()));

        assert_eq!(app.chat.entry_count(), 1);
        assert!(!app.input.is_enabled(), "streaming disables input");
        assert_eq!(app.status_bar.status(), Status::Streaming);
        assert!(!app.should_quit);
        let forwarded = rx.recv().await.expect("forwarded action");
        assert!(matches!(forwarded, UserAction::SubmitPrompt(s) if s == "hello"));
    }

    #[test]
    fn dispatch_quit_sets_should_quit_and_leaves_chat_untouched() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::Quit);

        assert!(app.should_quit);
        assert_eq!(app.chat.entry_count(), 0);
        // Status bar stays idle — Quit flows past the streaming setup so
        // the tear-down path doesn't have to un-spinner it.
        assert_eq!(app.status_bar.status(), Status::Idle);
    }

    // ── handle_agent_event ──

    #[test]
    fn handle_session_title_updated_refreshes_status_bar() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::SessionTitleUpdated("Fix auth flow".to_owned()));
        assert_eq!(app.status_bar.title(), Some("Fix auth flow"));
        assert!(app.dirty);
    }

    #[test]
    fn handle_session_title_updated_replaces_existing_title() {
        // AI titles arrive after the first-prompt title is already shown;
        // the bar must accept the overwrite instead of ignoring the event.
        let (mut app, _rx, _agent_tx) = test_app(Some("First prompt"));
        app.handle_agent_event(AgentEvent::SessionTitleUpdated("AI-generated".to_owned()));
        assert_eq!(app.status_bar.title(), Some("AI-generated"));
    }

    #[test]
    fn handle_stream_token_switches_to_streaming_and_disables_input() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::StreamToken("partial".to_owned()));
        assert_eq!(app.status_bar.status(), Status::Streaming);
        assert!(!app.input.is_enabled());
    }

    #[test]
    fn handle_tool_call_start_switches_to_tool_running() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::ToolCallStart {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": "ls"}),
        });
        assert_eq!(app.status_bar.status(), Status::ToolRunning);
        assert_eq!(
            app.chat.entry_count(),
            1,
            "tool call renders one chat entry",
        );
    }

    #[test]
    fn handle_turn_complete_returns_to_idle_and_reenables_input() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        // Drive into streaming first so TurnComplete has state to tear down.
        app.dispatch_user_action(UserAction::SubmitPrompt("hi".to_owned()));
        assert_eq!(app.status_bar.status(), Status::Streaming);
        assert!(!app.input.is_enabled());

        app.handle_agent_event(AgentEvent::TurnComplete);
        assert_eq!(app.status_bar.status(), Status::Idle);
        assert!(app.input.is_enabled());
    }

    #[test]
    fn handle_error_pushes_error_entry_and_finishes_turn() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.dispatch_user_action(UserAction::SubmitPrompt("boom".to_owned()));
        app.handle_agent_event(AgentEvent::Error("API blew up".to_owned()));

        assert!(app.chat.last_is_error(), "error entry appended");
        assert_eq!(app.status_bar.status(), Status::Idle);
        assert!(app.input.is_enabled());
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
            title: Some("ls /".to_owned()),
            content: "file1\nfile2\n".to_owned(),
            is_error: false,
        });
        assert_eq!(app.chat.entry_count(), before + 1);
    }

    #[test]
    fn handle_tool_call_end_without_title_skips_result_entry() {
        // `title: None` signals the tool dispatch layer chose to hide the
        // result (e.g., for tools whose output is purely model-facing).
        // The chat entry count must not grow in that case.
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_agent_event(AgentEvent::ToolCallStart {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": "ls"}),
        });
        let before = app.chat.entry_count();
        app.handle_agent_event(AgentEvent::ToolCallEnd {
            id: "t1".to_owned(),
            title: None,
            content: "silent".to_owned(),
            is_error: false,
        });
        assert_eq!(app.chat.entry_count(), before);
    }

    // ── handle_crossterm_event ──

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

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
        assert_eq!(app.status_bar.status(), Status::Streaming);
        let forwarded = rx.recv().await.unwrap();
        assert!(matches!(forwarded, UserAction::SubmitPrompt(s) if s == "hi"));
    }

    #[test]
    fn handle_crossterm_key_ctrl_c_triggers_quit_from_any_mode() {
        let (mut app, _rx, _agent_tx) = test_app(None);
        app.handle_crossterm_event(&key_event(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
        assert!(app.dirty);
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

    // ── draw_frame ──

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn render_app(app: &mut App, width: u16, height: u16) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut chat_area = Rect::default();
        terminal
            .draw(|frame| {
                chat_area = app.draw_frame(frame);
            })
            .unwrap();
        app.chat.update_layout(chat_area);
        terminal.backend().clone()
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
}
