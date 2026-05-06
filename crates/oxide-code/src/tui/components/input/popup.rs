//! Slash-command autocomplete popup. Selected row paints in `text`, others dim — contrast
//! stands in for a prefix glyph or fill. Aliases parenthesize only the typed alias
//! (`/clear (new)`); the full list stays unparenthesized.
//!
//! Lists past [`MAX_VISIBLE_ROWS`] scroll with a centered cursor (Claude Code typeahead style).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::slash::{MatchedCommand, filter_built_ins};
use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

const MAX_VISIBLE_ROWS: usize = 8;

const COLUMN_GAP: usize = 2;

/// Slash-command autocomplete overlay. Empty `matches` means hidden.
pub(crate) struct SlashPopup {
    theme: Theme,
    matches: Vec<MatchedCommand>,
    selected: usize,
}

impl SlashPopup {
    pub(crate) fn new(theme: &Theme) -> Self {
        Self {
            theme: theme.clone(),
            matches: Vec::new(),
            selected: 0,
        }
    }

    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
    }

    pub(crate) fn set_query(&mut self, query: Option<&str>) {
        let Some(q) = query else {
            self.matches.clear();
            self.selected = 0;
            return;
        };
        self.matches = filter_built_ins(q);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    pub(crate) fn is_visible(&self) -> bool {
        !self.matches.is_empty()
    }

    pub(crate) fn selected(&self) -> Option<&MatchedCommand> {
        self.matches.get(self.selected)
    }

    pub(crate) fn select_next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.matches.len();
    }

    pub(crate) fn select_prev(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.matches.len() - 1
        } else {
            self.selected - 1
        };
    }

    /// Row count needed in layout; zero when hidden, capped at [`MAX_VISIBLE_ROWS`].
    pub(crate) fn height(&self) -> u16 {
        let visible = self.matches.len().min(MAX_VISIBLE_ROWS);
        u16::try_from(visible).unwrap_or(u16::MAX)
    }

    pub(crate) fn render(&self, frame: &mut Frame, area: Rect) {
        if self.matches.is_empty() {
            return;
        }
        let width = usize::from(area.width);
        let offset = self.scroll_offset();
        let visible = self.matches.len().min(MAX_VISIBLE_ROWS);
        let window = &self.matches[offset..offset + visible];
        let label_width = window.iter().map(|m| label(m).width()).max().unwrap_or(0);
        let lines: Vec<Line<'static>> = window
            .iter()
            .enumerate()
            .map(|(i, m)| self.render_row(m, offset + i == self.selected, label_width, width))
            .collect();
        frame.render_widget(Paragraph::new(lines).style(self.theme.surface()), area);
    }

    /// First visible match index. Centered-cursor scroll: the selected row sits at the visual
    /// middle once it leaves the top half, then anchors at the bottom near the end of the list.
    fn scroll_offset(&self) -> usize {
        let total = self.matches.len();
        if total <= MAX_VISIBLE_ROWS {
            return 0;
        }
        let pad = MAX_VISIBLE_ROWS / 2;
        let max_offset = total - MAX_VISIBLE_ROWS;
        self.selected.saturating_sub(pad).min(max_offset)
    }

    fn render_row(
        &self,
        m: &MatchedCommand,
        selected: bool,
        label_width: usize,
        width: usize,
    ) -> Line<'static> {
        let label_text = label(m);
        let pad = label_width.saturating_sub(label_text.width());
        let row_style = row_style(&self.theme, selected);
        let desc_budget = width.saturating_sub(label_width + COLUMN_GAP);
        let desc = truncate_to_width(m.description, desc_budget);

        let mut spans = vec![Span::styled(format!("/{}", m.name), row_style)];
        if let Some(alias) = m.matched_alias {
            spans.push(Span::styled(format!(" ({alias})"), row_style));
        }
        let gap = " ".repeat(pad + COLUMN_GAP);
        spans.push(Span::raw(gap));
        spans.push(Span::styled(desc, row_style));
        Line::from(spans)
    }
}

/// `/name` plus the typed alias when matched via alias (`/clear (new)`). Used for both
/// rendering and column-width computation, so the two must agree exactly.
fn label(m: &MatchedCommand) -> String {
    match m.matched_alias {
        Some(alias) => format!("/{} ({alias})", m.name),
        None => format!("/{}", m.name),
    }
}

/// Selected → `text` + BOLD; non-selected → dim. Contrast mirrors Claude Code's popup.
fn row_style(theme: &Theme, selected: bool) -> Style {
    if selected {
        theme.text().add_modifier(Modifier::BOLD)
    } else {
        theme.dim()
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn theme() -> Theme {
        Theme::default()
    }

    fn popup_with_query(query: Option<&str>) -> SlashPopup {
        let mut p = SlashPopup::new(&theme());
        p.set_query(query);
        p
    }

    fn render_to_backend(popup: &SlashPopup, width: u16) -> TestBackend {
        let height = popup.height().max(1);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| popup.render(frame, frame.area()))
            .unwrap();
        terminal.backend().clone()
    }

    // ── set_query ──

    #[test]
    fn set_query_none_hides_popup() {
        let popup = popup_with_query(None);
        assert!(!popup.is_visible());
        assert_eq!(popup.height(), 0);
    }

    #[test]
    fn set_query_empty_lists_full_registry_in_presentation_order() {
        // Empty query is what the user sees right after typing `/`.
        let popup = popup_with_query(Some(""));
        assert!(popup.is_visible());
        let names: Vec<&str> = popup.matches.iter().map(|m| m.name).collect();
        // BUILT_INS is alphabetical, so empty-query first row is `/clear`.
        assert!(!names.is_empty());
        assert_eq!(names[0], "clear");
    }

    #[test]
    fn set_query_clamps_selection_when_match_count_shrinks() {
        // Empty query → full list; park selection on the last row,
        // then narrow to a single match — selection must clamp into
        // range so render() doesn't index past the end.
        let mut popup = popup_with_query(Some(""));
        let n = popup.matches.len();
        for _ in 0..n - 1 {
            popup.select_next();
        }
        assert_eq!(popup.selected, n - 1);

        popup.set_query(Some("help"));
        assert_eq!(popup.matches.len(), 1);
        assert_eq!(popup.selected, 0);
    }

    // ── select_next / select_prev ──

    #[test]
    fn select_next_wraps_at_bottom() {
        let mut popup = popup_with_query(Some(""));
        let n = popup.matches.len();
        for _ in 0..n {
            popup.select_next();
        }
        assert_eq!(popup.selected, 0, "wrap from last back to first");
    }

    #[test]
    fn select_prev_wraps_at_top() {
        let mut popup = popup_with_query(Some(""));
        let n = popup.matches.len();
        popup.select_prev();
        assert_eq!(popup.selected, n - 1, "wrap from first up to last");
    }

    #[test]
    fn select_prev_decrements_when_not_at_top() {
        // Pin the non-wrap branch — the decrement path is otherwise
        // dead because select_prev() from row 0 always wraps.
        let mut popup = popup_with_query(Some(""));
        popup.select_next();
        popup.select_next();
        assert_eq!(popup.selected, 2);

        popup.select_prev();
        assert_eq!(popup.selected, 1);
    }

    #[test]
    fn select_next_on_empty_popup_is_a_noop() {
        let mut popup = popup_with_query(None);
        popup.select_next();
        popup.select_prev();
        assert_eq!(popup.selected, 0);
    }

    // ── height ──

    #[test]
    fn height_caps_at_max_visible_rows() {
        let popup = popup_with_query(Some(""));
        let expected = popup.matches.len().min(MAX_VISIBLE_ROWS);
        assert_eq!(usize::from(popup.height()), expected);
    }

    // ── scroll_offset ──

    fn long_popup(n: usize) -> SlashPopup {
        // Hand-rolled match list keeps the test independent of registry growth.
        let mut p = SlashPopup::new(&theme());
        p.matches = (0..n)
            .map(|i| MatchedCommand {
                name: Box::leak(format!("cmd{i}").into_boxed_str()),
                description: "desc",
                matched_alias: None,
            })
            .collect();
        p
    }

    #[test]
    fn scroll_offset_anchors_at_top_while_cursor_in_first_half() {
        let mut p = long_popup(MAX_VISIBLE_ROWS + 4);
        for _ in 0..MAX_VISIBLE_ROWS / 2 {
            assert_eq!(p.scroll_offset(), 0);
            p.select_next();
        }
    }

    #[test]
    fn scroll_offset_centers_cursor_past_first_half() {
        // pad = MAX_VISIBLE_ROWS / 2 = 4 for the default cap. At selected = pad + k the offset is
        // k, keeping the cursor visually at row `pad`.
        let mut p = long_popup(MAX_VISIBLE_ROWS + 4);
        for _ in 0..=(MAX_VISIBLE_ROWS / 2) {
            p.select_next();
        }
        assert_eq!(p.scroll_offset(), 1, "cursor at pad + 1 → offset = 1");
    }

    #[test]
    fn scroll_offset_anchors_at_bottom_near_end() {
        // Once selected nears the end, the offset clamps to `len - MAX_VISIBLE_ROWS` so the last
        // row stays visible while the cursor advances within the bottom-anchored window.
        let total = MAX_VISIBLE_ROWS + 4;
        let mut p = long_popup(total);
        while p.selected < total - 1 {
            p.select_next();
        }
        assert_eq!(p.scroll_offset(), total - MAX_VISIBLE_ROWS);
    }

    #[test]
    fn scroll_offset_resets_on_wrap_to_first_row() {
        let total = MAX_VISIBLE_ROWS + 4;
        let mut p = long_popup(total);
        for _ in 0..total {
            p.select_next();
        }
        assert_eq!(p.selected, 0, "wrap to row 0");
        assert_eq!(p.scroll_offset(), 0, "wrap snaps the window back to top");
    }

    #[test]
    fn scroll_offset_is_zero_when_total_fits_window() {
        // With `MAX_VISIBLE_ROWS = 8` the live registry of 9 commands does scroll, but a
        // 5-element fake list must never offset.
        let mut p = long_popup(5);
        p.select_next();
        p.select_next();
        assert_eq!(p.scroll_offset(), 0);
    }

    #[test]
    fn scroll_offset_keeps_visible_row_at_pad_for_mid_list_selection() {
        // Pin the centering invariant directly: the visible row of the cursor (selected - offset)
        // equals `pad` whenever the selection is past the top half but not yet near the bottom.
        // Mutating the divisor (e.g. `/3`) or the formula would shift this row index.
        let total = MAX_VISIBLE_ROWS + 4;
        let mut p = long_popup(total);
        let pad = MAX_VISIBLE_ROWS / 2;
        for _ in 0..=(pad + 1) {
            p.select_next();
        }
        let visible_row = p.selected - p.scroll_offset();
        assert_eq!(visible_row, pad, "cursor must sit at the visual middle row");
    }

    #[test]
    fn scroll_offset_at_exactly_cap_returns_zero_for_last_row() {
        // Boundary: total == MAX_VISIBLE_ROWS hits the `<=` early-return. Mutating to `<` would
        // try to compute max_offset = 0 and still produce 0 here, but tightening the boundary
        // pins the invariant for the only case where the edge matters.
        let mut p = long_popup(MAX_VISIBLE_ROWS);
        while p.selected < MAX_VISIBLE_ROWS - 1 {
            p.select_next();
        }
        assert_eq!(p.scroll_offset(), 0);
    }

    #[test]
    fn scroll_offset_select_prev_from_top_anchors_at_bottom_window() {
        // Up-arrow from row 0 wraps to the last row; the bottom-anchored window must clamp to
        // `len - MAX_VISIBLE_ROWS` (the symmetric case to the existing wrap-to-top test).
        let total = MAX_VISIBLE_ROWS + 4;
        let mut p = long_popup(total);
        p.select_prev();
        assert_eq!(p.selected, total - 1, "wrap to last row");
        assert_eq!(p.scroll_offset(), total - MAX_VISIBLE_ROWS);
    }

    // ── selected ──

    #[test]
    fn selected_picks_match_at_index() {
        let mut popup = popup_with_query(Some(""));
        popup.select_next();
        let row = popup.selected().expect("popup visible");
        assert_eq!(row.name, popup.matches[1].name);
    }

    #[test]
    fn selected_is_none_when_hidden() {
        let popup = popup_with_query(None);
        assert!(popup.selected().is_none());
    }

    // ── render ──

    #[test]
    fn render_empty_query_shows_each_command_once() {
        let popup = popup_with_query(Some(""));
        insta::assert_snapshot!(render_to_backend(&popup, 60));
    }

    #[test]
    fn render_filtered_query_shows_only_matching_rows() {
        // Narrow query → single row. Confirms filter wiring and
        // that the unmatched commands disappear.
        let popup = popup_with_query(Some("hel"));
        insta::assert_snapshot!(render_to_backend(&popup, 60));
    }

    #[test]
    fn render_selected_row_paints_bold_text_others_dim() {
        // TestBackend snapshots don't capture style, so layout-only
        // snapshots can't tell selected from unselected. Pin the
        // bold-vs-dim contrast directly on the rendered cells.
        use ratatui::layout::Position;
        use ratatui::style::Modifier;

        let theme = theme();
        let mut popup = popup_with_query(Some(""));
        popup.select_next();
        let backend = render_to_backend(&popup, 60);

        let selected = backend.buffer().cell(Position::new(0, 1)).unwrap();
        assert_eq!(selected.fg, theme.text().fg.unwrap());
        assert!(
            selected.modifier.contains(Modifier::BOLD),
            "selected row should paint bold: got {:?}",
            selected.modifier,
        );

        let unselected = backend.buffer().cell(Position::new(0, 0)).unwrap();
        assert_eq!(unselected.fg, theme.dim().fg.unwrap());
        assert!(
            !unselected.modifier.contains(Modifier::BOLD),
            "unselected row must not be bold: got {:?}",
            unselected.modifier,
        );
    }

    #[test]
    fn render_narrow_terminal_truncates_description() {
        // At 30 cols the description gutter must shrink and wrap
        // through ELLIPSIS rather than overflowing the row.
        let popup = popup_with_query(Some(""));
        insta::assert_snapshot!(render_to_backend(&popup, 30));
    }

    #[test]
    fn render_alias_match_parenthesizes_only_typed_alias() {
        // No live registry command has aliases yet, but the renderer
        // is the place where the alias-display rule lives. Drive it via a hand-rolled match list.
        let mut popup = SlashPopup::new(&theme());
        popup.matches = vec![MatchedCommand {
            name: "clear",
            description: "wipe transcript",
            matched_alias: Some("new"),
        }];
        insta::assert_snapshot!(render_to_backend(&popup, 60));
    }

    #[test]
    fn render_hidden_popup_emits_nothing() {
        // The hidden-popup early-return in render() is otherwise
        // unreached — App's draw method gates by height(), so the
        // function only fires when the popup chose to be visible.
        let popup = popup_with_query(None);
        let backend = render_to_backend(&popup, 60);
        let buf = backend.buffer();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = buf.cell(ratatui::layout::Position::new(x, y)).unwrap();
                assert_eq!(cell.symbol(), " ", "hidden popup must paint nothing");
            }
        }
    }

    #[test]
    fn render_after_scroll_shows_window_starting_at_offset() {
        // 12 fake rows; advance to row 8 to push the window past the top so `cmd0` is hidden and
        // `cmd8` is visible — confirms the slice and the offset agree.
        let mut popup = long_popup(12);
        for _ in 0..8 {
            popup.select_next();
        }
        let backend = render_to_backend(&popup, 30);
        let rendered = format!("{backend}");
        assert!(!rendered.contains("cmd0"), "cmd0 scrolled off: {rendered}");
        assert!(rendered.contains("cmd8"), "cmd8 in window: {rendered}");
    }
}
