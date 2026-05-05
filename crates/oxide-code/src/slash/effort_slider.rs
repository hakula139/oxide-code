//! `/effort` — horizontal Speed ←→ Intelligence slider opened by bare `/effort`. Typed-arg
//! `/effort <level>` keeps direct-switch semantics in [`super::effort::EffortCmd`].

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::context::LiveSessionInfo;
use crate::agent::event::UserAction;
use crate::config::Effort;
use crate::model::capabilities_for;
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;

// ── Constants ──

/// Per-tier column slot. Wide enough for the longest label ("medium" = 6 chars) with breathing
/// room on each side.
const SLOT_WIDTH: usize = 10;
const SLOT_HALF: usize = SLOT_WIDTH / 2;

const SPEED_LABEL: &str = "Speed";
const INTEL_LABEL: &str = "Intelligence";
const TRACK_GLYPH: char = '─';
const MARKER_GLYPH: char = '▲';

const TITLE: &str = "Select effort";
const FOOTER: &str = "←/→ to change effort  ·  Enter to confirm  ·  Esc to cancel";

const BODY_HEIGHT: u16 = 8;

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
        if supported.is_empty() {
            return None;
        }
        // `resolve_effort` clamps a user-set value or falls back to the model default; both paths
        // land on a tier in `supported` because the table guarantees at least one accepted level.
        let initial = caps
            .resolve_effort(info.config.effort)
            .unwrap_or(supported[supported.len() / 2]);
        let selected = supported.iter().position(|e| *e == initial).unwrap_or(0);
        Some(Self {
            supported,
            selected,
            initial,
        })
    }

    fn step_left(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn step_right(&mut self) {
        if self.selected + 1 < self.supported.len() {
            self.selected += 1;
        }
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

    /// Total visual width — each tier owns one slot.
    fn slider_width(&self) -> usize {
        self.supported.len() * SLOT_WIDTH
    }

    fn tier_center(i: usize) -> usize {
        SLOT_HALF + i * SLOT_WIDTH
    }

    /// Track spans tier 0 center → tier (n-1) center, inclusive.
    fn track_width(&self) -> usize {
        match self.supported.len() {
            0 => 0,
            n => (n - 1) * SLOT_WIDTH + 1,
        }
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
            self.line_marker(theme),
            self.line_tier_labels(theme),
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
                self.step_right();
                ModalKey::Consumed
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.step_left();
                ModalKey::Consumed
            }
            _ => ModalKey::Consumed,
        }
    }
}

// ── Render helpers ──

impl EffortSlider {
    fn line_speed_intel(&self, theme: &Theme) -> Line<'static> {
        let total = self.slider_width();
        let n = self.supported.len();
        let speed_start = SLOT_HALF;
        let intel_end = SLOT_HALF + (n - 1) * SLOT_WIDTH;
        let intel_start = (intel_end + 1).saturating_sub(INTEL_LABEL.len());
        let speed_end = speed_start + SPEED_LABEL.len();

        let mut buf = String::with_capacity(total);
        buf.push_str(&" ".repeat(speed_start));
        buf.push_str(SPEED_LABEL);
        if intel_start > speed_end {
            buf.push_str(&" ".repeat(intel_start - speed_end));
            buf.push_str(INTEL_LABEL);
        }
        let used = buf.chars().count();
        if total > used {
            buf.push_str(&" ".repeat(total - used));
        }
        Line::from(Span::styled(buf, theme.dim()))
    }

    fn line_track(&self, theme: &Theme) -> Line<'static> {
        let total = self.slider_width();
        let track = self.track_width();
        let mut buf = String::with_capacity(total);
        buf.push_str(&" ".repeat(SLOT_HALF));
        for _ in 0..track {
            buf.push(TRACK_GLYPH);
        }
        let used = SLOT_HALF + track;
        if total > used {
            buf.push_str(&" ".repeat(total - used));
        }
        Line::from(Span::styled(buf, theme.dim()))
    }

    fn line_marker(&self, theme: &Theme) -> Line<'static> {
        let total = self.slider_width();
        let center = Self::tier_center(self.selected);
        let mut spans = Vec::with_capacity(3);
        if center > 0 {
            spans.push(Span::raw(" ".repeat(center)));
        }
        spans.push(Span::styled(
            MARKER_GLYPH.to_string(),
            theme.accent().add_modifier(Modifier::BOLD),
        ));
        let used = center + 1;
        if total > used {
            spans.push(Span::raw(" ".repeat(total - used)));
        }
        Line::from(spans)
    }

    fn line_tier_labels(&self, theme: &Theme) -> Line<'static> {
        let total = self.slider_width();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(self.supported.len() * 2 + 1);
        let mut col = 0usize;
        for (idx, level) in self.supported.iter().enumerate() {
            let label = format!("{level}");
            let label_len = label.chars().count();
            let center = Self::tier_center(idx);
            let label_start = center.saturating_sub(label_len / 2);
            if label_start > col {
                spans.push(Span::raw(" ".repeat(label_start - col)));
                col = label_start;
            }
            let style = if idx == self.selected {
                theme.accent().add_modifier(Modifier::BOLD)
            } else {
                theme.dim()
            };
            spans.push(Span::styled(label, style));
            col += label_len;
        }
        if col < total {
            spans.push(Span::raw(" ".repeat(total - col)));
        }
        Line::from(spans)
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
        // Haiku 4.5 has no effort tier — slider opens to nothing, caller errors first.
        let mut info = test_session_info();
        info.config.model_id = "claude-haiku-4-5".to_owned();
        assert!(EffortSlider::new(&info).is_none());
    }

    #[test]
    fn new_lists_only_supported_tiers_low_to_high() {
        // Sonnet 4.6 supports low/medium/high but not xhigh/max — the slider must drop the
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
        // No user-set effort + Opus 4.7 default = xhigh.
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
        // Pin: `h` and `l` must behave like Left/Right respectively.
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
                assert!(model.is_none(), "model axis must NOT be set");
                assert_eq!(effort, Some(Effort::Medium));
            }
            other => panic!("expected Submitted SwapConfig, got {other:?}"),
        }
    }

    // ── render ──

    #[test]
    fn render_runs_at_typical_widths_without_panicking() {
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
                    .draw(|frame| {
                        s.render(frame, Rect::new(0, 0, width, h), &theme);
                    })
                    .expect("render must not panic");
            }
        }
    }

    #[test]
    fn render_marker_column_tracks_selected_tier() {
        // The ▲ glyph must move horizontally as the cursor walks the ladder. Render two
        // adjacent positions and assert the marker column actually shifts.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let width: u16 = 80;
        let mut s = slider_for("claude-opus-4-7", Some(Effort::Low));
        let height = s.height(width);

        let render_marker_x = |s: &EffortSlider| -> u16 {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| s.render(frame, Rect::new(0, 0, width, height), &theme))
                .expect("render must not panic");
            let buf = terminal.backend().buffer().clone();
            // Marker is on row 4 (title+blank+speed/intel+track = 4 rows above).
            (0..width)
                .find(|x| buf[(*x, 4)].symbol() == MARKER_GLYPH.to_string())
                .expect("marker glyph must appear on the marker row")
        };

        let x_low = render_marker_x(&s);
        s.handle_key(&key(KeyCode::Right));
        let x_medium = render_marker_x(&s);
        assert!(
            x_medium > x_low,
            "Right must shift marker to the right (low={x_low}, medium={x_medium})",
        );
    }
}
