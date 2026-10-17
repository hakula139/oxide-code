//! Scrollable chat view. Each visible transcript unit is a [`blocks::ChatBlock`] impl.
//! [`ChatView`] owns the streaming buffer, caches committed block renders, and stacks them with
//! separator lines, rendering only the blocks near the viewport.

mod blocks;
mod stack;

use std::ops::Range;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use self::blocks::{
    AssistantText, AssistantThinking, ChatBlock, CompactedBlock, ErrorBlock, GitDiffBlock,
    InterruptedMarker, NoticeBlock, ProgressUpdate, RenderCtx, StreamingAssistant,
    SystemMessageBlock, ToolCallBlock, ToolResultBlock, UserMessage,
};
use self::stack::{Anchor, CachedBlock, SlotTops, Stack};
use crate::protocol::{Interaction, ToolCall, ToolResult};
use crate::tui::pending_calls::{PendingCalls, result_header};
use crate::tui::theme::Theme;

/// Scrollable chat message list with auto-scroll.
pub(crate) struct ChatView {
    settings: RenderSettings,
    blocks: Vec<CachedBlock>,

    streaming: Option<StreamingAssistant>,
    thinking_buffer: String,
    /// The compaction summary streaming in, which shows below the chat until compaction ends.
    summary_buffer: String,

    /// Top row of the last paint.
    scroll_offset: usize,
    /// Rows scrolled since the last paint, positive downward. The next frame applies them.
    pending_scroll: isize,
    viewport_height: u16,
    viewport_width: u16,
    auto_scroll: bool,
    new_content_since_pause: u32,
    /// Slot tops of the last paint, which the next frame anchors the scroll offset on.
    painted: SlotTops,

    pending_calls: PendingCalls,
}

/// Inputs shared by every block render.
struct RenderSettings {
    theme: Theme,
    /// Bumped per theme swap to invalidate cached block renders.
    theme_epoch: u64,
    show_thinking: bool,
}

impl RenderSettings {
    fn ctx(&self, width: u16) -> RenderCtx<'_> {
        RenderCtx {
            width,
            theme: &self.theme,
            show_thinking: self.show_thinking,
        }
    }
}

/// Thinking, streaming-tail, and summary rows, rendered every frame as their buffers grow.
struct LiveLines {
    thinking: Vec<Line<'static>>,
    streaming_tail: Vec<Line<'static>>,
    summary: Vec<Line<'static>>,
}

/// Where a frame paints the stack, and the rows that must be rendered at the current width and
/// theme for that paint to be exact.
struct Placement {
    offset: usize,
    max_offset: usize,
    /// The row rendering grows from: the last row under auto-scroll, else the anchored row.
    pin: usize,
    /// From the anchored row to the painted rows, plus a viewport height above and below.
    reach: Range<usize>,
}

impl ChatView {
    pub(crate) fn new(theme: &Theme, show_thinking: bool) -> Self {
        Self {
            settings: RenderSettings {
                theme: theme.clone(),
                theme_epoch: 0,
                show_thinking,
            },
            blocks: Vec::new(),
            streaming: None,
            thinking_buffer: String::new(),
            summary_buffer: String::new(),
            scroll_offset: 0,
            pending_scroll: 0,
            viewport_height: 0,
            viewport_width: 0,
            auto_scroll: true,
            new_content_since_pause: 0,
            painted: SlotTops::default(),
            pending_calls: PendingCalls::new(),
        }
    }

    /// Re-skins every subsequent render, including cached blocks and the streaming prefix.
    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        self.settings.theme = theme.clone();
        self.settings.theme_epoch += 1;
        if let Some(s) = &mut self.streaming {
            s.invalidate_cache();
        }
    }

    /// Populates from a resumed transcript's interactions, matching the live block shapes.
    pub(crate) fn load_history(&mut self, history: Vec<Interaction>) {
        for interaction in history {
            match interaction {
                Interaction::Compacted {
                    summary,
                    pre_count,
                    instructions,
                } => self.push_compacted_block(pre_count, instructions.as_deref(), summary),
                Interaction::UserText(text) => self.push_block(UserMessage::new(text)),
                Interaction::AssistantText(text) => self.push_block(AssistantText::new(text)),
                Interaction::AssistantThinking(text) => {
                    self.push_block(AssistantThinking::new(text));
                }
                Interaction::ProgressUpdate(text) => {
                    self.push_block(ProgressUpdate::new(text));
                }
                Interaction::ToolCall(call) => self.push_tool_call(call),
                Interaction::ToolResult(result) => self.push_tool_result(result),
                Interaction::Interrupted => self.push_block(InterruptedMarker),
            }
        }
    }

    /// Appends a user message, flushing any in-flight streaming buffer.
    pub(crate) fn push_user_message(&mut self, text: String) {
        self.commit_streaming();
        self.push_block(UserMessage::new(text));
        self.auto_scroll = true;
        self.new_content_since_pause = 0;
    }

    /// Appends a streamed token to the current assistant response.
    ///
    /// Pending thinking is committed first so a `Thinking → Text` boundary always lands at a
    /// block split. Auto-scroll only follows the tail when the user hasn't manually scrolled up.
    pub(crate) fn append_stream_token(&mut self, token: &str) {
        self.commit_thinking_buffer();
        if self.streaming.is_none() {
            self.bump_paused_counter();
        }
        self.streaming
            .get_or_insert_with(StreamingAssistant::new)
            .append(token);
        self.advance_streaming_cache();
    }

    /// Appends a thinking token to the live thinking display buffer.
    pub(crate) fn append_thinking_token(&mut self, token: &str) {
        if self.thinking_buffer.is_empty() {
            self.bump_paused_counter();
        }
        self.thinking_buffer.push_str(token);
    }

    /// Appends a token to the compaction summary in flight.
    pub(crate) fn append_summary_token(&mut self, token: &str) {
        if self.summary_buffer.is_empty() {
            self.bump_paused_counter();
        }
        self.summary_buffer.push_str(token);
    }

    /// Drops the compaction summary in flight, which then no longer counts as new content.
    pub(crate) fn discard_summary(&mut self) {
        if !self.summary_buffer.is_empty() && !self.auto_scroll {
            self.new_content_since_pause = self.new_content_since_pause.saturating_sub(1);
        }
        self.summary_buffer.clear();
    }

    /// Finalizes the streaming buffer into a committed block.
    ///
    /// Pending thinking is flushed first so a turn that produced only thinking still leaves an
    /// `AssistantThinking` block in the transcript.
    pub(crate) fn commit_streaming(&mut self) {
        self.commit_thinking_buffer();
        if let Some(mut s) = self.streaming.take() {
            let text = s.take_buffer();
            if !text.is_empty() {
                self.push_block(AssistantText::new(text));
            }
        }
    }

    fn commit_thinking_buffer(&mut self) {
        if self.thinking_buffer.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.thinking_buffer);
        self.push_block(AssistantThinking::new(text));
    }

    /// Appends the progress note for the tool call about to follow, flushing any in-flight
    /// streaming buffer.
    pub(crate) fn push_progress_update(&mut self, text: String) {
        self.commit_streaming();
        self.bump_paused_counter();
        self.push_block(ProgressUpdate::new(text));
    }

    /// Appends a tool call, flushing any in-flight streaming buffer.
    pub(crate) fn push_tool_call(&mut self, call: ToolCall) {
        self.commit_streaming();
        self.bump_paused_counter();
        self.pending_calls
            .insert(call.id, call.label.clone(), self.blocks.len());
        self.push_block(ToolCallBlock::new(call.icon, call.label));
    }

    /// Adds a tool result under the header [`result_header`] picks. It goes right below its call,
    /// even when calls that ran alongside it came after, so the chat reads like a resumed one.
    pub(crate) fn push_tool_result(&mut self, result: ToolResult) {
        let (label, call_block) = self.pending_calls.remove(&result.id).unzip();
        let header = result_header(result.title, label);
        let block = ToolResultBlock::new(&header, result.view, result.is_error);
        self.bump_paused_counter();
        match call_block.map(|call_block| call_block + 1) {
            Some(at) if at < self.blocks.len() => self.insert_block(at, block),
            _ => self.push_block(block),
        }
    }

    /// Forgets calls whose results never arrived, such as those of a cancelled turn.
    pub(crate) fn clear_pending_calls(&mut self) {
        self.pending_calls.clear();
    }

    #[cfg(test)]
    pub(crate) fn pending_call_count(&self) -> usize {
        self.pending_calls.len()
    }

    /// Appends an error message.
    pub(crate) fn push_error(&mut self, msg: &str) {
        self.bump_paused_counter();
        self.push_block(ErrorBlock::new(msg));
    }

    /// Appends a transient status line, such as a scheduled API retry.
    pub(crate) fn push_notice(&mut self, text: impl Into<String>) {
        self.bump_paused_counter();
        self.push_block(NoticeBlock::new(text));
    }

    /// Appends informational output from a slash command.
    pub(crate) fn push_system_message(&mut self, body: impl Into<String>) {
        self.bump_paused_counter();
        self.push_block(SystemMessageBlock::new(body));
    }

    /// Appends a compaction boundary block (`/compact` result).
    pub(crate) fn push_compacted_block(
        &mut self,
        pre_count: u32,
        instructions: Option<&str>,
        summary: impl Into<String>,
    ) {
        self.bump_paused_counter();
        self.push_block(CompactedBlock::new(pre_count, instructions, summary));
    }

    /// Appends a unified diff body for display.
    pub(crate) fn push_git_diff(&mut self, text: impl Into<String>) {
        self.bump_paused_counter();
        self.push_block(GitDiffBlock::new(text));
    }

    /// Appends an interrupted marker. Flushes any in-flight streaming buffer first.
    pub(crate) fn push_interrupted_marker(&mut self) {
        self.commit_streaming();
        self.bump_paused_counter();
        self.push_block(InterruptedMarker);
    }

    /// Resets transient state, preserving terminal-tied fields.
    pub(crate) fn clear_history(&mut self) {
        self.blocks.clear();
        self.streaming = None;
        self.thinking_buffer.clear();
        self.summary_buffer.clear();
        self.scroll_offset = 0;
        self.pending_scroll = 0;
        self.auto_scroll = true;
        self.new_content_since_pause = 0;
        self.painted = SlotTops::default();
        self.pending_calls.clear();
    }

    pub(crate) const fn is_scrolled_up(&self) -> bool {
        !self.auto_scroll
    }

    pub(crate) const fn new_content_since_pause(&self) -> u32 {
        self.new_content_since_pause
    }

    /// Number of committed chat blocks.
    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        self.blocks.len()
    }

    /// Whether the tail block is an [`ErrorBlock`].
    #[cfg(test)]
    pub(crate) fn last_is_error(&self) -> bool {
        self.blocks
            .last()
            .is_some_and(|b| b.block().is_error_marker())
    }

    /// Error text of the tail block, if it is an [`ErrorBlock`].
    #[cfg(test)]
    pub(crate) fn last_error_text(&self) -> Option<&str> {
        self.blocks.last().and_then(|b| b.block().error_text())
    }

    /// Body text of the tail block, if it is a [`SystemMessageBlock`].
    #[cfg(test)]
    pub(crate) fn last_system_text(&self) -> Option<&str> {
        self.blocks.last().and_then(|b| b.block().system_text())
    }
}

impl ChatView {
    pub(crate) fn handle_event(&mut self, event: &Event) {
        let page = usize::from(self.viewport_height.saturating_sub(2));
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
            }) => self.scroll_up(page),
            Event::Key(KeyEvent {
                code: KeyCode::PageDown,
                ..
            }) => self.scroll_down(page),
            Event::Key(KeyEvent {
                code: KeyCode::Home,
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                self.scroll_offset = 0;
                self.pending_scroll = 0;
                self.auto_scroll = false;
            }
            Event::Key(KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => self.jump_to_bottom(),
            _ => {}
        }
    }

    /// Lays out the chat for `area`, follows the tail while auto-scroll is on, and paints only
    /// the rows inside the viewport.
    ///
    /// Blocks render only once the paint can reach them. While scrolled up, the paint anchors on
    /// the row the last frame showed on top, then moves by the pending scroll. Each pass renders
    /// the stale block nearest the pinned row and places the paint again. Painting waits until
    /// every block within reach is current, so estimated heights stay off the screen, and the
    /// anchor keeps the visible rows in place while blocks above them change height.
    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.set_viewport(area);
        let live = self.render_live(area.width);
        let anchor = (!self.auto_scroll).then(|| self.painted.anchor_at(self.scroll_offset));

        loop {
            let stack = self.stack(&live);
            let placement = self.place(&stack, anchor);
            if let Some(stale) = self.nearest_stale(stack.tops(), &placement) {
                drop(stack);
                let ctx = self.settings.ctx(area.width);
                self.blocks[stale].refresh(&ctx, self.settings.theme_epoch);
                continue;
            }

            let lines = stack.slice(placement.offset, usize::from(area.height));
            self.painted = stack.into_tops();
            self.settle_scroll(&placement);
            let paragraph = Paragraph::new(lines).style(self.settings.theme.surface());
            frame.render_widget(paragraph, area);
            return;
        }
    }

    /// Re-arms auto-scroll, so the next frame shows the latest content.
    pub(crate) fn jump_to_bottom(&mut self) {
        self.auto_scroll = true;
        self.pending_scroll = 0;
        self.new_content_since_pause = 0;
    }
}

// ── Private Helpers ──

impl ChatView {
    fn push_block(&mut self, block: impl ChatBlock + 'static) {
        let ctx = self.settings.ctx(self.viewport_width);
        self.blocks.push(CachedBlock::new(Box::new(block), &ctx));
    }

    /// Inserts `block` at index `at`, so a paused view stays anchored on the block it showed.
    fn insert_block(&mut self, at: usize, block: impl ChatBlock + 'static) {
        let ctx = self.settings.ctx(self.viewport_width);
        self.blocks
            .insert(at, CachedBlock::new(Box::new(block), &ctx));
        self.pending_calls.shift_from(at);
        self.painted.insert_empty(at);
    }

    /// Adopts the chat area, dropping the streaming prefix cache on a width change.
    fn set_viewport(&mut self, area: Rect) {
        self.viewport_height = area.height;
        self.viewport_width = area.width;
        if let Some(s) = &mut self.streaming {
            s.invalidate_cache_for_width(area.width);
        }
    }

    /// Places the paint on `stack`: the tail under auto-scroll, else the anchored row moved by
    /// the pending scroll and clamped to the content.
    fn place(&self, stack: &Stack<'_>, anchor: Option<Anchor>) -> Placement {
        let viewport = usize::from(self.viewport_height);
        let max_offset = stack.height().saturating_sub(viewport);
        let Some(anchor) = anchor else {
            return Placement {
                offset: max_offset,
                max_offset,
                pin: stack.height().saturating_sub(1),
                reach: max_offset.saturating_sub(viewport)..stack.height() + viewport,
            };
        };

        let anchored = stack.tops().offset_of(anchor);
        let offset = anchored
            .saturating_add_signed(self.pending_scroll)
            .min(max_offset);
        Placement {
            offset,
            max_offset,
            pin: anchored,
            reach: anchored.min(offset).saturating_sub(viewport)
                ..anchored.max(offset) + 2 * viewport,
        }
    }

    /// The block within the placement's reach whose render is stale, nearest the pinned row.
    fn nearest_stale(&self, tops: &SlotTops, placement: &Placement) -> Option<usize> {
        let (width, theme_epoch) = (self.viewport_width, self.settings.theme_epoch);
        tops.blocks_within(placement.reach.clone())
            .filter(|&block| !self.blocks[block].is_current(width, theme_epoch))
            .min_by_key(|&block| {
                tops.slot(block)
                    .map_or(0, |slot| rows_between(&slot, placement.pin))
            })
    }

    /// Adopts the painted offset, re-arming auto-scroll when a scroll down reached the bottom.
    fn settle_scroll(&mut self, placement: &Placement) {
        if self.pending_scroll > 0 && placement.offset == placement.max_offset {
            self.auto_scroll = true;
            self.new_content_since_pause = 0;
        }
        self.pending_scroll = 0;
        self.scroll_offset = placement.offset;
    }

    fn scroll_up(&mut self, rows: usize) {
        self.pending_scroll = self.pending_scroll.saturating_sub_unsigned(rows);
        self.auto_scroll = false;
    }

    /// Scrolls toward the tail. The next frame re-arms auto-scroll once the paint reaches it.
    fn scroll_down(&mut self, rows: usize) {
        if !self.auto_scroll {
            self.pending_scroll = self.pending_scroll.saturating_add_unsigned(rows);
        }
    }

    fn bump_paused_counter(&mut self) {
        if !self.auto_scroll {
            self.new_content_since_pause = self.new_content_since_pause.saturating_add(1);
        }
    }

    fn render_live(&self, width: u16) -> LiveLines {
        let ctx = self.settings.ctx(width);
        let mut thinking = Vec::new();
        if !self.thinking_buffer.is_empty() {
            let block = AssistantThinking::new(self.thinking_buffer.clone());
            if block.visible(&ctx) {
                thinking = block.render(&ctx);
            }
        }
        let streaming_tail = self.streaming.as_ref().map_or_else(Vec::new, |s| {
            s.render_tail(&ctx, self.streaming_continues_turn())
        });
        let summary = if self.summary_buffer.is_empty() {
            Vec::new()
        } else {
            CompactedBlock::render_streaming(&self.summary_buffer, &ctx)
        };
        LiveLines {
            thinking,
            streaming_tail,
            summary,
        }
    }

    /// Stacks visible block renders and the live lines with blank-line separators.
    fn stack<'a>(&'a self, live: &'a LiveLines) -> Stack<'a> {
        let mut stack = Stack::default();
        for cached in &self.blocks {
            stack.push_block(cached);
        }

        stack.push_separated(&live.thinking);

        if let Some(streaming) = &self.streaming {
            if !self.streaming_continues_turn() && stack.last_has_width() {
                stack.push_blank();
            }
            stack.push(streaming.rendered());
            stack.push(&live.streaming_tail);
        }

        stack.push_separated(&live.summary);
        stack
    }

    fn advance_streaming_cache(&mut self) {
        let continues = self.streaming_continues_turn();
        if let Some(s) = &mut self.streaming {
            s.advance_cache(&self.settings.ctx(self.viewport_width), continues);
        }
    }

    /// Whether streaming tokens continue the last committed assistant turn.
    fn streaming_continues_turn(&self) -> bool {
        self.blocks
            .last()
            .is_some_and(|b| b.block().continues_assistant_turn())
    }

    /// True when the chat shows nothing: no committed block, reply, thinking, or summary.
    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
            && self.streaming.is_none()
            && self.thinking_buffer.is_empty()
            && self.summary_buffer.is_empty()
    }
}

fn rows_between(slot: &Range<usize>, row: usize) -> usize {
    slot.start.saturating_sub(row) + (row + 1).saturating_sub(slot.end)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use indoc::{formatdoc, indoc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use unicode_width::UnicodeWidthStr;

    use super::*;
    use crate::tool::ToolResultView;
    use crate::tui::glyphs::{
        ASSISTANT_PREFIX, BAR, INTERRUPTED_MARKER, TOOL_BORDER_CONT, TOOL_ERROR, TOOL_SUCCESS,
    };

    // ── Fixtures ──

    fn test_chat() -> ChatView {
        ChatView::new(&Theme::default(), true)
    }

    impl ChatView {
        fn push_tool_result_view(&mut self, header: &str, view: ToolResultView, is_error: bool) {
            self.bump_paused_counter();
            self.push_block(ToolResultBlock::new(header, view, is_error));
        }
    }

    /// A call keyed by its label.
    fn call(icon: &str, label: &str) -> ToolCall {
        ToolCall {
            id: label.to_owned(),
            name: "tool".to_owned(),
            input: serde_json::json!({}),
            icon: icon.to_owned(),
            label: label.to_owned(),
        }
    }

    fn text_result(id: &str, content: &str, is_error: bool, title: Option<&str>) -> ToolResult {
        ToolResult {
            id: id.to_owned(),
            content: content.to_owned(),
            is_error,
            title: title.map(ToOwned::to_owned),
            view: ToolResultView::Text {
                content: content.to_owned(),
            },
        }
    }

    /// A text result headed by its own `title`, matching no call.
    fn titled_result(title: &str, content: &str, is_error: bool) -> ToolResult {
        text_result("titled", content, is_error, Some(title))
    }

    /// Stacks every row at `width`, leaving the viewport and scroll state untouched.
    fn all_lines(chat: &mut ChatView, width: u16) -> Vec<Line<'static>> {
        let ctx = chat.settings.ctx(width);
        for block in &mut chat.blocks {
            block.refresh(&ctx, chat.settings.theme_epoch);
        }
        let live = chat.render_live(width);
        let stack = chat.stack(&live);
        stack.slice(0, stack.height())
    }

    /// Stacks every row at `width` as plain text.
    fn all_rows(chat: &mut ChatView, width: u16) -> Vec<String> {
        all_lines(chat, width)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Stacks every row at width 80 and joins them for substring assertions.
    fn all_text(chat: &mut ChatView) -> String {
        all_lines(chat, 80)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_count(chat: &mut ChatView) -> usize {
        all_lines(chat, 80).len()
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
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                chat.render(frame, frame.area());
            })
            .unwrap();
        terminal.backend().clone()
    }

    fn screen_rows(backend: &TestBackend) -> Vec<String> {
        let buffer = backend.buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// Emits `rows` numbered lines and counts its renders.
    struct CountingBlock {
        rows: usize,
        renders: Rc<Cell<usize>>,
    }

    impl ChatBlock for CountingBlock {
        fn render(&self, _ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
            self.renders.set(self.renders.get() + 1);
            (0..self.rows)
                .map(|i| Line::raw(format!("row {i}")))
                .collect()
        }

        fn standalone(&self) -> bool {
            false
        }
    }

    fn push_counting_block(chat: &mut ChatView, rows: usize) -> Rc<Cell<usize>> {
        let renders = Rc::new(Cell::new(0));
        chat.push_block(CountingBlock {
            rows,
            renders: Rc::clone(&renders),
        });
        renders
    }

    /// Emits `60 / width` rows labeled `b{index} {i}`, so narrowing the chat makes it taller.
    struct WrappingBlock {
        index: usize,
    }

    impl ChatBlock for WrappingBlock {
        fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
            (0..60 / usize::from(ctx.width))
                .map(|i| Line::raw(format!("b{} {i}", self.index)))
                .collect()
        }

        fn standalone(&self) -> bool {
            false
        }
    }

    /// A resumed session whose replies each hold a 30-line Rust block. Every code row names its
    /// turn, so each window of a few rows appears once in the chat.
    fn long_transcript(turns: usize) -> Vec<Interaction> {
        (0..turns)
            .flat_map(|turn| {
                let body = (0..28)
                    .map(|i| format!("    let value_{i} = compute({turn}, {i});"))
                    .collect::<Vec<_>>()
                    .join("\n");
                [
                    Interaction::UserText(format!("Question {turn}: how does this work?")),
                    Interaction::AssistantText(formatdoc! {"
                        Here is turn {turn}:

                        ```rust
                        fn turn_{turn}() {{
                        {body}
                        }}
                        ```

                        That covers it.
                    "}),
                ]
            })
            .collect()
    }

    /// Frame times over a resumed transcript of `rows` rows, with medians for the repeated frames.
    #[derive(Debug)]
    #[expect(
        dead_code,
        reason = "the timing harness reads the fields through Debug"
    )]
    struct FrameTimes {
        rows: usize,
        open: Duration,
        steady: Duration,
        resize: Duration,
        theme_swap: Duration,
    }

    /// Paints the frames the timing harness measures over `turns` resumed turns: an open, then
    /// `samples` frames each of steady state, resizes between `width` and 20 columns narrower, and
    /// theme swaps. Checks that each frame paints the tail of a full render.
    fn paint_timed_frames(turns: usize, width: u16, height: u16, samples: usize) -> FrameTimes {
        let widths = [width, width - 20];
        let full = widths.map(|width| {
            let mut full = test_chat();
            full.load_history(long_transcript(turns));
            all_lines(&mut full, width)
        });
        let tails = full.each_ref().map(|lines| {
            lines[lines.len() - usize::from(height)..]
                .iter()
                .map(|line| line.to_string().trim_end().to_owned())
                .collect::<Vec<_>>()
        });
        let themes = [
            Theme::default(),
            crate::tui::theme::load_builtin("latte").unwrap(),
        ];

        let mut chat = test_chat();
        chat.load_history(long_transcript(turns));
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let open = paint_timed(&mut terminal, &mut chat, &tails[0]);
        let mut runs: [Vec<Duration>; 3] = Default::default();
        for _ in 0..samples {
            runs[0].push(paint_timed(&mut terminal, &mut chat, &tails[0]));
        }
        for i in 1..=samples {
            terminal.backend_mut().resize(widths[i % 2], height);
            runs[1].push(paint_timed(&mut terminal, &mut chat, &tails[i % 2]));
        }
        terminal.backend_mut().resize(width, height);
        for i in 1..=samples {
            chat.set_theme(&themes[i % 2]);
            runs[2].push(paint_timed(&mut terminal, &mut chat, &tails[0]));
        }

        let [steady, resize, theme_swap] = runs.map(|mut run| {
            run.sort();
            run[run.len() / 2]
        });
        FrameTimes {
            rows: full[0].len(),
            open,
            steady,
            resize,
            theme_swap,
        }
    }

    fn paint_timed(
        terminal: &mut Terminal<TestBackend>,
        chat: &mut ChatView,
        tail: &[String],
    ) -> Duration {
        let start = Instant::now();
        terminal
            .draw(|frame| chat.render(frame, frame.area()))
            .unwrap();
        let elapsed = start.elapsed();
        assert_eq!(screen_rows(terminal.backend()), tail);
        elapsed
    }

    // ── set_theme ──

    #[test]
    fn set_theme_invalidates_streaming_cache_so_in_flight_tokens_repaint() {
        // Bare `/theme` opens its picker mid-stream; without dropping the cache, committed
        // prefix tokens would keep painting under the previous palette until commit_streaming.
        let mut chat = test_chat();
        chat.set_viewport(Rect::new(0, 0, 80, 24));
        chat.append_stream_token("a complete paragraph\n\nnext\n");
        let s = chat.streaming.as_ref().unwrap();
        assert_ne!(s.rendered().len(), 0, "cache populated");
        assert_eq!(s.cached_width(), 80);

        chat.set_theme(&Theme::default());

        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered().len(), 0, "cache dropped on theme swap");
        assert_eq!(s.rendered_boundary(), 0);
        assert_eq!(s.cached_width(), 0);
    }

    #[test]
    fn set_theme_without_active_streaming_is_a_noop_on_cache_state() {
        // The `if let Some(streaming)` guard skips the cache reset when no stream is in flight;
        // pin both branches so the guard isn't accidentally inverted in a future refactor.
        let mut chat = test_chat();
        assert!(chat.streaming.is_none());
        chat.set_theme(&Theme::default());
        assert!(
            chat.streaming.is_none(),
            "set_theme must not spawn a stream"
        );
    }

    // ── load_history ──

    #[test]
    fn load_history_renders_each_interaction_in_order() {
        let mut chat = test_chat();
        chat.load_history(vec![
            Interaction::Compacted {
                summary: "Kept the important decisions.".to_owned(),
                pre_count: 4,
                instructions: Some("focus on auth".to_owned()),
            },
            Interaction::UserText("hello".to_owned()),
            Interaction::AssistantText("hi there".to_owned()),
            Interaction::ToolCall(call("$", "ls")),
            Interaction::ToolResult(text_result("ls", "file a", false, None)),
            Interaction::Interrupted,
        ]);

        assert_eq!(chat.blocks.len(), 6);
        let text = all_text(&mut chat);
        let order = [
            "Compacted 4 messages",
            "focus on auth",
            "Kept the important decisions.",
            "hello",
            "hi there",
            "$ ls",
            "file a",
            INTERRUPTED_MARKER,
        ]
        .map(|needle| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {text}"))
        });
        assert!(order.is_sorted(), "{order:?}: {text}");
        assert!(
            text.contains(&format!("{TOOL_SUCCESS} ls")),
            "an untitled result reuses its call's label: {text}",
        );
    }

    #[test]
    fn load_history_titles_results_before_falling_back_for_orphans() {
        let mut chat = test_chat();
        chat.load_history(vec![
            Interaction::ToolCall(call("✎", "Edit(f.rs)")),
            Interaction::ToolResult(text_result(
                "Edit(f.rs)",
                "done",
                false,
                Some("Edited f.rs"),
            )),
            Interaction::ToolResult(text_result("ghost", "stderr", true, None)),
        ]);

        let text = all_text(&mut chat);
        assert!(
            text.contains(&format!("{TOOL_SUCCESS} Edited f.rs")),
            "{text}"
        );
        assert_eq!(text.matches("Edit(f.rs)").count(), 1, "{text}");
        assert!(text.contains(&format!("{TOOL_ERROR} (result)")), "{text}");
        assert!(text.contains("stderr"), "{text}");
    }

    #[test]
    fn load_history_shows_progress_update_once_ahead_of_its_tool_call() {
        let history = || {
            vec![
                Interaction::AssistantThinking("Running the suite.".to_owned()),
                Interaction::ProgressUpdate("Running the suite.".to_owned()),
                Interaction::ToolCall(call("$", "cargo test")),
                Interaction::ToolResult(text_result("cargo test", "ok", false, None)),
            ]
        };
        for show_thinking in [false, true] {
            let mut chat = ChatView::new(&Theme::default(), show_thinking);
            chat.load_history(history());
            let text = all_text(&mut chat);
            assert_eq!(
                text.matches("Running the suite.").count(),
                1,
                "show_thinking={show_thinking}: {text}",
            );
            assert_eq!(text.contains("Thinking..."), show_thinking, "{text}");
            assert!(
                text.find("Running the suite.") < text.find("cargo test"),
                "{text}"
            );
        }
    }

    // ── push_user_message ──

    #[test]
    fn push_user_message_has_icon_and_content() {
        let mut chat = test_chat();
        chat.push_user_message("hello world".to_owned());
        let text = all_text(&mut chat);
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
        chat.push_tool_call(call("$", "ls"));
        let text = all_text(&mut chat);
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
        chat.push_block(AssistantText::new("reply"));
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
        assert!(text.contains("line3"));
    }

    // ── append_stream_token ──

    #[test]
    fn append_stream_token_persists_thinking_into_blocks() {
        let mut chat = test_chat();
        chat.append_thinking_token("reasoning here");
        assert_ne!(chat.thinking_buffer, "");
        assert_eq!(chat.blocks.len(), 0);

        chat.append_stream_token("reply text");
        assert_eq!(chat.thinking_buffer, "");
        assert_eq!(chat.blocks.len(), 1);

        let text = all_text(&mut chat);
        assert!(text.contains("reasoning here"));
        assert!(text.contains("reply text"));
    }

    #[test]
    fn append_stream_token_shows_partial_text() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.append_stream_token("partial response");
        let text = all_text(&mut chat);
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

        let text = all_text(&mut chat);
        assert!(text.contains("cached line"));
        assert!(text.contains("tail text"));
    }

    #[test]
    fn append_stream_token_uncommitted_newlines_all_render() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.viewport_width = 80;
        chat.append_stream_token("line1\nline2\npartial");

        let text = all_text(&mut chat);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
        assert!(text.contains("partial"));
    }

    #[test]
    fn append_stream_token_without_prior_assistant_shows_icon() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.append_stream_token("response");

        let text = all_text(&mut chat);
        assert!(
            text.contains(ASSISTANT_PREFIX),
            "new turn should show assistant icon"
        );
    }

    #[test]
    fn append_stream_token_after_committed_assistant_omits_duplicate_icon() {
        let mut chat = test_chat();
        chat.push_block(AssistantText::new("committed"));
        let mut s = StreamingAssistant::new();
        s.append("streaming");
        chat.streaming = Some(s);

        let text = all_text(&mut chat);
        let count = text.matches(ASSISTANT_PREFIX).count();
        assert_eq!(count, 1, "icon should appear once, not duplicated");
    }

    #[test]
    fn append_stream_token_inserts_blank_separator_after_tool_output() {
        let mut chat = test_chat();
        chat.push_tool_call(call("$", "ls"));
        chat.append_stream_token("response");

        let text = all_text(&mut chat);
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

        let text = all_text(&mut chat);
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

        let text = all_text(&mut chat);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
    }

    #[test]
    fn append_stream_token_trailing_newline_with_empty_tail() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        chat.viewport_width = 80;
        chat.append_stream_token("line1\nline2\n");

        let text = all_text(&mut chat);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
    }

    #[test]
    fn append_stream_token_preserves_blank_between_committed_paragraphs() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("para1\n\n");
        chat.append_stream_token("para2\n\ntail\n");

        let text = all_text(&mut chat);
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

        let text = all_text(&mut chat);
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

        let text = all_text(&mut chat);
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

        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
        assert!(text.contains("Thinking..."));
        assert!(text.contains("pondering..."));
    }

    #[test]
    fn append_thinking_token_hidden_when_disabled() {
        let mut chat = ChatView::new(&Theme::default(), false);
        chat.append_thinking_token("pondering...");
        let text = all_text(&mut chat);
        assert!(!text.contains("Thinking..."));
        assert!(!text.contains("pondering..."));
    }

    #[test]
    fn append_thinking_token_after_user_has_separator() {
        let mut chat = test_chat();
        chat.push_user_message("hello".to_owned());
        chat.append_thinking_token("deep thought");
        let text = all_text(&mut chat);
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
        chat.push_tool_call(call("$", "ls"));
        chat.append_thinking_token("deep thought");

        let text = all_text(&mut chat);
        let lines: Vec<&str> = text.lines().collect();
        let tool_pos = lines.iter().position(|l| l.contains("ls")).unwrap();
        let thinking_pos = lines.iter().position(|l| l.contains("Thinking")).unwrap();
        assert!(
            (tool_pos + 1..thinking_pos).any(|i| lines[i].trim().is_empty()),
            "expected blank separator between tool call and thinking: {lines:?}"
        );
    }

    // ── append_summary_token ──

    #[test]
    fn append_summary_token_streams_the_summary_a_blank_row_below_the_chat() {
        let mut chat = test_chat();
        chat.push_tool_call(call("$", "ls"));
        let chat_rows = all_rows(&mut chat, 80);
        chat.auto_scroll = false;

        chat.append_summary_token("Fixed ");
        chat.append_summary_token("the bug.");

        let summary: Vec<String> =
            CompactedBlock::render_streaming("Fixed the bug.", &chat.settings.ctx(80))
                .iter()
                .map(ToString::to_string)
                .collect();
        assert_eq!(
            all_rows(&mut chat, 80),
            [chat_rows, vec![String::new()], summary].concat(),
        );
        assert_eq!(
            chat.new_content_since_pause(),
            1,
            "the summary counts as one new block while paused",
        );
    }

    // ── discard_summary ──

    #[test]
    fn discard_summary_drops_the_streamed_summary_so_a_retry_starts_over() {
        let mut chat = test_chat();
        chat.push_tool_call(call("$", "ls"));
        let chat_rows = all_rows(&mut chat, 80);
        chat.append_summary_token("First attempt.");

        chat.discard_summary();
        assert_eq!(all_rows(&mut chat, 80), chat_rows);

        chat.append_summary_token("Second attempt.");
        let text = all_text(&mut chat);
        assert!(
            text.contains("Second attempt.") && !text.contains("First attempt."),
            "{text}",
        );
    }

    #[test]
    fn discard_summary_stops_counting_the_summary_as_new_content_while_paused() {
        let mut chat = test_chat();
        chat.auto_scroll = false;
        chat.push_tool_call(call("$", "ls"));
        chat.append_summary_token("First attempt.");
        assert_eq!(chat.new_content_since_pause(), 2);

        chat.discard_summary();
        assert_eq!(chat.new_content_since_pause(), 1);
        chat.discard_summary();
        assert_eq!(
            chat.new_content_since_pause(),
            1,
            "a second discard has no summary left to stop counting",
        );

        chat.append_summary_token("Second attempt.");
        assert_eq!(chat.new_content_since_pause(), 2);
    }

    // ── commit_streaming ──

    #[test]
    fn commit_streaming_moves_buffer_to_block() {
        let mut chat = test_chat();
        chat.append_stream_token("hello world");
        assert!(chat.blocks.is_empty());

        chat.commit_streaming();
        assert_eq!(chat.blocks.len(), 1);
        let text = all_text(&mut chat);
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
        assert_eq!(chat.thinking_buffer, "");
    }

    #[test]
    fn commit_streaming_persists_thinking_only_turn() {
        let mut chat = test_chat();
        chat.append_thinking_token("plan before tool");
        assert!(chat.blocks.is_empty());

        chat.commit_streaming();
        assert_eq!(chat.blocks.len(), 1);
        assert_eq!(chat.thinking_buffer, "");
        let text = all_text(&mut chat);
        assert!(text.contains("plan before tool"));
    }

    // ── push_progress_update ──

    #[test]
    fn push_progress_update_shows_note_once_ahead_of_tool_call() {
        // Live, the note streams as thinking first and arrives again just before its tool call.
        for show_thinking in [false, true] {
            let mut chat = ChatView::new(&Theme::default(), show_thinking);
            chat.append_thinking_token("Running the suite.");
            chat.push_progress_update("Running the suite.".to_owned());
            chat.push_tool_call(call("$", "cargo test"));
            let text = all_text(&mut chat);
            assert_eq!(
                text.matches("Running the suite.").count(),
                1,
                "show_thinking={show_thinking}: {text}",
            );
            assert!(
                text.find("Running the suite.") < text.find("cargo test"),
                "{text}"
            );
        }
    }

    // ── push_tool_call ──

    #[test]
    fn push_tool_call_shows_icon_and_label() {
        let mut chat = test_chat();
        chat.push_tool_call(call("$", "ls -la"));
        let text = all_text(&mut chat);
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
        chat.push_block(AssistantText::new("some text"));
        chat.push_tool_call(call("$", "ls"));
        let text = all_text(&mut chat);
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
        chat.push_tool_call(call("$", "ls"));
        chat.push_tool_result(titled_result("ran ls", "output one", false));
        chat.push_tool_call(call("$", "pwd"));
        chat.push_tool_result(titled_result("ran pwd", "output two", false));
        let text = all_text(&mut chat);
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
        chat.push_tool_call(call("$", "ls"));
        chat.push_tool_call(call("$", "cat foo"));
        let text = all_text(&mut chat);
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
        chat.push_tool_call(call("$", long_cmd));
        let lines = all_lines(&mut chat, 60);
        assert!(
            lines.len() > 1,
            "long tool call label should wrap across multiple lines: {}",
            lines.len(),
        );
        for line in &lines {
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
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
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
    fn push_tool_result_of_calls_that_ran_together_reads_like_the_resumed_chat() {
        let mut live = test_chat();
        live.push_tool_call(call("*", "Grep(a)"));
        live.push_tool_call(call("*", "Grep(b)"));
        live.push_tool_result(text_result("Grep(a)", "a hits", false, None));
        live.push_tool_result(text_result("Grep(b)", "b hits", false, None));
        let mut resumed = test_chat();

        resumed.load_history(vec![
            Interaction::ToolCall(call("*", "Grep(a)")),
            Interaction::ToolResult(text_result("Grep(a)", "a hits", false, None)),
            Interaction::ToolCall(call("*", "Grep(b)")),
            Interaction::ToolResult(text_result("Grep(b)", "b hits", false, None)),
        ]);

        let rows = all_rows(&mut live, 40);
        assert_eq!(rows, all_rows(&mut resumed, 40));
        let order = ["* Grep(a)", "a hits", "* Grep(b)", "b hits"]
            .map(|needle| rows.iter().position(|row| row.contains(needle)));
        assert_eq!(order, [Some(0), Some(2), Some(4), Some(6)], "{rows:?}");
    }

    #[test]
    fn push_tool_result_placed_above_a_paused_view_keeps_it_in_place() {
        let mut chat = test_chat();
        chat.push_tool_call(call("*", "Grep(a)"));
        chat.push_tool_call(call("*", "Grep(b)"));
        push_counting_block(&mut chat, 30);
        render_chat(&mut chat, 20, 5);
        chat.auto_scroll = false;
        chat.scroll_offset = 12;
        let before = screen_rows(&render_chat(&mut chat, 20, 5));

        chat.push_tool_result(text_result("Grep(a)", "a hits", false, None));

        let after = screen_rows(&render_chat(&mut chat, 20, 5));
        assert_eq!(after, before);
        assert_eq!(before[0], "row 10");
    }

    #[test]
    fn push_tool_result_success() {
        let mut chat = test_chat();
        chat.push_tool_result(titled_result("done", "output text", false));
        let text = all_text(&mut chat);
        assert!(text.contains(TOOL_SUCCESS));
        assert!(text.contains("done"));
        assert!(text.contains("output text"));
    }

    #[test]
    fn push_tool_result_error() {
        let mut chat = test_chat();
        chat.push_tool_result(titled_result("failed", "error details", true));
        let text = all_text(&mut chat);
        assert!(text.contains(TOOL_ERROR));
        assert!(text.contains("failed"));
        assert!(text.contains("error details"));
        let bar_style = all_lines(&mut chat, 60)
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
        chat.push_tool_result(titled_result(long_label, "", false));
        let lines = all_lines(&mut chat, 50);
        assert!(
            lines.len() > 1,
            "long tool result label should wrap: {}",
            lines.len(),
        );
        for line in &lines {
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
        chat.push_tool_result(titled_result("result", &long_output.join("\n"), false));
        let text = all_text(&mut chat);

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
        chat.push_tool_result(titled_result("result", "  \n  ", false));
        let before = line_count(&mut chat);

        let mut chat2 = test_chat();
        chat2.push_tool_result(titled_result("result", "", false));
        let after = line_count(&mut chat2);

        assert_eq!(before, after);
    }

    #[test]
    fn push_tool_result_dedup_drops_first_body_line_matching_label() {
        let mut chat = test_chat();
        chat.push_tool_result(titled_result(
            "Found 2 files",
            indoc! {"
                Found 2 files
                a.rs
                b.rs"
            },
            false,
        ));
        let text = all_text(&mut chat);
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
        chat.push_tool_result(titled_result(
            "Found 2 files",
            indoc! {"
                Found 2 files in cache
                a.rs"
            },
            false,
        ));
        let text = all_text(&mut chat);
        assert!(
            text.contains("Found 2 files in cache"),
            "body preserved: {text}"
        );
    }

    #[test]
    fn push_tool_result_preserves_leading_whitespace_on_first_body_line() {
        let mut chat = test_chat();
        chat.push_tool_result(titled_result("out", " a.rs | 1 +\n b.rs | 2 +", false));
        let text = all_text(&mut chat);
        assert!(
            text.contains(" a.rs | 1 +"),
            "first body line must keep its leading space: {text}",
        );
        assert!(text.contains(" b.rs | 2 +"));
    }

    #[test]
    fn push_tool_result_drops_surrounding_blank_lines() {
        let mut chat = test_chat();
        chat.push_tool_result(titled_result("out", "\n\n real line\n\n\n", false));
        let text = all_text(&mut chat);
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
        chat.push_tool_result(titled_result("No matches found", "No matches found", false));
        let text = all_text(&mut chat);
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
        chat.push_tool_result(titled_result("result", &output.join("\n"), false));
        let text = all_text(&mut chat);
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
        chat.push_tool_result(titled_result("result", &output.join("\n"), false));
        let text = all_text(&mut chat);
        assert!(text.contains("... +1 line"));
        assert!(!text.contains("lines"), "singular 'line' expected: {text}");
    }

    #[test]
    fn push_tool_result_long_line_is_truncated() {
        const MAX_CHARS: usize = 512;
        let mut chat = test_chat();
        let long_line = "x".repeat(MAX_CHARS + 100);
        chat.push_tool_result(titled_result("result", &long_line, false));
        let text = all_text(&mut chat);
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
        let text = all_text(&mut chat);
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
        chat.push_tool_call(call("$", "ls"));
        chat.append_stream_token("partial reply");
        chat.append_thinking_token("considering");
        chat.append_summary_token("recap");
        render_chat(&mut chat, 80, 24);
        chat.scroll_offset = 25;
        chat.pending_scroll = -3;
        chat.auto_scroll = false;

        chat.clear_history();

        assert_eq!(chat.entry_count(), 0);
        assert_eq!(chat.pending_call_count(), 0);
        assert!(chat.streaming.is_none());
        assert_eq!(chat.thinking_buffer, "");
        assert_eq!(chat.summary_buffer, "");
        assert_eq!((chat.scroll_offset, chat.pending_scroll), (0, 0));
        assert_eq!(chat.painted, SlotTops::default());
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

        chat.push_tool_call(call("$", "ls"));
        assert!(!chat.last_is_error());

        chat.push_tool_result(titled_result("failed", "boom", true));
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

        chat.push_tool_call(call("$", "ls"));
        assert_eq!(chat.last_system_text(), None);

        chat.push_error("boom");
        assert_eq!(chat.last_system_text(), None);
    }

    #[test]
    fn last_system_text_none_when_no_blocks() {
        let chat = test_chat();
        assert_eq!(chat.last_system_text(), None);
    }

    // ── bump_paused_counter ──

    #[test]
    fn paused_counter_bumps_only_while_scrolled_up() {
        let mut chat = test_chat();
        chat.push_system_message("one");
        assert_eq!(chat.new_content_since_pause(), 0);

        chat.auto_scroll = false;
        chat.push_system_message("two");
        chat.push_error("three");
        assert_eq!(chat.new_content_since_pause(), 2);
    }

    #[test]
    fn paused_counter_streaming_bumps_once_per_block() {
        let mut chat = test_chat();
        chat.auto_scroll = false;

        chat.append_stream_token("a");
        chat.append_stream_token("b");
        assert_eq!(chat.new_content_since_pause(), 1);

        chat.commit_streaming();
        chat.append_stream_token("c");
        assert_eq!(chat.new_content_since_pause(), 2);
    }

    #[test]
    fn paused_counter_does_not_overflow_at_u32_max() {
        let mut chat = test_chat();
        chat.auto_scroll = false;
        chat.new_content_since_pause = u32::MAX;

        chat.push_error("overflow");
        assert_eq!(chat.new_content_since_pause(), u32::MAX);
    }

    // ── handle_event ──

    #[test]
    fn handle_event_arrows_and_wheel_scroll_one_row() {
        let mut chat = test_chat();
        chat.handle_event(&key_event(KeyCode::Up));
        chat.handle_event(&mouse_scroll(MouseEventKind::ScrollUp));
        assert_eq!(chat.pending_scroll, -2);
        assert!(!chat.auto_scroll);

        chat.handle_event(&key_event(KeyCode::Down));
        chat.handle_event(&mouse_scroll(MouseEventKind::ScrollDown));
        chat.handle_event(&key_event(KeyCode::Down));
        assert_eq!(chat.pending_scroll, 1);
    }

    #[test]
    fn handle_event_pages_scroll_two_rows_short_of_the_viewport() {
        let mut chat = test_chat();
        chat.viewport_height = 20;
        chat.handle_event(&key_event(KeyCode::PageUp));
        assert_eq!(chat.pending_scroll, -18);

        chat.handle_event(&key_event(KeyCode::PageDown));
        chat.handle_event(&key_event(KeyCode::PageDown));
        assert_eq!(chat.pending_scroll, 18);
    }

    #[test]
    fn handle_event_ctrl_home_jumps_to_the_top_and_ctrl_end_follows_the_tail() {
        let mut chat = test_chat();
        chat.scroll_offset = 50;
        chat.handle_event(&key_event(KeyCode::Up));
        chat.handle_event(&ctrl_key_event(KeyCode::Home));
        assert_eq!((chat.scroll_offset, chat.pending_scroll), (0, 0));
        assert!(!chat.auto_scroll);

        chat.new_content_since_pause = 3;
        chat.handle_event(&key_event(KeyCode::Up));
        chat.handle_event(&ctrl_key_event(KeyCode::End));
        assert_eq!(chat.pending_scroll, 0);
        assert!(chat.auto_scroll);
        assert_eq!(chat.new_content_since_pause(), 0);
    }

    #[test]
    fn handle_event_unhandled_key_leaves_state_unchanged() {
        let mut chat = test_chat();
        chat.handle_event(&key_event(KeyCode::Char('a')));
        assert_eq!(chat.pending_scroll, 0);
        assert!(chat.auto_scroll);
    }

    // ── render ──

    #[test]
    fn render_auto_scroll_pins_tail_in_one_pass() {
        let mut chat = test_chat();
        push_counting_block(&mut chat, 30);
        let screen = screen_rows(&render_chat(&mut chat, 20, 5));
        assert_eq!(chat.scroll_offset, 25);
        assert_eq!(screen, ["row 25", "row 26", "row 27", "row 28", "row 29"]);
    }

    #[test]
    fn render_paused_keeps_offset_and_paints_from_it() {
        let mut chat = test_chat();
        push_counting_block(&mut chat, 30);
        chat.auto_scroll = false;
        chat.scroll_offset = 10;
        let screen = screen_rows(&render_chat(&mut chat, 20, 3));
        assert_eq!(chat.scroll_offset, 10);
        assert_eq!(screen, ["row 10", "row 11", "row 12"]);
    }

    #[test]
    fn render_paused_past_the_end_clamps_to_the_last_page() {
        let mut chat = test_chat();
        push_counting_block(&mut chat, 30);
        chat.auto_scroll = false;
        chat.scroll_offset = 25;
        let screen = screen_rows(&render_chat(&mut chat, 20, 20));
        assert_eq!(screen[0], "row 10");
        assert!(chat.is_scrolled_up(), "a clamp alone keeps the pause");
    }

    #[test]
    fn render_applies_scrolls_and_rearms_auto_scroll_at_the_bottom() {
        let mut chat = test_chat();
        push_counting_block(&mut chat, 30);
        render_chat(&mut chat, 20, 5);
        for _ in 0..20 {
            chat.handle_event(&key_event(KeyCode::PageUp));
        }
        let screen = screen_rows(&render_chat(&mut chat, 20, 5));
        assert_eq!(screen[0], "row 0", "a burst past the top stops there");

        chat.scroll_offset = 22;
        chat.new_content_since_pause = 3;
        chat.handle_event(&key_event(KeyCode::Down));
        chat.handle_event(&key_event(KeyCode::Down));
        let screen = screen_rows(&render_chat(&mut chat, 20, 5));
        assert_eq!(screen[0], "row 24");
        assert!(chat.is_scrolled_up());

        chat.handle_event(&key_event(KeyCode::Down));
        let screen = screen_rows(&render_chat(&mut chat, 20, 5));
        assert_eq!(screen[0], "row 25");
        assert!(
            !chat.is_scrolled_up(),
            "reaching the bottom follows the tail again"
        );
        assert_eq!(chat.new_content_since_pause(), 0);
    }

    #[test]
    fn render_reuses_cached_block_renders_across_frames() {
        let mut chat = test_chat();
        let first = push_counting_block(&mut chat, 3);
        render_chat(&mut chat, 40, 10);
        render_chat(&mut chat, 40, 10);
        assert_eq!(first.get(), 1, "an unchanged block renders once");

        let second = push_counting_block(&mut chat, 3);
        chat.append_stream_token("streaming reply");
        render_chat(&mut chat, 40, 10);
        chat.append_stream_token(" keeps growing");
        let screen = screen_rows(&render_chat(&mut chat, 40, 10));
        assert_eq!(
            (first.get(), second.get()),
            (1, 1),
            "streaming tokens must not re-render committed blocks",
        );
        assert!(
            screen.iter().any(|row| row.contains("keeps growing")),
            "live tail still repaints: {screen:?}",
        );
    }

    #[test]
    fn render_rerenders_cached_blocks_only_on_width_change() {
        let mut chat = test_chat();
        let renders = push_counting_block(&mut chat, 1);
        render_chat(&mut chat, 40, 10);
        render_chat(&mut chat, 40, 6);
        assert_eq!(renders.get(), 1, "a height-only resize reuses the cache");

        render_chat(&mut chat, 30, 6);
        assert_eq!(renders.get(), 2, "a width change re-renders");
    }

    #[test]
    fn render_after_set_theme_repaints_cached_blocks_in_new_palette() {
        let latte = crate::tui::theme::load_builtin("latte").unwrap();
        let mocha = Theme::default();
        assert_ne!(latte.user().fg, mocha.user().fg);

        let mut chat = test_chat();
        chat.push_user_message("hello".to_owned());
        let before = render_chat(&mut chat, 40, 4);
        assert_eq!(before.buffer()[(0, 0)].fg, mocha.user().fg.unwrap());

        chat.set_theme(&latte);
        let after = render_chat(&mut chat, 40, 4);
        assert_eq!(after.buffer()[(0, 0)].fg, latte.user().fg.unwrap());
    }

    #[test]
    fn render_refreshes_only_blocks_near_the_viewport() {
        let mut chat = test_chat();
        let renders: Vec<_> = (0..40).map(|_| push_counting_block(&mut chat, 5)).collect();
        let counts = || renders.iter().map(|r| r.get()).collect::<Vec<_>>();

        render_chat(&mut chat, 20, 10);
        assert_eq!(counts(), [vec![0; 36], vec![1; 4]].concat(), "open");

        render_chat(&mut chat, 30, 10);
        chat.set_theme(&Theme::default());
        render_chat(&mut chat, 30, 10);
        assert_eq!(
            counts(),
            [vec![0; 36], vec![3; 4]].concat(),
            "resize and theme swap"
        );

        chat.handle_event(&ctrl_key_event(KeyCode::Home));
        render_chat(&mut chat, 30, 10);
        assert_eq!(
            counts(),
            [vec![1; 4], vec![0; 32], vec![3; 4]].concat(),
            "blocks render as they scroll into range",
        );

        for _ in 0..4 {
            chat.handle_event(&key_event(KeyCode::PageDown));
        }
        render_chat(&mut chat, 30, 10);
        render_chat(&mut chat, 40, 10);
        assert_eq!(
            counts(),
            [vec![1; 4], vec![2; 7], vec![0; 25], vec![3; 4]].concat(),
            "a paused resize re-renders a viewport height around the anchor in block 6",
        );
    }

    #[test]
    fn render_scrolls_unrendered_history_by_exact_rows() {
        let (width, height) = (60, 8);
        let mut reference = test_chat();
        reference.load_history(long_transcript(6));
        let rows: Vec<String> = all_lines(&mut reference, width)
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect();
        let top_row = |chat: &mut ChatView| {
            let screen = screen_rows(&render_chat(chat, width, height));
            rows.windows(usize::from(height))
                .position(|window| window == screen)
                .unwrap_or_else(|| panic!("not a window of the full render: {screen:?}"))
        };

        let page_up = |chat: &mut ChatView, pages| {
            for _ in 0..pages {
                chat.handle_event(&key_event(KeyCode::PageUp));
            }
        };

        let mut chat = test_chat();
        chat.load_history(long_transcript(6));
        render_chat(&mut chat, 30, height);
        page_up(&mut chat, 12);
        render_chat(&mut chat, 30, height);
        chat.handle_event(&ctrl_key_event(KeyCode::End));
        let bottom = rows.len() - usize::from(height);
        assert_eq!(top_row(&mut chat), bottom);

        let page = usize::from(height) - 2;
        page_up(&mut chat, 12);
        assert_eq!(
            top_row(&mut chat),
            bottom - 12 * page,
            "one frame applies the burst through blocks last rendered 30 columns wide",
        );
        page_up(&mut chat, 1);
        assert_eq!(top_row(&mut chat), bottom - 13 * page);

        chat.handle_event(&ctrl_key_event(KeyCode::Home));
        assert_eq!(top_row(&mut chat), 0);
        for _ in 0..3 {
            chat.handle_event(&key_event(KeyCode::PageDown));
        }
        assert_eq!(top_row(&mut chat), 3 * page);

        chat.handle_event(&ctrl_key_event(KeyCode::End));
        assert_eq!(top_row(&mut chat), bottom);
    }

    #[test]
    fn render_paused_inside_a_live_reply_stays_put_when_it_commits() {
        let reply = (0..30)
            .map(|i| format!("paragraph {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        for thinking in [false, true] {
            let mut chat = test_chat();
            chat.push_user_message("go".to_owned());
            render_chat(&mut chat, 40, 6);
            if thinking {
                chat.append_thinking_token(&reply);
            } else {
                chat.append_stream_token(&reply);
            }
            render_chat(&mut chat, 40, 6);
            for _ in 0..3 {
                chat.handle_event(&key_event(KeyCode::PageUp));
            }
            let before = screen_rows(&render_chat(&mut chat, 40, 6));

            chat.push_tool_call(call("$", "ls"));
            let after = screen_rows(&render_chat(&mut chat, 40, 6));
            assert_eq!(after, before, "thinking={thinking}");
        }
    }

    #[test]
    fn render_paused_resize_keeps_the_top_block_in_place() {
        let mut chat = test_chat();
        for index in 0..10 {
            chat.push_block(WrappingBlock { index });
        }
        render_chat(&mut chat, 20, 4);
        chat.handle_event(&ctrl_key_event(KeyCode::Home));
        render_chat(&mut chat, 20, 4);
        for _ in 0..4 {
            chat.handle_event(&key_event(KeyCode::Down));
        }
        let screen = screen_rows(&render_chat(&mut chat, 20, 4));
        assert_eq!(screen, ["b1 1", "b1 2", "b2 0", "b2 1"]);

        let screen = screen_rows(&render_chat(&mut chat, 10, 4));
        assert_eq!(
            screen,
            ["b1 2", "b1 3", "b1 4", "b1 5"],
            "b0 grew above the viewport, and row 1 of b1's 3 became row 2 of its 6",
        );
    }

    #[test]
    fn render_open_resize_and_theme_swap_paint_the_tail_of_the_full_render() {
        paint_timed_frames(4, 60, 8, 2);
    }

    #[test]
    #[ignore = "timing harness: run in release with --ignored --nocapture"]
    fn render_long_transcript_frame_timings() {
        eprintln!("{:?}", paint_timed_frames(200, 120, 40, 20));
    }

    #[test]
    fn render_reaches_rows_past_u16_max() {
        let rows = usize::from(u16::MAX) + 100;
        let mut chat = test_chat();
        push_counting_block(&mut chat, rows);

        let screen = screen_rows(&render_chat(&mut chat, 20, 2));
        assert_eq!(
            screen,
            [format!("row {}", rows - 2), format!("row {}", rows - 1)]
        );

        chat.auto_scroll = false;
        chat.scroll_offset = 65_600;
        let screen = screen_rows(&render_chat(&mut chat, 20, 2));
        assert_eq!(screen, ["row 65600", "row 65601"]);
    }

    #[test]
    fn render_empty_paints_blank_chat_region() {
        // ChatView paints only the surface background; App::draw_frame layers the welcome.
        let mut chat = test_chat();
        let backend = render_chat(&mut chat, 60, 8);
        for cell in &backend.buffer().content {
            assert_eq!(cell.symbol(), " ");
        }
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
        chat.push_tool_call(call("$", "echo hi"));
        chat.push_tool_result(titled_result("ran echo", "hi", false));
        insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
    }

    #[test]
    fn render_tool_call_with_edit_diff_result() {
        let mut chat = test_chat();
        chat.push_tool_call(call("✎", "Edit(/tmp/f.rs)"));
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
        chat.push_tool_call(call("✎", "Edit(/tmp/big.rs)"));
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
        chat.push_tool_call(call("✎", "Edit(/tmp/f.rs)"));
        let view = crate::tool::ToolResultView::Diff {
            chunks: vec![crate::tool::edit::synthesize_chunk(
                "fn foo() {}",
                "fn foo() -> i32 { 42 }",
            )],
            replace_all: false,
            replacements: 1,
        };
        chat.push_tool_result_view("Edited f.rs", view, true);
        let backend = render_chat(&mut chat, 60, 10);

        let theme = Theme::default();
        let buffer = backend.buffer();
        let error_fg = theme.error().fg.unwrap();
        assert_ne!(theme.tool_border().fg.unwrap(), error_fg);
        assert_eq!(buffer[(0, 0)].fg, theme.tool_border().fg.unwrap());
        for y in 1..=3 {
            assert_eq!(buffer[(0, y)].fg, error_fg, "result bar on row {y}");
        }
        assert_eq!(buffer[(2, 1)].fg, error_fg, "error indicator");
        assert_eq!(buffer[(4, 2)].bg, theme.diff_del.bg.unwrap());
        assert_eq!(buffer[(4, 3)].bg, theme.diff_add.bg.unwrap());
        insta::assert_snapshot!(backend);
    }

    #[test]
    fn render_tool_call_with_edit_diff_identical_sides_emits_no_change_marker() {
        let mut chat = test_chat();
        chat.push_tool_call(call("✎", "Edit(/tmp/f.rs)"));
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
        chat.push_tool_call(call("✎", "Edit(/tmp/f.rs)"));
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
        chat.push_tool_call(call("✎", "Edit(/tmp/f.rs)"));
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
        chat.push_tool_call(call("$", "ls"));
        let long = (0..5 + 3)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        chat.push_tool_result(titled_result("ls out", &long, false));
        insta::assert_snapshot!(render_chat(&mut chat, 60, 14));
    }

    #[test]
    fn render_error_entry_is_styled_distinctly() {
        let mut chat = test_chat();
        chat.push_error("API error (HTTP 503): overloaded");
        let backend = render_chat(&mut chat, 60, 4);

        let theme = Theme::default();
        let buffer = backend.buffer();
        let error_fg = theme.error().fg.unwrap();
        assert_ne!(theme.text().fg.unwrap(), error_fg);
        let message_width = u16::try_from("✗ API error (HTTP 503): overloaded".width()).unwrap();
        for x in 0..message_width {
            assert_eq!(buffer[(x, 0)].fg, error_fg, "column {x}");
            assert_eq!(buffer[(x, 0)].bg, theme.surface().bg.unwrap(), "column {x}");
        }
        insta::assert_snapshot!(backend);
    }

    #[test]
    fn render_history_with_resumed_thinking_block() {
        let mut chat = ChatView::new(&Theme::default(), true);
        chat.load_history(vec![
            Interaction::UserText("hello".to_owned()),
            Interaction::AssistantThinking("pondering...".to_owned()),
            Interaction::AssistantText("Hi!".to_owned()),
        ]);
        insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
    }

    // ── set_viewport ──

    #[test]
    fn set_viewport_adopts_area_and_drops_streaming_cache_on_width_change() {
        let mut chat = test_chat();
        chat.set_viewport(Rect::new(0, 0, 80, 24));
        assert_eq!((chat.viewport_width, chat.viewport_height), (80, 24));
        chat.append_stream_token("a complete paragraph\n\nnext\n");
        let s = chat.streaming.as_ref().unwrap();
        assert_ne!(s.rendered().len(), 0);
        assert_eq!(s.cached_width(), 80);

        chat.set_viewport(Rect::new(0, 0, 80, 10));
        assert_ne!(
            chat.streaming.as_ref().unwrap().rendered().len(),
            0,
            "a height-only change keeps the cache",
        );

        chat.set_viewport(Rect::new(0, 0, 40, 24));
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered().len(), 0);
        assert_eq!(s.rendered_boundary(), 0);
        assert_eq!(s.cached_width(), 0);
    }

    // ── scroll_down ──

    #[test]
    fn scroll_down_while_following_the_tail_is_ignored() {
        let mut chat = test_chat();
        chat.scroll_down(5);
        assert_eq!(chat.pending_scroll, 0);
        assert!(chat.auto_scroll);
    }

    // ── stack ──

    #[test]
    fn stack_empty_chat_has_no_rows() {
        let mut chat = test_chat();
        assert_eq!(line_count(&mut chat), 0);
    }

    #[test]
    fn stack_full_conversation() {
        let mut chat = test_chat();
        chat.push_user_message("What is 2+2?".to_owned());
        chat.push_block(AssistantText::new("The answer is 4."));
        chat.push_tool_call(call("$", "python -c 'print(2+2)'"));
        chat.push_tool_result(titled_result("4", "4", false));
        chat.push_user_message("Thanks!".to_owned());
        chat.append_stream_token("You're welcome");

        let text = all_text(&mut chat);
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
        assert_eq!(s.rendered().len(), 0);
    }

    #[test]
    fn advance_streaming_cache_line_boundary_does_not_commit() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("first line\nsecond line");
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), 0);
        assert_eq!(s.rendered().len(), 0);
    }

    #[test]
    fn advance_streaming_cache_paragraph_boundary_commits_once_the_next_line_ends() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("para1\n\npartial");
        assert_eq!(chat.streaming.as_ref().unwrap().rendered_boundary(), 0);

        chat.append_stream_token("\n");
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), "para1\n\n".len());
        assert_eq!(s.rendered().len(), 1);
    }

    #[test]
    fn advance_streaming_cache_multiple_paragraphs_commit_to_last_block_start() {
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("p1\n\np2\n\np3\n\np4\n");
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), "p1\n\np2\n\np3\n\n".len());
    }

    #[test]
    fn advance_streaming_cache_incremental_inserts_paragraph_gaps() {
        let mut chat = test_chat();
        chat.viewport_width = 80;

        chat.append_stream_token("para1\n\npara2\n");
        let (first_boundary, first_len) = {
            let s = chat.streaming.as_ref().unwrap();
            (s.rendered_boundary(), s.rendered().len())
        };
        assert_eq!(first_boundary, "para1\n\n".len());
        assert!(first_len >= 1);

        chat.append_stream_token("\npara3\n");
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), "para1\n\npara2\n\n".len());
        let final_len = s.rendered().len();
        assert!(
            final_len >= first_len + 2,
            "paragraph break must add >= 2 lines (blank + body): got {first_len} → {final_len}",
        );
    }

    #[test]
    fn advance_streaming_cache_waits_for_the_next_block() {
        // A loose list continues past a blank line, so no block is complete until another starts.
        let mut chat = test_chat();
        chat.viewport_width = 80;
        chat.append_stream_token("- item 1\n\n");
        assert_eq!(chat.streaming.as_ref().unwrap().rendered_boundary(), 0);

        chat.append_stream_token("- item 2\n\nafter\n");
        let s = chat.streaming.as_ref().unwrap();
        assert_eq!(s.rendered_boundary(), "- item 1\n\n- item 2\n\n".len());
    }

    #[test]
    fn advance_streaming_cache_matches_committed_render_at_every_char() {
        let body = indoc! {"
            ```
            ```

            Here:

            | a | b |
            | - | - |
            | 1 | 2 |

            text
            #tag

            para
            ```x
            code
            ```

            ```c
            #include <a.h>

            #define X 1
            - not a bullet
            ```

            [ref]: https://example.com

            ```
            ```

            <div>
            html
            </div>

            Done.
        "};
        let mut chat = test_chat();
        chat.set_viewport(Rect::new(0, 0, 80, 20));
        for (i, ch) in body.char_indices() {
            chat.append_stream_token(&ch.to_string());
            let prefix = &body[..i + ch.len_utf8()];
            let committed = AssistantText::new(prefix).render(&chat.settings.ctx(80));
            assert_eq!(all_lines(&mut chat, 80), committed, "after {prefix:?}");
        }
    }

    #[test]
    fn advance_streaming_cache_defers_until_viewport_measured() {
        let mut chat = test_chat();
        chat.append_stream_token("first paragraph\n\n");
        {
            let s = chat.streaming.as_ref().unwrap();
            assert_eq!(s.rendered().len(), 0);
            assert_eq!(s.rendered_boundary(), 0);
            assert_eq!(s.cached_width(), 0);
        }

        chat.set_viewport(Rect::new(0, 0, 80, 24));
        chat.append_stream_token("second paragraph\n\n");
        {
            let s = chat.streaming.as_ref().unwrap();
            assert_ne!(s.rendered().len(), 0);
            assert_eq!(s.cached_width(), 80);
        }
    }

    // ── is_empty ──

    #[test]
    fn is_empty_true_for_fresh_chat() {
        assert!(test_chat().is_empty());
    }

    #[test]
    fn is_empty_false_with_committed_block() {
        let mut chat = test_chat();
        chat.push_user_message("hi".to_owned());
        assert!(!chat.is_empty());
    }

    #[test]
    fn is_empty_false_with_in_flight_streaming_buffer() {
        let mut chat = test_chat();
        chat.append_stream_token("partial");
        assert!(!chat.is_empty());
    }

    #[test]
    fn is_empty_false_with_pending_thinking_buffer() {
        let mut chat = test_chat();
        chat.append_thinking_token("musing");
        assert!(!chat.is_empty());
    }

    #[test]
    fn is_empty_false_with_a_streaming_summary() {
        let mut chat = test_chat();
        chat.append_summary_token("recap");
        assert!(!chat.is_empty());
    }

    // ── rows_between ──

    #[test]
    fn rows_between_counts_rows_to_the_nearest_slot_edge() {
        assert_eq!(rows_between(&(5..8), 2), 3);
        assert_eq!(rows_between(&(5..8), 5), 0);
        assert_eq!(rows_between(&(5..8), 7), 0);
        assert_eq!(rows_between(&(5..8), 9), 2);
    }
}
