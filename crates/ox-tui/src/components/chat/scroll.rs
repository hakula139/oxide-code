//! Scrolling the chat: the keys and wheel that move it, where a frame places the paint, and the
//! auto-scroll that follows the tail.

use std::ops::Range;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use super::ChatView;
use super::selection::Selection;
use super::stack::{Anchor, SlotTops, Stack};

/// Rows one wheel notch scrolls, the common terminal default.
const WHEEL_SCROLL_ROWS: usize = 3;

/// Rows a page scroll keeps from the previous page.
const PAGE_OVERLAP: u16 = 2;

/// Where a frame paints the stack, and the rows that must be rendered at the current width and
/// theme for that paint to be exact.
pub(super) struct Placement {
    pub(super) offset: usize,
    max_offset: usize,
    /// The row rendering grows from: the last row under auto-scroll, else the anchored row.
    pin: usize,
    /// From the anchored row to the painted rows, plus a viewport height above and below.
    reach: Range<usize>,
}

// ── Scrolling ──

impl ChatView {
    pub(crate) fn handle_event(&mut self, event: &Event) {
        let page = usize::from(self.viewport.height.saturating_sub(PAGE_OVERLAP).max(1));
        match event {
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            }) => self.scroll_up(1),
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            }) => self.scroll_down(1),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                ..
            }) => self.scroll_up(WHEEL_SCROLL_ROWS),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                ..
            }) => self.scroll_down(WHEEL_SCROLL_ROWS),
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

    pub(crate) const fn is_scrolled_up(&self) -> bool {
        !self.auto_scroll
    }

    pub(crate) const fn new_content_since_pause(&self) -> u32 {
        self.new_content_since_pause
    }

    /// Re-arms auto-scroll, so the next frame shows the latest content.
    pub(super) fn jump_to_bottom(&mut self) {
        self.rearm_auto_scroll();
        self.pending_scroll = 0;
    }

    pub(super) fn scroll_up(&mut self, rows: usize) {
        self.pending_scroll = self.pending_scroll.saturating_sub_unsigned(rows);
        self.auto_scroll = false;
    }

    /// Scrolls toward the tail. The next frame re-arms auto-scroll once the paint reaches it.
    pub(super) fn scroll_down(&mut self, rows: usize) {
        if !self.auto_scroll {
            self.pending_scroll = self.pending_scroll.saturating_add_unsigned(rows);
        }
    }

    /// Follows the tail again, which clears the count of content that arrived while paused.
    pub(super) fn rearm_auto_scroll(&mut self) {
        self.auto_scroll = true;
        self.new_content_since_pause = 0;
        self.summary_counted = false;
    }

    /// Counts one new item while the user is scrolled up, and returns whether it counted.
    pub(super) fn bump_paused_counter(&mut self) -> bool {
        if self.auto_scroll {
            return false;
        }
        self.new_content_since_pause = self.new_content_since_pause.saturating_add(1);
        true
    }
}

// ── Placement ──

impl ChatView {
    /// Places the paint on `stack`: the tail under auto-scroll, else the anchored row moved by
    /// the pending scroll and clamped to the content. After blocks expand or collapse, the
    /// anchored row keeps its index in its block.
    pub(super) fn place(&self, stack: &Stack<'_>, anchor: Option<Anchor>) -> Placement {
        let viewport = usize::from(self.viewport.height);
        let max_offset = stack.height().saturating_sub(viewport);
        let Some(anchor) = anchor else {
            return Placement {
                offset: max_offset,
                max_offset,
                pin: stack.height().saturating_sub(1),
                reach: max_offset.saturating_sub(viewport)..stack.height() + viewport,
            };
        };

        let anchored = if self.expansion_changed {
            stack.tops().offset_keeping_row(anchor)
        } else {
            stack.tops().offset_of(anchor)
        };
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
    pub(super) fn nearest_stale(&self, tops: &SlotTops, placement: &Placement) -> Option<usize> {
        let ctx = self.settings.ctx(self.viewport.width);
        tops.blocks_within(placement.reach.clone())
            .filter(|&block| !self.blocks[block].is_current(&ctx, self.settings.theme_epoch))
            .min_by_key(|&block| {
                tops.slot(block)
                    .map_or(0, |slot| rows_between(&slot, placement.pin))
            })
    }

    /// Adopts the painted offset, re-arming auto-scroll when a scroll down, an expansion, or a
    /// collapse left the paint at the bottom. A held drag keeps the paint paused, so streamed rows
    /// do not slide under the pointer.
    pub(super) fn settle_scroll(&mut self, placement: &Placement) {
        let dragging = self.selection.as_ref().is_some_and(Selection::held);
        let moved = self.pending_scroll > 0 || self.expansion_changed;
        if moved && placement.offset == placement.max_offset && !dragging {
            self.rearm_auto_scroll();
        }
        self.pending_scroll = 0;
        self.expansion_changed = false;
        self.scroll_offset = placement.offset;
        self.max_offset = placement.max_offset;
    }
}

fn rows_between(slot: &Range<usize>, row: usize) -> usize {
    slot.start.saturating_sub(row) + (row + 1).saturating_sub(slot.end)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

    use super::*;
    use crate::theme::Theme;

    fn test_chat() -> ChatView {
        ChatView::new(&Theme::default(), true)
    }

    fn key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl_key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
    }

    mod handle_event {
        use super::*;

        fn mouse_scroll(kind: MouseEventKind) -> Event {
            Event::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        }

        #[test]
        fn arrows_scroll_one_row_and_wheel_notches_three() {
            let mut chat = test_chat();
            chat.handle_event(&key_event(KeyCode::Up));
            chat.handle_event(&mouse_scroll(MouseEventKind::ScrollUp));
            assert_eq!(chat.pending_scroll, -4);
            assert!(!chat.auto_scroll);

            chat.handle_event(&key_event(KeyCode::Down));
            chat.handle_event(&mouse_scroll(MouseEventKind::ScrollDown));
            chat.handle_event(&key_event(KeyCode::Down));
            assert_eq!(chat.pending_scroll, 1);
        }

        #[test]
        fn pages_scroll_two_rows_short_of_the_viewport() {
            let mut chat = test_chat();
            chat.viewport.height = 20;
            chat.handle_event(&key_event(KeyCode::PageUp));
            assert_eq!(chat.pending_scroll, -18);

            chat.handle_event(&key_event(KeyCode::PageDown));
            chat.handle_event(&key_event(KeyCode::PageDown));
            assert_eq!(chat.pending_scroll, 18);
        }

        #[test]
        fn pages_scroll_one_row_in_a_chat_no_taller_than_the_overlap() {
            let mut chat = test_chat();
            chat.viewport.height = PAGE_OVERLAP;

            chat.handle_event(&key_event(KeyCode::PageUp));

            assert_eq!(chat.pending_scroll, -1);
        }

        #[test]
        fn ctrl_home_jumps_to_the_top_and_ctrl_end_follows_the_tail() {
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
        fn unhandled_key_leaves_state_unchanged() {
            let mut chat = test_chat();
            chat.handle_event(&key_event(KeyCode::Char('a')));
            assert_eq!(chat.pending_scroll, 0);
            assert!(chat.auto_scroll);
        }
    }

    mod scroll_down {
        use super::*;

        #[test]
        fn while_following_the_tail_is_ignored() {
            let mut chat = test_chat();
            chat.scroll_down(5);
            assert_eq!(chat.pending_scroll, 0);
            assert!(chat.auto_scroll);
        }
    }

    mod bump_paused_counter {
        use super::*;

        #[test]
        fn bumps_only_while_scrolled_up() {
            let mut chat = test_chat();
            chat.push_system_message("one");
            assert_eq!(chat.new_content_since_pause(), 0);

            chat.auto_scroll = false;
            chat.push_system_message("two");
            chat.push_error("three");
            assert_eq!(chat.new_content_since_pause(), 2);
        }

        #[test]
        fn streaming_bumps_once_per_block() {
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
        fn does_not_overflow_at_u32_max() {
            let mut chat = test_chat();
            chat.auto_scroll = false;
            chat.new_content_since_pause = u32::MAX;

            chat.push_error("overflow");
            assert_eq!(chat.new_content_since_pause(), u32::MAX);
        }
    }

    mod rows_between {
        use super::*;

        #[test]
        fn counts_rows_to_the_nearest_slot_edge() {
            assert_eq!(rows_between(&(5..8), 2), 3);
            assert_eq!(rows_between(&(5..8), 5), 0);
            assert_eq!(rows_between(&(5..8), 7), 0);
            assert_eq!(rows_between(&(5..8), 9), 2);
        }
    }
}
