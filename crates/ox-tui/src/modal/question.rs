//! Mid-turn questions with suggested choices and a freeform answer.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use ox_core::protocol::{Event, Op, PromptDeadline};
use ox_core::tool::ask::{Question, QuestionAnswer};
use ox_core::util::text::escape_hidden;

use super::acceptance::Acceptance;
use super::chrome::{KEY_PAGE_UP_DOWN, KEY_UP_DOWN, above, format_deadline, wrap_hints};
use super::line_editor::LineEditor;
use super::{EngineUpdate, Modal, ModalAction, ModalKey, UpdateReply};
use crate::action::Action;
use crate::glyphs::cursor_gutter;
use crate::keymap::{KeyChord, hint, is_unmodified};
use crate::saturating_u16;
use crate::theme::Theme;
use crate::viewport::Viewport;
use crate::wrap::wrap_line;

const SKIP_KEY: KeyChord = KeyChord::ctrl('s');

pub(crate) struct QuestionModal {
    id: String,
    question: Question,
    selected: usize,
    editor: LineEditor,
    acceptance: Acceptance,
    viewport: Viewport,
    body_len: usize,
    focus_changed: bool,
    deadline: Option<PromptDeadline>,
}

impl QuestionModal {
    pub(crate) fn new(id: String, question: Question) -> Self {
        Self {
            id,
            selected: question.recommended.unwrap_or(0),
            question,
            editor: LineEditor::new(""),
            acceptance: Acceptance::default(),
            viewport: Viewport::default(),
            body_len: 0,
            focus_changed: true,
            deadline: None,
        }
    }

    pub(crate) fn with_deadline(mut self, deadline: Option<PromptDeadline>) -> Self {
        self.deadline = deadline;
        self
    }

    fn answer(&self, answer: QuestionAnswer) -> ModalAction {
        ModalAction::Dispatch(Action::Op(Op::AnswerQuestion {
            id: self.id.clone(),
            answer,
        }))
    }

    fn body(&self, width: u16, theme: Option<&Theme>) -> (Vec<Line<'static>>, usize) {
        let width = usize::from(width).max(1);
        let accent = theme.map_or_else(Style::default, Theme::accent);
        let text = theme.map_or_else(Style::default, Theme::text);
        let dim = theme.map_or_else(Style::default, Theme::dim);
        let mut lines = wrap_line(
            Line::from(Span::styled(escape_hidden(&self.question.question), accent)),
            width,
            0,
            None,
        );
        lines.push(Line::default());
        let mut selected_row = lines.len();
        for (index, option) in self.question.options.iter().enumerate() {
            if index == self.selected {
                selected_row = lines.len();
            }
            let gutter = cursor_gutter(index == self.selected);
            let recommended = if self.question.recommended == Some(index) {
                " (recommended)"
            } else {
                ""
            };
            let label = escape_hidden(&option.label);
            let style = if index == self.selected { accent } else { text };
            lines.extend(wrap_line(
                Line::from(Span::styled(format!("{gutter}{label}{recommended}"), style)),
                width,
                2,
                None,
            ));
            if let Some(description) = &option.description {
                lines.extend(wrap_line(
                    Line::from(Span::styled(escape_hidden(description), dim)),
                    width,
                    2,
                    None,
                ));
            }
        }
        (lines, selected_row)
    }

    fn footer_rows(&self, width: u16, overflow: bool) -> Vec<String> {
        let width = usize::from(width).max(1);
        let mut rows = self
            .deadline
            .into_iter()
            .flat_map(|deadline| wrap_line(Line::from(format_deadline(deadline)), width, 0, None))
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        let mut hints = vec![
            hint(KEY_UP_DOWN, "choose"),
            hint(KeyChord::plain(KeyCode::Tab), "type"),
            hint(KeyChord::plain(KeyCode::Enter), "answer"),
            hint(SKIP_KEY, "skip"),
            hint(KeyChord::plain(KeyCode::Esc), "abort"),
        ];
        if overflow {
            hints.push(hint(KEY_PAGE_UP_DOWN, "scroll"));
        }
        rows.extend(wrap_hints(&hints, width));
        rows
    }
}

impl Modal for QuestionModal {
    fn on_engine(&mut self, update: EngineUpdate<'_>) -> UpdateReply {
        match update {
            EngineUpdate::Event(Event::InteractionClosed { id }) if *id == self.id => {
                UpdateReply::Closed(None)
            }
            _ => UpdateReply::Unclaimed,
        }
    }

    fn height(&mut self, width: u16, max_height: u16) -> u16 {
        let rows = self.body(width, None).0.len() + 2 + self.footer_rows(width, false).len();
        saturating_u16(rows).min(max_height)
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let (mut lines, selected_row) = self.body(area.width, Some(theme));
        let editing = self.selected == self.question.options.len();
        lines.push(Line::from(Span::styled("Your answer:", theme.text())));
        let editor_row = lines.len();
        lines.push(self.editor.line(area.width, theme));
        let mut footer = self.footer_rows(area.width, false);
        if lines.len() + footer.len() > usize::from(area.height) {
            footer = self.footer_rows(area.width, true);
        }
        let body_area = above(area, saturating_u16(footer.len()));
        let footer_area = Rect {
            y: body_area.bottom(),
            height: area.height - body_area.height,
            ..area
        };
        let focus = if editing { editor_row } else { selected_row };
        self.body_len = lines.len();
        let cursor = (self.focus_changed && !body_area.is_empty()).then_some(focus);
        let visible = self
            .viewport
            .window(self.body_len, usize::from(body_area.height), cursor);
        if !body_area.is_empty() {
            self.focus_changed = false;
        }
        let scroll = visible.start;
        self.acceptance
            .update(!body_area.is_empty() && visible.contains(&focus));
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((saturating_u16(scroll), 0))
                .style(theme.surface()),
            body_area,
        );
        frame.render_widget(
            Paragraph::new(
                footer
                    .into_iter()
                    .map(|row| Line::from(Span::styled(row, theme.dim())))
                    .collect::<Vec<_>>(),
            )
            .style(theme.surface()),
            footer_area,
        );
        if editing && visible.contains(&editor_row) {
            self.editor.place_cursor(
                frame,
                body_area,
                body_area
                    .y
                    .saturating_add(saturating_u16(editor_row - scroll)),
            );
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        if self.acceptance.guard() {
            return ModalKey::Consumed;
        }
        let choices = self.question.options.len();
        match event.code {
            KeyCode::PageUp | KeyCode::PageDown => {
                let page = isize::try_from(self.viewport.page().saturating_sub(1).max(1))
                    .unwrap_or(isize::MAX);
                let delta = if event.code == KeyCode::PageUp {
                    -page
                } else {
                    page
                };
                self.viewport.scroll_by(delta, self.body_len);
                self.acceptance.reset();
                ModalKey::Consumed
            }
            _ if SKIP_KEY.matches(event) => {
                ModalKey::Submitted(self.answer(QuestionAnswer::Skipped))
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.focus_changed = true;
                self.acceptance.reset();
                ModalKey::Consumed
            }
            KeyCode::Down => {
                self.selected = self.selected.saturating_add(1).min(choices);
                self.focus_changed = true;
                self.acceptance.reset();
                ModalKey::Consumed
            }
            KeyCode::Tab => {
                self.selected = choices;
                self.focus_changed = true;
                self.acceptance.reset();
                ModalKey::Consumed
            }
            KeyCode::Enter if self.acceptance.accepts(event) => {
                let text = self.question.options.get(self.selected).map_or_else(
                    || self.editor.text().to_owned(),
                    |option| option.label.clone(),
                );
                ModalKey::Submitted(self.answer(QuestionAnswer::Answered(text)))
            }
            KeyCode::Enter => ModalKey::Consumed,
            _ if self.selected == choices => {
                self.editor.handle_key(event);
                ModalKey::Consumed
            }
            _ if is_unmodified(event) && matches!(event.code, KeyCode::Char(_)) => {
                self.selected = choices;
                self.focus_changed = true;
                self.editor.handle_key(event);
                ModalKey::Consumed
            }
            _ => ModalKey::Consumed,
        }
    }

    fn handle_paste(&mut self, text: &str) {
        self.selected = self.question.options.len();
        self.focus_changed = true;
        self.editor.paste(text);
    }

    fn binds_modified_key(&self, event: &KeyEvent) -> bool {
        self.acceptance.is_guarded()
            || SKIP_KEY.matches(event)
            || (self.selected == self.question.options.len()
                && LineEditor::binds_modified_key(event))
    }

    fn on_cancel(&mut self) -> Option<ModalAction> {
        Some(self.answer(QuestionAnswer::Aborted))
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use crossterm::event::KeyModifiers;

    use ox_core::protocol::PromptTimeoutAction;
    use ox_core::tool::ask::QuestionOption;

    use super::*;
    use crate::modal::acceptance::TYPE_AHEAD_GUARD;
    use crate::modal::testing::render_rows;

    fn modal() -> QuestionModal {
        QuestionModal::new(
            "question-1".to_owned(),
            Question {
                question: "Choose a color.".to_owned(),
                options: vec![
                    QuestionOption {
                        label: "Blue".to_owned(),
                        description: None,
                    },
                    QuestionOption {
                        label: "Green".to_owned(),
                        description: None,
                    },
                ],
                recommended: Some(1),
            },
        )
    }

    fn submitted(action: ModalKey) -> QuestionAnswer {
        let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::AnswerQuestion {
            id,
            answer,
        }))) = action
        else {
            panic!("expected a question answer, got {action:?}");
        };
        assert_eq!(id, "question-1");
        answer
    }

    mod render {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn restoring_the_body_shows_the_pending_answer_and_rearms_acceptance() {
            use ratatui::Terminal;
            use ratatui::backend::TestBackend;
            use ratatui::layout::Position;

            let mut modal = modal();
            modal.question.question = "Review the output before choosing a color. ".repeat(8);
            render_rows(&mut modal, 30, 12);
            modal.handle_paste("custom");
            render_rows(&mut modal, 30, 2);
            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
            assert!(matches!(modal.handle_key(&enter), ModalKey::Consumed));
            let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();

            terminal
                .draw(|frame| {
                    modal.render(frame, Rect::new(0, 0, 30, 12), &Theme::default());
                })
                .unwrap();

            let rows = crate::testing::buffer_rows(terminal.backend().buffer());
            let answer_row = rows
                .iter()
                .position(|row| row.contains("/ custom"))
                .unwrap();
            assert_eq!(
                terminal.get_cursor_position().unwrap(),
                Position::new(8, u16::try_from(answer_row).unwrap()),
            );
            assert!(matches!(modal.handle_key(&enter), ModalKey::Consumed));
            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            assert_eq!(
                submitted(modal.handle_key(&enter)),
                QuestionAnswer::Answered("custom".to_owned()),
            );
        }

        #[test]
        fn a_short_narrow_view_keeps_wrapped_controls_visible_while_the_question_scrolls() {
            for timeout_seconds in [None, NonZeroU32::new(120)] {
                let deadline = timeout_seconds.map(|timeout_seconds| PromptDeadline {
                    timeout_seconds,
                    action: PromptTimeoutAction::Recommended,
                });
                let mut modal = modal().with_deadline(deadline);
                modal.question.question = "Review the output before choosing a color. ".repeat(8);
                modal.question.options = (0..12)
                    .map(|index| QuestionOption {
                        label: format!("Choice {index}"),
                        description: Some("Consider the generated output carefully. ".repeat(4)),
                    })
                    .collect();
                let before = render_rows(&mut modal, 30, 12);
                modal.acceptance.expire();

                assert!(matches!(
                    modal.handle_key(&KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
                    ModalKey::Consumed
                ));
                let after_page = render_rows(&mut modal, 30, 12);
                modal.acceptance.expire();

                assert!(matches!(
                    modal.handle_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
                    ModalKey::Consumed
                ));
                let after_tab = render_rows(&mut modal, 30, 12);

                for rows in [&before, &after_page, &after_tab] {
                    for hint in [
                        "Up / Down to choose",
                        "Tab to type",
                        "Enter to answer",
                        "Ctrl+S to skip",
                        "Esc to abort",
                        "PageUp / PageDown to scroll",
                    ] {
                        assert!(
                            rows.iter().any(|row| row.contains(hint)),
                            "missing {hint}: {rows:?}"
                        );
                    }
                    if let Some(deadline) = deadline {
                        let policy = format_deadline(deadline);
                        let visible = rows
                            .iter()
                            .map(|row| row.trim())
                            .collect::<Vec<_>>()
                            .join(" ");
                        assert!(visible.contains(&policy), "missing {policy}: {rows:?}");
                    }
                }
                let footer_start = if deadline.is_some() {
                    "After 120 s:"
                } else {
                    "Up / Down to choose"
                };
                let footer = before
                    .iter()
                    .position(|row| row.contains(footer_start))
                    .unwrap();
                assert_eq!(&before[footer..], &after_page[footer..]);
                assert_eq!(&before[footer..], &after_tab[footer..]);
                assert_ne!(&before[..footer], &after_page[..footer]);
                assert!(
                    after_tab.iter().any(|row| row.contains("Your answer:")),
                    "{after_tab:?}"
                );
            }
        }
    }

    mod handle_key {
        use super::*;

        #[test]
        fn submits_the_recommendation_or_a_freeform_answer_including_empty_text() {
            for (paste, expected) in [
                (None, "Green"),
                (Some("custom answer"), "custom answer"),
                (Some(""), ""),
            ] {
                let mut modal = modal();
                if let Some(text) = paste {
                    modal.handle_paste(text);
                }
                render_rows(&mut modal, 60, 15);
                modal.acceptance.expire();

                let answer =
                    submitted(modal.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

                assert_eq!(answer, QuestionAnswer::Answered(expected.to_owned()));
            }
        }

        #[test]
        fn skipping_and_cancelling_are_distinct_from_an_empty_answer() {
            for event in [
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                KeyEvent::new(
                    KeyCode::Char('S'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
            ] {
                let mut modal = modal();
                render_rows(&mut modal, 60, 15);
                modal.acceptance.expire();

                let skipped = submitted(modal.handle_key(&event));
                let aborted = modal.on_cancel();

                assert_eq!(skipped, QuestionAnswer::Skipped, "{event:?}");
                assert!(
                    matches!(aborted, Some(ModalAction::Dispatch(Action::Op(Op::AnswerQuestion { id, answer: QuestionAnswer::Aborted }))) if id == "question-1")
                );
            }
        }

        #[test]
        fn unbound_and_modified_keys_do_not_jump_focus_to_the_freeform_editor() {
            let mut modal = modal();
            render_rows(&mut modal, 60, 15);
            modal.acceptance.expire();

            for event in [
                KeyEvent::from(KeyCode::F(5)),
                KeyEvent::from(KeyCode::Left),
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
            ] {
                assert!(
                    matches!(modal.handle_key(&event), ModalKey::Consumed),
                    "{event:?}"
                );
                assert_eq!(modal.selected, 1, "{event:?} must keep the chosen option");
            }

            assert!(matches!(
                modal.handle_key(&KeyEvent::from(KeyCode::Char('z'))),
                ModalKey::Consumed
            ));
            assert_eq!(modal.selected, modal.question.options.len());
            assert_eq!(modal.editor.text(), "z");
        }

        #[test]
        fn binds_modified_keys_while_guarded() {
            let mut modal = modal();
            let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);

            assert!(modal.binds_modified_key(&ctrl_x));

            modal.acceptance.expire();
            assert!(!modal.binds_modified_key(&ctrl_x));
            assert!(modal.binds_modified_key(&KeyEvent::new(
                KeyCode::Char('S'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )));
        }
    }
}
