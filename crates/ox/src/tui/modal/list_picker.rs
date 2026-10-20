//! Generic list-picker primitive for [`Modal`](super::Modal) impls. It holds the state and renders
//! the rows, while concrete pickers own submit semantics, so it needs no callbacks.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use ox_core::util::text::truncate_to_width;

use super::chrome::{COLUMN_GAP, Header};
use crate::tui::glyphs::{ACTIVE_MARKER, cursor_gutter};
use crate::tui::saturating_u16;
use crate::tui::theme::Theme;
use crate::tui::viewport::{Viewport, wrap_next, wrap_prev};

// ── PickerItem ──

/// One row in a [`ListPicker`].
pub(crate) trait PickerItem {
    /// Primary text, left-aligned.
    fn label(&self) -> &str;

    /// Optional dim text after the label columns, truncated to the row.
    fn description(&self) -> Option<&str> {
        None
    }

    /// Returns whether this row is the currently-active item (drawn with a `✓` marker).
    fn is_active(&self) -> bool {
        false
    }

    /// Single-character mnemonic for jump-to-row (`'1'`–`'9'`).
    fn key_hint(&self) -> Option<char> {
        None
    }
}

/// `'1'`–`'9'` for the first nine rows, `None` after that.
pub(crate) fn numeric_hint(index: usize) -> Option<char> {
    char::from_digit(u32::try_from(index).ok()?.checked_add(1)?, 10)
}

// ── ListPicker ──

/// Columns kept clear between a description and the right edge of its row.
const RIGHT_MARGIN: usize = 2;

/// Selectable list with a cursor and an active-row marker. Rows past the granted height scroll.
pub(crate) struct ListPicker<T: PickerItem> {
    header: Header,
    items: Vec<T>,
    cursor: usize,
    viewport: Viewport,
}

impl<T: PickerItem> ListPicker<T> {
    pub(crate) fn new(title: impl Into<String>, items: Vec<T>) -> Self {
        Self {
            header: Header::new(title),
            items,
            cursor: 0,
            viewport: Viewport::default(),
        }
    }

    pub(crate) fn with_description(mut self, description: impl Into<String>) -> Self {
        self.header = self.header.with_description(description);
        self
    }

    /// Moves the cursor to the first item matching `f`. No-op if none matches.
    pub(crate) fn cursor_to(&mut self, f: impl Fn(&T) -> bool) {
        if let Some(idx) = self.items.iter().position(f) {
            self.cursor = idx;
        }
    }

    pub(crate) fn selected(&self) -> Option<&T> {
        self.items.get(self.cursor)
    }

    #[cfg(test)]
    pub(crate) fn cursor_index(&self) -> usize {
        self.cursor
    }

    pub(crate) fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.cursor = wrap_next(self.cursor, self.items.len());
        }
    }

    pub(crate) fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.cursor = wrap_prev(self.cursor, self.items.len());
        }
    }

    /// Moves the cursor to the row whose `key_hint` is `c`. Returns whether a row matched.
    pub(crate) fn select_by_hint(&mut self, c: char) -> bool {
        if let Some(idx) = self.items.iter().position(|i| i.key_hint() == Some(c)) {
            self.cursor = idx;
            return true;
        }
        false
    }

    /// Header plus one row per item, within `max_height`.
    pub(crate) fn height(&self, _width: u16, max_height: u16) -> u16 {
        let rows = saturating_u16(self.items.len());
        self.header.height().saturating_add(rows).min(max_height)
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(usize::from(area.height));
        self.header.push_lines(&mut lines, area.width, theme);

        let label_width = self
            .items
            .iter()
            .map(|i| i.label().width())
            .max()
            .unwrap_or(0);

        let rows = area.height.saturating_sub(self.header.height());
        let window = self
            .viewport
            .window(self.items.len(), usize::from(rows), Some(self.cursor));
        for idx in window {
            lines.push(self.render_row(&self.items[idx], idx, label_width, area.width, theme));
        }

        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
    }

    fn render_row(
        &self,
        item: &T,
        idx: usize,
        label_width: usize,
        area_width: u16,
        theme: &Theme,
    ) -> Line<'static> {
        let is_cursor = idx == self.cursor;
        let row_style = theme.row_style(is_cursor);

        let hint = item
            .key_hint()
            .map_or_else(|| "   ".to_owned(), |c| format!("{c}. "));
        let label = format!(
            "{}{}",
            item.label(),
            " ".repeat(label_width - item.label().width())
        );
        let marker = if item.is_active() { ACTIVE_MARKER } else { " " };
        let mut line = Line::from(vec![
            Span::styled(cursor_gutter(is_cursor), theme.accent()),
            Span::styled(hint, row_style),
            Span::styled(label, row_style),
            Span::styled(
                format!("{}{marker} ", " ".repeat(COLUMN_GAP)),
                theme.accent(),
            ),
        ]);

        if let Some(desc) = item.description() {
            let budget = usize::from(area_width).saturating_sub(line.width() + RIGHT_MARGIN);
            line.push_span(Span::styled(truncate_to_width(desc, budget), theme.dim()));
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::modal::testing::render_rows_with;

    /// Minimal `PickerItem` covering all trait methods so tests can pin behavior without
    /// coupling to any concrete picker.
    struct FakeItem {
        label: &'static str,
        description: Option<&'static str>,
        active: bool,
        hint: Option<char>,
    }

    impl FakeItem {
        fn new(label: &'static str) -> Self {
            Self {
                label,
                description: None,
                active: false,
                hint: None,
            }
        }
    }

    impl PickerItem for FakeItem {
        fn label(&self) -> &str {
            self.label
        }

        fn description(&self) -> Option<&str> {
            self.description
        }

        fn is_active(&self) -> bool {
            self.active
        }

        fn key_hint(&self) -> Option<char> {
            self.hint
        }
    }

    fn picker(items: Vec<FakeItem>) -> ListPicker<FakeItem> {
        ListPicker::new("Pick one", items)
    }

    #[test]
    fn select_next_and_prev_on_empty_list_are_noops() {
        let mut p = picker(Vec::new());

        p.select_next();
        p.select_prev();

        assert_eq!(p.cursor_index(), 0);
        assert!(p.selected().is_none());
    }

    mod numeric_hint {
        use super::*;

        #[test]
        fn numbers_the_first_nine_rows_only() {
            assert_eq!(numeric_hint(0), Some('1'));
            assert_eq!(numeric_hint(8), Some('9'));
            assert_eq!(numeric_hint(9), None);
            assert_eq!(numeric_hint(42), None);
        }
    }

    mod cursor_to {
        use super::*;

        #[test]
        fn seeks_first_matching_item() {
            let mut p = picker(vec![
                FakeItem::new("a"),
                FakeItem::new("b"),
                FakeItem::new("c"),
            ]);

            p.cursor_to(|i| i.label() == "b");

            assert_eq!(p.cursor_index(), 1);
        }

        #[test]
        fn no_match_leaves_cursor_at_zero() {
            let mut p = picker(vec![FakeItem::new("a"), FakeItem::new("b")]);

            p.cursor_to(|i| i.label() == "missing");

            assert_eq!(p.cursor_index(), 0);
        }
    }

    mod select_next {
        use super::*;

        #[test]
        fn advances_and_wraps_at_end() {
            let mut p = picker(vec![FakeItem::new("a"), FakeItem::new("b")]);

            p.select_next();
            assert_eq!(p.cursor_index(), 1);

            p.select_next();
            assert_eq!(p.cursor_index(), 0, "wraps past the last row");
        }
    }

    mod select_prev {
        use super::*;

        #[test]
        fn retreats_and_wraps_at_zero() {
            let mut p = picker(vec![FakeItem::new("a"), FakeItem::new("b")]);

            p.select_prev();
            assert_eq!(p.cursor_index(), 1, "wraps past the first row");

            p.select_prev();
            assert_eq!(p.cursor_index(), 0);
        }
    }

    mod select_by_hint {
        use super::*;

        #[test]
        fn jumps_to_matching_item() {
            let mut p = picker(vec![
                FakeItem {
                    hint: Some('1'),
                    ..FakeItem::new("a")
                },
                FakeItem {
                    hint: Some('2'),
                    ..FakeItem::new("b")
                },
                FakeItem {
                    hint: Some('3'),
                    ..FakeItem::new("c")
                },
            ]);

            assert!(p.select_by_hint('2'));
            assert_eq!(p.cursor_index(), 1);
        }

        #[test]
        fn unknown_key_leaves_cursor() {
            let mut p = picker(vec![FakeItem {
                hint: Some('1'),
                ..FakeItem::new("a")
            }]);

            assert!(!p.select_by_hint('9'));
            assert_eq!(p.cursor_index(), 0, "cursor stays put on miss");
        }
    }

    mod height {
        use super::*;

        #[test]
        fn with_no_description_is_two_header_rows_plus_items() {
            // Title (1) + blank (1) + items.
            let p = picker(vec![FakeItem::new("a"), FakeItem::new("b")]);

            assert_eq!(p.height(80, 40), 4);
        }

        #[test]
        fn with_description_adds_one_more_row() {
            // Title (1) + description (1) + blank (1) = 3 header rows + items.
            let p = picker(vec![FakeItem::new("a")]).with_description("a small picker");

            assert_eq!(p.height(80, 40), 4);
        }

        #[test]
        fn with_empty_items_is_just_header_rows() {
            let p: ListPicker<FakeItem> = picker(Vec::new());

            assert_eq!(p.height(80, 40), 2, "empty list still draws title + blank");
        }

        #[test]
        fn caps_long_lists_at_the_max() {
            let items = (0..20).map(|_| FakeItem::new("x")).collect();

            assert_eq!(picker(items).height(80, 7), 7);
        }
    }

    mod render {
        use super::*;

        fn render_rows<T: PickerItem>(
            picker: &ListPicker<T>,
            width: u16,
            height: u16,
        ) -> Vec<String> {
            render_rows_with(width, height, |frame, area, theme| {
                picker.render(frame, area, theme);
            })
        }

        #[test]
        fn aligns_columns_by_display_width() {
            let p = picker(vec![
                FakeItem {
                    description: Some("abcdefghij"),
                    ..FakeItem::new("中文")
                },
                FakeItem {
                    description: Some("xyz"),
                    active: true,
                    ..FakeItem::new("a")
                },
            ]);

            let rows = render_rows(&p, 20, p.height(20, 20));

            assert_eq!(rows[2].trim_end(), ">    中文    ab...");
            assert_eq!(rows[3].trim_end(), "     a     ✓ xyz");
        }

        #[test]
        fn truncates_the_description_at_a_narrow_width() {
            let p = picker(vec![FakeItem {
                description: Some("a long description"),
                ..FakeItem::new("very-long-label")
            }])
            .with_description("title-line");

            let rows = render_rows(&p, 32, p.height(32, 8));

            let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
            assert_eq!(
                rows,
                [
                    "Pick one",
                    "title-line",
                    "",
                    ">    very-long-label    a l..."
                ]
            );
        }

        #[test]
        fn label_only_items_show_no_marker_or_description() {
            struct MinimalItem(&'static str);

            impl PickerItem for MinimalItem {
                fn label(&self) -> &str {
                    self.0
                }
            }

            let p = ListPicker::new("Pick one", vec![MinimalItem("a"), MinimalItem("b")]);

            let rows = render_rows(&p, 40, p.height(40, 40));

            let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
            assert_eq!(rows, ["Pick one", "", ">    a", "     b"]);
        }

        #[test]
        fn scrolls_the_cursor_row_into_a_short_area() {
            const LABELS: [&str; 6] = ["r0", "r1", "r2", "r3", "r4", "r5"];
            let mut p = picker(LABELS.into_iter().map(FakeItem::new).collect());
            p.cursor_to(|item| item.label == "r4");

            let rows = render_rows(&p, 20, p.height(20, 5));

            assert_eq!(
                rows.iter().map(|row| row.trim_end()).collect::<Vec<_>>(),
                ["Pick one", "", "     r2", "     r3", ">    r4"],
                "the header stays pinned and the cursor row lands at the bottom",
            );
        }
    }
}
