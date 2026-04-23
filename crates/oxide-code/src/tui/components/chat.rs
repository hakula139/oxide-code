//! Chat view — the scrollable message list.
//!
//! Each visible unit in the transcript (user messages, assistant
//! replies, tool calls and results, errors) is a [`blocks::ChatBlock`]
//! implementation. [`ChatView`] is the thin container: it appends
//! blocks, owns the streaming buffer, handles scroll state, and stacks
//! `render` outputs with appropriate blank-line separators.
//!
//! Adding a new block type — plan approval, task list, permission
//! prompt, skill invocation — means writing a new `impl ChatBlock`
//! module. No cascade through a giant match, no prefix-constant
//! editing spree.

mod blocks;

use std::cell::Cell;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use self::blocks::{
    AssistantText, AssistantThinking, ChatBlock, ErrorBlock, RenderCtx, StreamingAssistant,
    ToolCallBlock, ToolResultBlock, UserMessage, last_has_width,
};
use crate::agent::event::UserAction;
use crate::message::Message;
use crate::session::history::{Interaction, walk_transcript};
use crate::tool::{ToolRegistry, ToolResultView};
use crate::tui::component::Component;
use crate::tui::pending_calls::{FALLBACK_RESULT_HEADER, PendingCall, PendingCalls};
use crate::tui::theme::Theme;

/// Scrollable chat message list with markdown rendering, tool call
/// display, and thinking block support.
///
/// Renders blocks vertically and auto-scrolls to the bottom on new
/// content. The user can scroll up to review history; new content
/// pauses auto-scroll until the user scrolls back to the bottom.
pub(crate) struct ChatView {
    // Config
    theme: Theme,
    show_thinking: bool,

    // Committed blocks
    blocks: Vec<Box<dyn ChatBlock>>,

    // Transient state (cleared per turn)
    /// In-flight assistant tokens with a rendered-prefix cache.
    streaming: Option<StreamingAssistant>,
    /// Live thinking tokens — transient; cleared when a stream token or
    /// turn completion arrives. Resumed thinking comes through
    /// [`blocks`] as an [`AssistantThinking`] block instead.
    thinking_buffer: String,

    // View state
    scroll_offset: u16,
    /// Total content height from the last render (for scroll bounds).
    /// `Cell` for interior mutability so `render` (`&self`) can update
    /// it during the render pass without a second `build_text` call.
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

    /// Populate the chat from resumed session messages.
    ///
    /// Projects the transcript into [`Interaction`]s so the resumed view
    /// matches live rendering — paired tool calls and results appear
    /// together, orphan results get a fallback label, and
    /// `RedactedThinking` / whitespace-only blocks are dropped upstream.
    /// Thinking blocks are always pushed; [`AssistantThinking::visible`]
    /// collapses them to zero when `show_thinking` is off, so flipping
    /// the toggle at runtime doesn't require reloading the session.
    pub(crate) fn load_history(&mut self, messages: &[Message], tools: &ToolRegistry) {
        let mut pending = PendingCalls::new();
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
                    // [`walk_transcript`] emits `ToolResult` only
                    // inline right after its paired `ToolCall` —
                    // unpaired ids surface through `OrphanToolResult`
                    // instead — so the lookup is total.
                    let p = pending
                        .remove(tool_use_id)
                        .expect("walk_transcript pairs every ToolResult with its ToolCall");
                    let view = tools.result_view(&p.name, &p.input, content, is_error);
                    self.blocks
                        .push(Box::new(ToolResultBlock::new(p.label, view, is_error)));
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

    /// Appends a user message to the chat.
    pub(crate) fn push_user_message(&mut self, text: String) {
        self.blocks.push(Box::new(UserMessage::new(text)));
        self.auto_scroll = true;
    }

    /// Appends a streamed token to the current assistant response.
    pub(crate) fn append_stream_token(&mut self, token: &str) {
        if !self.thinking_buffer.is_empty() {
            self.thinking_buffer.clear();
        }
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

    /// Finalize the current streaming buffer into a committed assistant
    /// block.
    pub(crate) fn commit_streaming(&mut self) {
        self.thinking_buffer.clear();
        if let Some(mut s) = self.streaming.take() {
            let text = s.take_buffer();
            if !text.is_empty() {
                self.blocks.push(Box::new(AssistantText::new(text)));
            }
        }
    }

    /// Appends a tool call entry with its icon and label.
    ///
    /// Finalizes any in-flight streaming buffer first — a tool call
    /// implicitly ends the current assistant turn's text, so callers
    /// don't need to remember to `commit_streaming()` beforehand.
    pub(crate) fn push_tool_call(&mut self, icon: &'static str, label: &str) {
        self.commit_streaming();
        self.blocks.push(Box::new(ToolCallBlock::new(icon, label)));
    }

    /// Appends a tool result with a pre-built structured view. Used
    /// by [`App::handle_agent_event`](super::super::app::App::handle_agent_event)
    /// (which builds the view from the cached tool name + input) and
    /// by [`load_history`](Self::load_history) when resuming sessions.
    pub(crate) fn push_tool_result_view(
        &mut self,
        label: &str,
        view: ToolResultView,
        is_error: bool,
    ) {
        self.blocks
            .push(Box::new(ToolResultBlock::new(label, view, is_error)));
    }

    /// Test shortcut for the `Text` variant — production callers
    /// route through [`push_tool_result_view`](Self::push_tool_result_view).
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

    /// Number of committed chat blocks. Exposed for observable state in
    /// sibling-module tests (`tui::app`) so they don't need to reach
    /// through the private `blocks` field.
    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        self.blocks.len()
    }

    /// Whether the tail block is an [`ErrorBlock`]. Same rationale as
    /// [`entry_count`][Self::entry_count] — lets `tui::app` tests assert
    /// on error dispatch without reaching through the private `blocks`
    /// field or the block module's internals.
    #[cfg(test)]
    pub(crate) fn last_is_error(&self) -> bool {
        self.blocks.last().is_some_and(|b| b.is_error_marker())
    }

    /// Updates cached viewport height and syncs scroll position. Called
    /// by [`App`](super::super::app::App) after each frame.
    pub(crate) fn update_layout(&mut self, area: Rect) {
        self.viewport_height = area.height;
        self.viewport_width = area.width;
        if let Some(s) = &mut self.streaming {
            s.invalidate_cache_for_width(area.width);
        }
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }
}

impl Component for ChatView {
    fn handle_event(&mut self, event: &Event) -> Option<UserAction> {
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

        // Committed blocks.
        for block in &self.blocks {
            if !block.visible(&ctx) {
                continue;
            }
            if block.standalone() && !lines.is_empty() && last_has_width(&lines) {
                lines.push(Line::raw(""));
            }
            lines.extend(block.render(&ctx));
            if block.standalone() {
                lines.push(Line::raw(""));
            }
        }

        // Live thinking (transient — not stored in blocks). Visibility
        // lives in `AssistantThinking::visible`, same contract as the
        // committed-blocks loop above.
        if !self.thinking_buffer.is_empty() {
            let thinking = AssistantThinking::new(self.thinking_buffer.clone());
            if thinking.visible(&ctx) {
                if !lines.is_empty() && last_has_width(&lines) {
                    lines.push(Line::raw(""));
                }
                lines.extend(thinking.render(&ctx));
            }
        }

        // Streaming assistant tail (not yet committed).
        if let Some(streaming) = &self.streaming {
            let continues = self.streaming_continues_turn();
            streaming.render_into(&mut lines, &ctx, continues);
        }

        Text::from(lines)
    }

    fn advance_streaming_cache(&mut self) {
        let continues = self.streaming_continues_turn();
        let width = self.viewport_width;
        let theme = self.theme;
        let show_thinking = self.show_thinking;
        if let Some(s) = &mut self.streaming {
            let ctx = RenderCtx {
                width,
                theme: &theme,
                show_thinking,
            };
            s.advance_cache(&ctx, continues);
        }
    }

    /// Whether streaming tokens continue the last committed assistant
    /// turn. `false` when the preceding block is anything other than
    /// assistant text (user message, tool entry, error) — in which case
    /// streaming starts a fresh turn with its own icon and gap.
    fn streaming_continues_turn(&self) -> bool {
        self.blocks
            .last()
            .is_some_and(|b| b.continues_assistant_turn())
    }

    fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.streaming.is_none() && self.thinking_buffer.is_empty()
    }
}

/// Welcome splash for an empty chat: two blank lines + centered title +
/// centered subtitle.
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use indoc::indoc;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use unicode_width::UnicodeWidthStr;

    use super::*;
    use crate::message::{ContentBlock, Role};

    // ── Fixtures ──

    fn test_chat() -> ChatView {
        ChatView::new(Theme::default(), true)
    }

    fn test_tools() -> ToolRegistry {
        ToolRegistry::new(vec![
            Box::new(crate::tool::bash::BashTool),
            Box::new(crate::tool::read::ReadTool),
            Box::new(crate::tool::write::WriteTool),
            Box::new(crate::tool::edit::EditTool),
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
        chat.update_layout(Rect::new(0, 0, width, height));
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
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 2);
        let text = all_text(&chat);
        assert!(text.contains("hello"));
        assert!(text.contains("hi there"));
    }

    #[test]
    fn load_history_multi_tool_turn_pairs_inline_with_orphan_fallback() {
        // Live rendering pairs Call → Result inline. The resumed walk
        // must preserve that order regardless of JSONL batching; an
        // orphan result ("ghost", no matching call) surfaces at its
        // original position with the "(result)" fallback label.
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
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 5);
        let text = all_text(&chat);
        // Order: call(a.rs), result(a.rs)=file a, call(TODO), result(TODO)=3 matches, orphan=stale
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
        // Orphan tool_result — possible after crash sanitization. Render
        // with a generic fallback rather than dropping.
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
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 1);
        let text = all_text(&chat);
        assert!(text.contains("(result)"));
        assert!(text.contains("stderr"));
        assert!(text.contains('✗'));
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
            &test_tools(),
        );
        assert_eq!(chat.blocks.len(), 1);
        let text = all_text(&chat);
        assert!(text.contains("first"));
        assert!(text.contains("second"));
        // Assistant text renders bar-less after the redesign; load_history
        // is a natural place to pin this without a standalone test.
        assert!(
            !text.contains('▎'),
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
            &test_tools(),
        );
        assert!(chat.blocks.is_empty());
    }

    #[test]
    fn load_history_empty_slice_is_noop() {
        let mut chat = test_chat();
        chat.load_history(&[], &test_tools());
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
            &test_tools(),
        );
        let text = all_text(&chat);
        assert!(text.contains("Thinking..."));
        assert!(text.contains("resumed reasoning"));
    }

    #[test]
    fn load_history_hides_resumed_thinking_when_show_thinking_disabled() {
        let mut chat = ChatView::new(Theme::default(), false);
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
        // Bar redesign: user messages render bar-less. A regressed snapshot
        // that re-adds `▎` would land silently without this invariant.
        assert!(
            !text.contains('▎'),
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
    fn append_stream_token_clears_thinking() {
        let mut chat = test_chat();
        chat.append_thinking_token("thinking...");
        assert!(!chat.thinking_buffer.is_empty());

        chat.append_stream_token("text");
        assert!(chat.thinking_buffer.is_empty());
    }

    #[test]
    fn append_stream_token_shows_partial_text() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.append_stream_token("partial response");
        let text = all_text(&chat);
        assert!(text.contains('◉'), "should show assistant icon");
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
        assert!(text.contains('◉'), "new turn should show assistant icon");
    }

    #[test]
    fn append_stream_token_after_committed_assistant_omits_duplicate_icon() {
        let mut chat = test_chat();
        chat.blocks.push(Box::new(AssistantText::new("committed")));
        // Push streaming directly — simulates a continued turn.
        let mut s = StreamingAssistant::new();
        s.append("streaming");
        chat.streaming = Some(s);

        let text = all_text(&chat);
        let count = text.matches('◉').count();
        assert_eq!(count, 1, "icon should appear once, not duplicated");
    }

    #[test]
    fn append_stream_token_inserts_blank_separator_after_tool_output() {
        // When streaming tokens arrive after a non-standalone block (tool
        // call / tool result / error — no trailing blank of its own), the
        // streaming block must insert its own leading blank so the icon
        // doesn't sit flush against the preceding line.
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
        // With viewport_width = 0, advance_cache no-ops, so the streaming
        // buffer accumulates newlines that rfind('\n') inside render_into
        // then splits on first paint. Covers the Some(nl) match arm plus
        // the `!committed.is_empty()` branch that an advance-cache-first
        // flow skips.
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.append_stream_token("cached line\ntail text");
        // Pre-check the invariant that makes this test meaningful: cache
        // deferred because viewport wasn't measured.
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
        // Trailing newline with viewport_width = 0: `advance_cache` defers,
        // so `render_into` sees a tail that ends in `\n`. The rfind split
        // gives committed = "line1\nline2" and trailing = "" — this is the
        // fall-through where `if !trailing.is_empty()` is false.
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
        // The user-visible paragraph-spacing bug: committing chunk-
        // by-chunk on `\n` boundaries fed pulldown-cmark fragments
        // that each rendered as a standalone paragraph, losing the
        // inter-paragraph blank. Mid-stream view ended up collapsed
        // vs. the post-commit view. Pin the expected shape here —
        // two committed paragraphs with a blank line between them.
        let mut chat = test_chat();
        chat.viewport_width = 80;
        // First chunk ends a paragraph; second chunk starts a new
        // one. Both must sit in the cache when the next token lands
        // because each `advance_cache` call committed past a `\n\n`.
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
        // Guard against over-inserting: when the committed tail ends
        // with a list item and the trailing starts another one, they
        // share a block type and must render adjacent (tight list).
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
        // Mid-stream, a list item that arrives before the paragraph's
        // `\n\n` terminator gets rendered as a raw trailing fragment
        // (not through pulldown-cmark). Without an explicit block gap
        // the bullet visually glues to the preceding paragraph until
        // the next `\n` lands — pin the expected separator here.
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
        // Same invariant at the cache / live-tail seam: a committed
        // paragraph followed by a partially-typed next paragraph
        // must show the blank gap even while the new paragraph is
        // still streaming (pre-`\n\n`).
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
        let mut chat = ChatView::new(Theme::default(), false);
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
        // Live thinking pushes a leading blank when the tail block has no
        // trailing blank of its own. Tool call is the natural example —
        // standalone = false, no trail blank, so the thinking header needs
        // its own separator.
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

    // ── push_tool_call ──

    #[test]
    fn push_tool_call_shows_icon_and_label() {
        let mut chat = test_chat();
        chat.push_tool_call("$", "ls -la");
        let text = all_text(&chat);
        assert!(text.contains('$'));
        assert!(text.contains("ls -la"));
        // Bar is preserved specifically on tool call / tool result — it
        // visually groups the call with its output and color-codes status.
        assert!(
            text.contains('▎'),
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

    // ── push_tool_result ──

    #[test]
    fn push_tool_result_success() {
        let mut chat = test_chat();
        chat.push_tool_result("done", "output text", false);
        let text = all_text(&chat);
        assert!(text.contains("✓"));
        assert!(text.contains("done"));
        assert!(text.contains("output text"));
    }

    #[test]
    fn push_tool_result_error() {
        let mut chat = test_chat();
        chat.push_tool_result("failed", "error details", true);
        let text = all_text(&chat);
        assert!(text.contains("✗"));
        assert!(text.contains("failed"));
        assert!(text.contains("error details"));
        // The bar color is the status channel: neutral on success, error
        // on failure. Walk the rendered spans and pin that the `▎` span
        // carries the theme's error style here — a bug swapping the
        // tool_border / error branches in `border_style_for` would flip
        // success and error results identically otherwise.
        let rendered = chat.build_text(60);
        let bar_style = rendered
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains('▎'))
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
        // Body-indent invariant: output lines sit at col 4 (`▎   `), past
        // the `✓`/`✗` header at col 2. Anchored to the exact prefix so a
        // regression collapsing back to col 2 fails here, not just in
        // snapshots.
        assert!(
            text.contains("▎   line 0"),
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
        // Grep and glob both set `title = "Found N files"` as the
        // status-line label and emit the same string as the first
        // line of `content` for the model's context. Rendering both
        // duplicates it on screen; skip the first body line when it
        // matches the label verbatim.
        let mut chat = test_chat();
        chat.push_tool_result("Found 2 files", "Found 2 files\na.rs\nb.rs", false);
        let text = all_text(&chat);
        // Only the status line carries "Found 2 files" — the body
        // starts at the file list.
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
        // Body's first line only gets dropped when it exactly matches
        // the label. A superficially similar prefix ("Found 2 files"
        // vs "Found 2 files in cache") must render both — the label
        // is a distinct header.
        let mut chat = test_chat();
        chat.push_tool_result("Found 2 files", "Found 2 files in cache\na.rs", false);
        let text = all_text(&chat);
        assert!(
            text.contains("Found 2 files in cache"),
            "body preserved: {text}"
        );
    }

    #[test]
    fn push_tool_result_dedup_collapses_body_when_only_line_matches_label() {
        // When content is just the duplicated label (no trailing body
        // lines), rendering collapses to a bare status line.
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

    // ── push_tool_result_view ──

    #[test]
    fn push_tool_result_view_edit_renders_diff_markers() {
        // An Edit tool result wired through the structured view should
        // render the replaced text with `- ` for the old side and `+ `
        // for the new side, not the default 5-line truncation body.
        let mut chat = test_chat();
        let view = crate::tool::ToolResultView::Diff {
            old: "fn foo()".to_owned(),
            new: "fn bar()".to_owned(),
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited file.rs", view, false);
        let text = all_text(&chat);
        assert!(text.contains("- fn foo()"), "old side missing: {text}");
        assert!(text.contains("+ fn bar()"), "new side missing: {text}");
        // Diff rendering replaces the default body — the "Successfully
        // edited" message must not leak through.
        assert!(
            !text.contains("Successfully edited"),
            "diff should replace the raw content body: {text}",
        );
        // Single-replacement edits must not emit the
        // "N occurrences replaced" footer — closes an `&&` → `||`
        // mutation on the guard in `render_diff_body`.
        assert!(
            !text.contains("occurrences replaced"),
            "replace_all=false should suppress the match-count footer: {text}",
        );
    }

    #[test]
    fn push_tool_result_view_edit_replace_all_shows_match_count() {
        let mut chat = test_chat();
        let view = crate::tool::ToolResultView::Diff {
            old: "a".to_owned(),
            new: "b".to_owned(),
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
    fn push_tool_result_view_edit_single_replacement_hides_count_footer() {
        // The `applied to N matches` footer only makes sense when the
        // edit actually multiplied. A single replacement (either
        // replace_all=false or replace_all=true with one match) should
        // render a clean diff with no count footer.
        let mut chat = test_chat();
        let view = crate::tool::ToolResultView::Diff {
            old: "a".to_owned(),
            new: "b".to_owned(),
            replace_all: true,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited file.rs", view, false);
        let text = all_text(&chat);
        assert!(
            !text.contains("applied to"),
            "single-replacement footer should be suppressed: {text}",
        );
    }

    // ── push_error ──

    #[test]
    fn push_error_shows_error_indicator() {
        let mut chat = test_chat();
        chat.push_error("something broke");
        let text = all_text(&chat);
        assert!(text.contains("✗"));
        assert!(text.contains("something broke"));
        assert!(
            !text.contains('▎'),
            "error block should render without the left bar: {text}"
        );
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
        // Exercises the `ChatBlock::is_error_marker` default impl on every
        // non-error variant. A failed tool result also renders a ✗ but
        // `is_error_marker` stays `false` — the predicate is about block
        // identity, not rendered glyphs.
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

    #[test]
    fn update_layout_invalidates_streaming_cache_on_width_change() {
        let mut chat = test_chat();
        chat.update_layout(Rect::new(0, 0, 80, 24));
        // Full paragraph (ends in `\n\n`) so the cache actually
        // commits — a single `\n` no longer triggers advance_cache.
        chat.append_stream_token("a complete paragraph\n\n");
        let s = chat.streaming.as_ref().unwrap();
        assert_ne!(s.rendered_len(), 0);
        assert_eq!(s.cached_width(), 80);

        chat.update_layout(Rect::new(0, 0, 40, 24));
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
        let mut chat = test_chat();
        render_chat(&mut chat, 80, 24);
        // Welcome screen: 2 blank lines + title + subtitle = 4 lines.
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
        // Per-tool rendering: the Edit tool's result renders as a
        // `-` / `+` diff body, not the default truncated text block.
        // Pins the first structured-view override so a regression
        // routing Edit through `Text` again shows up here.
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/f.rs)");
        let view = crate::tool::ToolResultView::Diff {
            old: "fn foo() {}".to_owned(),
            new: "fn foo() -> i32 { 42 }".to_owned(),
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited f.rs", view, false);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
    }

    #[test]
    fn render_tool_call_with_edit_diff_over_budget_truncates_both_sides() {
        // Pins the symmetric truncation policy: when the combined line
        // count exceeds `MAX_DIFF_BODY_LINES`, each side renders with
        // a head + ellipsis + tail shape. Regressions that revert to
        // the old asymmetric policy — or that emit a bogus
        // `... +0 lines` footer on pure deletion — show up here.
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/big.rs)");
        let old = (0..8)
            .map(|i| format!("old{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..8)
            .map(|i| format!("new{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = crate::tool::ToolResultView::Diff {
            old,
            new,
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited big.rs", view, false);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 16));
    }

    #[test]
    fn render_tool_call_with_edit_diff_error_uses_error_border_color() {
        // Error-path Edit (e.g., `old_string` didn't match) still
        // renders through the Diff view but with the error-colored
        // border on the result row. Pins the `is_error = true`
        // branch in `render_diff_body` — all other diff snapshots
        // exercise the success border, so any theme regression on
        // the error path would otherwise slip through.
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/f.rs)");
        let view = crate::tool::ToolResultView::Diff {
            old: "fn foo() {}".to_owned(),
            new: "fn foo() -> i32 { 42 }".to_owned(),
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited f.rs", view, true);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
    }

    #[test]
    fn render_tool_call_with_edit_diff_identical_sides_emits_no_change_marker() {
        // Defensive guard in `render_diff_body`: when
        // `trim_common_boundaries` collapses both sides to empty
        // (old == new entirely — reachable on transcript replay
        // since `edit_file` rejects no-op edits live), emit a single
        // dim "(no change)" row so the user sees an explicit marker
        // instead of a bare success header that reads as "edit
        // applied, diff scrolled off".
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/f.rs)");
        let view = crate::tool::ToolResultView::Diff {
            old: "unchanged".to_owned(),
            new: "unchanged".to_owned(),
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited f.rs", view, false);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 6));
    }

    #[test]
    fn render_tool_call_with_edit_diff_trims_identical_boundary_lines() {
        // Pure tail insertion: the anchor line (`fn foo()`) is
        // identical on both sides and must NOT render as
        // `- fn foo()` / `+ fn foo()`. Pinned as a snapshot so the
        // trim regression would surface at the rendered-layout level,
        // not just in the unit test on `trim_common_boundaries`.
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/f.rs)");
        let view = crate::tool::ToolResultView::Diff {
            old: "fn foo()".to_owned(),
            new: "fn foo()\n    return 42;".to_owned(),
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited f.rs", view, false);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 8));
    }

    #[test]
    fn render_tool_call_with_edit_diff_wraps_long_lines_under_bar() {
        // Pins bar continuation under narrow widths — the `+` sigil
        // stays on the first visual row only; wrapped continuations
        // flush under the bar without repeating the sigil.
        let mut chat = test_chat();
        chat.push_tool_call("✎", "Edit(/tmp/f.rs)");
        let view = crate::tool::ToolResultView::Diff {
            old: "short".to_owned(),
            new: "an intentionally quite long replacement line that forces wrapping".to_owned(),
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
        let mut chat = ChatView::new(Theme::default(), true);
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
        chat.load_history(&history, &tools);
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
        // Two user messages → two user-icon prefixes.
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
        // Line boundaries mid-paragraph are not commit points — the
        // cache advances only when a full paragraph has arrived
        // (`\n\n`), so pulldown-cmark sees each committed chunk as a
        // complete block. A single `\n` inside a paragraph keeps the
        // buffer uncommitted and streaming-live.
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
        // The `\n\n` between para1 and "partial" is the commit point;
        // boundary lands past both newlines so subsequent advances
        // scan only the uncommitted tail.
        assert_eq!(s.rendered_boundary(), "para1\n\n".len());
        assert_eq!(s.rendered_len(), 1);
    }

    #[test]
    fn advance_streaming_cache_multiple_paragraphs_commit_to_last_break() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("p1\n\np2\n\np3\n\npartial");
        let s = chat.streaming.as_ref().unwrap();
        // Commit up to the last `\n\n` before the trailing live
        // fragment; p1, p2, p3 land in the cache in one render pass
        // so pulldown sees them as three consecutive blocks and emits
        // inter-paragraph blanks naturally.
        assert_eq!(s.rendered_boundary(), "p1\n\np2\n\np3\n\n".len());
    }

    #[test]
    fn advance_streaming_cache_incremental_inserts_paragraph_gaps() {
        let mut chat = test_chat();
        chat.viewport_width = 80;

        // First paragraph — cache empty, the render includes the `◉`
        // icon via render_assistant_markdown's `starts_new_turn`.
        chat.append_stream_token("para1\n\n");
        let (first_boundary, first_len) = {
            let s = chat.streaming.as_ref().unwrap();
            (s.rendered_boundary(), s.rendered_len())
        };
        assert_eq!(first_boundary, "para1\n\n".len());
        assert!(first_len >= 1);

        // Second paragraph — cache is non-empty, so advance_cache
        // prepends a blank separator before the new paragraph's
        // rendered lines. Total cache length grows by at least 2
        // (separator + paragraph line).
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
        // Empty (whitespace-only) commit — boundary still advances
        // past the `\n\n` so subsequent text isn't re-scanned, but
        // nothing lands in the cache.
        assert_eq!(s.rendered_boundary(), 2);
        assert_eq!(s.rendered_len(), 0);
    }

    #[test]
    fn advance_streaming_cache_defers_until_viewport_measured() {
        // Streaming before update_layout runs must not bake unwrapped
        // markdown into the cache. The cache stays empty until the
        // viewport width is supplied.
        let mut chat = test_chat();
        chat.append_stream_token("first paragraph\n\n");
        {
            let s = chat.streaming.as_ref().unwrap();
            assert_eq!(s.rendered_len(), 0);
            assert_eq!(s.rendered_boundary(), 0);
            assert_eq!(s.cached_width(), 0);
        }

        chat.update_layout(Rect::new(0, 0, 80, 24));
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
