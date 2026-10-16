//! Per-block render cache and the row stack the chat view paints from.

use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::Line;

use super::blocks::{BlockKind, ChatBlock, RenderCtx, last_has_width};

/// Separator row borrowed by every stack gap, and the stand-in for a block that never rendered.
static BLANK: [Line<'static>; 1] = [Line {
    style: Style::new(),
    alignment: None,
    spans: Vec::new(),
}];

// ── CachedBlock ──

/// Render inputs that can change after a block is pushed.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RenderKey {
    width: u16,
    theme_epoch: u64,
}

/// A committed block with its most recent render. Blocks never change after push, so the cached
/// lines only go stale when the width or theme epoch changes.
pub(super) struct CachedBlock {
    block: Box<dyn ChatBlock>,
    key: Option<RenderKey>,
    visible: bool,
    lines: Vec<Line<'static>>,
}

impl CachedBlock {
    /// Settles the block's visibility, which depends only on settings fixed for the chat's life.
    pub(super) fn new(block: Box<dyn ChatBlock>, ctx: &RenderCtx<'_>) -> Self {
        let visible = block.visible(ctx);
        Self {
            block,
            key: None,
            visible,
            lines: Vec::new(),
        }
    }

    pub(super) fn block(&self) -> &dyn ChatBlock {
        self.block.as_ref()
    }

    pub(super) fn visible(&self) -> bool {
        self.visible
    }

    /// The latest render, which may predate the current width or theme. A block that never
    /// rendered stands in as one blank row, so every block keeps a row the viewport can land on.
    pub(super) fn lines(&self) -> &[Line<'static>] {
        if self.key.is_some() {
            &self.lines
        } else {
            &BLANK
        }
    }

    /// Whether the cached lines were rendered at this width and theme epoch.
    pub(super) fn is_current(&self, width: u16, theme_epoch: u64) -> bool {
        self.key == Some(RenderKey { width, theme_epoch })
    }

    /// Renders at this width and theme epoch, replacing the cached lines.
    pub(super) fn refresh(&mut self, ctx: &RenderCtx<'_>, theme_epoch: u64) {
        self.lines = if self.visible {
            self.block.render(ctx)
        } else {
            Vec::new()
        };
        self.key = Some(RenderKey {
            width: ctx.width,
            theme_epoch,
        });
    }
}

// ── SlotTops ──

/// A stack row named by the block slot that holds it, so a later stack can find the same row
/// after blocks change height.
#[derive(Clone, Copy)]
pub(super) struct Anchor {
    /// Index of the block, or the block count for rows past the last block.
    block: usize,
    /// Row within the slot, counting its leading separator.
    row: usize,
    /// Height of the slot when named, or `None` past the last block. Never zero.
    slot_height: Option<usize>,
}

/// First row of each block's slot in a stack, then the row after the last block.
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(super) struct SlotTops(Vec<usize>);

impl Default for SlotTops {
    fn default() -> Self {
        Self(vec![0])
    }
}

impl SlotTops {
    /// Names row `offset` by the last slot that starts at or above it, skipping empty slots. Rows
    /// past the last block are named by their distance from its end.
    pub(super) fn anchor_at(&self, offset: usize) -> Anchor {
        let block = self.0.partition_point(|&top| top <= offset) - 1;
        Anchor {
            block,
            row: offset - self.0[block],
            slot_height: self.slot(block).map(|slot| slot.len()),
        }
    }

    /// The row `anchor` names here. A row in a slot whose height changed keeps its relative
    /// position in the slot. The stack must hold every block the anchor's stack held.
    pub(super) fn offset_of(&self, anchor: Anchor) -> usize {
        let row = match (anchor.slot_height, self.slot(anchor.block)) {
            (Some(named), Some(slot)) => anchor.row * slot.len() / named,
            _ => anchor.row,
        };
        self.0[anchor.block] + row
    }

    /// Indices of the blocks whose slots hold rows in `rows`.
    pub(super) fn blocks_within(&self, rows: Range<usize>) -> impl Iterator<Item = usize> + '_ {
        let first = self.0.partition_point(|&top| top <= rows.start) - 1;
        let end = self.0.partition_point(|&top| top < rows.end);
        (first..end.min(self.0.len() - 1)).filter(|&block| self.0[block] < self.0[block + 1])
    }

    /// Rows of a block's slot, or `None` for the end of the blocks.
    pub(super) fn slot(&self, block: usize) -> Option<Range<usize>> {
        Some(self.0[block]..*self.0.get(block + 1)?)
    }
}

// ── Stack ──

/// Chat rows stacked top to bottom as runs borrowed from block renders. Every block is pushed
/// before any other row, so the slot tops start at row 0.
#[derive(Default)]
pub(super) struct Stack<'a> {
    runs: Vec<&'a [Line<'static>]>,
    height: usize,
    tops: SlotTops,
    prev_kind: Option<BlockKind>,
}

impl<'a> Stack<'a> {
    /// Stacks a block's rows in its slot. Standalone blocks sit between blank rows. A tool `Call`
    /// after a `Result` opens a new tool group, while a `Result` stays flush under its `Call`.
    pub(super) fn push_block(&mut self, cached: &'a CachedBlock) {
        if cached.visible() {
            let block = cached.block();
            if self.last_has_width()
                && (block.standalone() || self.prev_kind == Some(BlockKind::Result))
            {
                self.push_blank();
            }
            self.push(cached.lines());
            if block.standalone() {
                self.push_blank();
            }
            self.prev_kind = Some(block.block_kind());
        }
        self.tops.0.push(self.height);
    }

    pub(super) fn push(&mut self, lines: &'a [Line<'static>]) {
        if !lines.is_empty() {
            self.runs.push(lines);
            self.height += lines.len();
        }
    }

    pub(super) fn push_blank(&mut self) {
        self.push(&BLANK);
    }

    /// Stacks live rows a blank row below any content above them.
    pub(super) fn push_separated(&mut self, lines: &'a [Line<'static>]) {
        if lines.is_empty() {
            return;
        }
        if self.last_has_width() {
            self.push_blank();
        }
        self.push(lines);
    }

    /// Whether the bottom row has visible width, so the next block needs a separator.
    pub(super) fn last_has_width(&self) -> bool {
        self.runs.last().is_some_and(|run| last_has_width(run))
    }

    pub(super) fn height(&self) -> usize {
        self.height
    }

    pub(super) fn tops(&self) -> &SlotTops {
        &self.tops
    }

    pub(super) fn into_tops(self) -> SlotTops {
        self.tops
    }

    /// Clones up to `height` rows starting at row `offset`, skipping whole runs above it.
    pub(super) fn slice(&self, offset: usize, height: usize) -> Vec<Line<'static>> {
        let mut out = Vec::with_capacity(height);
        let mut skip = offset;
        for run in &self.runs {
            if out.len() == height {
                break;
            }
            if skip >= run.len() {
                skip -= run.len();
                continue;
            }
            let take = (run.len() - skip).min(height - out.len());
            out.extend_from_slice(&run[skip..skip + take]);
            skip = 0;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

    // ── Fixtures ──

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    /// Renders fixed rows.
    struct FixedBlock {
        rows: Vec<&'static str>,
        standalone: bool,
        hidden: bool,
    }

    impl ChatBlock for FixedBlock {
        fn render(&self, _ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
            self.rows.iter().map(|row| Line::raw(*row)).collect()
        }

        fn standalone(&self) -> bool {
            self.standalone
        }

        fn visible(&self, _ctx: &RenderCtx<'_>) -> bool {
            !self.hidden
        }
    }

    fn with_ctx<T>(width: u16, f: impl FnOnce(&RenderCtx<'_>) -> T) -> T {
        let theme = Theme::default();
        f(&RenderCtx {
            width,
            theme: &theme,
            show_thinking: true,
        })
    }

    fn cached(block: FixedBlock) -> CachedBlock {
        with_ctx(40, |ctx| CachedBlock::new(Box::new(block), ctx))
    }

    /// A block separated from its neighbors by blank rows.
    fn standalone(rows: &[&'static str]) -> CachedBlock {
        cached(FixedBlock {
            rows: rows.to_vec(),
            standalone: true,
            hidden: false,
        })
    }

    /// A block that stacks flush against its neighbors, like a tool row.
    fn flush(rows: &[&'static str]) -> CachedBlock {
        cached(FixedBlock {
            rows: rows.to_vec(),
            standalone: false,
            hidden: false,
        })
    }

    fn hidden(rows: &[&'static str]) -> CachedBlock {
        cached(FixedBlock {
            rows: rows.to_vec(),
            standalone: true,
            hidden: true,
        })
    }

    fn refresh(block: &mut CachedBlock, width: u16) {
        with_ctx(width, |ctx| block.refresh(ctx, 0));
    }

    /// Refreshes every block at width 40 and stacks them.
    fn stack_of(blocks: &mut [CachedBlock]) -> Stack<'_> {
        for block in blocks.iter_mut() {
            refresh(block, 40);
        }
        let mut stack = Stack::default();
        for block in &*blocks {
            stack.push_block(block);
        }
        stack
    }

    // ── CachedBlock::lines ──

    #[test]
    fn lines_stand_in_as_one_blank_row_until_the_first_render() {
        let mut block = standalone(&["a", "b"]);
        assert_eq!(texts(block.lines()), [""]);

        refresh(&mut block, 40);
        assert_eq!(texts(block.lines()), ["a", "b"]);

        let mut hidden = hidden(&["a"]);
        assert!(!hidden.visible());
        refresh(&mut hidden, 40);
        assert_eq!(texts(hidden.lines()), Vec::<String>::new());
    }

    // ── CachedBlock::is_current ──

    #[test]
    fn is_current_until_the_width_or_theme_epoch_changes() {
        let mut block = standalone(&["a"]);
        assert!(!block.is_current(40, 0));

        refresh(&mut block, 40);
        assert!(block.is_current(40, 0));
        assert!(!block.is_current(30, 0), "a width change is stale");
        assert!(!block.is_current(40, 1), "a theme swap is stale");
    }

    // ── SlotTops::anchor_at / SlotTops::offset_of ──

    #[test]
    fn anchor_at_names_the_slot_holding_the_row() {
        let mut blocks = [flush(&["a0", "a1"]), hidden(&["h"]), flush(&["c0", "c1"])];
        let stack = stack_of(&mut blocks);
        let tops = stack.tops();
        assert_eq!(*tops, SlotTops(vec![0, 2, 2, 4]));

        let anchor = |offset| {
            let anchor = tops.anchor_at(offset);
            (anchor.block, anchor.row)
        };
        assert_eq!(anchor(1), (0, 1));
        assert_eq!(anchor(2), (2, 0), "skips the hidden block's empty slot");
        assert_eq!(anchor(5), (3, 1), "rows past the end count from it");
        assert_eq!(SlotTops::default().anchor_at(3).row, 3);
    }

    #[test]
    fn offset_of_keeps_the_row_in_place_as_slots_change_height() {
        let painted = SlotTops(vec![0, 4, 8, 10]);
        let in_slot = painted.anchor_at(6);
        let past_end = painted.anchor_at(12);

        let taller = SlotTops(vec![0, 10, 18, 20, 23]);
        assert_eq!(
            taller.offset_of(in_slot),
            10 + 4,
            "row 2 of 4 becomes row 4 of 8"
        );
        assert_eq!(
            taller.offset_of(past_end),
            20 + 2,
            "a row past the old end keeps its distance from it"
        );
        assert_eq!(painted.offset_of(in_slot), 6);
    }

    // ── SlotTops::blocks_within ──

    #[test]
    fn blocks_within_lists_nonempty_slots_overlapping_the_rows() {
        let tops = SlotTops(vec![0, 2, 2, 5, 9]);
        let within = |rows| tops.blocks_within(rows).collect::<Vec<_>>();
        assert_eq!(within(0..1), [0]);
        assert_eq!(within(1..3), [0, 2], "the empty slot at row 2 is skipped");
        assert_eq!(within(4..20), [2, 3]);
        assert_eq!(within(9..20), Vec::<usize>::new(), "past the last block");
    }

    // ── Stack::push_block ──

    #[test]
    fn push_block_separates_standalone_blocks_and_records_slot_tops() {
        let mut blocks = [
            standalone(&["user"]),
            hidden(&["thought"]),
            flush(&["call"]),
            standalone(&["reply"]),
        ];
        let stack = stack_of(&mut blocks);

        assert_eq!(
            texts(&stack.slice(0, stack.height())),
            ["user", "", "call", "", "reply", ""],
        );
        assert_eq!(stack.into_tops(), SlotTops(vec![0, 2, 2, 3, 6]));
    }

    // ── Stack::last_has_width ──

    #[test]
    fn last_has_width_ignores_empty_runs() {
        let text = [Line::raw("text")];
        let mut stack = Stack::default();
        assert!(!stack.last_has_width());

        stack.push(&text);
        stack.push(&[]);
        assert!(
            stack.last_has_width(),
            "an empty push must not hide the last row"
        );

        stack.push_blank();
        assert!(!stack.last_has_width());
    }

    // ── Stack::slice ──

    #[test]
    fn slice_spans_run_boundaries() {
        let a = [Line::raw("a0"), Line::raw("a1")];
        let b = [Line::raw("b0"), Line::raw("b1"), Line::raw("b2")];
        let mut stack = Stack::default();
        stack.push(&a);
        stack.push_blank();
        stack.push(&b);

        assert_eq!(stack.height(), 6);
        assert_eq!(texts(&stack.slice(1, 3)), ["a1", "", "b0"]);
        assert_eq!(texts(&stack.slice(4, 10)), ["b1", "b2"]);
        assert_eq!(texts(&stack.slice(6, 3)), Vec::<String>::new());
        assert_eq!(texts(&stack.slice(0, 0)), Vec::<String>::new());
    }
}
