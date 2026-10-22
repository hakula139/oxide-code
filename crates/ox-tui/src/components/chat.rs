//! Scrollable chat view. Each visible transcript unit is a [`blocks::ChatBlock`] impl.
//! [`ChatView`] owns the streaming buffer, caches committed block renders, and stacks them with
//! separator lines, rendering only the blocks near the viewport. The compact view caps tool bodies
//! and collapses committed thinking, while the expanded view renders every block in full.

mod blocks;
mod scroll;
mod selection;
mod stack;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use ox_core::protocol::{Interaction, ToolCall, ToolResult};

use self::blocks::{
    AssistantText, AssistantThinking, ChatBlock, CompactedBlock, ErrorBlock, GitDiffBlock,
    InterruptedMarker, NoticeBlock, ProgressUpdate, RenderCtx, StreamingAssistant,
    SystemMessageBlock, ToolCallBlock, ToolResultBlock, UserMessage,
};
use self::selection::{ClickCounter, Selection};
use self::stack::{Anchor, CachedBlock, SlotTops, Stack};
use crate::action::{APP_KEYMAP, AppAction};
use crate::glyphs::SPINNER_FRAMES;
use crate::pending_calls::{PendingCalls, result_header};
use crate::slash::WorkingTreeDiff;
use crate::theme::Theme;

// ── ChatView ──

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
    /// Top row of a paint that reaches the bottom, as of the last paint.
    max_offset: usize,
    /// Rows scrolled since the last paint, positive downward. The next frame applies them.
    pending_scroll: isize,
    /// Chat area of the last paint.
    viewport: Rect,
    auto_scroll: bool,
    new_content_since_pause: u32,
    /// Whether `new_content_since_pause` counts the summary in flight.
    summary_counted: bool,
    /// Slot tops of the last paint, which the next frame anchors the scroll offset on.
    painted: SlotTops,
    /// Whether blocks expanded or collapsed since the last paint, so the next paint keeps the
    /// anchored row's index in its block.
    expansion_changed: bool,
    /// Rows of the last paint, which a double click finds its word in.
    painted_rows: Vec<Line<'static>>,

    selection: Option<Selection>,
    clicks: ClickCounter,

    pending_calls: PendingCalls,
    /// Spinner frame that heads thinking still streaming in.
    spinner: char,
}

/// Inputs shared by every block render.
struct RenderSettings {
    theme: Theme,
    /// Bumped per theme swap to invalidate cached block renders.
    theme_epoch: u64,
    show_thinking: bool,
    /// Whether the chat shows the expanded view.
    expanded: bool,
    /// The toggle's hint in the compact view, which overflow footers end with.
    expand_hint: Option<String>,
}

impl RenderSettings {
    fn ctx(&self, width: u16) -> RenderCtx<'_> {
        RenderCtx {
            width,
            theme: &self.theme,
            expanded: self.expanded,
            expand_hint: self.expand_hint.as_deref(),
        }
    }
}

/// Thinking, streaming-tail, and summary rows, rendered every frame as their buffers grow.
struct LiveLines {
    thinking: Vec<Line<'static>>,
    streaming_tail: Vec<Line<'static>>,
    summary: Vec<Line<'static>>,
}

// ── Public API ──

impl ChatView {
    pub(crate) fn new(theme: &Theme, show_thinking: bool) -> Self {
        Self {
            settings: RenderSettings {
                theme: theme.clone(),
                theme_epoch: 0,
                show_thinking,
                expanded: false,
                expand_hint: APP_KEYMAP.hint(AppAction::ToggleExpanded, &()),
            },
            blocks: Vec::new(),
            streaming: None,
            thinking_buffer: String::new(),
            summary_buffer: String::new(),
            scroll_offset: 0,
            max_offset: 0,
            pending_scroll: 0,
            viewport: Rect::default(),
            auto_scroll: true,
            new_content_since_pause: 0,
            summary_counted: false,
            painted: SlotTops::default(),
            expansion_changed: false,
            painted_rows: Vec::new(),
            selection: None,
            clicks: ClickCounter::default(),
            pending_calls: PendingCalls::default(),
            spinner: SPINNER_FRAMES[0],
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

    /// Sets the spinner frame the next paint heads streaming thinking with.
    pub(crate) fn set_spinner(&mut self, spinner: char) {
        self.spinner = spinner;
    }

    /// Switches between the compact and expanded views, returning each block expanded by a click
    /// to the view. A paused paint keeps the row on top in place.
    pub(crate) fn toggle_expanded(&mut self) {
        self.settings.expanded = !self.settings.expanded;
        for block in &mut self.blocks {
            block.reset_view();
        }
        self.drop_selection();
        self.expansion_changed = true;
    }

    /// Expands the block if the row `anchor` names is one a click expands it from. The paint
    /// pauses, so the rows on screen stay put while the block grows below them.
    fn expand_at(&mut self, anchor: Anchor) {
        let Some(block) = self.blocks.get_mut(anchor.block()) else {
            return;
        };
        if !block.expands_from(anchor.row()) {
            return;
        }
        block.expand();
        self.auto_scroll = false;
        self.expansion_changed = true;
    }

    /// Populates from a resumed transcript's interactions, matching the live block shapes.
    pub(crate) fn load_history(&mut self, history: Vec<Interaction>) {
        for interaction in history {
            match interaction {
                Interaction::Compacted {
                    summary,
                    pre_count,
                    instructions,
                } => {
                    self.push_compacted_block(pre_count, instructions.as_deref(), summary);
                }
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
        self.rearm_auto_scroll();
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
            .get_or_insert_with(StreamingAssistant::default)
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
            self.summary_counted = self.bump_paused_counter();
        }
        self.summary_buffer.push_str(token);
    }

    /// Drops the compaction summary in flight, which then no longer counts as new content.
    pub(crate) fn discard_summary(&mut self) {
        if std::mem::take(&mut self.summary_counted) {
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
        if let Some(streaming) = self.streaming.take() {
            let text = streaming.into_buffer();
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
        let call = self.pending_calls.remove(&result.id);
        let at = call.as_ref().map(|call| call.block + 1);
        let header = result_header(result.title, call.map(|call| call.label));
        let block = ToolResultBlock::new(&header, result.view, result.is_error);
        self.bump_paused_counter();
        match at {
            Some(at) if at < self.blocks.len() => self.insert_block(at, block),
            _ => self.push_block(block),
        }
    }

    /// Forgets calls whose results never arrived, such as those of a cancelled turn.
    pub(crate) fn clear_pending_calls(&mut self) {
        self.pending_calls.clear();
    }

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

    /// Appends the boundary block of a manual, automatic, or resumed compaction.
    pub(crate) fn push_compacted_block(
        &mut self,
        pre_count: u32,
        instructions: Option<&str>,
        summary: impl Into<String>,
    ) {
        self.bump_paused_counter();
        self.push_block(CompactedBlock::new(pre_count, instructions, summary));
    }

    pub(crate) fn push_git_diff(&mut self, diff: WorkingTreeDiff) {
        self.bump_paused_counter();
        self.push_block(GitDiffBlock::new(diff));
    }

    /// Appends an interrupted marker. Flushes any in-flight streaming buffer first.
    pub(crate) fn push_interrupted_marker(&mut self) {
        self.commit_streaming();
        self.bump_paused_counter();
        self.push_block(InterruptedMarker);
    }

    /// Drops the transcript and the last paint, keeping the render settings and the chat area.
    pub(crate) fn clear_history(&mut self) {
        self.blocks.clear();
        self.streaming = None;
        self.thinking_buffer.clear();
        self.summary_buffer.clear();
        self.scroll_offset = 0;
        self.max_offset = 0;
        self.pending_scroll = 0;
        self.rearm_auto_scroll();
        self.painted = SlotTops::default();
        self.painted_rows.clear();
        self.drop_selection();
        self.pending_calls.clear();
    }

    /// Returns whether the chat shows nothing: no committed block, reply, thinking, or summary.
    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
            && self.streaming.is_none()
            && self.thinking_buffer.is_empty()
            && self.summary_buffer.is_empty()
    }

    /// Number of committed chat blocks.
    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        self.blocks.len()
    }

    #[cfg(test)]
    pub(crate) fn pending_call_count(&self) -> usize {
        self.pending_calls.len()
    }

    /// Returns the error text of the tail block, if it is an [`ErrorBlock`].
    #[cfg(test)]
    pub(crate) fn last_error_text(&self) -> Option<&str> {
        self.blocks.last().and_then(|b| b.block().error_text())
    }

    /// Returns the body text of the tail block, if it is a [`SystemMessageBlock`].
    #[cfg(test)]
    pub(crate) fn last_system_text(&self) -> Option<&str> {
        self.blocks.last().and_then(|b| b.block().system_text())
    }
}

// ── Rendering ──

impl ChatView {
    /// Lays out the chat for `area`, follows the tail while auto-scroll is on, and paints only
    /// the rows inside the viewport.
    ///
    /// Blocks render only once the paint can reach them. While scrolled up, the paint anchors on
    /// the row the last frame showed on top, then moves by the pending scroll. Each pass renders
    /// the stale block nearest the pinned row and places the paint again. Painting waits until
    /// every block within reach is current, so estimated heights stay off the screen, and the
    /// anchor keeps the visible rows in place while blocks above them change height.
    pub(crate) fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
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
            self.painted_rows.clone_from(&lines);
            self.follow_pointer();
            let paragraph = Paragraph::new(lines).style(self.settings.theme.surface());
            frame.render_widget(paragraph, area);
            self.paint_selection(frame.buffer_mut());
            return;
        }
    }
}

// ── Private Helpers ──

impl ChatView {
    fn push_block(&mut self, block: impl ChatBlock + 'static) {
        let block = CachedBlock::new(Box::new(block), self.settings.show_thinking);
        self.blocks.push(block);
    }

    /// Inserts `block` at index `at`, so a paused view stays anchored on the block it showed.
    fn insert_block(&mut self, at: usize, block: impl ChatBlock + 'static) {
        let block = CachedBlock::new(Box::new(block), self.settings.show_thinking);
        self.blocks.insert(at, block);
        self.pending_calls.shift_from(at);
        self.painted.insert_empty(at);
        if let Some(selection) = &mut self.selection {
            selection.shift_from(at);
        }
    }

    /// Adopts the chat area. A width change drops the streaming prefix cache and the selection,
    /// whose columns no longer match the rewrapped rows.
    fn set_viewport(&mut self, area: Rect) {
        if area.width != self.viewport.width {
            self.drop_selection();
        }
        self.viewport = area;
        if let Some(s) = &mut self.streaming {
            s.invalidate_cache_for_width(area.width);
        }
    }

    fn render_live(&self, width: u16) -> LiveLines {
        let ctx = self.settings.ctx(width);
        let thinking = if self.settings.show_thinking && !self.thinking_buffer.is_empty() {
            AssistantThinking::render_streaming(&self.thinking_buffer, self.spinner, &ctx)
        } else {
            Vec::new()
        };
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
            s.advance_cache(&self.settings.ctx(self.viewport.width), continues);
        }
    }

    /// Returns whether streaming tokens continue the last committed assistant turn.
    fn streaming_continues_turn(&self) -> bool {
        self.blocks
            .last()
            .is_some_and(|b| b.block().continues_assistant_turn())
    }
}

// ── Testing ──

#[cfg(test)]
mod testing {
    use ratatui::text::Line;

    use super::blocks::{ChatBlock, RenderCtx};

    /// Renders fixed rows.
    pub(super) struct FixedBlock {
        rows: Vec<&'static str>,
        standalone: bool,
        hidden: bool,
    }

    impl FixedBlock {
        /// Rows that stack flush against their neighbors, like a tool row.
        pub(super) fn flush(rows: &[&'static str]) -> Self {
            Self {
                rows: rows.to_vec(),
                standalone: false,
                hidden: false,
            }
        }

        /// Rows that blank rows separate from their neighbors.
        pub(super) fn standalone(rows: &[&'static str]) -> Self {
            Self {
                standalone: true,
                ..Self::flush(rows)
            }
        }

        /// Standalone rows the chat never shows.
        pub(super) fn hidden(rows: &[&'static str]) -> Self {
            Self {
                hidden: true,
                ..Self::standalone(rows)
            }
        }
    }

    impl ChatBlock for FixedBlock {
        fn render(&self, _ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
            self.rows.iter().map(|row| Line::raw(*row)).collect()
        }

        fn standalone(&self) -> bool {
            self.standalone
        }

        fn visible(&self, _show_thinking: bool) -> bool {
            !self.hidden
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Instant;

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use indoc::{formatdoc, indoc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use unicode_width::UnicodeWidthStr as _;

    use ox_core::protocol::testing::text_result;
    use ox_core::tool::testing::edit_diff;
    use ox_core::tool::view::{NumberedLine, ToolResultView};

    use super::*;
    use crate::glyphs::{
        BAR, INTERRUPTED_MARKER, THINKING_PREFIX, TOOL_BORDER_CONT, TOOL_BORDER_PREFIX, TOOL_ERROR,
        TOOL_SUCCESS,
    };
    use crate::theme::builtin::find;

    fn test_chat() -> ChatView {
        ChatView::new(&Theme::default(), true)
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

    /// A result for the call `id`, headed by `title`.
    fn view_result(id: &str, title: &str, view: ToolResultView, is_error: bool) -> ToolResult {
        ToolResult {
            id: id.to_owned(),
            content: String::new(),
            is_error,
            title: Some(title.to_owned()),
            view,
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

    /// Rows the streaming prefix cache holds.
    fn cached_rows(chat: &ChatView) -> Vec<String> {
        let streaming = chat.streaming.as_ref().expect("a reply is streaming");
        streaming
            .rendered()
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

    fn key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl_key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
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

    mod set_theme {
        use super::*;

        #[test]
        fn repaints_the_streamed_prefix_in_the_new_palette() {
            // Bare `/theme` opens its picker mid-stream. Without dropping the cache, committed
            // prefix tokens would keep the previous palette until `commit_streaming`.
            let reply = "a complete paragraph\n\nnext\n";
            let latte = find("latte").expect("latte is built in").load();
            let mut chat = test_chat();
            chat.set_viewport(Rect::new(0, 0, 80, 24));
            chat.append_stream_token(reply);

            chat.set_theme(&latte);

            let committed = AssistantText::new(reply).render(&chat.settings.ctx(80));
            assert_eq!(all_lines(&mut chat, 80), committed);
        }
    }

    mod set_spinner {
        use super::*;

        #[test]
        fn heads_streaming_thinking_with_the_frame() {
            let mut chat = test_chat();

            chat.set_spinner('⣯');
            chat.append_thinking_token("pondering");

            assert!(all_text(&mut chat).contains("⣯ Thinking"));
        }
    }

    mod toggle_expanded {
        use super::*;

        /// Calls that each print `lines` numbered lines.
        fn long_results(calls: usize, lines: usize) -> Vec<Interaction> {
            (0..calls)
                .flat_map(|run| {
                    let label = format!("run {run}");
                    let output = (0..lines)
                        .map(|i| format!("run {run} line {i}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    [
                        Interaction::ToolCall(call("$", &label)),
                        Interaction::ToolResult(text_result(&label, &output, false, None)),
                    ]
                })
                .collect()
        }

        #[test]
        fn keeps_the_top_row_in_place_while_scrolled_up() {
            let mut chat = test_chat();
            chat.load_history(long_results(6, 12));
            render_chat(&mut chat, 60, 8);
            for _ in 0..3 {
                chat.handle_event(&key_event(KeyCode::PageUp));
            }
            for _ in 0..3 {
                chat.handle_event(&key_event(KeyCode::Down));
            }
            let compact = screen_rows(&render_chat(&mut chat, 60, 8));
            assert_eq!(compact[0], format!("{TOOL_BORDER_CONT}run 3 line 1"));

            chat.toggle_expanded();
            let expanded = screen_rows(&render_chat(&mut chat, 60, 8));
            chat.toggle_expanded();
            let collapsed = screen_rows(&render_chat(&mut chat, 60, 8));

            assert_eq!(
                expanded,
                (1..9)
                    .map(|i| format!("{TOOL_BORDER_CONT}run 3 line {i}"))
                    .collect::<Vec<_>>(),
            );
            assert_eq!(collapsed, compact);
        }

        #[test]
        fn follows_the_tail_when_not_scrolled_up() {
            let mut chat = test_chat();
            chat.load_history(long_results(2, 12));
            render_chat(&mut chat, 60, 8);

            chat.toggle_expanded();
            let screen = screen_rows(&render_chat(&mut chat, 60, 8));

            assert_eq!(
                screen.last().map(String::as_str),
                Some(format!("{TOOL_BORDER_CONT}run 1 line 11").as_str()),
            );
        }

        #[test]
        fn returns_a_block_expanded_by_a_click_to_the_view() {
            let mut chat = test_chat();
            chat.load_history(long_results(1, 12));
            chat.blocks[1].expand();

            chat.toggle_expanded();
            chat.toggle_expanded();

            let text = all_text(&mut chat);
            assert!(text.contains("... +7 lines · Ctrl+O to expand"), "{text}");
            assert!(!text.contains("run 0 line 11"), "{text}");
        }

        #[test]
        fn follows_the_tail_again_once_a_collapse_lands_on_it() {
            let mut chat = test_chat();
            chat.load_history(long_results(2, 12));
            let screen = screen_rows(&render_chat(&mut chat, 60, 8));
            let footer = screen
                .iter()
                .rposition(|row| row.contains("... +7 lines"))
                .unwrap();
            chat.expand_at(chat.painted.anchor_at(chat.scroll_offset + footer));
            render_chat(&mut chat, 60, 8);
            assert!(chat.is_scrolled_up(), "the click pauses the tail");

            chat.toggle_expanded();
            render_chat(&mut chat, 60, 8);
            chat.toggle_expanded();
            render_chat(&mut chat, 60, 8);

            assert!(!chat.is_scrolled_up());
            chat.push_tool_call(call("$", "later"));
            let screen = screen_rows(&render_chat(&mut chat, 60, 8));
            assert_eq!(
                screen.last().map(String::as_str),
                Some(format!("{TOOL_BORDER_PREFIX}$ later").as_str()),
            );
        }

        #[test]
        fn drops_a_held_selection_and_follows_the_tail_again() {
            let mut chat = test_chat();
            chat.load_history(long_results(1, 12));
            render_chat(&mut chat, 60, 8);
            let now = Instant::now();
            let mouse = |chat: &mut ChatView, kind, row| {
                let event = MouseEvent {
                    kind,
                    column: 4,
                    row,
                    modifiers: KeyModifiers::NONE,
                };
                chat.handle_mouse(event, now)
            };
            mouse(&mut chat, MouseEventKind::Down(MouseButton::Left), 2);
            mouse(&mut chat, MouseEventKind::Drag(MouseButton::Left), 4);

            chat.toggle_expanded();

            let copied = mouse(&mut chat, MouseEventKind::Up(MouseButton::Left), 4);
            assert_eq!(copied, None);
            assert!(!chat.is_scrolled_up());
        }
    }

    mod load_history {
        use super::*;

        #[test]
        fn renders_each_interaction_in_order() {
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
        fn titles_results_before_falling_back_for_orphans() {
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
        fn shows_progress_update_ahead_of_its_tool_call_only_while_thinking_is_hidden() {
            let history = || {
                vec![
                    Interaction::AssistantThinking("Running the suite.".to_owned()),
                    Interaction::ProgressUpdate("Running the suite.".to_owned()),
                    Interaction::ToolCall(call("$", "cargo test")),
                    Interaction::ToolResult(text_result("cargo test", "ok", false, None)),
                ]
            };
            for (show_thinking, expanded, shown) in
                [(false, false, 1), (true, false, 0), (true, true, 1)]
            {
                let mut chat = ChatView::new(&Theme::default(), show_thinking);
                if expanded {
                    chat.toggle_expanded();
                }
                chat.load_history(history());
                let text = all_text(&mut chat);
                let case = format!("show_thinking={show_thinking} expanded={expanded}: {text}");
                assert_eq!(text.matches("Running the suite.").count(), shown, "{case}");
                assert_eq!(text.contains("Thought"), show_thinking, "{case}");
                assert!(
                    text.find("Running the suite.") < text.find("cargo test"),
                    "{case}"
                );
            }
        }

        #[test]
        fn keeps_a_thought_that_doubles_as_the_progress_note_collapsed() {
            let thinking = (1..=3)
                .map(|i| format!("Step {i} of reading the loader."))
                .collect::<Vec<_>>()
                .join("\n\n");
            let mut chat = test_chat();

            chat.load_history(vec![
                Interaction::AssistantThinking(thinking.clone()),
                Interaction::ProgressUpdate(thinking),
                Interaction::ToolCall(call("$", "cargo test")),
            ]);

            let rows = all_rows(&mut chat, 60);
            assert!(
                rows.contains(&format!("{BAR} Thought · Ctrl+O to expand")),
                "{rows:?}"
            );
            assert!(!rows.iter().any(|row| row.contains("Step ")), "{rows:?}");
        }
    }

    mod push_user_message {
        use super::*;

        #[test]
        fn renders_the_prompt_after_its_icon_without_a_bar() {
            let mut chat = test_chat();

            chat.push_user_message("hello world".to_owned());

            assert_eq!(all_rows(&mut chat, 80), ["❯ hello world", ""]);
        }

        #[test]
        fn renders_every_line_of_a_multiline_prompt() {
            let mut chat = test_chat();

            chat.push_user_message(
                indoc! {"
                    line1
                    line2
                    line3
                "}
                .to_owned(),
            );

            assert_eq!(
                all_rows(&mut chat, 80),
                ["❯ line1", "  line2", "  line3", ""]
            );
        }

        #[test]
        fn sits_one_blank_row_above_the_next_block() {
            let mut before_call = test_chat();
            let mut before_reply = test_chat();

            before_call.push_user_message("hello".to_owned());
            before_call.push_tool_call(call("$", "ls"));
            before_reply.push_user_message("hello".to_owned());
            before_reply.push_block(AssistantText::new("reply"));

            assert_eq!(
                all_rows(&mut before_call, 80),
                ["❯ hello", "", format!("{TOOL_BORDER_PREFIX}$ ls").as_str()]
            );
            assert_eq!(
                all_rows(&mut before_reply, 80),
                ["❯ hello", "", "◉ reply", ""]
            );
        }

        #[test]
        fn follows_the_tail_again() {
            let mut chat = test_chat();
            chat.auto_scroll = false;

            chat.push_user_message("hello".to_owned());

            assert!(chat.auto_scroll);
        }
    }

    mod append_stream_token {
        use super::*;

        #[test]
        fn commits_pending_thinking_as_a_block_first() {
            let mut chat = test_chat();
            chat.append_thinking_token("reasoning here");

            chat.append_stream_token("reply text");

            assert_eq!(chat.thinking_buffer, "");
            assert_eq!(chat.blocks.len(), 1);
            chat.toggle_expanded();
            assert_eq!(
                all_rows(&mut chat, 80),
                [
                    format!("{THINKING_PREFIX}Thought").as_str(),
                    THINKING_PREFIX,
                    format!("{THINKING_PREFIX}reasoning here").as_str(),
                    "",
                    "◉ reply text",
                ]
            );
        }

        #[test]
        fn streams_a_new_turn_under_the_assistant_icon() {
            let mut chat = test_chat();
            chat.push_user_message("hi".to_owned());

            chat.append_stream_token("partial response");

            assert_eq!(all_rows(&mut chat, 80), ["❯ hi", "", "◉ partial response"]);
        }

        #[test]
        fn continues_a_committed_reply_without_a_second_icon() {
            let mut chat = test_chat();
            chat.push_block(AssistantText::new("committed"));

            chat.append_stream_token("streaming");

            assert_eq!(all_rows(&mut chat, 80), ["◉ committed", "", "  streaming"]);
        }

        #[test]
        fn sits_one_blank_row_below_a_tool_call() {
            let mut chat = test_chat();
            chat.push_tool_call(call("$", "ls"));

            chat.append_stream_token("response");

            assert_eq!(
                all_rows(&mut chat, 80),
                [
                    format!("{TOOL_BORDER_PREFIX}$ ls").as_str(),
                    "",
                    "◉ response"
                ]
            );
        }

        #[test]
        fn renders_every_line_whether_or_not_the_cache_advanced() {
            let cases: [(&[&str], &[&str]); 7] = [
                (
                    &["cached line\n", "tail text"],
                    &["◉ cached line tail text"],
                ),
                (
                    &["para1\n\npara2\n", "tail"],
                    &["◉ para1", "  ", "  para2 tail"],
                ),
                (&["para1\n\npara2\n"], &["◉ para1", "  ", "  para2"]),
                (
                    &["para1\n\n", "para2\n\ntail\n"],
                    &["◉ para1", "  ", "  para2", "  ", "  tail"],
                ),
                (&["- item 1\n- item 2"], &["◉ - item 1", "  - item 2"]),
                (
                    &["Here are items:\n- item 1"],
                    &["◉ Here are items:", "  ", "  - item 1"],
                ),
                (
                    &["committed paragraph\n\n", "still streaming"],
                    &["◉ committed paragraph", "  ", "  still streaming"],
                ),
            ];
            for viewport_width in [0, 80] {
                for (tokens, rows) in cases {
                    let mut chat = test_chat();
                    chat.viewport.width = viewport_width;

                    for token in tokens {
                        chat.append_stream_token(token);
                    }

                    assert_eq!(
                        all_rows(&mut chat, 80),
                        rows,
                        "viewport width {viewport_width}, tokens {tokens:?}",
                    );
                }
            }
        }
    }

    mod append_thinking_token {
        use super::*;

        #[test]
        fn streams_under_the_spinner_only_while_thinking_shows() {
            let mut shown = test_chat();
            let mut hidden = ChatView::new(&Theme::default(), false);

            shown.append_thinking_token("pondering...");
            hidden.append_thinking_token("pondering...");

            assert_eq!(
                all_rows(&mut shown, 80),
                [
                    format!("{THINKING_PREFIX}{} Thinking", SPINNER_FRAMES[0]).as_str(),
                    THINKING_PREFIX,
                    format!("{THINKING_PREFIX}pondering...").as_str(),
                ]
            );
            assert_eq!(all_rows(&mut hidden, 80), [] as [&str; 0]);
        }

        #[test]
        fn sits_one_blank_row_below_the_block_above() {
            let mut after_user = test_chat();
            let mut after_call = test_chat();
            after_user.push_user_message("hello".to_owned());
            after_call.push_tool_call(call("$", "ls"));

            after_user.append_thinking_token("deep thought");
            after_call.append_thinking_token("deep thought");

            let thinking = [
                format!("{THINKING_PREFIX}{} Thinking", SPINNER_FRAMES[0]),
                THINKING_PREFIX.to_owned(),
                format!("{THINKING_PREFIX}deep thought"),
            ];
            assert_eq!(
                all_rows(&mut after_user, 80),
                [vec!["❯ hello".to_owned(), String::new()], thinking.to_vec()].concat()
            );
            assert_eq!(
                all_rows(&mut after_call, 80),
                [
                    vec![format!("{TOOL_BORDER_PREFIX}$ ls"), String::new()],
                    thinking.to_vec()
                ]
                .concat()
            );
        }
    }

    mod append_summary_token {
        use super::*;

        #[test]
        fn streams_the_summary_a_blank_row_below_the_chat() {
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
    }

    mod discard_summary {
        use super::*;

        #[test]
        fn drops_the_streamed_summary_so_a_retry_starts_over() {
            let mut chat = test_chat();
            chat.push_tool_call(call("$", "ls"));
            let chat_rows = all_rows(&mut chat, 80);
            chat.append_summary_token("First attempt.");

            chat.discard_summary();
            assert_eq!(all_rows(&mut chat, 80), chat_rows);

            chat.append_summary_token("Second attempt.");
            let summary: Vec<String> =
                CompactedBlock::render_streaming("Second attempt.", &chat.settings.ctx(80))
                    .iter()
                    .map(ToString::to_string)
                    .collect();
            assert_eq!(
                all_rows(&mut chat, 80),
                [chat_rows, vec![String::new()], summary].concat(),
            );
        }

        #[test]
        fn stops_counting_the_summary_as_new_content_while_paused() {
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

        #[test]
        fn keeps_the_count_when_the_pause_began_after_the_summary() {
            let mut chat = test_chat();
            chat.append_summary_token("First attempt.");
            chat.auto_scroll = false;
            chat.push_notice("Retrying in 1 s (retry 1 of 5)");

            chat.discard_summary();

            assert_eq!(chat.new_content_since_pause(), 1, "the notice still counts");
        }

        #[test]
        fn keeps_the_count_when_a_jump_to_the_bottom_cleared_the_summary() {
            let mut chat = test_chat();
            chat.auto_scroll = false;
            chat.append_summary_token("First attempt.");
            chat.jump_to_bottom();
            chat.auto_scroll = false;
            chat.push_notice("Retrying in 1 s (retry 1 of 5)");

            chat.discard_summary();

            assert_eq!(chat.new_content_since_pause(), 1, "the notice still counts");
        }
    }

    mod commit_streaming {
        use super::*;

        #[test]
        fn moves_the_reply_into_a_committed_block() {
            let mut chat = test_chat();
            chat.append_stream_token("hello world");

            chat.commit_streaming();

            assert!(chat.streaming.is_none());
            assert_eq!(all_rows(&mut chat, 80), ["◉ hello world", ""]);
        }

        #[test]
        fn adds_no_block_when_nothing_streamed() {
            let mut chat = test_chat();

            chat.commit_streaming();

            assert!(chat.blocks.is_empty());
        }

        #[test]
        fn commits_a_thinking_only_turn() {
            let mut chat = test_chat();
            chat.append_thinking_token("plan before tool");

            chat.commit_streaming();

            assert_eq!(chat.thinking_buffer, "");
            chat.toggle_expanded();
            assert_eq!(
                all_rows(&mut chat, 80),
                [
                    format!("{THINKING_PREFIX}Thought"),
                    THINKING_PREFIX.to_owned(),
                    format!("{THINKING_PREFIX}plan before tool"),
                    String::new(),
                ]
            );
        }
    }

    mod push_progress_update {
        use super::*;

        #[test]
        fn shows_note_ahead_of_tool_call_only_while_thinking_is_hidden() {
            // Live, the note streams as thinking first and arrives again just before its tool call.
            for (show_thinking, expanded, shown) in
                [(false, false, 1), (true, false, 0), (true, true, 1)]
            {
                let mut chat = ChatView::new(&Theme::default(), show_thinking);
                if expanded {
                    chat.toggle_expanded();
                }
                chat.append_thinking_token("Running the suite.");
                chat.push_progress_update("Running the suite.".to_owned());
                chat.push_tool_call(call("$", "cargo test"));
                let text = all_text(&mut chat);
                let case = format!("show_thinking={show_thinking} expanded={expanded}: {text}");
                assert_eq!(text.matches("Running the suite.").count(), shown, "{case}");
                assert!(
                    text.find("Running the suite.") < text.find("cargo test"),
                    "{case}"
                );
            }
        }
    }

    mod push_tool_call {
        use super::*;

        #[test]
        fn renders_the_icon_and_label_on_the_bar() {
            let mut chat = test_chat();

            chat.push_tool_call(call("$", "ls -la"));

            assert_eq!(
                all_rows(&mut chat, 80),
                [format!("{TOOL_BORDER_PREFIX}$ ls -la")]
            );
        }

        #[test]
        fn sits_one_blank_row_below_assistant_text() {
            let mut chat = test_chat();
            chat.push_block(AssistantText::new("some text"));

            chat.push_tool_call(call("$", "ls"));

            assert_eq!(
                all_rows(&mut chat, 80),
                [
                    "◉ some text".to_owned(),
                    String::new(),
                    format!("{TOOL_BORDER_PREFIX}$ ls"),
                ]
            );
        }

        #[test]
        fn stacks_flush_against_a_call_and_one_borderless_row_below_a_result() {
            let mut chat = test_chat();
            chat.push_tool_call(call("$", "ls"));
            chat.push_tool_result(titled_result("ran ls", "output one", false));

            chat.push_tool_call(call("$", "pwd"));
            chat.push_tool_call(call("$", "cat foo"));

            assert_eq!(
                all_rows(&mut chat, 80),
                [
                    format!("{TOOL_BORDER_PREFIX}$ ls"),
                    format!("{TOOL_BORDER_PREFIX}{TOOL_SUCCESS} ran ls"),
                    format!("{TOOL_BORDER_CONT}output one"),
                    String::new(),
                    format!("{TOOL_BORDER_PREFIX}$ pwd"),
                    format!("{TOOL_BORDER_PREFIX}$ cat foo"),
                ]
            );
        }

        #[test]
        fn wraps_a_long_label_within_the_width() {
            let mut chat = test_chat();
            let long_cmd =
                "cd /home/user/projects/example-app && ls ${XDG_DATA_HOME:-$HOME/.local/share}/ox";

            chat.push_tool_call(call("$", long_cmd));

            let lines = all_lines(&mut chat, 60);
            assert!(lines.len() > 1, "{lines:?}");
            assert!(lines.iter().all(|line| line.width() <= 60), "{lines:?}");
        }
    }

    mod push_tool_result {
        use super::*;

        #[test]
        fn of_calls_that_ran_together_reads_like_the_resumed_chat() {
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
        fn placed_above_a_paused_view_keeps_it_in_place() {
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
        fn marks_a_success_with_a_check_above_its_output() {
            let mut chat = test_chat();

            chat.push_tool_result(titled_result("done", "output text", false));

            assert_eq!(
                all_rows(&mut chat, 80),
                [
                    format!("{TOOL_BORDER_PREFIX}{TOOL_SUCCESS} done"),
                    format!("{TOOL_BORDER_CONT}output text"),
                ]
            );
        }

        #[test]
        fn renders_a_structured_view_in_place_of_the_content() {
            let mut chat = test_chat();
            let lines = vec![NumberedLine {
                number: 2,
                text: "fn main() {".to_owned(),
            }];

            chat.push_tool_result(ToolResult {
                id: "read".to_owned(),
                content: "raw tool output".to_owned(),
                is_error: false,
                title: Some("Read example.rs".to_owned()),
                view: ToolResultView::ReadExcerpt {
                    path: "/tmp/example.rs".to_owned(),
                    lines,
                    total_lines: 5,
                },
            });

            assert_eq!(
                all_rows(&mut chat, 80),
                [
                    format!("{TOOL_BORDER_PREFIX}{TOOL_SUCCESS} Read example.rs"),
                    format!("{TOOL_BORDER_CONT}/tmp/example.rs:2 of 5"),
                    format!("{TOOL_BORDER_CONT}2 │ fn main() {{"),
                ]
            );
        }

        #[test]
        fn marks_an_error_with_a_cross_and_the_error_style_bar() {
            let mut chat = test_chat();

            chat.push_tool_result(titled_result("failed", "error details", true));

            assert_eq!(
                all_rows(&mut chat, 80),
                [
                    format!("{TOOL_BORDER_PREFIX}{TOOL_ERROR} failed"),
                    format!("{TOOL_BORDER_CONT}error details"),
                ]
            );
            let lines = all_lines(&mut chat, 80);
            let bars: Vec<_> = lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| span.content.contains(BAR))
                .map(|span| span.style)
                .collect();
            assert_eq!(bars, [Theme::default().error(); 2]);
        }

        #[test]
        fn wraps_a_long_title_within_the_width() {
            let mut chat = test_chat();
            let long_label =
                "some-very-long-file-path-that-exceeds.the.width.budget/and/then/more/path";

            chat.push_tool_result(titled_result(long_label, "", false));

            let lines = all_lines(&mut chat, 50);
            assert!(lines.len() > 1, "{lines:?}");
            assert!(lines.iter().all(|line| line.width() <= 50), "{lines:?}");
        }
    }

    mod push_error {
        use super::*;

        #[test]
        fn renders_the_message_after_a_cross_without_a_bar() {
            let mut chat = test_chat();

            chat.push_error("something broke");

            assert_eq!(
                all_rows(&mut chat, 80),
                [format!("{TOOL_ERROR} something broke"), String::new()]
            );
        }
    }

    mod clear_history {
        use super::*;

        #[test]
        fn drops_blocks_streaming_thinking_and_resets_scroll() {
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
            assert_eq!(chat.painted_rows, Vec::<Line<'static>>::new());
            assert!(chat.auto_scroll);
            assert_eq!(chat.viewport.height, 24);
            assert_eq!(chat.viewport.width, 80);
        }
    }

    mod render {
        use super::*;

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

        /// Committed thinking, then a long bash result and a long read.
        fn long_output_chat() -> ChatView {
            let mut chat = test_chat();
            chat.push_user_message("look around".to_owned());
            chat.append_thinking_token("List the files, then read the loader.");
            chat.push_tool_call(call("$", "ls"));
            let listing = (0..8)
                .map(|i| format!("file{i}.rs"))
                .collect::<Vec<_>>()
                .join("\n");
            chat.push_tool_result(text_result("ls", &listing, false, None));
            chat.push_tool_call(call("→", "Read(src/loader.rs)"));
            chat.push_tool_result(ToolResult {
                id: "Read(src/loader.rs)".to_owned(),
                content: String::new(),
                is_error: false,
                title: Some("Read loader.rs".to_owned()),
                view: ToolResultView::ReadExcerpt {
                    path: "src/loader.rs".to_owned(),
                    lines: (1..=8)
                        .map(|number| NumberedLine {
                            number,
                            text: format!("let step_{number} = load({number});"),
                        })
                        .collect(),
                    total_lines: 8,
                },
            });
            chat
        }

        #[test]
        fn auto_scroll_pins_tail_in_one_pass() {
            let mut chat = test_chat();
            push_counting_block(&mut chat, 30);
            let screen = screen_rows(&render_chat(&mut chat, 20, 5));
            assert_eq!(chat.scroll_offset, 25);
            assert_eq!(screen, ["row 25", "row 26", "row 27", "row 28", "row 29"]);
        }

        #[test]
        fn paused_keeps_offset_and_paints_from_it() {
            let mut chat = test_chat();
            push_counting_block(&mut chat, 30);
            chat.auto_scroll = false;
            chat.scroll_offset = 10;
            let screen = screen_rows(&render_chat(&mut chat, 20, 3));
            assert_eq!(chat.scroll_offset, 10);
            assert_eq!(screen, ["row 10", "row 11", "row 12"]);
        }

        #[test]
        fn paused_past_the_end_clamps_to_the_last_page() {
            let mut chat = test_chat();
            push_counting_block(&mut chat, 30);
            chat.auto_scroll = false;
            chat.scroll_offset = 25;
            let screen = screen_rows(&render_chat(&mut chat, 20, 20));
            assert_eq!(screen[0], "row 10");
            assert!(chat.is_scrolled_up(), "a clamp alone keeps the pause");
        }

        #[test]
        fn applies_scrolls_and_rearms_auto_scroll_at_the_bottom() {
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
        fn reuses_cached_block_renders_across_frames() {
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
        fn rerenders_cached_blocks_only_on_width_change() {
            let mut chat = test_chat();
            let renders = push_counting_block(&mut chat, 1);
            render_chat(&mut chat, 40, 10);
            render_chat(&mut chat, 40, 6);
            assert_eq!(renders.get(), 1, "a height-only resize reuses the cache");

            render_chat(&mut chat, 30, 6);
            assert_eq!(renders.get(), 2, "a width change re-renders");
        }

        #[test]
        fn after_set_theme_repaints_cached_blocks_in_new_palette() {
            let latte = find("latte").unwrap().load();
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
        fn refreshes_only_blocks_near_the_viewport() {
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
        fn scrolls_unrendered_history_by_exact_rows() {
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
        fn paused_inside_a_live_reply_stays_put_when_it_commits() {
            let reply = (0..30)
                .map(|i| format!("paragraph {i}"))
                .collect::<Vec<_>>()
                .join("\n\n");
            for thinking in [false, true] {
                let mut chat = test_chat();
                if thinking {
                    chat.toggle_expanded();
                }
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
        fn paused_inside_live_thinking_lands_on_its_header_once_it_collapses() {
            let thinking = (0..30)
                .map(|i| format!("paragraph {i}"))
                .collect::<Vec<_>>()
                .join("\n\n");
            let mut chat = test_chat();
            chat.push_user_message("go".to_owned());
            render_chat(&mut chat, 40, 6);
            chat.append_thinking_token(&thinking);
            render_chat(&mut chat, 40, 6);
            for _ in 0..3 {
                chat.handle_event(&key_event(KeyCode::PageUp));
            }
            render_chat(&mut chat, 40, 6);

            chat.push_tool_call(call("$", "ls"));
            let screen = screen_rows(&render_chat(&mut chat, 40, 6));

            assert!(
                screen.contains(&format!("{BAR} Thought · Ctrl+O to expand")),
                "{screen:?}"
            );
        }

        #[test]
        fn paused_resize_keeps_the_top_block_in_place() {
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
        fn paused_resize_after_a_view_toggle_keeps_the_relative_row() {
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
            render_chat(&mut chat, 20, 4);
            chat.toggle_expanded();
            let toggled = screen_rows(&render_chat(&mut chat, 20, 4));

            let resized = screen_rows(&render_chat(&mut chat, 10, 4));

            assert_eq!(toggled, ["b1 1", "b1 2", "b2 0", "b2 1"]);
            assert_eq!(resized, ["b1 2", "b1 3", "b1 4", "b1 5"]);
        }

        #[test]
        fn open_resize_and_theme_swap_paint_the_tail_of_the_full_render() {
            let (wide, narrow, height) = (60, 40, 8);
            let tail = |width| {
                let mut full = test_chat();
                full.load_history(long_transcript(4));
                let lines = all_lines(&mut full, width);
                lines[lines.len() - usize::from(height)..]
                    .iter()
                    .map(|line| line.to_string().trim_end().to_owned())
                    .collect::<Vec<_>>()
            };
            let (wide_tail, narrow_tail) = (tail(wide), tail(narrow));
            let latte = find("latte").expect("latte is built in").load();
            let mut chat = test_chat();
            chat.load_history(long_transcript(4));

            let opened = screen_rows(&render_chat(&mut chat, wide, height));
            let steady = screen_rows(&render_chat(&mut chat, wide, height));
            let narrowed = screen_rows(&render_chat(&mut chat, narrow, height));
            let widened = screen_rows(&render_chat(&mut chat, wide, height));
            chat.set_theme(&latte);
            let swapped = screen_rows(&render_chat(&mut chat, wide, height));

            assert_eq!(opened, wide_tail);
            assert_eq!(steady, wide_tail);
            assert_eq!(narrowed, narrow_tail);
            assert_eq!(widened, wide_tail);
            assert_eq!(swapped, wide_tail);
        }

        #[test]
        fn reaches_rows_past_u16_max() {
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
        fn paints_only_the_surface_background_when_empty() {
            let mut chat = test_chat();

            let backend = render_chat(&mut chat, 60, 8);

            let surface = Theme::default().surface().bg;
            assert!(
                backend
                    .buffer()
                    .content
                    .iter()
                    .all(|cell| cell.symbol() == " " && Some(cell.bg) == surface)
            );
        }

        #[test]
        fn puts_a_reply_one_blank_row_below_its_prompt() {
            let mut chat = test_chat();
            chat.push_user_message("what is 2 + 2?".to_owned());
            chat.append_stream_token("The answer is 4.");

            chat.commit_streaming();

            insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
        }

        #[test]
        fn stacks_a_result_flush_under_its_call() {
            let mut chat = test_chat();
            chat.push_tool_call(call("$", "echo hi"));

            chat.push_tool_result(titled_result("ran echo", "hi", false));

            insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
        }

        #[test]
        fn tool_call_with_edit_diff_result() {
            let mut chat = test_chat();
            chat.push_tool_call(call("✎", "Edit(/tmp/f.rs)"));
            let view = ToolResultView::Diff(edit_diff("fn foo() {}", "fn foo() -> i32 { 42 }"));
            chat.push_tool_result(view_result("Edit(/tmp/f.rs)", "Edited f.rs", view, false));
            insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
        }

        #[test]
        fn tool_call_with_edit_diff_over_budget_truncates_both_sides() {
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
            let view = ToolResultView::Diff(edit_diff(&old, &new));
            chat.push_tool_result(view_result(
                "Edit(/tmp/big.rs)",
                "Edited big.rs",
                view,
                false,
            ));
            insta::assert_snapshot!(render_chat(&mut chat, 60, 26));
        }

        #[test]
        fn tool_call_with_edit_diff_error_uses_error_border_color() {
            let mut chat = test_chat();
            chat.push_tool_call(call("✎", "Edit(/tmp/f.rs)"));
            let view = ToolResultView::Diff(edit_diff("fn foo() {}", "fn foo() -> i32 { 42 }"));
            chat.push_tool_result(view_result("Edit(/tmp/f.rs)", "Edited f.rs", view, true));
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
            assert_eq!(buffer[(4, 2)].bg, theme.diff_del_row().bg.unwrap());
            assert_eq!(buffer[(4, 3)].bg, theme.diff_add_row().bg.unwrap());
            insta::assert_snapshot!(backend);
        }

        #[test]
        fn tool_call_with_edit_diff_identical_sides_emits_no_change_marker() {
            let mut chat = test_chat();
            chat.push_tool_call(call("✎", "Edit(/tmp/f.rs)"));
            let view = ToolResultView::Diff(edit_diff("unchanged", "unchanged"));
            chat.push_tool_result(view_result("Edit(/tmp/f.rs)", "Edited f.rs", view, false));
            insta::assert_snapshot!(render_chat(&mut chat, 60, 6));
        }

        #[test]
        fn tool_call_with_edit_diff_trims_identical_boundary_lines() {
            let mut chat = test_chat();
            chat.push_tool_call(call("✎", "Edit(/tmp/f.rs)"));
            let view = ToolResultView::Diff(edit_diff("fn foo()", "fn foo()\n    return 42;"));
            chat.push_tool_result(view_result("Edit(/tmp/f.rs)", "Edited f.rs", view, false));
            insta::assert_snapshot!(render_chat(&mut chat, 60, 8));
        }

        #[test]
        fn tool_call_with_edit_diff_wraps_long_lines_under_bar() {
            let mut chat = test_chat();
            chat.push_tool_call(call("✎", "Edit(/tmp/f.rs)"));
            let view = ToolResultView::Diff(edit_diff(
                "short",
                "an intentionally quite long replacement line that forces wrapping",
            ));
            chat.push_tool_result(view_result("Edit(/tmp/f.rs)", "Edited f.rs", view, false));
            insta::assert_snapshot!(render_chat(&mut chat, 40, 10));
        }

        #[test]
        fn long_output_and_thinking_in_the_compact_view() {
            let mut chat = long_output_chat();
            insta::assert_snapshot!(render_chat(&mut chat, 60, 22));
        }

        #[test]
        fn long_output_and_thinking_in_the_expanded_view() {
            let mut chat = long_output_chat();
            chat.toggle_expanded();
            insta::assert_snapshot!(render_chat(&mut chat, 60, 30));
        }

        #[test]
        fn error_entry_is_styled_distinctly() {
            let mut chat = test_chat();
            chat.push_error("API error (HTTP 503): overloaded");
            let backend = render_chat(&mut chat, 60, 4);

            let theme = Theme::default();
            let buffer = backend.buffer();
            let error_fg = theme.error().fg.unwrap();
            assert_ne!(theme.text().fg.unwrap(), error_fg);
            let message_width =
                u16::try_from("✗ API error (HTTP 503): overloaded".width()).unwrap();
            for x in 0..message_width {
                assert_eq!(buffer[(x, 0)].fg, error_fg, "column {x}");
                assert_eq!(buffer[(x, 0)].bg, theme.surface().bg.unwrap(), "column {x}");
            }
            insta::assert_snapshot!(backend);
        }

        #[test]
        fn collapses_resumed_thinking_between_its_turns() {
            let mut chat = test_chat();

            chat.load_history(vec![
                Interaction::UserText("hello".to_owned()),
                Interaction::AssistantThinking("pondering...".to_owned()),
                Interaction::AssistantText("Hi!".to_owned()),
            ]);

            insta::assert_snapshot!(render_chat(&mut chat, 60, 10));
        }
    }

    mod set_viewport {
        use super::*;

        #[test]
        fn adopts_area_and_drops_streaming_cache_on_width_change() {
            let mut chat = test_chat();
            chat.set_viewport(Rect::new(0, 0, 80, 24));
            assert_eq!((chat.viewport.width, chat.viewport.height), (80, 24));
            chat.append_stream_token("a complete paragraph\n\nnext\n");
            assert_eq!(cached_rows(&chat), ["◉ a complete paragraph"]);

            chat.set_viewport(Rect::new(0, 0, 80, 10));
            assert_eq!(
                cached_rows(&chat),
                ["◉ a complete paragraph"],
                "a height-only change keeps the cache",
            );

            chat.set_viewport(Rect::new(0, 0, 40, 24));
            assert_eq!(cached_rows(&chat), [] as [String; 0]);
        }
    }

    mod stack {
        use super::*;

        fn line_count(chat: &mut ChatView) -> usize {
            all_lines(chat, 80).len()
        }

        #[test]
        fn empty_chat_has_no_rows() {
            let mut chat = test_chat();
            assert_eq!(line_count(&mut chat), 0);
        }

        #[test]
        fn full_conversation() {
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
    }

    mod advance_streaming_cache {
        use super::*;

        #[test]
        fn defers_until_viewport_measured() {
            let mut chat = test_chat();
            chat.append_stream_token("first paragraph\n\n");
            assert_eq!(cached_rows(&chat), [] as [String; 0]);

            chat.set_viewport(Rect::new(0, 0, 80, 24));
            chat.append_stream_token("second paragraph\n\n");

            assert_eq!(cached_rows(&chat), ["◉ first paragraph"]);
        }
    }

    mod is_empty {
        use super::*;

        #[test]
        fn holds_only_until_a_block_or_stream_arrives() {
            let mut committed = test_chat();
            let mut streaming = test_chat();
            let mut thinking = test_chat();
            let mut summary = test_chat();

            committed.push_user_message("hi".to_owned());
            streaming.append_stream_token("partial");
            thinking.append_thinking_token("musing");
            summary.append_summary_token("recap");

            assert!(test_chat().is_empty());
            assert_eq!(
                [&committed, &streaming, &thinking, &summary].map(ChatView::is_empty),
                [false; 4]
            );
        }
    }
}
