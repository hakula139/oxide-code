//! Chat view — the scrollable message list.
//!
//! Each visible unit in the transcript (user messages, assistant
//! replies, tool calls and results, errors) is a [`blocks::ChatBlock`]
//! implementation. [`ChatView`] is the thin container: it appends
//! blocks, owns the streaming buffer, handles scroll state, and stacks
//! `render` outputs with appropriate blank-line separators.

mod blocks;

use std::cell::Cell;
use std::collections::HashMap;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use self::blocks::{
    AssistantText, AssistantThinking, BlockKind, ChatBlock, ErrorBlock, GitDiffBlock,
    InterruptedMarker, RenderCtx, StreamingAssistant, SystemMessageBlock, ToolCallBlock,
    ToolResultBlock, UserMessage, last_has_width,
};
use crate::message::Message;
use crate::session::history::{Interaction, walk_transcript};
use crate::tool::{ToolMetadata, ToolRegistry, ToolResultView};
use crate::tui::pending_calls::{FALLBACK_RESULT_HEADER, PendingCall, PendingCalls, result_header};
use crate::tui::theme::Theme;

/// Scrollable chat message list with auto-scroll.
pub(crate) struct ChatView {
    theme: Theme,
    show_thinking: bool,

    blocks: Vec<Box<dyn ChatBlock>>,

    streaming: Option<StreamingAssistant>,
    thinking_buffer: String,

    scroll_offset: u16,
    content_height: Cell<u16>,
    viewport_height: u16,
    viewport_width: u16,
    auto_scroll: bool,
}

impl ChatView {
    pub(crate) fn new(theme: &Theme, show_thinking: bool) -> Self {
        Self {
            theme: theme.clone(),
            show_thinking,
            blocks: Vec::new(),
            streaming: None,
            thinking_buffer: String::new(),
            scroll_offset: 0,
            content_height: Cell::new(0),
            viewport_height: 0,
            viewport_width: 0,
            auto_scroll: true,
        }
    }

    /// Populates from resumed transcript, projecting the same block shapes as live rendering.
    pub(crate) fn load_history(
        &mut self,
        messages: &[Message],
        metadata_by_tool_use_id: &HashMap<String, ToolMetadata>,
        tools: &ToolRegistry,
    ) {
        let mut pending = PendingCalls::new();
        let default_metadata = ToolMetadata::default();
        for interaction in walk_transcript(messages) {
            match interaction {
                Interaction::UserText(text) => {
                    self.blocks.push(Box::new(UserMessage::new(text)));
                }
                Interaction::AssistantText(text) => {
                    self.blocks.push(Box::new(AssistantText::new(text)));
                }
                Interaction::AssistantThinking(text) => {
                    self.blocks.push(Box::new(AssistantThinking::new(text)));
                }
                Interaction::ToolCall { id, name, input } => {
                    let icon = tools.icon(name);
                    let label = tools.label(name, input);
                    self.blocks
                        .push(Box::new(ToolCallBlock::new(icon, label.clone())));
                    pending.insert(
                        id.to_owned(),
                        PendingCall {
                            label,
                            name: name.to_owned(),
                            input: input.clone(),
                        },
                    );
                }
                Interaction::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let p = pending
                        .remove(tool_use_id)
                        .expect("walk_transcript pairs every ToolResult with its ToolCall");
                    let metadata = metadata_by_tool_use_id
                        .get(tool_use_id)
                        .unwrap_or(&default_metadata);
                    let view = tools.result_view(&p.name, &p.input, content, metadata, is_error);
                    let header = result_header(metadata, Some(p.label.as_str()));
                    self.blocks
                        .push(Box::new(ToolResultBlock::new(header, view, is_error)));
                }
                Interaction::OrphanToolResult { content, is_error } => {
                    let view = ToolResultView::Text {
                        content: content.to_owned(),
                    };
                    self.blocks.push(Box::new(ToolResultBlock::new(
                        FALLBACK_RESULT_HEADER,
                        view,
                        is_error,
                    )));
                }
            }
        }
    }

    /// Appends a user message, flushing any in-flight streaming buffer.
    pub(crate) fn push_user_message(&mut self, text: String) {
        self.commit_streaming();
        self.blocks.push(Box::new(UserMessage::new(text)));
        self.auto_scroll = true;
    }

    /// Appends a streamed token to the current assistant response.
    /// Any pending thinking is committed first.
    pub(crate) fn append_stream_token(&mut self, token: &str) {
        self.commit_thinking_buffer();
        self.streaming
            .get_or_insert_with(StreamingAssistant::new)
            .append(token);
        self.advance_streaming_cache();
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Appends a thinking token to the live thinking display buffer.
    pub(crate) fn append_thinking_token(&mut self, token: &str) {
        self.thinking_buffer.push_str(token);
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    /// Finalize the streaming buffer into a committed block. Flushes
    /// pending thinking first so thinking-only turns still leave a block.
    pub(crate) fn commit_streaming(&mut self) {
        self.commit_thinking_buffer();
        if let Some(mut s) = self.streaming.take() {
            let text = s.take_buffer();
            if !text.is_empty() {
                self.blocks.push(Box::new(AssistantText::new(text)));
            }
        }
    }

    fn commit_thinking_buffer(&mut self) {
        if self.thinking_buffer.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.thinking_buffer);
        self.blocks.push(Box::new(AssistantThinking::new(text)));
    }

    /// Appends a tool call, flushing any in-flight streaming buffer.
    pub(crate) fn push_tool_call(&mut self, icon: &'static str, label: &str) {
        self.commit_streaming();
        self.blocks.push(Box::new(ToolCallBlock::new(icon, label)));
    }

    /// Appends a tool result with a pre-built structured view.
    pub(crate) fn push_tool_result_view(
        &mut self,
        label: &str,
        view: ToolResultView,
        is_error: bool,
    ) {
        self.blocks
            .push(Box::new(ToolResultBlock::new(label, view, is_error)));
    }

    /// Test-only shortcut for the Text variant.
    #[cfg(test)]
    pub(crate) fn push_tool_result(&mut self, label: &str, content: &str, is_error: bool) {
        let view = ToolResultView::Text {
            content: content.to_owned(),
        };
        self.push_tool_result_view(label, view, is_error);
    }

    /// Appends an error message.
    pub(crate) fn push_error(&mut self, msg: &str) {
        self.blocks.push(Box::new(ErrorBlock::new(msg)));
    }

    /// Appends informational output from a slash command.
    pub(crate) fn push_system_message(&mut self, body: impl Into<String>) {
        self.blocks.push(Box::new(SystemMessageBlock::new(body)));
    }

    /// Appends a unified diff body for display.
    pub(crate) fn push_git_diff(&mut self, text: impl Into<String>) {
        self.blocks.push(Box::new(GitDiffBlock::new(text)));
    }

    /// Appends an interrupted marker. Flushes any in-flight streaming buffer first.
    pub(crate) fn push_interrupted_marker(&mut self) {
        self.commit_streaming();
        self.blocks.push(Box::new(InterruptedMarker));
    }

    /// Resets transient state, preserving terminal-tied fields.
    pub(crate) fn clear_history(&mut self) {
        self.blocks.clear();
        self.streaming = None;
        self.thinking_buffer.clear();
        self.scroll_offset = 0;
        self.content_height.set(0);
        self.auto_scroll = true;
    }

    /// Number of committed chat blocks.
    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        self.blocks.len()
    }

    /// Whether the tail block is an [`ErrorBlock`].
    #[cfg(test)]
    pub(crate) fn last_is_error(&self) -> bool {
        self.blocks.last().is_some_and(|b| b.is_error_marker())
    }

    /// Error text of the tail block, if it is an [`ErrorBlock`].
    #[cfg(test)]
    pub(crate) fn last_error_text(&self) -> Option<&str> {
        self.blocks.last().and_then(|b| b.error_text())
    }

    /// Body text of the tail block, if it is a [`SystemMessageBlock`].
    #[cfg(test)]
    pub(crate) fn last_system_text(&self) -> Option<&str> {
        self.blocks.last().and_then(|b| b.system_text())
    }

    /// Updates cached viewport height and syncs scroll position.
    /// Returns `true` when auto-scroll moved `scroll_offset`, so the
    /// caller knows to repaint with the new offset.
    #[must_use]
    pub(crate) fn update_layout(&mut self, area: Rect) -> bool {
        self.viewport_height = area.height;
        self.viewport_width = area.width;
        if let Some(s) = &mut self.streaming {
            s.invalidate_cache_for_width(area.width);
        }
        if !self.auto_scroll {
            return false;
        }
        let prev = self.scroll_offset;
        self.scroll_to_bottom();
        self.scroll_offset != prev
    }
}

impl ChatView {
    pub(crate) fn handle_event(&mut self, event: &Event) {
        match event {
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            })
            | Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                ..
            }) => self.scroll_up(1),
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            })
            | Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                ..
            }) => self.scroll_down(1),
            Event::Key(KeyEvent {
                code: KeyCode::PageUp,
                ..
            }) => self.scroll_up(self.viewport_height.saturating_sub(2)),
            Event::Key(KeyEvent {
                code: KeyCode::PageDown,
                ..
            }) => self.scroll_down(self.viewport_height.saturating_sub(2)),
            Event::Key(KeyEvent {
                code: KeyCode::Home,
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                self.scroll_offset = 0;
                self.auto_scroll = false;
            }
            Event::Key(KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                self.scroll_to_bottom();
                self.auto_scroll = true;
            }
            _ => {}
        }
    }

    pub(crate) fn render(&self, frame: &mut Frame, area: Rect) {
        let text = self.build_text(area.width);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "clamped to u16::MAX; truncation cannot occur"
        )]
        let height = text.lines.len().min(u16::MAX as usize) as u16;
        self.content_height.set(height);
        let paragraph = Paragraph::new(text)
            .style(self.theme.surface())
            .scroll((self.scroll_offset, 0));
        frame.render_widget(paragraph, area);
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

    fn render_ctx(&self, width: u16) -> RenderCtx<'_> {
        RenderCtx {
            width,
            theme: &self.theme,
            show_thinking: self.show_thinking,
        }
    }

    fn build_text(&self, width: u16) -> Text<'static> {
        let ctx = self.render_ctx(width);
        let mut lines: Vec<Line<'static>> = Vec::new();

        if self.is_empty() {
            push_welcome(&mut lines, &ctx);
            return Text::from(lines);
        }

        let mut prev_kind: Option<BlockKind> = None;
        for block in &self.blocks {
            if !block.visible(&ctx) {
                continue;
            }
            let kind = block.block_kind();
            let needs_blank_before = !lines.is_empty()
                && last_has_width(&lines)
                && (block.standalone() || prev_kind == Some(BlockKind::Result));
            if needs_blank_before {
                lines.push(Line::raw(""));
            }
            lines.extend(block.render(&ctx));
            if block.standalone() {
                lines.push(Line::raw(""));
            }
            prev_kind = Some(kind);
        }

        if !self.thinking_buffer.is_empty() {
            let thinking = AssistantThinking::new(self.thinking_buffer.clone());
            if thinking.visible(&ctx) {
                if !lines.is_empty() && last_has_width(&lines) {
                    lines.push(Line::raw(""));
                }
                lines.extend(thinking.render(&ctx));
            }
        }

        if let Some(streaming) = &self.streaming {
            let continues = self.streaming_continues_turn();
            streaming.render_into(&mut lines, &ctx, continues);
        }

        Text::from(lines)
    }

    fn advance_streaming_cache(&mut self) {
        let continues = self.streaming_continues_turn();
        let width = self.viewport_width;
        let theme = &self.theme;
        let show_thinking = self.show_thinking;
        if let Some(s) = &mut self.streaming {
            let ctx = RenderCtx {
                width,
                theme,
                show_thinking,
            };
            s.advance_cache(&ctx, continues);
        }
    }

    /// Whether streaming tokens continue the last committed assistant turn.
    fn streaming_continues_turn(&self) -> bool {
        self.blocks
            .last()
            .is_some_and(|b| b.continues_assistant_turn())
    }

    fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.streaming.is_none() && self.thinking_buffer.is_empty()
    }
}

/// Welcome splash for an empty chat.
fn push_welcome(lines: &mut Vec<Line<'static>>, ctx: &RenderCtx<'_>) {
    let title = "Welcome to ox";
    let subtitle = "Ask anything to begin.";
    let width = usize::from(ctx.width);
    let title_pad = width.saturating_sub(title.len()) / 2;
    let subtitle_pad = width.saturating_sub(subtitle.len()) / 2;

    lines.push(Line::raw(""));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(title_pad)),
        Span::styled(title, ctx.theme.accent()),
    ]));
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(subtitle_pad)),
        Span::styled(subtitle, ctx.theme.dim()),
    ]));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use indoc::indoc;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use unicode_width::UnicodeWidthStr;

    use super::*;
    use crate::file_tracker::testing::tracker;
    use crate::message::{ContentBlock, Role};
    use crate::tui::glyphs::{ASSISTANT_PREFIX, BAR, TOOL_BORDER_CONT, TOOL_ERROR, TOOL_SUCCESS};

    // ── Fixtures ──

    fn test_chat() -> ChatView {
        ChatView::new(&Theme::default(), true)
    }

    fn test_tools() -> ToolRegistry {
        let tracker = tracker();
        ToolRegistry::new(vec![
            Box::new(crate::tool::bash::BashTool),
            Box::new(crate::tool::read::ReadTool::new(Arc::clone(&tracker))),
            Box::new(crate::tool::write::WriteTool::new(Arc::clone(&tracker))),
            Box::new(crate::tool::edit::EditTool::new(tracker)),
            Box::new(crate::tool::glob::GlobTool),
            Box::new(crate::tool::grep::GrepTool),
        ])
    }

    /// Render `build_text` at default width and join all span content
    /// into a single string for substring assertions.
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

    fn line_count(chat: &ChatView) -> usize {
        chat.build_text(80).lines.len()
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

    fn render_chat(chat: &mut ChatView, width: u16, height: u16) -> TestBackend {
        _ = chat.update_layout(Rect::new(0, 0, width, height));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                chat.render(frame, frame.area());
            })
            .unwrap();
        terminal.backend().clone()
    }

    // ── load_history ──

    #[test]
    fn load_history_populates_user_and_assistant_entries() {
        let mut chat = test_chat();
        chat.load_history(
            &[Message::user("hello"), Message::assistant("hi there")],
            &HashMap::new(),
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 2);
        let text = all_text(&chat);
        assert!(text.contains("hello"));
        assert!(text.contains("hi there"));
    }

    #[test]
    fn load_history_multi_tool_turn_pairs_inline_with_orphan_fallback() {
        let mut chat = test_chat();
        chat.load_history(
            &[
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::ToolUse {
                            id: "t1".to_owned(),
                            name: "read".to_owned(),
                            input: serde_json::json!({"file_path": "a.rs"}),
                        },
                        ContentBlock::ToolUse {
                            id: "t2".to_owned(),
                            name: "grep".to_owned(),
                            input: serde_json::json!({"pattern": "TODO"}),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "t1".to_owned(),
                            content: "file a".to_owned(),
                            is_error: false,
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "ghost".to_owned(),
                            content: "stale output".to_owned(),
                            is_error: true,
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "t2".to_owned(),
                            content: "3 matches".to_owned(),
                            is_error: false,
                        },
                    ],
                },
            ],
            &HashMap::new(),
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 5);
        let text = all_text(&chat);
        let a_call = text.find("a.rs").unwrap();
        let file_a = text.find("file a").unwrap();
        let todo_call = text.find("TODO").unwrap();
        let matches = text.find("3 matches").unwrap();
        let stale = text.find("stale output").unwrap();
        let result_label = text.find("(result)").unwrap();
        assert!(a_call < file_a);
        assert!(file_a < todo_call);
        assert!(todo_call < matches);
        assert!(matches < stale);
        assert!(result_label < stale);
    }

    #[test]
    fn load_history_renders_tool_result_after_paired_tool_use() {
        let mut chat = test_chat();
        chat.load_history(
            &[
                Message::user("ask"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "t1".to_owned(),
                        name: "bash".to_owned(),
                        input: serde_json::json!({"command": "ls"}),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "t1".to_owned(),
                        content: "output".to_owned(),
                        is_error: false,
                    }],
                },
                Message::assistant("reply"),
            ],
            &HashMap::new(),
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 4);
        let text = all_text(&chat);
        assert!(
            text.find("ask")
                < text
                    .find("ls")
                    .and_then(|i| text[i..].find("output").map(|j| i + j))
        );
        assert!(text.contains("ask"));
        assert!(text.contains("ls"));
        assert!(text.contains("output"));
        assert!(text.contains("reply"));
    }

    #[test]
    fn load_history_tool_result_without_matching_tool_use_uses_fallback_label() {
        let mut chat = test_chat();
        chat.load_history(
            &[Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "missing".to_owned(),
                    content: "stderr".to_owned(),
                    is_error: true,
                }],
            }],
            &HashMap::new(),
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 1);
        let text = all_text(&chat);
        assert!(text.contains("(result)"));
        assert!(text.contains("stderr"));
        assert!(text.contains(TOOL_ERROR));
    }

    #[test]
    fn load_history_joins_multiple_text_blocks() {
        let mut chat = test_chat();
        chat.load_history(
            &[Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "first".to_owned(),
                    },
                    ContentBlock::Text {
                        text: "second".to_owned(),
                    },
                ],
            }],
            &HashMap::new(),
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 1);
        let text = all_text(&chat);
        assert!(text.contains("first"));
        assert!(text.contains("second"));
        assert!(
            !text.contains(BAR),
            "assistant text should render without the left bar: {text}"
        );
    }

    #[test]
    fn load_history_skips_whitespace_only_text() {
        let mut chat = test_chat();
        chat.load_history(
            &[Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "  \n  ".to_owned(),
                }],
            }],
            &HashMap::new(),
            &test_tools(),
        );
        assert!(chat.blocks.is_empty());
    }

    #[test]
    fn load_history_empty_slice_is_noop() {
        let mut chat = test_chat();
        chat.load_history(&[], &HashMap::new(), &test_tools());
        assert!(chat.blocks.is_empty());
    }

    #[test]
    fn load_history_restores_tool_call_after_assistant_text() {
        let mut chat = test_chat();
        chat.load_history(
            &[Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Let me check that.".to_owned(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".to_owned(),
                        name: "bash".to_owned(),
                        input: serde_json::json!({"command": "ls -la"}),
                    },
                ],
            }],
            &HashMap::new(),
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 2);
        let text = all_text(&chat);
        assert!(text.find("Let me check that.") < text.find("ls -la"));
        assert!(text.contains('$')); // bash icon
    }

    #[test]
    fn load_history_unknown_tool_falls_back_to_tool_name_as_label() {
        let mut chat = test_chat();
        chat.load_history(
            &[Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "custom_tool".to_owned(),
                    input: serde_json::json!({"arg": "value"}),
                }],
            }],
            &HashMap::new(),
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 1);
        let text = all_text(&chat);
        assert!(text.contains('⟡'));
        assert!(text.contains("custom_tool"));
    }

    #[test]
    fn load_history_server_tool_use_renders_like_local_tool_call() {
        let mut chat = test_chat();
        chat.load_history(
            &[Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ServerToolUse {
                    id: "srv1".to_owned(),
                    name: "web_search".to_owned(),
                    input: serde_json::json!({"query": "rust"}),
                }],
            }],
            &HashMap::new(),
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 1);
        let text = all_text(&chat);
        assert!(text.contains('⟡'));
        assert!(text.contains("web_search"));
    }

    #[test]
    fn load_history_redacted_thinking_is_dropped() {
        let mut chat = test_chat();
        chat.load_history(
            &[Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::RedactedThinking {
                        data: "opaque-ciphertext".to_owned(),
                    },
                    ContentBlock::Text {
                        text: "fine".to_owned(),
                    },
                ],
            }],
            &HashMap::new(),
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 1);
        let text = all_text(&chat);
        assert!(text.contains("fine"));
        assert!(!text.contains("opaque-ciphertext"));
    }

    #[test]
    fn load_history_renders_resumed_thinking_when_show_thinking_enabled() {
        let mut chat = test_chat();
        chat.load_history(
            &[Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "resumed reasoning".to_owned(),
                        signature: "sig".to_owned(),
                    },
                    ContentBlock::Text {
                        text: "reply".to_owned(),
                    },
                ],
            }],
            &HashMap::new(),
            &test_tools(),
        );
        let text = all_text(&chat);
        assert!(text.contains("Thinking..."));
        assert!(text.contains("resumed reasoning"));
    }

    #[test]
    fn load_history_uses_persisted_metadata_title_and_replacements() {
        let tools = test_tools();
        let mut chat = test_chat();
        let history = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "edit1".to_owned(),
                    name: "edit".to_owned(),
                    input: serde_json::json!({
                        "file_path": "/tmp/f.rs",
                        "old_string": "a",
                        "new_string": "b",
                        "replace_all": true,
                    }),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "edit1".to_owned(),
                    content: "Replaced 4 occurrences in /tmp/f.rs.".to_owned(),
                    is_error: false,
                }],
            },
        ];
        let mut metadata_map = HashMap::new();
        metadata_map.insert(
            "edit1".to_owned(),
            crate::tool::ToolMetadata {
                title: Some("Edited f.rs".to_owned()),
                replacements: Some(4),
                ..crate::tool::ToolMetadata::default()
            },
        );
        chat.load_history(&history, &metadata_map, &tools);
        let text = all_text(&chat);
        assert!(
            text.contains("✓ Edited f.rs"),
            "persisted title should drive the result row: {text}",
        );
        assert!(
            text.contains("4 occurrences replaced"),
            "metadata.replacements should drive the diff footer: {text}",
        );
    }

    #[test]
    fn load_history_hides_resumed_thinking_when_show_thinking_disabled() {
        let mut chat = ChatView::new(&Theme::default(), false);
        chat.load_history(
            &[Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "private reasoning".to_owned(),
                        signature: "sig".to_owned(),
                    },
                    ContentBlock::Text {
                        text: "reply".to_owned(),
                    },
                ],
            }],
            &HashMap::new(),
            &test_tools(),
        );
        let text = all_text(&chat);
        assert!(!text.contains("Thinking..."));
        assert!(!text.contains("private reasoning"));
        assert!(text.contains("reply"));
    }

    // ── push_user_message ──

    #[test]
    fn push_user_message_has_icon_and_content() {
        let mut chat = test_chat();
        chat.push_user_message("hello world".to_owned());
        let text = all_text(&chat);
        assert!(text.contains('❯'));
        assert!(text.contains("hello world"));
        assert!(
            !text.contains(BAR),
            "user message should render without the left bar: {text}"
        );
    }

    #[test]
    fn push_user_message_has_trailing_blank_before_tool_call() {
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
    fn user_followed_by_assistant_has_no_double_blank() {
        let mut chat = test_chat();
        chat.push_user_message("hello".to_owned());
        chat.blocks.push(Box::new(AssistantText::new("reply")));
        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
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
    fn push_user_message_multiline_renders_every_line() {
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

    // ── append_stream_token ──

    #[test]
    fn append_stream_token_persists_thinking_into_blocks() {
        let mut chat = test_chat();
        chat.append_thinking_token("reasoning here");
        assert!(!chat.thinking_buffer.is_empty());
        assert_eq!(chat.blocks.len(), 0);

        chat.append_stream_token("reply text");
        assert!(chat.thinking_buffer.is_empty());
        assert_eq!(chat.blocks.len(), 1);

        let text = all_text(&chat);
        assert!(text.contains("reasoning here"));
        assert!(text.contains("reply text"));
    }

    #[test]
    fn append_stream_token_shows_partial_text() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.append_stream_token("partial response");
        let text = all_text(&chat);
        assert!(
            text.contains(ASSISTANT_PREFIX),
            "should show assistant icon"
        );
        assert!(text.contains("partial response"));
    }

    #[test]
    fn append_stream_token_cached_and_tail_both_visible() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("cached line\n");
        chat.append_stream_token("tail text");

        let text = all_text(&chat);
        assert!(text.contains("cached line"));
        assert!(text.contains("tail text"));
    }

    #[test]
    fn append_stream_token_uncommitted_newlines_all_render() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.viewport_width = 80;
        chat.append_stream_token("line1\nline2\npartial");

        let text = all_text(&chat);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
        assert!(text.contains("partial"));
    }

    #[test]
    fn append_stream_token_without_prior_assistant_shows_icon() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.append_stream_token("response");

        let text = all_text(&chat);
        assert!(
            text.contains(ASSISTANT_PREFIX),
            "new turn should show assistant icon"
        );
    }

    #[test]
    fn append_stream_token_after_committed_assistant_omits_duplicate_icon() {
        let mut chat = test_chat();
        chat.blocks.push(Box::new(AssistantText::new("committed")));
        let mut s = StreamingAssistant::new();
        s.append("streaming");
        chat.streaming = Some(s);

        let text = all_text(&chat);
        let count = text.matches(ASSISTANT_PREFIX).count();
        assert_eq!(count, 1, "icon should appear once, not duplicated");
    }

    #[test]
    fn append_stream_token_inserts_blank_separator_after_tool_output() {
        let mut chat = test_chat();
        chat.push_tool_call("$", "ls");
        chat.append_stream_token("response");

        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        let tool_pos = lines.iter().position(|l| l.contains("ls")).unwrap();
        let stream_pos = lines.iter().position(|l| l.contains("response")).unwrap();
        assert!(
            (tool_pos + 1..stream_pos).any(|i| lines[i].trim().is_empty()),
            "expected blank separator between tool call and streaming: {lines:?}"
        );
    }

    #[test]
    fn append_stream_token_renders_committed_and_trailing_before_cache_advance() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.append_stream_token("cached line\ntail text");
        assert_eq!(
            chat.streaming.as_ref().unwrap().rendered_boundary(),
            0,
            "advance_cache must defer when viewport_width is 0"
        );

        let text = all_text(&chat);
        assert!(text.contains("cached line"));
        assert!(text.contains("tail text"));
    }

    #[test]
    fn append_stream_token_renders_buffer_ending_in_newline_before_cache_advance() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.append_stream_token("line1\nline2\n");
        assert_eq!(
            chat.streaming.as_ref().unwrap().rendered_boundary(),
            0,
            "advance_cache must defer when viewport_width is 0"
        );

        let text = all_text(&chat);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
    }

    #[test]
    fn append_stream_token_trailing_newline_with_empty_tail() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.viewport_width = 80;
        chat.append_stream_token("line1\nline2\n");

        let text = all_text(&chat);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
    }

    #[test]
    fn append_stream_token_preserves_blank_between_committed_paragraphs() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("para1\n\n");
        chat.append_stream_token("para2\n\ntail");

        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        let p1 = lines.iter().position(|l| l.contains("para1")).unwrap();
        let p2 = lines.iter().position(|l| l.contains("para2")).unwrap();
        assert!(
            (p1 + 1..p2).any(|i| lines[i].trim().is_empty()),
            "expected blank separator between committed paragraphs: {lines:?}"
        );
    }

    #[test]
    fn append_stream_token_no_spurious_blank_between_consecutive_list_items() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("- item 1\n- item 2");

        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        let first = lines.iter().position(|l| l.contains("item 1")).unwrap();
        let second = lines.iter().position(|l| l.contains("item 2")).unwrap();
        assert!(
            (first + 1..second).all(|i| !lines[i].trim().is_empty()),
            "expected no blank between consecutive list items: {lines:?}",
        );
    }

    #[test]
    fn append_stream_token_preserves_blank_before_partial_list_item_trailing() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("Here are items:\n- item 1");

        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        let header = lines
            .iter()
            .position(|l| l.contains("Here are items:"))
            .unwrap();
        let item = lines.iter().position(|l| l.contains("item 1")).unwrap();
        assert!(
            (header + 1..item).any(|i| lines[i].trim().is_empty()),
            "expected blank separator before partial list item: {lines:?}",
        );
    }

    #[test]
    fn append_stream_token_preserves_blank_between_cache_and_live_tail() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("committed paragraph\n\n");
        chat.append_stream_token("still streaming");

        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        let committed = lines
            .iter()
            .position(|l| l.contains("committed paragraph"))
            .unwrap();
        let live = lines
            .iter()
            .position(|l| l.contains("still streaming"))
            .unwrap();
        assert!(
            (committed + 1..live).any(|i| lines[i].trim().is_empty()),
            "expected blank separator between cache and live tail: {lines:?}"
        );
    }

    // ── append_thinking_token ──

    #[test]
    fn append_thinking_token_visible_when_enabled() {
        let mut chat = test_chat();
        chat.append_thinking_token("pondering...");
        let text = all_text(&chat);
        assert!(text.contains("Thinking..."));
        assert!(text.contains("pondering..."));
    }

    #[test]
    fn append_thinking_token_hidden_when_disabled() {
        let mut chat = ChatView::new(&Theme::default(), false);
        chat.append_thinking_token("pondering...");
        let text = all_text(&chat);
        assert!(!text.contains("Thinking..."));
        assert!(!text.contains("pondering..."));
    }

    #[test]
    fn append_thinking_token_after_user_has_separator() {
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

    #[test]
    fn append_thinking_token_after_tool_call_has_separator() {
        let mut chat = test_chat();
        chat.push_tool_call("$", "ls");
        chat.append_thinking_token("deep thought");

        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        let tool_pos = lines.iter().position(|l| l.contains("ls")).unwrap();
        let thinking_pos = lines.iter().position(|l| l.contains("Thinking")).unwrap();
        assert!(
            (tool_pos + 1..thinking_pos).any(|i| lines[i].trim().is_empty()),
            "expected blank separator between tool call and thinking: {lines:?}"
        );
    }

    // ── commit_streaming ──

    #[test]
    fn commit_streaming_moves_buffer_to_block() {
        let mut chat = test_chat();
        chat.append_stream_token("hello world");
        assert!(chat.blocks.is_empty());

        chat.commit_streaming();
        assert_eq!(chat.blocks.len(), 1);
        let text = all_text(&chat);
        assert!(text.contains("hello world"));
        assert!(chat.streaming.is_none());
    }

    #[test]
    fn commit_streaming_empty_buffer_no_block() {
        let mut chat = test_chat();
        chat.commit_streaming();
        assert!(chat.blocks.is_empty());
    }

    #[test]
    fn commit_streaming_clears_state() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("line1\nline2\npartial");
        assert!(chat.streaming.is_some());

        chat.commit_streaming();
        assert!(chat.streaming.is_none());
        assert!(chat.thinking_buffer.is_empty());
    }

    #[test]
    fn commit_streaming_persists_thinking_only_turn() {
        let mut chat = test_chat();
        chat.append_thinking_token("plan before tool");
        assert!(chat.blocks.is_empty());

        chat.commit_streaming();
        assert_eq!(chat.blocks.len(), 1);
        assert!(chat.thinking_buffer.is_empty());
        let text = all_text(&chat);
        assert!(text.contains("plan before tool"));
    }

    // ── push_tool_call ──

    #[test]
    fn push_tool_call_shows_icon_and_label() {
        let mut chat = test_chat();
        chat.push_tool_call("$", "ls -la");
        let text = all_text(&chat);
        assert!(text.contains('$'));
        assert!(text.contains("ls -la"));
        assert!(
            text.contains(BAR),
            "tool call should retain the left bar: {text}"
        );
    }

    #[test]
    fn push_tool_call_after_assistant_has_blank_separator() {
        let mut chat = test_chat();
        chat.blocks.push(Box::new(AssistantText::new("some text")));
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
    fn tool_result_followed_by_new_tool_call_has_borderless_spacer() {
        let mut chat = test_chat();
        chat.push_tool_call("$", "ls");
        chat.push_tool_result("ran ls", "output one", false);
        chat.push_tool_call("$", "pwd");
        chat.push_tool_result("ran pwd", "output two", false);
        let text = all_text(&chat);
        let lines: Vec<&str> = text.lines().collect();
        let first_result = lines.iter().position(|l| l.contains("output one")).unwrap();
        let next_call = lines.iter().position(|l| l.contains("pwd")).unwrap();
        let separators: Vec<&str> = lines[first_result + 1..next_call]
            .iter()
            .filter(|l| l.trim().is_empty())
            .copied()
            .collect();
        assert_eq!(
            separators.len(),
            1,
            "expected one blank separator between groups: {lines:?}",
        );
        assert!(
            separators.iter().all(|s| !s.contains(BAR)),
            "spacer must be borderless: {separators:?}",
        );
    }

    #[test]
    fn consecutive_tool_calls_have_no_gap() {
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

    #[test]
    fn push_tool_call_wraps_long_label() {
        let mut chat = test_chat();
        let long_cmd =
            "cd /home/user/projects/example-app && ls ${XDG_DATA_HOME:-$HOME/.local/share}/ox";
        chat.push_tool_call("$", long_cmd);
        let text = chat.build_text(60);
        assert!(
            text.lines.len() > 1,
            "long tool call label should wrap across multiple lines: {}",
            text.lines.len(),
        );
        for line in &text.lines {
            let width: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(
                width <= 60,
                "wrapped tool call line must fit the width budget (got {width}): {line:?}",
            );
        }
    }

    // ── push_tool_result_view ──

    #[test]
    fn push_tool_result_view_read_excerpt_renders_context_and_line_numbers() {
        let mut chat = test_chat();
        let view = crate::tool::ToolResultView::ReadExcerpt {
            path: "/tmp/example.rs".to_owned(),
            lines: vec![
                crate::tool::ReadExcerptLine {
                    number: 2,
                    text: "fn main() {".to_owned(),
                },
                crate::tool::ReadExcerptLine {
                    number: 3,
                    text: "}".to_owned(),
                },
            ],
            total_lines: 5,
        };
        chat.push_tool_result_view("Read example.rs", view, false);
        let text = all_text(&chat);
        assert!(text.contains("/tmp/example.rs:2-3 of 5"));
        assert!(text.contains("2 │ fn main() {"));
        assert!(text.contains("3 │ }"));
    }

    #[test]
    fn push_tool_result_view_read_excerpt_truncates_body_lines() {
        let mut chat = test_chat();
        let lines = (1..=6)
            .map(|number| crate::tool::ReadExcerptLine {
                number,
                text: format!("line {number}"),
            })
            .collect();
        let view = crate::tool::ToolResultView::ReadExcerpt {
            path: "/tmp/example.rs".to_owned(),
            lines,
            total_lines: 6,
        };
        chat.push_tool_result_view("Read example.rs", view, false);
        let text = all_text(&chat);
        assert!(text.contains("... +1 line"));
        assert!(text.contains("5 │ line 5"));
        assert!(!text.contains("6 │ line 6"));
    }

    #[test]
    fn push_tool_result_view_read_excerpt_empty_file_keeps_context() {
        let mut chat = test_chat();
        let view = crate::tool::ToolResultView::ReadExcerpt {
            path: "/tmp/empty.rs".to_owned(),
            lines: Vec::new(),
            total_lines: 0,
        };
        chat.push_tool_result_view("Read empty.rs", view, false);
        let text = all_text(&chat);
        assert!(text.contains("/tmp/empty.rs (empty file)"));
    }

    #[test]
    fn push_tool_result_view_edit_renders_diff_markers() {
        let mut chat = test_chat();
        let view = crate::tool::ToolResultView::Diff {
            chunks: vec![crate::tool::edit::synthesize_chunk("fn foo()", "fn bar()")],
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited file.rs", view, false);
        let text = all_text(&chat);
        assert!(text.contains("- fn foo()"), "old side missing: {text}");
        assert!(text.contains("+ fn bar()"), "new side missing: {text}");
        assert!(
            !text.contains("Successfully edited"),
            "diff should replace the raw content body: {text}",
        );
        assert!(
            !text.contains("occurrences replaced"),
            "replace_all=false should suppress the match-count footer: {text}",
        );
    }

    #[test]
    fn push_tool_result_view_edit_replace_all_shows_match_count() {
        let mut chat = test_chat();
        let view = crate::tool::ToolResultView::Diff {
            chunks: vec![crate::tool::edit::synthesize_chunk("a", "b")],
            replace_all: true,
            replacements: 3,
        };
        chat.push_tool_result_view("Edited file.rs", view, false);
        let text = all_text(&chat);
        assert!(
            text.contains("3 occurrences replaced"),
            "replace-all footer missing: {text}",
        );
    }

    #[test]
    fn push_tool_result_view_edit_replace_all_multi_chunk_shows_locations_footer() {
        let mut chat = test_chat();
        let chunks = [12, 47, 200]
            .into_iter()
            .map(|line| crate::tool::DiffChunk {
                old: vec![crate::tool::DiffLine {
                    number: line,
                    text: "old".to_owned(),
                }],
                new: vec![crate::tool::DiffLine {
                    number: line,
                    text: "new".to_owned(),
                }],
            })
            .collect();
        let view = crate::tool::ToolResultView::Diff {
            chunks,
            replace_all: true,
            replacements: 3,
        };
        chat.push_tool_result_view("Edited file.rs", view, false);
        let text = all_text(&chat);
        assert!(
            text.contains("applied at lines 12, 47, 200"),
            "locations footer missing: {text}",
        );
        assert!(
            !text.contains("3 occurrences replaced"),
            "legacy count footer must not duplicate the locations footer: {text}",
        );
    }

    #[test]
    fn push_tool_result_view_edit_single_replacement_hides_count_footer() {
        let mut chat = test_chat();
        let view = crate::tool::ToolResultView::Diff {
            chunks: vec![crate::tool::edit::synthesize_chunk("a", "b")],
            replace_all: true,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited file.rs", view, false);
        let text = all_text(&chat);
        assert!(
            !text.contains("occurrences replaced"),
            "single-replacement count footer should be suppressed: {text}",
        );
        assert!(
            !text.contains("applied at"),
            "single-replacement locations footer should be suppressed: {text}",
        );
    }

    #[test]
    fn push_tool_result_view_grep_renders_path_header_and_numbered_match_rows() {
        let mut chat = test_chat();
        let view = crate::tool::ToolResultView::GrepMatches {
            groups: vec![
                crate::tool::GrepFileGroup {
                    path: "src/main.rs".to_owned(),
                    lines: vec![
                        crate::tool::GrepMatchLine {
                            number: 10,
                            text: "fn main() {".to_owned(),
                            is_match: true,
                        },
                        crate::tool::GrepMatchLine {
                            number: 11,
                            text: "    helper();".to_owned(),
                            is_match: false,
                        },
                    ],
                },
                crate::tool::GrepFileGroup {
                    path: "src/lib.rs".to_owned(),
                    lines: vec![crate::tool::GrepMatchLine {
                        number: 5,
                        text: "fn lib_func()".to_owned(),
                        is_match: true,
                    }],
                },
            ],
            truncated: false,
        };
        chat.push_tool_result_view("Grep(fn)", view, false);
        let text = all_text(&chat);
        assert!(text.contains("src/main.rs"), "missing main.rs: {text}");
        assert!(text.contains("src/lib.rs"), "missing lib.rs: {text}");
        assert!(text.contains("10 │ fn main() {"), "missing match: {text}");
        assert!(
            text.contains("11 │     helper();"),
            "missing context: {text}",
        );
        assert!(
            text.contains(" 5 │ fn lib_func()"),
            "padding mismatch: {text}",
        );
    }

    #[test]
    fn push_tool_result_view_grep_truncates_body_with_hidden_line_count() {
        let mut chat = test_chat();
        let lines = (1..=5)
            .map(|number| crate::tool::GrepMatchLine {
                number,
                text: format!("hit {number}"),
                is_match: true,
            })
            .collect();
        let view = crate::tool::ToolResultView::GrepMatches {
            groups: vec![crate::tool::GrepFileGroup {
                path: "src/main.rs".to_owned(),
                lines,
            }],
            truncated: false,
        };
        chat.push_tool_result_view("Grep(hit)", view, false);
        let text = all_text(&chat);
        assert!(text.contains("4 │ hit 4"), "missing 4th row: {text}");
        assert!(!text.contains("5 │ hit 5"), "5th row leaked: {text}");
        assert!(text.contains("... +1 line"), "wrong footer: {text}");
    }

    #[test]
    fn push_tool_result_view_grep_truncated_flag_emits_limit_reached_marker() {
        let mut chat = test_chat();
        let view = crate::tool::ToolResultView::GrepMatches {
            groups: vec![crate::tool::GrepFileGroup {
                path: "src/main.rs".to_owned(),
                lines: vec![crate::tool::GrepMatchLine {
                    number: 1,
                    text: "hit".to_owned(),
                    is_match: true,
                }],
            }],
            truncated: true,
        };
        chat.push_tool_result_view("Grep(hit)", view, false);
        let text = all_text(&chat);
        assert!(text.contains("limit reached"), "missing footer: {text}");
        assert!(!text.contains("+0"), "phantom hidden count: {text}");
    }

    #[test]
    fn push_tool_result_view_glob_renders_path_list_with_total_in_footer() {
        let mut chat = test_chat();
        let files: Vec<String> = (0..7).map(|i| format!("src/f{i}.rs")).collect();
        let view = crate::tool::ToolResultView::GlobFiles {
            pattern: "**/*.rs".to_owned(),
            files,
            total: 1234,
        };
        chat.push_tool_result_view("Glob(**/*.rs)", view, false);
        let text = all_text(&chat);
        assert!(
            text.contains("**/*.rs (5 of 1234)"),
            "header missing: {text}",
        );
        assert!(text.contains("src/f0.rs"), "first row missing: {text}");
        assert!(text.contains("src/f4.rs"), "5th row missing: {text}");
        assert!(
            !text.contains("src/f5.rs"),
            "6th row leaked past cap: {text}"
        );
        assert!(
            text.contains("... +2 files (limit reached)"),
            "footer text wrong: {text}",
        );
    }

    // ── push_tool_result ──

    #[test]
    fn push_tool_result_success() {
        let mut chat = test_chat();
        chat.push_tool_result("done", "output text", false);
        let text = all_text(&chat);
        assert!(text.contains(TOOL_SUCCESS));
        assert!(text.contains("done"));
        assert!(text.contains("output text"));
    }

    #[test]
    fn push_tool_result_error() {
        let mut chat = test_chat();
        chat.push_tool_result("failed", "error details", true);
        let text = all_text(&chat);
        assert!(text.contains(TOOL_ERROR));
        assert!(text.contains("failed"));
        assert!(text.contains("error details"));
        let rendered = chat.build_text(60);
        let bar_style = rendered
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(BAR))
            .map(|s| s.style)
            .expect("rendered line should contain a ▎ span");
        assert_eq!(
            bar_style,
            Theme::default().error(),
            "error-result bar should use the theme error style, got {bar_style:?}"
        );
    }

    #[test]
    fn push_tool_result_wraps_long_label() {
        let mut chat = test_chat();
        let long_label =
            "some-very-long-file-path-that-exceeds.the.width.budget/and/then/more/path";
        chat.push_tool_result(long_label, "", false);
        let text = chat.build_text(50);
        assert!(
            text.lines.len() > 1,
            "long tool result label should wrap: {}",
            text.lines.len(),
        );
        for line in &text.lines {
            let width: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(
                width <= 50,
                "wrapped tool result line must fit width (got {width}): {line:?}",
            );
        }
    }

    #[test]
    fn push_tool_result_truncation() {
        let mut chat = test_chat();
        let long_output = (0..10).map(|i| format!("line {i}")).collect::<Vec<_>>();
        chat.push_tool_result("result", &long_output.join("\n"), false);
        let text = all_text(&chat);

        assert!(text.contains("line 0"));
        assert!(text.contains("line 4"));
        assert!(!text.contains("line 5"));
        assert!(text.contains("... +5 lines"));
        assert!(
            text.contains(&format!("{TOOL_BORDER_CONT}line 0")),
            "tool result body should indent past the status indicator: {text}"
        );
    }

    #[test]
    fn push_tool_result_empty_content_adds_nothing() {
        let mut chat = test_chat();
        chat.push_tool_result("result", "  \n  ", false);
        let before = line_count(&chat);

        let mut chat2 = test_chat();
        chat2.push_tool_result("result", "", false);
        let after = line_count(&chat2);

        assert_eq!(before, after);
    }

    #[test]
    fn push_tool_result_dedup_drops_first_body_line_matching_label() {
        let mut chat = test_chat();
        chat.push_tool_result(
            "Found 2 files",
            indoc! {"
                Found 2 files
                a.rs
                b.rs"
            },
            false,
        );
        let text = all_text(&chat);
        assert_eq!(
            text.matches("Found 2 files").count(),
            1,
            "label must not appear twice: {text}",
        );
        assert!(text.contains("a.rs"));
        assert!(text.contains("b.rs"));
    }

    #[test]
    fn push_tool_result_dedup_leaves_unrelated_first_line_intact() {
        let mut chat = test_chat();
        chat.push_tool_result(
            "Found 2 files",
            indoc! {"
                Found 2 files in cache
                a.rs"
            },
            false,
        );
        let text = all_text(&chat);
        assert!(
            text.contains("Found 2 files in cache"),
            "body preserved: {text}"
        );
    }

    #[test]
    fn push_tool_result_preserves_leading_whitespace_on_first_body_line() {
        let mut chat = test_chat();
        chat.push_tool_result("out", " a.rs | 1 +\n b.rs | 2 +", false);
        let text = all_text(&chat);
        assert!(
            text.contains(" a.rs | 1 +"),
            "first body line must keep its leading space: {text}",
        );
        assert!(text.contains(" b.rs | 2 +"));
    }

    #[test]
    fn push_tool_result_drops_surrounding_blank_lines() {
        let mut chat = test_chat();
        chat.push_tool_result("out", "\n\n real line\n\n\n", false);
        let text = all_text(&chat);
        let body_row_count = text
            .lines()
            .filter(|l| l.starts_with(TOOL_BORDER_CONT))
            .count();
        assert_eq!(
            body_row_count, 1,
            "expected one body row after blank-line stripping: {text}",
        );
        assert!(
            text.contains(&format!("{TOOL_BORDER_CONT} real line")),
            "data-line indent must survive: {text}",
        );
    }

    #[test]
    fn push_tool_result_dedup_collapses_body_when_only_line_matches_label() {
        let mut chat = test_chat();
        chat.push_tool_result("No matches found", "No matches found", false);
        let text = all_text(&chat);
        assert_eq!(
            text.matches("No matches found").count(),
            1,
            "body collapses when it only repeats the label: {text}",
        );
    }

    #[test]
    fn push_tool_result_exactly_max_no_truncation() {
        const MAX: usize = 5; // matches MAX_TOOL_OUTPUT_LINES in tool.rs
        let mut chat = test_chat();
        let output: Vec<_> = (0..MAX).map(|i| format!("line {i}")).collect();
        chat.push_tool_result("result", &output.join("\n"), false);
        let text = all_text(&chat);
        assert!(
            !text.contains("... +"),
            "no truncation summary expected: {text}"
        );
    }

    #[test]
    fn push_tool_result_one_over_max_shows_singular_line() {
        const MAX: usize = 5;
        let mut chat = test_chat();
        let output: Vec<_> = (0..=MAX).map(|i| format!("line {i}")).collect();
        chat.push_tool_result("result", &output.join("\n"), false);
        let text = all_text(&chat);
        assert!(text.contains("... +1 line"));
        assert!(!text.contains("lines"), "singular 'line' expected: {text}");
    }

    #[test]
    fn push_tool_result_long_line_is_truncated() {
        const MAX_CHARS: usize = 512;
        let mut chat = test_chat();
        let long_line = "x".repeat(MAX_CHARS + 100);
        chat.push_tool_result("result", &long_line, false);
        let text = all_text(&chat);
        assert!(text.contains("..."), "long line should be truncated");
        assert!(
            !text.contains(&long_line),
            "full long line should not appear"
        );
    }

    // ── push_error ──

    #[test]
    fn push_error_shows_error_indicator() {
        let mut chat = test_chat();
        chat.push_error("something broke");
        let text = all_text(&chat);
        assert!(text.contains(TOOL_ERROR));
        assert!(text.contains("something broke"));
        assert!(
            !text.contains(BAR),
            "error block should render without the left bar: {text}"
        );
    }

    // ── clear_history ──

    #[test]
    fn clear_history_drops_blocks_streaming_thinking_and_resets_scroll() {
        let mut chat = test_chat();
        chat.push_user_message("user prompt".to_owned());
        chat.push_tool_call("$", "ls");
        chat.append_stream_token("partial reply");
        chat.append_thinking_token("considering");
        chat.scroll_offset = 25;
        chat.content_height.set(100);
        chat.auto_scroll = false;
        chat.viewport_height = 24;
        chat.viewport_width = 80;

        chat.clear_history();

        assert_eq!(chat.entry_count(), 0);
        assert!(chat.streaming.is_none());
        assert!(chat.thinking_buffer.is_empty());
        assert_eq!(chat.scroll_offset, 0);
        assert_eq!(chat.content_height.get(), 0);
        assert!(chat.auto_scroll);
        assert_eq!(chat.viewport_height, 24);
        assert_eq!(chat.viewport_width, 80);
    }

    // ── last_is_error ──

    #[test]
    fn last_is_error_true_after_push_error() {
        let mut chat = test_chat();
        chat.push_error("boom");
        assert!(chat.last_is_error());
    }

    #[test]
    fn last_is_error_false_for_non_error_blocks() {
        let mut chat = test_chat();
        chat.push_user_message("hello".into());
        assert!(!chat.last_is_error());

        chat.push_tool_call("$", "ls");
        assert!(!chat.last_is_error());

        chat.push_tool_result("failed", "boom", true);
        assert!(
            !chat.last_is_error(),
            "failed tool result is not an error marker"
        );
    }

    #[test]
    fn last_is_error_false_when_no_blocks() {
        let chat = test_chat();
        assert!(!chat.last_is_error());
    }

    // ── last_system_text ──

    #[test]
    fn last_system_text_produces_body_for_system_message() {
        let mut chat = test_chat();
        chat.push_system_message("hello there");
        assert_eq!(chat.last_system_text(), Some("hello there"));
    }

    #[test]
    fn last_system_text_default_is_none_for_non_system_blocks() {
        let mut chat = test_chat();
        chat.push_user_message("a user prompt".into());
        assert_eq!(chat.last_system_text(), None);

        chat.push_tool_call("$", "ls");
        assert_eq!(chat.last_system_text(), None);

        chat.push_error("boom");
        assert_eq!(chat.last_system_text(), None);
    }

    #[test]
    fn last_system_text_none_when_no_blocks() {
        let chat = test_chat();
        assert_eq!(chat.last_system_text(), None);
    }

    // ── update_layout ──

    #[test]
    fn update_layout_sets_viewport_height() {
        let mut chat = test_chat();
        _ = chat.update_layout(Rect::new(0, 0, 80, 30));
        assert_eq!(chat.viewport_height, 30);
    }

    #[test]
    fn update_layout_auto_scrolls_when_enabled() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.auto_scroll = true;

        let moved = chat.update_layout(Rect::new(0, 0, 80, 20));
        assert_eq!(chat.scroll_offset, 80);
        assert!(moved);
    }

    #[test]
    fn update_layout_is_false_when_offset_unchanged() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.auto_scroll = true;
        let area = Rect::new(0, 0, 80, 20);
        assert!(chat.update_layout(area));
        assert!(!chat.update_layout(area));
    }

    #[test]
    fn update_layout_paused_skips_scroll_and_keeps_offset() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.scroll_offset = 25;
        chat.auto_scroll = false;
        let moved = chat.update_layout(Rect::new(0, 0, 80, 20));
        assert!(!moved);
        assert_eq!(chat.scroll_offset, 25);
        assert_eq!(chat.viewport_height, 20);
    }

    #[test]
    fn update_layout_invalidates_streaming_cache_on_width_change() {
        let mut chat = test_chat();
        _ = chat.update_layout(Rect::new(0, 0, 80, 24));
        chat.append_stream_token("a complete paragraph\n\n");
        let s = chat.streaming.as_ref().unwrap();
        assert_ne!(s.rendered_len(), 0);
        assert_eq!(s.cached_width(), 80);

        _ = chat.update_layout(Rect::new(0, 0, 40, 24));
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_len(), 0);
        assert_eq!(s.rendered_boundary(), 0);
        assert_eq!(s.cached_width(), 0);
    }

    // ── handle_event ──

    #[test]
    fn handle_event_arrow_up_scrolls_up() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 10;

        chat.handle_event(&key_event(KeyCode::Up));
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

        chat.handle_event(&key_event(KeyCode::Down));
        assert_eq!(chat.scroll_offset, 11);
    }

    #[test]
    fn handle_event_mouse_scroll_up() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 10;

        chat.handle_event(&mouse_scroll(MouseEventKind::ScrollUp));
        assert_eq!(chat.scroll_offset, 9);
    }

    #[test]
    fn handle_event_mouse_scroll_down() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 10;
        chat.auto_scroll = false;

        chat.handle_event(&mouse_scroll(MouseEventKind::ScrollDown));
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
    fn handle_event_unhandled_key_leaves_state_unchanged() {
        let mut chat = test_chat();
        chat.content_height.set(100);
        chat.viewport_height = 20;
        chat.scroll_offset = 10;

        chat.handle_event(&key_event(KeyCode::Char('a')));
        assert_eq!(chat.scroll_offset, 10);
    }

    // ── render ──

    #[test]
    fn render_updates_content_height() {
        let mut chat = test_chat();
        render_chat(&mut chat, 80, 24);
        assert_eq!(chat.content_height.get(), 4);
    }

    #[test]
    fn render_empty_shows_welcome_screen() {
        let mut chat = test_chat();
        insta::assert_snapshot!(render_chat(&mut chat, 60, 8));
    }

    #[test]
    fn render_user_and_assistant_interleaved() {
        let mut chat = test_chat();
        chat.push_user_message("what is 2 + 2?".into());
        chat.append_stream_token("The answer is 4.");
        chat.commit_streaming();
        insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
    }

    #[test]
    fn render_tool_call_followed_by_result() {
        let mut chat = test_chat();
        chat.push_tool_call("$", "echo hi");
        chat.push_tool_result("ran echo", "hi", false);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
    }

    #[test]
    fn render_tool_call_with_edit_diff_result() {
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/f.rs)");
        let view = crate::tool::ToolResultView::Diff {
            chunks: vec![crate::tool::edit::synthesize_chunk(
                "fn foo() {}",
                "fn foo() -> i32 { 42 }",
            )],
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited f.rs", view, false);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
    }

    #[test]
    fn render_tool_call_with_edit_diff_over_budget_truncates_both_sides() {
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/big.rs)");
        let old = (0..14)
            .map(|i| format!("old{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..14)
            .map(|i| format!("new{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = crate::tool::ToolResultView::Diff {
            chunks: vec![crate::tool::edit::synthesize_chunk(&old, &new)],
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited big.rs", view, false);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 26));
    }

    #[test]
    fn render_tool_call_with_edit_diff_error_uses_error_border_color() {
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/f.rs)");
        let view = crate::tool::ToolResultView::Diff {
            chunks: vec![crate::tool::edit::synthesize_chunk(
                "fn foo() {}",
                "fn foo() -> i32 { 42 }",
            )],
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited f.rs", view, true);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
    }

    #[test]
    fn render_tool_call_with_edit_diff_identical_sides_emits_no_change_marker() {
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/f.rs)");
        let view = crate::tool::ToolResultView::Diff {
            chunks: vec![crate::tool::edit::synthesize_chunk(
                "unchanged",
                "unchanged",
            )],
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited f.rs", view, false);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 6));
    }

    #[test]
    fn render_tool_call_with_edit_diff_trims_identical_boundary_lines() {
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/f.rs)");
        let view = crate::tool::ToolResultView::Diff {
            chunks: vec![crate::tool::edit::synthesize_chunk(
                "fn foo()",
                "fn foo()\n    return 42;",
            )],
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited f.rs", view, false);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 8));
    }

    #[test]
    fn render_tool_call_with_edit_diff_wraps_long_lines_under_bar() {
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/f.rs)");
        let view = crate::tool::ToolResultView::Diff {
            chunks: vec![crate::tool::edit::synthesize_chunk(
                "short",
                "an intentionally quite long replacement line that forces wrapping",
            )],
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited f.rs", view, false);
        insta::assert_snapshot!(render_chat(&mut chat, 40, 10));
    }

    #[test]
    fn render_tool_result_overflow_shows_line_count() {
        let mut chat = test_chat();
        chat.push_tool_call("$", "ls");
        let long = (0..5 + 3)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        chat.push_tool_result("ls out", &long, false);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 14));
    }

    #[test]
    fn render_error_entry_is_styled_distinctly() {
        let mut chat = test_chat();
        chat.push_error("API error (HTTP 503): overloaded");
        insta::assert_snapshot!(render_chat(&mut chat, 60, 4));
    }

    #[test]
    fn render_history_with_resumed_thinking_block() {
        let mut chat = ChatView::new(&Theme::default(), true);
        let tools = test_tools();
        let history = vec![
            Message::user("hello"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "pondering...".into(),
                        signature: "sig".into(),
                    },
                    ContentBlock::Text { text: "Hi!".into() },
                ],
            },
        ];
        chat.load_history(&history, &HashMap::new(), &tools);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
    }

    // ── scroll_to_bottom / scroll_up / scroll_down ──

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
        chat.blocks
            .push(Box::new(AssistantText::new("The answer is 4.")));
        chat.push_tool_call("$", "python -c 'print(2+2)'");
        chat.push_tool_result("4", "4", false);
        chat.push_user_message("Thanks!".to_owned());
        chat.append_stream_token("You're welcome");

        let text = all_text(&chat);
        assert!(text.contains("What is 2+2?"));
        assert!(text.contains("The answer is 4."));
        assert!(text.contains("python -c 'print(2+2)'"));
        assert!(text.contains("You're welcome"));
        assert_eq!(text.matches('❯').count(), 2);
    }

    // ── advance_streaming_cache ──

    #[test]
    fn advance_streaming_cache_no_newline_keeps_boundary_zero() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("no newline here");
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), 0);
        assert_eq!(s.rendered_len(), 0);
    }

    #[test]
    fn advance_streaming_cache_line_boundary_does_not_commit() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("first line\nsecond line");
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), 0);
        assert_eq!(s.rendered_len(), 0);
    }

    #[test]
    fn advance_streaming_cache_paragraph_boundary_commits() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("para1\n\npartial");
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), "para1\n\n".len());
        assert_eq!(s.rendered_len(), 1);
    }

    #[test]
    fn advance_streaming_cache_multiple_paragraphs_commit_to_last_break() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("p1\n\np2\n\np3\n\npartial");
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), "p1\n\np2\n\np3\n\n".len());
    }

    #[test]
    fn advance_streaming_cache_incremental_inserts_paragraph_gaps() {
        let mut chat = test_chat();
        chat.viewport_width = 80;

        chat.append_stream_token("para1\n\n");
        let (first_boundary, first_len) = {
            let s = chat.streaming.as_ref().unwrap();
            (s.rendered_boundary(), s.rendered_len())
        };
        assert_eq!(first_boundary, "para1\n\n".len());
        assert!(first_len >= 1);

        chat.append_stream_token("para2\n\n");
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), "para1\n\npara2\n\n".len());
        let final_len = s.rendered_len();
        assert!(
            final_len >= first_len + 2,
            "paragraph break must add >= 2 lines (blank + body): got {first_len} → {final_len}",
        );
    }

    #[test]
    fn advance_streaming_cache_trailing_paragraph_break_only() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("\n\n");
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), 2);
        assert_eq!(s.rendered_len(), 0);
    }

    #[test]
    fn advance_streaming_cache_defers_until_viewport_measured() {
        let mut chat = test_chat();
        chat.append_stream_token("first paragraph\n\n");
        {
            let s = chat.streaming.as_ref().unwrap();
            assert_eq!(s.rendered_len(), 0);
            assert_eq!(s.rendered_boundary(), 0);
            assert_eq!(s.cached_width(), 0);
        }

        _ = chat.update_layout(Rect::new(0, 0, 80, 24));
        chat.append_stream_token("second paragraph\n\n");
        {
            let s = chat.streaming.as_ref().unwrap();
            assert_ne!(s.rendered_len(), 0);
            assert_eq!(s.cached_width(), 80);
        }
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
}
