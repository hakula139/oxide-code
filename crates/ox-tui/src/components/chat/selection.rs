//! Mouse selection in the chat. A selection names its ends by content anchors, so it stays on the
//! same text while the chat streams, and yields the text it covers and the cells it highlights.

use std::ops::Range;
use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::{Buffer, CellWidth as _};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::Line;

use super::ChatView;
use super::stack::{Anchor, SlotTops};
use crate::saturating_u16;

/// Longest gap between presses on one cell that still counts toward a double or triple click.
const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(500);

/// Time between rows of auto-scroll while a drag holds the pointer past the chat's top or bottom.
const EDGE_SCROLL_INTERVAL: Duration = Duration::from_millis(50);

/// Rows auto-scroll moves without a mouse event before it stops, in case the terminal lost the
/// release. 10 s at [`EDGE_SCROLL_INTERVAL`].
const MAX_EDGE_SCROLLS: u16 = 200;

/// Graphemes that end a word for double-click selection, besides whitespace.
const WORD_DELIMITERS: &[&str] = &[
    "\"", "'", "`", "(", ")", "[", "]", "{", "}", "<", ">", ",", ";", "│", "▎",
];

// ── Selection ──

/// What one press selects, by how many quick presses landed on the same cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Cell,
    Word,
    Row,
}

/// A cell in the content: a stack row named by its anchor, and a column in that row.
#[derive(Clone, Copy)]
struct Point {
    anchor: Anchor,
    col: u16,
}

/// Cells from `start` to `end`, each a stack row and column, with `end` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionSpan {
    start: (usize, u16),
    end: (usize, u16),
}

impl SelectionSpan {
    /// Columns of `row` inside the span, or `None` when the span misses the row.
    fn cols(&self, row: usize) -> Option<Range<u16>> {
        if row < self.start.0 || row > self.end.0 {
            return None;
        }
        let from = if row == self.start.0 { self.start.1 } else { 0 };
        let to = if row == self.end.0 {
            self.end.1
        } else {
            u16::MAX
        };
        (from < to).then_some(from..to)
    }
}

/// The text a left-button drag selected: always the unit under the press, through the unit under
/// the pointer.
pub(super) struct Selection {
    unit: Unit,
    origin: [Point; 2],
    head: [Point; 2],
    /// Screen cell of the press.
    pressed: (u16, u16),
    /// Screen cell of the pointer while the button is held, `None` once it is released.
    pointer: Option<(u16, u16)>,
    /// Whether the selection grew past a plain click, which selects nothing.
    extended: bool,
    /// Whether the press landed on a painted row, where a plain click can expand a block.
    on_row: bool,
    /// Whether the selection paused following the tail, which the release resumes.
    paused_follow: bool,
    last_edge_scroll: Option<Instant>,
    /// Rows auto-scrolled since the last mouse event.
    edge_scrolls: u16,
}

impl Selection {
    fn span(&self, tops: &SlotTops) -> SelectionSpan {
        let resolve = |point: Point| (tops.offset_of(point.anchor), point.col);
        SelectionSpan {
            start: resolve(self.origin[0]).min(resolve(self.head[0])),
            end: resolve(self.origin[1]).max(resolve(self.head[1])),
        }
    }

    /// Keeps naming the same content after a block is inserted at index `at`.
    pub(super) fn shift_from(&mut self, at: usize) {
        for point in self.origin.iter_mut().chain(&mut self.head) {
            point.anchor.shift_from(at);
        }
    }

    /// Returns whether the left button is still down.
    pub(super) fn held(&self) -> bool {
        self.pointer.is_some()
    }

    /// Whether a drag past the edge of the chat `viewport` scrolls up or down, and when its next
    /// row is due. `None` when the pointer is inside the chat, the view already shows that end, or
    /// the drag spent its rows.
    fn edge_scroll(
        &self,
        viewport: Rect,
        scroll_offset: usize,
        max_offset: usize,
        now: Instant,
    ) -> Option<(bool, Instant)> {
        let (_, y) = self.pointer?;
        if self.edge_scrolls >= MAX_EDGE_SCROLLS {
            return None;
        }
        let up = if y < viewport.y && scroll_offset > 0 {
            true
        } else if y >= viewport.bottom() && scroll_offset < max_offset {
            false
        } else {
            return None;
        };
        let due = self
            .last_edge_scroll
            .map_or(now, |at| at + EDGE_SCROLL_INTERVAL);
        Some((up, due))
    }
}

// ── ClickCounter ──

/// Counts quick presses on one cell to pick what a press selects.
#[derive(Default)]
pub(super) struct ClickCounter {
    last: Option<(Instant, (u16, u16))>,
    count: u8,
}

impl ClickCounter {
    /// Counts a press at `cell` and returns what it selects: a cell for one press, a word for a
    /// double click, and a row for a triple click, after which the count starts over.
    fn press(&mut self, cell: (u16, u16), now: Instant) -> Unit {
        let repeat = self.last.is_some_and(|(at, last_cell)| {
            last_cell == cell && now.duration_since(at) <= MULTI_CLICK_WINDOW
        });
        self.count = if repeat { self.count % 3 + 1 } else { 1 };
        self.last = Some((now, cell));
        match self.count {
            1 => Unit::Cell,
            2 => Unit::Word,
            _ => Unit::Row,
        }
    }
}

// ── ChatView ──

impl ChatView {
    /// Starts, extends, or ends a left-button selection. A release returns the selected text,
    /// unless the press was a plain click.
    pub(crate) fn handle_mouse(&mut self, event: MouseEvent, now: Instant) -> Option<String> {
        let cell = (event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.press(cell, now),
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(cell),
            MouseEventKind::Up(MouseButton::Left) => return self.release(),
            _ => {}
        }
        None
    }

    pub(crate) fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    /// Clears the selection, returning its text.
    pub(crate) fn take_selection(&mut self) -> Option<String> {
        let text = self.selected_text();
        self.drop_selection();
        text
    }

    /// Clears the selection. Dropping one still held resumes following the tail its drag paused,
    /// since no release will find the selection to resume it.
    pub(super) fn drop_selection(&mut self) {
        if self
            .selection
            .take()
            .is_some_and(|selection| selection.held() && selection.paused_follow)
        {
            self.jump_to_bottom();
        }
    }

    /// Scrolls one row toward the pointer while a drag holds it past the chat's top or bottom,
    /// until the view reaches the content's end. Returns whether it scrolled.
    pub(crate) fn tick_selection(&mut self, now: Instant) -> bool {
        let Some(selection) = &mut self.selection else {
            return false;
        };
        let Some((up, due)) =
            selection.edge_scroll(self.viewport, self.scroll_offset, self.max_offset, now)
        else {
            return false;
        };
        if now < due {
            return false;
        }
        selection.last_edge_scroll = Some(now);
        selection.edge_scrolls += 1;
        if up {
            self.scroll_up(1);
        } else {
            self.scroll_down(1);
        }
        true
    }

    /// When [`Self::tick_selection`] next scrolls, or `None` while it has nothing to scroll.
    pub(crate) fn next_selection_tick(&self, now: Instant) -> Option<Instant> {
        let (_, due) = self.selection.as_ref()?.edge_scroll(
            self.viewport,
            self.scroll_offset,
            self.max_offset,
            now,
        )?;
        Some(due)
    }

    /// Moves the selection's head to the pointer once a frame has scrolled the content under it.
    pub(super) fn follow_pointer(&mut self) {
        let Some(selection) = &self.selection else {
            return;
        };
        let Some(pointer) = selection.pointer.filter(|&cell| !self.covers_row(cell)) else {
            return;
        };
        if let Some(head) = self.unit_at(pointer, selection.unit)
            && let Some(selection) = &mut self.selection
        {
            selection.head = head;
        }
    }

    /// Paints the selection highlight over the chat rows of the last paint.
    pub(super) fn paint_selection(&self, buf: &mut Buffer) {
        let Some(selection) = &self.selection else {
            return;
        };
        let span = selection.span(&self.painted);
        let area = self.viewport;
        let style = self.settings.theme.selection();
        for y in 0..area.height {
            let Some(cols) = span.cols(self.scroll_offset + usize::from(y)) else {
                continue;
            };
            for x in cols.start..cols.end.min(area.width) {
                buf[(area.x + x, area.y + y)].set_style(style);
            }
        }
    }

    fn press(&mut self, cell: (u16, u16), now: Instant) {
        self.drop_selection();
        if !self.viewport.contains(Position::from(cell)) {
            return;
        }
        let unit = self.clicks.press(cell, now);
        let Some(origin) = self.unit_at(cell, unit) else {
            return;
        };
        let extended = unit != Unit::Cell;
        let on_row = usize::from(cell.1 - self.viewport.y) < self.painted_rows.len();
        let paused_follow = extended && self.pause_follow();
        self.selection = Some(Selection {
            unit,
            origin,
            head: origin,
            pressed: cell,
            pointer: Some(cell),
            extended,
            on_row,
            paused_follow,
            last_edge_scroll: None,
            edge_scrolls: 0,
        });
    }

    fn drag_to(&mut self, cell: (u16, u16)) {
        let Some(selection) = &self.selection else {
            return;
        };
        if !selection.held() {
            return;
        }
        let head = self.unit_at(cell, selection.unit);
        let moved = cell != selection.pressed && !selection.extended;
        let paused = moved && self.pause_follow();
        if let Some(selection) = &mut self.selection {
            if let Some(head) = head {
                selection.head = head;
            }
            selection.pointer = Some(cell);
            selection.extended |= moved;
            selection.paused_follow |= paused;
            selection.edge_scrolls = 0;
        }
    }

    /// Ends the press. A plain click on a row a block expands from, such as an overflow footer,
    /// expands that block.
    fn release(&mut self) -> Option<String> {
        let selection = self.selection.as_mut()?;
        selection.pointer.take()?;
        if selection.paused_follow {
            self.jump_to_bottom();
        }
        let selection = self.selection.as_ref()?;
        if !selection.extended {
            if selection.on_row {
                self.expand_at(selection.origin[0].anchor);
            }
            self.selection = None;
            return None;
        }
        self.selected_text()
    }

    /// Renders the stale blocks the selection covers, such as a block pushed since the last frame,
    /// then returns the selected text, one line per row with trailing whitespace trimmed, or `None`
    /// when the selection holds only whitespace.
    fn selected_text(&mut self) -> Option<String> {
        let width = self.viewport.width;
        let live = self.render_live(width);
        loop {
            let stack = self.stack(&live);
            let span = self.selection.as_ref()?.span(stack.tops());
            let ctx = self.settings.ctx(width);
            let stale = stack
                .tops()
                .blocks_within(span.start.0..span.end.0 + 1)
                .find(|&block| !self.blocks[block].is_current(&ctx, self.settings.theme_epoch));
            if let Some(stale) = stale {
                drop(stack);
                self.blocks[stale].refresh(&ctx, self.settings.theme_epoch);
                continue;
            }
            let rows: Vec<String> = (span.start.0..=span.end.0)
                .filter_map(|row| Some(row_text(stack.row(row)?, span.cols(row)?)))
                .collect();
            let text = rows.join("\n");
            return (!text.trim().is_empty()).then_some(text);
        }
    }

    /// The unit at a screen cell, or `None` when the last paint showed no rows. A cell above the
    /// chat clamps to its first row's start, and one below the painted rows to the last row's end.
    fn unit_at(&self, (x, y): (u16, u16), unit: Unit) -> Option<[Point; 2]> {
        let area = self.viewport;
        let last_row = saturating_u16(self.painted_rows.len()).checked_sub(1)?;
        let (row_in_view, col) = if y < area.y {
            (0, 0)
        } else if y - area.y > last_row {
            (last_row, area.width)
        } else {
            let col = x.saturating_sub(area.x).min(area.width.saturating_sub(1));
            (y - area.y, col)
        };
        let anchor = self
            .painted
            .anchor_at(self.scroll_offset + usize::from(row_in_view));
        let line = &self.painted_rows[usize::from(row_in_view)];
        let cols = match unit {
            Unit::Cell => grapheme_bounds(line, col),
            Unit::Word => word_bounds(line, col),
            Unit::Row => 0..u16::MAX,
        };
        Some([
            Point {
                anchor,
                col: cols.start,
            },
            Point {
                anchor,
                col: cols.end,
            },
        ])
    }

    fn covers_row(&self, (_, y): (u16, u16)) -> bool {
        (self.viewport.y..self.viewport.bottom()).contains(&y)
    }

    /// Holds the paint where it is, so streamed output does not move text out from under a drag.
    /// Returns whether it paused following the tail.
    fn pause_follow(&mut self) -> bool {
        if !self.auto_scroll {
            return false;
        }
        self.auto_scroll = false;
        true
    }
}

// ── Cells ──

/// The text of the cells of `line` that overlap `cols`, with trailing whitespace trimmed.
fn row_text(line: &Line<'_>, cols: Range<u16>) -> String {
    let mut text = String::new();
    for (x, width, grapheme) in cells(line) {
        if x < cols.end && x.saturating_add(width) > cols.start {
            text.push_str(grapheme);
        }
    }
    text.truncate(text.trim_end().len());
    text
}

/// Columns of the grapheme covering `col`, so a press on the second cell of a wide character takes
/// the whole character. A column past the row's end is its own cell.
fn grapheme_bounds(line: &Line<'_>, col: u16) -> Range<u16> {
    cells(line)
        .find(|&(x, width, _)| x <= col && col < x.saturating_add(width))
        .map_or(col..col.saturating_add(1), |(x, width, _)| {
            x..x.saturating_add(width)
        })
}

/// Columns of the word at `col`, or of the grapheme there if it is whitespace or a delimiter.
fn word_bounds(line: &Line<'_>, col: u16) -> Range<u16> {
    let cells: Vec<_> = cells(line).collect();
    let end_of = |(x, width, _): (u16, u16, &str)| x.saturating_add(width);
    let Some(hit) = cells
        .iter()
        .position(|&cell| cell.0 <= col && col < end_of(cell))
    else {
        return col..col.saturating_add(1);
    };
    if !is_word(cells[hit].2) {
        return cells[hit].0..end_of(cells[hit]);
    }
    let first = cells[..hit]
        .iter()
        .rposition(|cell| !is_word(cell.2))
        .map_or(0, |i| i + 1);
    let last = cells[hit..]
        .iter()
        .position(|cell| !is_word(cell.2))
        .map_or(cells.len(), |i| hit + i)
        - 1;
    cells[first].0..end_of(cells[last])
}

fn is_word(grapheme: &str) -> bool {
    !grapheme.chars().all(char::is_whitespace) && !WORD_DELIMITERS.contains(&grapheme)
}

/// Each grapheme of `line` that ratatui paints, with its starting column and the cells it covers,
/// measured as ratatui measures them. Graphemes ratatui skips, those holding a control character
/// and zero-width ones, take no column.
fn cells<'a>(line: &'a Line<'_>) -> impl Iterator<Item = (u16, u16, &'a str)> {
    let mut x: u16 = 0;
    line.styled_graphemes(Style::new())
        .filter_map(move |grapheme| {
            let width = grapheme.symbol.cell_width();
            if width == 0 {
                return None;
            }
            let start = x;
            x = x.saturating_add(width);
            Some((start, width, grapheme.symbol))
        })
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    use ox_core::protocol::ToolCall;
    use ox_core::protocol::testing::text_result;

    use super::*;
    use crate::components::chat::testing::FixedBlock;
    use crate::glyphs::TOOL_BORDER_CONT;
    use crate::theme::Theme;

    mod click_counter {
        use super::*;

        mod press {
            use super::*;

            #[test]
            fn counts_quick_presses_on_one_cell_up_to_a_row() {
                let mut clicks = ClickCounter::default();
                let start = Instant::now();
                let at = |ms| start + Duration::from_millis(ms);

                let units: Vec<Unit> = [0, 100, 200, 300]
                    .into_iter()
                    .map(|ms| clicks.press((3, 1), at(ms)))
                    .collect();

                assert_eq!(units, [Unit::Cell, Unit::Word, Unit::Row, Unit::Cell]);
                assert_eq!(clicks.press((3, 1), at(1000)), Unit::Cell, "too slow");
                assert_eq!(clicks.press((4, 1), at(1100)), Unit::Cell, "another cell");
            }
        }
    }

    mod chat_view {
        use super::*;

        /// Chat area one row below the top of a 6-row screen, so screen rows differ from chat rows.
        const AREA: Rect = Rect::new(0, 1, 20, 4);

        fn chat_of(rows: &[&'static str]) -> ChatView {
            let mut chat = ChatView::new(&Theme::default(), false);
            chat.push_block(FixedBlock::flush(rows));
            paint_in(&mut chat, AREA);
            chat
        }

        fn paint_in(chat: &mut ChatView, area: Rect) -> Buffer {
            let mut terminal = Terminal::new(TestBackend::new(area.width, 6)).unwrap();
            terminal.draw(|frame| chat.render(frame, area)).unwrap();
            terminal.backend().buffer().clone()
        }

        fn paint(chat: &mut ChatView) -> Buffer {
            paint_in(chat, AREA)
        }

        fn mouse(
            chat: &mut ChatView,
            kind: MouseEventKind,
            (x, y): (u16, u16),
            now: Instant,
        ) -> Option<String> {
            chat.handle_mouse(
                MouseEvent {
                    kind,
                    column: x,
                    row: y,
                    modifiers: KeyModifiers::NONE,
                },
                now,
            )
        }

        /// Presses at `from` and drags to `to`, holding the button down.
        fn hold(chat: &mut ChatView, from: (u16, u16), to: (u16, u16), now: Instant) {
            mouse(chat, MouseEventKind::Down(MouseButton::Left), from, now);
            mouse(chat, MouseEventKind::Drag(MouseButton::Left), to, now);
        }

        /// Releases the button at `cell`, returning the copied text.
        fn release(chat: &mut ChatView, cell: (u16, u16), now: Instant) -> Option<String> {
            mouse(chat, MouseEventKind::Up(MouseButton::Left), cell, now)
        }

        /// Presses at `from`, drags to `to`, and releases there, returning the copied text.
        fn drag(chat: &mut ChatView, from: (u16, u16), to: (u16, u16)) -> Option<String> {
            let now = Instant::now();
            hold(chat, from, to, now);
            release(chat, to, now)
        }

        /// Screen cell where `needle` first shows in `buf`.
        fn find(buf: &Buffer, needle: &str) -> (u16, u16) {
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    let rest: String = (x..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                    if rest.starts_with(needle) {
                        return (x, y);
                    }
                }
            }
            panic!("{needle:?} is not on screen")
        }

        /// Rows `r0` through `r9`, which overflow the chat area.
        const TEN_ROWS: [&str; 10] = ["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9"];

        fn highlighted(buf: &Buffer, x: u16, y: u16) -> bool {
            buf[(x, y)].bg == Theme::default().selection().bg.unwrap()
        }

        #[test]
        fn edge_scroll_stops_at_the_bottom_and_the_release_follows_the_tail_again() {
            let mut chat = chat_of(&TEN_ROWS);
            let now = Instant::now();
            hold(&mut chat, (0, 2), (0, 5), now);

            assert!(!chat.tick_selection(now), "already at the bottom");
            chat.handle_event(&Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 2,
                modifiers: KeyModifiers::NONE,
            }));
            chat.push_block(FixedBlock::flush(&["streamed"]));
            paint(&mut chat);
            assert!(chat.is_scrolled_up(), "a held drag keeps the paint paused");
            let copied = release(&mut chat, (0, 5), now);

            assert_eq!(copied.as_deref(), Some("r7\nr8\nr9\nstreamed"));
            assert!(!chat.is_scrolled_up(), "the release follows the tail again");
        }

        mod handle_mouse {
            use super::*;

            fn click(chat: &mut ChatView, cell: (u16, u16), now: Instant) -> Option<String> {
                mouse(chat, MouseEventKind::Down(MouseButton::Left), cell, now);
                release(chat, cell, now)
            }

            fn tool_call(label: &str) -> ToolCall {
                ToolCall {
                    id: label.to_owned(),
                    name: "grep".to_owned(),
                    input: serde_json::json!({}),
                    icon: "*".to_owned(),
                    label: label.to_owned(),
                }
            }

            /// Results of eight lines for the calls `ids`, which the compact view caps at five with
            /// a footer, painted at the tail of a 10-row chat.
            fn long_results(ids: &[&str]) -> (ChatView, Vec<String>) {
                let output = (0..8)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut chat = ChatView::new(&Theme::default(), false);
                for id in ids {
                    chat.push_tool_call(tool_call(id));
                    chat.push_tool_result(text_result(id, &output, false, None));
                }
                let screen = paint_rows(&mut chat);
                (chat, screen)
            }

            fn paint_rows(chat: &mut ChatView) -> Vec<String> {
                let area = Rect::new(0, 0, 40, 10);
                let mut terminal =
                    Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
                terminal.draw(|frame| chat.render(frame, area)).unwrap();
                let buf = terminal.backend().buffer();
                (0..area.height)
                    .map(|y| {
                        let row: String = (0..area.width).map(|x| buf[(x, y)].symbol()).collect();
                        row.trim_end().to_owned()
                    })
                    .collect()
            }

            fn screen_row(index: usize) -> u16 {
                u16::try_from(index).unwrap()
            }

            #[test]
            fn drag_copies_from_the_press_through_the_pointer_in_either_direction() {
                let mut chat = chat_of(&["alpha beta", "gamma delta", "epsilon"]);

                let forward = drag(&mut chat, (6, 1), (4, 2));
                let backward = drag(&mut chat, (4, 2), (6, 1));

                assert_eq!(forward.as_deref(), Some("beta\ngamma"));
                assert_eq!(backward, forward);
            }

            #[test]
            fn plain_click_selects_nothing() {
                let mut chat = chat_of(&["alpha beta"]);
                drag(&mut chat, (0, 1), (4, 1));

                let copied = click(&mut chat, (2, 1), Instant::now());

                assert_eq!(copied, None);
                assert!(chat.selection.is_none(), "a click clears the old selection");
            }

            #[test]
            fn plain_click_on_an_overflow_footer_expands_its_block_below_the_rows_on_screen() {
                let (mut chat, before) = long_results(&["Grep(a)", "Grep(b)"]);
                let footers: Vec<usize> = (0..before.len())
                    .filter(|&row| before[row].contains("... +3 lines"))
                    .collect();
                assert_eq!(footers.len(), 2, "{before:?}");
                let footer = footers[1];

                let copied = click(&mut chat, (8, screen_row(footer)), Instant::now());

                let after = paint_rows(&mut chat);
                assert_eq!(copied, None);
                assert_eq!(after[..footer], before[..footer], "the rows above stay put");
                assert_eq!(after[footer], format!("{TOOL_BORDER_CONT}line 5"));
                assert!(
                    after[footers[0]].contains("... +3 lines"),
                    "the other block stays compact"
                );
            }

            #[test]
            fn plain_click_off_a_footer_expands_nothing() {
                let (mut chat, before) = long_results(&["Grep(a)", "Grep(b)"]);
                let body = before
                    .iter()
                    .rposition(|row| row.contains("line 0"))
                    .unwrap();

                click(&mut chat, (8, screen_row(body)), Instant::now());

                assert_eq!(paint_rows(&mut chat), before);
                assert!(!chat.is_scrolled_up());
            }

            #[test]
            fn drag_from_an_overflow_footer_selects_without_expanding() {
                let (mut chat, before) = long_results(&["Grep(a)", "Grep(b)"]);
                let footer = before
                    .iter()
                    .rposition(|row| row.contains("... +3 lines"))
                    .unwrap();

                let copied = drag(&mut chat, (0, screen_row(footer)), (39, screen_row(footer)));

                assert_eq!(copied.as_deref(), Some(before[footer].as_str()));
                assert_eq!(paint_rows(&mut chat), before);
            }

            #[test]
            fn plain_click_below_a_footer_that_ends_the_chat_expands_nothing() {
                let (mut chat, before) = long_results(&["Grep(a)"]);
                let footer = before
                    .iter()
                    .rposition(|row| row.contains("... +3 lines"))
                    .unwrap();
                assert_eq!(
                    before[footer + 1..],
                    ["", ""],
                    "blank space below the footer"
                );

                click(&mut chat, (8, screen_row(footer + 1)), Instant::now());

                assert_eq!(paint_rows(&mut chat), before);
            }

            #[test]
            fn plain_click_on_a_live_reply_expands_nothing() {
                let mut chat = chat_of(&["alpha"]);
                chat.append_stream_token("beta gamma");
                let cell = find(&paint(&mut chat), "beta");

                let copied = click(&mut chat, cell, Instant::now());

                assert_eq!(copied, None);
                assert!(!chat.is_scrolled_up(), "the paint keeps following the tail");
            }

            #[test]
            fn double_and_triple_clicks_select_a_word_and_a_row() {
                let mut chat = chat_of(&["let x = foo(bar_baz);"]);
                let now = Instant::now();

                assert_eq!(click(&mut chat, (13, 1), now), None);
                let word = click(&mut chat, (13, 1), now);
                let row = click(&mut chat, (13, 1), now);

                assert_eq!(word.as_deref(), Some("bar_baz"));
                assert_eq!(row.as_deref(), Some("let x = foo(bar_baz);"));
            }

            #[test]
            fn ignores_presses_outside_the_chat_and_stray_events() {
                let mut chat = chat_of(&["alpha beta"]);
                let now = Instant::now();

                let outside = drag(&mut chat, (0, 0), (4, 1));
                mouse(
                    &mut chat,
                    MouseEventKind::Drag(MouseButton::Left),
                    (4, 1),
                    now,
                );
                let moved = mouse(&mut chat, MouseEventKind::Moved, (4, 1), now);

                assert_eq!((outside, moved), (None, None));
                assert!(chat.selection.is_none());
            }

            #[test]
            fn selection_keeps_its_text_as_blocks_arrive_around_it() {
                let mut chat = chat_of(&["alpha beta", "gamma delta"]);
                let now = Instant::now();
                hold(&mut chat, (6, 1), (4, 2), now);
                assert!(chat.is_scrolled_up(), "a drag pauses following the tail");

                chat.insert_block(0, FixedBlock::flush(&["inserted above"]));
                chat.push_block(FixedBlock::flush(&["streamed"; 6]));
                let buf = paint(&mut chat);
                let copied = release(&mut chat, (4, 2), now);

                assert_eq!(copied.as_deref(), Some("beta\ngamma"));
                assert!(
                    highlighted(&buf, 6, 1),
                    "the paused view keeps the text in place"
                );
            }

            #[test]
            fn selection_on_a_call_stays_put_when_a_result_lands_above_it() {
                let area = Rect::new(0, 1, 30, 5);
                let mut chat = ChatView::new(&Theme::default(), false);
                chat.push_tool_call(tool_call("Grep(a)"));
                chat.push_tool_call(tool_call("Grep(b)"));
                let (x, y) = find(&paint_in(&mut chat, area), "Grep(b)");
                let now = Instant::now();
                hold(&mut chat, (x, y), (x + 6, y), now);

                chat.push_tool_result(text_result("Grep(a)", "a hits", false, None));
                let buf = paint_in(&mut chat, area);
                let copied = release(&mut chat, (x + 6, y), now);

                assert_eq!(copied.as_deref(), Some("Grep(b)"));
                let (x, y) = find(&buf, "Grep(b)");
                assert!(highlighted(&buf, x, y) && highlighted(&buf, x + 6, y));
                assert!(
                    !highlighted(&buf, x, y - 1),
                    "the separator above stays plain"
                );
            }

            #[test]
            fn release_reads_a_block_pushed_since_the_last_frame() {
                let mut chat = chat_of(&["alpha"]);
                chat.append_stream_token("beta gamma");
                let (x, y) = find(&paint(&mut chat), "beta");
                let now = Instant::now();
                hold(&mut chat, (x, y), (x + 3, y), now);

                chat.commit_streaming();
                let copied = release(&mut chat, (x + 3, y), now);

                assert_eq!(copied.as_deref(), Some("beta"));
            }

            #[test]
            fn drag_into_blank_space_ends_at_the_last_painted_row() {
                let mut chat = chat_of(&["alpha beta"]);
                let now = Instant::now();
                hold(&mut chat, (6, 1), (2, 3), now);

                chat.push_block(FixedBlock::flush(&["pushed"]));
                paint(&mut chat);
                let copied = release(&mut chat, (2, 3), now);

                assert_eq!(copied.as_deref(), Some("beta"));
            }

            #[test]
            fn selects_nothing_once_the_chat_is_cleared() {
                let mut chat = chat_of(&["old text"]);
                let now = Instant::now();
                hold(&mut chat, (0, 1), (2, 1), now);

                chat.clear_history();
                let across_clear = release(&mut chat, (2, 1), now);
                let cleared = !chat.has_selection();
                let over_welcome = drag(&mut chat, (1, 1), (2, 1));
                let welcome_untouched = !chat.has_selection();
                chat.push_block(FixedBlock::flush(&["new text"]));
                paint(&mut chat);
                let after_content = drag(&mut chat, (0, 1), (2, 1));

                assert_eq!((across_clear, cleared), (None, true));
                assert_eq!((over_welcome, welcome_untouched), (None, true));
                assert_eq!(after_content.as_deref(), Some("new"));
            }

            #[test]
            fn whitespace_only_drag_copies_nothing() {
                let mut chat = chat_of(&["a      b"]);

                let copied = drag(&mut chat, (2, 1), (5, 1));

                assert_eq!(copied, None);
            }

            #[test]
            fn press_on_a_wide_character_takes_all_of_it() {
                let mut chat = chat_of(&["日本語"]);
                let now = Instant::now();
                hold(&mut chat, (1, 1), (2, 1), now);

                let buf = paint(&mut chat);

                assert!(highlighted(&buf, 0, 1), "the press moved to the lead cell");
                assert!(!highlighted(&buf, 4, 1));
            }

            #[test]
            fn measures_rows_as_ratatui_paints_them() {
                let mut chat = chat_of(&["a\x1b[1mb\u{200b} ｶﾞ word"]);
                let (x, y) = find(&paint(&mut chat), "word");
                let now = Instant::now();

                let row = drag(&mut chat, (0, y), (x + 3, y));
                let drawn = drag(&mut chat, (x + 3, y), (x, y));
                click(&mut chat, (x, y), now);
                let word = click(&mut chat, (x, y), now);

                assert_eq!(row.as_deref(), Some("a[1mb ｶﾞ word"));
                assert_eq!(drawn.as_deref(), Some("word"), "ｶﾞ covers two cells");
                assert_eq!(word.as_deref(), Some("word"));
            }

            #[test]
            fn press_after_a_lost_release_follows_the_tail_again() {
                let mut chat = chat_of(&TEN_ROWS);
                let now = Instant::now();
                hold(&mut chat, (0, 2), (1, 3), now);

                click(&mut chat, (0, 4), now);
                chat.push_block(FixedBlock::flush(&["streamed"]));
                let buf = paint(&mut chat);

                assert!(!chat.is_scrolled_up());
                assert_eq!(find(&buf, "streamed"), (0, 4));
            }
        }

        mod take_selection {
            use super::*;

            #[test]
            fn returns_the_text_and_follows_the_tail_again_mid_drag() {
                let mut chat = chat_of(&TEN_ROWS);
                let now = Instant::now();
                hold(&mut chat, (0, 3), (1, 4), now);
                assert!(chat.is_scrolled_up(), "the drag pauses the tail");

                let text = chat.take_selection();

                chat.push_block(FixedBlock::flush(&["streamed"]));
                let buf = paint(&mut chat);
                assert_eq!(text.as_deref(), Some("r8\nr9"));
                assert_eq!(release(&mut chat, (1, 4), now), None);
                assert!(!chat.is_scrolled_up());
                assert_eq!(find(&buf, "streamed"), (0, 4));
            }
        }

        mod drop_selection {
            use super::*;

            #[test]
            fn keeps_a_pause_the_drag_did_not_make() {
                let mut chat = chat_of(&TEN_ROWS);
                chat.scroll_up(1);
                paint(&mut chat);
                hold(&mut chat, (0, 2), (1, 3), Instant::now());

                chat.drop_selection();

                assert!(chat.is_scrolled_up());
            }

            #[test]
            fn keeps_a_pause_made_after_the_release() {
                let mut chat = chat_of(&TEN_ROWS);
                let now = Instant::now();
                hold(&mut chat, (0, 2), (1, 3), now);
                release(&mut chat, (1, 3), now);
                chat.scroll_up(1);

                chat.drop_selection();

                assert!(chat.is_scrolled_up());
            }
        }

        mod tick_selection {
            use super::*;

            #[test]
            fn scrolls_toward_a_pointer_held_past_the_edge() {
                let mut chat = chat_of(&TEN_ROWS);
                let now = Instant::now();
                hold(&mut chat, (0, 3), (0, 0), now);

                assert!(chat.tick_selection(now));
                assert!(
                    !chat.tick_selection(now + Duration::from_millis(10)),
                    "throttled"
                );
                paint(&mut chat);
                assert!(chat.tick_selection(now + EDGE_SCROLL_INTERVAL));
                let buf = paint(&mut chat);
                let copied = release(&mut chat, (0, 0), now);

                assert_eq!(copied.as_deref(), Some("r4\nr5\nr6\nr7\nr"));
                assert!(
                    highlighted(&buf, 0, 1),
                    "the head follows the content under the edge"
                );
                assert!(
                    !chat.tick_selection(now + 2 * EDGE_SCROLL_INTERVAL),
                    "released"
                );
            }

            #[test]
            fn scrolls_down_toward_a_pointer_below_the_chat() {
                let mut chat = chat_of(&TEN_ROWS);
                chat.scroll_up(10);
                paint(&mut chat);
                let now = Instant::now();
                hold(&mut chat, (0, 1), (0, 5), now);

                assert!(chat.tick_selection(now));
                paint(&mut chat);
                let copied = release(&mut chat, (0, 5), now);

                assert_eq!(copied.as_deref(), Some("r0\nr1\nr2\nr3\nr4"));
            }

            #[test]
            fn gives_up_after_max_edge_scrolls_without_a_mouse_event() {
                let mut chat = chat_of(&["row"; 300]);
                let start = Instant::now();
                hold(&mut chat, (0, 2), (0, 0), start);
                let mut now = start;
                let mut scrolls = 0;

                while chat.tick_selection(now) && scrolls <= MAX_EDGE_SCROLLS {
                    scrolls += 1;
                    paint(&mut chat);
                    now += EDGE_SCROLL_INTERVAL;
                }
                mouse(
                    &mut chat,
                    MouseEventKind::Drag(MouseButton::Left),
                    (0, 0),
                    now,
                );

                assert_eq!(scrolls, MAX_EDGE_SCROLLS);
                assert!(chat.tick_selection(now), "a drag event starts it over");
            }

            #[test]
            fn leaves_a_pointer_inside_the_chat_alone() {
                let mut chat = chat_of(&["alpha beta"]);
                let now = Instant::now();
                hold(&mut chat, (0, 1), (4, 1), now);

                assert!(!chat.tick_selection(now));
            }
        }

        mod next_selection_tick {
            use super::*;

            #[test]
            fn is_due_at_once_and_then_one_interval_after_each_scroll() {
                let mut chat = chat_of(&TEN_ROWS);
                let now = Instant::now();
                hold(&mut chat, (0, 3), (0, 0), now);
                assert_eq!(chat.next_selection_tick(now), Some(now));

                assert!(chat.tick_selection(now));

                assert_eq!(
                    chat.next_selection_tick(now),
                    Some(now + EDGE_SCROLL_INTERVAL)
                );
            }

            #[test]
            fn never_scrolls_for_a_pointer_inside_the_chat() {
                let mut chat = chat_of(&TEN_ROWS);
                let now = Instant::now();
                hold(&mut chat, (0, 1), (2, 2), now);

                assert_eq!(chat.next_selection_tick(now), None);
            }
        }

        mod paint_selection {
            use super::*;

            #[test]
            fn highlights_only_the_selected_cells() {
                let mut chat = chat_of(&["alpha beta", "gamma delta"]);
                let now = Instant::now();
                hold(&mut chat, (6, 1), (4, 2), now);

                let buf = paint(&mut chat);

                assert!(!highlighted(&buf, 5, 1));
                assert!(highlighted(&buf, 6, 1));
                assert!(
                    highlighted(&buf, 19, 1),
                    "middle and first rows run to the edge"
                );
                assert!(highlighted(&buf, 4, 2));
                assert!(!highlighted(&buf, 5, 2));
                assert!(!highlighted(&buf, 0, 3));
            }
        }

        mod set_viewport {
            use super::*;

            #[test]
            fn width_change_drops_the_selection_and_follows_the_tail_again() {
                let mut chat = chat_of(&["alpha beta"]);
                let now = Instant::now();
                hold(&mut chat, (0, 1), (4, 1), now);

                paint_in(&mut chat, Rect { width: 12, ..AREA });

                assert!(chat.selection.is_none());
                assert!(!chat.is_scrolled_up());
            }

            #[test]
            fn height_change_keeps_the_selection() {
                let mut chat = chat_of(&["alpha beta"]);
                let now = Instant::now();
                hold(&mut chat, (6, 1), (9, 1), now);

                paint_in(&mut chat, Rect { height: 3, ..AREA });
                let copied = release(&mut chat, (9, 1), now);

                assert_eq!(copied.as_deref(), Some("beta"));
            }
        }
    }

    mod row_text {
        use super::*;

        #[test]
        fn takes_each_grapheme_that_overlaps_the_columns() {
            let line = Line::raw("日本語 text   ");

            assert_eq!(row_text(&line, 1..3), "日本");
            assert_eq!(row_text(&line, 4..u16::MAX), "語 text");
        }
    }

    mod grapheme_bounds {
        use super::*;

        #[test]
        fn covers_every_cell_of_a_wide_character() {
            let line = Line::raw("a日b");

            assert_eq!(grapheme_bounds(&line, 2), 1..3, "the second cell of 日");
            assert_eq!(grapheme_bounds(&line, 3), 3..4);
            assert_eq!(grapheme_bounds(&line, 9), 9..10, "past the end of the row");
        }
    }

    mod word_bounds {
        use super::*;

        #[test]
        fn stops_at_whitespace_and_delimiters() {
            let line = Line::raw("let x = foo(bar_baz);");

            assert_eq!(word_bounds(&line, 14), 12..19, "bar_baz");
            assert_eq!(word_bounds(&line, 11), 11..12, "a delimiter selects itself");
            assert_eq!(word_bounds(&line, 3), 3..4, "so does a space");
            assert_eq!(word_bounds(&line, 40), 40..41, "past the end of the row");
        }

        #[test]
        fn measures_wide_graphemes_by_cells() {
            let line = Line::from(vec!["日本".into(), "語 text".into()]);

            assert_eq!(word_bounds(&line, 3), 0..6);
            assert_eq!(word_bounds(&line, 8), 7..11);
        }
    }
}
