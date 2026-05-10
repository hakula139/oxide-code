//! Searchable + scrollable list primitive for [`Modal`](super::Modal) impls. Adds a search
//! input + scrollable viewport on top of the [`ListPicker`](super::list_picker::ListPicker)
//! shape; concrete pickers own submit semantics.

use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

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
        Self: Sized,
    {
        1
    }
}

// ── SearchableList ──

const CURSOR_MARKER: &str = "> ";
const CURSOR_MARKER_WIDTH: u16 = 2;
const SEARCH_PROMPT: &str = "/ ";
const SEARCH_PROMPT_WIDTH: u16 = 2;
const TITLE_ROW_HEIGHT: u16 = 1;
const SEARCH_ROW_HEIGHT: u16 = 1;
const SECTION_GAP: u16 = 1;

/// Selectable + searchable list with a scrollable viewport. Cursor walks the **filtered** index
/// space, so out-of-filter rows are skipped by navigation.
pub(crate) struct SearchableList<T: SearchableItem> {
    title: String,
    description: Option<String>,
    items: Vec<T>,
    query: String,
    /// Indices into `items` that pass the current `query`, in original item order.
    visible: Vec<usize>,
    /// Cursor into `visible`. Resets to 0 on filter changes.
    cursor: usize,
    /// First visible row painted. Tracks `cursor` to stay on screen.
    viewport_offset: usize,
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

    /// Replace all items and re-run the filter. Cursor and viewport reset to 0.
    pub(crate) fn replace_items(&mut self, items: Vec<T>) {
        self.items = items;
        self.recompute_visible();
    }

    /// Seek the cursor to the first visible row matching `f`, scrolling it into view. No-op if no
    /// row matches. Pairs with [`Self::replace_items`] to keep the cursor pinned across a refresh.
    pub(crate) fn cursor_to<F: Fn(&T) -> bool>(&mut self, f: F) {
        for (vi, &item_idx) in self.visible.iter().enumerate() {
            if self.items.get(item_idx).is_some_and(&f) {
                self.cursor = vi;
                self.scroll_into_view();
                return;
            }
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

    /// Rows currently passing the filter — for "X / Y matching" footers.
    pub(crate) fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub(crate) fn is_filtered(&self) -> bool {
        !self.query.is_empty()
    }

    pub(crate) fn select_next(&mut self) {
        let Some(len) = self.nonzero_visible_len() else {
            return;
        };
        self.cursor = (self.cursor + 1) % len;
        self.scroll_into_view();
    }

    pub(crate) fn select_prev(&mut self) {
        let Some(len) = self.nonzero_visible_len() else {
            return;
        };
        self.cursor = if self.cursor == 0 {
            len - 1
        } else {
            self.cursor - 1
        };
        self.scroll_into_view();
    }

    pub(crate) fn page_down(&mut self) {
        let Some(len) = self.nonzero_visible_len() else {
            return;
        };
        let step = usize::from(self.viewport_height).max(1);
        self.cursor = (self.cursor + step).min(len - 1);
        self.scroll_into_view();
    }

    pub(crate) fn page_up(&mut self) {
        let Some(_len) = self.nonzero_visible_len() else {
            return;
        };
        let step = usize::from(self.viewport_height).max(1);
        self.cursor = self.cursor.saturating_sub(step);
        self.scroll_into_view();
    }

    fn nonzero_visible_len(&self) -> Option<usize> {
        (!self.visible.is_empty()).then_some(self.visible.len())
    }

    fn recompute_visible(&mut self) {
        // Empty-needle fast path skips the per-row to_lowercase allocation.
        self.visible = if self.query.is_empty() {
            (0..self.items.len()).collect()
        } else {
            let needle = self.query.to_lowercase();
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

    /// Total rows the list occupies. Caller adds footer / borders.
    pub(crate) fn height(&self, _width: u16) -> u16 {
        self.chrome_height()
            .saturating_add(self.viewport_height.saturating_mul(T::row_height()))
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let mut lines: Vec<Line<'static>> = Vec::new();

        lines.push(Line::from(Span::styled(
            self.title.clone(),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
        if let Some(desc) = &self.description {
            lines.push(Line::from(Span::styled(desc.clone(), theme.dim())));
        }
        lines.push(Line::default());

        let (search_line, query_display_width) = self.render_search_row(area.width, theme);
        lines.push(search_line);
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
            Self::push_item_lines(&mut lines, item, is_cursor, row_width, theme);
        }

        if self.visible.is_empty() && !self.query.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("    no matches for `{}`", self.query),
                theme.dim(),
            )));
        }

        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
        self.place_terminal_cursor(frame, area, query_display_width);
    }

    /// Paint the search prompt + query (or placeholder hint). Returns the rendered query's display
    /// width so the caller can place the terminal-native cursor at the insertion point.
    fn render_search_row(&self, area_width: u16, theme: &Theme) -> (Line<'static>, u16) {
        let prompt_style = theme.accent();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(2);
        spans.push(Span::styled(SEARCH_PROMPT.to_owned(), prompt_style));
        if self.query.is_empty() {
            spans.push(Span::styled(
                "Type to filter (substring match)".to_owned(),
                theme.dim(),
            ));
            (Line::from(spans), 0)
        } else {
            let budget = usize::from(area_width.saturating_sub(SEARCH_PROMPT_WIDTH + 1));
            let shown = truncate_to_width(&self.query, budget);
            let width = u16::try_from(UnicodeWidthStr::width(shown.as_str())).unwrap_or(u16::MAX);
            spans.push(Span::styled(shown, theme.text()));
            (Line::from(spans), width)
        }
    }

    /// Anchor the terminal-native cursor at the search row's insertion point — matches the input
    /// panel's cursor (terminal-themed shape, OS-blinking) instead of painting a static glyph at
    /// the wrong column when the query is empty.
    fn place_terminal_cursor(&self, frame: &mut Frame<'_>, area: Rect, query_display_width: u16) {
        let search_y_offset =
            TITLE_ROW_HEIGHT + u16::from(self.description.is_some()) + SECTION_GAP;
        if search_y_offset >= area.height {
            return;
        }
        let cursor_y = area.y.saturating_add(search_y_offset);
        let raw_x = area
            .x
            .saturating_add(SEARCH_PROMPT_WIDTH)
            .saturating_add(query_display_width);
        crate::tui::cursor::place_clamped(frame, raw_x, cursor_y, area);
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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

        fn render(&self, width: u16, _is_cursor: bool, theme: &Theme) -> Vec<Line<'static>> {
            let trimmed = truncate_to_width(self.haystack, usize::from(width));
            vec![Line::from(Span::styled(trimmed, theme.text()))]
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
        l.set_query("ap".to_owned());
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
    fn render_at_minimum_width_does_not_panic() {
        let l = list(vec![FakeItem::new("a long-ish entry"), FakeItem::new("b")]);
        let theme = Theme::default();
        let h = l.height(20);
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
        let h = list(vec![FakeItem::new("alpha")]).height(40);
        let row_y = TITLE_ROW_HEIGHT + SECTION_GAP;

        let l = list(vec![FakeItem::new("alpha")]);
        let mut terminal = Terminal::new(TestBackend::new(40, h)).unwrap();
        terminal
            .draw(|frame| l.render(frame, Rect::new(0, 0, 40, h), &theme))
            .expect("render must not panic");
        let pos = terminal.get_cursor_position().unwrap();
        assert_eq!((pos.x, pos.y), (SEARCH_PROMPT_WIDTH, row_y));
        let buf = terminal.backend().buffer();
        let dump: String = (0..h)
            .flat_map(|y| (0..40).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_owned())
            .collect();
        assert!(!dump.contains('▏'), "no painted cursor glyph: {dump}");

        let mut l = list(vec![FakeItem::new("alpha")]);
        l.set_query("ab".to_owned());
        let mut terminal = Terminal::new(TestBackend::new(40, h)).unwrap();
        terminal
            .draw(|frame| l.render(frame, Rect::new(0, 0, 40, h), &theme))
            .expect("render must not panic");
        let pos = terminal.get_cursor_position().unwrap();
        assert_eq!((pos.x, pos.y), (SEARCH_PROMPT_WIDTH + 2, row_y));
    }

    #[test]
    fn render_shows_no_match_line_when_filter_excludes_everything() {
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

    #[test]
    fn place_terminal_cursor_skips_when_area_too_short_for_search_row() {
        // Defensive guard: when the host shrinks the modal to fewer rows than `title + gap +
        // search-row` requires, `place_terminal_cursor` returns early instead of placing the
        // cursor outside the area. The render call must still complete without panicking.
        let l = list(vec![FakeItem::new("alpha")]);
        let theme = Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        terminal
            .draw(|frame| l.render(frame, Rect::new(0, 0, 40, 1), &theme))
            .expect("render must not panic when area is too short for the search row");
    }
}
