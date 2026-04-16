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
use crate::tui::wrap::{expand_tabs, wrap_line_styled};

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
    Error(String),
}

// ── Chat View ──

/// Maximum lines of tool output shown inline before truncation.
const MAX_TOOL_OUTPUT_LINES: usize = 5;

/// Maximum characters per tool output line before horizontal truncation.
const MAX_TOOL_OUTPUT_LINE_CHARS: usize = 512;

/// Left bar character for bordered content.
const BAR: &str = "▎";

/// Border prefix for continuation lines and non-first content lines.
const BORDER_PREFIX: &str = "  ▎ ";

/// Icon prefix for the first line of user messages.
const USER_PREFIX: &str = "❯ ▎ ";

/// Icon prefix for the first line of assistant messages.
const ASSISTANT_PREFIX: &str = "⟡ ▎ ";

/// Border prefix for tool result status lines (indicator + label).
const TOOL_RESULT_PREFIX: &str = "  ▎   ";

/// Border prefix for tool output body lines.
const TOOL_OUTPUT_PREFIX: &str = "  ▎     ";

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
    viewport_width: u16,
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
            viewport_width: 0,
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
        self.entries.push(ChatEntry::Error(msg.to_owned()));
    }

    /// Update cached viewport height and sync scroll position. Called by
    /// [`App`](super::super::app::App) after each frame.
    pub(crate) fn update_layout(&mut self, area: Rect) {
        self.viewport_height = area.height;
        self.viewport_width = area.width;
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
            reason = "clamped to u16::MAX; truncation cannot occur"
        )]
        let height = text.lines.len().min(u16::MAX as usize) as u16;
        self.content_height.set(height);
        let paragraph = Paragraph::new(text).scroll((self.scroll_offset, 0));
        frame.render_widget(paragraph, area);
    }

    fn build_text(&self, width: u16) -> Text<'_> {
        let width = usize::from(width);
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
                    self.push_user_message_lines(&mut lines, content, width);
                    lines.push(Line::raw(""));
                }
                ChatEntry::Assistant(content) => {
                    self.push_assistant_message_lines(&mut lines, content, width);
                    lines.push(Line::raw(""));
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
                    self.push_tool_output_lines(&mut lines, content, *is_error, width);
                }
                ChatEntry::Error(msg) => {
                    self.push_tool_result_line(&mut lines, msg, true);
                }
            }
        }

        // Thinking buffer (ephemeral — not stored in history).
        if self.show_thinking && !self.thinking_buffer.is_empty() {
            self.push_thinking_lines(&mut lines, width);
        }

        // Streaming buffer (not yet committed).
        if !self.streaming_buffer.is_empty() {
            self.push_streaming_lines(&mut lines, width);
        }

        Text::from(lines)
    }

    // ── Welcome ──

    fn push_welcome(&self, lines: &mut Vec<Line<'_>>, width: usize) {
        let title = "Welcome to ox";
        let subtitle = "Ask anything to begin.";
        let title_pad = width.saturating_sub(title.len()) / 2;
        let subtitle_pad = width.saturating_sub(subtitle.len()) / 2;

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

    fn push_user_message_lines<'a>(
        &'a self,
        lines: &mut Vec<Line<'a>>,
        content: &'a str,
        width: usize,
    ) {
        push_section_gap(lines);
        push_bordered_lines(
            lines,
            content.trim(),
            USER_PREFIX,
            self.theme.user(),
            self.theme.text(),
            width,
        );
    }

    // ── Assistant Messages ──

    fn push_assistant_message_lines<'a>(
        &'a self,
        lines: &mut Vec<Line<'a>>,
        content: &'a str,
        width: usize,
    ) {
        push_section_gap(lines);

        // The markdown renderer wraps to (width - 4) so the 4-char
        // border prefix doesn't push content past the terminal edge.
        let bar_style = self.theme.secondary();
        let md_width = width.saturating_sub(BORDER_PREFIX.len());
        let rendered = render_markdown(content, &self.theme, md_width);
        let mut first = true;
        for line in rendered.lines {
            let prefix = if first {
                ASSISTANT_PREFIX
            } else {
                BORDER_PREFIX
            };
            first = false;
            lines.push(border_markdown_line(line, prefix, bar_style));
        }
    }

    // ── Tool Calls ──

    fn push_tool_call_line<'a>(&'a self, lines: &mut Vec<Line<'a>>, icon: &'a str, label: &'a str) {
        lines.push(Line::from(vec![
            Span::styled(BORDER_PREFIX, self.theme.tool_border()),
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
            Span::styled(TOOL_RESULT_PREFIX, self.tool_border_style(is_error)),
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
        width: usize,
    ) {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return;
        }

        let border_style = self.tool_border_style(is_error);
        let text_style = self.theme.dim();
        let cont_prefix = border_continuation_prefix(TOOL_OUTPUT_PREFIX, border_style);

        let output_lines: Vec<&str> = trimmed.lines().collect();
        let truncated = output_lines.len() > MAX_TOOL_OUTPUT_LINES;
        let visible = if truncated {
            &output_lines[..MAX_TOOL_OUTPUT_LINES]
        } else {
            &output_lines
        };

        for text_line in visible {
            let expanded = expand_tabs(text_line);
            let display_text = truncate_line(&expanded, MAX_TOOL_OUTPUT_LINE_CHARS);
            let line = Line::from(vec![
                Span::styled(TOOL_OUTPUT_PREFIX, border_style),
                Span::styled(display_text, text_style),
            ]);
            for wrapped in
                wrap_line_styled(line, width, TOOL_OUTPUT_PREFIX.len(), Some(&cont_prefix))
            {
                lines.push(wrapped);
            }
        }

        if truncated {
            let n = output_lines.len() - MAX_TOOL_OUTPUT_LINES;
            let label = if n == 1 { "line" } else { "lines" };
            lines.push(Line::from(vec![
                Span::styled(TOOL_OUTPUT_PREFIX, border_style),
                Span::styled(format!("... {n} more {label}"), self.theme.dim()),
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

    fn push_thinking_lines<'a>(&'a self, lines: &mut Vec<Line<'a>>, width: usize) {
        push_section_header(lines, "Thinking...", self.theme.thinking());
        push_bordered_lines(
            lines,
            &self.thinking_buffer,
            BORDER_PREFIX,
            self.theme.dim(),
            self.theme.thinking(),
            width,
        );
    }

    // ── Streaming ──

    fn push_streaming_lines<'a>(&'a self, lines: &mut Vec<Line<'a>>, width: usize) {
        let bar_style = self.theme.secondary();

        // Show icon + separator only when this is a new assistant turn
        // (not a continuation of a committed assistant entry).
        let is_new_turn = !self
            .entries
            .last()
            .is_some_and(|e| matches!(e, ChatEntry::Assistant(_)));
        if is_new_turn && !lines.is_empty() {
            lines.push(Line::raw(""));
        }

        // Emit cached lines from the stable prefix (already rendered).
        // The first cached line already carries the icon from advance_streaming_cache.
        for line in &self.streaming_rendered {
            lines.push(line.clone());
        }

        // Render only the new chunk beyond the cached boundary.
        let buf = &self.streaming_buffer;
        let tail = &buf[self.streaming_rendered_boundary..];
        let md_width = width.saturating_sub(BORDER_PREFIX.len());

        // Determine whether the next rendered line is the very first
        // line of this assistant turn (needs icon prefix).
        let needs_icon = is_new_turn && self.streaming_rendered.is_empty();

        if let Some(rel_boundary) = tail.rfind('\n') {
            let new_committed = &tail[..rel_boundary];
            let trailing = &tail[rel_boundary + 1..];

            if !new_committed.is_empty() {
                let rendered = render_markdown(new_committed, &self.theme, md_width);
                let mut first = needs_icon;
                for line in rendered.lines {
                    let prefix = if first {
                        ASSISTANT_PREFIX
                    } else {
                        BORDER_PREFIX
                    };
                    first = false;
                    lines.push(border_markdown_line(line, prefix, bar_style));
                }
            }

            if !trailing.is_empty() {
                let prefix = if needs_icon && new_committed.is_empty() {
                    ASSISTANT_PREFIX
                } else {
                    BORDER_PREFIX
                };
                lines.push(border_markdown_line(
                    Line::from(Span::styled(trailing.to_owned(), self.theme.text())),
                    prefix,
                    bar_style,
                ));
            }
        } else if !tail.is_empty() {
            let prefix = if needs_icon {
                ASSISTANT_PREFIX
            } else {
                BORDER_PREFIX
            };
            lines.push(border_markdown_line(
                Line::from(Span::styled(tail.to_owned(), self.theme.text())),
                prefix,
                bar_style,
            ));
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
            let bar_style = self.theme.secondary();
            let md_width = usize::from(self.viewport_width).saturating_sub(BORDER_PREFIX.len());
            let rendered = render_markdown(new_committed, &self.theme, md_width);

            // The first cached line of a new assistant turn gets the icon prefix.
            let is_new_turn = !self
                .entries
                .last()
                .is_some_and(|e| matches!(e, ChatEntry::Assistant(_)));
            let mut first = is_new_turn && self.streaming_rendered.is_empty();

            for line in rendered.lines {
                let prefix = if first {
                    ASSISTANT_PREFIX
                } else {
                    BORDER_PREFIX
                };
                first = false;
                self.streaming_rendered
                    .push(border_markdown_line(line, prefix, bar_style));
            }
        }

        self.streaming_rendered_boundary = boundary + rel_boundary + 1;
    }
}

// ── Free Helpers ──

/// Push a blank line separator unless `lines` is empty or already ends
/// with a blank line.
fn push_section_gap(lines: &mut Vec<Line<'_>>) {
    let needs_gap = lines.last().is_some_and(|last| last.width() > 0);
    if needs_gap {
        lines.push(Line::raw(""));
    }
}

/// Push a blank separator (when lines exist) and a styled section label.
fn push_section_header<'a>(lines: &mut Vec<Line<'a>>, label: &'a str, style: Style) {
    push_section_gap(lines);
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(label, style),
    ]));
}

/// Emit bordered lines: the first content line uses `first_prefix` (with
/// an icon like `"❯ ▎ "`), subsequent lines use [`BORDER_PREFIX`]. All
/// lines are wrapped with a styled continuation prefix preserving the bar.
fn push_bordered_lines(
    lines: &mut Vec<Line<'_>>,
    content: &str,
    first_prefix: &str,
    bar_style: Style,
    text_style: Style,
    width: usize,
) {
    let cont_prefix = border_continuation_prefix(BORDER_PREFIX, bar_style);
    let mut is_first = true;
    for text_line in content.lines() {
        let prefix = if is_first {
            first_prefix
        } else {
            BORDER_PREFIX
        };
        is_first = false;
        let line = Line::from(vec![
            Span::styled(prefix.to_owned(), bar_style),
            Span::styled(text_line.to_owned(), text_style),
        ]);
        for wrapped in wrap_line_styled(line, width, BORDER_PREFIX.len(), Some(&cont_prefix)) {
            lines.push(wrapped);
        }
    }
}

/// Build a continuation prefix that keeps the `▎` bar aligned under the
/// original prefix. For a prefix like `"  ▎ "` (4 cols), produces spans
/// `["  ", "▎", " "]` where the bar span is styled.
fn border_continuation_prefix(prefix: &str, bar_style: Style) -> Vec<Span<'static>> {
    // Split at the bar character to determine left padding and right padding.
    if let Some(bar_pos) = prefix.find(BAR) {
        let left = &prefix[..bar_pos];
        let right = &prefix[bar_pos + BAR.len()..];
        vec![
            Span::raw(left.to_owned()),
            Span::styled(BAR, bar_style),
            Span::raw(right.to_owned()),
        ]
    } else {
        vec![Span::raw(" ".repeat(prefix.len()))]
    }
}

/// Prepend a styled border prefix to a markdown-rendered line.
fn border_markdown_line(line: Line<'static>, prefix: &str, bar_style: Style) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix.to_owned(), bar_style)];
    spans.extend(line.spans);
    Line::from(spans)
}

/// Truncate a string to `max_chars` characters, appending `...` if cut.
fn truncate_line(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_owned();
    }
    // Find a char boundary at or before max_chars.
    let boundary = s.floor_char_boundary(max_chars);
    format!("{}...", &s[..boundary])
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use indoc::indoc;

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
        // Welcome screen: 2 blank lines + title + subtitle = 4 lines.
        assert_eq!(chat.content_height.get(), 4);
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
        // Two user messages → two user icon prefixes.
        assert_eq!(text.matches('❯').count(), 2);
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
    fn push_user_message_lines_has_icon_and_content() {
        let mut chat = test_chat();
        chat.push_user_message("hello world".to_owned());
        let text = all_text(&chat);
        assert!(text.contains('❯'));
        assert!(text.contains("hello world"));
    }

    #[test]
    fn push_user_message_lines_has_trailing_blank() {
        let mut chat = test_chat();
        chat.push_user_message("hello".to_owned());
        chat.push_tool_call("$", "ls");
        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        let user = lines.iter().rposition(|l| l.contains("hello")).unwrap();
        let tool = lines.iter().position(|l| l.contains("ls")).unwrap();
        assert!(
            (user + 1..tool).any(|i| lines[i].trim().is_empty()),
            "expected blank line after user message"
        );
    }

    #[test]
    fn push_user_message_no_double_blank_before_assistant() {
        let mut chat = test_chat();
        chat.push_user_message("hello".to_owned());
        chat.entries.push(ChatEntry::Assistant("reply".to_owned()));
        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        // Count consecutive blank lines — should never exceed 1.
        let max_consecutive_blanks = lines
            .windows(2)
            .filter(|w| w[0].trim().is_empty() && w[1].trim().is_empty())
            .count();
        assert_eq!(
            max_consecutive_blanks, 0,
            "no double blank lines between user and assistant: {lines:?}"
        );
    }

    #[test]
    fn push_user_message_enables_auto_scroll() {
        let mut chat = test_chat();
        chat.auto_scroll = false;
        chat.push_user_message("hello".to_owned());
        assert!(chat.auto_scroll);
    }

    #[test]
    fn push_user_message_lines_multiline() {
        let mut chat = test_chat();
        chat.push_user_message(
            indoc! {"
                line1
                line2
                line3
            "}
            .to_owned(),
        );
        let text = all_text(&chat);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
        assert!(text.contains("line3"));
    }

    // ── push_assistant_message_lines ──

    #[test]
    fn push_assistant_message_lines_has_icon_and_content() {
        let mut chat = test_chat();
        chat.entries
            .push(ChatEntry::Assistant("response".to_owned()));
        let text = all_text(&chat);
        assert!(text.contains('⟡'));
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

    #[test]
    fn push_tool_call_line_after_assistant_has_blank_separator() {
        let mut chat = test_chat();
        chat.entries
            .push(ChatEntry::Assistant("some text".to_owned()));
        chat.push_tool_call("$", "ls");
        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        let assistant = lines.iter().rposition(|l| l.contains("some text")).unwrap();
        let tool = lines.iter().position(|l| l.contains("ls")).unwrap();
        assert!(
            (assistant + 1..tool).any(|i| lines[i].trim().is_empty()),
            "expected blank separator between assistant text and tool call"
        );
    }

    #[test]
    fn push_tool_call_line_consecutive_no_extra_gap() {
        let mut chat = test_chat();
        chat.push_tool_call("$", "ls");
        chat.push_tool_call("$", "cat foo");
        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        let ls_line = lines.iter().position(|l| l.contains("ls")).unwrap();
        let cat_line = lines.iter().position(|l| l.contains("cat foo")).unwrap();
        assert_eq!(
            cat_line,
            ls_line + 1,
            "consecutive tool calls should have no blank gap"
        );
    }

    // ── push_tool_result_line ──

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

    // ── push_error ──

    #[test]
    fn push_error_shows_error_indicator() {
        let mut chat = test_chat();
        chat.push_error("something broke");
        let text = all_text(&chat);
        assert!(text.contains("✗"));
        assert!(text.contains("something broke"));
    }

    // ── push_tool_output_lines ──

    #[test]
    fn push_tool_output_lines_truncation() {
        let mut chat = test_chat();
        let long_output = (0..10).map(|i| format!("line {i}")).collect::<Vec<_>>();
        chat.push_tool_result("result", &long_output.join("\n"), false);
        let text = all_text(&chat);

        assert!(text.contains("line 0"));
        assert!(text.contains("line 4"));
        assert!(!text.contains("line 5"));
        assert!(text.contains("... 5 more lines"));
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
        assert!(text.contains("... 1 more line"));
        assert!(!text.contains("lines"), "singular 'line' expected: {text}");
    }

    #[test]
    fn push_tool_output_lines_long_line_truncated() {
        let mut chat = test_chat();
        let long_line = "x".repeat(MAX_TOOL_OUTPUT_LINE_CHARS + 100);
        chat.push_tool_result("result", &long_line, false);
        let text = all_text(&chat);
        assert!(
            text.contains("..."),
            "long line should be truncated with ..."
        );
        assert!(
            !text.contains(&long_line),
            "full long line should not appear"
        );
    }

    // ── push_thinking_lines ──

    #[test]
    fn push_thinking_lines_visible_when_enabled() {
        let mut chat = test_chat();
        chat.append_thinking_token("pondering...");
        let text = all_text(&chat);
        assert!(text.contains("Thinking..."));
        assert!(text.contains("pondering..."));
    }

    #[test]
    fn push_thinking_lines_hidden_when_disabled() {
        let mut chat = ChatView::new(Theme::default(), false);
        chat.append_thinking_token("pondering...");
        let text = all_text(&chat);
        assert!(!text.contains("Thinking..."));
        assert!(!text.contains("pondering..."));
    }

    #[test]
    fn push_thinking_lines_after_entries_has_separator() {
        let mut chat = test_chat();
        chat.push_user_message("hello".to_owned());
        chat.append_thinking_token("deep thought");
        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        let last_user = lines.iter().rposition(|l| l.contains("hello")).unwrap();
        let thinking = lines.iter().position(|l| l.contains("Thinking")).unwrap();
        assert!(
            (last_user + 1..thinking).any(|i| lines[i].trim().is_empty()),
            "expected blank separator between user message and thinking block"
        );
    }

    // ── push_streaming_lines ──

    #[test]
    fn push_streaming_lines_shows_partial_text() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.append_stream_token("partial response");
        let text = all_text(&chat);
        assert!(text.contains('⟡'), "should show assistant icon");
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
    fn push_streaming_lines_without_prior_assistant_shows_icon() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.streaming_buffer = "response".to_owned();

        let text = all_text(&chat);
        assert!(text.contains('⟡'), "new turn should show assistant icon");
    }

    #[test]
    fn push_streaming_lines_after_assistant_omits_duplicate_icon() {
        let mut chat = test_chat();
        chat.entries
            .push(ChatEntry::Assistant("committed".to_owned()));
        chat.streaming_buffer = "streaming".to_owned();

        let text = all_text(&chat);
        let count = text.matches('⟡').count();
        assert_eq!(count, 1, "icon should appear once, not duplicated");
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
        assert_eq!(chat.streaming_rendered.len(), 1);
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
        assert_eq!(chat.streaming_rendered_boundary, 6); // "first\n".len()
        let cached1 = chat.streaming_rendered.len();
        assert_eq!(cached1, 1);

        chat.streaming_buffer.push_str("second\n");
        chat.advance_streaming_cache();
        assert_eq!(chat.streaming_rendered_boundary, 13); // "first\nsecond\n".len()
        assert_eq!(chat.streaming_rendered.len(), 2);
    }

    #[test]
    fn advance_streaming_cache_trailing_newline_only() {
        let mut chat = test_chat();
        chat.streaming_buffer = "\n".to_owned();
        chat.advance_streaming_cache();
        assert_eq!(chat.streaming_rendered_boundary, 1);
    }
}
