//! Visual glyphs used across the TUI — single source of truth for the
//! UI's iconography so the visual identity is easy to scan and easy
//! to keep consistent.
//!
//! Conventions:
//!
//! - First-line prefix glyphs end with a trailing space so callers
//!   can concatenate without padding (`"❯ "`, `"◉ "`, `"✗ "`).
//! - Continuation indents (`*_CONT`) match the prefix's display width
//!   so wrapped lines align under the text, not under the icon.
//! - Status indicators that participate in structured layouts (the
//!   tool `✓` / `✗`) carry no trailing space; the renderer handles
//!   the column gap explicitly.

// ── User prompt ──

/// First-line prefix shown for any user-typed prompt — past messages
/// in the chat history, queued prompts in the preview panel, and the
/// active input area.
pub(crate) const USER_PROMPT_PREFIX: &str = "❯ ";

/// Display width of [`USER_PROMPT_PREFIX`] in terminal columns.
pub(crate) const USER_PROMPT_PREFIX_WIDTH: u16 = 2;

/// Continuation indent for multi-line user-typed content.
pub(crate) const USER_PROMPT_CONT: &str = "  ";

// ── Assistant ──

/// First-line prefix for assistant text — diamond + space.
pub(crate) const ASSISTANT_PREFIX: &str = "◉ ";

/// Continuation indent for multi-line assistant content.
pub(crate) const ASSISTANT_CONT: &str = "  ";

// ── Tool / Thinking ──

/// Left-bar glyph for tool blocks and the thinking block. Single
/// column wide; usually paired with a trailing space when used as a
/// line prefix (see [`THINKING_PREFIX`]).
pub(crate) const BAR: &str = "▎";

/// Per-line prefix for thinking blocks — [`BAR`] + space, so thinking
/// rows align with adjacent tool-block bars.
pub(crate) const THINKING_PREFIX: &str = "▎ ";

/// Tool-result success indicator. No trailing space — tool blocks
/// handle their own column gap via [`BAR`] and the status-line layout.
pub(crate) const TOOL_SUCCESS: &str = "✓";

/// Tool-result failure indicator. Same glyph as [`ERROR_PREFIX`]
/// without the trailing space.
pub(crate) const TOOL_ERROR: &str = "✗";

// ── Error ──

/// First-line prefix for error blocks — cross mark + space.
pub(crate) const ERROR_PREFIX: &str = "✗ ";

// ── Inline markers ──

/// Inline newline replacement for collapsing multi-line previews to
/// a single row without losing the "this is more than it looks" hint.
pub(crate) const NEWLINE_GLYPH: &str = " ⏎ ";

// ── Status spinner ──

/// Braille spinner frames for the streaming / running-tool status.
/// Cycle by index — `SPINNER_FRAMES[(tick / period) % len()]`.
pub(crate) const SPINNER_FRAMES: &[char] =
    &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    use super::*;

    // ── Width invariants ──

    #[test]
    fn user_prompt_prefix_width_matches_string_width() {
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

    // ── Cross-glyph consistency ──

    #[test]
    fn thinking_prefix_starts_with_bar() {
        assert!(
            THINKING_PREFIX.starts_with(BAR),
            "THINKING_PREFIX ({THINKING_PREFIX:?}) must start with BAR ({BAR:?})",
        );
    }

    #[test]
    fn tool_error_matches_error_prefix_glyph() {
        assert!(
            ERROR_PREFIX.starts_with(TOOL_ERROR),
            "ERROR_PREFIX ({ERROR_PREFIX:?}) must start with TOOL_ERROR ({TOOL_ERROR:?})",
        );
    }
}
