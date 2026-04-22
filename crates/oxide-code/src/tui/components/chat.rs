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
use std::collections::HashMap;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use self::blocks::{
    AssistantText, AssistantThinking, ChatBlock, ErrorBlock, RenderCtx, StreamingAssistant,
    ToolCallBlock, ToolResultBlock, UserMessage,
};
use crate::agent::event::UserAction;
use crate::message::Message;
use crate::session::history::{Interaction, walk_transcript};
use crate::tool::ToolRegistry;
use crate::tui::component::Component;
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
        let mut labels: HashMap<&str, String> = HashMap::new();
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
                    let label = tools
                        .summarize_input(name, input)
                        .map_or_else(|| name.to_owned(), str::to_owned);
                    labels.insert(id, label.clone());
                    self.blocks.push(Box::new(ToolCallBlock::new(icon, label)));
                }
                Interaction::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let label = labels
                        .get(tool_use_id)
                        .cloned()
                        .unwrap_or_else(|| "(result)".to_owned());
                    self.blocks
                        .push(Box::new(ToolResultBlock::new(label, content, is_error)));
                }
                Interaction::OrphanToolResult { content, is_error } => {
                    self.blocks.push(Box::new(ToolResultBlock::new(
                        "(result)", content, is_error,
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
    pub(crate) fn push_tool_call(&mut self, icon: &'static str, label: &str) {
        self.blocks.push(Box::new(ToolCallBlock::new(icon, label)));
    }

    /// Appends a tool result summary line with optional output content.
    pub(crate) fn push_tool_result(&mut self, label: &str, content: &str, is_error: bool) {
        self.blocks
            .push(Box::new(ToolResultBlock::new(label, content, is_error)));
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
    /// on error dispatch without depending on the block module's
    /// internals. Uses downcasting via a marker trait isn't available,
    /// so we introspect the last block's render output for the error
    /// indicator.
    #[cfg(test)]
    pub(crate) fn last_is_error(&self) -> bool {
        self.blocks.last().is_some_and(|b| {
            let ctx = RenderCtx {
                width: 80,
                theme: &self.theme,
                show_thinking: self.show_thinking,
            };
            b.render(&ctx)
                .iter()
                .any(|l| l.spans.iter().any(|s| s.content.contains('✗')))
        })
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

        // Live thinking (transient — not stored in blocks).
        if self.show_thinking && !self.thinking_buffer.is_empty() {
            let thinking = AssistantThinking::new(self.thinking_buffer.clone());
            if !lines.is_empty() && last_has_width(&lines) {
                lines.push(Line::raw(""));
            }
            lines.extend(thinking.render(&ctx));
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

fn last_has_width(lines: &[Line<'_>]) -> bool {
    lines.last().is_some_and(|l| l.width() > 0)
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
mod tests;
