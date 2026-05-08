//! Searchable + scrollable list primitive for [`Modal`](super::Modal) impls. Sibling to
//! [`ListPicker`](super::list_picker::ListPicker): adds a search input row, a scrollable
//! viewport, and case-insensitive substring filtering. Concrete pickers own their submit
//! semantics so the primitive stays callback-free.

use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

// ── SearchableItem ──

/// One row in a [`SearchableList`].
pub(crate) trait SearchableItem {
    /// Composite haystack used by substring search. Include every field the user might want to
    /// filter against (title, id, project, branch).
    fn haystack(&self) -> Cow<'_, str>;

    /// Render the row body into a single line at the given column budget — the primitive owns
    /// the cursor gutter, so the impl paints content to the right of it.
    fn render_row(&self, width: u16, is_cursor: bool, theme: &Theme) -> Line<'static>;
}

// ── SearchableList ──

const CURSOR_MARKER: &str = "> ";
const CURSOR_MARKER_WIDTH: u16 = 2;
const SEARCH_PROMPT: &str = "/ ";
const SEARCH_PROMPT_WIDTH: u16 = 2;
const TITLE_ROW_HEIGHT: u16 = 1;
const SEARCH_ROW_HEIGHT: u16 = 1;
const SECTION_GAP: u16 = 1;

/// Selectable + searchable list with a scrollable viewport.
///
/// The cursor operates on the **filtered** index space (`visible`), so navigation skips
/// out-of-filter rows. Filtering happens on every `set_query` / `push_char` / `pop_char`,
/// case-insensitive substring on each item's [`SearchableItem::haystack`].
pub(crate) struct SearchableList<T: SearchableItem> {
    title: String,
    description: Option<String>,
    items: Vec<T>,
    query: String,
    /// Indices into `items` that pass the current `query`, in original item order.
    visible: Vec<usize>,
    /// Cursor position into `visible`; clamped on filter changes.
    cursor: usize,
    /// First visible row painted in the viewport. Tracks `cursor` to keep it on screen.
    viewport_offset: usize,
    /// Target viewport height in rows — caller-supplied so the modal owns layout policy.
    viewport_height: u16,
}

impl<T: SearchableItem> SearchableList<T> {
    pub(crate) fn new(title: impl Into<String>, items: Vec<T>, viewport_height: u16) -> Self {
        let visible: Vec<usize> = (0..items.len()).collect();
        Self {
            title: title.into(),
            description: None,
            items,
            query: String::new(),
            visible,
            cursor: 0,
            viewport_offset: 0,
            viewport_height: viewport_height.max(1),
        }
    }

    pub(crate) fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[cfg(test)]
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn set_query(&mut self, q: String) {
        self.query = q;
        self.recompute_visible();
    }

    pub(crate) fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.recompute_visible();
    }

    pub(crate) fn pop_char(&mut self) {
        if self.query.pop().is_some() {
            self.recompute_visible();
        }
    }

    /// Replace the underlying items (e.g., after a scope toggle reloads from the store) and
    /// re-run the filter. Cursor + viewport reset to the top.
    pub(crate) fn replace_items(&mut self, items: Vec<T>) {
        self.items = items;
        self.recompute_visible();
    }

    pub(crate) fn selected(&self) -> Option<&T> {
        self.visible
            .get(self.cursor)
            .and_then(|i| self.items.get(*i))
    }

    #[cfg(test)]
    pub(crate) fn cursor_index(&self) -> usize {
        self.cursor
    }

    /// Number of rows currently passing the substring filter — for footers that show
    /// "X / Y matching".
    pub(crate) fn visible_len(&self) -> usize {
        self.visible.len()
    }

    /// `true` when the user has typed a filter that excludes some rows.
    pub(crate) fn is_filtered(&self) -> bool {
        !self.query.is_empty()
    }

    pub(crate) fn select_next(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.visible.len();
        self.scroll_into_view();
    }

    pub(crate) fn select_prev(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        self.cursor = if self.cursor == 0 {
            self.visible.len() - 1
        } else {
            self.cursor - 1
        };
        self.scroll_into_view();
    }

    pub(crate) fn page_down(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        let step = usize::from(self.viewport_height).max(1);
        self.cursor = (self.cursor + step).min(self.visible.len() - 1);
        self.scroll_into_view();
    }

    pub(crate) fn page_up(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        let step = usize::from(self.viewport_height).max(1);
        self.cursor = self.cursor.saturating_sub(step);
        self.scroll_into_view();
    }

    fn recompute_visible(&mut self) {
        let needle = self.query.to_lowercase();
        self.visible.clear();
        for (i, item) in self.items.iter().enumerate() {
            if needle.is_empty() || item.haystack().to_lowercase().contains(&needle) {
                self.visible.push(i);
            }
        }
        self.cursor = 0;
        self.viewport_offset = 0;
    }

    fn scroll_into_view(&mut self) {
        let height = usize::from(self.viewport_height);
        if self.cursor < self.viewport_offset {
            self.viewport_offset = self.cursor;
        } else if self.cursor >= self.viewport_offset + height {
            self.viewport_offset = self.cursor + 1 - height;
        }
    }

    /// Total rows occupied by chrome (title + optional description + blanks + search row).
    fn chrome_height(&self) -> u16 {
        let mut h = TITLE_ROW_HEIGHT;
        if self.description.is_some() {
            h += 1;
        }
        h += SECTION_GAP + SEARCH_ROW_HEIGHT + SECTION_GAP;
        h
    }

    /// Total rows the list reports for [`super::Modal::height`]. Caller adds footer / borders.
    pub(crate) fn height(&self, _width: u16) -> u16 {
        self.chrome_height().saturating_add(self.viewport_height)
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(usize::from(area.height));

        lines.push(Line::from(Span::styled(
            self.title.clone(),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
        if let Some(desc) = &self.description {
            lines.push(Line::from(Span::styled(desc.clone(), theme.dim())));
        }
        lines.push(Line::default());

        lines.push(self.render_search_row(area.width, theme));
        lines.push(Line::default());

        let row_width = area.width.saturating_sub(CURSOR_MARKER_WIDTH);
        let viewport_h = usize::from(self.viewport_height);
        let take = self
            .visible
            .len()
            .saturating_sub(self.viewport_offset)
            .min(viewport_h);
        for vi in self.viewport_offset..self.viewport_offset + take {
            let is_cursor = vi == self.cursor;
            let item_idx = self.visible[vi];
            let item = &self.items[item_idx];
            lines.push(Self::render_row(item, is_cursor, row_width, theme));
        }

        if self.visible.is_empty() && !self.query.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("    no matches for `{}`", self.query),
                theme.dim(),
            )));
        }

        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
    }

    fn render_search_row(&self, area_width: u16, theme: &Theme) -> Line<'static> {
        let prompt_style = theme.accent();
        let cursor_glyph = "▏";
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
        spans.push(Span::styled(SEARCH_PROMPT.to_owned(), prompt_style));
        if self.query.is_empty() {
            spans.push(Span::styled(
                "type to filter (substring match)".to_owned(),
                theme.dim(),
            ));
            spans.push(Span::styled(cursor_glyph.to_owned(), prompt_style));
        } else {
            let budget = usize::from(area_width.saturating_sub(SEARCH_PROMPT_WIDTH + 1));
            let shown = truncate_to_width(&self.query, budget);
            spans.push(Span::styled(shown, theme.text()));
            spans.push(Span::styled(cursor_glyph.to_owned(), prompt_style));
        }
        Line::from(spans)
    }

    fn render_row(item: &T, is_cursor: bool, body_width: u16, theme: &Theme) -> Line<'static> {
        let cursor_span = Span::styled(
            if is_cursor {
                CURSOR_MARKER.to_owned()
            } else {
                " ".repeat(usize::from(CURSOR_MARKER_WIDTH))
            },
            theme.accent(),
        );
        let body = item.render_row(body_width, is_cursor, theme);
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(1 + body.spans.len());
        spans.push(cursor_span);
        spans.extend(body.spans);
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test fixture ──

    /// Minimal `SearchableItem` for behavior tests, without coupling to any concrete picker.
    struct FakeItem {
        haystack: &'static str,
    }

    impl FakeItem {
        fn new(haystack: &'static str) -> Self {
            Self { haystack }
        }
    }

    impl SearchableItem for FakeItem {
        fn haystack(&self) -> Cow<'_, str> {
            Cow::Borrowed(self.haystack)
        }

        fn render_row(&self, width: u16, _is_cursor: bool, theme: &Theme) -> Line<'static> {
            let trimmed = truncate_to_width(self.haystack, usize::from(width));
            Line::from(Span::styled(trimmed, theme.text()))
        }
    }

    fn list(items: Vec<FakeItem>) -> SearchableList<FakeItem> {
        SearchableList::new("Pick one", items, 5)
    }

    // ── set_query / filter ──

    #[test]
    fn set_query_filters_to_substring_matches_case_insensitively() {
        let mut l = list(vec![
            FakeItem::new("alpha"),
            FakeItem::new("BETA"),
            FakeItem::new("gamma"),
        ]);
        l.set_query("a".to_owned());
        assert_eq!(l.visible_len(), 3, "all three contain 'a' case-insensitive");
        l.set_query("BE".to_owned());
        assert_eq!(l.visible_len(), 1);
        l.set_query("BeT".to_owned());
        assert_eq!(l.visible_len(), 1, "case-insensitive substring");
    }

    #[test]
    fn set_query_resets_cursor_and_viewport_to_top() {
        let mut l = list(vec![
            FakeItem::new("alpha"),
            FakeItem::new("beta"),
            FakeItem::new("gamma"),
        ]);
        l.select_next();
        l.select_next();
        assert_eq!(l.cursor_index(), 2);
        l.set_query("a".to_owned());
        assert_eq!(l.cursor_index(), 0, "filter resets cursor");
    }

    #[test]
    fn empty_query_includes_every_item() {
        let mut l = list(vec![FakeItem::new("a"), FakeItem::new("b")]);
        l.set_query("zzz".to_owned());
        assert_eq!(l.visible_len(), 0);
        l.set_query(String::new());
        assert_eq!(l.visible_len(), 2);
    }

    // ── push_char / pop_char ──

    #[test]
    fn push_then_pop_round_trips_through_filter_state() {
        let mut l = list(vec![FakeItem::new("alpha"), FakeItem::new("beta")]);
        l.push_char('a');
        assert_eq!(l.visible_len(), 2, "both contain 'a'");
        l.push_char('l');
        assert_eq!(l.visible_len(), 1, "only `alpha` contains `al`");
        l.pop_char();
        assert_eq!(l.visible_len(), 2);
        l.pop_char();
        assert_eq!(l.query(), "");
    }

    #[test]
    fn pop_char_on_empty_query_is_a_noop() {
        let mut l = list(vec![FakeItem::new("a")]);
        l.pop_char();
        assert_eq!(l.query(), "");
    }

    // ── replace_items ──

    #[test]
    fn replace_items_resets_cursor_and_reapplies_filter() {
        let mut l = list(vec![FakeItem::new("alpha"), FakeItem::new("alphabet")]);
        l.set_query("alpha".to_owned());
        l.select_next();
        assert_eq!(l.cursor_index(), 1);

        l.replace_items(vec![FakeItem::new("alpine"), FakeItem::new("albatross")]);
        assert_eq!(
            l.visible_len(),
            0,
            "the prior `alpha` query no longer matches the new items",
        );
        assert_eq!(l.cursor_index(), 0, "replace_items must reset cursor");
    }

    // ── select_next / select_prev ──

    #[test]
    fn select_next_advances_through_visible_indices_and_wraps() {
        let mut l = list(vec![
            FakeItem::new("alpha"),
            FakeItem::new("beta"),
            FakeItem::new("gamma"),
        ]);
        l.select_next();
        assert_eq!(l.cursor_index(), 1);
        l.select_next();
        l.select_next();
        assert_eq!(l.cursor_index(), 0, "wraps past last");
    }

    #[test]
    fn select_prev_wraps_at_zero() {
        let mut l = list(vec![FakeItem::new("a"), FakeItem::new("b")]);
        l.select_prev();
        assert_eq!(l.cursor_index(), 1);
    }

    #[test]
    fn select_next_skips_filtered_out_items() {
        // Filter narrows to two of three items; cursor walks the visible pair only — the hidden
        // middle row never receives the cursor.
        let mut l = list(vec![
            FakeItem::new("apple-pie"),
            FakeItem::new("BERRY"),
            FakeItem::new("apricot"),
        ]);
        l.set_query("ap".to_owned());
        assert_eq!(l.visible_len(), 2, "berry filtered out");
        l.select_next();
        assert_eq!(l.cursor_index(), 1);
        l.select_next();
        assert_eq!(l.cursor_index(), 0, "wraps within filtered set");
    }

    // ── page_down / page_up ──

    #[test]
    fn page_down_clamps_at_last_visible_row() {
        let mut l: SearchableList<FakeItem> = SearchableList::new(
            "Pick",
            (0..20).map(|i| FakeItem::new(item_label(i))).collect(),
            5,
        );
        l.page_down();
        assert_eq!(l.cursor_index(), 5);
        l.page_down();
        l.page_down();
        l.page_down();
        l.page_down();
        assert_eq!(l.cursor_index(), 19, "clamps at last");
    }

    #[test]
    fn page_up_clamps_at_zero() {
        let items: Vec<FakeItem> = (0..10).map(|i| FakeItem::new(item_label(i))).collect();
        let mut l = SearchableList::new("Pick", items, 5);
        l.page_down();
        l.page_up();
        l.page_up();
        assert_eq!(l.cursor_index(), 0);
    }

    #[test]
    fn navigation_on_empty_visible_set_is_silent_noop() {
        // Filter out everything → all four navigation guards must short-circuit. Without the
        // is_empty checks, `% self.visible.len()` would panic.
        let mut l = list(vec![FakeItem::new("alpha"), FakeItem::new("beta")]);
        l.set_query("zzz".to_owned());
        assert_eq!(l.visible_len(), 0);
        l.select_next();
        l.select_prev();
        l.page_down();
        l.page_up();
        assert_eq!(l.cursor_index(), 0);
    }

    fn item_label(i: usize) -> &'static str {
        // Leak owned strings for &'static str; fine in tests.
        Box::leak(format!("item-{i}").into_boxed_str())
    }

    // ── render ──

    #[test]
    fn render_runs_at_minimum_width_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let l = list(vec![FakeItem::new("a long-ish entry"), FakeItem::new("b")]);
        let theme = Theme::default();
        let h = l.height(20);
        let mut terminal = Terminal::new(TestBackend::new(20, h)).unwrap();
        terminal
            .draw(|frame| l.render(frame, Rect::new(0, 0, 20, h), &theme))
            .expect("render must not panic");
    }

    #[test]
    fn render_shows_no_match_line_when_filter_excludes_everything() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut l = list(vec![FakeItem::new("alpha"), FakeItem::new("beta")]);
        l.set_query("zzz".to_owned());

        let theme = Theme::default();
        let h = l.height(40);
        let mut terminal = Terminal::new(TestBackend::new(40, h)).unwrap();
        terminal
            .draw(|frame| l.render(frame, Rect::new(0, 0, 40, h), &theme))
            .expect("render must not panic");

        let buf = terminal.backend().buffer();
        let dump: String = (0..h)
            .flat_map(|y| (0..40).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_owned())
            .collect();
        assert!(
            dump.contains("no matches"),
            "no-matches notice should render: {dump}",
        );
    }
}
