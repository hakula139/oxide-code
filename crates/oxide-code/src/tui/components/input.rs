use std::cell::Cell;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_textarea::{TextArea, WrapMode};
use unicode_width::UnicodeWidthStr;

use crate::agent::event::UserAction;
use crate::tui::component::Component;
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
    /// Last render width for visual line count estimation. Updated each
    /// frame by `render()`, used by `height()` on the *next* frame.
    /// `Cell` because `render(&self)` is immutable.
    last_width: Cell<u16>,
    /// Tracked viewport scroll offset (screen line index of the topmost
    /// visible row). Mirrors ratatui-textarea's internal `viewport` which
    /// is not publicly accessible.
    scroll_top: Cell<u16>,
}

impl InputArea {
    pub(crate) fn new(theme: Theme) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_cursor_line_style(Style::default());
        textarea.set_style(theme.text());
        textarea.set_placeholder_text("Ask anything...");
        textarea.set_placeholder_style(theme.dim());
        textarea.set_wrap_mode(WrapMode::Word);
        textarea.set_block(Block::default());

        Self {
            theme,
            textarea,
            enabled: true,
            last_width: Cell::new(0),
            scroll_top: Cell::new(0),
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
    pub(crate) fn height(&self) -> u16 {
        let content_lines = self.visual_line_count();
        // content + top border (1) + hint line (1)
        content_lines.min(MAX_VISIBLE_LINES) + 2
    }
}

impl Component for InputArea {
    fn handle_event(&mut self, event: &Event) -> Option<UserAction> {
        // Ctrl+C / Ctrl+D always quits, even when disabled.
        if let Event::Key(KeyEvent {
            code: KeyCode::Char('c' | 'd'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) = event
        {
            return Some(UserAction::Quit);
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

        // Store width for visual line count estimation on the next frame.
        self.last_width.set(chunks[0].width);

        if self.enabled {
            // screen_cursor().row is an absolute screen-line index across
            // all wrapped lines, not viewport-relative. Replicate the
            // scroll logic from ratatui-textarea's `next_scroll_top` to
            // convert to a position within the rendered area.
            let sc = self.textarea.screen_cursor();
            #[expect(
                clippy::cast_possible_truncation,
                reason = "cursor position fits in u16 for terminal widths"
            )]
            let cursor_row = sc.row as u16;
            let height = chunks[0].height;
            let prev = self.scroll_top.get();
            let top = if cursor_row < prev {
                cursor_row
            } else if height > 0 && prev + height <= cursor_row {
                cursor_row + 1 - height
            } else {
                prev
            };
            self.scroll_top.set(top);

            #[expect(
                clippy::cast_possible_truncation,
                reason = "cursor position fits in u16 for terminal widths"
            )]
            let cursor_x = chunks[0]
                .x
                .saturating_add(sc.col as u16)
                .min(chunks[0].right().saturating_sub(1));
            let cursor_y = chunks[0].y + cursor_row - top;
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
    /// Estimate the number of visual (screen) lines after word-wrap.
    ///
    /// Uses `last_width` from the previous render frame. Falls back to
    /// logical line count when no width is known yet (first frame).
    #[expect(
        clippy::cast_possible_truncation,
        reason = "line count fits in u16 for any practical input"
    )]
    fn visual_line_count(&self) -> u16 {
        let width = self.last_width.get() as usize;
        if width == 0 {
            return (self.textarea.lines().len() as u16).max(1);
        }
        self.textarea
            .lines()
            .iter()
            .map(|line| {
                let w = UnicodeWidthStr::width(line.as_str());
                if w <= width {
                    1u16
                } else {
                    w.div_ceil(width) as u16
                }
            })
            .sum::<u16>()
            .max(1)
    }

    fn submit(&mut self) -> Option<UserAction> {
        let content: String = self.textarea.lines().join("\n");
        let trimmed = content.trim().to_owned();
        if trimmed.is_empty() {
            return None;
        }

        // Clear the textarea and reset scroll state.
        self.textarea.select_all();
        self.textarea.cut();
        self.scroll_top.set(0);

        Some(UserAction::SubmitPrompt(trimmed))
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;

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
        assert!(matches!(action, Some(UserAction::Quit)));
    }

    #[test]
    fn handle_event_ctrl_d_returns_quit() {
        let mut input = test_input();
        let action = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(matches!(action, Some(UserAction::Quit)));
    }

    #[test]
    fn handle_event_ctrl_c_quits_even_when_disabled() {
        let mut input = test_input();
        input.set_enabled(false);
        let action = input.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, Some(UserAction::Quit)));
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
        assert!(matches!(action, Some(UserAction::SubmitPrompt(s)) if s == "hi"));
    }

    // ── render ──

    fn render_to_backend(input: &InputArea, width: u16, height: u16) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                input.render(frame, frame.area());
            })
            .unwrap();
        terminal.backend().clone()
    }

    fn type_text(input: &mut InputArea, text: &str) {
        for ch in text.chars() {
            input.textarea.input(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
    }

    #[test]
    fn render_empty_shows_placeholder_and_hint_line() {
        let input = test_input();
        insta::assert_snapshot!(render_to_backend(&input, 60, 3));
    }

    #[test]
    fn render_with_text_shows_typed_content() {
        let mut input = test_input();
        type_text(&mut input, "hello world");
        insta::assert_snapshot!(render_to_backend(&input, 60, 3));
    }

    #[test]
    fn render_disabled_applies_dim_foreground_to_text() {
        // Enable/disable only changes per-cell styling, which a text-only
        // snapshot collapses. Inspect the buffer directly instead.
        let theme = Theme::default();
        let mut input = InputArea::new(theme);
        type_text(&mut input, "pending");

        let enabled_fg = render_to_backend(&input, 60, 3)
            .buffer()
            .cell(Position::new(0, 1))
            .unwrap()
            .fg;
        input.set_enabled(false);
        let disabled_fg = render_to_backend(&input, 60, 3)
            .buffer()
            .cell(Position::new(0, 1))
            .unwrap()
            .fg;

        assert_eq!(enabled_fg, theme.text().fg.unwrap());
        assert_eq!(disabled_fg, theme.dim().fg.unwrap());
        assert_ne!(enabled_fg, disabled_fg);
    }

    #[test]
    fn render_multiline_grows_textarea_region() {
        let mut input = test_input();
        type_text(&mut input, "line 1");
        input.textarea.insert_newline();
        type_text(&mut input, "line 2");
        input.textarea.insert_newline();
        type_text(&mut input, "line 3");
        insta::assert_snapshot!(render_to_backend(&input, 60, input.height()));
    }

    #[test]
    fn render_long_line_wraps_and_engages_scroll_offset() {
        // Narrow width forces word-wrap; typing past the visible row
        // engages scroll_top so the cursor stays on-screen.
        let mut input = test_input();
        type_text(
            &mut input,
            "a long input that overflows a narrow terminal and forces the textarea to wrap",
        );
        insta::assert_snapshot!(render_to_backend(&input, 30, 5));
    }
    // ── visual_line_count ──

    #[test]
    fn visual_line_count_no_width_falls_back_to_logical() {
        let mut input = test_input();
        // last_width is 0 (no render yet), so falls back to logical count.
        assert_eq!(input.visual_line_count(), 1);

        input.textarea.insert_newline();
        assert_eq!(input.visual_line_count(), 2);
    }

    #[test]
    fn visual_line_count_wraps_long_line() {
        let mut input = test_input();
        input.last_width.set(10);
        // Insert a 25-char line: wraps to ceil(25/10) = 3 visual lines.
        for ch in "abcdefghijklmnopqrstuvwxy".chars() {
            input.textarea.input(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(input.visual_line_count(), 3);
    }

    #[test]
    fn visual_line_count_mixed_logical_and_wrapped() {
        let mut input = test_input();
        input.last_width.set(10);
        // Line 1: 5 chars (fits in 10) -> 1 visual line.
        for ch in "hello".chars() {
            input.textarea.input(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        input.textarea.insert_newline();
        // Line 2: 15 chars -> ceil(15/10) = 2 visual lines.
        for ch in "abcdefghijklmno".chars() {
            input.textarea.input(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(input.visual_line_count(), 3);
    }

    #[test]
    fn height_accounts_for_visual_wrapping() {
        let mut input = test_input();
        input.last_width.set(10);
        // Single logical line, 25 chars -> 3 visual lines.
        for ch in "abcdefghijklmnopqrstuvwxy".chars() {
            input.textarea.input(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        // 3 content + 1 border + 1 hint = 5
        assert_eq!(input.height(), 5);
    }

    // ── submit ──

    #[test]
    fn submit_empty_produces_no_action() {
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
        assert!(matches!(action, Some(UserAction::SubmitPrompt(s)) if s == "a"));
    }
}
