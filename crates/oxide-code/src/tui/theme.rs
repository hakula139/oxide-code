use ratatui::style::{Color, Modifier, Style};

/// Color palette and style constants for the TUI.
///
/// The default theme uses Catppuccin Mocha colors with a transparent
/// background (respects the user's terminal theme). All components reference
/// the theme via [`Theme::default()`] rather than hardcoding colors.
///
/// Users can override individual color slots in the `[tui.theme]` config
/// section. The built-in palette is named `"default"`.
#[expect(
    dead_code,
    reason = "all color slots are part of the theme API; not all consumed yet"
)]
#[derive(Debug, Clone)]
pub struct Theme {
    /// Primary text.
    pub fg: Color,
    /// Secondary text, labels, borders.
    pub fg_muted: Color,
    /// Dimmed metadata, timestamps.
    pub fg_dim: Color,
    /// User messages, highlights, active borders.
    pub accent: Color,
    /// Assistant role labels, focused elements.
    pub secondary: Color,
    /// Successful tool results, normal status.
    pub success: Color,
    /// Warnings, caution status.
    pub warning: Color,
    /// Errors, failed tools, critical status.
    pub error: Color,
    /// Informational highlights, cost display.
    pub info: Color,
    /// Code block background.
    pub code_bg: Color,
    /// Elevated surfaces, tool call backgrounds.
    pub surface: Color,
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
    pub fn text(&self) -> Style {
        Style::default().fg(self.fg)
    }

    /// Muted / secondary text.
    #[expect(
        dead_code,
        reason = "part of the theme API; no component reads this slot yet"
    )]
    pub fn muted(&self) -> Style {
        Style::default().fg(self.fg_muted)
    }

    /// Dimmed metadata.
    pub fn dim(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }

    /// Bold accent (user messages, highlights).
    pub fn accent(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// Secondary accent (assistant labels).
    pub fn secondary(&self) -> Style {
        Style::default().fg(self.secondary)
    }

    /// Success indicator.
    pub fn success(&self) -> Style {
        Style::default().fg(self.success)
    }

    /// Warning indicator.
    pub fn warning(&self) -> Style {
        Style::default().fg(self.warning)
    }

    /// Error indicator.
    #[expect(
        dead_code,
        reason = "part of the theme API; no component reads this slot yet"
    )]
    pub fn error(&self) -> Style {
        Style::default().fg(self.error)
    }

    /// Info / cost indicator.
    #[expect(
        dead_code,
        reason = "part of the theme API; no component reads this slot yet"
    )]
    pub fn info(&self) -> Style {
        Style::default().fg(self.info)
    }

    /// Status bar separator style (dimmed pipe).
    pub fn separator(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }

    /// Border style for focused components.
    pub fn border_focused(&self) -> Style {
        Style::default().fg(self.accent)
    }

    /// Border style for unfocused components.
    pub fn border_unfocused(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }
}
