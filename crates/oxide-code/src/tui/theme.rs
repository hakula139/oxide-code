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
    /// Primary text.
    pub(crate) fg: Color,
    /// Secondary text, labels, borders.
    pub(crate) fg_muted: Color,
    /// Dimmed metadata, timestamps.
    pub(crate) fg_dim: Color,
    /// User messages, highlights, active borders.
    pub(crate) accent: Color,
    /// Assistant role labels, focused elements.
    pub(crate) secondary: Color,
    /// Successful tool results, normal status.
    pub(crate) success: Color,
    /// Warnings, caution status.
    pub(crate) warning: Color,
    /// Errors, failed tools, critical status.
    pub(crate) error: Color,
    /// Informational highlights, cost display.
    pub(crate) info: Color,
    /// Code block background.
    pub(crate) code_bg: Color,
    /// Elevated surfaces, tool call backgrounds.
    pub(crate) surface: Color,
}

impl Default for Theme {
    /// Catppuccin Mocha palette with transparent terminal background.
    fn default() -> Self {
        Self {
            fg: Color::from_u32(0x00cd_d6f4),        // Text
            fg_muted: Color::from_u32(0x006c_7086),  // Overlay0
            fg_dim: Color::from_u32(0x0058_5b70),    // Surface2
            accent: Color::from_u32(0x0089_b4fa),    // Blue
            secondary: Color::from_u32(0x00b4_befe), // Lavender
            success: Color::from_u32(0x00a6_e3a1),   // Green
            warning: Color::from_u32(0x00f9_e2af),   // Yellow
            error: Color::from_u32(0x00f3_8ba8),     // Red
            info: Color::from_u32(0x0089_dceb),      // Sky
            code_bg: Color::from_u32(0x001e_1e2e),   // Base
            surface: Color::from_u32(0x0031_3244),   // Surface0
        }
    }
}

// ── Style Helpers ──

impl Theme {
    /// Primary text style (no background override).
    pub(crate) fn text(&self) -> Style {
        Style::default().fg(self.fg)
    }

    /// Muted / secondary text.
    #[expect(
        dead_code,
        reason = "part of the theme API; no component reads this slot yet"
    )]
    pub(crate) fn muted(&self) -> Style {
        Style::default().fg(self.fg_muted)
    }

    /// Dimmed metadata.
    pub(crate) fn dim(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }

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

    /// Success indicator.
    pub(crate) fn success(&self) -> Style {
        Style::default().fg(self.success)
    }

    /// Warning indicator.
    pub(crate) fn warning(&self) -> Style {
        Style::default().fg(self.warning)
    }

    /// Error indicator.
    #[expect(
        dead_code,
        reason = "part of the theme API; no component reads this slot yet"
    )]
    pub(crate) fn error(&self) -> Style {
        Style::default().fg(self.error)
    }

    /// Info / cost indicator.
    #[expect(
        dead_code,
        reason = "part of the theme API; no component reads this slot yet"
    )]
    pub(crate) fn info(&self) -> Style {
        Style::default().fg(self.info)
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
