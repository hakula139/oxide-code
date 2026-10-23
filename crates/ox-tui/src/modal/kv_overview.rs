//! Read-only key-value overview modal with a title, multi-section body, and fixed footer.
//! `/status`, `/config`, and `/help` supply the sections, and the modal lays them out.

use std::cell::Cell;
use std::iter;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr as _;

use ox_core::util::text::{center_truncate_to_width, pad_to_width};

use super::chrome::{COLUMN_GAP, FOOTER_HEIGHT, Header, KEY_UP_DOWN, render_footer};
use super::{Modal, ModalKey};
use crate::saturating_u16;
use crate::theme::Theme;
use crate::viewport::Viewport;

// ── Constants ──

const CLOSE_HINT: &str = "Esc to close";

// ── KvSection ──

pub(crate) struct KvSection {
    heading: Option<String>,
    rows: Vec<KvRow>,
}

struct KvRow {
    label: String,
    value: String,
    note: Option<String>,
}

impl KvRow {
    /// The row's value lines and the note that fits beside the first, for a label column of
    /// `label_width` within `width`. A note shows whole, or not at all once the value would keep
    /// less than half its room.
    fn layout(&self, label_width: usize, width: usize) -> (Vec<String>, Option<&str>) {
        let room = width.saturating_sub(label_width + COLUMN_GAP);
        let note = self.note.as_deref().filter(|note| {
            let claim = COLUMN_GAP + note.width();
            self.value.width() + claim <= room || claim <= room / 2
        });
        let value_room = note.map_or(room, |note| room - COLUMN_GAP - note.width());
        (wrap_value(&self.value, value_room), note)
    }

    /// The row's lines with its label padded to `label_width` and any note aligned after
    /// `value_width`, fitted to `width`.
    fn lines(
        &self,
        label_width: usize,
        value_width: usize,
        width: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let (values, note) = self.layout(label_width, width);
        let mut values = values.into_iter();
        let first = values.next().unwrap_or_default();
        let pad = value_width.saturating_sub(first.width());
        let mut spans = vec![
            Span::styled(pad_to_width(&self.label, label_width), theme.dim()),
            Span::styled(" ".repeat(COLUMN_GAP), theme.surface()),
            Span::styled(first, theme.text()),
        ];
        if let Some(note) = note {
            spans.push(Span::styled(" ".repeat(pad + COLUMN_GAP), theme.surface()));
            spans.push(Span::styled(note.to_owned(), theme.dim()));
        }
        let indent = " ".repeat(label_width + COLUMN_GAP);
        iter::once(Line::from(spans))
            .chain(values.map(|value| {
                Line::from(vec![
                    Span::styled(indent.clone(), theme.surface()),
                    Span::styled(value, theme.text()),
                ])
            }))
            .collect()
    }
}

/// `value` in lines of at most `room` columns: whole when it fits, or else wrapped after its `, `
/// separators, with any part still too wide truncated in its middle, keeping the ends that tell
/// paths apart.
fn wrap_value(value: &str, room: usize) -> Vec<String> {
    if value.width() <= room {
        return vec![value.to_owned()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut items = value.split(", ").peekable();
    while let Some(item) = items.next() {
        let separator = if items.peek().is_some() { "," } else { "" };
        if !line.is_empty() && line.width() + 1 + item.width() + separator.len() > room {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(item);
        line.push_str(separator);
    }
    lines.push(line);
    lines
        .iter()
        .map(|line| center_truncate_to_width(line, room))
        .collect()
}

/// One row of the overview body, which an entry may wrap onto several lines.
enum BodyRow<'a> {
    Blank,
    Heading(&'a str),
    Entry { section: usize, row: &'a KvRow },
}

impl KvSection {
    pub(crate) fn new(rows: Vec<(String, String)>) -> Self {
        Self::from_rows(rows.into_iter().map(|(label, value)| KvRow {
            label,
            value,
            note: None,
        }))
    }

    /// Rows with a dim third column, such as where each value came from.
    pub(crate) fn annotated(rows: Vec<(String, String, String)>) -> Self {
        Self::from_rows(rows.into_iter().map(|(label, value, note)| KvRow {
            label,
            value,
            note: Some(note),
        }))
    }

    fn from_rows(rows: impl IntoIterator<Item = KvRow>) -> Self {
        Self {
            heading: None,
            rows: rows.into_iter().collect(),
        }
    }

    pub(crate) fn with_heading(mut self, heading: impl Into<String>) -> Self {
        self.heading = Some(heading.into());
        self
    }
}

// ── KvOverview ──

/// The body scrolls between the pinned header and footer when the granted height is short.
pub(crate) struct KvOverview {
    header: Header,
    sections: Vec<KvSection>,
    viewport: Viewport,
    /// The width of the last render, which sets how many lines the wrapped rows take.
    width: Cell<usize>,
}

impl KvOverview {
    pub(crate) fn new(title: impl Into<String>, sections: Vec<KvSection>) -> Self {
        Self {
            header: Header::new(title),
            sections,
            viewport: Viewport::default(),
            width: Cell::new(0),
        }
    }

    /// Label gutter spans every section so columns stay aligned across headings.
    fn label_width(&self) -> usize {
        self.sections
            .iter()
            .flat_map(|s| s.rows.iter())
            .map(|row| row.label.width())
            .max()
            .unwrap_or(0)
    }

    /// Every body row in order: sections apart by a blank line, each heading followed by one.
    fn body_rows(&self) -> impl Iterator<Item = BodyRow<'_>> {
        self.sections.iter().enumerate().flat_map(|(i, section)| {
            let gap = (i > 0).then_some(BodyRow::Blank);
            let heading = section
                .heading
                .iter()
                .flat_map(|heading| [BodyRow::Heading(heading), BodyRow::Blank]);
            let entries = section
                .rows
                .iter()
                .map(move |row| BodyRow::Entry { section: i, row });
            gap.into_iter().chain(heading).chain(entries)
        })
    }

    /// How many lines the body takes at `width`.
    fn body_len(&self, width: usize) -> usize {
        let label_width = self.label_width();
        self.body_rows()
            .map(|row| match row {
                BodyRow::Blank | BodyRow::Heading(_) => 1,
                BodyRow::Entry { row, .. } => row.layout(label_width, width).0.len(),
            })
            .sum()
    }

    fn body_lines(&self, theme: &Theme, width: usize) -> Vec<Line<'static>> {
        let label_width = self.label_width();
        let value_widths: Vec<usize> = self
            .sections
            .iter()
            .map(|section| note_column(section, label_width, width))
            .collect();
        self.body_rows()
            .flat_map(|row| match row {
                BodyRow::Blank => vec![Line::default()],
                BodyRow::Heading(heading) => vec![Line::from(Span::styled(
                    heading.to_owned(),
                    theme.text().add_modifier(Modifier::BOLD),
                ))],
                BodyRow::Entry { section, row } => {
                    row.lines(label_width, value_widths[section], width, theme)
                }
            })
            .collect()
    }

    fn scroll(&self, delta: isize) {
        self.viewport
            .scroll_by(delta, self.body_len(self.width.get()));
    }

    fn page(&self) -> isize {
        isize::try_from(self.viewport.page()).unwrap_or(isize::MAX)
    }
}

/// Width of the value column that aligns a section's notes: the widest value that leaves room for
/// the widest note within `width`.
/// A wider value puts its note right after it.
fn note_column(section: &KvSection, label_width: usize, width: usize) -> usize {
    let annotated: Vec<(usize, usize)> = section
        .rows
        .iter()
        .filter_map(|row| Some((row.value.width(), row.note.as_ref()?.width())))
        .collect();
    let note_width = annotated.iter().map(|&(_, note)| note).max().unwrap_or(0);
    let room = width.saturating_sub(label_width + COLUMN_GAP + COLUMN_GAP + note_width);
    annotated
        .iter()
        .map(|&(value, _)| value)
        .filter(|&value| value <= room)
        .max()
        .unwrap_or(0)
}

impl Modal for KvOverview {
    fn height(&self, width: u16, max_height: u16) -> u16 {
        let body = saturating_u16(self.body_len(usize::from(width)));
        (self.header.height() + FOOTER_HEIGHT)
            .saturating_add(body)
            .min(max_height)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let rows = area
            .height
            .saturating_sub(self.header.height() + FOOTER_HEIGHT);
        self.width.set(usize::from(area.width));
        let body = self.body_lines(theme, usize::from(area.width));
        let window = self.viewport.window(body.len(), usize::from(rows), None);
        let scrollable = window.len() < body.len();

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(usize::from(area.height));
        self.header.push_lines(&mut lines, area.width, theme);
        lines.extend(body.into_iter().skip(window.start).take(window.len()));
        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);

        let scroll_hint = format!("{KEY_UP_DOWN} to scroll");
        let hints: &[&str] = if scrollable {
            &[&scroll_hint, CLOSE_HINT]
        } else {
            &[CLOSE_HINT]
        };
        render_footer(frame, area, hints, theme);
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        // Esc / Ctrl+C cancel universally via the modal stack, so only scrolling lands here.
        match event.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll(1),
            KeyCode::PageUp => self.scroll(-self.page()),
            KeyCode::PageDown => self.scroll(self.page()),
            KeyCode::Home => self.scroll(isize::MIN),
            KeyCode::End => self.scroll(isize::MAX),
            _ => {}
        }
        ModalKey::Consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modal::testing::render_rows;

    fn trimmed_rows(modal: &KvOverview, width: u16, height: u16) -> Vec<String> {
        render_rows(modal, width, height)
            .into_iter()
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    fn row(k: &str, v: &str) -> (String, String) {
        (k.to_owned(), v.to_owned())
    }

    fn flat_overview() -> KvOverview {
        KvOverview::new(
            "Status",
            vec![KvSection::new(vec![
                row("Model", "Claude Opus"),
                row("Effort", "high"),
            ])],
        )
    }

    fn sectioned_overview() -> KvOverview {
        KvOverview::new(
            "Config",
            vec![
                KvSection::new(vec![row("Model", "Claude Opus")]).with_heading("Resolved"),
                KvSection::new(vec![row("User", "~/.config/ox/config.toml")])
                    .with_heading("Source Files"),
            ],
        )
    }

    fn long_overview() -> KvOverview {
        let rows = (0..10)
            .map(|i| row(&format!("k{i}"), &format!("v{i}")))
            .collect();
        KvOverview::new("Long", vec![KvSection::new(rows)])
    }

    mod label_width {
        use super::*;

        #[test]
        fn spans_every_section() {
            let m = KvOverview::new(
                "T",
                vec![
                    KvSection::new(vec![row("a", "1")]),
                    KvSection::new(vec![row("longer", "2")]),
                ],
            );

            assert_eq!(m.label_width(), "longer".len());
        }

        #[test]
        fn empty_sections_is_zero() {
            let m = KvOverview::new("T", vec![]);

            assert_eq!(m.label_width(), 0);
        }
    }

    mod body_lines {
        use super::*;

        #[test]
        fn aligns_cjk_labels_values_and_notes_by_display_width_and_drops_a_note_that_does_not_fit()
        {
            let theme = Theme::default();
            let m = KvOverview::new(
                "Config",
                vec![KvSection::annotated(vec![
                    ("中文".to_owned(), "值".to_owned(), "说明".to_owned()),
                    ("a".to_owned(), "abc".to_owned(), "note".to_owned()),
                ])],
            );
            let text = |width| -> Vec<String> {
                m.body_lines(&theme, width)
                    .iter()
                    .map(ToString::to_string)
                    .collect()
            };

            assert_eq!(text(15), ["中文  值   说明", "a     abc  note"]);
            assert_eq!(text(14), ["中文  值  说明", "a     abc"]);
        }

        #[test]
        fn truncates_a_wide_value_in_its_middle_and_drops_a_note_without_room() {
            let theme = Theme::default();
            let m = KvOverview::new(
                "Config",
                vec![KvSection::annotated(vec![(
                    "CA".to_owned(),
                    "/etc/ssl/certs/corporate/ca-chain.pem".to_owned(),
                    "user".to_owned(),
                )])],
            );

            let text = |width| m.body_lines(&theme, width)[0].to_string();

            assert_eq!(text(30), "CA  /etc/ssl...chain.pem  user");
            assert_eq!(text(12), "CA  /e...pem");
        }
    }

    mod wrap_value {
        use super::*;

        #[test]
        fn keeps_a_value_that_fits_whole() {
            assert_eq!(wrap_value("model, run-state", 16), ["model, run-state"]);
        }

        #[test]
        fn wraps_after_the_commas_and_truncates_an_item_still_too_wide() {
            assert_eq!(
                wrap_value("model, run-state, model-with-effort, cost", 18),
                ["model, run-state,", "model-with-effort,", "cost"]
            );
            assert_eq!(
                wrap_value("a, /etc/ssl/certs/corporate/ca-chain.pem", 12),
                ["a,", "/etc...n.pem"]
            );
        }
    }

    mod height {
        use super::*;

        #[test]
        fn counts_the_lines_a_wrapped_value_takes() {
            let m = KvOverview::new(
                "Config",
                vec![KvSection::new(vec![row(
                    "Status",
                    "model, run-state, cost",
                )])],
            );

            assert_eq!((m.height(30, 40), m.height(20, 40)), (5, 7));
        }

        #[test]
        fn for_single_section_without_heading_counts_title_blank_rows_blank_footer() {
            assert_eq!(flat_overview().height(80, 40), 6);
        }

        #[test]
        fn for_two_headed_sections_adds_heading_blanks_and_inter_section_blank() {
            assert_eq!(sectioned_overview().height(80, 40), 11);
        }

        #[test]
        fn caps_at_the_max() {
            assert_eq!(sectioned_overview().height(80, 7), 7);
        }
    }

    mod render {
        use super::*;

        fn annotated_overview() -> KvOverview {
            let note_row = |k: &str, v: &str, n: &str| (k.to_owned(), v.to_owned(), n.to_owned());
            KvOverview::new(
                "Config",
                vec![
                    KvSection::annotated(vec![
                        note_row("Model", "Opus", "$OX_MODEL"),
                        note_row("Base URL", "https://x.test", "default"),
                    ]),
                    KvSection::new(vec![row("user", "~/.config/ox/config.toml")]),
                ],
            )
        }

        #[test]
        fn lays_out_every_row_with_a_close_hint_when_it_fits() {
            let m = flat_overview();

            let rows = trimmed_rows(&m, 40, m.height(40, 40));

            assert_eq!(
                rows,
                [
                    "Status",
                    "",
                    "Model   Claude Opus",
                    "Effort  high",
                    "",
                    "Esc to close"
                ],
            );
        }

        #[test]
        fn clips_the_body_between_pinned_header_and_scroll_hint() {
            let m = long_overview();

            let rows = trimmed_rows(&m, 40, 7);

            assert_eq!(
                rows,
                [
                    "Long",
                    "",
                    "k0  v0",
                    "k1  v1",
                    "k2  v2",
                    "",
                    "↑ / ↓ to scroll  ·  Esc to close"
                ]
            );
        }

        #[test]
        fn aligns_notes_in_a_column_when_they_fit() {
            // 35 columns is exactly label + gap + value + gap + note for this fixture.
            let m = annotated_overview();

            let rows = trimmed_rows(&m, 35, m.height(35, 40));

            assert_eq!(
                rows[2..6],
                [
                    "Model     Opus            $OX_MODEL",
                    "Base URL  https://x.test  default",
                    "",
                    "user      ~/.config/ox/config.toml",
                ],
            );
        }

        #[test]
        fn puts_notes_after_their_values_when_the_column_would_overflow() {
            let m = annotated_overview();

            let rows = trimmed_rows(&m, 34, m.height(34, 40));

            assert_eq!(
                rows[2..4],
                [
                    "Model     Opus  $OX_MODEL",
                    "Base URL  https://x.test  default",
                ],
            );
        }

        #[test]
        fn keeps_the_other_notes_aligned_beside_a_value_too_wide_for_the_column() {
            let note_row = |k: &str, v: &str, n: &str| (k.to_owned(), v.to_owned(), n.to_owned());
            let m = KvOverview::new(
                "Config",
                vec![KvSection::annotated(vec![
                    note_row("Model", "Opus", "user"),
                    note_row("Theme", "mocha-dark", "default"),
                    note_row("Status", "model, run-state, current-time", "env"),
                ])],
            );

            let rows = trimmed_rows(&m, 44, m.height(44, 40));

            assert_eq!(
                rows[2..5],
                [
                    "Model   Opus        user",
                    "Theme   mocha-dark  default",
                    "Status  model, run-state, current-time  env",
                ],
            );
        }

        #[test]
        fn separates_headed_sections_with_blank_rows() {
            let m = sectioned_overview();

            let rows = trimmed_rows(&m, 40, m.height(40, 40));

            assert_eq!(
                rows,
                [
                    "Config",
                    "",
                    "Resolved",
                    "",
                    "Model  Claude Opus",
                    "",
                    "Source Files",
                    "",
                    "User   ~/.config/ox/config.toml",
                    "",
                    "Esc to close",
                ]
            );
        }
    }

    mod handle_key {
        use super::*;

        #[test]
        fn scrolls_the_clipped_body_within_its_bounds() {
            let mut m = long_overview();
            trimmed_rows(&m, 40, 7);

            m.handle_key(&KeyEvent::from(KeyCode::Down));

            assert_eq!(trimmed_rows(&m, 40, 7)[2], "k1  v1");

            m.handle_key(&KeyEvent::from(KeyCode::PageDown));

            assert_eq!(
                trimmed_rows(&m, 40, 7)[2],
                "k4  v4",
                "a page is the three body rows"
            );

            m.handle_key(&KeyEvent::from(KeyCode::End));

            assert_eq!(
                trimmed_rows(&m, 40, 7)[4],
                "k9  v9",
                "End pins the last row"
            );

            m.handle_key(&KeyEvent::from(KeyCode::Down));

            assert_eq!(
                trimmed_rows(&m, 40, 7)[4],
                "k9  v9",
                "no scrolling past the end"
            );

            m.handle_key(&KeyEvent::from(KeyCode::Char('k')));

            assert_eq!(trimmed_rows(&m, 40, 7)[4], "k8  v8");

            m.handle_key(&KeyEvent::from(KeyCode::PageUp));

            assert_eq!(trimmed_rows(&m, 40, 7)[2], "k3  v3");

            m.handle_key(&KeyEvent::from(KeyCode::Home));

            assert_eq!(trimmed_rows(&m, 40, 7)[2], "k0  v0");
        }

        #[test]
        fn every_key_is_consumed_so_only_universal_cancel_dismisses() {
            // Esc / Ctrl+C close at the stack layer, so the modal itself never pops.
            let mut m = flat_overview();

            for code in [
                KeyCode::Enter,
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::Char('x'),
                KeyCode::Tab,
            ] {
                let outcome = m.handle_key(&KeyEvent::from(code));

                assert!(
                    matches!(outcome, ModalKey::Consumed),
                    "{code:?} must be consumed",
                );
            }
        }
    }
}
