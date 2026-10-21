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

use ox_core::command::is_name_char;
use ox_core::util::text::truncate_to_width;

use crate::saturating_u16;
use crate::slash::{complete_arg_for, filter_built_ins};
use crate::theme::Theme;
use crate::viewport::{wrap_next, wrap_prev};

const MAX_VISIBLE_ROWS: usize = 8;

const COLUMN_GAP: usize = 2;

/// Rows of chrome above the row list. One dim horizontal rule frames the popup as a separate
/// overlay so it doesn't bleed into whatever sits above (welcome, chat, preview).
const CHROME_ROWS: u16 = 1;

/// In-progress popup context: `Name` while the user types the command name, and `Arg` once a
/// space follows it. `None` for plain prompts and the `//` escape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PopupState<'a> {
    Name(&'a str),
    Arg { name: &'a str, prefix: &'a str },
}

/// What the rows complete: a command name, or an argument of the named command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PopupMode {
    Name,
    /// Owned because the name comes from the live input buffer.
    Arg {
        name: String,
    },
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
}

/// A visible popup's list. `rows` is never empty, and `selected` indexes into it.
struct PopupList {
    mode: PopupMode,
    rows: Vec<PopupRow>,
    selected: usize,
}

impl SlashPopup {
    pub(super) fn new(theme: &Theme) -> Self {
        Self {
            theme: theme.clone(),
            list: None,
        }
    }

    /// Re-skins every subsequent render.
    pub(super) fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
    }

    pub(super) fn set_state(&mut self, state: Option<&PopupState<'_>>) {
        let mut list = state.and_then(PopupList::new);
        // Mode transitions, including a switch to another command's args, reset to row 0 since
        // the rosters are unrelated. Query or prefix changes clamp instead, so the cursor sticks.
        if let (Some(old), Some(new)) = (&self.list, &mut list)
            && old.mode == new.mode
        {
            new.selected = old.selected.min(new.rows.len() - 1);
        }
        self.list = list;
    }

    pub(super) fn is_visible(&self) -> bool {
        self.list.is_some()
    }

    /// Returns the command line the selected row completes to: `/{name}` in name mode, or
    /// `/{cmd} {value}` in arg mode.
    pub(super) fn completion(&self) -> Option<String> {
        let list = self.list.as_ref()?;
        let value = &list.rows[list.selected].value;
        Some(match &list.mode {
            PopupMode::Name => format!("/{value}"),
            PopupMode::Arg { name } => format!("/{name} {value}"),
        })
    }

    pub(super) fn select_next(&mut self) {
        if let Some(list) = &mut self.list {
            list.selected = wrap_next(list.selected, list.rows.len());
        }
    }

    pub(super) fn select_prev(&mut self) {
        if let Some(list) = &mut self.list {
            list.selected = wrap_prev(list.selected, list.rows.len());
        }
    }

    /// Rows the layout needs, zero when hidden and capped at [`MAX_VISIBLE_ROWS`] plus chrome.
    pub(super) fn height(&self) -> u16 {
        self.list.as_ref().map_or(0, |list| {
            saturating_u16(list.rows.len().min(MAX_VISIBLE_ROWS)).saturating_add(CHROME_ROWS)
        })
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
        let offset = list.scroll_offset();
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
                self.render_row(r, label, offset + i == list.selected, label_width, width)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines).style(self.theme.surface()), inner);
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
        (!rows.is_empty()).then_some(Self {
            mode,
            rows,
            selected: 0,
        })
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
}

/// Classifies a single-line input as a command name being typed or an argument of the named
/// command, by the name rules of [`parse_slash`](ox_core::command::parse_slash). An empty `prefix`
/// in `Arg` means the user typed `/cmd ` and has not started the argument yet.
pub(super) fn popup_state(buffer: &str) -> Option<PopupState<'_>> {
    let rest = buffer.trim_start().strip_prefix('/')?;
    if rest.starts_with('/') {
        return None;
    }
    let Some((name, after)) = rest.split_once(char::is_whitespace) else {
        return rest
            .chars()
            .all(is_name_char)
            .then_some(PopupState::Name(rest));
    };
    if name.is_empty() || !name.chars().all(is_name_char) {
        return None;
    }
    Some(PopupState::Arg {
        name,
        prefix: after.trim_start(),
    })
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;

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

    fn list(popup: &SlashPopup) -> &PopupList {
        popup.list.as_ref().expect("popup visible")
    }

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
            selected: 0,
        });
        p
    }

    #[test]
    fn select_next_and_prev_leave_a_hidden_popup_hidden() {
        let mut popup = popup_with_state(None);

        popup.select_next();
        popup.select_prev();

        assert!(!popup.is_visible());
    }

    mod set_state {

        use super::*;

        #[test]
        fn hides_the_popup_without_a_slash_command() {
            let popup = popup_with_state(None);
            assert!(!popup.is_visible());
            assert_eq!(popup.height(), 0);
        }

        #[test]
        fn name_empty_query_lists_full_registry_in_presentation_order() {
            let popup = name_popup("");

            let list = list(&popup);
            let values: Vec<&str> = list.rows.iter().map(|r| r.value.as_ref()).collect();
            assert_eq!(list.mode, PopupMode::Name);
            assert_eq!(
                values,
                [
                    "archive",
                    "clear",
                    "compact",
                    "config",
                    "delete",
                    "diff",
                    "effort",
                    "help",
                    "init",
                    "login",
                    "logout",
                    "mode",
                    "model",
                    "rename",
                    "resume",
                    "status",
                    "theme",
                    "unarchive",
                ]
            );
        }

        #[test]
        fn arg_with_curated_roster_populates_arg_mode() {
            let popup = arg_popup("model", "");

            assert_eq!(
                list(&popup).mode,
                PopupMode::Arg {
                    name: "model".to_owned()
                }
            );
        }

        #[test]
        fn arg_with_empty_roster_stays_hidden() {
            // `/compact` has no roster, so the popup hides, leaving Tab and Enter to the arg.
            let popup = arg_popup("compact", "");
            assert!(!popup.is_visible());
        }

        #[test]
        fn arg_unknown_command_stays_hidden() {
            let popup = arg_popup("nope", "");
            assert!(!popup.is_visible());
        }

        #[test]
        fn clamps_selection_when_row_count_shrinks() {
            let mut popup = name_popup("");
            let n = list(&popup).rows.len();
            for _ in 0..n - 1 {
                popup.select_next();
            }
            assert_eq!(list(&popup).selected, n - 1);

            popup.set_state(Some(&PopupState::Name("help")));

            assert_eq!(list(&popup).rows.len(), 1);
            assert_eq!(list(&popup).selected, 0);
        }

        #[test]
        fn resets_selection_on_mode_transition() {
            let mut popup = name_popup("");
            popup.select_next();
            popup.select_next();
            assert_eq!(list(&popup).selected, 2);

            popup.set_state(Some(&PopupState::Arg {
                name: "model",
                prefix: "",
            }));

            assert!(matches!(list(&popup).mode, PopupMode::Arg { .. }));
            assert_eq!(list(&popup).selected, 0, "mode transition resets selection");
        }

        #[test]
        fn resets_selection_on_switch_to_another_commands_args() {
            let mut popup = arg_popup("model", "");
            popup.select_next();
            popup.select_next();
            assert_eq!(list(&popup).selected, 2);

            popup.set_state(Some(&PopupState::Arg {
                name: "effort",
                prefix: "",
            }));

            assert_eq!(popup.completion().as_deref(), Some("/effort low"));
        }

        #[test]
        fn intra_mode_query_change_preserves_selection_via_clamp() {
            let mut popup = name_popup("");
            popup.select_next();
            let parked = list(&popup).selected;
            assert!(parked > 0, "selection past row 0");

            popup.set_state(Some(&PopupState::Name("")));

            assert_eq!(
                list(&popup).selected,
                parked,
                "same mode preserves selection"
            );
        }
    }

    mod completion {
        use super::*;

        #[test]
        fn in_name_mode_is_the_slash_command() {
            let popup = name_popup("hel");

            assert_eq!(popup.completion().as_deref(), Some("/help"));
        }

        #[test]
        fn in_arg_mode_follows_the_command_with_the_selected_value() {
            let mut popup = arg_popup("effort", "");
            popup.select_next();

            assert_eq!(popup.completion().as_deref(), Some("/effort medium"));
        }

        #[test]
        fn of_a_hidden_popup_completes_nothing() {
            let popup = popup_with_state(None);

            assert_eq!(popup.completion(), None);
        }
    }

    mod select_next {
        use super::*;

        #[test]
        fn wraps_at_bottom() {
            let mut popup = name_popup("");
            let n = list(&popup).rows.len();

            for _ in 0..n {
                popup.select_next();
            }

            assert_eq!(list(&popup).selected, 0, "wrap from last back to first");
        }
    }

    mod select_prev {
        use super::*;

        #[test]
        fn wraps_at_top() {
            let mut popup = name_popup("");
            let n = list(&popup).rows.len();

            popup.select_prev();

            assert_eq!(list(&popup).selected, n - 1, "wrap from first up to last");
        }

        #[test]
        fn when_not_at_top_moves_up_one_row() {
            // Pins the decrement branch, since select_prev() from row 0 takes the wrap branch.
            let mut popup = name_popup("");
            popup.select_next();
            popup.select_next();
            assert_eq!(list(&popup).selected, 2);

            popup.select_prev();

            assert_eq!(list(&popup).selected, 1);
        }
    }

    mod height {
        use super::*;

        #[test]
        fn caps_at_max_visible_rows_plus_chrome() {
            assert_eq!(long_popup(3).height(), 3 + CHROME_ROWS);
            assert_eq!(long_popup(12).height(), 8 + CHROME_ROWS);
        }
    }

    mod render {
        use super::*;

        fn render_to_backend(popup: &SlashPopup, width: u16) -> TestBackend {
            let height = popup.height().max(1);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| popup.render(frame, frame.area()))
                .unwrap();
            terminal.backend().clone()
        }

        #[test]
        fn empty_name_query_shows_each_command_once() {
            let popup = name_popup("");
            insta::assert_snapshot!(render_to_backend(&popup, 60));
        }

        #[test]
        fn filtered_name_query_shows_only_matching_rows() {
            let popup = name_popup("hel");
            insta::assert_snapshot!(render_to_backend(&popup, 60));
        }

        #[test]
        fn arg_mode_lists_curated_roster_without_slash_prefix() {
            let popup = arg_popup("effort", "");
            insta::assert_snapshot!(render_to_backend(&popup, 60));
        }

        #[test]
        fn arg_mode_prefix_filters_to_subset() {
            let popup = arg_popup("theme", "m");
            insta::assert_snapshot!(render_to_backend(&popup, 60));
        }

        #[test]
        fn selected_row_paints_bold_text_others_dim() {
            // TestBackend snapshots don't capture style, so layout-only snapshots can't tell
            // selected from unselected. Pin the bold-vs-dim contrast on the rendered cells.
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
        fn narrow_terminal_truncates_description() {
            let popup = name_popup("");
            insta::assert_snapshot!(render_to_backend(&popup, 30));
        }

        #[test]
        fn alias_match_parenthesizes_only_typed_alias() {
            let popup = name_popup("new");
            insta::assert_snapshot!(render_to_backend(&popup, 60));
        }

        #[test]
        fn with_only_chrome_room_paints_the_border_and_skips_rows() {
            // With exactly CHROME_ROWS reserved, the inner area has zero height, so the border
            // paints and the row loop is skipped.
            let popup = name_popup("");
            let width: u16 = 60;
            let mut terminal = Terminal::new(TestBackend::new(width, CHROME_ROWS)).unwrap();
            terminal
                .draw(|frame| popup.render(frame, Rect::new(0, 0, width, CHROME_ROWS)))
                .unwrap();
            let buf = terminal.backend().buffer();
            for x in 0..width {
                assert_eq!(
                    buf.cell(Position::new(x, 0)).unwrap().symbol(),
                    "─",
                    "chrome row must be a continuous border at col {x}",
                );
            }
        }

        #[test]
        fn hidden_popup_emits_nothing() {
            // App's draw gates on height(), so only this test hits render()'s hidden-popup return.
            let popup = popup_with_state(None);
            let backend = render_to_backend(&popup, 60);
            let buf = backend.buffer();
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    let cell = buf.cell(Position::new(x, y)).unwrap();
                    assert_eq!(cell.symbol(), " ", "hidden popup must paint nothing");
                }
            }
        }

        #[test]
        fn after_scroll_shows_window_starting_at_offset() {
            // Moving to row 8 of 12 scrolls `cmd0` out and `cmd8` in, so slice and offset agree.
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

    mod popup_list {
        use super::*;

        mod scroll_offset {
            use super::*;

            #[test]
            fn is_zero_when_total_fits_window() {
                let mut p = long_popup(5);
                p.select_next();
                p.select_next();
                assert_eq!(list(&p).scroll_offset(), 0);
            }

            #[test]
            fn at_exactly_cap_does_not_scroll_last_row() {
                // Boundary: total == MAX_VISIBLE_ROWS hits the `<=` early-return. Tightening the
                // boundary pins the invariant for the only case where the edge matters.
                let mut p = long_popup(MAX_VISIBLE_ROWS);
                while list(&p).selected < MAX_VISIBLE_ROWS - 1 {
                    p.select_next();
                }
                assert_eq!(list(&p).scroll_offset(), 0);
            }

            #[test]
            fn anchors_at_top_while_cursor_in_first_half() {
                let mut p = long_popup(MAX_VISIBLE_ROWS + 4);
                for _ in 0..MAX_VISIBLE_ROWS / 2 {
                    assert_eq!(list(&p).scroll_offset(), 0);
                    p.select_next();
                }
            }

            #[test]
            fn centers_cursor_past_first_half() {
                // pad = MAX_VISIBLE_ROWS / 2 = 4 for the default cap. At selected = pad + k the
                // offset is k, keeping the cursor visually at row `pad`.
                let mut p = long_popup(MAX_VISIBLE_ROWS + 4);
                for _ in 0..=(MAX_VISIBLE_ROWS / 2) {
                    p.select_next();
                }
                assert_eq!(
                    list(&p).scroll_offset(),
                    1,
                    "cursor at pad + 1 → offset = 1"
                );
            }

            #[test]
            fn keeps_visible_row_at_pad_for_mid_list_selection() {
                // Pin the centering invariant: the visible row of the cursor (selected - offset)
                // equals `pad` whenever the selection is past the top half but not yet near the
                // bottom. Mutating the divisor (e.g. `/3`) or the formula would shift this row.
                let total = MAX_VISIBLE_ROWS + 4;
                let mut p = long_popup(total);
                let pad = MAX_VISIBLE_ROWS / 2;
                for _ in 0..=(pad + 1) {
                    p.select_next();
                }
                let visible_row = list(&p).selected - list(&p).scroll_offset();
                assert_eq!(visible_row, pad, "cursor must sit at the visual middle row");
            }

            #[test]
            fn anchors_at_bottom_near_end() {
                // Once selected nears the end, the offset clamps to `len - MAX_VISIBLE_ROWS` so the
                // last row stays visible while the cursor advances in the bottom-anchored window.
                let total = MAX_VISIBLE_ROWS + 4;
                let mut p = long_popup(total);
                while list(&p).selected < total - 1 {
                    p.select_next();
                }
                assert_eq!(list(&p).scroll_offset(), total - MAX_VISIBLE_ROWS);
            }

            #[test]
            fn resets_on_wrap_to_first_row() {
                let total = MAX_VISIBLE_ROWS + 4;
                let mut p = long_popup(total);
                for _ in 0..total {
                    p.select_next();
                }
                assert_eq!(list(&p).selected, 0, "wrap to row 0");
                assert_eq!(
                    list(&p).scroll_offset(),
                    0,
                    "wrap snaps the window back to top"
                );
            }

            #[test]
            fn select_prev_from_top_anchors_at_bottom_window() {
                // Up from row 0 wraps to the last row, and the bottom-anchored window clamps to
                // `len - MAX_VISIBLE_ROWS`, mirroring the wrap-to-top test above.
                let total = MAX_VISIBLE_ROWS + 4;
                let mut p = long_popup(total);
                p.select_prev();
                assert_eq!(list(&p).selected, total - 1, "wrap to last row");
                assert_eq!(list(&p).scroll_offset(), total - MAX_VISIBLE_ROWS);
            }
        }
    }

    mod popup_state {
        use super::*;

        fn name(s: &str) -> PopupState<'_> {
            PopupState::Name(s)
        }

        fn arg<'a>(name: &'a str, prefix: &'a str) -> PopupState<'a> {
            PopupState::Arg { name, prefix }
        }

        #[test]
        fn bare_slash_is_empty_name() {
            assert_eq!(popup_state("/"), Some(name("")));
        }

        #[test]
        fn partial_name_carries_typed_chars() {
            assert_eq!(popup_state("/cl"), Some(name("cl")));
            assert_eq!(popup_state("/clear"), Some(name("clear")));
        }

        #[test]
        fn tolerates_leading_whitespace_before_slash() {
            assert_eq!(popup_state("   /he"), Some(name("he")));
        }

        #[test]
        fn trailing_space_after_name_switches_to_empty_arg() {
            assert_eq!(popup_state("/clear "), Some(arg("clear", "")));
        }

        #[test]
        fn typed_arg_carries_prefix_trimmed_of_leading_whitespace() {
            assert_eq!(popup_state("/model claude-"), Some(arg("model", "claude-")));
            assert_eq!(popup_state("/model    opus"), Some(arg("model", "opus")));
        }

        #[test]
        fn inner_whitespace_in_args_is_kept_for_free_form_commands() {
            assert_eq!(
                popup_state("/init please write CLAUDE.md"),
                Some(arg("init", "please write CLAUDE.md")),
            );
        }

        #[test]
        fn double_slash_escape_is_not_a_command() {
            assert!(popup_state("//etc/hosts").is_none());
            assert!(popup_state("//").is_none());
        }

        #[test]
        fn plain_prompts_are_not_a_command() {
            assert!(popup_state("hello").is_none());
            assert!(popup_state("explain /etc/hosts").is_none());
            assert!(popup_state("").is_none());
        }

        #[test]
        fn invalid_name_chars_reject_the_buffer() {
            assert!(popup_state("/foo🦀").is_none());
            assert!(popup_state("/foo!").is_none());
            assert!(popup_state("/foo! arg").is_none());
        }

        #[test]
        fn empty_name_rejects_buffer() {
            assert!(popup_state("/ ").is_none());
            assert!(popup_state("/    ").is_none());
            assert!(popup_state("/  arg").is_none());
        }
    }
}
