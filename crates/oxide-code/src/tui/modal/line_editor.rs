//! Single-line text field for modals. `ratatui-textarea` supplies cursor movement and the
//! readline-style editing keys the main input already uses.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui_textarea::{CursorMove, TextArea};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::saturating_u16;
use crate::tui::theme::Theme;

const PROMPT: &str = "/ ";
const PROMPT_WIDTH: u16 = 2;

/// One-line editor rendered as `/ ` followed by the text.
pub(crate) struct LineEditor {
    textarea: TextArea<'static>,
    max_width: usize,
    placeholder: Option<&'static str>,
    /// First char shown when the text overflows its row. `Cell` because it settles at render.
    scroll: Cell<usize>,
}

impl LineEditor {
    pub(crate) fn new(text: &str) -> Self {
        let mut textarea = TextArea::new(vec![text.to_owned()]);
        textarea.move_cursor(CursorMove::End);
        Self {
            textarea,
            max_width: usize::MAX,
            placeholder: None,
            scroll: Cell::new(0),
        }
    }

    /// Caps the text at `max_width` display columns. Truncates the current text, and edits that
    /// would grow past the cap are dropped.
    pub(crate) fn with_max_width(mut self, max_width: usize) -> Self {
        let text = head_within(self.text(), max_width);
        self.max_width = max_width;
        self.set_text(&text);
        self
    }

    /// Dim hint shown in place of the empty text.
    pub(crate) fn with_placeholder(mut self, placeholder: &'static str) -> Self {
        self.placeholder = Some(placeholder);
        self
    }

    pub(crate) fn text(&self) -> &str {
        &self.textarea.lines()[0]
    }

    /// Replaces the text and parks the cursor at its end.
    pub(crate) fn set_text(&mut self, text: &str) {
        self.textarea = TextArea::new(vec![text.to_owned()]);
        self.textarea.move_cursor(CursorMove::End);
    }

    /// Applies one editing key and returns whether the text changed.
    pub(crate) fn handle_key(&mut self, event: &KeyEvent) -> bool {
        if is_blocked_key(event) || !self.textarea.input(*event) {
            return false;
        }
        if self.text().width() > self.max_width {
            self.textarea.undo();
            return false;
        }
        true
    }

    /// Inserts pasted `text` at the cursor as one line, with each run of control characters such
    /// as a line break collapsed to a space. Keeps what fits under the cap and returns whether
    /// the text changed.
    pub(crate) fn paste(&mut self, text: &str) -> bool {
        let room = self.max_width.saturating_sub(self.text().width());
        let flat = text
            .split(char::is_control)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        self.textarea.insert_str(head_within(&flat, room))
    }

    /// The prompt plus the slice of the text that fits `width`, or the placeholder while empty.
    pub(crate) fn line(&self, width: u16, theme: &Theme) -> Line<'static> {
        let prompt = Span::styled(PROMPT, theme.accent());
        let body = match self.placeholder {
            Some(placeholder) if self.text().is_empty() => Span::styled(placeholder, theme.dim()),
            _ => Span::styled(self.visible(width).0, theme.text()),
        };
        Line::from(vec![prompt, body])
    }

    /// Places the terminal cursor at the editing position on row `y`, clamped inside `area`.
    pub(crate) fn place_cursor(&self, frame: &mut Frame<'_>, area: Rect, y: u16) {
        let column = saturating_u16(self.visible(area.width).1);
        let raw_x = area.x.saturating_add(PROMPT_WIDTH).saturating_add(column);
        crate::tui::cursor::place_clamped(frame, raw_x, y, area);
    }

    /// The text shown in a `width`-column row and the cursor's column within it. The shown slice
    /// scrolls as little as possible to keep the cursor in view.
    fn visible(&self, width: u16) -> (String, usize) {
        let budget = usize::from(width.saturating_sub(PROMPT_WIDTH + 1));
        let chars: Vec<char> = self.text().chars().collect();
        let widths: Vec<usize> = chars.iter().map(|c| c.width().unwrap_or(0)).collect();
        let span = |from: usize, to: usize| -> usize { widths[from..to].iter().sum() };
        let cursor = self.textarea.cursor().1;

        let mut start = self.scroll.get().min(cursor);
        while span(start, cursor) > budget {
            start += 1;
        }
        while start > 0 && span(start - 1, chars.len()) <= budget {
            start -= 1;
        }
        self.scroll.set(start);

        let mut end = start;
        while end < chars.len() && span(start, end + 1) <= budget {
            end += 1;
        }
        (chars[start..end].iter().collect(), span(start, cursor))
    }
}

/// Keys the textarea maps to a line break or an indent, neither of which fits a one-line field.
fn is_blocked_key(event: &KeyEvent) -> bool {
    match event.code {
        KeyCode::Enter | KeyCode::Tab | KeyCode::Char('\n' | '\r') => true,
        KeyCode::Char('m') => event.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

fn head_within(text: &str, max_width: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= max_width
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // cspell:ignore bcdefghij cdefg mnopqrstu rstuvwxyz
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(editor: &mut LineEditor, text: &str) {
        for c in text.chars() {
            editor.handle_key(&key(KeyCode::Char(c)));
        }
    }

    // ── new ──

    #[test]
    fn new_parks_the_cursor_after_the_initial_text() {
        let mut editor = LineEditor::new("ab");
        type_str(&mut editor, "c");
        assert_eq!(editor.text(), "abc");
    }

    // ── with_max_width ──

    #[test]
    fn with_max_width_truncates_and_drops_growth_past_the_cap() {
        let mut editor = LineEditor::new("hello world").with_max_width(5);
        assert_eq!(editor.text(), "hello");

        assert!(!editor.handle_key(&key(KeyCode::Char('x'))));
        assert_eq!(editor.text(), "hello", "insert at the cap is dropped");

        assert!(editor.handle_key(&key(KeyCode::Backspace)));
        assert!(editor.handle_key(&key(KeyCode::Char('!'))));
        assert_eq!(editor.text(), "hell!", "room freed by a delete is usable");
    }

    #[test]
    fn with_max_width_counts_wide_chars_by_column() {
        let mut editor = LineEditor::new("界界界").with_max_width(5);
        assert_eq!(editor.text(), "界界");

        assert!(!editor.handle_key(&key(KeyCode::Char('界'))));
        assert!(editor.handle_key(&key(KeyCode::Char('a'))));
        assert_eq!(
            editor.text(),
            "界界a",
            "a narrow char fills the last column"
        );
    }

    // ── set_text ──

    #[test]
    fn set_text_replaces_the_text_and_parks_the_cursor_at_its_end() {
        let mut editor = LineEditor::new("old");
        editor.set_text("new");
        type_str(&mut editor, "!");
        assert_eq!(editor.text(), "new!");
    }

    // ── handle_key ──

    #[test]
    fn handle_key_inserts_at_the_cursor_after_moving_it() {
        let mut editor = LineEditor::new("acd");
        editor.handle_key(&key(KeyCode::Left));
        editor.handle_key(&key(KeyCode::Left));
        assert!(editor.handle_key(&key(KeyCode::Char('b'))));
        assert_eq!(editor.text(), "abcd");

        editor.handle_key(&ctrl('a'));
        type_str(&mut editor, ">");
        assert_eq!(editor.text(), ">abcd", "Ctrl+A jumps to the start");
    }

    #[test]
    fn handle_key_supports_readline_word_deletion() {
        let mut editor = LineEditor::new("fix auth bug");
        assert!(editor.handle_key(&ctrl('w')));
        assert_eq!(editor.text(), "fix auth ");
    }

    #[test]
    fn handle_key_never_inserts_control_chords_or_line_breaks() {
        let mut editor = LineEditor::new("ab");
        for event in [
            ctrl('g'),
            ctrl('m'),
            key(KeyCode::Enter),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            key(KeyCode::Tab),
            key(KeyCode::Char('\n')),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
        ] {
            assert!(!editor.handle_key(&event), "{event:?} must not edit");
        }
        assert_eq!(editor.text(), "ab");
    }

    // ── paste ──

    #[test]
    fn paste_inserts_at_the_cursor_with_line_breaks_collapsed() {
        let mut editor = LineEditor::new("[]");
        editor.handle_key(&key(KeyCode::Left));
        assert!(editor.paste("\nfix\r\nauth\n\tflow\n"));
        assert_eq!(editor.text(), "[fix auth flow]");
    }

    #[test]
    fn paste_keeps_only_what_fits_under_the_cap() {
        let mut editor = LineEditor::new("ab").with_max_width(6);
        assert!(editor.paste("c界d界"));
        assert_eq!(
            editor.text(),
            "abc界d",
            "a wide char past the cap is dropped"
        );
        assert!(!editor.paste("x"), "a full field takes nothing");
        assert!(!editor.paste("\n\n"), "line breaks alone insert nothing");
    }

    // ── line ──

    #[test]
    fn line_shows_the_placeholder_only_while_empty() {
        let theme = Theme::default();
        let mut editor = LineEditor::new("").with_placeholder("Type to filter");
        assert_eq!(editor.line(40, &theme).to_string(), "/ Type to filter");

        type_str(&mut editor, "ab");
        assert_eq!(editor.line(40, &theme).to_string(), "/ ab");
    }

    #[test]
    fn line_scrolls_as_little_as_possible_to_keep_the_cursor_in_view() {
        // Width 12 leaves 9 columns for text beside the prompt and the end-of-line cursor.
        let mut editor = LineEditor::new("abcdefghijklmnopqrstuvwxyz");
        assert_eq!(shown(&editor), ("/ rstuvwxyz".to_owned(), 11));

        editor.handle_key(&ctrl('a'));
        assert_eq!(shown(&editor), ("/ abcdefghi".to_owned(), 2));

        for _ in 0..10 {
            editor.handle_key(&key(KeyCode::Right));
        }
        assert_eq!(shown(&editor), ("/ bcdefghij".to_owned(), 11));

        for _ in 0..5 {
            editor.handle_key(&key(KeyCode::Left));
        }
        assert_eq!(
            shown(&editor),
            ("/ bcdefghij".to_owned(), 6),
            "moving back inside the row keeps the slice",
        );
    }

    #[test]
    fn line_refills_the_row_when_the_tail_shrinks() {
        let mut editor = LineEditor::new("abcdefghijklmnopqrstuvwxyz");
        assert_eq!(shown(&editor).0, "/ rstuvwxyz");

        for _ in 0..5 {
            editor.handle_key(&key(KeyCode::Backspace));
        }
        assert_eq!(shown(&editor), ("/ mnopqrstu".to_owned(), 11));
    }

    // ── place_cursor ──

    fn cursor_x(editor: &LineEditor, width: u16) -> u16 {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| editor.place_cursor(frame, Rect::new(0, 0, width, 1), 0))
            .unwrap();
        terminal.get_cursor_position().unwrap().x
    }

    /// The rendered row and the cursor column at width 12.
    fn shown(editor: &LineEditor) -> (String, u16) {
        let line = editor.line(12, &Theme::default()).to_string();
        (line, cursor_x(editor, 12))
    }

    #[test]
    fn place_cursor_tracks_the_editing_position_by_display_width() {
        let mut editor = LineEditor::new("日本");
        assert_eq!(
            cursor_x(&editor, 20),
            PROMPT_WIDTH + 4,
            "wide glyphs count twice"
        );

        editor.handle_key(&key(KeyCode::Left));
        assert_eq!(cursor_x(&editor, 20), PROMPT_WIDTH + 2);
    }
}
