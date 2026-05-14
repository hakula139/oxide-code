//! Read-only key-value overview modal with a title, multi-section body, and fixed footer.
//! Used by `/status`, `/config`, and `/help`. The modal owns layout for `KvSection` fixtures.

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{Modal, ModalKey};
use crate::tui::theme::Theme;

// ── Constants ──

const COLUMN_GAP: usize = 2;
const FOOTER: &str = "Esc to close";

// ── KvSection ──

pub(crate) struct KvSection {
    heading: Option<String>,
    rows: Vec<(String, String)>,
}

impl KvSection {
    pub(crate) fn new(rows: Vec<(String, String)>) -> Self {
        Self {
            heading: None,
            rows,
        }
    }

    pub(crate) fn with_heading(mut self, heading: impl Into<String>) -> Self {
        self.heading = Some(heading.into());
        self
    }
}

// ── KvOverview ──

pub(crate) struct KvOverview {
    title: String,
    sections: Vec<KvSection>,
}

impl KvOverview {
    pub(crate) fn new(title: impl Into<String>, sections: Vec<KvSection>) -> Self {
        Self {
            title: title.into(),
            sections,
        }
    }

    /// Label gutter spans every section so columns stay aligned across headings.
    fn label_width(&self) -> usize {
        self.sections
            .iter()
            .flat_map(|s| s.rows.iter())
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(0)
    }

    fn line_count(&self) -> usize {
        let mut total = 2;
        for (i, section) in self.sections.iter().enumerate() {
            if i > 0 {
                total += 1;
            }
            if section.heading.is_some() {
                total += 2;
            }
            total += section.rows.len();
        }
        total + 2
    }
}

impl Modal for KvOverview {
    fn height(&self, _width: u16) -> u16 {
        u16::try_from(self.line_count()).unwrap_or(u16::MAX)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let label_width = self.label_width();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(self.line_count());

        lines.push(Line::from(Span::styled(
            self.title.clone(),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

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
            for (label, value) in &section.rows {
                lines.push(Line::from(vec![
                    Span::styled(format!("{label:<label_width$}"), theme.dim()),
                    Span::styled(" ".repeat(COLUMN_GAP), theme.surface()),
                    Span::styled(value.clone(), theme.text()),
                ]));
            }
        }

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(FOOTER.to_owned(), theme.dim())));

        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
    }

    fn handle_key(&mut self, _event: &KeyEvent) -> ModalKey {
        // Read-only: nothing to commit. Esc / Ctrl+C cancel universally via the modal stack.
        ModalKey::Consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── KvOverview::height ──

    #[test]
    fn height_for_single_section_without_heading_counts_title_blank_rows_blank_footer() {
        assert_eq!(flat_overview().height(80), 6);
    }

    #[test]
    fn height_for_two_headed_sections_adds_heading_blanks_and_inter_section_blank() {
        assert_eq!(sectioned_overview().height(80), 11);
    }

    // ── KvOverview::render ──

    #[test]
    fn render_runs_at_typical_widths_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let m = sectioned_overview();
        let theme = Theme::default();
        for width in [40_u16, 80, 120] {
            let h = m.height(width).max(1);
            let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
            terminal
                .draw(|frame| m.render(frame, Rect::new(0, 0, width, h), &theme))
                .expect("render must not panic");
        }
    }

    // ── KvOverview::handle_key ──

    #[test]
    fn every_key_is_consumed_so_only_universal_cancel_dismisses() {
        // Read-only modal — Esc / Ctrl+C close at the stack layer; the modal itself never pops.
        use crossterm::event::KeyCode;
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
