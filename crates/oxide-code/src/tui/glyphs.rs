//! Visual glyphs shared across the TUI.
//!
//! Conventions:
//!
//! - First-line prefix glyphs end with a trailing space (`"❯ "`, `"◉ "`, `"✗ "`).
//! - Continuation indents (`*_CONT`) match the prefix's display width.
//! - Tool status indicators (`✓` / `✗`) carry no trailing space. The renderer handles the gap.

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

// ── Error ──

/// First-line prefix for error blocks.
pub(crate) const ERROR_PREFIX: &str = "✗ ";

/// Prefix for an error shown inside a modal, such as a failed delete or session load.
pub(crate) const MODAL_ERROR_PREFIX: &str = "! ";

// ── Pickers ──

/// Prefix of the highlighted row in list and searchable pickers.
pub(crate) const CURSOR_MARKER: &str = "> ";

/// Display width of [`CURSOR_MARKER`] in terminal columns.
pub(crate) const CURSOR_MARKER_WIDTH: u16 = 2;

/// Marker after the label of a list picker's active row.
pub(crate) const ACTIVE_MARKER: &str = "✓";

/// Dot for the chosen effort level.
pub(crate) const RADIO_SELECTED: char = '●';

/// Dot for an effort level on the slider that is not chosen.
pub(crate) const RADIO_UNSELECTED: char = '○';

// ── Rules ──

/// Horizontal line segment, repeated to draw rules and the effort slider track.
pub(crate) const HORIZONTAL_RULE: &str = "─";

// ── Separators ──

/// Separator between status bar segments.
pub(crate) const SEGMENT_SEPARATOR: &str = " │ ";

/// Separator between items on one line of the welcome screen.
pub(crate) const INLINE_SEPARATOR: &str = " · ";

// ── Inline Markers ──

/// Inline newline replacement for collapsed multi-line previews.
pub(crate) const NEWLINE_GLYPH: &str = " ⏎ ";

/// Line marking where a cancelled turn's reply stopped, in the chat and on stderr in `--no-tui`
/// mode.
pub(crate) const INTERRUPTED_MARKER: &str = "(interrupted)";

// ── Status Spinner ──

/// 8-dot Braille spinner, counterclockwise. Cycle by modulo index.
pub(crate) const SPINNER_FRAMES: &[char] = &['⣷', '⣯', '⣟', '⡿', '⢿', '⣻', '⣽', '⣾'];

#[cfg(test)]
mod tests {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    use super::*;

    // ── Width Invariants ──

    #[test]
    fn user_prompt_prefix_width_matches_display_width() {
        assert_eq!(
            USER_PROMPT_PREFIX.width(),
            usize::from(USER_PROMPT_PREFIX_WIDTH),
        );
    }

    #[test]
    fn user_prompt_continuation_aligns_with_prefix_width() {
        assert_eq!(USER_PROMPT_CONT.width(), USER_PROMPT_PREFIX.width());
    }

    #[test]
    fn assistant_continuation_aligns_with_prefix_width() {
        assert_eq!(ASSISTANT_CONT.width(), ASSISTANT_PREFIX.width());
    }

    #[test]
    fn cursor_marker_width_matches_display_width() {
        assert_eq!(CURSOR_MARKER.width(), usize::from(CURSOR_MARKER_WIDTH));
    }

    // ── Cross-Glyph Consistency ──

    #[test]
    fn thinking_prefix_starts_with_bar() {
        assert!(
            THINKING_PREFIX.starts_with(BAR),
            "THINKING_PREFIX ({THINKING_PREFIX:?}) must start with BAR ({BAR:?})",
        );
    }

    #[test]
    fn tool_border_prefix_starts_with_bar() {
        assert!(
            TOOL_BORDER_PREFIX.starts_with(BAR),
            "TOOL_BORDER_PREFIX ({TOOL_BORDER_PREFIX:?}) must start with BAR ({BAR:?})",
        );
    }

    #[test]
    fn tool_border_cont_starts_with_bar() {
        assert!(
            TOOL_BORDER_CONT.starts_with(BAR),
            "TOOL_BORDER_CONT ({TOOL_BORDER_CONT:?}) must start with BAR ({BAR:?})",
        );
    }

    #[test]
    fn tool_error_matches_error_prefix_glyph() {
        assert!(
            ERROR_PREFIX.starts_with(TOOL_ERROR),
            "ERROR_PREFIX ({ERROR_PREFIX:?}) must start with TOOL_ERROR ({TOOL_ERROR:?})",
        );
    }

    // ── Spinner ──

    #[test]
    fn spinner_frames_each_render_in_one_column() {
        for &frame in SPINNER_FRAMES {
            assert_eq!(
                frame.width(),
                Some(1),
                "spinner frame {frame:?} must render in one terminal column",
            );
        }
    }
}
