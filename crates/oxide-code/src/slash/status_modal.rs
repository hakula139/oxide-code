//! `/status` overview modal — read-only single panel of session descriptors. Esc / Enter close.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::context::SessionInfo;
use crate::config::display_effort;
use crate::tui::modal::{Modal, ModalKey};
use crate::tui::theme::Theme;

// ── Constants ──

const TITLE: &str = "Status";
const FOOTER: &str = "Esc to close";
const COLUMN_GAP: usize = 2;

// ── StatusModal ──

pub(super) struct StatusModal {
    rows: Vec<(&'static str, String)>,
}

impl StatusModal {
    pub(super) fn new(info: &SessionInfo) -> Self {
        let model = format!("{} ({})", info.marketing_name(), info.config.model_id);
        let show_thinking = if info.config.show_thinking {
            "on".to_owned()
        } else {
            "off".to_owned()
        };
        let rows = vec![
            ("Model", model),
            ("Effort", display_effort(info.config.effort)),
            ("Working Directory", info.cwd.clone()),
            ("Session", info.session_id.clone()),
            ("Auth", info.config.auth_label.to_owned()),
            ("Version", info.version.to_owned()),
            ("Context Cache", info.config.prompt_cache_ttl.to_string()),
            ("Show Thinking", show_thinking),
        ];
        Self { rows }
    }
}

impl Modal for StatusModal {
    fn height(&self, _width: u16) -> u16 {
        let body = u16::try_from(self.rows.len()).unwrap_or(u16::MAX);
        body.saturating_add(4)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let label_width = self
            .rows
            .iter()
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(0);

        let mut lines: Vec<Line<'static>> =
            Vec::with_capacity(usize::from(self.height(area.width)));
        lines.push(Line::from(Span::styled(
            TITLE.to_owned(),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        for (label, value) in &self.rows {
            lines.push(Line::from(vec![
                Span::styled(format!("{label:<label_width$}"), theme.dim()),
                Span::styled(" ".repeat(COLUMN_GAP), theme.surface()),
                Span::styled(value.clone(), theme.text()),
            ]));
        }

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(FOOTER.to_owned(), theme.dim())));

        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        match event.code {
            KeyCode::Esc | KeyCode::Enter => ModalKey::Cancelled,
            _ => ModalKey::Consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;

    fn modal() -> StatusModal {
        StatusModal::new(&test_session_info())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    // ── new ──

    #[test]
    fn new_produces_one_row_per_session_descriptor() {
        let m = modal();
        assert_eq!(m.rows.len(), 8);
    }

    #[test]
    fn new_collects_every_session_field_value() {
        let info = test_session_info();
        let m = StatusModal::new(&info);
        let body: String = m
            .rows
            .iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join("|");
        for needle in [
            info.config.model_id.as_str(),
            info.cwd.as_str(),
            info.config.auth_label,
            info.version,
            info.session_id.as_str(),
        ] {
            assert!(body.contains(needle), "missing `{needle}`: {body}");
        }
    }

    #[test]
    fn new_renders_thinking_off_when_snapshot_says_false() {
        let info = test_session_info();
        let m = StatusModal::new(&info);
        let thinking_row = m
            .rows
            .iter()
            .find(|(k, _)| *k == "Show Thinking")
            .expect("show-thinking row");
        assert_eq!(thinking_row.1, "off");
    }

    #[test]
    fn new_renders_thinking_on_when_snapshot_says_true() {
        let mut info = test_session_info();
        info.config.show_thinking = true;
        let m = StatusModal::new(&info);
        let thinking_row = m
            .rows
            .iter()
            .find(|(k, _)| *k == "Show Thinking")
            .expect("show-thinking row");
        assert_eq!(thinking_row.1, "on");
    }

    // ── handle_key ──

    #[test]
    fn esc_closes_modal_silently() {
        let mut m = modal();
        let outcome = m.handle_key(&key(KeyCode::Esc));
        assert!(matches!(outcome, ModalKey::Cancelled));
    }

    #[test]
    fn enter_also_closes_modal_silently() {
        let mut m = modal();
        let outcome = m.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, ModalKey::Cancelled));
    }

    #[test]
    fn other_keys_are_consumed_and_modal_stays_open() {
        let mut m = modal();
        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Char('x'), KeyCode::Tab] {
            let outcome = m.handle_key(&key(code));
            assert!(
                matches!(outcome, ModalKey::Consumed),
                "{code:?} must be consumed",
            );
        }
    }

    // ── render ──

    #[test]
    fn render_runs_at_typical_widths_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let m = modal();
        let theme = Theme::default();
        for width in [40_u16, 80, 120] {
            let h = m.height(width).max(1);
            let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
            terminal
                .draw(|frame| {
                    m.render(frame, Rect::new(0, 0, width, h), &theme);
                })
                .expect("render must not panic");
        }
    }
}
