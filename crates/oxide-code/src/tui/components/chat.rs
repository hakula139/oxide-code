use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::component::{Action, Component};
use crate::tui::theme::Theme;

// ── Chat Message ──

/// A rendered chat message. Stores the role and content text for display.
/// Future PRs will add tool call blocks, markdown rendering, and collapsed
/// sections.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
}

// ── Chat View ──

/// Scrollable chat message list.
///
/// Renders messages vertically with role labels and auto-scrolls to the
/// bottom on new content. The user can scroll up to review history; new
/// content pauses auto-scroll until the user scrolls back to the bottom.
///
/// For PR 3.1 this renders plain text. PR 3.2 adds markdown rendering.
/// PR 3.6 adds viewport virtualization for long conversations.
pub struct ChatView {
    theme: Theme,
    messages: Vec<ChatMessage>,
    /// Text being streamed for the current assistant response. Appended to
    /// on each `AgentEvent::StreamToken`.
    streaming_buffer: String,
    scroll_offset: u16,
    /// Total content height from the last render (for scroll bounds).
    content_height: u16,
    /// Viewport height from the last render.
    viewport_height: u16,
    auto_scroll: bool,
}

impl ChatView {
    pub fn new() -> Self {
        Self {
            theme: Theme::default(),
            messages: Vec::new(),
            streaming_buffer: String::new(),
            scroll_offset: 0,
            content_height: 0,
            viewport_height: 0,
            auto_scroll: true,
        }
    }

    /// Append a user message to the chat history.
    pub fn push_user_message(&mut self, text: String) {
        self.messages.push(ChatMessage {
            role: ChatRole::User,
            content: text,
        });
        self.auto_scroll = true;
    }

    /// Append a streamed token to the current assistant response buffer.
    pub fn append_stream_token(&mut self, token: &str) {
        self.streaming_buffer.push_str(token);
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Finalize the current streaming buffer into a committed assistant message.
    pub fn commit_streaming(&mut self) {
        if !self.streaming_buffer.is_empty() {
            let content = std::mem::take(&mut self.streaming_buffer);
            self.messages.push(ChatMessage {
                role: ChatRole::Assistant,
                content,
            });
        }
    }

    /// Append a tool call summary to the chat.
    pub fn push_tool_call(&mut self, name: &str, title: Option<&str>) {
        let label = title.map_or_else(|| format!("⟡ {name}"), |t| format!("⟡ {name}: {t}"));
        self.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: label,
        });
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = self.content_height.saturating_sub(self.viewport_height);
    }

    fn scroll_up(&mut self, lines: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        self.auto_scroll = false;
    }

    fn scroll_down(&mut self, lines: u16) {
        let max = self.content_height.saturating_sub(self.viewport_height);
        self.scroll_offset = (self.scroll_offset + lines).min(max);
        if self.scroll_offset >= max {
            self.auto_scroll = true;
        }
    }

    /// Build the full text content for rendering.
    fn build_text(&self) -> Text<'_> {
        let mut lines: Vec<Line<'_>> = Vec::new();

        for msg in &self.messages {
            // Two blank lines between messages for visual breathing room.
            if !lines.is_empty() {
                lines.push(Line::raw(""));
                lines.push(Line::raw(""));
            }

            // Role label.
            let (label, style) = match msg.role {
                ChatRole::User => ("❯ You", self.theme.accent()),
                ChatRole::Assistant => ("⟡ Assistant", self.theme.secondary()),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(label, style),
            ]));
            lines.push(Line::raw(""));

            // Content lines.
            for line in msg.content.lines() {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(line, self.theme.text()),
                ]));
            }
        }

        // Streaming buffer (not yet committed).
        if !self.streaming_buffer.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::raw(""));
                lines.push(Line::raw(""));
            }
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("⟡ Assistant", self.theme.secondary()),
            ]));
            lines.push(Line::raw(""));
            for line in self.streaming_buffer.lines() {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(line, self.theme.text()),
                ]));
            }
        }

        Text::from(lines)
    }
}

impl Component for ChatView {
    fn handle_event(&mut self, event: &Event) -> Option<Action> {
        match event {
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            }) => {
                self.scroll_up(1);
                None
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            }) => {
                self.scroll_down(1);
                None
            }
            Event::Key(KeyEvent {
                code: KeyCode::PageUp,
                ..
            }) => {
                self.scroll_up(self.viewport_height.saturating_sub(2));
                None
            }
            Event::Key(KeyEvent {
                code: KeyCode::PageDown,
                ..
            }) => {
                self.scroll_down(self.viewport_height.saturating_sub(2));
                None
            }
            Event::Key(KeyEvent {
                code: KeyCode::Home,
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                self.scroll_offset = 0;
                self.auto_scroll = false;
                None
            }
            Event::Key(KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                self.scroll_to_bottom();
                self.auto_scroll = true;
                None
            }
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                ..
            }) => {
                self.scroll_up(3);
                None
            }
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                ..
            }) => {
                self.scroll_down(3);
                None
            }
            _ => None,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        self.render_inner(frame, area);
    }
}

impl ChatView {
    /// Separate method so we can mutate `content_height` / `viewport_height`
    /// which are rendering bookkeeping, not logical state. We use interior
    /// pattern: compute once, cache for scroll bounds.
    ///
    /// Note: `&self` here means we can't update the cached heights. The
    /// `App` layer calls a post-render update instead.
    fn render_inner(&self, frame: &mut Frame, area: Rect) {
        let text = self.build_text();
        let paragraph = Paragraph::new(text).scroll((self.scroll_offset, 0));
        frame.render_widget(paragraph, area);
    }

    /// Update cached layout dimensions after a render pass. Called by
    /// [`App`](super::super::app::App) after each frame.
    pub fn update_layout(&mut self, area: Rect) {
        self.viewport_height = area.height;
        // Approximate content height by building text. In PR 3.6 this will
        // use word-wrap-aware line counting.
        let text = self.build_text();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "line count fits in u16 for any realistic conversation"
        )]
        let height = text.lines.len() as u16;
        self.content_height = height;

        if self.auto_scroll {
            // Can't mutate self.scroll_offset here since we only have &mut
            // through update_layout. The caller handles this.
        }
    }

    /// Ensures scroll offset is at the bottom if auto-scroll is active.
    /// Call after `update_layout`.
    pub fn sync_scroll(&mut self) {
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }
}
