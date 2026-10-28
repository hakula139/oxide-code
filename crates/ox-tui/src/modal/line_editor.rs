//! Single-line text field for modals, drawing on the editor table the prompt shares.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui_textarea::{CursorMove, TextArea};
use unicode_width::UnicodeWidthStr as _;

use ox_core::util::text::take_head;

use crate::cursor::place_clamped;
use crate::grapheme::{self, GraphemeStops};
use crate::saturating_u16;
use crate::theme::Theme;

const PROMPT: &str = "/ ";
const PROMPT_WIDTH: u16 = 2;

/// One-line editor rendered as `/ ` followed by the text.
pub(crate) struct LineEditor {
    textarea: TextArea<'static>,
    max_width: usize,
    placeholder: Option<&'static str>,
    /// First grapheme cluster shown when the text overflows its row.
    scroll: usize,
}

impl LineEditor {
    pub(crate) fn new(text: &str) -> Self {
        let mut textarea = TextArea::new(vec![text.to_owned()]);
        textarea.move_cursor(CursorMove::End);
        Self {
            textarea,
            max_width: usize::MAX,
            placeholder: None,
            scroll: 0,
        }
    }

    /// Caps the text at `max_width` display columns. Truncates the current text, and edits that
    /// would grow past the cap are dropped.
    pub(crate) fn with_max_width(mut self, max_width: usize) -> Self {
        let text = take_head(self.text(), max_width);
        self.max_width = max_width;
        self.set_text(&text);
        self
    }

    /// Dim hint shown in place of the empty text.
    pub(super) fn with_placeholder(mut self, placeholder: &'static str) -> Self {
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

    /// Applies one editing key and returns whether the text changed, dropping a change that would
    /// grow past the width cap.
    pub(crate) fn handle_key(&mut self, event: &KeyEvent) -> bool {
        if is_blocked_key(event) {
            return false;
        }
        let changed = grapheme::edit(&mut self.textarea, event);
        if !changed {
            return false;
        }
        if self.text().width() > self.max_width {
            self.textarea.undo();
            return false;
        }
        true
    }

    /// Returns whether the editor takes `event`, a modified key: every one but a line break or an
    /// indent, neither of which fits a one-line field.
    pub(crate) fn binds_modified_key(event: &KeyEvent) -> bool {
        !is_blocked_key(event)
    }

    /// Inserts pasted `text` as one line, with each run of control characters collapsed to a space.
    /// Keeps a prefix within the available width, dropping the paste if its joined text exceeds the
    /// cap. Returns whether the text changed.
    pub(crate) fn paste(&mut self, text: &str) -> bool {
        let flat = text
            .split(char::is_control)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");

        if flat.is_empty() {
            return false;
        }
        let cursor = self.textarea.cursor().1;
        let ((_, start), (_, end)) = self
            .textarea
            .selection_range()
            .unwrap_or(((0, cursor), (0, cursor)));

        let current = self.text();
        let byte_at = |column| {
            current
                .char_indices()
                .nth(column)
                .map_or(current.len(), |(byte, _)| byte)
        };
        let start = byte_at(start);
        let mut candidate = current.to_owned();
        candidate.replace_range(start..byte_at(end), "");

        let room = self.max_width.saturating_sub(candidate.width());
        let prefix = take_head(&flat, room);
        if prefix.is_empty() {
            return false;
        }
        candidate.insert_str(start, &prefix);
        if candidate.width() > self.max_width {
            return false;
        }
        let changed = self.textarea.insert_str(prefix);
        grapheme::advance_cursor_to_boundary(&mut self.textarea);
        changed
    }

    /// The prompt plus the slice of the text that fits `width`, or the placeholder while empty.
    pub(crate) fn line(&mut self, width: u16, theme: &Theme) -> Line<'static> {
        let prompt = Span::styled(PROMPT, theme.accent());
        let body = match self.placeholder {
            Some(placeholder) if self.text().is_empty() => Span::styled(placeholder, theme.dim()),
            _ => Span::styled(self.visible(width).0, theme.text()),
        };
        Line::from(vec![prompt, body])
    }

    /// Places the terminal cursor at the editing position on row `y`, clamped inside `area`.
    pub(crate) fn place_cursor(&mut self, frame: &mut Frame<'_>, area: Rect, y: u16) {
        let column = saturating_u16(self.visible(area.width).1);
        let raw_x = area.x.saturating_add(PROMPT_WIDTH).saturating_add(column);
        place_clamped(frame, raw_x, y, area);
    }

    /// The text shown in a `width`-column row and the cursor's column within it. The shown slice
    /// scrolls as little as possible to keep the cursor in view.
    fn visible(&mut self, width: u16) -> (String, usize) {
        let budget = usize::from(width.saturating_sub(PROMPT_WIDTH + 1));
        let text = self.text();
        let stops = GraphemeStops::new(text);
        let span = |from: usize, to: usize| stops.column(to) - stops.column(from);
        let len = stops.len();
        let cursor = stops.at_char(self.textarea.cursor().1);

        let mut start = self.scroll.min(cursor);
        while span(start, cursor) > budget {
            start += 1;
        }
        while start > 0 && span(start - 1, len) <= budget {
            start -= 1;
        }
        let mut end = start;
        while end < len && span(start, end + 1) <= budget {
            end += 1;
        }
        let shown = text[stops.byte(start)..stops.byte(end)].to_owned();
        self.scroll = start;
        (shown, span(start, cursor))
    }
}

/// Returns whether the key adds a line break or indent that a one-line field cannot hold.
fn is_blocked_key(event: &KeyEvent) -> bool {
    match event.code {
        KeyCode::Enter | KeyCode::Tab | KeyCode::Char('\n' | '\r') => true,
        KeyCode::Char('j' | 'm') => event.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    // cspell:ignore bcdefghij cdefg mnopqrstu rstuvwxyz
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn type_str(editor: &mut LineEditor, text: &str) {
        for c in text.chars() {
            editor.handle_key(&KeyEvent::from(KeyCode::Char(c)));
        }
    }

    fn cursor_x(editor: &mut LineEditor, width: u16) -> u16 {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| editor.place_cursor(frame, Rect::new(0, 0, width, 1), 0))
            .unwrap();
        terminal.get_cursor_position().unwrap().x
    }

    mod new {
        use super::*;

        #[test]
        fn parks_the_cursor_after_the_initial_text() {
            let mut editor = LineEditor::new("ab");

            type_str(&mut editor, "c");

            assert_eq!(editor.text(), "abc");
        }
    }

    mod with_max_width {
        use super::*;

        #[test]
        fn truncates_and_drops_growth_past_the_cap() {
            let mut editor = LineEditor::new("hello world").with_max_width(5);

            assert_eq!(editor.text(), "hello");

            let inserted = editor.handle_key(&KeyEvent::from(KeyCode::Char('x')));

            assert!(!inserted);
            assert_eq!(editor.text(), "hello", "insert at the cap is dropped");

            let deleted = editor.handle_key(&KeyEvent::from(KeyCode::Backspace));
            let inserted = editor.handle_key(&KeyEvent::from(KeyCode::Char('!')));

            assert!(deleted && inserted);
            assert_eq!(editor.text(), "hell!", "room freed by a delete is usable");
        }

        #[test]
        fn counts_wide_chars_by_column() {
            let mut editor = LineEditor::new("界界界").with_max_width(5);

            assert_eq!(editor.text(), "界界");

            let wide = editor.handle_key(&KeyEvent::from(KeyCode::Char('界')));
            let narrow = editor.handle_key(&KeyEvent::from(KeyCode::Char('a')));

            assert!(!wide);
            assert!(narrow);
            assert_eq!(
                editor.text(),
                "界界a",
                "a narrow char fills the last column"
            );
        }
    }

    mod set_text {
        use super::*;

        #[test]
        fn replaces_the_text_and_parks_the_cursor_at_its_end() {
            let mut editor = LineEditor::new("old");

            editor.set_text("new");
            type_str(&mut editor, "!");

            assert_eq!(editor.text(), "new!");
        }
    }

    mod handle_key {
        use super::*;

        #[test]
        fn inserts_at_the_cursor_after_moving_it() {
            let mut editor = LineEditor::new("acd");
            editor.handle_key(&KeyEvent::from(KeyCode::Left));
            editor.handle_key(&KeyEvent::from(KeyCode::Left));

            let inserted = editor.handle_key(&KeyEvent::from(KeyCode::Char('b')));

            assert!(inserted);
            assert_eq!(editor.text(), "abcd");

            editor.handle_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
            type_str(&mut editor, ">");

            assert_eq!(editor.text(), ">abcd", "Ctrl+A jumps to the start");
        }

        #[test]
        fn supports_readline_word_deletion() {
            let mut editor = LineEditor::new("fix auth bug");

            let deleted =
                editor.handle_key(&KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));

            assert!(deleted);
            assert_eq!(editor.text(), "fix auth ");
        }

        #[test]
        fn never_inserts_control_chords_or_line_breaks() {
            let mut editor = LineEditor::new("ab");

            for event in [
                KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
                KeyEvent::from(KeyCode::Enter),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
                KeyEvent::from(KeyCode::Tab),
                KeyEvent::from(KeyCode::Char('\n')),
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
            ] {
                let edited = editor.handle_key(&event);

                assert!(!edited, "{event:?} must not edit");
            }

            assert_eq!(editor.text(), "ab");
        }

        #[test]
        fn steps_over_grapheme_clusters_in_both_directions() {
            let mut editor = LineEditor::new("a👨‍👩‍👧b");

            editor.handle_key(&KeyEvent::from(KeyCode::Left));
            editor.handle_key(&KeyEvent::from(KeyCode::Left));
            type_str(&mut editor, "<");
            editor.handle_key(&KeyEvent::from(KeyCode::Right));
            type_str(&mut editor, ">");

            assert_eq!(editor.text(), "a<👨‍👩‍👧>b");
        }

        #[test]
        fn deletes_graphemes_in_one_undo_step() {
            for (name, code, modifiers, backward) in [
                ("Backspace", KeyCode::Backspace, KeyModifiers::NONE, true),
                ("Ctrl+H", KeyCode::Char('h'), KeyModifiers::CONTROL, true),
                ("Delete", KeyCode::Delete, KeyModifiers::NONE, false),
                ("Ctrl+D", KeyCode::Char('d'), KeyModifiers::CONTROL, false),
            ] {
                for grapheme in ["e\u{301}", "❤️", "👨‍👩‍👧"] {
                    let text = format!("a{grapheme}b");
                    let mut editor = LineEditor::new(&text);
                    let column = if backward {
                        1 + grapheme.chars().count()
                    } else {
                        1
                    };
                    editor
                        .textarea
                        .move_cursor(CursorMove::Jump(0, saturating_u16(column)));
                    editor.textarea.set_yank_text(" saved ");

                    let deleted = editor.handle_key(&KeyEvent::new(code, modifiers));
                    assert!(deleted, "{name}: {grapheme:?}");

                    assert_eq!(editor.text(), "ab", "{name}: {grapheme:?}");
                    assert_eq!(editor.textarea.cursor(), (0, 1), "{name}: {grapheme:?}");

                    let undone = editor
                        .handle_key(&KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
                    assert!(undone, "{name}: {grapheme:?}");

                    assert_eq!(editor.text(), text, "{name}: {grapheme:?}");

                    let redone = editor
                        .handle_key(&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
                    assert!(redone, "{name}: {grapheme:?}");

                    assert_eq!(editor.text(), "ab", "{name}: {grapheme:?}");

                    let pasted = editor
                        .handle_key(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
                    assert!(pasted, "{name}: {grapheme:?}");

                    assert_eq!(editor.text(), "a saved b", "{name}: {grapheme:?}");
                }
            }
        }

        #[test]
        fn deletion_at_the_buffer_edge_keeps_the_text() {
            for text in ["", "ab"] {
                for (code, movement) in [
                    (KeyCode::Backspace, CursorMove::Head),
                    (KeyCode::Delete, CursorMove::End),
                ] {
                    let mut editor = LineEditor::new(text);
                    editor.textarea.move_cursor(movement);

                    let deleted = editor.handle_key(&KeyEvent::from(code));

                    assert!(!deleted, "{code:?}: {text:?}");
                    assert_eq!(editor.text(), text, "{code:?}: {text:?}");
                }
            }
        }

        #[test]
        fn honors_selections_during_grapheme_deletion() {
            for (code, empty_selection_text) in
                [(KeyCode::Backspace, "axb"), (KeyCode::Delete, "a❤️b")]
            {
                for (start, end, expected) in
                    [(1, 4, "ab"), (4, 1, "ab"), (3, 3, empty_selection_text)]
                {
                    let mut editor = LineEditor::new("a❤️xb");
                    editor.textarea.move_cursor(CursorMove::Jump(0, start));
                    editor.textarea.start_selection();
                    editor.textarea.move_cursor(CursorMove::Jump(0, end));

                    let deleted = editor.handle_key(&KeyEvent::from(code));
                    assert!(deleted, "{code:?}: {start} → {end}");

                    assert_eq!(editor.text(), expected, "{code:?}: {start} → {end}");

                    let undone = editor
                        .handle_key(&KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
                    assert!(undone, "{code:?}: {start} → {end}");

                    assert_eq!(editor.text(), "a❤️xb", "{code:?}: {start} → {end}");
                }
            }
        }

        #[test]
        fn keeps_graphemes_joined_by_a_deletion() {
            for (code, column) in [(KeyCode::Backspace, 2), (KeyCode::Delete, 1)] {
                let mut editor = LineEditor::new("🇦x🇧");
                editor.textarea.move_cursor(CursorMove::Jump(0, column));

                let deleted = editor.handle_key(&KeyEvent::from(code));
                assert!(deleted, "{code:?}");

                assert_eq!(editor.text(), "🇦🇧", "{code:?}");
                assert_eq!(editor.textarea.cursor(), (0, 2), "{code:?}");

                let undone =
                    editor.handle_key(&KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
                assert!(undone, "{code:?}");

                assert_eq!(editor.text(), "🇦x🇧", "{code:?}");

                let redone =
                    editor.handle_key(&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
                assert!(redone, "{code:?}");

                assert_eq!(editor.text(), "🇦🇧", "{code:?}");
                assert_eq!(editor.textarea.cursor(), (0, 2), "{code:?}");
            }
        }

        #[test]
        fn inserts_once_when_joining_adjacent_graphemes() {
            let mut editor = LineEditor::new("👨👩");
            editor.handle_key(&KeyEvent::from(KeyCode::Left));

            let inserted = editor.handle_key(&KeyEvent::from(KeyCode::Char('\u{200d}')));
            assert!(inserted);

            assert_eq!(editor.text(), "👨‍👩");
            assert_eq!(editor.textarea.cursor(), (0, 3));

            let undone =
                editor.handle_key(&KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
            assert!(undone);

            assert_eq!(editor.text(), "👨👩");

            let redone =
                editor.handle_key(&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
            assert!(redone);

            assert_eq!(editor.text(), "👨‍👩");
            assert_eq!(editor.textarea.cursor(), (0, 3));
        }

        #[test]
        fn undo_restores_only_the_last_edit_inside_a_cluster() {
            let mut editor = LineEditor::new("❤");
            type_str(&mut editor, "\u{fe0f}");

            let undone =
                editor.handle_key(&KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));

            assert!(undone);
            assert_eq!(editor.text(), "❤");
        }
    }

    mod paste {
        use super::*;

        #[test]
        fn inserts_at_the_cursor_with_line_breaks_collapsed() {
            let mut editor = LineEditor::new("[]");
            editor.handle_key(&KeyEvent::from(KeyCode::Left));

            let pasted = editor.paste("\nfix\r\nauth\n\tflow\n");

            assert!(pasted);
            assert_eq!(editor.text(), "[fix auth flow]");
        }

        #[test]
        fn parks_the_cursor_after_graphemes_joined_by_a_paste() {
            for (initial, column, text, expected, end) in
                [("👨👩", 1, "\u{200d}", "👨‍👩", 3), ("🇧", 0, "🇦", "🇦🇧", 2)]
            {
                let mut editor = LineEditor::new(initial);
                editor.textarea.move_cursor(CursorMove::Jump(0, column));

                let pasted = editor.paste(text);
                assert!(pasted, "{initial:?}: {text:?}");

                assert_eq!(editor.text(), expected, "{initial:?}: {text:?}");
                assert_eq!(editor.textarea.cursor(), (0, end), "{initial:?}: {text:?}");

                let undone =
                    editor.handle_key(&KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
                assert!(undone, "{initial:?}: {text:?}");

                assert_eq!(editor.text(), initial, "{initial:?}: {text:?}");

                let redone =
                    editor.handle_key(&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
                assert!(redone, "{initial:?}: {text:?}");

                assert_eq!(editor.text(), expected, "{initial:?}: {text:?}");
                assert_eq!(editor.textarea.cursor(), (0, end), "{initial:?}: {text:?}");

                let deleted = editor.handle_key(&KeyEvent::from(KeyCode::Backspace));
                assert!(deleted, "{initial:?}: {text:?}");

                assert_eq!(editor.text(), "", "{initial:?}: {text:?}");
            }
        }

        #[test]
        fn keeps_only_what_fits_under_the_cap() {
            for (text, expected) in [("cdefg", "abcdef"), ("c界d界", "abc界d")] {
                let mut editor = LineEditor::new("ab").with_max_width(6);

                let pasted = editor.paste(text);
                assert!(pasted, "{text:?}");

                assert_eq!(editor.text(), expected, "{text:?}");

                let past_the_cap = editor.paste("x");
                let line_breaks = editor.paste("\n\n");

                assert!(!past_the_cap, "{text:?}: a full field takes nothing");
                assert!(!line_breaks, "{text:?}: line breaks alone insert nothing");
                assert_eq!(editor.text(), expected, "{text:?}");
            }
        }

        #[test]
        fn clips_replacement_text_to_the_space_freed_by_the_selection() {
            for (start, end) in [(1, 3), (3, 1)] {
                let mut editor = LineEditor::new("abcd").with_max_width(4);
                editor.textarea.move_cursor(CursorMove::Jump(0, start));
                editor.textarea.start_selection();
                editor.textarea.move_cursor(CursorMove::Jump(0, end));
                editor.textarea.set_yank_text("saved");

                let pasted = editor.paste("界界");
                assert!(pasted, "{start} → {end}");

                assert_eq!(editor.text(), "a界d", "{start} → {end}");
                assert_eq!(editor.textarea.cursor(), (0, 2), "{start} → {end}");
                assert_eq!(editor.textarea.selection_range(), None, "{start} → {end}");
                assert_eq!(editor.textarea.yank_text(), "saved", "{start} → {end}");
            }
        }

        #[test]
        fn keeps_whole_graphemes_under_the_cap() {
            let mut family = LineEditor::new("").with_max_width(4);
            let mut heart = LineEditor::new("abcde").with_max_width(6);

            let family_changed = family.paste("👨‍👩‍👧👨‍👩‍👧👨‍👩‍👧");
            let heart_changed = heart.paste("❤️");

            assert!(family_changed);
            assert_eq!(family.text(), "👨‍👩‍👧👨‍👩‍👧", "a ZWJ sequence is never split");
            assert!(!heart_changed);
            assert_eq!(heart.text(), "abcde", "a VS16 emoji takes two columns");
        }

        #[test]
        fn validates_joined_graphemes_against_the_cap() {
            for (cap, text, expected, changed) in [
                (1, "\u{fe0f}", "❤", false),
                (2, "\u{fe0f}", "❤️", true),
                (2, "\u{fe0f}x", "❤", false),
                (3, "\u{fe0f}x界", "❤️x", true),
            ] {
                let mut editor = LineEditor::new("❤").with_max_width(cap);
                editor.textarea.set_yank_text("saved");

                let pasted = editor.paste(text);

                assert_eq!(pasted, changed, "cap {cap}: {text:?}");
                assert_eq!(editor.text(), expected, "cap {cap}: {text:?}");
                assert_eq!(
                    editor.textarea.cursor(),
                    (0, expected.chars().count()),
                    "cap {cap}: {text:?}",
                );
                assert_eq!(editor.textarea.yank_text(), "saved", "cap {cap}: {text:?}");

                let undone =
                    editor.handle_key(&KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));

                assert_eq!(undone, changed, "cap {cap}: {text:?}");
                assert_eq!(editor.text(), "❤", "cap {cap}: {text:?}");

                let redone =
                    editor.handle_key(&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));

                assert_eq!(redone, changed, "cap {cap}: {text:?}");
                assert_eq!(editor.text(), expected, "cap {cap}: {text:?}");
            }
        }

        #[test]
        fn ignores_inadmissible_content_without_changing_selection_or_history() {
            for (initial, cap, start, end, text) in [
                ("a", 1, 1, 0, ""),
                ("a", 1, 1, 0, "\n"),
                ("a", 1, 1, 0, "\r\n\t"),
                ("a", 1, 1, 0, "界"),
                ("❤a", 2, 2, 1, "\u{fe0f}x"),
            ] {
                let mut editor = LineEditor::new(initial).with_max_width(cap);
                type_str(&mut editor, "\u{301}");
                editor.handle_key(&KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
                editor.textarea.move_cursor(CursorMove::Jump(0, start));
                editor.textarea.start_selection();
                editor.textarea.move_cursor(CursorMove::Jump(0, end));
                let selection = editor.textarea.selection_range();
                editor.textarea.set_yank_text("saved");

                let pasted = editor.paste(text);

                assert!(!pasted, "{text:?}");
                assert_eq!(editor.text(), initial, "{text:?}");
                assert_eq!(editor.textarea.cursor(), (0, usize::from(end)), "{text:?}");
                assert_eq!(editor.textarea.selection_range(), selection, "{text:?}");
                assert_eq!(editor.textarea.yank_text(), "saved", "{text:?}");

                let redone =
                    editor.handle_key(&KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
                assert!(redone, "{text:?}");

                assert_eq!(editor.text(), format!("{initial}\u{301}"), "{text:?}");
            }
        }
    }

    mod line {
        use super::*;

        /// The rendered row and the cursor column at width 12.
        fn shown(editor: &mut LineEditor) -> (String, u16) {
            let line = editor.line(12, &Theme::default()).to_string();
            (line, cursor_x(editor, 12))
        }

        #[test]
        fn shows_the_placeholder_only_while_empty() {
            let theme = Theme::default();
            let mut editor = LineEditor::new("").with_placeholder("Type to filter");

            assert_eq!(editor.line(40, &theme).to_string(), "/ Type to filter");

            type_str(&mut editor, "ab");

            assert_eq!(editor.line(40, &theme).to_string(), "/ ab");
        }

        #[test]
        fn scrolls_as_little_as_possible_to_keep_the_cursor_in_view() {
            // Width 12 leaves 9 columns for text beside the prompt and the end-of-line cursor.
            let mut editor = LineEditor::new("abcdefghijklmnopqrstuvwxyz");

            assert_eq!(shown(&mut editor), ("/ rstuvwxyz".to_owned(), 11));

            editor.handle_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));

            assert_eq!(shown(&mut editor), ("/ abcdefghi".to_owned(), 2));

            for _ in 0..10 {
                editor.handle_key(&KeyEvent::from(KeyCode::Right));
            }

            assert_eq!(shown(&mut editor), ("/ bcdefghij".to_owned(), 11));

            for _ in 0..5 {
                editor.handle_key(&KeyEvent::from(KeyCode::Left));
            }

            assert_eq!(
                shown(&mut editor),
                ("/ bcdefghij".to_owned(), 6),
                "moving back inside the row keeps the slice",
            );
        }

        #[test]
        fn refills_the_row_when_the_tail_shrinks() {
            let mut editor = LineEditor::new("abcdefghijklmnopqrstuvwxyz");

            assert_eq!(shown(&mut editor).0, "/ rstuvwxyz");

            for _ in 0..5 {
                editor.handle_key(&KeyEvent::from(KeyCode::Backspace));
            }

            assert_eq!(shown(&mut editor), ("/ mnopqrstu".to_owned(), 11));
        }

        #[test]
        fn slices_wide_text_by_display_width() {
            let mut editor = LineEditor::new("日本語テキスト");

            let row = shown(&mut editor);

            assert_eq!(row, ("/ テキスト".to_owned(), 10));
        }

        #[test]
        fn slices_emoji_sequences_as_whole_clusters() {
            let mut hearts = LineEditor::new(&"❤️".repeat(12));
            let mut family = LineEditor::new("👨‍👩‍👧x");

            assert_eq!(shown(&mut hearts), (format!("/ {}", "❤️".repeat(4)), 10));
            assert_eq!(shown(&mut family), ("/ 👨‍👩‍👧x".to_owned(), 5));
        }
    }

    mod place_cursor {
        use super::*;

        #[test]
        fn tracks_the_editing_position_by_display_width() {
            let mut editor = LineEditor::new("日本");

            assert_eq!(
                cursor_x(&mut editor, 20),
                PROMPT_WIDTH + 4,
                "wide glyphs count twice"
            );

            editor.handle_key(&KeyEvent::from(KeyCode::Left));

            assert_eq!(cursor_x(&mut editor, 20), PROMPT_WIDTH + 2);
        }

        #[test]
        fn measures_emoji_sequences_as_ratatui_paints_them() {
            let mut editor = LineEditor::new("❤️x");

            assert_eq!(cursor_x(&mut editor, 20), PROMPT_WIDTH + 3);

            editor.handle_key(&KeyEvent::from(KeyCode::Left));

            assert_eq!(
                cursor_x(&mut editor, 20),
                PROMPT_WIDTH + 2,
                "the cursor sits on the x"
            );

            editor.handle_key(&KeyEvent::from(KeyCode::Left));

            assert_eq!(
                cursor_x(&mut editor, 20),
                PROMPT_WIDTH,
                "Left steps over the whole emoji"
            );
        }
    }

    mod prompt_width {
        use super::*;

        #[test]
        fn matches_display_width() {
            assert_eq!(PROMPT.width(), usize::from(PROMPT_WIDTH));
        }
    }
}
