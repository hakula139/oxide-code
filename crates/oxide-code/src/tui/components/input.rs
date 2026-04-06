use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::component::{Action, Component};
use crate::tui::theme::Theme;

/// Multi-line input area at the bottom of the TUI.
///
/// For PR 3.1 this is a simple single-line input with cursor. PR 3.3 will
/// replace the internals with `tui-textarea` for full multi-line editing.
///
/// Key bindings:
/// - Enter: submit prompt
/// - Ctrl+C: quit
/// - Backspace: delete character
/// - Left / Right: move cursor
pub struct InputArea {
    theme: Theme,
    buffer: String,
    cursor: usize,
    enabled: bool,
}

impl InputArea {
    pub fn new() -> Self {
        Self {
            theme: Theme::default(),
            buffer: String::new(),
            cursor: 0,
            enabled: true,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the height this component needs (input line + hint line + border).
    pub fn height(&self) -> u16 {
        _ = self;
        3
    }

    fn submit(&mut self) -> Option<Action> {
        let text = self.buffer.trim().to_owned();
        if text.is_empty() {
            return None;
        }
        self.buffer.clear();
        self.cursor = 0;
        Some(Action::SubmitPrompt(text))
    }
}

impl Component for InputArea {
    fn handle_event(&mut self, event: &Event) -> Option<Action> {
        if !self.enabled {
            // Still allow Ctrl+C to quit.
            if let Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) = event
            {
                return Some(Action::Quit);
            }
            return None;
        }

        let Event::Key(key) = event else {
            return None;
        };

        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Action::Quit),
            (KeyCode::Enter, _) => self.submit(),
            (KeyCode::Backspace, _) => {
                if self.cursor > 0 {
                    let byte_idx = self
                        .buffer
                        .char_indices()
                        .nth(self.cursor - 1)
                        .map_or(0, |(i, _)| i);
                    let next_byte_idx = self
                        .buffer
                        .char_indices()
                        .nth(self.cursor)
                        .map_or(self.buffer.len(), |(i, _)| i);
                    self.buffer.drain(byte_idx..next_byte_idx);
                    self.cursor -= 1;
                }
                None
            }
            (KeyCode::Left, _) => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            (KeyCode::Right, _) => {
                let char_count = self.buffer.chars().count();
                if self.cursor < char_count {
                    self.cursor += 1;
                }
                None
            }
            (KeyCode::Home, _) => {
                self.cursor = 0;
                None
            }
            (KeyCode::End, _) => {
                self.cursor = self.buffer.chars().count();
                None
            }
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                let byte_idx = self
                    .buffer
                    .char_indices()
                    .nth(self.cursor)
                    .map_or(self.buffer.len(), |(i, _)| i);
                self.buffer.insert(byte_idx, c);
                self.cursor += 1;
                None
            }
            _ => None,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        let border_style = if self.enabled {
            self.theme.border_focused()
        } else {
            self.theme.border_unfocused()
        };

        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(border_style);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::vertical([
            Constraint::Length(1), // input line
            Constraint::Length(1), // hint line
        ])
        .split(inner);

        // Input line with prompt character.
        let prompt = Span::styled("> ", self.theme.accent());
        let text = Span::styled(&self.buffer, self.theme.text());
        let input_line = Line::from(vec![Span::raw(" "), prompt, text]);
        frame.render_widget(Paragraph::new(input_line), chunks[0]);

        // Place cursor after the prompt.
        if self.enabled {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "cursor position fits in u16 for terminal widths"
            )]
            frame.set_cursor_position((chunks[0].x + 3 + self.cursor as u16, chunks[0].y));
        }

        // Hint line.
        let hint = Line::from(vec![
            Span::raw(" "),
            Span::styled("Enter", self.theme.dim()),
            Span::styled(": send", self.theme.dim()),
            Span::styled(" │ ", self.theme.separator()),
            Span::styled("Ctrl+C", self.theme.dim()),
            Span::styled(": quit", self.theme.dim()),
        ]);
        frame.render_widget(Paragraph::new(hint), chunks[1]);
    }
}
