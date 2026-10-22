//! `/mode` opens the picker, or switches with `/mode <mode>`, which [`command::mode`] parses. Both
//! forms run mid-turn, and the switch applies from the next tool call the gate weighs.

use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;

use ox_core::command;
use ox_core::permission::Mode;
use ox_core::protocol::Op;

use super::context::SlashContext;
use super::matcher::rank_by_prefix;
use super::registry::{ArgCompletion, SlashCommand, SlashKind, SlashOutcome};
use crate::action::Action;
use crate::modal::chrome::CONFIRM_HINTS;
use crate::modal::list_picker::{ListPicker, PickerItem, numeric_hint};
use crate::modal::{Modal, ModalAction, ModalKey};
use crate::theme::Theme;

// ── ModeCmd ──

pub(super) struct ModeCmd;

impl SlashCommand for ModeCmd {
    fn name(&self) -> &'static str {
        "mode"
    }

    fn description(&self) -> &'static str {
        "Switch the permission mode"
    }

    fn classify(&self, args: &str) -> SlashKind {
        if args.is_empty() {
            SlashKind::ReadOnly
        } else {
            SlashKind::Steering
        }
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.is_empty()
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<mode>]")
    }

    fn complete_arg(&self, prefix: &str) -> Vec<ArgCompletion> {
        rank_by_prefix(&Mode::ALL, prefix, |mode| mode.as_str())
            .into_iter()
            .map(|mode| ArgCompletion {
                value: Cow::Borrowed(mode.as_str()),
                description: Cow::Borrowed(mode.description()),
            })
            .collect()
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        if args.is_empty() {
            return Ok(SlashOutcome::Modal(Box::new(ModePicker::new(
                ctx.info.config.permission_mode,
            ))));
        }
        Ok(SlashOutcome::Forward(Action::Op(command::mode(args)?)))
    }
}

// ── PickerItem ──

struct ModeRow {
    mode: Mode,
    is_active: bool,
    hint: Option<char>,
}

impl PickerItem for ModeRow {
    fn label(&self) -> &str {
        self.mode.as_str()
    }

    fn description(&self) -> Option<&str> {
        Some(self.mode.description())
    }

    fn is_active(&self) -> bool {
        self.is_active
    }

    fn key_hint(&self) -> Option<char> {
        self.hint
    }
}

// ── ModePicker ──

struct ModePicker {
    list: ListPicker<ModeRow>,
    /// Mode active at open. Enter on its row cancels, since switching to it would be a no-op.
    current: Mode,
}

impl ModePicker {
    fn new(current: Mode) -> Self {
        let rows = Mode::ALL
            .iter()
            .enumerate()
            .map(|(idx, &mode)| ModeRow {
                mode,
                is_active: mode == current,
                hint: numeric_hint(idx),
            })
            .collect();
        let mut list = ListPicker::new("Select permission mode", rows).with_description(
            "Switch the permission mode for this session. Restarting returns to your config.",
        );
        list.cursor_to(|row| row.is_active);
        Self { list, current }
    }

    fn submit(&self) -> ModalKey {
        let mode = self.list.selected().mode;
        if mode == self.current {
            return ModalKey::Cancelled;
        }
        ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::SetMode(mode))))
    }
}

impl Modal for ModePicker {
    fn height(&self, width: u16, max_height: u16) -> u16 {
        self.list.height_with_footer(width, max_height)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        self.list
            .render_with_footer(frame, area, &CONFIRM_HINTS, theme);
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        if event.code == KeyCode::Enter {
            return self.submit();
        }
        self.list.navigate(event);
        ModalKey::Consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modal::testing::render_rows;
    use crate::slash::registry::testing::run_execute;

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(ModeCmd.name(), "mode");
        assert_ne!(ModeCmd.description(), "");
        assert_eq!(ModeCmd.usage(), Some("[<mode>]"));
    }

    mod classify {
        use super::*;

        #[test]
        fn opens_the_picker_read_only_and_steers_with_a_mode() {
            assert_eq!(ModeCmd.classify(""), SlashKind::ReadOnly);
            assert_eq!(ModeCmd.classify("plan"), SlashKind::Steering);
        }
    }

    mod complete_arg {
        use super::*;

        #[test]
        fn ranks_the_modes_by_prefix_with_their_descriptions() {
            let rows = |prefix| -> Vec<(String, String)> {
                ModeCmd
                    .complete_arg(prefix)
                    .into_iter()
                    .map(|c| (c.value.into_owned(), c.description.into_owned()))
                    .collect()
            };

            assert_eq!(
                rows("")
                    .into_iter()
                    .map(|(value, _)| value)
                    .collect::<Vec<_>>(),
                ["manual", "plan", "yolo"]
            );
            assert_eq!(
                rows("Y"),
                [("yolo".to_owned(), Mode::Yolo.description().to_owned())]
            );
        }
    }

    mod execute {
        use super::*;

        #[test]
        fn no_args_opens_the_picker() {
            run_execute(&ModeCmd, "").unwrap().into_modal();
        }

        #[test]
        fn with_a_mode_forwards_the_switch() {
            assert_eq!(
                run_execute(&ModeCmd, " Yolo ").unwrap().into_forward(),
                Action::Op(Op::SetMode(Mode::Yolo)),
            );
        }

        #[test]
        fn unknown_mode_errors_listing_valid_options() {
            assert_eq!(
                run_execute(&ModeCmd, "turbo").unwrap_err(),
                "Unknown mode: `turbo`. Valid: manual, plan, yolo."
            );
        }
    }

    mod mode_picker {
        use super::*;

        mod render {
            use super::*;

            #[test]
            fn marks_the_current_mode_above_the_footer() {
                let picker = ModePicker::new(Mode::Plan);

                let rows = render_rows(&picker, 80, picker.height(80, 20));

                let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
                assert_eq!(
                    rows,
                    [
                        "Select permission mode",
                        "Switch the permission mode for this session. Restarting returns to your config.",
                        "",
                        "  1. manual    Ask before tool calls that no permission rule allows or denies",
                        "> 2. plan    ✓ Read-only: deny edits and commands",
                        "  3. yolo      Run tool calls without asking unless a rule denies or asks",
                        "",
                        "Enter to confirm  ·  Esc to cancel",
                    ]
                );
            }
        }

        mod handle_key {
            use super::*;

            fn submitted(current: Mode, keys: &[KeyCode]) -> Option<Op> {
                let mut picker = ModePicker::new(current);
                for &code in keys {
                    let outcome = picker.handle_key(&KeyEvent::from(code));
                    assert!(matches!(outcome, ModalKey::Consumed));
                }
                match picker.handle_key(&KeyEvent::from(KeyCode::Enter)) {
                    ModalKey::Submitted(ModalAction::Dispatch(Action::Op(op))) => Some(op),
                    ModalKey::Cancelled => None,
                    other => panic!("expected a submit or a cancel, got {other:?}"),
                }
            }

            #[test]
            fn enter_switches_to_the_row_under_the_cursor() {
                assert_eq!(
                    submitted(Mode::Manual, &[KeyCode::Down]),
                    Some(Op::SetMode(Mode::Plan))
                );
                assert_eq!(
                    submitted(Mode::Manual, &[KeyCode::Char('k')]),
                    Some(Op::SetMode(Mode::Yolo))
                );
                assert_eq!(
                    submitted(Mode::Yolo, &[KeyCode::Char('1')]),
                    Some(Op::SetMode(Mode::Manual))
                );
            }

            #[test]
            fn enter_on_the_current_mode_cancels() {
                assert_eq!(submitted(Mode::Plan, &[]), None);
                assert_eq!(
                    submitted(
                        Mode::Plan,
                        &[KeyCode::Char('j'), KeyCode::Char('x'), KeyCode::Up]
                    ),
                    None
                );
            }
        }
    }
}
