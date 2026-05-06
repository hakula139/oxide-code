//! Combined model + effort picker, opened by bare `/model`. Both axes commit through one
//! [`UserAction::SwapConfig`].

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::context::LiveSessionInfo;
use super::effort_slider::tier_color;
use crate::agent::event::UserAction;
use crate::config::Effort;
use crate::model::{ResolvedModelId, capabilities_for, display_name};
use crate::tui::modal::list_picker::{ListPicker, PickerItem};
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;

// ── Constants ──

/// Curated roster shown in the picker; `/model <id>` resolves against the full `MODELS` table.
const LISTED_MODELS: &[&str] = &[
    "claude-opus-4-7",
    "claude-opus-4-7[1m]",
    "claude-sonnet-4-6",
    "claude-sonnet-4-6[1m]",
    "claude-haiku-4-5",
];

// ── PickerItem ──

struct ModelRow {
    id: &'static str,
    is_active: bool,
    description: String,
    hint: Option<char>,
}

impl ModelRow {
    fn build(active_id: &str) -> Vec<Self> {
        LISTED_MODELS
            .iter()
            .enumerate()
            .map(|(idx, id)| Self {
                id,
                is_active: *id == active_id,
                description: display_name(id).into_owned(),
                hint: numeric_hint(idx),
            })
            .collect()
    }
}

/// `'1'`–`'9'` for the first nine rows; `None` after that.
fn numeric_hint(idx: usize) -> Option<char> {
    let digit = u32::try_from(idx).ok()?.checked_add(1)?;
    if (1..=9).contains(&digit) {
        char::from_digit(digit, 10)
    } else {
        None
    }
}

impl PickerItem for ModelRow {
    fn label(&self) -> &str {
        self.id
    }
    fn description(&self) -> Option<&str> {
        Some(&self.description)
    }
    fn is_active(&self) -> bool {
        self.is_active
    }
    fn key_hint(&self) -> Option<char> {
        self.hint
    }
}

// ── ModelEffortPicker ──

pub(super) struct ModelEffortPicker {
    list: ListPicker<ModelRow>,
    /// Active model at open — compared on submit to detect a model-axis change.
    active_model: String,
    /// Active effort at open — drives the "(default)" suffix when the user hasn't picked.
    active_effort: Option<Effort>,
    /// Resolved initial effort. Guards against spurious `SwapConfig` when `active_effort` is
    /// `None` but the model's default resolves to a concrete tier.
    initial_effort: Option<Effort>,
    /// Current pick; `None` when the highlighted model lacks an effort tier.
    effort: Option<Effort>,
    /// Set by Left / Right navigation; lets submit distinguish a real pick from the resolved
    /// default.
    effort_dirty: bool,
}

impl ModelEffortPicker {
    /// Infallible — both axes are always populated.
    pub(super) fn new(info: &LiveSessionInfo) -> Self {
        let active_model = info.config.model_id.clone();
        let active_effort = info.config.effort;
        let rows = ModelRow::build(&active_model);

        let mut list = ListPicker::new("Select model", rows).with_description(
            "Switch the active model. Applies to this session only — restart returns to your config.",
        );
        list.select_initial(|row| row.is_active);

        let effort = effort_for_highlighted(&list, active_effort);

        Self {
            list,
            active_model,
            active_effort,
            initial_effort: effort,
            effort,
            effort_dirty: false,
        }
    }

    /// Re-resolve the effort axis after the cursor moves. Reflects the highlighted model's caps
    /// — `None` when that model has no effort tier.
    fn refresh_effort_for_cursor(&mut self) {
        self.effort = effort_for_highlighted(&self.list, self.effort_or_active());
    }

    fn effort_or_active(&self) -> Option<Effort> {
        self.effort.or(self.active_effort)
    }

    fn cycle_effort(&mut self, direction: Direction) {
        let Some(row) = self.list.selected() else {
            return;
        };
        let caps = capabilities_for(row.id);
        if !caps.effort {
            return;
        }
        let supported: Vec<Effort> = Effort::ALL
            .iter()
            .copied()
            .filter(|level| caps.accepts_effort(*level))
            .collect();
        if supported.is_empty() {
            return;
        }
        let current_idx = self
            .effort
            .and_then(|e| supported.iter().position(|s| *s == e))
            .unwrap_or(0);
        let next_idx = match direction {
            Direction::Forward => (current_idx + 1) % supported.len(),
            Direction::Backward => {
                if current_idx == 0 {
                    supported.len() - 1
                } else {
                    current_idx - 1
                }
            }
        };
        self.effort = Some(supported[next_idx]);
        self.effort_dirty = true;
    }

    /// Commit both axes through one atomic `SwapConfig`. Each axis is `Some` only when actually
    /// moved — a no-touch Enter (or a touch that returns to the initial pick) cancels rather than
    /// firing a spurious swap that would re-resolve config defaults.
    fn submit(&self) -> ModalKey {
        let model = self
            .list
            .selected()
            .map_or_else(|| self.active_model.clone(), |row| row.id.to_owned());
        let model_changed = model != self.active_model;
        let effort_changed = self.effort_dirty && self.effort != self.initial_effort;

        if !model_changed && !effort_changed {
            return ModalKey::Cancelled;
        }
        ModalKey::Submitted(ModalAction::User(UserAction::SwapConfig {
            model: model_changed.then(|| ResolvedModelId::new(model)),
            effort: effort_changed.then_some(self.effort).flatten(),
        }))
    }

    fn render_effort_row(&self, theme: &Theme) -> Option<Line<'static>> {
        let row = self.list.selected()?;
        let caps = capabilities_for(row.id);
        if !caps.effort {
            return None;
        }
        let level = self.effort_or_active()?;
        let was_default = self.active_effort.is_none() && !self.effort_dirty;
        let suffix = if was_default { " (default)" } else { "" };
        let color = tier_color(level);
        Some(Line::from(vec![
            Span::styled("● ", Style::default().fg(color)),
            Span::styled(
                format!("{level} effort{suffix}"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ← →  to adjust", theme.dim()),
        ]))
    }
}

impl Modal for ModelEffortPicker {
    fn height(&self, width: u16) -> u16 {
        // List height + (effort row + spacer)? + footer + spacer
        let list_height = self.list.height(width);
        let mut h = list_height + 1; // spacer before footer
        if self.list.selected().is_some_and(has_effort_tier) {
            h += 2; // spacer + effort row
        }
        h + 1 // footer line
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let list_h = self.list.height(area.width);
        let list_area = Rect {
            height: list_h.min(area.height),
            ..area
        };
        self.list.render(frame, list_area, theme);

        let mut cursor_y = area.y.saturating_add(list_h);
        let mut remaining = area.height.saturating_sub(list_h);

        if let Some(line) = self.render_effort_row(theme) {
            cursor_y = cursor_y.saturating_add(1);
            remaining = remaining.saturating_sub(1);
            let row_area = Rect {
                x: area.x,
                y: cursor_y,
                width: area.width,
                height: 1.min(remaining),
            };
            frame.render_widget(Paragraph::new(line).style(theme.surface()), row_area);
            cursor_y = cursor_y.saturating_add(1);
            remaining = remaining.saturating_sub(1);
        }

        if remaining >= 2 {
            let footer_area = Rect {
                x: area.x,
                y: cursor_y.saturating_add(1),
                width: area.width,
                height: 1,
            };
            let footer = Line::from(Span::styled(
                "Enter to confirm  ·  Esc to cancel",
                theme.dim(),
            ));
            frame.render_widget(Paragraph::new(footer).style(theme.surface()), footer_area);
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        match event.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.list.select_prev();
                self.refresh_effort_for_cursor();
                ModalKey::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.list.select_next();
                self.refresh_effort_for_cursor();
                ModalKey::Consumed
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.cycle_effort(Direction::Forward);
                ModalKey::Consumed
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.cycle_effort(Direction::Backward);
                ModalKey::Consumed
            }
            KeyCode::Char(c @ '1'..='9') => {
                if self.list.select_by_hint(c) {
                    self.refresh_effort_for_cursor();
                }
                ModalKey::Consumed
            }
            _ => ModalKey::Consumed,
        }
    }
}

// ── Helpers ──

#[derive(Debug, Clone, Copy)]
enum Direction {
    Forward,
    Backward,
}

fn has_effort_tier(row: &ModelRow) -> bool {
    capabilities_for(row.id).effort
}

fn effort_for_highlighted(list: &ListPicker<ModelRow>, fallback: Option<Effort>) -> Option<Effort> {
    let row = list.selected()?;
    let caps = capabilities_for(row.id);
    if !caps.effort {
        return None;
    }
    Some(caps.resolve_effort(fallback).unwrap_or(Effort::High))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;

    fn picker() -> ModelEffortPicker {
        ModelEffortPicker::new(&test_session_info())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    // ── new ──

    #[test]
    fn new_positions_cursor_on_active_model() {
        // `test_session_info` ships claude-opus-4-7 active.
        let p = picker();
        let row = p.list.selected().expect("active row");
        assert_eq!(row.id, "claude-opus-4-7");
        assert!(row.is_active);
    }

    #[test]
    fn new_opens_with_clean_effort_axis() {
        // Effort dirty must start false so a no-touch Enter cancels rather than firing a
        // spurious SwapConfig (matters when the model resolves a default effort but the user
        // hasn't expressed a pick).
        let p = picker();
        assert!(!p.effort_dirty);
    }

    // ── handle_key ──

    #[test]
    fn down_arrow_advances_cursor_and_refreshes_effort() {
        let mut p = picker();
        let before = p.list.selected_index();
        p.handle_key(&key(KeyCode::Down));
        assert_eq!(p.list.selected_index(), before + 1);
    }

    #[test]
    fn numeric_jump_routes_cursor_to_matching_row() {
        // `5` jumps to the fifth listed model — Haiku 4.5.
        let mut p = picker();
        p.handle_key(&key(KeyCode::Char('5')));
        let row = p.list.selected().expect("selected row");
        assert_eq!(row.id, "claude-haiku-4-5");
    }

    #[test]
    fn right_arrow_cycles_effort_within_supported_levels() {
        // Opus 4.7 supports the full ladder. Pressing Right walks
        // through it; Left walks back.
        let mut p = picker();
        let initial = p.effort;
        p.handle_key(&key(KeyCode::Right));
        assert_ne!(p.effort, initial, "Right must change effort");
        assert!(p.effort_dirty, "navigation marks effort dirty");
    }

    #[test]
    fn right_arrow_on_no_tier_model_is_a_noop() {
        // Haiku 4.5 has no effort tier — Left/Right must not mutate the (None) effort state.
        let mut p = picker();
        p.handle_key(&key(KeyCode::Char('5'))); // jump to Haiku
        assert!(p.effort.is_none());
        assert!(!p.effort_dirty);
        p.handle_key(&key(KeyCode::Right));
        assert!(p.effort.is_none(), "no-tier model must stay None");
        assert!(
            !p.effort_dirty,
            "navigation that no-ops must not mark effort dirty",
        );
    }

    #[test]
    fn left_arrow_walks_effort_backward_with_wrap() {
        // Backward branch in `cycle_effort` has different arithmetic from the forward branch
        // — pin it independently. Cycle Left until the effort returns to the initial pick.
        let mut p = picker();
        p.handle_key(&key(KeyCode::Right)); // arm the axis with a known starting tier
        let initial = p.effort.expect("Opus 4.7 has an effort axis");
        for _ in 0..16 {
            p.handle_key(&key(KeyCode::Left));
            if p.effort == Some(initial) {
                return;
            }
        }
        panic!(
            "Left-arrow cycle never returned to the starting tier; got {:?}",
            p.effort
        );
    }

    #[test]
    fn navigating_from_no_tier_back_to_tier_model_restores_effort() {
        let mut p = picker();
        p.handle_key(&key(KeyCode::Char('5'))); // jump to Haiku
        assert!(p.effort.is_none(), "Haiku has no effort tier");
        p.handle_key(&key(KeyCode::Up)); // back to Sonnet 4.6 [1m] (index 3)
        assert!(
            p.effort.is_some(),
            "tier model must restore effort via effort_or_active fallback",
        );
    }

    #[test]
    fn enter_with_no_changes_returns_cancelled() {
        // Open + Enter without touching anything is the same shape as
        // Esc — nothing to dispatch.
        let mut p = picker();
        let outcome = p.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, ModalKey::Cancelled));
    }

    #[test]
    fn enter_after_model_change_emits_swap_with_model_only() {
        let mut p = picker();
        p.handle_key(&key(KeyCode::Down));
        let outcome = p.handle_key(&key(KeyCode::Enter));
        match outcome {
            ModalKey::Submitted(ModalAction::User(UserAction::SwapConfig { model, effort })) => {
                assert_eq!(
                    model.map(ResolvedModelId::into_inner).as_deref(),
                    Some("claude-opus-4-7[1m]"),
                );
                assert!(
                    effort.is_none(),
                    "effort must NOT be set when only the model axis moved",
                );
            }
            other => panic!("expected Submitted(SwapConfig {{ model: Some, .. }}), got {other:?}"),
        }
    }

    #[test]
    fn enter_after_effort_change_emits_swap_with_effort_only() {
        // Opus 4.7 active + High; cycle effort Forward once → Xhigh.
        let mut p = picker();
        p.handle_key(&key(KeyCode::Right));
        let outcome = p.handle_key(&key(KeyCode::Enter));
        match outcome {
            ModalKey::Submitted(ModalAction::User(UserAction::SwapConfig { model, effort })) => {
                assert!(
                    model.is_none(),
                    "model must NOT be set when only the effort axis moved",
                );
                assert_eq!(effort, Some(Effort::Xhigh));
            }
            other => panic!("expected Submitted with effort-only SwapConfig, got {other:?}"),
        }
    }

    // ── height ──

    #[test]
    fn height_drops_when_highlighted_model_lacks_effort_tier() {
        // The effort row + spacer (2 rows) only render when the highlighted model has an
        // effort tier. Pin the no-tier path so a regression that always reserves the row
        // fails here.
        let mut p = picker();
        let with_tier = p.height(80);
        p.handle_key(&key(KeyCode::Char('5'))); // jump to Haiku 4.5
        let no_tier = p.height(80);
        assert_eq!(
            with_tier.saturating_sub(no_tier),
            2,
            "no-tier model drops exactly the effort row + spacer",
        );
    }

    // ── render ──

    #[test]
    fn render_runs_at_typical_widths_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        // Two cursor positions: an effort-tier model (Opus 4.7) so the effort row renders, and
        // a no-tier model (Haiku 4.5) so the hide branch executes.
        for setup in [
            None,                     // Opus 4.7 — has effort tier
            Some(KeyCode::Char('5')), // Haiku 4.5 — no effort tier
        ] {
            let mut p = picker();
            if let Some(jump) = setup {
                p.handle_key(&key(jump));
            }
            for width in [40_u16, 80, 120] {
                let h = p.height(width).min(20);
                let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
                terminal
                    .draw(|frame| {
                        p.render(frame, Rect::new(0, 0, width, h), &theme);
                    })
                    .expect("render must not panic");
            }
        }
    }
}
