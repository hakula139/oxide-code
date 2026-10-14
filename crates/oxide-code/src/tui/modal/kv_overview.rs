//! Read-only key-value overview modal with a title, multi-section body, and fixed footer.
//! Used by `/status`, `/config`, and `/help`. The modal owns layout for `KvSection` fixtures.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::chrome::{FOOTER_HEIGHT, Header, render_footer};
use super::{Modal, ModalKey};
use crate::tui::theme::Theme;
use crate::tui::viewport::Viewport;

// ── Constants ──

const COLUMN_GAP: usize = 2;
const FOOTER: &str = "Esc to close";
const SCROLL_FOOTER: &str = "↑ ↓ to scroll · Esc to close";

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
            .map(|(k, _)| k.chars().count())
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

    fn body_lines(&self, theme: &Theme) -> Vec<Line<'static>> {
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
            for (label, value) in &section.rows {
                lines.push(Line::from(vec![
                    Span::styled(format!("{label:<label_width$}"), theme.dim()),
                    Span::styled(" ".repeat(COLUMN_GAP), theme.surface()),
                    Span::styled(value.clone(), theme.text()),
                ]));
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

impl Modal for KvOverview {
    fn height(&self, _width: u16, max_height: u16) -> u16 {
        let body = u16::try_from(self.body_len()).unwrap_or(u16::MAX);
        (self.header.height() + FOOTER_HEIGHT)
            .saturating_add(body)
            .min(max_height)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let rows = area
            .height
            .saturating_sub(self.header.height() + FOOTER_HEIGHT);
        let body = self.body_lines(theme);
        let window = self.viewport.window(body.len(), usize::from(rows), None);
        let scrollable = window.len() < body.len();

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(usize::from(area.height));
        self.header.push_lines(&mut lines, area.width, theme);
        lines.extend(body.into_iter().skip(window.start).take(window.len()));
        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);

        let hint = if scrollable { SCROLL_FOOTER } else { FOOTER };
        render_footer(
            frame,
            area,
            Line::from(Span::styled(hint, theme.dim())),
            theme,
        );
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

    fn render_rows(m: &KvOverview, width: u16, height: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| m.render(frame, Rect::new(0, 0, width, height), &Theme::default()))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
                row.trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn render_lays_out_every_row_with_a_close_hint_when_it_fits() {
        let m = flat_overview();
        let rows = render_rows(&m, 40, m.height(40, 40));
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
        let rows = render_rows(&m, 40, 7);
        assert_eq!(
            rows,
            ["Long", "", "k0  v0", "k1  v1", "k2  v2", "", SCROLL_FOOTER]
        );
    }

    #[test]
    fn render_runs_at_typical_widths_without_panicking() {
        let m = sectioned_overview();
        for width in [40_u16, 80, 120] {
            render_rows(&m, width, m.height(width, 40));
        }
    }

    // ── KvOverview::handle_key ──

    #[test]
    fn handle_key_scrolls_the_clipped_body_within_its_bounds() {
        let mut m = long_overview();
        render_rows(&m, 40, 7);

        m.handle_key(&KeyEvent::from(KeyCode::Down));
        assert_eq!(render_rows(&m, 40, 7)[2], "k1  v1");
        m.handle_key(&KeyEvent::from(KeyCode::PageDown));
        assert_eq!(
            render_rows(&m, 40, 7)[2],
            "k4  v4",
            "a page is the three body rows"
        );
        m.handle_key(&KeyEvent::from(KeyCode::End));
        assert_eq!(render_rows(&m, 40, 7)[4], "k9  v9", "End pins the last row");
        m.handle_key(&KeyEvent::from(KeyCode::Down));
        assert_eq!(
            render_rows(&m, 40, 7)[4],
            "k9  v9",
            "no scrolling past the end"
        );
        m.handle_key(&KeyEvent::from(KeyCode::Char('k')));
        assert_eq!(render_rows(&m, 40, 7)[4], "k8  v8");
        m.handle_key(&KeyEvent::from(KeyCode::PageUp));
        assert_eq!(render_rows(&m, 40, 7)[2], "k3  v3");
        m.handle_key(&KeyEvent::from(KeyCode::Home));
        assert_eq!(render_rows(&m, 40, 7)[2], "k0  v0");
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
