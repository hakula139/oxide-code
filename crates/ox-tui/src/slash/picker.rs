//! Model and effort picker for bare `/model`. Both axes commit through one [`Op::SetModel`].

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use ox_core::config::{DEFAULT_MODEL, Effort};
use ox_core::model::{MODELS, ModelInfo, ResolvedModelId};
use ox_core::protocol::{ConfigPick, Op};

use super::context::LiveSessionInfo;
use super::effort_slider::tier_color;
use crate::action::Action;
use crate::glyphs::RADIO_SELECTED;
use crate::modal::chrome::{CONFIRM_HINTS, FOOTER_HEIGHT, KEY_LEFT_RIGHT, render_footer};
use crate::modal::list_picker::{ListPicker, PickerItem, numeric_hint};
use crate::modal::{Modal, ModalAction, ModalKey};
use crate::theme::Theme;
use crate::viewport::{wrap_next, wrap_prev};

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
    /// Active model at open.
    active_model: String,
    /// Active effort at open. A model swap carries it over, so it seeds every row's effort axis.
    active_effort: Option<Effort>,
    /// Set by Left / Right. `None` until the user touches the effort axis.
    picked_effort: Option<Effort>,
}

impl ModelEffortPicker {
    /// Infallible because every catalog row has an effort axis. A session on an ID outside the
    /// catalog opens on the default model's row.
    pub(super) fn new(info: &LiveSessionInfo) -> Self {
        let model = &info.config.model;
        let active_model = model.id().to_owned();
        let rows = ModelRow::build(&active_model);

        let mut list = ListPicker::new("Select model", rows).with_description(
            "Switch the active model for this session. Restarting returns to your config.",
        );
        let initial = model.info().map_or(DEFAULT_MODEL, |row| row.id);
        list.cursor_to(|row| row.info.id == initial);

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

    /// Commits both axes through one atomic `SetModel`, picking only the axes that differ from the
    /// open-time state. A no-touch Enter, or a pick that returns to the active effort, cancels
    /// without re-resolving config defaults.
    fn submit(&self) -> ModalKey {
        let model = self
            .list
            .selected()
            .map_or_else(|| self.active_model.clone(), |row| row.info.id.to_owned());
        let effort = self
            .picked_effort
            .filter(|pick| Some(*pick) != self.active_effort);

        let pick = if model != self.active_model {
            ConfigPick::Model {
                id: ResolvedModelId::new(model),
                effort,
            }
        } else if let Some(effort) = effort {
            ConfigPick::Effort(effort)
        } else {
            return ModalKey::Cancelled;
        };
        ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::SetModel(pick))))
    }

    fn render_effort_row(&self, theme: &Theme) -> Option<Line<'static>> {
        let level = self.shown_effort()?;
        let was_default = self.picked_effort.is_none() && self.active_effort.is_none();
        let suffix = if was_default { " (default)" } else { "" };
        let color = tier_color(level);
        Some(Line::from(vec![
            Span::styled(format!("{RADIO_SELECTED} "), Style::default().fg(color)),
            Span::styled(
                format!("{level} effort{suffix}"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {KEY_LEFT_RIGHT} to adjust"), theme.dim()),
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
        render_footer(frame, area, &CONFIRM_HINTS, theme);
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

// ── Direction ──

#[derive(Debug, Clone, Copy)]
enum Direction {
    Forward,
    Backward,
}

#[cfg(test)]
mod tests {
    use ox_core::model::ModelSpec;

    use super::*;
    use crate::modal::testing::render_rows;
    use crate::slash::testing::test_session_info;

    /// `test_session_info` ships Opus 5.5 active at `high`.
    fn picker() -> ModelEffortPicker {
        ModelEffortPicker::new(&test_session_info())
    }

    fn picker_on_unknown_model() -> ModelEffortPicker {
        let mut info = test_session_info();
        info.config.model = ModelSpec::resolve("custom-model");
        info.config.effort = None;
        ModelEffortPicker::new(&info)
    }

    mod new {
        use super::*;

        #[test]
        fn lists_catalog_and_positions_cursor_on_active_model() {
            let p = picker();
            let row = p.list.selected().expect("active row");
            assert_eq!(row.info.id, "claude-opus-5-5");
            assert!(row.is_active);
            assert_eq!(p.shown_effort(), Some(Effort::High));
        }

        #[test]
        fn on_unknown_model_highlights_default_model_and_shows_row_default() {
            let mut p = picker_on_unknown_model();

            let row = p.list.selected().expect("default model row");
            assert_eq!(row.info.id, "claude-opus-5-5");
            assert!(!row.is_active);
            assert_eq!(p.shown_effort(), Some(Effort::Medium), "Opus 5.5 default");

            p.handle_key(&KeyEvent::from(KeyCode::Char('1')));
            assert_eq!(p.shown_effort(), Some(Effort::High), "Fable 5.1 default");
        }
    }

    mod render_effort_row {
        use super::*;

        #[test]
        fn marks_default_only_without_active_effort() {
            let theme = Theme::default();
            let row_text = |p: &ModelEffortPicker| {
                p.render_effort_row(&theme).expect("effort row").to_string()
            };

            assert_eq!(row_text(&picker()), "● high effort  ← / → to adjust");
            assert_eq!(
                row_text(&picker_on_unknown_model()),
                "● medium effort (default)  ← / → to adjust"
            );
        }
    }

    mod render {
        use super::*;

        #[test]
        fn lists_models_above_the_effort_row_and_footer() {
            let p = picker();
            let height = p.height(80, 20);

            let rows = render_rows(&p, 80, height);

            let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
            assert_eq!(
                rows,
                [
                    "Select model",
                    "Switch the active model for this session. Restarting returns to your config.",
                    "",
                    "  1. claude-fable-5-1     Claude Fable 5.1",
                    "> 2. claude-opus-5-5    ✓ Claude Opus 5.5",
                    "  3. claude-sonnet-5-5    Claude Sonnet 5.5",
                    "  4. claude-haiku-5-5     Claude Haiku 5.5",
                    "",
                    "● high effort  ← / → to adjust",
                    "",
                    "Enter to confirm  ·  Esc to cancel",
                ]
            );
        }

        #[test]
        fn pins_the_effort_row_and_footer_below_a_scrolled_list() {
            let mut p = picker();
            p.handle_key(&KeyEvent::from(KeyCode::Char('4')));

            let rows = render_rows(&p, 80, 8);

            let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
            assert_eq!(
                rows,
                [
                    "Select model",
                    "Switch the active model for this session. Restarting returns to your config.",
                    "",
                    "> 4. claude-haiku-5-5     Claude Haiku 5.5",
                    "",
                    "● high effort  ← / → to adjust",
                    "",
                    "Enter to confirm  ·  Esc to cancel",
                ]
            );
        }
    }

    mod handle_key {
        use super::*;

        fn submitted_pick(outcome: ModalKey) -> ConfigPick {
            match outcome {
                ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::SetModel(pick)))) => pick,
                other => panic!("expected Submitted SetModel, got {other:?}"),
            }
        }

        fn model_only(id: &str) -> ConfigPick {
            ConfigPick::Model {
                id: ResolvedModelId::new(id),
                effort: None,
            }
        }

        #[test]
        fn numeric_jump_routes_cursor_to_matching_row() {
            let mut p = picker();
            p.handle_key(&KeyEvent::from(KeyCode::Char('4')));
            let row = p.list.selected().expect("selected row");
            assert_eq!(row.info.id, "claude-haiku-5-5");
        }

        #[test]
        fn right_and_left_walk_the_full_ladder_with_wrap() {
            let mut p = picker();
            p.handle_key(&KeyEvent::from(KeyCode::Right));
            assert_eq!(p.shown_effort(), Some(Effort::Xhigh));
            p.handle_key(&KeyEvent::from(KeyCode::Right));
            p.handle_key(&KeyEvent::from(KeyCode::Right));
            assert_eq!(p.shown_effort(), Some(Effort::Low), "Right wraps past max");
            p.handle_key(&KeyEvent::from(KeyCode::Left));
            assert_eq!(p.shown_effort(), Some(Effort::Max), "Left wraps past low");
        }

        #[test]
        fn effort_pick_survives_cursor_moves() {
            let mut p = picker();
            p.handle_key(&KeyEvent::from(KeyCode::Left));
            p.handle_key(&KeyEvent::from(KeyCode::Down));
            assert_eq!(p.shown_effort(), Some(Effort::Medium));
        }

        #[test]
        fn enter_with_no_changes_returns_cancelled() {
            let mut p = picker();
            let outcome = p.handle_key(&KeyEvent::from(KeyCode::Enter));
            assert!(matches!(outcome, ModalKey::Cancelled));
        }

        #[test]
        fn enter_after_effort_round_trip_returns_cancelled() {
            let mut p = picker();
            p.handle_key(&KeyEvent::from(KeyCode::Right));
            p.handle_key(&KeyEvent::from(KeyCode::Left));
            let outcome = p.handle_key(&KeyEvent::from(KeyCode::Enter));
            assert!(matches!(outcome, ModalKey::Cancelled));
        }

        #[test]
        fn enter_after_model_change_emits_swap_with_model_only() {
            let mut p = picker();
            p.handle_key(&KeyEvent::from(KeyCode::Down));

            let pick = submitted_pick(p.handle_key(&KeyEvent::from(KeyCode::Enter)));

            assert_eq!(pick, model_only("claude-sonnet-5-5"));
        }

        #[test]
        fn enter_after_effort_change_emits_swap_with_effort_only() {
            let mut p = picker();
            p.handle_key(&KeyEvent::from(KeyCode::Right));

            let pick = submitted_pick(p.handle_key(&KeyEvent::from(KeyCode::Enter)));

            assert_eq!(pick, ConfigPick::Effort(Effort::Xhigh));
        }

        #[test]
        fn enter_after_model_and_effort_changes_emits_swap_with_both() {
            let mut p = picker();
            p.handle_key(&KeyEvent::from(KeyCode::Down));
            p.handle_key(&KeyEvent::from(KeyCode::Right));

            let pick = submitted_pick(p.handle_key(&KeyEvent::from(KeyCode::Enter)));

            assert_eq!(
                pick,
                ConfigPick::Model {
                    id: ResolvedModelId::new("claude-sonnet-5-5"),
                    effort: Some(Effort::Xhigh),
                },
            );
        }

        #[test]
        fn enter_on_unknown_model_without_touch_swaps_model_only() {
            // A model-only swap resolves to the highlighted row's default, so it is not sent.
            let mut p = picker_on_unknown_model();

            let pick = submitted_pick(p.handle_key(&KeyEvent::from(KeyCode::Enter)));

            assert_eq!(pick, model_only("claude-opus-5-5"));
        }
    }
}
