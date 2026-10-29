//! Grapheme-cluster-aware editing shared by the prompt input and the modal text fields.
//!
//! `TextArea` counts and deletes in `char`s, which splits a cluster such as `👨‍👩‍👧` or `e`
//! followed by U+0301 into code points, and its emacs table puts Ctrl+J, Ctrl+U, and Ctrl+V on
//! keys that readline binds elsewhere. One table here serves both callers, so the prompt and every
//! modal field agree on what a key does.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{CursorMove, DataCursor, TextArea};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

// ── Key Table ──

/// Applies one editing key, stepping cursor moves and single deletions over whole grapheme
/// clusters. Returns whether the text changed. A key the table does not claim keeps the
/// textarea's emacs binding.
pub(crate) fn edit(textarea: &mut TextArea<'_>, key: &KeyEvent) -> bool {
    let modifiers = key.modifiers - KeyModifiers::SHIFT;
    let changed = match (key.code, modifiers) {
        (KeyCode::Backspace, KeyModifiers::NONE) | (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
            delete(textarea, Side::Before)
        }
        (KeyCode::Delete, KeyModifiers::NONE) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            delete(textarea, Side::After)
        }
        // Terminals report Shift+Enter as a line break, and the prompt reads Ctrl+J the same way.
        (KeyCode::Char('j' | '\n'), KeyModifiers::CONTROL) => {
            textarea.insert_newline();
            true
        }
        // Readline kills to the line start and keeps the text for Ctrl+Y, where the default undoes.
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => kill_to_head(textarea),
        // Terminal paste never reaches the textarea as a key, so the page-scroll default only
        // surprises a readline user.
        (KeyCode::Char('v'), KeyModifiers::CONTROL) => false,
        // Ctrl+_ arrives as Ctrl+7 from a legacy terminal (0x1F) and as Ctrl+Shift+- under the
        // kitty keyboard protocol, which reports the base key with Shift as a modifier.
        (KeyCode::Char('z' | '_' | '-' | '7'), KeyModifiers::CONTROL) => textarea.undo(),
        _ => textarea.input(*key),
    };

    let moves_back = !changed
        && match key.code {
            KeyCode::Left => true,
            KeyCode::Char('b') => modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
            _ => false,
        };
    if moves_back {
        retreat_cursor_to_boundary(textarea);
    } else {
        advance_cursor_to_boundary(textarea);
    }
    changed
}

/// Removes the text from the cursor back to the row start as one history entry, leaving it in the
/// yank buffer for Ctrl+Y.
fn kill_to_head(textarea: &mut TextArea<'_>) -> bool {
    if textarea.cursor().1 == 0 {
        return false;
    }
    textarea.start_selection();
    textarea.move_cursor(CursorMove::Head);
    textarea.cut()
}

// ── Cluster Deletion ──

/// Which side of the cursor a deletion removes from.
#[derive(Debug, Clone, Copy)]
enum Side {
    Before,
    After,
}

/// Deletes the cluster on `side` of the cursor, or the cluster around a cursor inside one, as one
/// history entry and leaves the yank buffer alone. At the start or end of a row, the cluster is
/// the line break, which joins the rows.
fn delete(textarea: &mut TextArea<'_>, side: Side) -> bool {
    if textarea
        .selection_range()
        .is_some_and(|(start, end)| start != end)
    {
        return textarea.delete_str(0);
    }

    let DataCursor(row, col) = textarea.cursor();
    let stops = GraphemeStops::new(&textarea.lines()[row]);
    let at = stops.at_char(col);
    let cluster = if stops.char(at) == col {
        match side {
            Side::Before if at == 0 => return textarea.delete_char(),
            Side::After if at == stops.len() => return textarea.delete_next_char(),
            Side::Before => at - 1,
            Side::After => at,
        }
    } else {
        at - 1
    };

    let (start, end) = (stops.char(cluster), stops.char(cluster + 1));
    for _ in start..col {
        textarea.move_cursor(CursorMove::Back);
    }
    textarea.start_selection();
    for _ in start..end {
        textarea.move_cursor(CursorMove::Forward);
    }
    textarea.delete_str(0)
}

/// Moves the cursor to the first grapheme boundary at or after its column, since an edit, a
/// paste, or a move by display column can leave it inside a cluster.
pub(crate) fn advance_cursor_to_boundary(textarea: &mut TextArea<'_>) {
    let DataCursor(row, col) = textarea.cursor();
    let stops = GraphemeStops::new(&textarea.lines()[row]);
    for _ in col..stops.char(stops.at_char(col)) {
        textarea.move_cursor(CursorMove::Forward);
    }
}

/// Moves the cursor back to the start of the cluster it sits inside, so a backward move steps over
/// the whole cluster.
fn retreat_cursor_to_boundary(textarea: &mut TextArea<'_>) {
    let DataCursor(row, col) = textarea.cursor();
    let stops = GraphemeStops::new(&textarea.lines()[row]);
    let at = stops.at_char(col);
    if stops.char(at) == col {
        return;
    }
    for _ in stops.char(at - 1)..col {
        textarea.move_cursor(CursorMove::Back);
    }
}

// ── Cluster Stops ──

/// The boundary before each grapheme cluster of a line, then its end, with the byte offset, char
/// index, and display column of each. Columns sum grapheme widths, as ratatui paints them.
pub(crate) struct GraphemeStops(Vec<(usize, usize, usize)>);

impl GraphemeStops {
    pub(crate) fn new(text: &str) -> Self {
        let mut stops = Vec::new();
        let (mut chars, mut column) = (0, 0);
        for (byte, grapheme) in text.grapheme_indices(true) {
            stops.push((byte, chars, column));
            chars += grapheme.chars().count();
            column += grapheme.width();
        }
        stops.push((text.len(), chars, column));
        Self(stops)
    }

    /// The number of grapheme clusters, which is also the index of the end stop.
    pub(crate) fn len(&self) -> usize {
        self.0.len() - 1
    }

    pub(crate) fn byte(&self, stop: usize) -> usize {
        self.0[stop].0
    }

    pub(crate) fn char(&self, stop: usize) -> usize {
        self.0[stop].1
    }

    pub(crate) fn column(&self, stop: usize) -> usize {
        self.0[stop].2
    }

    /// The first stop at or after char index `char`.
    pub(crate) fn at_char(&self, char: usize) -> usize {
        self.0.partition_point(|&(_, start, _)| start < char)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn textarea_with(text: &str) -> TextArea<'static> {
        let mut textarea = TextArea::new(vec![text.to_owned()]);
        textarea.move_cursor(CursorMove::End);
        textarea
    }

    mod delete {
        use super::*;

        /// Each cluster's code points are removed in one Backspace, whatever the cursor moves
        /// through, and the cursor lands on the next cluster's boundary.
        #[test]
        fn removes_whole_clusters_on_either_side() {
            for (text, side, expected) in [
                ("a👨‍👩‍👧", Side::Before, "a"),
                ("🇯🇵x", Side::Before, "🇯🇵"),
                ("e\u{301}x", Side::Before, "e\u{301}"),
                ("a👨‍👩‍👧", Side::After, "👨‍👩‍👧"),
            ] {
                let mut textarea = textarea_with(text);
                if matches!(side, Side::After) {
                    textarea.move_cursor(CursorMove::Head);
                }

                assert!(delete(&mut textarea, side), "{text:?} {side:?}");

                assert_eq!(textarea.lines(), [expected], "{text:?} {side:?}");
                let cursor = match side {
                    Side::Before => expected.chars().count(),
                    Side::After => 0,
                };
                assert_eq!(textarea.cursor(), (0, cursor), "{text:?} {side:?}");
            }
        }

        #[test]
        fn joins_rows_at_a_line_edge() {
            let mut textarea = TextArea::new(vec!["ab".to_owned(), "cd".to_owned()]);
            textarea.move_cursor(CursorMove::Head);
            textarea.move_cursor(CursorMove::Down);

            assert!(delete(&mut textarea, Side::Before));

            assert_eq!(textarea.lines(), ["abcd"]);
            assert_eq!(textarea.cursor(), (0, 2));

            let mut textarea = TextArea::new(vec!["ab".to_owned(), "cd".to_owned()]);
            textarea.move_cursor(CursorMove::End);

            assert!(delete(&mut textarea, Side::After));

            assert_eq!(textarea.lines(), ["abcd"]);
            assert_eq!(textarea.cursor(), (0, 2));
        }

        #[test]
        fn removes_the_cluster_around_a_cursor_inside_it() {
            for side in [Side::Before, Side::After] {
                let mut textarea = TextArea::new(vec!["ae\u{301}x".to_owned()]);
                textarea.move_cursor(CursorMove::Jump(0, 2));

                assert!(delete(&mut textarea, side), "{side:?}");

                assert_eq!(textarea.lines(), ["ax"], "{side:?}");
                assert_eq!(textarea.cursor(), (0, 1), "{side:?}");
            }
        }

        #[test]
        fn removes_a_selection_as_one_entry() {
            let mut textarea = TextArea::new(vec!["abc".to_owned()]);
            textarea.move_cursor(CursorMove::Head);
            textarea.start_selection();
            textarea.move_cursor(CursorMove::Forward);
            textarea.move_cursor(CursorMove::Forward);

            assert!(delete(&mut textarea, Side::Before));

            assert_eq!(textarea.lines(), ["c"]);
        }
    }

    mod edit {
        use super::*;

        #[test]
        fn steps_cursor_moves_to_the_next_cluster_boundary() {
            let mut textarea = textarea_with("a👨‍👩‍👧b");
            textarea.move_cursor(CursorMove::Head);

            assert!(!edit(&mut textarea, &KeyEvent::from(KeyCode::Right)));

            assert_eq!(textarea.cursor(), (0, 1));
        }

        #[test]
        fn steps_over_a_whole_cluster_in_both_directions() {
            let mut textarea = textarea_with("a👨‍👩‍👧b");
            textarea.move_cursor(CursorMove::Head);
            assert!(!edit(&mut textarea, &KeyEvent::from(KeyCode::Right)));
            assert_eq!(textarea.cursor(), (0, 1));

            assert!(!edit(&mut textarea, &KeyEvent::from(KeyCode::Right)));
            assert_eq!(textarea.cursor(), (0, 6));

            assert!(!edit(&mut textarea, &KeyEvent::from(KeyCode::Left)));
            assert_eq!(textarea.cursor(), (0, 1));
        }

        /// Up and Down keep the display column, which can land inside a cluster on the next row.
        #[test]
        fn snaps_a_vertical_move_past_the_cluster_it_lands_in() {
            for (lines, key, from, to) in [
                (["ab", "e\u{301}x"], KeyCode::Down, 0, 1),
                (["e\u{301}x", "ab"], KeyCode::Up, 1, 0),
            ] {
                let mut textarea = TextArea::new(lines.map(str::to_owned).to_vec());
                textarea.move_cursor(CursorMove::Jump(from, 1));

                assert!(!edit(&mut textarea, &KeyEvent::from(key)), "{key:?}");

                assert_eq!(textarea.cursor(), (to, 2), "{key:?}");
                assert!(edit(&mut textarea, &KeyEvent::from(KeyCode::Backspace)));
                let mut expected = lines;
                expected[to] = "x";
                assert_eq!(textarea.lines(), expected, "{key:?}");
            }
        }

        #[test]
        fn pairs_each_readline_chord_with_its_edit() {
            let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);

            let mut textarea = textarea_with("onetwo");
            for _ in 0..3 {
                textarea.move_cursor(CursorMove::Back);
            }
            assert!(edit(&mut textarea, &ctrl('j')));
            assert_eq!(textarea.lines(), ["one", "two"], "Ctrl+J breaks the line");

            let mut textarea = textarea_with("keep cut");
            assert!(edit(&mut textarea, &ctrl('u')));
            assert_eq!(textarea.lines(), [""], "Ctrl+U kills to the row start");
            assert_eq!(textarea.yank_text(), "keep cut");
            assert!(!edit(&mut textarea, &ctrl('u')), "Ctrl+U at the row start");
            assert_eq!(textarea.yank_text(), "keep cut");
            assert!(edit(&mut textarea, &ctrl('y')));
            assert_eq!(textarea.lines(), ["keep cut"], "Ctrl+Y pastes the kill");

            // Ctrl+_ as a legacy terminal (Ctrl+7) and the kitty protocol (Ctrl+Shift+-) report it.
            for undo in [
                ctrl('z'),
                ctrl('_'),
                ctrl('7'),
                KeyEvent::new(
                    KeyCode::Char('-'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
            ] {
                let mut textarea = textarea_with("ab");
                textarea.move_cursor(CursorMove::Head);
                assert!(edit(&mut textarea, &KeyEvent::from(KeyCode::Char('x'))));
                assert_eq!(textarea.lines(), ["xab"]);

                assert!(edit(&mut textarea, &undo), "{undo:?}");
                assert_eq!(textarea.lines(), ["ab"], "{undo:?} undoes the insert");
            }

            let mut textarea = textarea_with("ab");
            assert!(edit(&mut textarea, &ctrl('h')));
            assert_eq!(textarea.lines(), ["a"], "Ctrl+H deletes back");

            let mut textarea = textarea_with("ab");
            textarea.move_cursor(CursorMove::Head);
            assert!(edit(&mut textarea, &ctrl('d')));
            assert_eq!(textarea.lines(), ["b"], "Ctrl+D deletes forward");
        }

        #[test]
        fn leaves_ctrl_v_to_the_terminal_paste() {
            let mut textarea = textarea_with("ab");

            let handled = edit(
                &mut textarea,
                &KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            );

            assert!(!handled);
            assert_eq!(textarea.lines(), ["ab"]);
            assert_eq!(textarea.cursor(), (0, 2));
        }

        #[test]
        fn keeps_clusters_whole_when_typing_joins_them() {
            let mut textarea = textarea_with("e\u{301}x");
            textarea.move_cursor(CursorMove::Head);
            textarea.move_cursor(CursorMove::Forward);

            assert!(edit(
                &mut textarea,
                &KeyEvent::new(KeyCode::Char('\u{301}'), KeyModifiers::NONE)
            ));

            assert_eq!(textarea.cursor(), (0, 3), "the mark joins the cluster");
        }
    }
}
