use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Color palette and style constants for the TUI.
///
/// The default theme uses Catppuccin Mocha colors with a transparent
/// background (respects the user's terminal theme). All components reference
/// the theme via [`Theme::default()`] rather than hardcoding colors.
/// Named color slots are designed for future user-configurable overrides.
#[expect(
    dead_code,
    reason = "all color slots are part of the theme API; not all consumed yet"
)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Theme {
    // Text hierarchy
    /// Primary text.
    pub(crate) fg: Color,
    /// Secondary text, labels, borders.
    pub(crate) fg_muted: Color,
    /// Dimmed metadata, timestamps.
    pub(crate) fg_dim: Color,

    // Surfaces
    /// Elevated surfaces, tool call backgrounds.
    pub(crate) surface: Color,
    /// Code block background.
    pub(crate) code_bg: Color,

    // Semantic accents (UI roles)
    /// User messages, highlights, active borders.
    pub(crate) accent: Color,
    /// Assistant role labels, focused elements.
    pub(crate) secondary: Color,

    // Status indicators (ascending severity)
    /// Informational highlights, cost display.
    pub(crate) info: Color,
    /// Successful tool results, normal status.
    pub(crate) success: Color,
    /// Warnings, caution status.
    pub(crate) warning: Color,
    /// Errors, failed tools, critical status.
    pub(crate) error: Color,
}

impl Default for Theme {
    /// Catppuccin Mocha palette with transparent terminal background.
    fn default() -> Self {
        Self {
            fg: Color::from_u32(0x00cd_d6f4),        // Text
            fg_muted: Color::from_u32(0x006c_7086),  // Overlay0
            fg_dim: Color::from_u32(0x0058_5b70),    // Surface2
            surface: Color::from_u32(0x0031_3244),   // Surface0
            code_bg: Color::from_u32(0x001e_1e2e),   // Base
            accent: Color::from_u32(0x0089_b4fa),    // Blue
            secondary: Color::from_u32(0x00b4_befe), // Lavender
            info: Color::from_u32(0x0089_dceb),      // Sky
            success: Color::from_u32(0x00a6_e3a1),   // Green
            warning: Color::from_u32(0x00f9_e2af),   // Yellow
            error: Color::from_u32(0x00f3_8ba8),     // Red
        }
    }
}

// ── Style Helpers ──

impl Theme {
    // Text styles

    /// Primary text style (no background override).
    pub(crate) fn text(&self) -> Style {
        Style::default().fg(self.fg)
    }

    /// Muted / secondary text.
    pub(crate) fn muted(&self) -> Style {
        Style::default().fg(self.fg_muted)
    }

    /// Dimmed metadata.
    pub(crate) fn dim(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }

    // Semantic accents

    /// Bold accent (user messages, highlights).
    pub(crate) fn accent(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// Secondary accent (assistant labels).
    pub(crate) fn secondary(&self) -> Style {
        Style::default().fg(self.secondary)
    }

    // Status indicators

    /// Info / cost indicator.
    #[expect(
        dead_code,
        reason = "part of the theme API; no component reads this slot yet"
    )]
    pub(crate) fn info(&self) -> Style {
        Style::default().fg(self.info)
    }

    /// Success indicator.
    pub(crate) fn success(&self) -> Style {
        Style::default().fg(self.success)
    }

    /// Warning indicator.
    pub(crate) fn warning(&self) -> Style {
        Style::default().fg(self.warning)
    }

    /// Error indicator.
    pub(crate) fn error(&self) -> Style {
        Style::default().fg(self.error)
    }

    // Composite helpers

    /// Left border for tool call blocks.
    pub(crate) fn tool_border(&self) -> Style {
        Style::default().fg(self.fg_muted)
    }

    /// Tool icon accent (non-bold).
    pub(crate) fn tool_icon(&self) -> Style {
        Style::default().fg(self.accent)
    }

    /// Thinking text (dimmed italic).
    pub(crate) fn thinking(&self) -> Style {
        Style::default()
            .fg(self.fg_dim)
            .add_modifier(Modifier::ITALIC)
    }

    /// Styled pipe separator span (`" │ "`).
    pub(crate) fn separator_span(&self) -> Span<'static> {
        Span::styled(" │ ", self.separator())
    }

    /// Status bar separator style (dimmed pipe).
    pub(crate) fn separator(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }

    /// Border style for focused components.
    pub(crate) fn border_focused(&self) -> Style {
        Style::default().fg(self.accent)
    }

    /// Border style for unfocused components.
    pub(crate) fn border_unfocused(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Default ──

    #[test]
    fn default_theme_has_distinct_colors() {
        let t = Theme::default();
        assert_ne!(t.fg, t.fg_muted);
        assert_ne!(t.fg_muted, t.fg_dim);
        assert_ne!(t.accent, t.secondary);
        assert_ne!(t.success, t.error);
    }

    // ── Style helpers ──

    #[test]
    fn style_helpers_return_expected_foreground() {
        let t = Theme::default();

        assert_eq!(t.text().fg, Some(t.fg));
        assert_eq!(t.muted().fg, Some(t.fg_muted));
        assert_eq!(t.dim().fg, Some(t.fg_dim));
        assert_eq!(t.accent().fg, Some(t.accent));
        assert_eq!(t.secondary().fg, Some(t.secondary));
        assert_eq!(t.success().fg, Some(t.success));
        assert_eq!(t.warning().fg, Some(t.warning));
        assert_eq!(t.error().fg, Some(t.error));
    }

    #[test]
    fn accent_is_bold() {
        let t = Theme::default();
        assert!(t.accent().add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn thinking_is_italic() {
        let t = Theme::default();
        assert!(t.thinking().add_modifier.contains(Modifier::ITALIC));
    }

    // ── Composite helpers ──

    #[test]
    fn tool_border_uses_muted_foreground() {
        let t = Theme::default();
        assert_eq!(t.tool_border().fg, Some(t.fg_muted));
    }

    #[test]
    fn tool_icon_uses_accent_foreground() {
        let t = Theme::default();
        assert_eq!(t.tool_icon().fg, Some(t.accent));
    }

    #[test]
    fn separator_span_contains_pipe() {
        let t = Theme::default();
        assert!(t.separator_span().content.contains('│'));
    }

    #[test]
    fn border_focused_uses_accent() {
        let t = Theme::default();
        assert_eq!(t.border_focused().fg, Some(t.accent));
    }

    #[test]
    fn border_unfocused_uses_dim() {
        let t = Theme::default();
        assert_eq!(t.border_unfocused().fg, Some(t.fg_dim));
    }
}
