//! Searchable + scrollable list primitive for [`Modal`](super::Modal) impls. Adds a search
//! input + scrollable viewport on top of the [`ListPicker`](super::list_picker::ListPicker)
//! shape; concrete pickers own submit semantics.

use std::borrow::Cow;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::chrome::Header;
use super::line_editor::LineEditor;
use crate::tui::theme::Theme;
use crate::tui::viewport::{Viewport, wrap_next, wrap_prev};

// ── SearchableItem ──

pub(crate) trait SearchableItem {
    /// Composite haystack — include every field the user might filter against (title, id,
    /// project, etc).
    fn haystack(&self) -> Cow<'_, str>;

    /// Paint the item body to the right of the cursor gutter (owned by the primitive). Multi-line
    /// returns let layouts split across rows (e.g. title row + dim metadata row). The primitive
    /// places the gutter marker on the first line and pads subsequent lines with blanks for
    /// alignment. Length must equal [`Self::row_height`] for every instance.
    fn render(&self, width: u16, is_cursor: bool, theme: &Theme) -> Vec<Line<'static>>;

    /// Constant terminal-rows per item. Must match `render(...).len()` for every instance, since
    /// the list primitive treats this as a layout invariant when sizing its viewport.
    fn row_height() -> u16
    where
        Self: Sized;
}

// ── SearchableList ──

const CURSOR_MARKER: &str = "> ";
const CURSOR_MARKER_WIDTH: u16 = 2;
const SEARCH_PLACEHOLDER: &str = "Type to filter (substring match)";
const SEARCH_ROW_HEIGHT: u16 = 1;
const SECTION_GAP: u16 = 1;

/// Selectable + searchable list with a scrollable viewport. Cursor walks the **filtered** index
/// space, so out-of-filter rows are skipped by navigation.
pub(crate) struct SearchableList<T: SearchableItem> {
    header: Header,
    items: Vec<T>,
    query: LineEditor,
    /// Indices into `items` that pass the current `query`, in original item order.
    visible: Vec<usize>,
    /// Cursor into `visible`. Resets to 0 on filter changes.
    cursor: usize,
    viewport: Viewport,
    /// Most items shown at once. Fewer show when the granted height is shorter.
    max_visible: u16,
}

impl<T: SearchableItem> SearchableList<T> {
    pub(crate) fn new(title: impl Into<String>, items: Vec<T>, max_visible: u16) -> Self {
        let visible: Vec<usize> = (0..items.len()).collect();
        Self {
            header: Header::new(title),
            items,
            query: LineEditor::new("").with_placeholder(SEARCH_PLACEHOLDER),
            visible,
            cursor: 0,
            viewport: Viewport::default(),
            max_visible: max_visible.max(1),
        }
    }

    pub(crate) fn with_description(mut self, description: impl Into<String>) -> Self {
        self.header = self.header.with_description(description);
        self
    }

    #[cfg(test)]
    pub(crate) fn query(&self) -> &str {
        self.query.text()
    }

    pub(crate) fn set_query(&mut self, query: &str) {
        self.query.set_text(query);
        self.recompute_visible();
    }

    /// Routes one editing key to the search field, refiltering when the query changes.
    pub(crate) fn edit_query(&mut self, event: &KeyEvent) {
        if self.query.handle_key(event) {
            self.recompute_visible();
        }
    }

    /// Pastes `text` into the search field, refiltering when the query changes.
    pub(crate) fn paste_query(&mut self, text: &str) {
        if self.query.paste(text) {
            self.recompute_visible();
        }
    }

    /// Replace all items and re-run the filter. Cursor and viewport reset to 0.
    pub(crate) fn replace_items(&mut self, items: Vec<T>) {
        self.items = items;
        self.recompute_visible();
    }

    /// Seek the cursor to the first visible row matching `f`. No-op if no row matches. Pairs with
    /// [`Self::replace_items`] to keep the cursor pinned across a refresh.
    pub(crate) fn cursor_to<F: Fn(&T) -> bool>(&mut self, f: F) {
        if let Some(vi) = self
            .visible
            .iter()
            .position(|&item_idx| self.items.get(item_idx).is_some_and(&f))
        {
            self.cursor = vi;
        }
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

    /// Rows currently passing the filter, for match-count footers.
    pub(crate) fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub(crate) fn is_filtered(&self) -> bool {
        !self.query.text().is_empty()
    }

    pub(crate) fn select_next(&mut self) {
        if !self.visible.is_empty() {
            self.cursor = wrap_next(self.cursor, self.visible.len());
        }
    }

    pub(crate) fn select_prev(&mut self) {
        if !self.visible.is_empty() {
            self.cursor = wrap_prev(self.cursor, self.visible.len());
        }
    }

    pub(crate) fn page_down(&mut self) {
        if !self.visible.is_empty() {
            self.cursor = (self.cursor + self.viewport.page()).min(self.visible.len() - 1);
        }
    }

    pub(crate) fn page_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(self.viewport.page());
    }

    fn recompute_visible(&mut self) {
        let query = self.query.text();
        // Empty-needle fast path skips the per-row to_lowercase allocation.
        self.visible = if query.is_empty() {
            (0..self.items.len()).collect()
        } else {
            let needle = query.to_lowercase();
            self.items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    item.haystack()
                        .to_lowercase()
                        .contains(&needle)
                        .then_some(i)
                })
                .collect()
        };
        self.cursor = 0;
        self.viewport.reset();
    }

    /// Total rows occupied by chrome (header + search row + gap).
    fn chrome_height(&self) -> u16 {
        self.header.height() + SEARCH_ROW_HEIGHT + SECTION_GAP
    }

    /// Chrome plus up to `max_visible` whole items, within `max_height`. Caller adds the footer.
    pub(crate) fn height(&self, _width: u16, max_height: u16) -> u16 {
        let chrome = self.chrome_height();
        let rows = self.visible_rows(max_height);
        chrome
            .saturating_add(rows.saturating_mul(T::row_height()))
            .min(max_height)
    }

    /// Whole items that fit beside the chrome in `height` rows, capped at `max_visible`.
    fn visible_rows(&self, height: u16) -> u16 {
        let fit = height.saturating_sub(self.chrome_height()) / T::row_height();
        fit.min(self.max_visible)
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        self.header.push_lines(&mut lines, area.width, theme);
        lines.push(self.query.line(area.width, theme));
        lines.push(Line::default());

        let row_width = area.width.saturating_sub(CURSOR_MARKER_WIDTH);
        let rows = usize::from(self.visible_rows(area.height));
        for vi in self
            .viewport
            .window(self.visible.len(), rows, Some(self.cursor))
        {
            let item = &self.items[self.visible[vi]];
            Self::push_item_lines(&mut lines, item, vi == self.cursor, row_width, theme);
        }

        if self.visible.is_empty() && self.is_filtered() {
            lines.push(Line::from(Span::styled(
                format!(
                    "{}no matches for `{}`",
                    " ".repeat(CURSOR_MARKER_WIDTH.into()),
                    self.query.text(),
                ),
                theme.dim(),
            )));
        }

        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
        let search_y_offset = self.header.height();
        if search_y_offset < area.height {
            self.query
                .place_cursor(frame, area, area.y.saturating_add(search_y_offset));
        }
    }

    fn push_item_lines(
        lines: &mut Vec<Line<'static>>,
        item: &T,
        is_cursor: bool,
        body_width: u16,
        theme: &Theme,
    ) {
        let blank_gutter = " ".repeat(usize::from(CURSOR_MARKER_WIDTH));
        let cursor_marker = if is_cursor {
            CURSOR_MARKER.to_owned()
        } else {
            blank_gutter.clone()
        };
        let body_lines = item.render(body_width, is_cursor, theme);
        for (idx, body) in body_lines.into_iter().enumerate() {
            let gutter = if idx == 0 {
                cursor_marker.clone()
            } else {
                blank_gutter.clone()
            };
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(1 + body.spans.len());
            spans.push(Span::styled(gutter, theme.accent()));
            spans.extend(body.spans);
            lines.push(Line::from(spans));
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::util::text::truncate_to_width;

    // ── Test fixture ──

    /// Minimal `SearchableItem` for behavior tests, without coupling to any concrete picker.
    /// Renders the haystack, then blank rows up to `ROWS`.
    struct FakeItem<const ROWS: u16 = 1> {
        haystack: &'static str,
    }

    impl FakeItem {
        fn new(haystack: &'static str) -> Self {
            Self { haystack }
        }
    }

    impl<const ROWS: u16> SearchableItem for FakeItem<ROWS> {
        fn haystack(&self) -> Cow<'_, str> {
            Cow::Borrowed(self.haystack)
        }

        fn render(&self, width: u16, _is_cursor: bool, theme: &Theme) -> Vec<Line<'static>> {
            let trimmed = truncate_to_width(self.haystack, usize::from(width));
            let mut lines = vec![Line::from(Span::styled(trimmed, theme.text()))];
            lines.resize(usize::from(ROWS), Line::default());
            lines
        }

        fn row_height() -> u16 {
            ROWS
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
        l.set_query("a");
        assert_eq!(l.visible_len(), 3, "all three contain 'a' case-insensitive");
        l.set_query("BE");
        assert_eq!(l.visible_len(), 1);
        l.set_query("BeT");
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
        l.set_query("a");
        assert_eq!(l.cursor_index(), 0, "filter resets cursor");
    }

    #[test]
    fn empty_query_includes_every_item() {
        let mut l = list(vec![FakeItem::new("a"), FakeItem::new("b")]);
        l.set_query("zzz");
        assert_eq!(l.visible_len(), 0);
        l.set_query("");
        assert_eq!(l.visible_len(), 2);
    }

    // ── edit_query ──

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    #[test]
    fn edit_query_refilters_as_the_query_grows_and_shrinks() {
        let mut l = list(vec![FakeItem::new("alpha"), FakeItem::new("beta")]);
        l.edit_query(&key(KeyCode::Char('a')));
        assert_eq!(l.visible_len(), 2, "both contain 'a'");
        l.edit_query(&key(KeyCode::Char('l')));
        assert_eq!(l.visible_len(), 1, "only `alpha` contains `al`");
        l.edit_query(&key(KeyCode::Backspace));
        assert_eq!(l.visible_len(), 2);
        l.edit_query(&key(KeyCode::Backspace));
        assert_eq!(l.query(), "");
    }

    #[test]
    fn edit_query_inserts_at_the_editing_position() {
        let mut l = list(vec![FakeItem::new("alpha"), FakeItem::new("beta")]);
        l.set_query("aha");
        assert_eq!(l.visible_len(), 0);

        l.edit_query(&key(KeyCode::Left));
        l.edit_query(&key(KeyCode::Left));
        l.edit_query(&key(KeyCode::Char('l')));
        l.edit_query(&key(KeyCode::Char('p')));
        assert_eq!(l.query(), "alpha");
        assert_eq!(l.visible_len(), 1);
    }

    #[test]
    fn edit_query_cursor_only_keys_keep_the_list_cursor() {
        let mut l = list(vec![FakeItem::new("alpha"), FakeItem::new("beta")]);
        l.select_next();
        l.edit_query(&key(KeyCode::Home));
        l.edit_query(&KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(l.query(), "", "control chords never insert");
        assert_eq!(l.cursor_index(), 1, "no refilter, so no cursor reset");
    }

    // ── paste_query ──

    #[test]
    fn paste_query_refilters_on_the_pasted_line() {
        let mut l = list(vec![FakeItem::new("fix auth"), FakeItem::new("beta")]);
        l.select_next();
        l.paste_query("fix\nauth");
        assert_eq!(l.query(), "fix auth");
        assert_eq!(l.visible_len(), 1);
        assert_eq!(l.cursor_index(), 0, "a refilter resets the cursor");
    }

    // ── cursor_to ──

    #[test]
    fn cursor_to_seeks_to_first_visible_row_matching_predicate() {
        let mut l = list(vec![
            FakeItem::new("alpha"),
            FakeItem::new("beta"),
            FakeItem::new("gamma"),
        ]);
        l.cursor_to(|item| item.haystack == "gamma");
        assert_eq!(l.cursor_index(), 2);
        l.cursor_to(|item| item.haystack == "alpha");
        assert_eq!(l.cursor_index(), 0);
    }

    #[test]
    fn cursor_to_is_a_noop_when_no_row_matches() {
        let mut l = list(vec![FakeItem::new("alpha"), FakeItem::new("beta")]);
        l.select_next();
        assert_eq!(l.cursor_index(), 1);
        l.cursor_to(|item| item.haystack == "missing");
        assert_eq!(l.cursor_index(), 1, "no match leaves cursor untouched");
    }

    #[test]
    fn cursor_to_walks_only_filtered_rows() {
        let mut l = list(vec![
            FakeItem::new("apple"),
            FakeItem::new("BERRY"),
            FakeItem::new("apricot"),
        ]);
        l.set_query("ap");
        assert_eq!(l.visible_len(), 2, "berry filtered out");
        l.cursor_to(|item| item.haystack == "BERRY");
        assert_eq!(
            l.cursor_index(),
            0,
            "filtered-out row never receives the cursor"
        );
    }

    // ── replace_items ──

    #[test]
    fn replace_items_resets_cursor_and_reapplies_filter() {
        let mut l = list(vec![FakeItem::new("alpha"), FakeItem::new("alphabet")]);
        l.set_query("alpha");
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
        l.set_query("ap");
        assert_eq!(l.visible_len(), 2, "berry filtered out");
        l.select_next();
        assert_eq!(l.cursor_index(), 1);
        l.select_next();
        assert_eq!(l.cursor_index(), 0, "wraps within filtered set");
    }

    // ── page_down / page_up ──

    fn numbered(count: usize, max_visible: u16) -> SearchableList<FakeItem> {
        SearchableList::new(
            "Pick",
            (0..count).map(|i| FakeItem::new(item_label(i))).collect(),
            max_visible,
        )
    }

    /// Renders `l` into a `width` x `height` area and returns the painted rows.
    fn render_rows(l: &SearchableList<FakeItem>, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| l.render(frame, Rect::new(0, 0, width, height), &Theme::default()))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn page_down_steps_by_the_rendered_page_and_clamps_at_last_visible_row() {
        let mut l = numbered(20, 5);
        render_rows(&l, 20, l.height(20, 40));
        l.page_down();
        assert_eq!(l.cursor_index(), 5);
        for _ in 0..4 {
            l.page_down();
        }
        assert_eq!(l.cursor_index(), 19, "clamps at last");
    }

    #[test]
    fn page_down_shrinks_its_step_to_a_shorter_render() {
        let mut l = numbered(20, 5);
        render_rows(&l, 20, l.height(20, 6));
        l.page_down();
        assert_eq!(
            l.cursor_index(),
            2,
            "two rows fit beside the four chrome rows"
        );
    }

    #[test]
    fn page_up_clamps_at_zero() {
        let mut l = numbered(10, 5);
        render_rows(&l, 20, l.height(20, 40));
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
        l.set_query("zzz");
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

    // ── height ──

    #[test]
    fn height_keeps_the_full_viewport_when_it_fits() {
        assert_eq!(list(vec![FakeItem::new("a")]).height(40, 40), 4 + 5);
    }

    #[test]
    fn height_drops_items_that_do_not_fit_whole() {
        let l = SearchableList::new("Pick", vec![FakeItem::<3> { haystack: "tall" }], 6);
        assert_eq!(
            l.height(40, 12),
            4 + 6,
            "two whole 3-row items, never a partial one"
        );
        assert_eq!(l.height(40, 3), 3, "chrome alone is clipped to the max");
    }

    // ── render ──

    #[test]
    fn render_scrolls_the_cursor_into_a_short_window() {
        let mut l = numbered(10, 5);
        for _ in 0..6 {
            l.select_next();
        }
        let rows = render_rows(&l, 20, l.height(20, 7));
        assert_eq!(rows.len(), 7);
        assert!(
            rows[4].contains("item-4") && rows[6].contains("> item-6"),
            "{rows:?}"
        );
    }

    #[test]
    fn render_at_minimum_width_does_not_panic() {
        let l = list(vec![FakeItem::new("a long-ish entry"), FakeItem::new("b")]);
        let theme = Theme::default();
        let h = l.height(20, 40);
        let mut terminal = Terminal::new(TestBackend::new(20, h)).unwrap();
        terminal
            .draw(|frame| l.render(frame, Rect::new(0, 0, 20, h), &theme))
            .expect("render must not panic");
    }

    #[test]
    fn render_places_cursor_at_prompt_plus_visible_query_width() {
        // Empty query → cursor sits at the prompt column with no `▏` glyph painted into the buffer
        // (the search row delegates to `frame.set_cursor_position`, not a fake painted cursor).
        // After `set_query("ab")` the cursor advances by the display width of the visible query.
        let theme = Theme::default();
        let h = list(vec![FakeItem::new("alpha")]).height(40, 40);
        let row_y = 2;

        let l = list(vec![FakeItem::new("alpha")]);
        let mut terminal = Terminal::new(TestBackend::new(40, h)).unwrap();
        terminal
            .draw(|frame| l.render(frame, Rect::new(0, 0, 40, h), &theme))
            .expect("render must not panic");
        let pos = terminal.get_cursor_position().unwrap();
        assert_eq!((pos.x, pos.y), (2, row_y));
        let buf = terminal.backend().buffer();
        let dump: String = (0..h)
            .flat_map(|y| (0..40).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_owned())
            .collect();
        assert!(!dump.contains('▏'), "no painted cursor glyph: {dump}");

        let mut l = list(vec![FakeItem::new("alpha")]);
        l.set_query("ab");
        let mut terminal = Terminal::new(TestBackend::new(40, h)).unwrap();
        terminal
            .draw(|frame| l.render(frame, Rect::new(0, 0, 40, h), &theme))
            .expect("render must not panic");
        let pos = terminal.get_cursor_position().unwrap();
        assert_eq!((pos.x, pos.y), (4, row_y));
    }

    #[test]
    fn render_shows_no_match_line_when_filter_excludes_everything() {
        let mut l = list(vec![FakeItem::new("alpha"), FakeItem::new("beta")]);
        l.set_query("zzz");

        let theme = Theme::default();
        let h = l.height(40, 40);
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

    #[test]
    fn render_skips_the_cursor_when_the_area_is_too_short_for_the_search_row() {
        // A modal shrunk below `title + gap + search row` must not place the cursor outside its
        // area, and the render must still complete.
        let l = list(vec![FakeItem::new("alpha")]);
        let theme = Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        terminal
            .draw(|frame| l.render(frame, Rect::new(0, 0, 40, 1), &theme))
            .expect("render must not panic when area is too short for the search row");
    }
}
