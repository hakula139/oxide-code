use std::cell::Cell;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::tui::component::{Action, Component};
use crate::tui::markdown::render_markdown;
use crate::tui::theme::Theme;

// ── Chat Entry ──

/// A single entry in the chat history.
#[derive(Debug, Clone)]
enum ChatEntry {
    User(String),
    Assistant(String),
    ToolCall { icon: &'static str, label: String },
    ToolResult { label: String, is_error: bool },
}

// ── Chat View ──

/// Scrollable chat message list with markdown rendering, tool call display,
/// and thinking block support.
///
/// Renders messages vertically with role labels and auto-scrolls to the
/// bottom on new content. The user can scroll up to review history; new
/// content pauses auto-scroll until the user scrolls back to the bottom.
pub(crate) struct ChatView {
    theme: Theme,
    entries: Vec<ChatEntry>,
    /// Text being streamed for the current assistant response.
    streaming_buffer: String,
    /// Thinking tokens accumulated during extended thinking.
    thinking_buffer: String,
    show_thinking: bool,
    scroll_offset: u16,
    /// Total content height from the last render (for scroll bounds).
    /// Uses `Cell` for interior mutability so `render` (`&self`) can
    /// update it during the render pass without a second `build_text` call.
    content_height: Cell<u16>,
    /// Viewport height from the last render.
    viewport_height: u16,
    auto_scroll: bool,
}

impl ChatView {
    pub(crate) fn new(theme: Theme, show_thinking: bool) -> Self {
        Self {
            theme,
            entries: Vec::new(),
            streaming_buffer: String::new(),
            thinking_buffer: String::new(),
            show_thinking,
            scroll_offset: 0,
            content_height: Cell::new(0),
            viewport_height: 0,
            auto_scroll: true,
        }
    }

    /// Append a user message to the chat history.
    pub(crate) fn push_user_message(&mut self, text: String) {
        self.entries.push(ChatEntry::User(text));
        self.auto_scroll = true;
    }

    /// Append a streamed token to the current assistant response buffer.
    pub(crate) fn append_stream_token(&mut self, token: &str) {
        if !self.thinking_buffer.is_empty() {
            self.thinking_buffer.clear();
        }
        self.streaming_buffer.push_str(token);
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Append a thinking token to the thinking display buffer.
    pub(crate) fn append_thinking_token(&mut self, token: &str) {
        self.thinking_buffer.push_str(token);
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Finalize the current streaming buffer into a committed assistant message.
    pub(crate) fn commit_streaming(&mut self) {
        self.thinking_buffer.clear();
        if !self.streaming_buffer.is_empty() {
            let content = std::mem::take(&mut self.streaming_buffer);
            self.entries.push(ChatEntry::Assistant(content));
        }
    }

    /// Append a tool call entry with its icon and label.
    pub(crate) fn push_tool_call(&mut self, icon: &'static str, label: &str) {
        self.entries.push(ChatEntry::ToolCall {
            icon,
            label: label.to_owned(),
        });
    }

    /// Append a tool result summary line.
    pub(crate) fn push_tool_result(&mut self, label: &str, is_error: bool) {
        self.entries.push(ChatEntry::ToolResult {
            label: label.to_owned(),
            is_error,
        });
    }

    /// Append an error message.
    pub(crate) fn push_error(&mut self, msg: &str) {
        self.entries.push(ChatEntry::ToolResult {
            label: msg.to_owned(),
            is_error: true,
        });
    }

    /// Update cached viewport height and sync scroll position. Called by
    /// [`App`](super::super::app::App) after each frame.
    pub(crate) fn update_layout(&mut self, area: Rect) {
        self.viewport_height = area.height;
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }
}

impl Component for ChatView {
    fn handle_event(&mut self, event: &Event) -> Option<Action> {
        match event {
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            })
            | Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                ..
            }) => {
                self.scroll_up(1);
                None
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            })
            | Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
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
            _ => None,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        self.render_inner(frame, area);
    }
}

// ── Private Helpers ──

impl ChatView {
    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = self
            .content_height
            .get()
            .saturating_sub(self.viewport_height);
    }

    fn scroll_up(&mut self, lines: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        self.auto_scroll = false;
    }

    fn scroll_down(&mut self, lines: u16) {
        let max = self
            .content_height
            .get()
            .saturating_sub(self.viewport_height);
        self.scroll_offset = self.scroll_offset.saturating_add(lines).min(max);
        if self.scroll_offset >= max {
            self.auto_scroll = true;
        }
    }

    fn render_inner(&self, frame: &mut Frame, area: Rect) {
        let text = self.build_text();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "line count fits in u16 for any realistic conversation"
        )]
        let height = text.lines.len() as u16;
        self.content_height.set(height);
        let paragraph = Paragraph::new(text).scroll((self.scroll_offset, 0));
        frame.render_widget(paragraph, area);
    }

    fn build_text(&self) -> Text<'_> {
        let mut lines: Vec<Line<'_>> = Vec::new();

        if self.entries.is_empty()
            && self.streaming_buffer.is_empty()
            && self.thinking_buffer.is_empty()
        {
            self.push_welcome(&mut lines);
            return Text::from(lines);
        }

        for entry in &self.entries {
            match entry {
                ChatEntry::User(content) => {
                    self.push_user_message_lines(&mut lines, content);
                }
                ChatEntry::Assistant(content) => {
                    self.push_assistant_message_lines(&mut lines, content);
                }
                ChatEntry::ToolCall { icon, label } => {
                    self.push_tool_call_line(&mut lines, icon, label);
                }
                ChatEntry::ToolResult { label, is_error } => {
                    self.push_tool_result_line(&mut lines, label, *is_error);
                }
            }
        }

        // Thinking buffer (ephemeral — not stored in history).
        if self.show_thinking && !self.thinking_buffer.is_empty() {
            self.push_thinking_lines(&mut lines);
        }

        // Streaming buffer (not yet committed).
        if !self.streaming_buffer.is_empty() {
            self.push_streaming_lines(&mut lines);
        }

        Text::from(lines)
    }

    // ── Welcome ──

    fn push_welcome(&self, lines: &mut Vec<Line<'_>>) {
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::raw("          "),
            Span::styled("Welcome to ox", self.theme.accent()),
        ]));
        lines.push(Line::from(vec![
            Span::raw("       "),
            Span::styled("Ask anything to begin.", self.theme.dim()),
        ]));
    }

    // ── User Messages ──

    fn push_user_message_lines<'a>(&'a self, lines: &mut Vec<Line<'a>>, content: &'a str) {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("❯ You", self.theme.accent()),
        ]));
        for line in content.trim().lines() {
            lines.push(Line::from(vec![
                Span::styled("  ┃ ", self.theme.tool_border()),
                Span::styled(line, self.theme.text()),
            ]));
        }
    }

    // ── Assistant Messages ──

    fn push_assistant_message_lines<'a>(&'a self, lines: &mut Vec<Line<'a>>, content: &'a str) {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("⟡ Assistant", self.theme.secondary()),
        ]));

        let rendered = render_markdown(content);
        for line in rendered.lines {
            let mut spans = vec![Span::raw("    ")];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }

    // ── Tool Calls ──

    fn push_tool_call_line<'a>(&'a self, lines: &mut Vec<Line<'a>>, icon: &'a str, label: &'a str) {
        lines.push(Line::from(vec![
            Span::styled("  ┃ ", self.theme.tool_border()),
            Span::styled(icon, self.theme.tool_icon()),
            Span::raw(" "),
            Span::styled(label, self.theme.text()),
        ]));
    }

    fn push_tool_result_line<'a>(
        &'a self,
        lines: &mut Vec<Line<'a>>,
        label: &'a str,
        is_error: bool,
    ) {
        let (indicator, style) = if is_error {
            ("✗", self.theme.error())
        } else {
            ("✓", self.theme.success())
        };
        let border_style = if is_error {
            self.theme.error()
        } else {
            self.theme.tool_border()
        };
        lines.push(Line::from(vec![
            Span::styled("  ┃   ", border_style),
            Span::styled(indicator, style),
            Span::raw(" "),
            Span::styled(label, self.theme.muted()),
        ]));
    }

    // ── Thinking ──

    fn push_thinking_lines<'a>(&'a self, lines: &mut Vec<Line<'a>>) {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("Thinking…", self.theme.thinking()),
        ]));
        for line in self.thinking_buffer.lines() {
            lines.push(Line::from(vec![
                Span::styled("  ┃ ", self.theme.dim()),
                Span::styled(line, self.theme.thinking()),
            ]));
        }
    }

    // ── Streaming ──

    fn push_streaming_lines<'a>(&'a self, lines: &mut Vec<Line<'a>>) {
        if !lines.is_empty()
            && !self
                .entries
                .last()
                .is_some_and(|e| matches!(e, ChatEntry::Assistant(_)))
        {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("⟡ Assistant", self.theme.secondary()),
            ]));
        }

        // Split at the last newline: committed lines get markdown, trailing
        // partial line gets plain text.
        let buf = &self.streaming_buffer;
        if let Some(boundary) = buf.rfind('\n') {
            let committed = &buf[..boundary];
            let trailing = &buf[boundary + 1..];

            let rendered = render_markdown(committed);
            for line in rendered.lines {
                let mut spans = vec![Span::raw("    ")];
                spans.extend(line.spans);
                lines.push(Line::from(spans));
            }

            if !trailing.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(trailing, self.theme.text()),
                ]));
            }
        } else {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(buf.as_str(), self.theme.text()),
            ]));
        }
    }
}
