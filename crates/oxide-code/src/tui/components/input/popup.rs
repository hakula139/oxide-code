//! Slash-command autocomplete popup.
//!
//! Rendered as a band of rows above the [`InputArea`](super::InputArea)
//! whenever the textarea buffer reads as a slash query (see
//! [`SlashPopup::set_query`]). Pure render + selection state — the
//! filtering happens in [`crate::slash::filter_built_ins`] and the
//! component owns no registry handle.
//!
//! Style follows Claude Code's popup: the selected row paints in the
//! normal `text` palette while the rest of the rows render dim, so
//! the active suggestion stands out by contrast rather than by a
//! prefix glyph or background fill. Aliases parenthesize only the
//! alias the user typed (`/clear (new)`); the popup never paints a
//! full alias list.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::slash::{MatchedCommand, filter_built_ins};
use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

/// Maximum visible rows. Beyond this the popup truncates with a
/// `… (N more)` footer so the input layout doesn't lose half the
/// terminal to the overlay.
const MAX_VISIBLE_ROWS: usize = 8;

/// Padding columns between the name column and the description.
const COLUMN_GAP: usize = 2;

/// Slash-command autocomplete overlay.
///
/// `matches.is_empty()` is the visibility predicate — callers set the
/// query via [`Self::set_query`], and any state where the query is
/// `None` or yields no matches collapses the popup to zero rows.
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

    /// Update the popup's match list from a typed query. `query` is
    /// the buffer with the leading `/` stripped; `None` hides the
    /// popup outright (e.g., the buffer no longer starts with `/`).
    pub(crate) fn set_query(&mut self, query: Option<&str>) {
        let Some(q) = query else {
            self.matches.clear();
            self.selected = 0;
            return;
        };
        self.matches = filter_built_ins(q);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    /// Whether the popup currently has any matches to draw.
    pub(crate) fn is_visible(&self) -> bool {
        !self.matches.is_empty()
    }

    /// Currently-selected match. `None` when the popup is hidden.
    pub(crate) fn selected(&self) -> Option<&MatchedCommand> {
        self.matches.get(self.selected)
    }

    /// Move the selection to the next row, wrapping at the bottom.
    /// Wrapping fits the small surface — Up-from-top reaches the
    /// last row in one keystroke.
    pub(crate) fn select_next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.matches.len();
    }

    /// Move the selection to the previous row, wrapping at the top.
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

    /// Number of rows the popup needs in the surrounding layout. Zero
    /// when hidden so the input keeps full height.
    pub(crate) fn height(&self) -> u16 {
        if self.matches.is_empty() {
            return 0;
        }
        let visible = self.matches.len().min(MAX_VISIBLE_ROWS);
        let footer = usize::from(self.matches.len() > MAX_VISIBLE_ROWS);
        u16::try_from(visible + footer).unwrap_or(u16::MAX)
    }

    pub(crate) fn render(&self, frame: &mut Frame, area: Rect) {
        if self.matches.is_empty() {
            return;
        }
        let width = usize::from(area.width);
        let label_width = self
            .matches
            .iter()
            .take(MAX_VISIBLE_ROWS)
            .map(|m| label(m).width())
            .max()
            .unwrap_or(0);
        let lines: Vec<Line<'static>> = self
            .matches
            .iter()
            .take(MAX_VISIBLE_ROWS)
            .enumerate()
            .map(|(i, m)| self.render_row(m, i == self.selected, label_width, width))
            .chain(self.render_footer(width))
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
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

    /// `… (N more)` footer when matches exceed `MAX_VISIBLE_ROWS`.
    /// Returned as an iterator so the caller can chain it onto the
    /// row list without an `if`-shaped branch.
    fn render_footer(&self, width: usize) -> impl Iterator<Item = Line<'static>> {
        let hidden = self.matches.len().saturating_sub(MAX_VISIBLE_ROWS);
        let style = self.theme.dim();
        (hidden > 0)
            .then(move || {
                let raw = format!("  … ({hidden} more)");
                let text = truncate_to_width(&raw, width);
                Line::from(Span::styled(text, style))
            })
            .into_iter()
    }
}

/// Display label for a matched row: `/name` plus the typed alias when
/// the match landed on an alias (`/clear (new)`). Used for both
/// rendering and column-width computation, so it must match exactly.
fn label(m: &MatchedCommand) -> String {
    match m.matched_alias {
        Some(alias) => format!("/{} ({alias})", m.name),
        None => format!("/{}", m.name),
    }
}

/// Row palette: selected rows paint in the normal `text` slot with a
/// `BOLD` modifier so they stand out against the surrounding `dim`
/// non-selected rows. Mirrors Claude Code's selected-row contrast.
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
        // Pin against the live registry — if BUILT_INS reorders, this
        // test moves with it (which is the right invariant).
        assert!(!names.is_empty());
        assert_eq!(names[0], "help");
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
    fn select_next_on_empty_popup_is_a_noop() {
        let mut popup = popup_with_query(None);
        popup.select_next();
        popup.select_prev();
        assert_eq!(popup.selected, 0);
    }

    // ── height ──

    #[test]
    fn height_matches_match_count_below_cap() {
        // BUILT_INS today has 4 commands, so empty query is below the
        // cap. The popup occupies exactly that many rows.
        let popup = popup_with_query(Some(""));
        assert_eq!(usize::from(popup.height()), popup.matches.len());
    }

    // ── selected ──

    #[test]
    fn selected_returns_match_at_index() {
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
        // is the place where the alias-display rule lives. Drive it
        // via a hand-rolled match list.
        let mut popup = SlashPopup::new(&theme());
        popup.matches = vec![MatchedCommand {
            name: "clear",
            description: "wipe transcript",
            matched_alias: Some("new"),
        }];
        insta::assert_snapshot!(render_to_backend(&popup, 60));
    }
}
