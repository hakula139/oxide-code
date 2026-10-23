//! The horizontal Speed ↔ Intelligence slider that bare `/effort` opens.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use ox_core::model::Effort;
use ox_core::protocol::{ConfigPick, Op};

use crate::action::Action;
use crate::glyphs::{HORIZONTAL_RULE, RADIO_SELECTED, RADIO_UNSELECTED};
use crate::modal::chrome::{CONFIRM_HINTS, HINT_SEPARATOR, KEY_LEFT_RIGHT};
use crate::modal::{Modal, ModalAction, ModalKey};
use crate::session_info::LiveSessionInfo;
use crate::theme::Theme;

// ── Constants ──

const SPEED_LABEL: &str = "Speed";
const INTEL_LABEL: &str = "Intelligence";

const GLYPH_PREFIX_WIDTH: usize = 2;
/// Fixed inter-level spacing, since slot-based widths drift by half a column on even-length labels.
const LEVEL_GAP: usize = 3;

const TITLE: &str = "Select effort";

/// Title + blank + speed / intel + track + level labels + blank + footer.
const MODAL_HEIGHT: u16 = 7;

// ── Level Colors ──

/// Per-level color from Low blue to Max red. Xhigh is magenta because the gradient's yellow reads
/// close to High's green on pastel palettes.
pub(super) fn level_color(level: Effort) -> Color {
    match level {
        Effort::Low => Color::Blue,
        Effort::Medium => Color::Cyan,
        Effort::High => Color::Green,
        Effort::Xhigh => Color::Magenta,
        Effort::Max => Color::Red,
    }
}

// ── EffortSlider ──

/// Opened by bare `/effort` when the active model has effort levels.
pub(super) struct EffortSlider {
    /// Cursor index into [`Effort::ALL`].
    selected: usize,
    /// Effort at modal open.
    initial: Effort,
}

impl EffortSlider {
    /// `None` when the active model has no effort levels.
    pub(super) fn new(info: &LiveSessionInfo) -> Option<Self> {
        let initial = info.config.model.resolve_effort(info.config.effort)?;
        let selected = Effort::ALL
            .iter()
            .position(|e| *e == initial)
            .expect("Effort::ALL lists every level");
        Some(Self { selected, initial })
    }

    /// No-touch Enter, or a touch that returns to the initial pick, cancels. That avoids a spurious
    /// `SetModel` that would re-resolve config defaults.
    fn submit(&self) -> ModalKey {
        let picked = Effort::ALL[self.selected];
        if picked == self.initial {
            return ModalKey::Cancelled;
        }
        ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::SetModel(
            ConfigPick::Effort(picked),
        ))))
    }

    /// Width every render line targets so `Alignment::Center` aligns them on a single axis.
    fn slider_width() -> usize {
        let units: usize = Effort::ALL
            .iter()
            .map(|e| GLYPH_PREFIX_WIDTH + e.as_str().len())
            .sum();
        units + (Effort::ALL.len() - 1) * LEVEL_GAP
    }

    fn line_speed_intel(theme: &Theme) -> Line<'static> {
        let total = Self::slider_width();
        let middle = total
            .saturating_sub(SPEED_LABEL.len())
            .saturating_sub(INTEL_LABEL.len());
        let buf = format!("{SPEED_LABEL}{}{INTEL_LABEL}", " ".repeat(middle));
        Line::from(Span::styled(buf, theme.dim()))
    }

    fn line_track(theme: &Theme) -> Line<'static> {
        let total = Self::slider_width();
        let buf: String = HORIZONTAL_RULE.repeat(total);
        Line::from(Span::styled(buf, theme.dim()))
    }

    fn line_level_labels(&self) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(Effort::ALL.len() * 2);
        for (idx, level) in Effort::ALL.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::raw(" ".repeat(LEVEL_GAP)));
            }
            let active = idx == self.selected;
            let glyph = if active {
                RADIO_SELECTED
            } else {
                RADIO_UNSELECTED
            };
            let mut style = Style::default().fg(level_color(*level));
            if active {
                style = style.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(format!("{glyph} {level}"), style));
        }
        Line::from(spans)
    }

    fn line_footer(theme: &Theme) -> Line<'static> {
        let confirm = CONFIRM_HINTS.join(HINT_SEPARATOR);
        let hints = format!("{KEY_LEFT_RIGHT} to change effort{HINT_SEPARATOR}{confirm}");
        Line::from(Span::styled(hints, theme.dim()))
    }
}

impl Modal for EffortSlider {
    fn height(&self, _width: u16, _max_height: u16) -> u16 {
        MODAL_HEIGHT
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let lines = vec![
            Line::from(Span::styled(
                TITLE,
                theme.accent().add_modifier(Modifier::BOLD),
            )),
            Line::default(),
            Self::line_speed_intel(theme),
            Self::line_track(theme),
            self.line_level_labels(),
            Line::default(),
            Self::line_footer(theme),
        ];

        frame.render_widget(
            Paragraph::new(lines)
                .alignment(Alignment::Center)
                .style(theme.surface()),
            area,
        );
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        match event.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Right | KeyCode::Char('l') => {
                self.selected = step_level(self.selected, Direction::Forward);
                ModalKey::Consumed
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.selected = step_level(self.selected, Direction::Backward);
                ModalKey::Consumed
            }
            _ => ModalKey::Consumed,
        }
    }
}

// ── Effort Ladder ──

#[derive(Debug, Clone, Copy)]
pub(super) enum Direction {
    Forward,
    Backward,
}

/// The index one level from `idx` along [`Effort::ALL`], stopping at either end so a held key
/// cannot overshoot onto the opposite end.
pub(super) fn step_level(idx: usize, direction: Direction) -> usize {
    match direction {
        Direction::Forward => (idx + 1).min(Effort::ALL.len() - 1),
        Direction::Backward => idx.saturating_sub(1),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use ox_core::model::ModelSpec;

    use super::*;
    use crate::modal::testing::render_rows;
    use crate::session_info::testing::test_session_info;

    fn slider_for(model_id: &str, effort: Option<Effort>) -> EffortSlider {
        let mut info = test_session_info();
        info.config.model = ModelSpec::resolve(model_id);
        info.config.effort = effort;
        EffortSlider::new(&info).expect("model accepts effort")
    }

    fn selected_level(s: &EffortSlider) -> Effort {
        Effort::ALL[s.selected]
    }

    mod new {
        use super::*;

        #[test]
        fn rejects_an_unknown_model() {
            let mut info = test_session_info();
            info.config.model = ModelSpec::resolve("custom-model");
            info.config.effort = None;
            assert!(EffortSlider::new(&info).is_none());
        }

        #[test]
        fn seeds_selected_to_active_effort() {
            let s = slider_for("claude-opus-5-5", Some(Effort::High));
            assert_eq!(selected_level(&s), Effort::High);
            assert_eq!(s.initial, Effort::High);
        }

        #[test]
        fn falls_back_to_model_default_when_effort_unset() {
            let s = slider_for("claude-sonnet-5-5", None);
            assert_eq!(s.initial, Effort::High);
            assert_eq!(selected_level(&s), Effort::High);
        }
    }

    mod render {
        use super::*;

        #[test]
        fn centers_the_track_between_title_and_footer() {
            let s = slider_for("claude-opus-5-5", Some(Effort::High));

            let rows = render_rows(&s, 80, s.height(80, u16::MAX));

            let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
            assert_eq!(
                rows,
                [
                    "                                  Select effort",
                    "",
                    "                   Speed                          Intelligence",
                    "                   ───────────────────────────────────────────",
                    "                   ○ low   ○ medium   ● high   ○ xhigh   ○ max",
                    "",
                    "          ← / → to change effort  ·  Enter to confirm  ·  Esc to cancel",
                ]
            );
        }

        #[test]
        fn active_glyph_column_tracks_selected_level() {
            let theme = Theme::default();
            let width: u16 = 80;
            let mut s = slider_for("claude-opus-5-5", Some(Effort::Low));
            let height = s.height(width, u16::MAX);

            let active_x = |s: &EffortSlider| -> u16 {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| s.render(frame, Rect::new(0, 0, width, height), &theme))
                    .expect("render must not panic");
                let buf = terminal.backend().buffer().clone();
                // Row 4 = title + blank + speed / intel + track.
                (0..width)
                    .find(|x| {
                        let cell = &buf[(*x, 4)];
                        cell.symbol() == "●" && cell.style().add_modifier.contains(Modifier::BOLD)
                    })
                    .expect("active bold `●` must appear on the level-label row")
            };

            let x_low = active_x(&s);
            s.handle_key(&KeyEvent::from(KeyCode::Right));
            let x_medium = active_x(&s);
            assert!(
                x_medium > x_low,
                "Right must shift active glyph to the right (low={x_low}, medium={x_medium})",
            );
        }
    }

    mod handle_key {
        use super::*;

        #[test]
        fn right_arrow_advances_selected_within_supported() {
            let mut s = slider_for("claude-opus-5-5", Some(Effort::Low));
            let before = s.selected;
            s.handle_key(&KeyEvent::from(KeyCode::Right));
            assert_eq!(s.selected, before + 1);
        }

        #[test]
        fn right_arrow_at_top_clamps_without_panicking() {
            let mut s = slider_for("claude-opus-5-5", Some(Effort::Max));
            s.handle_key(&KeyEvent::from(KeyCode::Right));
            assert_eq!(selected_level(&s), Effort::Max, "must stay clamped");
        }

        #[test]
        fn left_arrow_retreats_selected_until_low() {
            let mut s = slider_for("claude-opus-5-5", Some(Effort::Medium));
            s.handle_key(&KeyEvent::from(KeyCode::Left));
            assert_eq!(selected_level(&s), Effort::Low);
            s.handle_key(&KeyEvent::from(KeyCode::Left));
            assert_eq!(selected_level(&s), Effort::Low, "stays clamped");
        }

        #[test]
        fn vim_keys_mirror_arrow_navigation() {
            let mut s = slider_for("claude-opus-5-5", Some(Effort::Low));
            s.handle_key(&KeyEvent::from(KeyCode::Char('l')));
            assert_eq!(selected_level(&s), Effort::Medium);
            s.handle_key(&KeyEvent::from(KeyCode::Char('h')));
            assert_eq!(selected_level(&s), Effort::Low);
        }

        #[test]
        fn enter_with_no_change_cancels() {
            let mut s = slider_for("claude-opus-5-5", Some(Effort::High));
            let outcome = s.handle_key(&KeyEvent::from(KeyCode::Enter));
            assert!(matches!(outcome, ModalKey::Cancelled));
        }

        #[test]
        fn enter_after_change_emits_set_model_with_effort_only() {
            let mut s = slider_for("claude-opus-5-5", Some(Effort::Low));
            s.handle_key(&KeyEvent::from(KeyCode::Right));
            let outcome = s.handle_key(&KeyEvent::from(KeyCode::Enter));
            match outcome {
                ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::SetModel(pick)))) => {
                    assert_eq!(pick, ConfigPick::Effort(Effort::Medium));
                }
                other => panic!("expected Submitted SetModel, got {other:?}"),
            }
        }

        #[test]
        fn unhandled_key_is_consumed_without_state_change() {
            // ModalStack handles Esc and Ctrl+C before the slider sees them.
            let mut s = slider_for("claude-opus-5-5", Some(Effort::Medium));
            let before = s.selected;
            for code in [KeyCode::Up, KeyCode::Down, KeyCode::Tab, KeyCode::Char('q')] {
                let outcome = s.handle_key(&KeyEvent::from(code));
                assert!(
                    matches!(outcome, ModalKey::Consumed),
                    "{code:?} must be consumed"
                );
                assert_eq!(s.selected, before, "{code:?} must not move the cursor");
            }
        }
    }
}
