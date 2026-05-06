//! `/effort` — horizontal Speed ↔ Intelligence slider opened by bare `/effort`. Typed-arg
//! `/effort <level>` keeps direct-switch semantics in [`super::effort::EffortCmd`].

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::context::LiveSessionInfo;
use crate::agent::event::UserAction;
use crate::config::Effort;
use crate::model::capabilities_for;
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;

// ── Constants ──

const SPEED_LABEL: &str = "Speed";
const INTEL_LABEL: &str = "Intelligence";
const TRACK_GLYPH: char = '─';

const GLYPH_PREFIX_WIDTH: usize = 2;
/// Fixed inter-tier spacing — slot-based widths drifted by half a col on even-length labels.
const TIER_GAP: usize = 3;

const TITLE: &str = "Select effort";
const FOOTER: &str = "←/→ to change effort  ·  Enter to confirm  ·  Esc to cancel";

/// Title + blank + speed / intel + track + tier labels + blank + footer.
const BODY_HEIGHT: u16 = 7;

/// Per-tier color (Low blue → Max red). Magenta — not yellow — at Xhigh keeps High and Xhigh
/// distinct on pastel palettes where green and yellow read close.
pub(super) fn tier_color(level: Effort) -> Color {
    match level {
        Effort::Low => Color::Blue,
        Effort::Medium => Color::Cyan,
        Effort::High => Color::Green,
        Effort::Xhigh => Color::Magenta,
        Effort::Max => Color::Red,
    }
}

// ── EffortSlider ──

/// Opened by bare `/effort` when the active model supports an effort tier.
pub(super) struct EffortSlider {
    /// Tiers the active model accepts, low → high.
    supported: Vec<Effort>,
    /// Cursor index into `supported`.
    selected: usize,
    /// Effort at modal open. Submit short-circuits when unchanged.
    initial: Effort,
}

impl EffortSlider {
    /// `None` when the active model has no effort tier; caller errors before opening.
    pub(super) fn new(info: &LiveSessionInfo) -> Option<Self> {
        let caps = capabilities_for(&info.config.model_id);
        if !caps.effort {
            return None;
        }
        let supported: Vec<Effort> = Effort::ALL
            .iter()
            .copied()
            .filter(|level| caps.accepts_effort(*level))
            .collect();
        let initial = caps
            .resolve_effort(info.config.effort)
            .expect("caps.effort implies a resolvable tier");
        let selected = supported
            .iter()
            .position(|e| *e == initial)
            .expect("resolve_effort lands inside `supported`");
        Some(Self {
            supported,
            selected,
            initial,
        })
    }

    /// No-touch Enter (or a touch that returns to the initial pick) cancels — avoids a spurious
    /// `SwapConfig` that would re-resolve config defaults.
    fn submit(&self) -> ModalKey {
        let picked = self.supported[self.selected];
        if picked == self.initial {
            return ModalKey::Cancelled;
        }
        ModalKey::Submitted(ModalAction::User(UserAction::SwapConfig {
            model: None,
            effort: Some(picked),
        }))
    }

    /// Width every render line targets so `Paragraph::Center` aligns them on a single axis.
    fn slider_width(&self) -> usize {
        let units: usize = self
            .supported
            .iter()
            .map(|e| GLYPH_PREFIX_WIDTH + format!("{e}").len())
            .sum();
        units + self.supported.len().saturating_sub(1) * TIER_GAP
    }

    fn line_speed_intel(&self, theme: &Theme) -> Line<'static> {
        let total = self.slider_width();
        let middle = total
            .saturating_sub(SPEED_LABEL.len())
            .saturating_sub(INTEL_LABEL.len());
        let buf = format!("{SPEED_LABEL}{}{INTEL_LABEL}", " ".repeat(middle));
        Line::from(Span::styled(buf, theme.dim()))
    }

    fn line_track(&self, theme: &Theme) -> Line<'static> {
        let total = self.slider_width();
        let buf: String = std::iter::repeat_n(TRACK_GLYPH, total).collect();
        Line::from(Span::styled(buf, theme.dim()))
    }

    fn line_tier_labels(&self) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(self.supported.len() * 2);
        for (idx, level) in self.supported.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::raw(" ".repeat(TIER_GAP)));
            }
            let active = idx == self.selected;
            let glyph = if active { '●' } else { '○' };
            let mut style = Style::default().fg(tier_color(*level));
            if active {
                style = style.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(format!("{glyph} {level}"), style));
        }
        Line::from(spans)
    }
}

impl Modal for EffortSlider {
    fn height(&self, _width: u16) -> u16 {
        BODY_HEIGHT
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let lines = vec![
            Line::from(Span::styled(
                TITLE,
                theme.accent().add_modifier(Modifier::BOLD),
            )),
            Line::default(),
            self.line_speed_intel(theme),
            self.line_track(theme),
            self.line_tier_labels(),
            Line::default(),
            Line::from(Span::styled(FOOTER, theme.dim())),
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
                if self.selected + 1 < self.supported.len() {
                    self.selected += 1;
                }
                ModalKey::Consumed
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.selected = self.selected.saturating_sub(1);
                ModalKey::Consumed
            }
            _ => ModalKey::Consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;

    fn slider_for(model_id: &str, effort: Option<Effort>) -> EffortSlider {
        let mut info = test_session_info();
        info.config.model_id = model_id.to_owned();
        info.config.effort = effort;
        EffortSlider::new(&info).expect("model accepts effort")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    // ── new ──

    #[test]
    fn new_returns_none_for_no_effort_model() {
        let mut info = test_session_info();
        info.config.model_id = "claude-haiku-4-5".to_owned();
        assert!(EffortSlider::new(&info).is_none());
    }

    #[test]
    fn new_lists_only_supported_tiers_low_to_high() {
        // Sonnet 4.6 supports low / medium / high but not xhigh / max — the slider must drop the
        // unsupported tiers entirely so the cursor never lands on a value the API would reject.
        let s = slider_for("claude-sonnet-4-6", Some(Effort::High));
        assert_eq!(s.supported, vec![Effort::Low, Effort::Medium, Effort::High]);
    }

    #[test]
    fn new_seeds_selected_to_active_effort() {
        let s = slider_for("claude-opus-4-7", Some(Effort::High));
        assert_eq!(s.supported[s.selected], Effort::High);
        assert_eq!(s.initial, Effort::High);
    }

    #[test]
    fn new_falls_back_to_model_default_when_effort_unset() {
        let s = slider_for("claude-opus-4-7", None);
        assert_eq!(s.initial, Effort::Xhigh);
        assert_eq!(s.supported[s.selected], Effort::Xhigh);
    }

    // ── handle_key ──

    #[test]
    fn right_arrow_advances_selected_within_supported() {
        let mut s = slider_for("claude-opus-4-7", Some(Effort::Low));
        let before = s.selected;
        s.handle_key(&key(KeyCode::Right));
        assert_eq!(s.selected, before + 1);
    }

    #[test]
    fn right_arrow_at_top_clamps_without_panicking() {
        let mut s = slider_for("claude-opus-4-7", Some(Effort::Max));
        s.handle_key(&key(KeyCode::Right));
        assert_eq!(s.supported[s.selected], Effort::Max, "must stay clamped");
    }

    #[test]
    fn left_arrow_retreats_selected_until_low() {
        let mut s = slider_for("claude-opus-4-7", Some(Effort::Medium));
        s.handle_key(&key(KeyCode::Left));
        assert_eq!(s.supported[s.selected], Effort::Low);
        s.handle_key(&key(KeyCode::Left));
        assert_eq!(s.supported[s.selected], Effort::Low, "stays clamped");
    }

    #[test]
    fn vim_keys_mirror_arrow_navigation() {
        let mut s = slider_for("claude-opus-4-7", Some(Effort::Low));
        s.handle_key(&key(KeyCode::Char('l')));
        assert_eq!(s.supported[s.selected], Effort::Medium);
        s.handle_key(&key(KeyCode::Char('h')));
        assert_eq!(s.supported[s.selected], Effort::Low);
    }

    #[test]
    fn enter_with_no_change_returns_cancelled() {
        // Open + Enter without moving is the same shape as Esc — nothing to dispatch.
        let mut s = slider_for("claude-opus-4-7", Some(Effort::High));
        let outcome = s.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, ModalKey::Cancelled));
    }

    #[test]
    fn enter_after_change_emits_swap_config_with_effort_only() {
        let mut s = slider_for("claude-opus-4-7", Some(Effort::Low));
        s.handle_key(&key(KeyCode::Right));
        let outcome = s.handle_key(&key(KeyCode::Enter));
        match outcome {
            ModalKey::Submitted(ModalAction::User(UserAction::SwapConfig { model, effort })) => {
                assert!(model.is_none(), "effort-only swap; model unchanged");
                assert_eq!(effort, Some(Effort::Medium));
            }
            other => panic!("expected Submitted SwapConfig, got {other:?}"),
        }
    }

    #[test]
    fn unhandled_key_is_consumed_without_state_change() {
        // Esc / Ctrl+C are intercepted at ModalStack; everything else (Up, Tab, letters other
        // than h / l) must fall through to `Consumed` and leave the cursor put.
        let mut s = slider_for("claude-opus-4-7", Some(Effort::Medium));
        let before = s.selected;
        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Tab, KeyCode::Char('q')] {
            let outcome = s.handle_key(&key(code));
            assert!(
                matches!(outcome, ModalKey::Consumed),
                "{code:?} must be consumed"
            );
            assert_eq!(s.selected, before, "{code:?} must not move the cursor");
        }
    }

    // ── render ──

    #[test]
    fn render_emits_title_at_typical_widths() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        // Cover both a full-ladder model (Opus 4.7 → 5 tiers) and a short-ladder one
        // (Sonnet 4.6 → 3 tiers) so the n-dependent width arithmetic gets exercised.
        for (model, effort) in [
            ("claude-opus-4-7", Some(Effort::High)),
            ("claude-sonnet-4-6", Some(Effort::Medium)),
        ] {
            let s = slider_for(model, effort);
            for width in [40_u16, 80, 120] {
                let h = s.height(width);
                let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
                terminal
                    .draw(|frame| s.render(frame, Rect::new(0, 0, width, h), &theme))
                    .expect("render must not panic");
                let buf = terminal.backend().buffer().clone();
                let title_visible = (0..h).any(|y| {
                    (0..width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                        .contains(TITLE)
                });
                assert!(
                    title_visible,
                    "{model} at width {width}: title `{TITLE}` must render"
                );
            }
        }
    }

    #[test]
    fn render_active_glyph_column_tracks_selected_tier() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let width: u16 = 80;
        let mut s = slider_for("claude-opus-4-7", Some(Effort::Low));
        let height = s.height(width);

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
                .expect("active bold `●` must appear on the tier-label row")
        };

        let x_low = active_x(&s);
        s.handle_key(&key(KeyCode::Right));
        let x_medium = active_x(&s);
        assert!(
            x_medium > x_low,
            "Right must shift active glyph to the right (low={x_low}, medium={x_medium})",
        );
    }
}
