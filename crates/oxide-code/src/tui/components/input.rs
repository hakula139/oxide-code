use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::component::{Action, Component};
use crate::tui::theme::Theme;

/// Multi-line input area at the bottom of the TUI.
///
/// Currently a simple single-line input with cursor. Will be replaced
/// with `tui-textarea` for full multi-line editing.
///
/// Key bindings:
/// - Enter: submit prompt
/// - Ctrl+C / Ctrl+D: quit
/// - Backspace / Delete: delete character
/// - Left / Right / Home / End: move cursor
pub(crate) struct InputArea {
    theme: Theme,
    buffer: String,
    /// Cursor position in character (not byte) units.
    cursor: usize,
    /// Cached character count of `buffer`, maintained on every mutation.
    char_count: usize,
    enabled: bool,
}

impl InputArea {
    pub(crate) fn new(theme: Theme) -> Self {
        Self {
            theme,
            buffer: String::new(),
            cursor: 0,
            char_count: 0,
            enabled: true,
        }
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the height this component needs (input line + hint line + border).
    pub(crate) fn height(&self) -> u16 {
        _ = self;
        3
    }
}

impl Component for InputArea {
    fn handle_event(&mut self, event: &Event) -> Option<Action> {
        if !self.enabled {
            // Still allow Ctrl+C / Ctrl+D to quit.
            if let Event::Key(KeyEvent {
                code: KeyCode::Char('c' | 'd'),
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
            (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL) => Some(Action::Quit),
            (KeyCode::Enter, _) => self.submit(),
            (KeyCode::Backspace, _) => {
                if self.cursor > 0 {
                    let start = self.byte_offset(self.cursor - 1);
                    let end = self.byte_offset(self.cursor);
                    self.buffer.drain(start..end);
                    self.cursor -= 1;
                    self.char_count -= 1;
                }
                None
            }
            (KeyCode::Delete, _) => {
                if self.cursor < self.char_count {
                    let start = self.byte_offset(self.cursor);
                    let end = self.byte_offset(self.cursor + 1);
                    self.buffer.drain(start..end);
                    self.char_count -= 1;
                }
                None
            }
            (KeyCode::Left, _) => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            (KeyCode::Right, _) => {
                if self.cursor < self.char_count {
                    self.cursor += 1;
                }
                None
            }
            (KeyCode::Home, _) => {
                self.cursor = 0;
                None
            }
            (KeyCode::End, _) => {
                self.cursor = self.char_count;
                None
            }
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                let offset = self.byte_offset(self.cursor);
                self.buffer.insert(offset, c);
                self.cursor += 1;
                self.char_count += 1;
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

        // Place cursor after the prompt (" > " = 3 chars offset).
        if self.enabled {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "cursor position fits in u16 for terminal widths"
            )]
            let cursor_x = chunks[0]
                .x
                .saturating_add(3)
                .saturating_add(self.cursor as u16)
                .min(chunks[0].right().saturating_sub(1));
            frame.set_cursor_position((cursor_x, chunks[0].y));
        }

        // Hint line.
        let hint = Line::from(vec![
            Span::raw(" "),
            Span::styled("Enter", self.theme.dim()),
            Span::styled(": send", self.theme.dim()),
            self.theme.separator_span(),
            Span::styled("Ctrl+C / Ctrl+D", self.theme.dim()),
            Span::styled(": quit", self.theme.dim()),
        ]);
        frame.render_widget(Paragraph::new(hint), chunks[1]);
    }
}

// ── Private Helpers ──

impl InputArea {
    fn submit(&mut self) -> Option<Action> {
        let text = self.buffer.trim().to_owned();
        if text.is_empty() {
            return None;
        }
        self.buffer.clear();
        self.cursor = 0;
        self.char_count = 0;
        Some(Action::SubmitPrompt(text))
    }

    /// Converts a character index to a byte offset in `buffer`.
    fn byte_offset(&self, char_idx: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(char_idx)
            .map_or(self.buffer.len(), |(i, _)| i)
    }
}
