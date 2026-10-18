//! Slash-command autocomplete popup. Two modes:
//!
//! - **Name**: typing `/cmd`. Rows are matched commands. Aliases parenthesize only the typed
//!   alias (`/clear (new)`). Tab inserts `/{name} ` into the buffer.
//! - **Arg**: typing `/cmd <prefix>`. Rows are arg completions from the command's curated
//!   roster (`/model`, `/effort`, `/theme`). Tab replaces the prefix with `/{cmd} {value} `.
//!
//! Selected row paints in `text` + BOLD. Other rows dim so contrast stands in for a prefix glyph
//! or fill. Lists past [`MAX_VISIBLE_ROWS`] scroll with a centered cursor (Claude Code
//! typeahead style).

use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::slash::{PopupState, complete_arg_for, filter_built_ins};
use crate::tui::saturating_u16;
use crate::tui::theme::Theme;
use crate::tui::viewport::{wrap_next, wrap_prev};
use crate::util::text::truncate_to_width;

const MAX_VISIBLE_ROWS: usize = 8;

const COLUMN_GAP: usize = 2;

/// Rows of chrome above the row list. One dim horizontal rule frames the popup as a separate
/// overlay so it doesn't bleed into whatever sits above (welcome, chat, preview).
const CHROME_ROWS: u16 = 1;

/// What the rows complete: a command name, or an argument of the named command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PopupMode {
    Name,
    /// Owned because the name comes from the live input buffer.
    Arg {
        name: String,
    },
}

impl PopupMode {
    /// Left-column label. Name mode adds the `/` and parenthesizes a matched alias.
    fn label(&self, row: &PopupRow) -> String {
        match self {
            Self::Name => match row.matched_alias {
                Some(alias) => format!("/{} ({alias})", row.value),
                None => format!("/{}", row.value),
            },
            Self::Arg { .. } => row.value.to_string(),
        }
    }
}

/// One popup row. `value` is the bare token (command name or arg value). The renderer adds
/// `/` and any matched-alias suffix in name mode.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PopupRow {
    value: Cow<'static, str>,
    description: Cow<'static, str>,
    /// The alias a name-mode query matched, or `None` when it matched the canonical name.
    matched_alias: Option<&'static str>,
}

/// Slash-command autocomplete overlay, hidden while `list` is `None`.
pub(super) struct SlashPopup {
    theme: Theme,
    list: Option<PopupList>,
    selected: usize,
}

/// The rows a visible popup lists, never empty, and the mode they complete in.
struct PopupList {
    mode: PopupMode,
    rows: Vec<PopupRow>,
}

impl SlashPopup {
    pub(super) fn new(theme: &Theme) -> Self {
        Self {
            theme: theme.clone(),
            list: None,
            selected: 0,
        }
    }

    /// Re-skins every subsequent render.
    pub(super) fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
    }

    pub(super) fn set_state(&mut self, state: Option<&PopupState<'_>>) {
        let list = state.and_then(PopupList::new);
        // Mode transitions reset to row 0 (rosters are unrelated). Intra-mode query / prefix
        // changes clamp instead so the cursor sticks.
        self.selected = match (&self.list, &list) {
            (Some(old), Some(new)) if old.mode == new.mode => self.selected.min(new.rows.len() - 1),
            _ => 0,
        };
        self.list = list;
    }

    pub(super) fn is_visible(&self) -> bool {
        self.list.is_some()
    }

    /// Returns the command line the selected row completes to: `/{name}` in name mode, or
    /// `/{cmd} {value}` in arg mode.
    pub(super) fn completion(&self) -> Option<String> {
        let list = self.list.as_ref()?;
        let value = &list.rows[self.selected].value;
        Some(match &list.mode {
            PopupMode::Name => format!("/{value}"),
            PopupMode::Arg { name } => format!("/{name} {value}"),
        })
    }

    pub(super) fn select_next(&mut self) {
        if self.rows().is_empty() {
            return;
        }
        self.selected = wrap_next(self.selected, self.rows().len());
    }

    pub(super) fn select_prev(&mut self) {
        if self.rows().is_empty() {
            return;
        }
        self.selected = wrap_prev(self.selected, self.rows().len());
    }

    /// Rows the layout needs, zero when hidden and capped at [`MAX_VISIBLE_ROWS`] plus chrome.
    pub(super) fn height(&self) -> u16 {
        let visible = self.rows().len().min(MAX_VISIBLE_ROWS);
        let body = saturating_u16(visible);
        if body == 0 {
            0
        } else {
            body.saturating_add(CHROME_ROWS)
        }
    }

    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(list) = &self.list else {
            return;
        };
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(self.theme.border_unfocused())
            .style(self.theme.surface());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 {
            return;
        }
        let width = usize::from(inner.width);
        let offset = self.scroll_offset();
        let visible = list.rows.len().min(MAX_VISIBLE_ROWS);
        let window = &list.rows[offset..offset + visible];
        // Cache labels so width measurement and row rendering can't drift.
        let labels: Vec<String> = window.iter().map(|r| list.mode.label(r)).collect();
        let label_width = labels.iter().map(|l| l.width()).max().unwrap_or(0);
        let lines: Vec<Line<'static>> = window
            .iter()
            .zip(labels)
            .enumerate()
            .map(|(i, (r, label))| {
                self.render_row(r, label, offset + i == self.selected, label_width, width)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines).style(self.theme.surface()), inner);
    }

    /// First visible match index. Centered-cursor scroll: the selected row sits at the visual
    /// middle once it leaves the top half, then anchors at the bottom near the end of the list.
    fn scroll_offset(&self) -> usize {
        let total = self.rows().len();
        if total <= MAX_VISIBLE_ROWS {
            return 0;
        }
        let pad = MAX_VISIBLE_ROWS / 2;
        let max_offset = total - MAX_VISIBLE_ROWS;
        self.selected.saturating_sub(pad).min(max_offset)
    }

    /// The listed rows, empty when hidden.
    fn rows(&self) -> &[PopupRow] {
        self.list.as_ref().map_or(&[], |list| &list.rows)
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
        let row_style = self.theme.row_style(selected);
        let desc_budget = width.saturating_sub(label_width + COLUMN_GAP);
        let desc = truncate_to_width(&row.description, desc_budget);

        Line::from(vec![
            Span::styled(label_text, row_style),
            Span::raw(" ".repeat(pad + COLUMN_GAP)),
            Span::styled(desc, row_style),
        ])
    }
}

impl PopupList {
    /// Lists the rows `state` matches, or returns `None` when it matches none. An arg without a
    /// curated roster matches none, which leaves Tab and Enter to the arg.
    fn new(state: &PopupState<'_>) -> Option<Self> {
        let (mode, rows): (_, Vec<_>) = match state {
            PopupState::Name(query) => (
                PopupMode::Name,
                filter_built_ins(query)
                    .into_iter()
                    .map(|m| PopupRow {
                        value: Cow::Borrowed(m.name),
                        description: Cow::Borrowed(m.description),
                        matched_alias: m.matched_alias,
                    })
                    .collect(),
            ),
            PopupState::Arg { name, prefix } => (
                PopupMode::Arg {
                    name: (*name).to_owned(),
                },
                complete_arg_for(name, prefix)
                    .into_iter()
                    .map(|c| PopupRow {
                        value: c.value,
                        description: c.description,
                        matched_alias: None,
                    })
                    .collect(),
            ),
        };
        (!rows.is_empty()).then_some(Self { mode, rows })
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

    fn mode(popup: &SlashPopup) -> Option<&PopupMode> {
        popup.list.as_ref().map(|list| &list.mode)
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
        let popup = name_popup("");
        assert!(popup.is_visible());
        assert_eq!(mode(&popup), Some(&PopupMode::Name));
        let values: Vec<String> = popup
            .rows()
            .iter()
            .map(|r| r.value.clone().into_owned())
            .collect();
        // BUILT_INS is alphabetical, so empty-query first row is `archive`.
        assert_eq!(values[0], "archive");
    }

    #[test]
    fn set_state_clamps_selection_when_row_count_shrinks() {
        // The selection sits past the surviving row count and must clamp to it.
        let mut popup = name_popup("");
        let n = popup.rows().len();
        for _ in 0..n - 1 {
            popup.select_next();
        }
        assert_eq!(popup.selected, n - 1);

        popup.set_state(Some(&PopupState::Name("help")));
        assert_eq!(popup.rows().len(), 1);
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn set_state_resets_selection_on_mode_transition() {
        // A mode transition resets the selection, since the new roster is unrelated.
        let mut popup = name_popup("");
        popup.select_next();
        popup.select_next();
        assert_eq!(popup.selected, 2);

        popup.set_state(Some(&PopupState::Arg {
            name: "model",
            prefix: "",
        }));
        assert!(matches!(mode(&popup), Some(PopupMode::Arg { .. })));
        assert_eq!(popup.selected, 0, "mode transition resets selection");
    }

    #[test]
    fn set_state_intra_mode_query_change_preserves_selection_via_clamp() {
        let mut popup = name_popup("");
        popup.select_next();
        let parked = popup.selected;
        assert!(parked > 0, "selection past row 0");

        popup.set_state(Some(&PopupState::Name("")));
        assert_eq!(popup.selected, parked, "same mode preserves selection");
    }

    #[test]
    fn set_state_arg_with_curated_roster_populates_arg_mode() {
        let popup = arg_popup("model", "");
        assert!(popup.is_visible());
        assert_eq!(
            mode(&popup),
            Some(&PopupMode::Arg {
                name: "model".to_owned()
            })
        );
        assert_ne!(popup.rows(), []);
    }

    #[test]
    fn set_state_arg_with_empty_roster_stays_hidden() {
        // `/init` has no curated arg roster, so the popup hides, leaving Tab and Enter to the arg.
        let popup = arg_popup("init", "");
        assert!(!popup.is_visible());
    }

    #[test]
    fn set_state_arg_unknown_command_stays_hidden() {
        let popup = arg_popup("nope", "");
        assert!(!popup.is_visible());
    }

    // ── completion ──

    #[test]
    fn completion_in_name_mode_is_the_slash_command() {
        let popup = name_popup("hel");

        assert_eq!(popup.completion().as_deref(), Some("/help"));
    }

    #[test]
    fn completion_in_arg_mode_follows_the_command_with_the_selected_value() {
        let mut popup = arg_popup("effort", "");
        popup.select_next();

        assert_eq!(popup.completion().as_deref(), Some("/effort medium"));
    }

    #[test]
    fn completion_of_a_hidden_popup_is_none() {
        let popup = popup_with_state(None);

        assert_eq!(popup.completion(), None);
    }

    // ── select_next / select_prev ──

    #[test]
    fn select_next_wraps_at_bottom() {
        let mut popup = name_popup("");
        let n = popup.rows().len();
        for _ in 0..n {
            popup.select_next();
        }
        assert_eq!(popup.selected, 0, "wrap from last back to first");
    }

    #[test]
    fn select_prev_wraps_at_top() {
        let mut popup = name_popup("");
        let n = popup.rows().len();
        popup.select_prev();
        assert_eq!(popup.selected, n - 1, "wrap from first up to last");
    }

    #[test]
    fn select_prev_when_not_at_top_moves_up_one_row() {
        // Pins the decrement branch, since select_prev() from row 0 always takes the wrap branch.
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
    fn height_caps_at_max_visible_rows_plus_chrome() {
        assert_eq!(long_popup(3).height(), 3 + CHROME_ROWS);
        assert_eq!(long_popup(12).height(), 8 + CHROME_ROWS);
    }

    // ── render ──

    fn long_popup(n: usize) -> SlashPopup {
        // Hand-rolled list keeps the test independent of registry growth.
        let mut p = SlashPopup::new(&theme());
        p.list = Some(PopupList {
            mode: PopupMode::Name,
            rows: (0..n)
                .map(|i| PopupRow {
                    value: Cow::Owned(format!("cmd{i}")),
                    description: Cow::Borrowed("desc"),
                    matched_alias: None,
                })
                .collect(),
        });
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
        // Arg-mode rows render bare values (`low`, `medium`, ...) with no leading `/`.
        let popup = arg_popup("effort", "");
        insta::assert_snapshot!(render_to_backend(&popup, 60));
    }

    #[test]
    fn render_arg_mode_prefix_filters_to_subset() {
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

        // Data rows start below the top-border chrome row, at y = CHROME_ROWS.
        let chrome = CHROME_ROWS;
        let selected = backend.buffer().cell(Position::new(0, chrome + 1)).unwrap();
        assert_eq!(selected.fg, theme.text().fg.unwrap());
        assert!(
            selected.modifier.contains(Modifier::BOLD),
            "selected row should paint bold: got {:?}",
            selected.modifier,
        );

        let unselected = backend.buffer().cell(Position::new(0, chrome)).unwrap();
        assert_eq!(unselected.fg, theme.dim().fg.unwrap());
        assert!(
            !unselected.modifier.contains(Modifier::BOLD),
            "unselected row must not be bold: got {:?}",
            unselected.modifier,
        );
    }

    #[test]
    fn render_narrow_terminal_truncates_description() {
        // At 30 cols the description shrinks and truncates with an ellipsis to fit the row.
        let popup = name_popup("");
        insta::assert_snapshot!(render_to_backend(&popup, 30));
    }

    #[test]
    fn render_alias_match_parenthesizes_only_typed_alias() {
        let popup = name_popup("new");
        insta::assert_snapshot!(render_to_backend(&popup, 60));
    }

    #[test]
    fn render_with_only_chrome_room_paints_the_border_and_skips_rows() {
        // With exactly CHROME_ROWS reserved, the inner area has zero height, so the border paints
        // and the row loop is skipped.
        let popup = name_popup("");
        let width: u16 = 60;
        let mut terminal = Terminal::new(TestBackend::new(width, CHROME_ROWS)).unwrap();
        terminal
            .draw(|frame| popup.render(frame, Rect::new(0, 0, width, CHROME_ROWS)))
            .unwrap();
        let buf = terminal.backend().buffer();
        for x in 0..width {
            assert_eq!(
                buf.cell(ratatui::layout::Position::new(x, 0))
                    .unwrap()
                    .symbol(),
                "─",
                "chrome row must be a continuous border at col {x}",
            );
        }
    }

    #[test]
    fn render_hidden_popup_emits_nothing() {
        // App's draw gates on height(), so only this test reaches render()'s hidden-popup return.
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
        // Moving to row 8 of 12 scrolls `cmd0` out and `cmd8` in, so the slice and offset agree.
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
    fn scroll_offset_at_exactly_cap_does_not_scroll_last_row() {
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
        // Up from row 0 wraps to the last row, and the bottom-anchored window clamps to
        // `len - MAX_VISIBLE_ROWS`, mirroring the wrap-to-top test above.
        let total = MAX_VISIBLE_ROWS + 4;
        let mut p = long_popup(total);
        p.select_prev();
        assert_eq!(p.selected, total - 1, "wrap to last row");
        assert_eq!(p.scroll_offset(), total - MAX_VISIBLE_ROWS);
    }
}
