//! Slash-command autocomplete popup. Two modes:
//!
//! - **Name** — typing `/cmd`. Rows are matched commands; aliases parenthesize only the typed
//!   alias (`/clear (new)`). Tab inserts `/{name} ` into the buffer.
//! - **Arg** — typing `/cmd <prefix>`. Rows are arg completions from the command's curated
//!   roster (`/model`, `/effort`, `/theme`). Tab replaces the prefix with `/{cmd} {value} `.
//!
//! Selected row paints in `text` + BOLD; others dim — contrast stands in for a prefix glyph
//! or fill. Lists past [`MAX_VISIBLE_ROWS`] scroll with a centered cursor (Claude Code
//! typeahead style).

use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::slash::{PopupState, complete_arg_for, filter_built_ins};
use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

const MAX_VISIBLE_ROWS: usize = 8;

const COLUMN_GAP: usize = 2;

/// Mode-tagged so [`super::InputArea`] can format the right Tab-insertion text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PopupMode {
    Name,
    /// Owned because the cmd name comes from the live input buffer.
    Arg {
        cmd: String,
    },
}

/// One popup row. `value` is the bare token (command name or arg value); the renderer adds
/// `/` and any matched-alias suffix in name mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PopupRow {
    pub(super) value: Cow<'static, str>,
    pub(super) description: Cow<'static, str>,
    /// Set in name mode when the typed query matched an alias rather than the canonical name.
    pub(super) matched_alias: Option<&'static str>,
}

/// Slash-command autocomplete overlay. Empty `rows` means hidden.
pub(crate) struct SlashPopup {
    theme: Theme,
    /// `Some` when visible. Mode discriminates which Tab-insertion shape applies.
    mode: Option<PopupMode>,
    rows: Vec<PopupRow>,
    selected: usize,
}

impl SlashPopup {
    pub(crate) fn new(theme: &Theme) -> Self {
        Self {
            theme: theme.clone(),
            mode: None,
            rows: Vec::new(),
            selected: 0,
        }
    }

    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
    }

    pub(crate) fn set_state(&mut self, state: Option<&PopupState<'_>>) {
        let (mode, rows) = match state {
            None => (None, Vec::new()),
            Some(PopupState::Name(query)) => (
                Some(PopupMode::Name),
                filter_built_ins(query)
                    .into_iter()
                    .map(|m| PopupRow {
                        value: Cow::Borrowed(m.name),
                        description: Cow::Borrowed(m.description),
                        matched_alias: m.matched_alias,
                    })
                    .collect(),
            ),
            Some(PopupState::Arg { name, prefix }) => {
                let completions = complete_arg_for(name, prefix);
                if completions.is_empty() {
                    // No curated roster for this command — hide the popup so it doesn't
                    // intercept Tab / Enter while the user types the arg.
                    (None, Vec::new())
                } else {
                    (
                        Some(PopupMode::Arg {
                            cmd: (*name).to_owned(),
                        }),
                        completions
                            .into_iter()
                            .map(|c| PopupRow {
                                value: c.value,
                                description: c.description,
                                matched_alias: None,
                            })
                            .collect(),
                    )
                }
            }
        };
        // Mode transitions reset to row 0 (rosters are unrelated). Intra-mode query / prefix
        // changes clamp instead so the cursor sticks.
        let mode_changed = self.mode != mode;
        self.mode = mode;
        self.rows = rows;
        self.selected = if mode_changed {
            0
        } else {
            self.selected.min(self.rows.len().saturating_sub(1))
        };
    }

    pub(crate) fn is_visible(&self) -> bool {
        !self.rows.is_empty()
    }

    pub(crate) fn mode(&self) -> Option<&PopupMode> {
        self.mode.as_ref()
    }

    pub(crate) fn selected(&self) -> Option<&PopupRow> {
        self.rows.get(self.selected)
    }

    pub(crate) fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.rows.len();
    }

    pub(crate) fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.rows.len() - 1
        } else {
            self.selected - 1
        };
    }

    /// Row count needed in layout; zero when hidden, capped at [`MAX_VISIBLE_ROWS`].
    pub(crate) fn height(&self) -> u16 {
        let visible = self.rows.len().min(MAX_VISIBLE_ROWS);
        u16::try_from(visible).unwrap_or(u16::MAX)
    }

    pub(crate) fn render(&self, frame: &mut Frame, area: Rect) {
        if self.rows.is_empty() {
            return;
        }
        let width = usize::from(area.width);
        let offset = self.scroll_offset();
        let visible = self.rows.len().min(MAX_VISIBLE_ROWS);
        let window = &self.rows[offset..offset + visible];
        // Cache labels so width measurement and row rendering can't drift.
        let labels: Vec<String> = window.iter().map(|r| self.label(r)).collect();
        let label_width = labels.iter().map(|l| l.width()).max().unwrap_or(0);
        let lines: Vec<Line<'static>> = window
            .iter()
            .zip(labels)
            .enumerate()
            .map(|(i, (r, label))| {
                self.render_row(r, label, offset + i == self.selected, label_width, width)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines).style(self.theme.surface()), area);
    }

    /// First visible match index. Centered-cursor scroll: the selected row sits at the visual
    /// middle once it leaves the top half, then anchors at the bottom near the end of the list.
    fn scroll_offset(&self) -> usize {
        let total = self.rows.len();
        if total <= MAX_VISIBLE_ROWS {
            return 0;
        }
        let pad = MAX_VISIBLE_ROWS / 2;
        let max_offset = total - MAX_VISIBLE_ROWS;
        self.selected.saturating_sub(pad).min(max_offset)
    }

    /// Mode-aware left-column label. `None` arm is a defensive fallback — `render` gates on
    /// non-empty `rows`, which keeps `mode` `Some`.
    fn label(&self, row: &PopupRow) -> String {
        match &self.mode {
            Some(PopupMode::Name) => match row.matched_alias {
                Some(alias) => format!("/{} ({alias})", row.value),
                None => format!("/{}", row.value),
            },
            Some(PopupMode::Arg { .. }) | None => row.value.to_string(),
        }
    }

    fn render_row(
        &self,
        row: &PopupRow,
        label_text: String,
        selected: bool,
        label_width: usize,
        width: usize,
    ) -> Line<'static> {
        let pad = label_width.saturating_sub(label_text.width());
        let row_style = row_style(&self.theme, selected);
        let desc_budget = width.saturating_sub(label_width + COLUMN_GAP);
        let desc = truncate_to_width(&row.description, desc_budget);

        let mut spans = vec![Span::styled(label_text, row_style)];
        let gap = " ".repeat(pad + COLUMN_GAP);
        spans.push(Span::raw(gap));
        spans.push(Span::styled(desc, row_style));
        Line::from(spans)
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

    fn popup_with_state(state: Option<&PopupState<'_>>) -> SlashPopup {
        let mut p = SlashPopup::new(&theme());
        p.set_state(state);
        p
    }

    fn name_popup(query: &str) -> SlashPopup {
        popup_with_state(Some(&PopupState::Name(query)))
    }

    fn arg_popup<'a>(name: &'a str, prefix: &'a str) -> SlashPopup {
        popup_with_state(Some(&PopupState::Arg { name, prefix }))
    }

    fn render_to_backend(popup: &SlashPopup, width: u16) -> TestBackend {
        let height = popup.height().max(1);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| popup.render(frame, frame.area()))
            .unwrap();
        terminal.backend().clone()
    }

    // ── set_state ──

    #[test]
    fn set_state_none_hides_popup() {
        let popup = popup_with_state(None);
        assert!(!popup.is_visible());
        assert_eq!(popup.height(), 0);
    }

    #[test]
    fn set_state_name_empty_query_lists_full_registry_in_presentation_order() {
        // Empty query is what the user sees right after typing `/`.
        let popup = name_popup("");
        assert!(popup.is_visible());
        assert_eq!(popup.mode(), Some(&PopupMode::Name));
        let values: Vec<String> = popup
            .rows
            .iter()
            .map(|r| r.value.clone().into_owned())
            .collect();
        // BUILT_INS is alphabetical, so empty-query first row is `clear`.
        assert!(!values.is_empty());
        assert_eq!(values[0], "clear");
    }

    #[test]
    fn set_state_clamps_selection_when_row_count_shrinks() {
        // Empty query → full list; park selection on the last row, then narrow to a single match
        // — selection must clamp so render() doesn't index past the end.
        let mut popup = name_popup("");
        let n = popup.rows.len();
        for _ in 0..n - 1 {
            popup.select_next();
        }
        assert_eq!(popup.selected, n - 1);

        popup.set_state(Some(&PopupState::Name("help")));
        assert_eq!(popup.rows.len(), 1);
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn set_state_resets_selection_on_mode_transition() {
        // Park selection on a non-zero name-mode row, then transition to arg mode — the new
        // mode's roster is unrelated, so selection must drop back to row 0 instead of pointing
        // at whichever index happens to survive the clamp.
        let mut popup = name_popup("");
        popup.select_next();
        popup.select_next();
        assert!(popup.selected >= 2, "park selection past row 0");

        popup.set_state(Some(&PopupState::Arg {
            name: "model",
            prefix: "",
        }));
        assert!(matches!(popup.mode(), Some(PopupMode::Arg { .. })));
        assert_eq!(popup.selected, 0, "mode transition resets selection");
    }

    #[test]
    fn set_state_arg_with_curated_roster_populates_arg_mode() {
        let popup = arg_popup("model", "");
        assert!(popup.is_visible());
        assert!(matches!(popup.mode(), Some(PopupMode::Arg { cmd }) if cmd == "model"));
        // Roster is non-empty for /model.
        assert!(!popup.rows.is_empty());
    }

    #[test]
    fn set_state_arg_with_empty_roster_stays_hidden() {
        // /init has no curated arg roster — popup must hide so the user can type the arg
        // without the popup intercepting Tab / Enter.
        let popup = arg_popup("init", "");
        assert!(!popup.is_visible());
        assert!(popup.mode().is_none());
    }

    #[test]
    fn set_state_arg_unknown_command_stays_hidden() {
        let popup = arg_popup("nope", "");
        assert!(!popup.is_visible());
    }

    // ── selected ──

    #[test]
    fn selected_picks_row_at_index() {
        let mut popup = name_popup("");
        popup.select_next();
        let row = popup.selected().expect("popup visible");
        assert_eq!(row.value, popup.rows[1].value);
    }

    #[test]
    fn selected_is_none_when_hidden() {
        let popup = popup_with_state(None);
        assert!(popup.selected().is_none());
    }

    // ── select_next / select_prev ──

    #[test]
    fn select_next_wraps_at_bottom() {
        let mut popup = name_popup("");
        let n = popup.rows.len();
        for _ in 0..n {
            popup.select_next();
        }
        assert_eq!(popup.selected, 0, "wrap from last back to first");
    }

    #[test]
    fn select_prev_wraps_at_top() {
        let mut popup = name_popup("");
        let n = popup.rows.len();
        popup.select_prev();
        assert_eq!(popup.selected, n - 1, "wrap from first up to last");
    }

    #[test]
    fn select_prev_decrements_when_not_at_top() {
        // Pin the non-wrap branch — the decrement path is otherwise dead because select_prev()
        // from row 0 always wraps.
        let mut popup = name_popup("");
        popup.select_next();
        popup.select_next();
        assert_eq!(popup.selected, 2);

        popup.select_prev();
        assert_eq!(popup.selected, 1);
    }

    #[test]
    fn select_next_on_empty_popup_is_a_noop() {
        let mut popup = popup_with_state(None);
        popup.select_next();
        popup.select_prev();
        assert_eq!(popup.selected, 0);
    }

    // ── height ──

    #[test]
    fn height_caps_at_max_visible_rows() {
        let popup = name_popup("");
        let expected = popup.rows.len().min(MAX_VISIBLE_ROWS);
        assert_eq!(usize::from(popup.height()), expected);
    }

    // ── render ──

    fn long_popup(n: usize) -> SlashPopup {
        // Hand-rolled list keeps the test independent of registry growth.
        let mut p = SlashPopup::new(&theme());
        p.mode = Some(PopupMode::Name);
        p.rows = (0..n)
            .map(|i| PopupRow {
                value: Cow::Owned(format!("cmd{i}")),
                description: Cow::Borrowed("desc"),
                matched_alias: None,
            })
            .collect();
        p
    }

    #[test]
    fn render_empty_name_query_shows_each_command_once() {
        let popup = name_popup("");
        insta::assert_snapshot!(render_to_backend(&popup, 60));
    }

    #[test]
    fn render_filtered_name_query_shows_only_matching_rows() {
        // Narrow query → single row. Confirms filter wiring and that unmatched commands disappear.
        let popup = name_popup("hel");
        insta::assert_snapshot!(render_to_backend(&popup, 60));
    }

    #[test]
    fn render_arg_mode_lists_curated_roster_without_slash_prefix() {
        // Arg-mode rows render bare values (`low`, `medium`, ...) — no leading `/`.
        let popup = arg_popup("effort", "");
        insta::assert_snapshot!(render_to_backend(&popup, 60));
    }

    #[test]
    fn render_arg_mode_prefix_filters_to_subset() {
        // Pin that arg-mode prefix filtering reaches the renderer (not just the data layer).
        let popup = arg_popup("theme", "m");
        insta::assert_snapshot!(render_to_backend(&popup, 60));
    }

    #[test]
    fn render_selected_row_paints_bold_text_others_dim() {
        // TestBackend snapshots don't capture style, so layout-only snapshots can't tell selected
        // from unselected. Pin the bold-vs-dim contrast directly on the rendered cells.
        use ratatui::layout::Position;
        use ratatui::style::Modifier;

        let theme = theme();
        let mut popup = name_popup("");
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
        // At 30 cols the description gutter must shrink and wrap through ELLIPSIS rather than
        // overflowing the row.
        let popup = name_popup("");
        insta::assert_snapshot!(render_to_backend(&popup, 30));
    }

    #[test]
    fn render_alias_match_parenthesizes_only_typed_alias() {
        // No live registry command has aliases yet, but the renderer is the place where the
        // alias-display rule lives. Drive it via a hand-rolled match list.
        let mut popup = SlashPopup::new(&theme());
        popup.mode = Some(PopupMode::Name);
        popup.rows = vec![PopupRow {
            value: Cow::Borrowed("clear"),
            description: Cow::Borrowed("wipe transcript"),
            matched_alias: Some("new"),
        }];
        insta::assert_snapshot!(render_to_backend(&popup, 60));
    }

    #[test]
    fn render_hidden_popup_emits_nothing() {
        // The hidden-popup early-return in render() is otherwise unreached — App's draw method
        // gates by height(), so the function only fires when the popup chose to be visible.
        let popup = popup_with_state(None);
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
        // 12 fake rows; advance to row 8 to push the window past the top so `cmd0` is hidden
        // and `cmd8` is visible — confirms the slice and the offset agree.
        let mut popup = long_popup(12);
        for _ in 0..8 {
            popup.select_next();
        }
        let backend = render_to_backend(&popup, 30);
        let rendered = format!("{backend}");
        assert!(!rendered.contains("cmd0"), "cmd0 scrolled off: {rendered}");
        assert!(rendered.contains("cmd8"), "cmd8 in window: {rendered}");
    }

    // ── scroll_offset ──

    #[test]
    fn scroll_offset_is_zero_when_total_fits_window() {
        // total < cap → no scroll regardless of cursor.
        let mut p = long_popup(5);
        p.select_next();
        p.select_next();
        assert_eq!(p.scroll_offset(), 0);
    }

    #[test]
    fn scroll_offset_at_exactly_cap_returns_zero_for_last_row() {
        // Boundary: total == MAX_VISIBLE_ROWS hits the `<=` early-return. Tightening the boundary
        // pins the invariant for the only case where the edge matters.
        let mut p = long_popup(MAX_VISIBLE_ROWS);
        while p.selected < MAX_VISIBLE_ROWS - 1 {
            p.select_next();
        }
        assert_eq!(p.scroll_offset(), 0);
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
    fn scroll_offset_select_prev_from_top_anchors_at_bottom_window() {
        // Up-arrow from row 0 wraps to the last row; the bottom-anchored window must clamp to
        // `len - MAX_VISIBLE_ROWS` (the symmetric case to the wrap-to-top test above).
        let total = MAX_VISIBLE_ROWS + 4;
        let mut p = long_popup(total);
        p.select_prev();
        assert_eq!(p.selected, total - 1, "wrap to last row");
        assert_eq!(p.scroll_offset(), total - MAX_VISIBLE_ROWS);
    }
}
