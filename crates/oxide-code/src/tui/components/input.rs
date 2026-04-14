use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_textarea::TextArea;

use crate::tui::component::{Action, Component};
use crate::tui::theme::Theme;

/// Maximum number of visible content lines before the input stops growing.
const MAX_VISIBLE_LINES: u16 = 6;

/// Multi-line input area at the bottom of the TUI.
///
/// Wraps [`ratatui_textarea::TextArea`] for multi-line editing with
/// dynamic height. Grows from 1 to [`MAX_VISIBLE_LINES`] as content
/// expands.
///
/// Key bindings:
/// - Enter: submit prompt
/// - Shift+Enter: insert newline
/// - Ctrl+C / Ctrl+D: quit
pub(crate) struct InputArea {
    theme: Theme,
    textarea: TextArea<'static>,
    enabled: bool,
}

impl InputArea {
    pub(crate) fn new(theme: Theme) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_cursor_line_style(Style::default());
        textarea.set_style(theme.text());
        textarea.set_placeholder_text("Ask anything...");
        textarea.set_placeholder_style(theme.dim());
        textarea.set_block(Block::default());

        Self {
            theme,
            textarea,
            enabled: true,
        }
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            self.textarea.set_style(self.theme.text());
        } else {
            self.textarea.set_style(self.theme.dim());
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the height this component needs (content lines + border + hint).
    #[expect(
        clippy::cast_possible_truncation,
        reason = "line count fits in u16 for any practical input"
    )]
    pub(crate) fn height(&self) -> u16 {
        let content_lines = (self.textarea.lines().len() as u16).max(1);
        // content + top border (1) + hint line (1)
        content_lines.min(MAX_VISIBLE_LINES) + 2
    }
}

impl Component for InputArea {
    fn handle_event(&mut self, event: &Event) -> Option<Action> {
        // Ctrl+C / Ctrl+D always quits, even when disabled.
        if let Event::Key(KeyEvent {
            code: KeyCode::Char('c' | 'd'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) = event
        {
            return Some(Action::Quit);
        }

        if !self.enabled {
            return None;
        }

        if let Event::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers,
            ..
        }) = event
        {
            // Native Kitty protocol: terminal reports SHIFT directly.
            // VS Code / Cursor keybinding: Shift+Enter sends \x1b\r (ESC CR),
            // which crossterm parses as Alt+Enter.
            if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) {
                self.textarea.insert_newline();
                return None;
            }
            return self.submit();
        }

        // Delegate to textarea for all other input.
        self.textarea.input(event.clone());
        None
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
            Constraint::Min(1),    // textarea
            Constraint::Length(1), // hint line
        ])
        .split(inner);

        frame.render_widget(&self.textarea, chunks[0]);

        if self.enabled {
            // Place cursor inside the textarea area.
            let (row, col) = self.textarea.cursor();
            #[expect(
                clippy::cast_possible_truncation,
                reason = "cursor position fits in u16 for terminal widths"
            )]
            let cursor_y = chunks[0].y + row as u16;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "cursor position fits in u16 for terminal widths"
            )]
            let cursor_x = chunks[0]
                .x
                .saturating_add(col as u16)
                .min(chunks[0].right().saturating_sub(1));
            frame.set_cursor_position((cursor_x, cursor_y));
        }

        // Hint line.
        let hint = Line::from(vec![
            Span::styled("Enter", self.theme.dim()),
            Span::styled(": send", self.theme.dim()),
            self.theme.separator_span(),
            Span::styled("Shift+Enter", self.theme.dim()),
            Span::styled(": newline", self.theme.dim()),
            self.theme.separator_span(),
            Span::styled("Ctrl+C", self.theme.dim()),
            Span::styled(": quit", self.theme.dim()),
        ]);
        frame.render_widget(Paragraph::new(hint), chunks[1]);
    }
}

// ── Private Helpers ──

impl InputArea {
    fn submit(&mut self) -> Option<Action> {
        let content: String = self.textarea.lines().join("\n");
        let trimmed = content.trim().to_owned();
        if trimmed.is_empty() {
            return None;
        }

        // Clear the textarea.
        self.textarea.select_all();
        self.textarea.cut();

        Some(Action::SubmitPrompt(trimmed))
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn test_input() -> InputArea {
        InputArea::new(Theme::default())
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    // ── set_enabled ──

    #[test]
    fn set_enabled_toggles_state() {
        let mut input = test_input();
        assert!(input.is_enabled());

        input.set_enabled(false);
        assert!(!input.is_enabled());

        input.set_enabled(true);
        assert!(input.is_enabled());
    }

    #[test]
    fn set_enabled_same_value_is_noop() {
        let mut input = test_input();
        input.set_enabled(true);
        assert!(input.is_enabled());
    }

    // ── height ──

    #[test]
    fn height_empty_input_is_three() {
        let input = test_input();
        assert_eq!(input.height(), 3); // 1 content + 1 border + 1 hint
    }

    #[test]
    fn height_grows_with_content() {
        let mut input = test_input();
        input.textarea.insert_newline();
        input.textarea.insert_newline();
        assert_eq!(input.height(), 5); // 3 content + 1 border + 1 hint
    }

    #[test]
    fn height_capped_at_max() {
        let mut input = test_input();
        for _ in 0..10 {
            input.textarea.insert_newline();
        }
        assert_eq!(input.height(), MAX_VISIBLE_LINES + 2);
    }

    // ── handle_event ──

    #[test]
    fn handle_event_ctrl_c_returns_quit() {
        let mut input = test_input();
        let action = input.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, Some(Action::Quit)));
    }

    #[test]
    fn handle_event_ctrl_d_returns_quit() {
        let mut input = test_input();
        let action = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(matches!(action, Some(Action::Quit)));
    }

    #[test]
    fn handle_event_ctrl_c_quits_even_when_disabled() {
        let mut input = test_input();
        input.set_enabled(false);
        let action = input.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, Some(Action::Quit)));
    }

    #[test]
    fn handle_event_disabled_ignores_input() {
        let mut input = test_input();
        input.set_enabled(false);
        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(action.is_none());
    }

    #[test]
    fn handle_event_shift_enter_inserts_newline() {
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));

        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(action.is_none());
        assert_eq!(input.textarea.lines().len(), 2);
    }

    #[test]
    fn handle_event_alt_enter_inserts_newline() {
        // VS Code / Cursor keybinding sends \x1b\r for Shift+Enter,
        // which crossterm parses as ALT+Enter.
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));

        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::ALT));
        assert!(action.is_none());
        assert_eq!(input.textarea.lines().len(), 2);
    }

    #[test]
    fn handle_event_enter_submits_nonempty() {
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        )));
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('i'),
            KeyModifiers::NONE,
        )));

        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(Action::SubmitPrompt(s)) if s == "hi"));
    }

    // ── submit ──

    #[test]
    fn submit_empty_returns_none() {
        let mut input = test_input();
        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(action.is_none());
    }

    #[test]
    fn submit_clears_textarea() {
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));

        input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(input.textarea.lines(), vec![""]);
    }

    #[test]
    fn submit_trims_whitespace() {
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        )));
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        )));

        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(Action::SubmitPrompt(s)) if s == "a"));
    }
}
