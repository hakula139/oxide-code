use std::cell::Cell;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
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
    ToolCall {
        icon: &'static str,
        label: String,
    },
    ToolResult {
        label: String,
        content: String,
        is_error: bool,
    },
}

// ── Chat View ──

/// Maximum lines of tool output shown inline before truncation.
const MAX_TOOL_OUTPUT_LINES: usize = 5;

/// Tab stop width for expanding `\t` in tool output. Ratatui renders each
/// character into fixed-width cells, so tabs must be expanded to spaces.
const TAB_WIDTH: usize = 4;

/// Scrollable chat message list with markdown rendering, tool call display,
/// and thinking block support.
///
/// Renders messages vertically with role labels and auto-scrolls to the
/// bottom on new content. The user can scroll up to review history; new
/// content pauses auto-scroll until the user scrolls back to the bottom.
pub(crate) struct ChatView {
    // Config
    theme: Theme,
    show_thinking: bool,

    // Persistent data
    entries: Vec<ChatEntry>,

    // Transient buffers (cleared per turn)
    /// Text being streamed for the current assistant response.
    streaming_buffer: String,
    /// Rendered lines for the stable prefix of the streaming buffer.
    /// Avoids re-parsing all committed text on every frame during
    /// streaming — only new complete lines since the last boundary
    /// are parsed and appended.
    streaming_rendered: Vec<Line<'static>>,
    /// Byte offset in `streaming_buffer` up to which `streaming_rendered`
    /// is current. Everything before this offset is already rendered and
    /// cached; only text from here to the next `\n` needs parsing.
    streaming_rendered_boundary: usize,
    /// Thinking tokens accumulated during extended thinking.
    thinking_buffer: String,

    // View state
    scroll_offset: u16,
    /// Total content height from the last render (for scroll bounds).
    /// Uses `Cell` for interior mutability so `render` (`&self`) can
    /// update it during the render pass without a second `build_text` call.
    content_height: Cell<u16>,
    viewport_height: u16,
    auto_scroll: bool,
}

impl ChatView {
    pub(crate) fn new(theme: Theme, show_thinking: bool) -> Self {
        Self {
            theme,
            show_thinking,
            entries: Vec::new(),
            streaming_buffer: String::new(),
            streaming_rendered: Vec::new(),
            streaming_rendered_boundary: 0,
            thinking_buffer: String::new(),
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
        self.advance_streaming_cache();
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
        self.streaming_rendered.clear();
        self.streaming_rendered_boundary = 0;
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

    /// Append a tool result summary line with optional output content.
    pub(crate) fn push_tool_result(&mut self, label: &str, content: &str, is_error: bool) {
        self.entries.push(ChatEntry::ToolResult {
            label: label.to_owned(),
            content: content.to_owned(),
            is_error,
        });
    }

    /// Append an error message.
    pub(crate) fn push_error(&mut self, msg: &str) {
        self.entries.push(ChatEntry::ToolResult {
            label: msg.to_owned(),
            content: String::new(),
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
        let text = self.build_text(area.width);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "line count fits in u16 for any realistic conversation"
        )]
        let height = text.lines.len() as u16;
        self.content_height.set(height);
        let paragraph = Paragraph::new(text).scroll((self.scroll_offset, 0));
        frame.render_widget(paragraph, area);
    }

    fn build_text(&self, width: u16) -> Text<'_> {
        let mut lines: Vec<Line<'_>> = Vec::new();

        if self.entries.is_empty()
            && self.streaming_buffer.is_empty()
            && self.thinking_buffer.is_empty()
        {
            self.push_welcome(&mut lines, width);
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
                ChatEntry::ToolResult {
                    label,
                    content,
                    is_error,
                } => {
                    self.push_tool_result_line(&mut lines, label, *is_error);
                    self.push_tool_output_lines(&mut lines, content, *is_error);
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

    fn push_welcome(&self, lines: &mut Vec<Line<'_>>, width: u16) {
        let w = usize::from(width);
        let title = "Welcome to ox";
        let subtitle = "Ask anything to begin.";
        let title_pad = w.saturating_sub(title.len()) / 2;
        let subtitle_pad = w.saturating_sub(subtitle.len()) / 2;

        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(title_pad)),
            Span::styled(title, self.theme.accent()),
        ]));
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(subtitle_pad)),
            Span::styled(subtitle, self.theme.dim()),
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
            lines.push(indent_markdown_line(line));
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
        let (indicator, indicator_style) = if is_error {
            ("✗", self.theme.error())
        } else {
            ("✓", self.theme.success())
        };
        lines.push(Line::from(vec![
            Span::styled("  ┃   ", self.tool_border_style(is_error)),
            Span::styled(indicator, indicator_style),
            Span::raw(" "),
            Span::styled(label, self.theme.muted()),
        ]));
    }

    fn push_tool_output_lines<'a>(
        &'a self,
        lines: &mut Vec<Line<'a>>,
        content: &'a str,
        is_error: bool,
    ) {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return;
        }

        let border_style = self.tool_border_style(is_error);
        let text_style = self.theme.dim();

        let output_lines: Vec<&str> = trimmed.lines().collect();
        let truncated = output_lines.len() > MAX_TOOL_OUTPUT_LINES;
        let visible = if truncated {
            &output_lines[..MAX_TOOL_OUTPUT_LINES]
        } else {
            &output_lines
        };

        for line in visible {
            lines.push(Line::from(vec![
                Span::styled("  ┃     ", border_style),
                Span::styled(expand_tabs(line), text_style),
            ]));
        }

        if truncated {
            lines.push(Line::from(vec![
                Span::styled("  ┃     ", border_style),
                Span::styled(
                    format!(
                        "… {} more lines",
                        output_lines.len() - MAX_TOOL_OUTPUT_LINES
                    ),
                    self.theme.dim(),
                ),
            ]));
        }
    }

    fn tool_border_style(&self, is_error: bool) -> Style {
        if is_error {
            self.theme.error()
        } else {
            self.theme.tool_border()
        }
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

        // Emit cached lines from the stable prefix (already rendered).
        for line in &self.streaming_rendered {
            lines.push(line.clone());
        }

        // Render only the new chunk beyond the cached boundary.
        let buf = &self.streaming_buffer;
        let tail = &buf[self.streaming_rendered_boundary..];

        if let Some(rel_boundary) = tail.rfind('\n') {
            let new_committed = &tail[..rel_boundary];
            let trailing = &tail[rel_boundary + 1..];

            if !new_committed.is_empty() {
                let rendered = render_markdown(new_committed);
                for line in rendered.lines {
                    lines.push(indent_markdown_line(line));
                }
            }

            if !trailing.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(trailing, self.theme.text()),
                ]));
            }
        } else if !tail.is_empty() {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(tail, self.theme.text()),
            ]));
        }
    }

    /// Advance the streaming cache: render newly committed lines and store
    /// them so subsequent frames skip re-parsing the stable prefix.
    fn advance_streaming_cache(&mut self) {
        let boundary = self.streaming_rendered_boundary;
        let tail = &self.streaming_buffer[boundary..];

        let Some(rel_boundary) = tail.rfind('\n') else {
            return;
        };

        let new_committed = &self.streaming_buffer[boundary..boundary + rel_boundary];
        if !new_committed.is_empty() {
            let rendered = render_markdown(new_committed);
            for line in rendered.lines {
                self.streaming_rendered.push(indent_markdown_line(line));
            }
        }

        self.streaming_rendered_boundary = boundary + rel_boundary + 1;
    }
}

// ── Markdown Indent ──

/// Prepend a 4-space indent to a markdown-rendered line.
fn indent_markdown_line(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("    ")];
    spans.extend(line.spans);
    Line::from(spans)
}

// ── Tab Expansion ──

/// Expand tab characters to spaces, aligning to [`TAB_WIDTH`]-column stops.
fn expand_tabs(s: &str) -> String {
    if !s.contains('\t') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 16);
    let mut col = 0;
    for ch in s.chars() {
        if ch == '\t' {
            let spaces = TAB_WIDTH - (col % TAB_WIDTH);
            for _ in 0..spaces {
                out.push(' ');
            }
            col += spaces;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

    use super::*;

    fn test_chat() -> ChatView {
        ChatView::new(Theme::default(), true)
    }

    /// Count lines produced by `build_text` at a default width.
    fn line_count(chat: &ChatView) -> usize {
        chat.build_text(80).lines.len()
    }

    /// Collect all raw text from `build_text` into a single string for
    /// substring assertions.
    fn all_text(chat: &ChatView) -> String {
        chat.build_text(80)
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl_key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
    }

    fn mouse_scroll(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }

    // ── append_stream_token ──

    #[test]
    fn append_stream_token_clears_thinking() {
        let mut chat = test_chat();
        chat.append_thinking_token("thinking...");
        assert!(!chat.thinking_buffer.is_empty());

        chat.append_stream_token("text");
        assert!(chat.thinking_buffer.is_empty());
    }

    // ── commit_streaming ──

    #[test]
    fn commit_streaming_moves_buffer_to_entry() {
        let mut chat = test_chat();
        chat.append_stream_token("hello world");
        assert!(chat.entries.is_empty());

        chat.commit_streaming();
        assert_eq!(chat.entries.len(), 1);
        assert!(matches!(&chat.entries[0], ChatEntry::Assistant(s) if s == "hello world"));
        assert!(chat.streaming_buffer.is_empty());
    }

    #[test]
    fn commit_streaming_empty_buffer_no_entry() {
        let mut chat = test_chat();
        chat.commit_streaming();
        assert!(chat.entries.is_empty());
    }

    #[test]
    fn commit_streaming_clears_cache() {
        let mut chat = test_chat();
        chat.streaming_buffer = "line1\nline2\npartial".to_owned();
        chat.advance_streaming_cache();
        assert!(!chat.streaming_rendered.is_empty());

        chat.commit_streaming();
        assert!(chat.streaming_rendered.is_empty());
        assert_eq!(chat.streaming_rendered_boundary, 0);
        assert!(chat.thinking_buffer.is_empty());
    }

    // ── update_layout ──

    #[test]
    fn update_layout_sets_viewport_height() {
        let mut chat = test_chat();
        chat.update_layout(Rect::new(0, 0, 80, 30));
        assert_eq!(chat.viewport_height, 30);
    }

    #[test]
    fn update_layout_auto_scrolls_when_enabled() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.auto_scroll = true;

        chat.update_layout(Rect::new(0, 0, 80, 20));
        assert_eq!(chat.scroll_offset, 80);
    }

    // ── handle_event ──

    #[test]
    fn handle_event_arrow_up_scrolls_up() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 10;

        let action = chat.handle_event(&key_event(KeyCode::Up));
        assert!(action.is_none());
        assert_eq!(chat.scroll_offset, 9);
        assert!(!chat.auto_scroll);
    }

    #[test]
    fn handle_event_arrow_down_scrolls_down() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 10;
        chat.auto_scroll = false;

        let action = chat.handle_event(&key_event(KeyCode::Down));
        assert!(action.is_none());
        assert_eq!(chat.scroll_offset, 11);
    }

    #[test]
    fn handle_event_mouse_scroll_up() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 10;

        let action = chat.handle_event(&mouse_scroll(MouseEventKind::ScrollUp));
        assert!(action.is_none());
        assert_eq!(chat.scroll_offset, 9);
    }

    #[test]
    fn handle_event_mouse_scroll_down() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 10;
        chat.auto_scroll = false;

        let action = chat.handle_event(&mouse_scroll(MouseEventKind::ScrollDown));
        assert!(action.is_none());
        assert_eq!(chat.scroll_offset, 11);
    }

    #[test]
    fn handle_event_page_up() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 30;

        chat.handle_event(&key_event(KeyCode::PageUp));
        assert_eq!(chat.scroll_offset, 12);
    }

    #[test]
    fn handle_event_page_down() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 30;
        chat.auto_scroll = false;

        chat.handle_event(&key_event(KeyCode::PageDown));
        assert_eq!(chat.scroll_offset, 48);
    }

    #[test]
    fn handle_event_ctrl_home_scrolls_to_top() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 50;

        chat.handle_event(&ctrl_key_event(KeyCode::Home));
        assert_eq!(chat.scroll_offset, 0);
        assert!(!chat.auto_scroll);
    }

    #[test]
    fn handle_event_ctrl_end_scrolls_to_bottom() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 10;
        chat.auto_scroll = false;

        chat.handle_event(&ctrl_key_event(KeyCode::End));
        assert_eq!(chat.scroll_offset, 80);
        assert!(chat.auto_scroll);
    }

    #[test]
    fn handle_event_unhandled_key_returns_none() {
        let mut chat = test_chat();
        let action = chat.handle_event(&key_event(KeyCode::Char('a')));
        assert!(action.is_none());
    }

    // ── render ──

    #[test]
    fn render_updates_content_height() {
        let chat = test_chat();

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                chat.render(frame, frame.area());
            })
            .unwrap();
        assert!(chat.content_height.get() > 0);
    }

    // ── scroll_to_bottom ──

    #[test]
    fn scroll_to_bottom_sets_offset_correctly() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;

        chat.scroll_to_bottom();
        assert_eq!(chat.scroll_offset, 80);
    }

    #[test]
    fn scroll_to_bottom_zero_when_content_fits() {
        let mut chat = test_chat();
        chat.content_height.set(10);
        chat.viewport_height = 20;

        chat.scroll_to_bottom();
        assert_eq!(chat.scroll_offset, 0);
    }

    // ── scroll_up ──

    #[test]
    fn scroll_up_decreases_offset_and_disables_auto_scroll() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 50;
        chat.auto_scroll = true;

        chat.scroll_up(5);
        assert_eq!(chat.scroll_offset, 45);
        assert!(!chat.auto_scroll);
    }

    #[test]
    fn scroll_up_saturates_at_zero() {
        let mut chat = test_chat();
        chat.scroll_offset = 3;

        chat.scroll_up(10);
        assert_eq!(chat.scroll_offset, 0);
    }

    // ── scroll_down ──

    #[test]
    fn scroll_down_increases_offset() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 50;
        chat.auto_scroll = false;

        chat.scroll_down(5);
        assert_eq!(chat.scroll_offset, 55);
        assert!(!chat.auto_scroll);
    }

    #[test]
    fn scroll_down_clamps_to_max_and_enables_auto_scroll() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 75;

        chat.scroll_down(10);
        assert_eq!(chat.scroll_offset, 80);
        assert!(chat.auto_scroll);
    }

    // ── build_text ──

    #[test]
    fn build_text_empty_shows_welcome() {
        let chat = test_chat();
        let text = all_text(&chat);
        assert!(text.contains("Welcome to ox"));
        assert!(text.contains("Ask anything to begin."));
    }

    #[test]
    fn build_text_full_conversation() {
        let mut chat = test_chat();
        chat.push_user_message("What is 2+2?".to_owned());
        chat.entries
            .push(ChatEntry::Assistant("The answer is 4.".to_owned()));
        chat.push_tool_call("$", "python -c 'print(2+2)'");
        chat.push_tool_result("4", "4", false);
        chat.push_user_message("Thanks!".to_owned());
        chat.append_stream_token("You're welcome");

        let text = all_text(&chat);
        assert!(text.contains("What is 2+2?"));
        assert!(text.contains("The answer is 4."));
        assert!(text.contains("python -c 'print(2+2)'"));
        assert!(text.contains("You're welcome"));
        assert_eq!(text.matches("❯ You").count(), 2);
    }

    // ── push_welcome ──

    #[test]
    fn push_welcome_centered_for_width() {
        let chat = test_chat();

        let narrow = chat.build_text(30);
        let wide = chat.build_text(120);

        let narrow_pad = narrow.lines[2].spans.first().map_or(0, |s| s.content.len());
        let wide_pad = wide.lines[2].spans.first().map_or(0, |s| s.content.len());
        assert!(wide_pad > narrow_pad);
    }

    // ── push_user_message_lines ──

    #[test]
    fn push_user_message_lines_has_label_and_content() {
        let mut chat = test_chat();
        chat.push_user_message("hello world".to_owned());
        let text = all_text(&chat);
        assert!(text.contains("❯ You"));
        assert!(text.contains("hello world"));
    }

    #[test]
    fn push_user_message_lines_multiline() {
        let mut chat = test_chat();
        chat.push_user_message("line1\nline2\nline3".to_owned());
        let text = all_text(&chat);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
        assert!(text.contains("line3"));
    }

    // ── push_assistant_message_lines ──

    #[test]
    fn push_assistant_message_lines_has_label() {
        let mut chat = test_chat();
        chat.entries
            .push(ChatEntry::Assistant("response".to_owned()));
        let text = all_text(&chat);
        assert!(text.contains("⟡ Assistant"));
        assert!(text.contains("response"));
    }

    // ── push_tool_call_line ──

    #[test]
    fn push_tool_call_line_shows_icon_and_label() {
        let mut chat = test_chat();
        chat.push_tool_call("$", "ls -la");
        let text = all_text(&chat);
        assert!(text.contains('$'));
        assert!(text.contains("ls -la"));
    }

    // ── push_tool_result_line / push_tool_output_lines ──

    #[test]
    fn push_tool_result_line_success() {
        let mut chat = test_chat();
        chat.push_tool_result("done", "output text", false);
        let text = all_text(&chat);
        assert!(text.contains("✓"));
        assert!(text.contains("done"));
        assert!(text.contains("output text"));
    }

    #[test]
    fn push_tool_result_line_error() {
        let mut chat = test_chat();
        chat.push_tool_result("failed", "error details", true);
        let text = all_text(&chat);
        assert!(text.contains("✗"));
        assert!(text.contains("failed"));
        assert!(text.contains("error details"));
    }

    #[test]
    fn push_tool_result_line_push_error() {
        let mut chat = test_chat();
        chat.push_error("something broke");
        let text = all_text(&chat);
        assert!(text.contains("✗"));
        assert!(text.contains("something broke"));
    }

    #[test]
    fn push_tool_output_lines_truncation() {
        let mut chat = test_chat();
        let long_output = (0..10).map(|i| format!("line {i}")).collect::<Vec<_>>();
        chat.push_tool_result("result", &long_output.join("\n"), false);
        let text = all_text(&chat);

        assert!(text.contains("line 0"));
        assert!(text.contains("line 4"));
        assert!(!text.contains("line 5"));
        assert!(text.contains("… 5 more lines"));
    }

    #[test]
    fn push_tool_output_lines_empty_content() {
        let mut chat = test_chat();
        chat.push_tool_result("result", "  \n  ", false);
        let before = line_count(&chat);

        let mut chat2 = test_chat();
        chat2.push_tool_result("result", "", false);
        let after = line_count(&chat2);

        assert_eq!(before, after);
    }

    #[test]
    fn push_tool_output_lines_exactly_max_no_truncation() {
        let mut chat = test_chat();
        let output: Vec<_> = (0..MAX_TOOL_OUTPUT_LINES)
            .map(|i| format!("line {i}"))
            .collect();
        chat.push_tool_result("result", &output.join("\n"), false);
        let text = all_text(&chat);
        assert!(!text.contains("more lines"));
    }

    #[test]
    fn push_tool_output_lines_one_over_max_shows_truncation() {
        let mut chat = test_chat();
        let output: Vec<_> = (0..=MAX_TOOL_OUTPUT_LINES)
            .map(|i| format!("line {i}"))
            .collect();
        chat.push_tool_result("result", &output.join("\n"), false);
        let text = all_text(&chat);
        assert!(text.contains("… 1 more lines"));
    }

    // ── push_thinking_lines ──

    #[test]
    fn push_thinking_lines_visible_when_enabled() {
        let mut chat = test_chat();
        chat.append_thinking_token("pondering...");
        let text = all_text(&chat);
        assert!(text.contains("Thinking…"));
        assert!(text.contains("pondering..."));
    }

    #[test]
    fn push_thinking_lines_hidden_when_disabled() {
        let mut chat = ChatView::new(Theme::default(), false);
        chat.append_thinking_token("pondering...");
        let text = all_text(&chat);
        assert!(!text.contains("Thinking…"));
        assert!(!text.contains("pondering..."));
    }

    #[test]
    fn push_thinking_lines_after_entries_has_separator() {
        let mut chat = test_chat();
        chat.push_user_message("hello".to_owned());
        chat.append_thinking_token("deep thought");
        let lines_before_thinking = {
            let mut c = test_chat();
            c.push_user_message("hello".to_owned());
            line_count(&c)
        };
        assert!(line_count(&chat) > lines_before_thinking + 1);
    }

    // ── push_streaming_lines ──

    #[test]
    fn push_streaming_lines_shows_partial_text() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.append_stream_token("partial response");
        let text = all_text(&chat);
        assert!(text.contains("⟡ Assistant"));
        assert!(text.contains("partial response"));
    }

    #[test]
    fn push_streaming_lines_cached_and_tail() {
        let mut chat = test_chat();
        chat.streaming_buffer = "cached line\ntail text".to_owned();
        chat.advance_streaming_cache();

        let text = all_text(&chat);
        assert!(text.contains("cached line"));
        assert!(text.contains("tail text"));
    }

    #[test]
    fn push_streaming_lines_uncommitted_newlines() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.streaming_buffer = "line1\nline2\npartial".to_owned();

        let text = all_text(&chat);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
        assert!(text.contains("partial"));
    }

    #[test]
    fn push_streaming_lines_without_prior_assistant_shows_header() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.streaming_buffer = "response".to_owned();

        let text = all_text(&chat);
        assert!(text.contains("⟡ Assistant"));
    }

    #[test]
    fn push_streaming_lines_after_assistant_omits_duplicate_header() {
        let mut chat = test_chat();
        chat.entries
            .push(ChatEntry::Assistant("committed".to_owned()));
        chat.streaming_buffer = "streaming".to_owned();

        let text = all_text(&chat);
        let count = text.matches("⟡ Assistant").count();
        assert_eq!(count, 1, "header should appear once, not duplicated");
    }

    #[test]
    fn push_streaming_lines_uncommitted_newline_with_empty_trailing() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.streaming_buffer = "line1\nline2\n".to_owned();

        let text = all_text(&chat);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
    }

    #[test]
    fn push_streaming_lines_empty_committed_with_trailing() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.streaming_buffer = "cached\n\ntrailing".to_owned();
        chat.streaming_rendered_boundary = 7;

        let text = all_text(&chat);
        assert!(text.contains("trailing"));
    }

    // ── advance_streaming_cache ──

    #[test]
    fn advance_streaming_cache_no_newline_stays_at_zero() {
        let mut chat = test_chat();
        chat.streaming_buffer = "no newline here".to_owned();
        chat.advance_streaming_cache();
        assert_eq!(chat.streaming_rendered_boundary, 0);
        assert!(chat.streaming_rendered.is_empty());
    }

    #[test]
    fn advance_streaming_cache_single_newline() {
        let mut chat = test_chat();
        chat.streaming_buffer = "first line\nincomplete".to_owned();
        chat.advance_streaming_cache();
        assert_eq!(chat.streaming_rendered_boundary, "first line\n".len());
        assert!(!chat.streaming_rendered.is_empty());
    }

    #[test]
    fn advance_streaming_cache_multiple_newlines() {
        let mut chat = test_chat();
        chat.streaming_buffer = "line1\nline2\nline3\npartial".to_owned();
        chat.advance_streaming_cache();
        assert_eq!(
            chat.streaming_rendered_boundary,
            "line1\nline2\nline3\n".len()
        );
    }

    #[test]
    fn advance_streaming_cache_incremental() {
        let mut chat = test_chat();

        chat.streaming_buffer = "first\n".to_owned();
        chat.advance_streaming_cache();
        let boundary1 = chat.streaming_rendered_boundary;
        let cached1 = chat.streaming_rendered.len();

        chat.streaming_buffer.push_str("second\n");
        chat.advance_streaming_cache();
        assert!(chat.streaming_rendered_boundary > boundary1);
        assert!(chat.streaming_rendered.len() >= cached1);
    }

    #[test]
    fn advance_streaming_cache_trailing_newline_only() {
        let mut chat = test_chat();
        chat.streaming_buffer = "\n".to_owned();
        chat.advance_streaming_cache();
        assert_eq!(chat.streaming_rendered_boundary, 1);
    }

    // ── expand_tabs ──

    #[test]
    fn expand_tabs_no_tabs_unchanged() {
        assert_eq!(expand_tabs("hello world"), "hello world");
    }

    #[test]
    fn expand_tabs_line_number_format() {
        assert_eq!(expand_tabs("1\tuse std::io;"), "1   use std::io;");
        assert_eq!(expand_tabs("10\tuse std::io;"), "10  use std::io;");
        assert_eq!(expand_tabs("100\tuse std::io;"), "100 use std::io;");
    }

    #[test]
    fn expand_tabs_mid_line_aligns_to_stop() {
        assert_eq!(expand_tabs("ab\tcd"), "ab  cd");
        assert_eq!(expand_tabs("abcd\tx"), "abcd    x");
    }
}
