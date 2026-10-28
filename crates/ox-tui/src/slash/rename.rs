//! `/rename` sets the session title, from a modal pre-filled with the current title or directly
//! as `/rename <title>`. A manual title stops AI titles for the rest of the session, so a late one
//! cannot overwrite it.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use ox_core::command;
use ox_core::session::display::MAX_TITLE_WIDTH;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::action::Action;
use crate::modal::chrome::{FOOTER_HEIGHT, Header, render_footer};
use crate::modal::line_editor::LineEditor;
use crate::modal::{Modal, ModalAction, ModalKey};
use crate::theme::Theme;

// ── Constants ──

const TITLE: &str = "Rename session";
const FOOTER_HINTS: [&str; 2] = ["Enter to save", "Esc to cancel"];

// ── RenameCmd ──

pub(super) struct RenameCmd;

impl SlashCommand for RenameCmd {
    fn name(&self) -> &'static str {
        "rename"
    }

    fn description(&self) -> &'static str {
        "Rename the current session"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<title>]")
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.is_empty()
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        if args.is_empty() {
            return Ok(SlashOutcome::Modal(Box::new(RenameModal::new(ctx.title))));
        }
        Ok(SlashOutcome::Forward(Action::Op(command::rename(args)?)))
    }
}

// ── RenameModal ──

struct RenameModal {
    header: Header,
    editor: LineEditor,
}

impl RenameModal {
    fn new(initial: Option<&str>) -> Self {
        Self {
            header: Header::new(TITLE),
            editor: LineEditor::new(initial.unwrap_or_default()).with_max_width(MAX_TITLE_WIDTH),
        }
    }

    fn submit(&self) -> ModalKey {
        let Ok(op) = command::rename(self.editor.text()) else {
            return ModalKey::Consumed;
        };
        ModalKey::Submitted(ModalAction::Dispatch(Action::Op(op)))
    }
}

impl Modal for RenameModal {
    fn height(&mut self, _width: u16, _max_height: u16) -> u16 {
        self.header.height() + 1 + FOOTER_HEIGHT
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let mut lines = Vec::new();
        self.header.push_lines(&mut lines, area.width, theme);
        lines.push(self.editor.line(area.width, theme));
        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
        render_footer(frame, area, &FOOTER_HINTS, theme);

        let input_y = self.header.height();
        if input_y < area.height {
            self.editor.place_cursor(frame, area, area.y + input_y);
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        if event.code == KeyCode::Enter {
            return self.submit();
        }
        self.editor.handle_key(event);
        ModalKey::Consumed
    }

    fn handle_paste(&mut self, text: &str) {
        self.editor.paste(text);
    }

    fn binds_modified_key(&self, event: &KeyEvent) -> bool {
        LineEditor::binds_modified_key(event)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use ox_core::protocol::Op;

    use super::*;
    use crate::modal::ModalStack;
    use crate::modal::testing::render_modal;
    use crate::session_info::testing::test_session_info;
    use crate::slash::registry::testing::run_execute;

    mod classify {
        use super::*;

        #[test]
        fn is_always_mutating() {
            assert_eq!(RenameCmd.classify(""), SlashKind::Mutating);
            assert_eq!(RenameCmd.classify("New Title"), SlashKind::Mutating);
        }
    }

    mod echoes_input {
        use super::*;

        #[test]
        fn only_for_typed_arg_form() {
            assert!(!RenameCmd.echoes_input(""));
            assert!(RenameCmd.echoes_input("New Title"));
        }
    }

    mod execute {
        use super::*;

        #[test]
        fn typed_arg_forwards_rename_action_with_the_title() {
            assert_eq!(
                run_execute(&RenameCmd, "Fix auth bug")
                    .unwrap()
                    .into_forward(),
                Action::Op(Op::Rename {
                    title: "Fix auth bug".to_owned(),
                }),
            );
        }

        #[test]
        fn bare_form_opens_modal_pre_filled_with_current_title() {
            let info = test_session_info();
            let mut modal = RenameCmd
                .execute("", &SlashContext::new(&info, Some("Existing title")))
                .unwrap()
                .into_modal();
            let result = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
            let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Rename { title }))) =
                result
            else {
                panic!("Enter on pre-filled buffer must submit, got {result:?}");
            };
            assert_eq!(title, "Existing title");
        }

        #[test]
        fn bare_form_with_no_current_title_opens_empty_modal() {
            let mut modal = run_execute(&RenameCmd, "").unwrap().into_modal();
            let result = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
            assert!(
                matches!(result, ModalKey::Consumed),
                "empty Enter must not submit, got {result:?}",
            );
        }
    }

    mod rename_modal {
        use super::*;

        mod render {
            use super::*;

            #[test]
            fn lays_out_title_prompt_and_footer_in_order() {
                let mut modal = RenameModal::new(Some("Fix auth"));
                let rows = render_modal(&mut modal, 40, u16::MAX);

                let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
                assert_eq!(
                    rows,
                    [
                        "Rename session",
                        "",
                        "/ Fix auth",
                        "",
                        "Enter to save  ·  Esc to cancel",
                    ]
                );
            }

            #[test]
            fn scrolls_a_title_wider_than_the_row_to_its_tail() {
                let long = format!("{}tail", "x".repeat(60));
                let mut modal = RenameModal::new(Some(&long));
                let rows = render_modal(&mut modal, 20, u16::MAX);

                let input = &rows[2];
                assert_eq!(input.trim_end(), format!("/ {}tail", "x".repeat(13)));
            }

            #[test]
            fn places_cursor_after_visible_buffer_text() {
                let theme = Theme::default();
                let mut modal = RenameModal::new(Some("ab"));
                let width: u16 = 40;
                let h = modal.height(width, u16::MAX);
                let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
                terminal
                    .draw(|frame| modal.render(frame, Rect::new(0, 0, width, h), &theme))
                    .unwrap();
                let pos = terminal.get_cursor_position().unwrap();

                // The input row sits below the title and a gap, and the cursor lands after "/ ab".
                assert_eq!(
                    (pos.x, pos.y),
                    (4, 2),
                    "cursor must land after the rendered buffer",
                );
            }

            #[test]
            fn clamps_cursor_when_buffer_overflows_visible_row() {
                let theme = Theme::default();
                let long: String = "x".repeat(100);
                let mut modal = RenameModal::new(Some(&long));
                let width: u16 = 20;
                let h = modal.height(width, u16::MAX);
                let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
                terminal
                    .draw(|frame| modal.render(frame, Rect::new(0, 0, width, h), &theme))
                    .unwrap();
                let pos = terminal.get_cursor_position().unwrap();

                assert_eq!(pos.x, width - 1, "cursor must clamp to the right edge");
            }

            #[test]
            fn skips_cursor_when_area_too_short_for_input_row() {
                let theme = Theme::default();
                let mut modal = RenameModal::new(Some("ab"));
                let width: u16 = 40;
                let mut terminal = Terminal::new(TestBackend::new(width, 5)).unwrap();
                terminal
                    .draw(|frame| modal.render(frame, Rect::new(0, 0, width, 1), &theme))
                    .expect("short-area render must not panic");

                assert!(
                    !terminal.backend().cursor_visible(),
                    "a hidden input row places no cursor",
                );
            }
        }

        mod handle_key {
            use super::*;

            #[test]
            fn typing_appends_to_buffer_and_enter_submits_trimmed_title() {
                let mut modal = RenameModal::new(None);
                for ch in "  Fix auth  ".chars() {
                    assert!(matches!(
                        modal.handle_key(&KeyEvent::from(KeyCode::Char(ch))),
                        ModalKey::Consumed,
                    ));
                }
                let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
                let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Rename { title }))) =
                    outcome
                else {
                    panic!("trim happens at submit time, got {outcome:?}");
                };
                assert_eq!(title, "Fix auth");
            }

            #[test]
            fn forwards_modified_keys_and_paste_through_the_stack_to_the_line_editor() {
                let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
                let mut stack = ModalStack::default();
                stack.push(Box::new(RenameModal::new(Some("Fix the bug"))));

                for event in [ctrl('w'), KeyEvent::from(KeyCode::Char('!')), ctrl('a')] {
                    let outcome = stack.handle_key(&event);
                    assert!(outcome.is_none(), "{event:?} returned {outcome:?}");
                }
                stack.handle_paste("Re: ");
                let action = stack.handle_key(&KeyEvent::from(KeyCode::Enter));

                let Some(ModalAction::Dispatch(Action::Op(Op::Rename { title }))) = action else {
                    panic!("expected a rename, got {action:?}");
                };
                assert_eq!(title, "Re: Fix the !");
            }

            #[test]
            fn caps_the_title_at_the_session_title_width() {
                let mut modal = RenameModal::new(Some(&"x".repeat(MAX_TITLE_WIDTH + 20)));

                modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));
                let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));

                let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Rename { title }))) =
                    outcome
                else {
                    panic!("expected Submitted, got {outcome:?}");
                };
                assert_eq!(title, "x".repeat(MAX_TITLE_WIDTH));
            }

            #[test]
            fn blank_only_buffer_does_not_submit() {
                let mut modal = RenameModal::new(None);
                for _ in 0..3 {
                    modal.handle_key(&KeyEvent::from(KeyCode::Char(' ')));
                }
                let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
                assert!(
                    matches!(outcome, ModalKey::Consumed),
                    "all-whitespace Enter must not submit, got {outcome:?}",
                );
            }
        }
    }
}
