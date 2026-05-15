//! Mouse-drag text selection over the chat viewport.
//!
//! [`Selection`] is a small state machine driven by the app's mouse handler. It tracks the start
//! and current cell coordinates of a left-button drag, materializes the selected text from the
//! chat's rendered `Text` buffer, and writes the result to the system clipboard via OSC 52.
//!
//! Selection geometry follows terminal convention: from `(start_row, start_col)` the selection
//! extends to the end of that row, all of every intermediate row, and from column 0 to
//! `(end_row, end_col)` on the final row. Block / column selection is deferred.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Text;
use unicode_width::UnicodeWidthChar;

use crate::tui::theme::Theme;

/// Conservative pre-base64 cap. xterm's OSC budget is ~8 KB; kitty / iTerm2 are larger. Pick the
/// floor so the same selection works everywhere.
const OSC52_PAYLOAD_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Cell {
    col: u16,
    row: u16,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) enum Selection {
    #[default]
    Idle,
    /// Left button pressed; `end` updates on every drag event.
    Dragging { start: Cell, end: Cell },
}

impl Selection {
    pub(super) fn is_dragging(&self) -> bool {
        matches!(self, Selection::Dragging { .. })
    }

    /// Begins a drag. A subsequent `update` with the same coordinates leaves the selection empty,
    /// so the click-vs-drag distinction is "did `end` move from `start` before mouse-up?".
    pub(super) fn begin(&mut self, col: u16, row: u16) {
        let cell = Cell { col, row };
        *self = Selection::Dragging {
            start: cell,
            end: cell,
        };
    }

    /// Updates the drag endpoint. No-op when not currently dragging.
    pub(super) fn update(&mut self, col: u16, row: u16) {
        if let Selection::Dragging { end, .. } = self {
            *end = Cell { col, row };
        }
    }

    /// Clears any in-flight drag. Returns the prior state so callers can finalize before clearing.
    pub(super) fn clear(&mut self) -> Selection {
        std::mem::replace(self, Selection::Idle)
    }

    /// Returns the normalized `(start, end)` cells with `start` always above-or-left of `end`.
    /// Returns `None` when not dragging or when start == end (a click, not a drag).
    fn normalized(&self) -> Option<(Cell, Cell)> {
        match self {
            Selection::Dragging { start, end } if start != end => {
                let (a, b) = if (start.row, start.col) <= (end.row, end.col) {
                    (*start, *end)
                } else {
                    (*end, *start)
                };
                Some((a, b))
            }
            _ => None,
        }
    }

    /// Materializes the selected text from `text` clipped to `area` with `scroll_offset`. Returns
    /// `None` when there's no selection or when the drag misses the chat area entirely.
    pub(super) fn materialize(
        &self,
        text: &Text<'_>,
        area: Rect,
        scroll_offset: u16,
    ) -> Option<String> {
        let (start, end) = self.normalized()?;
        let row_range = clip_rows(start, end, area)?;
        let mut out = String::new();
        for screen_row in row_range {
            let line_idx = usize::from(scroll_offset) + usize::from(screen_row - area.y);
            let Some(line) = text.lines.get(line_idx) else {
                break;
            };
            let (col_start, col_end) =
                row_columns(screen_row, start, end, area.x, area.x + area.width);
            let segment = slice_line(line, col_start - area.x, col_end - area.x);
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&segment);
        }
        if out.is_empty() { None } else { Some(out) }
    }

    /// Highlights selected cells in `buf` using `theme.selection`. Cells outside the chat `area`
    /// are clipped. No-op when not dragging.
    pub(super) fn paint(&self, buf: &mut Buffer, area: Rect, theme: &Theme) {
        let Some((start, end)) = self.normalized() else {
            return;
        };
        let Some(rows) = clip_rows(start, end, area) else {
            return;
        };
        let style = theme.selection.style();
        for row in rows {
            let (col_start, col_end) = row_columns(row, start, end, area.x, area.x + area.width);
            for col in col_start..col_end {
                if col < buf.area.x + buf.area.width && row < buf.area.y + buf.area.height {
                    buf[(col, row)].set_style(style);
                }
            }
        }
    }
}

/// Clip the selection's row span to `area`. `None` when entirely above or below.
fn clip_rows(start: Cell, end: Cell, area: Rect) -> Option<std::ops::Range<u16>> {
    let area_top = area.y;
    let area_bottom = area.y + area.height;
    let lo = start.row.max(area_top);
    let hi = (end.row + 1).min(area_bottom);
    if lo >= hi { None } else { Some(lo..hi) }
}

/// Per-row column range. The first row starts at `start.col`, the last row ends at `end.col + 1`,
/// every other row spans the full chat-area width.
fn row_columns(row: u16, start: Cell, end: Cell, area_left: u16, area_right: u16) -> (u16, u16) {
    let lo = if row == start.row {
        start.col.max(area_left)
    } else {
        area_left
    };
    let hi = if row == end.row {
        (end.col + 1).min(area_right)
    } else {
        area_right
    };
    (lo.min(area_right), hi.min(area_right))
}

/// Walks the line's spans to extract the slice between cell columns `[col_start, col_end)`.
/// Wide chars (`UnicodeWidthChar::width == 2`) are taken whole when their leading half lands
/// inside `[col_start, col_end)` so multi-byte sequences never split. Zero-width combining
/// marks attach to the preceding base char and are kept whenever that base char was kept.
fn slice_line(line: &ratatui::text::Line<'_>, col_start: u16, col_end: u16) -> String {
    let mut out = String::new();
    let mut col: u16 = 0;
    let mut last_base_kept = false;
    'spans: for span in &line.spans {
        for ch in span.content.chars() {
            let w = u16::try_from(UnicodeWidthChar::width(ch).unwrap_or(0)).unwrap_or(0);
            if w == 0 {
                if last_base_kept {
                    out.push(ch);
                }
                continue;
            }
            if col >= col_end {
                break 'spans;
            }
            let next = col.saturating_add(w);
            let kept = next > col_start;
            if kept {
                out.push(ch);
            }
            last_base_kept = kept;
            col = next;
        }
    }
    out
}

/// OSC 52 set-clipboard sequence. `c` selects the system clipboard (xterm `selection`).
/// Returns the bytes plus a flag indicating whether the payload was clamped.
pub(super) fn osc52_set_clipboard(text: &str) -> (Vec<u8>, bool) {
    let bytes = text.as_bytes();
    let (clipped, truncated) = if bytes.len() > OSC52_PAYLOAD_BYTES {
        (
            &bytes[..floor_to_char_boundary(text, OSC52_PAYLOAD_BYTES)],
            true,
        )
    } else {
        (bytes, false)
    };
    let encoded = BASE64.encode(clipped);
    let mut out = Vec::with_capacity(encoded.len() + 8);
    out.extend_from_slice(b"\x1b]52;c;");
    out.extend_from_slice(encoded.as_bytes());
    out.push(0x07);
    (out, truncated)
}

/// Largest byte index `<= cap` that lies on a UTF-8 char boundary. Walks back at most 3 bytes.
fn floor_to_char_boundary(s: &str, cap: usize) -> usize {
    let mut i = cap.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;
    use ratatui::text::{Line, Span};

    use super::*;

    fn cell(col: u16, row: u16) -> Cell {
        Cell { col, row }
    }

    fn fixture_text() -> Text<'static> {
        Text::from(vec![
            Line::from("hello world"),
            Line::from("second line"),
            Line::from("third"),
        ])
    }

    // ── Selection ──

    #[test]
    fn begin_then_update_tracks_drag_endpoint() {
        let mut s = Selection::default();
        s.begin(2, 3);
        s.update(7, 5);
        assert_eq!(
            s,
            Selection::Dragging {
                start: cell(2, 3),
                end: cell(7, 5),
            }
        );
        assert!(s.is_dragging());
    }

    #[test]
    fn clear_returns_prior_state_and_resets_to_idle() {
        let mut s = Selection::default();
        s.begin(0, 0);
        let prior = s.clear();
        assert!(matches!(prior, Selection::Dragging { .. }));
        assert_eq!(s, Selection::Idle);
    }

    #[test]
    fn normalized_orders_start_before_end() {
        let mut s = Selection::default();
        s.begin(7, 5);
        s.update(2, 3);
        let (start, end) = s.normalized().expect("normalizes despite reversed drag");
        assert_eq!((start.col, start.row), (2, 3));
        assert_eq!((end.col, end.row), (7, 5));
    }

    #[test]
    fn normalized_is_none_for_zero_length_drag() {
        let mut s = Selection::default();
        s.begin(4, 4);
        s.update(4, 4);
        assert!(s.normalized().is_none(), "click without drag");
    }

    // ── materialize ──

    #[test]
    fn materialize_single_row_returns_substring() {
        let mut s = Selection::default();
        s.begin(2, 0);
        s.update(7, 0);
        let area = Rect::new(0, 0, 80, 3);
        assert_eq!(s.materialize(&fixture_text(), area, 0).unwrap(), "llo wo");
    }

    #[test]
    fn materialize_multi_row_joins_with_newlines() {
        let mut s = Selection::default();
        s.begin(6, 0);
        s.update(5, 1);
        let area = Rect::new(0, 0, 80, 3);
        assert_eq!(
            s.materialize(&fixture_text(), area, 0).unwrap(),
            "world\nsecond",
        );
    }

    #[test]
    fn materialize_respects_scroll_offset() {
        let mut s = Selection::default();
        s.begin(0, 0);
        s.update(4, 0);
        let area = Rect::new(0, 0, 80, 3);
        // scroll_offset 2 puts content row "third" at screen row 0.
        assert_eq!(s.materialize(&fixture_text(), area, 2).unwrap(), "third");
    }

    #[test]
    fn materialize_returns_none_when_drag_starts_below_area() {
        let mut s = Selection::default();
        s.begin(0, 5);
        s.update(99, 99);
        let area = Rect::new(0, 0, 80, 3);
        assert!(s.materialize(&fixture_text(), area, 0).is_none());
    }

    // ── slice_line ──

    #[test]
    fn slice_line_keeps_cjk_chars_whole_at_each_boundary() {
        let line = Line::from("Hello 你好 World");
        // Columns: H=0 e=1 l=2 l=3 o=4 ' '=5 你=6,7(wide) 好=8,9(wide) ' '=10 W=11 ...
        // Slice from col 6 to col 10 should include both CJK chars in full.
        assert_eq!(slice_line(&line, 6, 10), "你好");
        // Slice that begins inside the trailing half of `你` is greedy: the wide char's leading
        // half at col 6 contributes content past the 7-col boundary, so the whole char is taken.
        assert_eq!(slice_line(&line, 7, 10), "你好");
        // Slice that begins exactly at `好`'s leading half (col 8) skips `你`.
        assert_eq!(slice_line(&line, 8, 10), "好");
    }

    #[test]
    fn slice_line_handles_emoji_and_mixed_widths() {
        let line = Line::from(vec![Span::raw("ab"), Span::raw("好"), Span::raw("c")]);
        // a=0 b=1 好=2,3 c=4. Selecting cols 1..=4 should pick "b好c".
        assert_eq!(slice_line(&line, 1, 5), "b好c");
    }

    #[test]
    fn slice_line_preserves_zero_width_combining_marks() {
        // NFD-decomposed `é` is `e` + COMBINING ACUTE ACCENT (U+0301, width 0).
        let line = Line::from("e\u{0301}cho");
        // Selecting just the first column keeps the base char and its combining mark.
        assert_eq!(slice_line(&line, 0, 1), "e\u{0301}");
        // Selecting from col 1 starts past the combining mark's base, so it's dropped along with
        // the base it modified.
        assert_eq!(slice_line(&line, 1, 4), "cho");
    }

    // ── osc52_set_clipboard ──

    #[test]
    fn osc52_round_trips_cjk_bytes() {
        let payload = "Hello 你好 🌏 World";
        let (sequence, truncated) = osc52_set_clipboard(payload);
        assert!(!truncated);
        let prefix = b"\x1b]52;c;";
        let suffix = b"\x07";
        assert!(sequence.starts_with(prefix));
        assert!(sequence.ends_with(suffix));
        let b64 = &sequence[prefix.len()..sequence.len() - suffix.len()];
        let decoded = BASE64.decode(b64).expect("valid base64");
        assert_eq!(decoded, payload.as_bytes(), "UTF-8 round-trip preserved");
    }

    #[test]
    fn osc52_clamps_oversize_payload_at_char_boundary() {
        let big = "A".repeat(OSC52_PAYLOAD_BYTES + 1024);
        let (sequence, truncated) = osc52_set_clipboard(&big);
        assert!(truncated);
        let prefix = b"\x1b]52;c;";
        let b64 = &sequence[prefix.len()..sequence.len() - 1];
        let decoded = BASE64.decode(b64).unwrap();
        assert_eq!(decoded.len(), OSC52_PAYLOAD_BYTES);
    }

    // ── floor_to_char_boundary ──

    #[test]
    fn floor_to_char_boundary_walks_back_through_multibyte() {
        // `好` is 3 bytes (e5 a5 bd). Cap inside the 2nd or 3rd byte must rewind to the start.
        let s = "A好B";
        assert_eq!(floor_to_char_boundary(s, 2), 1, "between A and 好");
        assert_eq!(floor_to_char_boundary(s, 3), 1, "inside 好's 2nd byte");
        assert_eq!(floor_to_char_boundary(s, 4), 4, "between 好 and B");
    }

    // ── paint ──

    #[test]
    fn paint_applies_selection_style_to_in_area_cells() {
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        let mut theme = Theme::default();
        theme.selection.bg = Some(Color::Red);

        let mut s = Selection::default();
        s.begin(2, 1);
        s.update(5, 1);
        s.paint(&mut buf, area, &theme);

        for col in 2..=5 {
            assert_eq!(buf[(col, 1)].bg, Color::Red, "col {col} highlighted");
        }
        assert_eq!(buf[(1, 1)].bg, Color::Reset, "before selection");
        assert_eq!(buf[(6, 1)].bg, Color::Reset, "after selection");
    }
}
