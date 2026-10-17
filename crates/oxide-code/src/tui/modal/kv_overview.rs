//! Read-only key-value overview modal with a title, multi-section body, and fixed footer.
//! Used by `/status`, `/config`, and `/help`. The modal owns layout for `KvSection` fixtures.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::chrome::{FOOTER_HEIGHT, Header, KEY_UP_DOWN, render_footer};
use super::{Modal, ModalKey};
use crate::tui::saturating_u16;
use crate::tui::theme::Theme;
use crate::tui::viewport::Viewport;

// ── Constants ──

const COLUMN_GAP: usize = 2;
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
}

impl KvOverview {
    pub(crate) fn new(title: impl Into<String>, sections: Vec<KvSection>) -> Self {
        Self {
            header: Header::new(title),
            sections,
            viewport: Viewport::default(),
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

    fn body_len(&self) -> usize {
        let mut total = 0;
        for (i, section) in self.sections.iter().enumerate() {
            if i > 0 {
                total += 1;
            }
            if section.heading.is_some() {
                total += 2;
            }
            total += section.rows.len();
        }
        total
    }

    fn body_lines(&self, theme: &Theme, width: usize) -> Vec<Line<'static>> {
        let label_width = self.label_width();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(self.body_len());
        for (i, section) in self.sections.iter().enumerate() {
            if i > 0 {
                lines.push(Line::default());
            }
            if let Some(heading) = &section.heading {
                lines.push(Line::from(Span::styled(
                    heading.clone(),
                    theme.text().add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::default());
            }
            let value_width = note_column(section, label_width, width);
            for row in &section.rows {
                let mut spans = vec![
                    Span::styled(
                        format!(
                            "{}{}",
                            row.label,
                            " ".repeat(label_width - row.label.width())
                        ),
                        theme.dim(),
                    ),
                    Span::styled(" ".repeat(COLUMN_GAP), theme.surface()),
                    Span::styled(row.value.clone(), theme.text()),
                ];
                if let Some(note) = &row.note {
                    let pad = value_width.saturating_sub(row.value.width());
                    spans.push(Span::styled(" ".repeat(pad + COLUMN_GAP), theme.surface()));
                    spans.push(Span::styled(note.clone(), theme.dim()));
                }
                lines.push(Line::from(spans));
            }
        }
        lines
    }

    fn scroll(&self, delta: isize) {
        self.viewport.scroll_by(delta, self.body_len());
    }

    fn page(&self) -> isize {
        isize::try_from(self.viewport.page()).unwrap_or(isize::MAX)
    }
}

/// Width of the value column that aligns a section's notes, or 0 to put each note right after its
/// value when the aligned notes would overflow `width`.
fn note_column(section: &KvSection, label_width: usize, width: usize) -> usize {
    let (value_width, note_width) = section
        .rows
        .iter()
        .filter_map(|row| {
            let note = row.note.as_ref()?;
            Some((row.value.width(), note.width()))
        })
        .fold((0, 0), |(values, notes), (value, note)| {
            (values.max(value), notes.max(note))
        });
    let aligned = label_width + COLUMN_GAP + value_width + COLUMN_GAP + note_width;
    if aligned <= width { value_width } else { 0 }
}

impl Modal for KvOverview {
    fn height(&self, _width: u16, max_height: u16) -> u16 {
        let body = saturating_u16(self.body_len());
        (self.header.height() + FOOTER_HEIGHT)
            .saturating_add(body)
            .min(max_height)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let rows = area
            .height
            .saturating_sub(self.header.height() + FOOTER_HEIGHT);
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
    use crate::tui::modal::testing::render_rows;

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

    // ── KvOverview::label_width ──

    #[test]
    fn label_width_spans_every_section() {
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
    fn label_width_empty_sections_is_zero() {
        let m = KvOverview::new("T", vec![]);
        assert_eq!(m.label_width(), 0);
    }

    // ── KvOverview::body_lines ──

    #[test]
    fn body_lines_aligns_cjk_labels_values_and_notes_by_display_width() {
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
        assert_eq!(text(14), ["中文  值  说明", "a     abc  note"]);
    }

    // ── KvOverview::height ──

    #[test]
    fn height_for_single_section_without_heading_counts_title_blank_rows_blank_footer() {
        assert_eq!(flat_overview().height(80, 40), 6);
    }

    #[test]
    fn height_for_two_headed_sections_adds_heading_blanks_and_inter_section_blank() {
        assert_eq!(sectioned_overview().height(80, 40), 11);
    }

    #[test]
    fn height_caps_at_the_max() {
        assert_eq!(sectioned_overview().height(80, 7), 7);
    }

    // ── KvOverview::render ──

    fn long_overview() -> KvOverview {
        let rows = (0..10)
            .map(|i| row(&format!("k{i}"), &format!("v{i}")))
            .collect();
        KvOverview::new("Long", vec![KvSection::new(rows)])
    }

    #[test]
    fn render_lays_out_every_row_with_a_close_hint_when_it_fits() {
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
    fn render_clips_the_body_between_pinned_header_and_scroll_hint() {
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

    fn annotated_overview() -> KvOverview {
        let note_row = |k: &str, v: &str, n: &str| (k.to_owned(), v.to_owned(), n.to_owned());
        KvOverview::new(
            "Config",
            vec![
                KvSection::annotated(vec![
                    note_row("Model", "Opus", "$ANTHROPIC_MODEL"),
                    note_row("Base URL", "https://x.test", "default"),
                ]),
                KvSection::new(vec![row("user", "~/.config/ox/config.toml")]),
            ],
        )
    }

    #[test]
    fn render_aligns_notes_in_a_column_when_they_fit() {
        // 42 columns is exactly label + gap + value + gap + note for this fixture.
        let m = annotated_overview();
        let rows = trimmed_rows(&m, 42, m.height(42, 40));
        assert_eq!(
            rows[2..6],
            [
                "Model     Opus            $ANTHROPIC_MODEL",
                "Base URL  https://x.test  default",
                "",
                "user      ~/.config/ox/config.toml",
            ],
        );
    }

    #[test]
    fn render_puts_notes_after_their_values_when_the_column_would_overflow() {
        let m = annotated_overview();
        let rows = trimmed_rows(&m, 41, m.height(41, 40));
        assert_eq!(
            rows[2..4],
            [
                "Model     Opus  $ANTHROPIC_MODEL",
                "Base URL  https://x.test  default",
            ],
        );
    }

    #[test]
    fn render_separates_headed_sections_with_blank_rows() {
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

    // ── KvOverview::handle_key ──

    #[test]
    fn handle_key_scrolls_the_clipped_body_within_its_bounds() {
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
    fn handle_key_every_key_is_consumed_so_only_universal_cancel_dismisses() {
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
