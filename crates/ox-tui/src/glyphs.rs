//! Visual glyphs shared by the TUI and the stdio drivers.
//!
//! Conventions:
//!
//! - First-line prefix glyphs end with a trailing space (`"❯ "`, `"◉ "`, `"✗ "`).
//! - The user, assistant, and error continuation indents (`*_CONT`) match their prefix's display
//!   width. Tool bodies (`TOOL_BORDER_CONT`) indent two columns past `TOOL_BORDER_PREFIX`.
//! - Tool status indicators (`✓` / `✗`) carry no trailing space. The renderer handles the gap.

use ox_core::tool::tasks::TaskStatus;
use ox_core::tool::view::DiffLineKind;

// ── User Prompt ──

/// First-line prefix for user-typed prompts.
pub(crate) const USER_PROMPT_PREFIX: &str = "❯ ";

/// Display width of [`USER_PROMPT_PREFIX`] in terminal columns.
pub(crate) const USER_PROMPT_PREFIX_WIDTH: u16 = 2;

/// Continuation indent for multi-line user-typed content.
pub(crate) const USER_PROMPT_CONT: &str = "  ";

// ── Assistant ──

/// First-line prefix for assistant text.
pub(crate) const ASSISTANT_PREFIX: &str = "◉ ";

/// Continuation indent for multi-line assistant content.
pub(crate) const ASSISTANT_CONT: &str = "  ";

// ── Tool / Thinking ──

/// Left-bar glyph for tool and thinking blocks.
pub(crate) const BAR: &str = "▎";

/// Per-line prefix for thinking blocks.
pub(crate) const THINKING_PREFIX: &str = "▎ ";

/// First-line prefix for tool-call and tool-result status lines.
pub(crate) const TOOL_BORDER_PREFIX: &str = "▎ ";

/// Continuation prefix for tool-block body lines (content at col 4).
pub(crate) const TOOL_BORDER_CONT: &str = "▎   ";

/// Tool-result success indicator.
pub(crate) const TOOL_SUCCESS: &str = "✓";

/// Tool-result failure indicator.
pub(crate) const TOOL_ERROR: &str = "✗";

// ── Tasks ──

/// Marker before a task, by status.
pub const fn task_marker(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "○",
        TaskStatus::InProgress => "●",
        TaskStatus::Completed => "✓",
    }
}

// ── Error ──

/// First-line prefix for error blocks.
pub(crate) const ERROR_PREFIX: &str = "✗ ";

/// Continuation indent for multi-line error messages.
pub(crate) const ERROR_CONT: &str = "  ";

/// Prefix for an error shown inside a modal, such as a failed delete or session load.
pub(crate) const MODAL_ERROR_PREFIX: &str = "! ";

// ── Pickers ──

/// Prefix of the highlighted row in list and searchable pickers.
const CURSOR_MARKER: &str = "> ";

/// Display width of [`CURSOR_MARKER`] in terminal columns.
pub(crate) const CURSOR_MARKER_WIDTH: u16 = 2;

/// A picker row's gutter: [`CURSOR_MARKER`] on the cursor row, and blank columns of the same width
/// on the others.
pub(crate) const fn cursor_gutter(is_cursor: bool) -> &'static str {
    if is_cursor { CURSOR_MARKER } else { "  " }
}

/// Marker after the label of a list picker's active row.
pub(crate) const ACTIVE_MARKER: &str = "✓";

/// Dot for the chosen effort level.
pub(crate) const RADIO_SELECTED: char = '●';

/// Dot for an effort level on the slider that is not chosen.
pub(crate) const RADIO_UNSELECTED: char = '○';

// ── Rules and Tables ──

/// Horizontal line segment, repeated to draw rules and the effort slider track.
pub(crate) const HORIZONTAL_RULE: &str = "─";

/// Border between markdown table cells.
pub(crate) const VERTICAL_RULE: &str = "│";

/// Per-line prefix for markdown blockquotes.
pub(crate) const BLOCKQUOTE_PREFIX: &str = "│ ";

/// Corner and junction glyphs of one horizontal rule in a markdown table.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TableRule {
    pub(crate) left: char,
    pub(crate) join: char,
    pub(crate) right: char,
}

/// Rule above a table's first row.
pub(crate) const TABLE_TOP: TableRule = TableRule {
    left: '┌',
    join: '┬',
    right: '┐',
};

/// Rule between two rows of a table.
pub(crate) const TABLE_MIDDLE: TableRule = TableRule {
    left: '├',
    join: '┼',
    right: '┤',
};

/// Rule below a table's last row.
pub(crate) const TABLE_BOTTOM: TableRule = TableRule {
    left: '└',
    join: '┴',
    right: '┘',
};

// ── Separators ──

/// Separator between status bar segments.
pub(crate) const SEGMENT_SEPARATOR: &str = " │ ";

/// Separator between items on one line of the welcome screen.
pub(crate) const INLINE_SEPARATOR: &str = " · ";

// ── Inline Markers ──

/// Inline newline replacement for collapsed multi-line previews.
pub(crate) const NEWLINE_GLYPH: &str = " ⏎ ";

/// Line marking where a cancelled turn's reply stopped, in the chat and on stderr under `--no-tui`.
pub const INTERRUPTED_MARKER: &str = "(interrupted)";

// ── Diffs ──

/// The row between two diff hunks, right-aligned in the number column.
pub const HUNK_GAP: &str = "⋮";

/// The separator between a diff row's number and its text, which signs a changed row.
pub const fn diff_sign(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Context => " │ ",
        DiffLineKind::Removed => " - ",
        DiffLineKind::Added => " + ",
    }
}

// ── Status Spinner ──

/// 8-dot Braille spinner frames whose one empty dot moves clockwise, indexed modulo their count.
pub(crate) const SPINNER_FRAMES: &[char] = &['⣷', '⣯', '⣟', '⡿', '⢿', '⣻', '⣽', '⣾'];

#[cfg(test)]
mod tests {
    use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

    use super::*;

    mod task_marker {
        use super::*;

        #[test]
        fn every_status_renders_in_one_column() {
            for status in [
                TaskStatus::Pending,
                TaskStatus::InProgress,
                TaskStatus::Completed,
            ] {
                assert_eq!(task_marker(status).width(), 1, "{status:?}");
            }
        }
    }

    mod cursor_gutter {
        use super::*;

        #[test]
        fn marks_only_the_cursor_row_at_the_marker_width() {
            assert_eq!(cursor_gutter(true), CURSOR_MARKER);
            assert_eq!(cursor_gutter(false), " ".repeat(CURSOR_MARKER.width()));
        }
    }

    mod user_prompt_prefix_width {
        use super::*;

        #[test]
        fn matches_display_width() {
            assert_eq!(
                USER_PROMPT_PREFIX.width(),
                usize::from(USER_PROMPT_PREFIX_WIDTH),
            );
        }
    }

    mod user_prompt_cont {
        use super::*;

        #[test]
        fn aligns_with_prefix_width() {
            assert_eq!(USER_PROMPT_CONT.width(), USER_PROMPT_PREFIX.width());
        }
    }

    mod assistant_cont {
        use super::*;

        #[test]
        fn aligns_with_prefix_width() {
            assert_eq!(ASSISTANT_CONT.width(), ASSISTANT_PREFIX.width());
        }
    }

    mod thinking_prefix {
        use super::*;

        #[test]
        fn starts_with_bar() {
            assert!(
                THINKING_PREFIX.starts_with(BAR),
                "THINKING_PREFIX ({THINKING_PREFIX:?}) must start with BAR ({BAR:?})",
            );
        }
    }

    mod tool_border_prefix {
        use super::*;

        #[test]
        fn starts_with_bar() {
            assert!(
                TOOL_BORDER_PREFIX.starts_with(BAR),
                "TOOL_BORDER_PREFIX ({TOOL_BORDER_PREFIX:?}) must start with BAR ({BAR:?})",
            );
        }
    }

    mod tool_border_cont {
        use super::*;

        #[test]
        fn starts_with_bar() {
            assert!(
                TOOL_BORDER_CONT.starts_with(BAR),
                "TOOL_BORDER_CONT ({TOOL_BORDER_CONT:?}) must start with BAR ({BAR:?})",
            );
        }
    }

    mod tool_error {
        use super::*;

        #[test]
        fn matches_error_prefix_glyph() {
            assert!(
                ERROR_PREFIX.starts_with(TOOL_ERROR),
                "ERROR_PREFIX ({ERROR_PREFIX:?}) must start with TOOL_ERROR ({TOOL_ERROR:?})",
            );
        }
    }

    mod error_cont {
        use super::*;

        #[test]
        fn aligns_with_prefix_width() {
            assert_eq!(ERROR_CONT.width(), ERROR_PREFIX.width());
        }
    }

    mod cursor_marker_width {
        use super::*;

        #[test]
        fn matches_display_width() {
            assert_eq!(CURSOR_MARKER.width(), usize::from(CURSOR_MARKER_WIDTH));
        }
    }

    mod spinner_frames {
        use super::*;

        #[test]
        fn each_render_in_one_column() {
            for &frame in SPINNER_FRAMES {
                assert_eq!(
                    frame.width(),
                    Some(1),
                    "spinner frame {frame:?} must render in one terminal column",
                );
            }
        }
    }
}
