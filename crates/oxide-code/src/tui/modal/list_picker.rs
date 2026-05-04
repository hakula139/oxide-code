//! Generic list-picker primitive for [`Modal`](super::Modal) impls.
//!
//! [`ListPicker<T>`] is the state + render surface for "select one of
//! N items" pickers. It is **not** a [`Modal`](super::Modal) itself —
//! concrete pickers own their submit semantics (Enter dispatches what?
//! Esc cancels) so the picker stays free of `Box<dyn Fn(&T) ->
//! ModalAction>` callbacks.
//!
//! Each item implements [`PickerItem`]: label, optional description,
//! `is_active` for the active marker, and an optional `key_hint`
//! character (typically `'1'`–`'9'`) for muscle-memory jumps.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

// ── PickerItem ──

/// One row in a [`ListPicker`]. Concrete picker types wrap their data
/// in a small adapter type that implements this trait — see
/// `slash::picker::ModelRow` for the canonical example.
pub(crate) trait PickerItem {
    /// Primary text, left-aligned in the row's first column.
    fn label(&self) -> &str;

    /// Optional secondary text, right-aligned. Renders dim.
    fn description(&self) -> Option<&str> {
        None
    }

    /// Marks the row currently in effect (e.g. the active model). Drawn
    /// with a `✓` marker. Independent of cursor position.
    fn is_active(&self) -> bool {
        false
    }

    /// Single-character mnemonic for jump-to-row (typically `'1'`–`'9'`).
    /// `None` ⇒ row is not directly addressable; cursor reaches it via
    /// arrows only.
    fn key_hint(&self) -> Option<char> {
        None
    }
}

// ── ListPicker ──

/// Marker rendered to the left of the cursor row.
const CURSOR_MARKER: &str = "> ";
/// Width of `CURSOR_MARKER`. ASCII so byte length matches column width.
const CURSOR_MARKER_WIDTH: usize = 2;
/// Marker rendered to the right of the row currently in effect.
const ACTIVE_MARKER: &str = "✓";

/// Padding columns between the label and the description.
const COLUMN_GAP: usize = 2;

/// Body row count not counting items: title + blank + (description? +
/// blank?). Updated by [`ListPicker::header_height`].
const TITLE_ROW_HEIGHT: u16 = 1;
const TITLE_BLANK_ROW: u16 = 1;

/// Selectable list with cursor + active marker. Concrete modals embed
/// this and forward navigation keys (`↑`/`↓`/`j`/`k`/`1`–`9`) to it.
pub(crate) struct ListPicker<T: PickerItem> {
    title: String,
    description: Option<String>,
    items: Vec<T>,
    selected: usize,
}

impl<T: PickerItem> ListPicker<T> {
    /// New picker with cursor at index 0. Empty `items` is allowed —
    /// height shrinks accordingly and `selected()` returns `None`.
    pub(crate) fn new(title: impl Into<String>, items: Vec<T>) -> Self {
        Self {
            title: title.into(),
            description: None,
            items,
            selected: 0,
        }
    }

    /// Builder: optional description line under the title (rendered dim).
    pub(crate) fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Position the cursor on the first item matching `predicate`. No-op
    /// if no item matches — cursor stays where it was.
    pub(crate) fn select_initial(&mut self, predicate: impl Fn(&T) -> bool) {
        if let Some(idx) = self.items.iter().position(predicate) {
            self.selected = idx;
        }
    }

    /// Currently-highlighted item. `None` when the list is empty.
    pub(crate) fn selected(&self) -> Option<&T> {
        self.items.get(self.selected)
    }

    /// Cursor row index. Test-only — production picker logic reaches
    /// the highlighted row through [`Self::selected`] which already
    /// returns the value most callers need.
    #[cfg(test)]
    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    /// Move cursor down one row; wraps from last to first.
    pub(crate) fn select_next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    /// Move cursor up one row; wraps from first to last.
    pub(crate) fn select_prev(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.items.len() - 1
        } else {
            self.selected - 1
        };
    }

    /// Jump cursor to the row whose `key_hint` matches `c`. Returns
    /// whether a jump happened — `false` for keys that don't address a
    /// row, so the wrapping modal can fall through to other handling.
    pub(crate) fn select_by_hint(&mut self, c: char) -> bool {
        if let Some(idx) = self.items.iter().position(|i| i.key_hint() == Some(c)) {
            self.selected = idx;
            return true;
        }
        false
    }

    /// Total rows the picker needs at `width`. Title + optional
    /// description + items, with one-row gutters between sections.
    pub(crate) fn height(&self, _width: u16) -> u16 {
        let header = self.header_height();
        let body = u16::try_from(self.items.len()).unwrap_or(u16::MAX);
        header.saturating_add(body)
    }

    /// Number of rows the title + (description?) header occupies, plus
    /// the trailing blank that visually separates header from list.
    fn header_height(&self) -> u16 {
        let mut h = TITLE_ROW_HEIGHT + TITLE_BLANK_ROW;
        if self.description.is_some() {
            h += 2; // description row + blank
        }
        h
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let mut lines: Vec<Line<'static>> =
            Vec::with_capacity(usize::from(self.height(area.width)));

        lines.push(Line::from(Span::styled(
            self.title.clone(),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
        if let Some(desc) = &self.description {
            lines.push(Line::from(Span::styled(desc.clone(), theme.dim())));
        }
        lines.push(Line::default());

        let label_width = self
            .items
            .iter()
            .map(|i| i.label().chars().count())
            .max()
            .unwrap_or(0);

        for (idx, item) in self.items.iter().enumerate() {
            lines.push(self.render_row(item, idx, label_width, area.width, theme));
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
        let is_cursor = idx == self.selected;
        let row_style = if is_cursor {
            theme.text().add_modifier(Modifier::BOLD)
        } else {
            theme.dim()
        };

        let mut spans: Vec<Span<'static>> = Vec::with_capacity(6);

        // Cursor gutter — `> ` on cursor row, two spaces otherwise.
        spans.push(Span::styled(
            if is_cursor {
                CURSOR_MARKER.to_owned()
            } else {
                " ".repeat(CURSOR_MARKER_WIDTH)
            },
            theme.accent(),
        ));

        // Numeric mnemonic (or two-space gutter when none).
        if let Some(c) = item.key_hint() {
            spans.push(Span::styled(format!("{c}. "), row_style));
        } else {
            spans.push(Span::styled("   ".to_owned(), row_style));
        }

        let label = format!("{:width$}", item.label(), width = label_width);
        spans.push(Span::styled(label, row_style));

        // Active marker: `✓` after the label, before the description.
        spans.push(Span::styled(
            if item.is_active() {
                format!("  {ACTIVE_MARKER} ")
            } else {
                "    ".to_owned()
            },
            theme.accent(),
        ));

        if let Some(desc) = item.description() {
            let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            let budget = usize::from(area_width).saturating_sub(used + COLUMN_GAP);
            let truncated = truncate_to_width(desc, budget);
            spans.push(Span::styled(truncated, theme.dim()));
        }

        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test fixture ──

    /// Minimal `PickerItem` impl that exposes every trait method so
    /// the tests can pin `description` / `is_active` / `key_hint`
    /// behavior without coupling to any concrete picker.
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

    // ── select_next / select_prev ──

    #[test]
    fn select_next_advances_and_wraps_at_end() {
        let mut p = picker(vec![FakeItem::new("a"), FakeItem::new("b")]);
        p.select_next();
        assert_eq!(p.selected_index(), 1);
        p.select_next();
        assert_eq!(p.selected_index(), 0, "wraps past the last row");
    }

    #[test]
    fn select_prev_retreats_and_wraps_at_zero() {
        let mut p = picker(vec![FakeItem::new("a"), FakeItem::new("b")]);
        p.select_prev();
        assert_eq!(p.selected_index(), 1, "wraps past the first row");
        p.select_prev();
        assert_eq!(p.selected_index(), 0);
    }

    #[test]
    fn select_next_and_prev_on_empty_list_are_noops() {
        let mut p = picker(Vec::new());
        p.select_next();
        p.select_prev();
        assert_eq!(p.selected_index(), 0);
        assert!(p.selected().is_none());
    }

    // ── select_by_hint ──

    #[test]
    fn select_by_hint_jumps_to_matching_item_and_returns_true() {
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
        assert_eq!(p.selected_index(), 1);
    }

    #[test]
    fn select_by_hint_unknown_key_leaves_cursor_and_returns_false() {
        let mut p = picker(vec![FakeItem {
            hint: Some('1'),
            ..FakeItem::new("a")
        }]);
        assert!(!p.select_by_hint('9'));
        assert_eq!(p.selected_index(), 0, "cursor stays put on miss");
    }

    // ── select_initial ──

    #[test]
    fn select_initial_seeks_first_matching_item() {
        let mut p = picker(vec![
            FakeItem::new("a"),
            FakeItem::new("b"),
            FakeItem::new("c"),
        ]);
        p.select_initial(|i| i.label() == "b");
        assert_eq!(p.selected_index(), 1);
    }

    #[test]
    fn select_initial_no_match_leaves_cursor_at_zero() {
        let mut p = picker(vec![FakeItem::new("a"), FakeItem::new("b")]);
        p.select_initial(|i| i.label() == "missing");
        assert_eq!(p.selected_index(), 0);
    }

    // ── height ──

    #[test]
    fn height_with_no_description_is_two_header_rows_plus_items() {
        // Title (1) + blank (1) + items.
        let p = picker(vec![FakeItem::new("a"), FakeItem::new("b")]);
        assert_eq!(p.height(80), 4);
    }

    #[test]
    fn height_with_description_adds_two_more_rows() {
        // Title (1) + description (1) + blank (1) + 1 spacer between
        // header sections totals 4 header rows + items.
        let p = picker(vec![FakeItem::new("a")]).with_description("a small picker");
        assert_eq!(p.height(80), 5);
    }

    #[test]
    fn height_with_empty_items_is_just_header_rows() {
        let p: ListPicker<FakeItem> = picker(Vec::new());
        assert_eq!(p.height(80), 2, "empty list still draws title + blank");
    }

    // ── render ──

    #[test]
    fn render_runs_without_panicking_at_minimum_width() {
        // Smoke test: extreme narrow widths must not panic on the
        // truncation arithmetic. Real visual snapshots happen in the
        // concrete picker tests where output is meaningful.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let p = picker(vec![FakeItem {
            description: Some("a long description"),
            ..FakeItem::new("very-long-label")
        }])
        .with_description("title-line");
        let theme = Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 20, p.height(20).min(8));
                p.render(frame, area, &theme);
            })
            .expect("render must not panic");
    }
}
