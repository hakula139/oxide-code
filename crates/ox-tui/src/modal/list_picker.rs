//! Generic list-picker primitive for [`Modal`](super::Modal) impls. It holds the state and renders
//! the rows, while concrete pickers own submit semantics, so it needs no callbacks.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr as _;

use ox_core::util::text::truncate_to_width;

use super::chrome::{COLUMN_GAP, FOOTER_HEIGHT, Header, above, height_above, render_footer};
use crate::glyphs::{ACTIVE_MARKER, cursor_gutter};
use crate::saturating_u16;
use crate::theme::Theme;
use crate::viewport::{Viewport, wrap_next, wrap_prev};

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

/// Selectable list with a cursor and an active-row marker.
/// Rows past the granted height scroll.
/// Every picker lists a fixed roster, so the list is never empty and always has a selected row.
pub(crate) struct ListPicker<T: PickerItem> {
    header: Header,
    items: Vec<T>,
    cursor: usize,
    viewport: Viewport,
}

impl<T: PickerItem> ListPicker<T> {
    /// Panics if `items` is empty.
    pub(crate) fn new(title: impl Into<String>, items: Vec<T>) -> Self {
        assert!(!items.is_empty(), "a picker lists at least one row");
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

    pub(crate) fn selected(&self) -> &T {
        &self.items[self.cursor]
    }

    #[cfg(test)]
    pub(crate) fn cursor_index(&self) -> usize {
        self.cursor
    }

    /// Moves the cursor for a navigation key: Up / `k` and Down / `j` step with wrap,
    /// and a digit jumps to the row it labels.
    /// Returns whether the cursor landed on a row,
    /// which a key outside these or a digit past the roster does not.
    pub(crate) fn navigate(&mut self, event: &KeyEvent) -> bool {
        match event.code {
            KeyCode::Up | KeyCode::Char('k') => self.select_prev(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next(),
            KeyCode::Char(c @ '1'..='9') => return self.select_by_hint(c),
            _ => return false,
        }
        true
    }

    fn select_next(&mut self) {
        self.cursor = wrap_next(self.cursor, self.items.len());
    }

    fn select_prev(&mut self) {
        self.cursor = wrap_prev(self.cursor, self.items.len());
    }

    /// Moves the cursor to the row whose `key_hint` is `c`. Returns whether a row matched.
    fn select_by_hint(&mut self, c: char) -> bool {
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

    /// [`Self::height`] plus the [`FOOTER_HEIGHT`] footer rows, within `max_height`.
    pub(crate) fn height_with_footer(&self, width: u16, max_height: u16) -> u16 {
        height_above(max_height, FOOTER_HEIGHT, |room| self.height(width, room))
    }

    /// Renders the list above a footer of `hints` at the bottom of `area`.
    pub(crate) fn render_with_footer(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        hints: &[&str],
        theme: &Theme,
    ) {
        self.render(frame, above(area, FOOTER_HEIGHT), theme);
        render_footer(frame, area, hints, theme);
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
    use crate::modal::testing::render_rows_with;

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

    mod new {
        use super::*;

        #[test]
        #[should_panic(expected = "a picker lists at least one row")]
        fn rejects_an_empty_roster() {
            _ = picker(Vec::new());
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

    mod navigate {
        use super::*;

        fn press(p: &mut ListPicker<FakeItem>, code: KeyCode) -> bool {
            p.navigate(&KeyEvent::from(code))
        }

        fn hinted(labels: &[&'static str]) -> ListPicker<FakeItem> {
            picker(
                labels
                    .iter()
                    .enumerate()
                    .map(|(idx, &label)| FakeItem {
                        hint: numeric_hint(idx),
                        ..FakeItem::new(label)
                    })
                    .collect(),
            )
        }

        #[test]
        fn down_and_j_advance_and_wrap_past_the_last_row() {
            for code in [KeyCode::Down, KeyCode::Char('j')] {
                let mut p = hinted(&["a", "b"]);

                let moved = press(&mut p, code);

                assert!(moved, "{code:?}");
                assert_eq!(p.cursor_index(), 1, "{code:?}");

                let wrapped = press(&mut p, code);

                assert!(wrapped, "{code:?}");
                assert_eq!(p.cursor_index(), 0, "{code:?} wraps past the last row");
            }
        }

        #[test]
        fn up_and_k_retreat_and_wrap_past_the_first_row() {
            for code in [KeyCode::Up, KeyCode::Char('k')] {
                let mut p = hinted(&["a", "b", "c"]);

                let wrapped = press(&mut p, code);

                assert!(wrapped, "{code:?}");
                assert_eq!(p.cursor_index(), 2, "{code:?} wraps past the first row");

                let moved = press(&mut p, code);

                assert!(moved, "{code:?}");
                assert_eq!(p.cursor_index(), 1, "{code:?}");
            }
        }

        #[test]
        fn a_digit_jumps_to_the_row_it_labels() {
            let mut p = hinted(&["a", "b", "c"]);

            let jumped = press(&mut p, KeyCode::Char('3'));

            assert!(jumped);
            assert_eq!(p.cursor_index(), 2);
        }

        #[test]
        fn other_keys_and_a_digit_past_the_roster_leave_the_cursor() {
            let mut p = hinted(&["a", "b"]);

            for code in [KeyCode::Char('9'), KeyCode::Char('x'), KeyCode::Enter] {
                let moved = press(&mut p, code);

                assert!(!moved, "{code:?}");
            }

            assert_eq!(p.cursor_index(), 0);
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
        fn caps_long_lists_at_the_max() {
            let items = (0..20).map(|_| FakeItem::new("x")).collect();

            assert_eq!(picker(items).height(80, 7), 7);
        }
    }

    mod height_with_footer {
        use super::*;

        #[test]
        fn adds_the_footer_rows_within_the_max() {
            let short = picker(vec![FakeItem::new("a"), FakeItem::new("b")]);
            let long = picker((0..20).map(|_| FakeItem::new("x")).collect());

            assert_eq!(short.height_with_footer(80, 40), 4 + FOOTER_HEIGHT);
            assert_eq!(long.height_with_footer(80, 7), 7);
            assert_eq!(long.height_with_footer(80, 1), 1);
        }
    }

    mod render_with_footer {
        use super::*;

        #[test]
        fn keeps_the_cursor_row_above_the_footer_when_the_list_scrolls() {
            let mut p = picker(
                ["a", "b", "c", "d", "e"]
                    .into_iter()
                    .map(FakeItem::new)
                    .collect(),
            );
            p.navigate(&KeyEvent::from(KeyCode::Up));
            let height = 2 + 2 + FOOTER_HEIGHT;

            let rows = render_rows_with(40, height, |frame, area, theme| {
                p.render_with_footer(frame, area, &["Enter to confirm"], theme);
            });

            assert!(rows[3].trim_start().starts_with('>'), "{rows:#?}");
            assert!(rows[3].contains('e'), "{rows:#?}");
            assert!(rows[4].trim().is_empty(), "{rows:#?}");
            assert!(rows[5].contains("Enter to confirm"), "{rows:#?}");
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
