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
use crate::model::{MODELS, ModelInfo, ResolvedModelId};
use crate::tui::modal::chrome::{CONFIRM_HINT, FOOTER_HEIGHT, render_footer};
use crate::tui::modal::list_picker::{ListPicker, PickerItem, numeric_hint};
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;
use crate::tui::viewport::{wrap_next, wrap_prev};

// ── Constants ──

/// Rows pinned below the list: a spacer, the effort row, and the footer.
const BELOW_LIST_HEIGHT: u16 = 2 + FOOTER_HEIGHT;

// ── PickerItem ──

struct ModelRow {
    info: &'static ModelInfo,
    is_active: bool,
    hint: Option<char>,
}

impl ModelRow {
    fn build(active_id: &str) -> Vec<Self> {
        MODELS
            .iter()
            .enumerate()
            .map(|(idx, info)| Self {
                info,
                is_active: info.id == active_id,
                hint: numeric_hint(idx),
            })
            .collect()
    }
}

impl PickerItem for ModelRow {
    fn label(&self) -> &str {
        self.info.id
    }
    fn description(&self) -> Option<&str> {
        Some(self.info.display_name)
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
    /// Active model at open. Compared on submit to detect a model-axis change.
    active_model: String,
    /// Active effort at open. A model swap carries it over, so it seeds every row's effort axis.
    active_effort: Option<Effort>,
    /// Set by Left / Right. `None` until the user touches the effort axis.
    picked_effort: Option<Effort>,
}

impl ModelEffortPicker {
    /// Infallible because every catalogue row has an effort axis.
    pub(super) fn new(info: &LiveSessionInfo) -> Self {
        let active_model = info.config.model.id().to_owned();
        let rows = ModelRow::build(&active_model);

        let mut list = ListPicker::new("Select model", rows).with_description(
            "Switch the active model for this session. Restarting returns to your config.",
        );
        list.select_initial(|row| row.is_active);

        Self {
            list,
            active_model,
            active_effort: info.config.effort,
            picked_effort: None,
        }
    }

    /// Effort the highlighted model would run at if submitted now.
    fn shown_effort(&self) -> Option<Effort> {
        let row = self.list.selected()?;
        Some(
            self.picked_effort
                .or(self.active_effort)
                .unwrap_or(row.info.default_effort),
        )
    }

    fn cycle_effort(&mut self, direction: Direction) {
        let Some(current) = self.shown_effort() else {
            return;
        };
        let len = Effort::ALL.len();
        let idx = Effort::ALL
            .iter()
            .position(|level| *level == current)
            .expect("Effort::ALL lists every level");
        let next = match direction {
            Direction::Forward => wrap_next(idx, len),
            Direction::Backward => wrap_prev(idx, len),
        };
        self.picked_effort = Some(Effort::ALL[next]);
    }

    /// Commit both axes through one atomic `SwapConfig`. Each axis is `Some` only when it differs
    /// from the open-time state, so a no-touch Enter (or a pick that returns to the active effort)
    /// cancels without re-resolving config defaults.
    fn submit(&self) -> ModalKey {
        let model = self
            .list
            .selected()
            .map_or_else(|| self.active_model.clone(), |row| row.info.id.to_owned());
        let model_changed = model != self.active_model;
        let effort = self
            .picked_effort
            .filter(|pick| Some(*pick) != self.active_effort);

        if !model_changed && effort.is_none() {
            return ModalKey::Cancelled;
        }
        ModalKey::Submitted(ModalAction::User(UserAction::SwapConfig {
            model: model_changed.then(|| ResolvedModelId::new(model)),
            effort,
        }))
    }

    fn render_effort_row(&self, theme: &Theme) -> Option<Line<'static>> {
        let level = self.shown_effort()?;
        let was_default = self.picked_effort.is_none() && self.active_effort.is_none();
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
    fn height(&self, width: u16, max_height: u16) -> u16 {
        self.list
            .height(width, max_height.saturating_sub(BELOW_LIST_HEIGHT))
            + BELOW_LIST_HEIGHT
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let list_h = area.height.saturating_sub(BELOW_LIST_HEIGHT);
        self.list.render(
            frame,
            Rect {
                height: list_h,
                ..area
            },
            theme,
        );

        let effort_y = area.y + list_h + 1;
        if let Some(line) = self.render_effort_row(theme)
            && effort_y < area.bottom()
        {
            let row = Rect {
                y: effort_y,
                height: 1,
                ..area
            };
            frame.render_widget(Paragraph::new(line).style(theme.surface()), row);
        }
        render_footer(
            frame,
            area,
            Line::from(Span::styled(CONFIRM_HINT, theme.dim())),
            theme,
        );
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        match event.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.list.select_prev();
                ModalKey::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.list.select_next();
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
                self.list.select_by_hint(c);
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::model::ModelSpec;
    use crate::slash::test_session_info;

    /// `test_session_info` ships Opus 5.5 active at `high`.
    fn picker() -> ModelEffortPicker {
        ModelEffortPicker::new(&test_session_info())
    }

    fn picker_on_unknown_model() -> ModelEffortPicker {
        let mut info = test_session_info();
        info.config.model = ModelSpec::resolve("custom-model".to_owned());
        info.config.effort = None;
        ModelEffortPicker::new(&info)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn submitted_swap(outcome: ModalKey) -> (Option<String>, Option<Effort>) {
        match outcome {
            ModalKey::Submitted(ModalAction::User(UserAction::SwapConfig { model, effort })) => {
                (model.map(ResolvedModelId::into_inner), effort)
            }
            other => panic!("expected Submitted SwapConfig, got {other:?}"),
        }
    }

    // ── new ──

    #[test]
    fn new_lists_catalogue_and_positions_cursor_on_active_model() {
        let p = picker();
        let row = p.list.selected().expect("active row");
        assert_eq!(row.info.id, "claude-opus-5-5");
        assert!(row.is_active);
        assert_eq!(p.shown_effort(), Some(Effort::High));
    }

    #[test]
    fn new_on_unknown_model_shows_highlighted_row_default() {
        let mut p = picker_on_unknown_model();
        assert_eq!(p.shown_effort(), Some(Effort::Medium), "Opus 5.5 default");
        p.handle_key(&key(KeyCode::Char('2')));
        assert_eq!(p.shown_effort(), Some(Effort::High), "Sonnet 5.5 default");
    }

    // ── handle_key ──

    #[test]
    fn numeric_jump_routes_cursor_to_matching_row() {
        let mut p = picker();
        p.handle_key(&key(KeyCode::Char('3')));
        let row = p.list.selected().expect("selected row");
        assert_eq!(row.info.id, "claude-haiku-5-5");
    }

    #[test]
    fn right_and_left_walk_the_full_ladder_with_wrap() {
        let mut p = picker();
        p.handle_key(&key(KeyCode::Right));
        assert_eq!(p.shown_effort(), Some(Effort::Xhigh));
        p.handle_key(&key(KeyCode::Right));
        p.handle_key(&key(KeyCode::Right));
        assert_eq!(p.shown_effort(), Some(Effort::Low), "Right wraps past max");
        p.handle_key(&key(KeyCode::Left));
        assert_eq!(p.shown_effort(), Some(Effort::Max), "Left wraps past low");
    }

    #[test]
    fn effort_pick_survives_cursor_moves() {
        let mut p = picker();
        p.handle_key(&key(KeyCode::Left));
        p.handle_key(&key(KeyCode::Down));
        assert_eq!(p.shown_effort(), Some(Effort::Medium));
    }

    #[test]
    fn enter_with_no_changes_returns_cancelled() {
        let mut p = picker();
        let outcome = p.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, ModalKey::Cancelled));
    }

    #[test]
    fn enter_after_effort_round_trip_returns_cancelled() {
        let mut p = picker();
        p.handle_key(&key(KeyCode::Right));
        p.handle_key(&key(KeyCode::Left));
        let outcome = p.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, ModalKey::Cancelled));
    }

    #[test]
    fn enter_after_model_change_emits_swap_with_model_only() {
        let mut p = picker();
        p.handle_key(&key(KeyCode::Down));
        let (model, effort) = submitted_swap(p.handle_key(&key(KeyCode::Enter)));
        assert_eq!(model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(effort, None, "effort stays unset when only the model moved");
    }

    #[test]
    fn enter_after_effort_change_emits_swap_with_effort_only() {
        let mut p = picker();
        p.handle_key(&key(KeyCode::Right));
        let (model, effort) = submitted_swap(p.handle_key(&key(KeyCode::Enter)));
        assert_eq!(model, None, "model stays unset when only the effort moved");
        assert_eq!(effort, Some(Effort::Xhigh));
    }

    #[test]
    fn enter_on_unknown_model_without_touch_swaps_model_only() {
        // The highlighted row's default is what a model-only swap resolves to, so it is not sent.
        let mut p = picker_on_unknown_model();
        let (model, effort) = submitted_swap(p.handle_key(&key(KeyCode::Enter)));
        assert_eq!(model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(effort, None);
    }

    // ── render ──

    #[test]
    fn render_effort_row_marks_default_only_without_active_effort() {
        let theme = Theme::default();
        let row_text = |p: &ModelEffortPicker| -> String {
            p.render_effort_row(&theme)
                .expect("effort row")
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        assert!(row_text(&picker()).starts_with("● high effort  "));
        assert!(row_text(&picker_on_unknown_model()).starts_with("● medium effort (default)"));
    }

    #[test]
    fn render_runs_at_typical_widths_without_panicking() {
        let theme = Theme::default();
        let p = picker();
        for width in [40_u16, 80, 120] {
            let h = p.height(width, 20);
            let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
            terminal
                .draw(|frame| {
                    p.render(frame, Rect::new(0, 0, width, h), &theme);
                })
                .expect("render must not panic");
        }
    }

    #[test]
    fn render_pins_the_effort_row_and_footer_below_a_scrolled_list() {
        let theme = Theme::default();
        let last = MODELS.len() - 1;
        let mut p = picker();
        for _ in 0..last {
            p.handle_key(&key(KeyCode::Down));
        }
        let (width, height) = (60, 8);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| p.render(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();

        let buf = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        };
        let cursor_row = row(3);
        assert!(
            cursor_row.contains(MODELS[last].id),
            "cursor row: {cursor_row:?}"
        );
        assert!(row(5).contains("effort"), "effort row: {:?}", row(5));
        assert_eq!(row(7), CONFIRM_HINT);
    }
}
