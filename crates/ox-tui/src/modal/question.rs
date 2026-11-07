//! Mid-turn questions with suggested choices and a freeform answer.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use ox_core::protocol::Op;
use ox_core::tool::ask::{Question, QuestionAnswer};
use ox_core::util::text::escape_hidden;

use super::acceptance::Acceptance;
use super::chrome::{above, wrap_hints};
use super::line_editor::LineEditor;
use super::{Modal, ModalAction, ModalKey};
use crate::action::Action;
use crate::saturating_u16;
use crate::theme::Theme;
use crate::wrap::wrap_line;

pub(crate) struct QuestionModal {
    id: String,
    question: Question,
    selected: usize,
    editor: LineEditor,
    acceptance: Acceptance,
    scroll: usize,
    rows: usize,
    max_scroll: usize,
    focus_changed: bool,
}

impl QuestionModal {
    pub(crate) fn new(id: String, question: Question) -> Self {
        Self {
            id,
            selected: question.recommended.unwrap_or(0),
            question,
            editor: LineEditor::new(""),
            acceptance: Acceptance::default(),
            scroll: 0,
            rows: 0,
            max_scroll: 0,
            focus_changed: true,
        }
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
            let marker = if index == self.selected { ">" } else { " " };
            let recommended = if self.question.recommended == Some(index) {
                " (recommended)"
            } else {
                ""
            };
            let label = escape_hidden(&option.label);
            let style = if index == self.selected { accent } else { text };
            lines.extend(wrap_line(
                Line::from(Span::styled(
                    format!("{marker} {label}{recommended}"),
                    style,
                )),
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

    fn footer_rows(width: u16, overflow: bool) -> Vec<String> {
        let mut hints = vec![
            "↑/↓ choose",
            "Tab type",
            "Enter answer",
            "Ctrl+S skip",
            "Esc abort",
        ];
        if overflow {
            hints.push("PgUp/PgDn scroll");
        }
        wrap_hints(&hints, usize::from(width).max(1))
    }
}

impl Modal for QuestionModal {
    fn height(&mut self, width: u16, max_height: u16) -> u16 {
        let rows = self.body(width, None).0.len() + 2 + Self::footer_rows(width, false).len();
        saturating_u16(rows).min(max_height)
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let (mut lines, selected_row) = self.body(area.width, Some(theme));
        let editing = self.selected == self.question.options.len();
        lines.push(Line::from(Span::styled("Your answer:", theme.text())));
        let editor_row = lines.len();
        lines.push(self.editor.line(area.width, theme));
        let mut footer = Self::footer_rows(area.width, false);
        if lines.len() + footer.len() > usize::from(area.height) {
            footer = Self::footer_rows(area.width, true);
        }
        let body_area = above(area, saturating_u16(footer.len()));
        let footer_area = Rect {
            y: body_area.bottom(),
            height: area.height - body_area.height,
            ..area
        };
        let focus = if editing { editor_row } else { selected_row };
        self.rows = usize::from(body_area.height);
        self.max_scroll = lines.len().saturating_sub(self.rows);
        if self.focus_changed {
            if focus < self.scroll {
                self.scroll = focus;
            } else if focus >= self.scroll + self.rows {
                self.scroll = focus.saturating_add(1).saturating_sub(self.rows);
            }
            self.focus_changed = false;
        }
        self.scroll = self.scroll.min(self.max_scroll);
        let scroll = self.scroll;
        self.acceptance
            .update(!body_area.is_empty() && focus >= scroll && focus < scroll + self.rows);
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
        if editing && editor_row >= scroll && editor_row < scroll + self.rows {
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
                let page = self.rows.saturating_sub(1).max(1);
                self.scroll = if event.code == KeyCode::PageUp {
                    self.scroll.saturating_sub(page)
                } else {
                    self.scroll.saturating_add(page).min(self.max_scroll)
                };
                self.acceptance.reset();
                ModalKey::Consumed
            }
            KeyCode::Char('s') if event.modifiers == KeyModifiers::CONTROL => {
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
            _ => {
                self.selected = choices;
                self.focus_changed = true;
                self.editor.handle_key(event);
                ModalKey::Consumed
            }
        }
    }

    fn handle_paste(&mut self, text: &str) {
        self.selected = self.question.options.len();
        self.focus_changed = true;
        self.editor.paste(text);
    }

    fn binds_modified_key(&self, event: &KeyEvent) -> bool {
        event.code == KeyCode::Char('s') && event.modifiers == KeyModifiers::CONTROL
            || LineEditor::binds_modified_key(event)
    }

    fn on_cancel(&mut self) -> Option<ModalAction> {
        Some(self.answer(QuestionAnswer::Aborted))
    }
}

#[cfg(test)]
mod tests {
    use ox_core::tool::ask::QuestionOption;

    use super::*;
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
            let mut modal = modal();
            render_rows(&mut modal, 60, 15);
            modal.acceptance.expire();

            let skipped = submitted(
                modal.handle_key(&KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            );
            let aborted = modal.on_cancel();

            assert_eq!(skipped, QuestionAnswer::Skipped);
            assert!(
                matches!(aborted, Some(ModalAction::Dispatch(Action::Op(Op::AnswerQuestion { id, answer: QuestionAnswer::Aborted }))) if id == "question-1")
            );
        }
    }
}
