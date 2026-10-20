//! Per-block render cache and the row stack the chat view paints from.

use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::Line;

use super::blocks::{BlockRows, ChatBlock, RenderCtx, last_has_width};

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
    expanded: bool,
}

/// A committed block with its most recent render. Blocks never change after push, so the cached
/// lines only go stale when the width, theme epoch, or expanded state changes.
pub(super) struct CachedBlock {
    block: Box<dyn ChatBlock>,
    /// Whether a click expanded this block alone, which the compact view renders in full.
    expanded: bool,
    key: Option<RenderKey>,
    visible: bool,
    rows: BlockRows,
}

impl CachedBlock {
    /// Settles the block's visibility, which depends only on settings fixed for the chat's life.
    pub(super) fn new(block: Box<dyn ChatBlock>, ctx: &RenderCtx<'_>) -> Self {
        let visible = block.visible(ctx);
        Self {
            block,
            expanded: false,
            key: None,
            visible,
            rows: BlockRows::default(),
        }
    }

    pub(super) fn block(&self) -> &dyn ChatBlock {
        self.block.as_ref()
    }

    pub(super) fn visible(&self) -> bool {
        self.visible
    }

    /// The latest render, which may predate the current width, theme, or view. A block that never
    /// rendered stands in as one blank row, so every block keeps a row the viewport can land on.
    pub(super) fn lines(&self) -> &[Line<'static>] {
        if self.key.is_some() {
            &self.rows.lines
        } else {
            &BLANK
        }
    }

    /// Returns whether a click on row `row` of the latest render expands the block.
    pub(super) fn expands_from(&self, row: usize) -> bool {
        self.rows.expands_from(row)
    }

    /// Returns whether the cached lines were rendered for `ctx` at this theme epoch.
    pub(super) fn is_current(&self, ctx: &RenderCtx<'_>, theme_epoch: u64) -> bool {
        self.key == Some(self.key_for(ctx, theme_epoch))
    }

    /// Renders for `ctx` at this theme epoch, replacing the cached lines.
    pub(super) fn refresh(&mut self, ctx: &RenderCtx<'_>, theme_epoch: u64) {
        let key = self.key_for(ctx, theme_epoch);
        let ctx = RenderCtx {
            expanded: key.expanded,
            ..*ctx
        };
        self.rows = if self.visible {
            self.block.render_rows(&ctx)
        } else {
            BlockRows::default()
        };
        self.key = Some(key);
    }

    /// Renders the block in full while the view is compact.
    pub(super) fn expand(&mut self) {
        self.expanded = true;
    }

    /// Drops a click's expansion, returning the block to the chat's view.
    pub(super) fn reset_view(&mut self) {
        self.expanded = false;
    }

    fn key_for(&self, ctx: &RenderCtx<'_>, theme_epoch: u64) -> RenderKey {
        RenderKey {
            width: ctx.width,
            theme_epoch,
            expanded: ctx.expanded || self.expanded,
        }
    }
}

// ── SlotTops ──

/// A stack row named by the block slot that holds it, so a later stack can find the same row
/// after blocks change height.
#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(super) struct Anchor {
    /// Index of the block, or the block count for rows past the last block.
    block: usize,
    /// Row within the slot, from the block's first row.
    row: usize,
    /// Height of the slot when named, or `None` past the last block. Never zero.
    slot_height: Option<usize>,
}

impl Anchor {
    pub(super) fn block(self) -> usize {
        self.block
    }

    pub(super) fn row(self) -> usize {
        self.row
    }

    /// Keeps naming the same block after a block is inserted at index `at`.
    pub(super) fn shift_from(&mut self, at: usize) {
        if self.block >= at {
            self.block += 1;
        }
    }
}

/// First row of each block's slot in a stack, then the first row past the last block. A slot starts
/// at the block's first row, so the separator above a block belongs to the slot before it, and an
/// insert that adds or drops that separator leaves the rows of the block's own slot in place.
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

    /// The row `anchor` names here, like [`Self::offset_of`], except that a row in a slot whose
    /// height changed keeps its index, up to the slot's last row. A block that expands or
    /// collapses keeps its head, so the rows there stay put.
    pub(super) fn offset_keeping_row(&self, anchor: Anchor) -> usize {
        let row = match (anchor.slot_height, self.slot(anchor.block)) {
            (Some(_), Some(slot)) => anchor.row.min(slot.len().saturating_sub(1)),
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

    /// Adds an empty slot at block index `at`, for a block inserted there since these tops were
    /// taken, so each later slot keeps naming the block it held.
    pub(super) fn insert_empty(&mut self, at: usize) {
        if let Some(&top) = self.0.get(at) {
            self.0.insert(at, top);
        }
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
    /// Whether the last visible block closed a tool group.
    after_tool_group: bool,
}

impl<'a> Stack<'a> {
    /// Stacks a block's rows in its slot. Standalone blocks sit between blank rows, and a block
    /// after a closed tool group sits a blank row below it.
    pub(super) fn push_block(&mut self, cached: &'a CachedBlock) {
        if cached.visible() {
            let block = cached.block();
            if self.last_has_width() && (block.standalone() || self.after_tool_group) {
                self.push_blank();
            }
            self.push(cached.lines());
            if block.standalone() {
                self.push_blank();
            }
            self.after_tool_group = block.ends_tool_group();
        }
        self.tops.0.push(self.height);
    }

    pub(super) fn push(&mut self, lines: &'a [Line<'static>]) {
        if !lines.is_empty() {
            self.runs.push(lines);
            self.height += lines.len();
        }
    }

    /// Stacks a separator row. One that opens the next slot, or the rows past the last block,
    /// joins the slot before it.
    pub(super) fn push_blank(&mut self) {
        let top = self.height;
        self.push(&BLANK);
        for slot_top in self.tops.0[1..].iter_mut().rev().take_while(|t| **t == top) {
            *slot_top = self.height;
        }
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

    /// Returns whether the bottom row has visible width, so the next block needs a separator.
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

    /// Row `index`, or `None` past the bottom.
    pub(super) fn row(&self, index: usize) -> Option<&Line<'static>> {
        let mut skip = index;
        for run in &self.runs {
            if let Some(line) = run.get(skip) {
                return Some(line);
            }
            skip -= run.len();
        }
        None
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
    use crate::theme::Theme;

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

    /// Renders a head row over two body rows, which the compact view folds into a footer that
    /// expands the block.
    struct FoldingBlock;

    impl ChatBlock for FoldingBlock {
        fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
            let rows: &[&str] = if ctx.expanded {
                &["head", "a", "b"]
            } else {
                &["head", "... +2 rows"]
            };
            rows.iter().map(|row| Line::raw(*row)).collect()
        }

        fn render_rows(&self, ctx: &RenderCtx<'_>) -> BlockRows {
            let mut rows = self.render(ctx).into_iter();
            let mut out = BlockRows::default();
            out.lines.extend(rows.next());
            if ctx.expanded {
                out.lines.extend(rows);
            } else {
                out.push_expander(|lines| lines.extend(rows));
            }
            out
        }
    }

    fn with_ctx<T>(width: u16, f: impl FnOnce(&RenderCtx<'_>) -> T) -> T {
        with_view_ctx(width, false, f)
    }

    fn with_view_ctx<T>(width: u16, expanded: bool, f: impl FnOnce(&RenderCtx<'_>) -> T) -> T {
        let theme = Theme::default();
        f(&RenderCtx {
            width,
            theme: &theme,
            show_thinking: true,
            expanded,
            expand_hint: None,
        })
    }

    fn cached(block: impl ChatBlock + 'static) -> CachedBlock {
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

    fn folding() -> CachedBlock {
        cached(FoldingBlock)
    }

    fn refresh(block: &mut CachedBlock, width: u16) {
        with_ctx(width, |ctx| block.refresh(ctx, 0));
    }

    /// Returns whether `block` rendered at `width` and theme epoch `epoch` in the given view.
    fn is_current_in(block: &CachedBlock, width: u16, epoch: u64, expanded: bool) -> bool {
        with_view_ctx(width, expanded, |ctx| block.is_current(ctx, epoch))
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

    mod cached_block {
        use super::*;

        mod lines {
            use super::*;

            #[test]
            fn stand_in_as_one_blank_row_until_the_first_render() {
                let mut block = standalone(&["a", "b"]);
                assert_eq!(texts(block.lines()), [""]);

                refresh(&mut block, 40);
                assert_eq!(texts(block.lines()), ["a", "b"]);

                let mut hidden = hidden(&["a"]);
                assert!(!hidden.visible());
                refresh(&mut hidden, 40);
                assert_eq!(texts(hidden.lines()), Vec::<String>::new());
            }
        }

        mod expands_from {
            use super::*;

            #[test]
            fn holds_on_the_rows_that_expand_the_latest_render() {
                let mut block = folding();

                refresh(&mut block, 40);

                assert_eq!(texts(block.lines()), ["head", "... +2 rows"]);
                assert!(!block.expands_from(0));
                assert!(block.expands_from(1));
                assert!(!block.expands_from(2));
            }
        }

        mod is_current {
            use super::*;

            #[test]
            fn until_the_width_theme_epoch_or_view_changes() {
                let mut block = standalone(&["a"]);
                assert!(!is_current_in(&block, 40, 0, false));

                refresh(&mut block, 40);
                assert!(is_current_in(&block, 40, 0, false));
                assert!(
                    !is_current_in(&block, 30, 0, false),
                    "a width change is stale"
                );
                assert!(
                    !is_current_in(&block, 40, 1, false),
                    "a theme swap is stale"
                );
                assert!(
                    !is_current_in(&block, 40, 0, true),
                    "a view toggle is stale"
                );
            }
        }

        mod expand {
            use super::*;

            #[test]
            fn renders_the_block_in_full_while_the_view_is_compact() {
                let mut block = folding();
                refresh(&mut block, 40);

                block.expand();

                assert!(!is_current_in(&block, 40, 0, false));
                refresh(&mut block, 40);
                assert_eq!(texts(block.lines()), ["head", "a", "b"]);
                assert!(!block.expands_from(1), "a full render has no footer");
                assert!(
                    is_current_in(&block, 40, 0, true),
                    "the render matches the expanded view"
                );
            }
        }

        mod reset_view {
            use super::*;

            #[test]
            fn drops_a_click_expansion() {
                let mut block = folding();
                block.expand();
                refresh(&mut block, 40);

                block.reset_view();

                assert!(!is_current_in(&block, 40, 0, false));
                refresh(&mut block, 40);
                assert_eq!(texts(block.lines()), ["head", "... +2 rows"]);
            }
        }
    }

    mod anchor {
        use super::*;

        mod shift_from {
            use super::*;

            #[test]
            fn keeps_naming_the_block_moved_by_an_insert() {
                let painted = SlotTops(vec![0, 2, 4]);
                let mut above = painted.anchor_at(1);
                let mut below = painted.anchor_at(3);

                above.shift_from(1);
                below.shift_from(1);

                let inserted = SlotTops(vec![0, 2, 3, 5]);
                assert_eq!(inserted.offset_of(above), 1);
                assert_eq!(
                    inserted.offset_of(below),
                    4,
                    "row 1 of the block now at index 2"
                );
            }
        }
    }

    mod slot_tops {
        use super::*;

        mod anchor_at {
            use super::*;

            #[test]
            fn names_the_slot_holding_the_row() {
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
        }

        mod offset_of {
            use super::*;

            #[test]
            fn keeps_the_row_in_place_as_slots_change_height() {
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
        }

        mod offset_keeping_row {
            use super::*;

            #[test]
            fn keeps_the_row_index_up_to_the_slot_end() {
                let painted = SlotTops(vec![0, 4, 8, 10]);
                let early = painted.anchor_at(5);
                let late = painted.anchor_at(7);
                let past_end = painted.anchor_at(12);

                let taller = SlotTops(vec![0, 4, 16, 18]);
                let shorter = SlotTops(vec![0, 4, 6, 8]);

                assert_eq!(taller.offset_keeping_row(late), 4 + 3);
                assert_eq!(shorter.offset_keeping_row(early), 4 + 1);
                assert_eq!(
                    shorter.offset_keeping_row(late),
                    4 + 1,
                    "row 3 of a slot cut to 2 rows lands on its last"
                );
                assert_eq!(
                    shorter.offset_keeping_row(past_end),
                    8 + 2,
                    "a row past the end keeps its distance from it"
                );
            }
        }

        mod blocks_within {
            use super::*;

            #[test]
            fn lists_nonempty_slots_overlapping_the_rows() {
                let tops = SlotTops(vec![0, 2, 2, 5, 9]);
                let within = |rows| tops.blocks_within(rows).collect::<Vec<_>>();
                assert_eq!(within(0..1), [0]);
                assert_eq!(within(1..3), [0, 2], "the empty slot at row 2 is skipped");
                assert_eq!(within(4..20), [2, 3]);
                assert_eq!(within(9..20), Vec::<usize>::new(), "past the last block");
            }
        }
    }

    mod push_block {
        use super::*;

        #[test]
        fn separates_standalone_blocks_and_starts_each_slot_at_its_first_row() {
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
            assert_eq!(
                stack.into_tops(),
                SlotTops(vec![0, 2, 2, 4, 6]),
                "the blank above the reply ends the call's slot",
            );
        }
    }

    mod last_has_width {
        use super::*;

        #[test]
        fn ignores_empty_runs() {
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
    }

    mod row {
        use super::*;

        #[test]
        fn reads_across_run_boundaries() {
            let a = [Line::raw("a0")];
            let b = [Line::raw("b0"), Line::raw("b1")];
            let mut stack = Stack::default();
            stack.push(&a);
            stack.push_blank();
            stack.push(&b);

            let row = |index| stack.row(index).map(ToString::to_string);
            assert_eq!(row(0).as_deref(), Some("a0"));
            assert_eq!(row(1).as_deref(), Some(""));
            assert_eq!(row(3).as_deref(), Some("b1"));
            assert_eq!(row(4), None);
        }
    }

    mod slice {
        use super::*;

        #[test]
        fn spans_run_boundaries() {
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
}
