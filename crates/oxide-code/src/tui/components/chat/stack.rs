//! Per-block render cache and the row stack the chat view paints from.

use ratatui::style::Style;
use ratatui::text::Line;

use super::blocks::{ChatBlock, RenderCtx, last_has_width};

/// Separator row borrowed by every stack gap.
static BLANK: [Line<'static>; 1] = [Line {
    style: Style::new(),
    alignment: None,
    spans: Vec::new(),
}];

// ── CachedBlock ──

/// Render inputs beyond a block's own state. A cached render stays valid while its key matches.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct RenderKey {
    pub(super) width: u16,
    pub(super) theme_epoch: u64,
    pub(super) show_thinking: bool,
}

/// A committed block with its most recent render. Blocks never change after push, so the cached
/// lines only go stale when the [`RenderKey`] changes.
pub(super) struct CachedBlock {
    pub(super) block: Box<dyn ChatBlock>,
    key: Option<RenderKey>,
    pub(super) visible: bool,
    pub(super) lines: Vec<Line<'static>>,
}

impl CachedBlock {
    pub(super) fn new(block: Box<dyn ChatBlock>) -> Self {
        Self {
            block,
            key: None,
            visible: false,
            lines: Vec::new(),
        }
    }

    /// Re-renders unless the cached lines were produced under `key`.
    pub(super) fn refresh(&mut self, ctx: &RenderCtx<'_>, key: RenderKey) {
        if self.key == Some(key) {
            return;
        }
        self.visible = self.block.visible(ctx);
        self.lines = if self.visible {
            self.block.render(ctx)
        } else {
            Vec::new()
        };
        self.key = Some(key);
    }
}

// ── Stack ──

/// Chat rows stacked top to bottom as runs borrowed from block renders.
#[derive(Default)]
pub(super) struct Stack<'a> {
    runs: Vec<&'a [Line<'static>]>,
}

impl<'a> Stack<'a> {
    pub(super) fn push(&mut self, lines: &'a [Line<'static>]) {
        if !lines.is_empty() {
            self.runs.push(lines);
        }
    }

    pub(super) fn push_blank(&mut self) {
        self.runs.push(&BLANK);
    }

    /// Whether the bottom row has visible width, so the next block needs a separator.
    pub(super) fn last_has_width(&self) -> bool {
        self.runs.last().is_some_and(|run| last_has_width(run))
    }

    pub(super) fn height(&self) -> usize {
        self.runs.iter().map(|run| run.len()).sum()
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

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
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
