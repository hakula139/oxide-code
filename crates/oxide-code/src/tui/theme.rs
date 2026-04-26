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
    reason = "all color slots are part of the theme API; some are unused by current components"
)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Theme {
    // Text hierarchy
    /// Primary text
    pub(crate) fg: Color,
    /// Secondary text, labels, borders
    pub(crate) fg_muted: Color,
    /// Dimmed metadata, timestamps
    pub(crate) fg_dim: Color,

    // Surfaces
    /// Elevated surfaces (reserved; no live consumer)
    pub(crate) surface: Color,

    // Semantic accents (UI roles)
    /// Highlights, active borders, links, list markers
    pub(crate) accent: Color,
    /// User messages and icon
    pub(crate) user: Color,
    /// Assistant messages and icon
    pub(crate) secondary: Color,

    // Code
    /// Inline code, code block fallback
    pub(crate) code: Color,
    /// Code block background (reserved for future theming; currently unused)
    pub(crate) code_bg: Color,

    // Diff
    /// Background fill for added diff rows (Catppuccin Mocha plus-style)
    pub(crate) diff_add_bg: Color,
    /// Background fill for deleted diff rows (Catppuccin Mocha minus-style)
    pub(crate) diff_del_bg: Color,

    // Status indicators (ascending severity)
    /// Informational highlights (reserved for future theming; currently unused)
    pub(crate) info: Color,
    /// Successful tool results, normal status
    pub(crate) success: Color,
    /// Warnings, caution status
    pub(crate) warning: Color,
    /// Errors, failed tools, critical status
    pub(crate) error: Color,
}

impl Default for Theme {
    /// Catppuccin Mocha palette with transparent terminal background.
    fn default() -> Self {
        Self {
            fg: Color::from_u32(0x00cd_d6f4),          // Text
            fg_muted: Color::from_u32(0x006c_7086),    // Overlay0
            fg_dim: Color::from_u32(0x0058_5b70),      // Surface2
            surface: Color::from_u32(0x0031_3244),     // Surface0
            accent: Color::from_u32(0x0089_b4fa),      // Blue
            user: Color::from_u32(0x00fa_b387),        // Peach
            secondary: Color::from_u32(0x00b4_befe),   // Lavender
            code: Color::from_u32(0x0094_e2d5),        // Teal
            code_bg: Color::from_u32(0x001e_1e2e),     // Base
            diff_add_bg: Color::from_u32(0x002a_3a37), // catppuccin/delta plus-style
            diff_del_bg: Color::from_u32(0x0038_2c34), // catppuccin/delta minus-style
            info: Color::from_u32(0x0089_dceb),        // Sky
            success: Color::from_u32(0x00a6_e3a1),     // Green
            warning: Color::from_u32(0x00f9_e2af),     // Yellow
            error: Color::from_u32(0x00f3_8ba8),       // Red
        }
    }
}

// ── Style Helpers ──

impl Theme {
    // Text styles

    /// Primary text style (no background override)
    pub(crate) fn text(&self) -> Style {
        Style::default().fg(self.fg)
    }

    /// Muted / secondary text
    pub(crate) fn muted(&self) -> Style {
        Style::default().fg(self.fg_muted)
    }

    /// Dimmed metadata
    pub(crate) fn dim(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }

    // Semantic accents

    /// Bold accent (highlights, active borders)
    pub(crate) fn accent(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// User message bar and icon
    pub(crate) fn user(&self) -> Style {
        Style::default().fg(self.user)
    }

    /// Assistant message bar and icon
    pub(crate) fn secondary(&self) -> Style {
        Style::default().fg(self.secondary)
    }

    // Status indicators

    /// Info / cost indicator
    #[expect(
        dead_code,
        reason = "part of the theme API; no component reads this slot"
    )]
    pub(crate) fn info(&self) -> Style {
        Style::default().fg(self.info)
    }

    /// Success indicator
    pub(crate) fn success(&self) -> Style {
        Style::default().fg(self.success)
    }

    /// Warning indicator
    pub(crate) fn warning(&self) -> Style {
        Style::default().fg(self.warning)
    }

    /// Error indicator
    pub(crate) fn error(&self) -> Style {
        Style::default().fg(self.error)
    }

    // Diff row backgrounds

    /// Bg-only style for added diff rows. Patched onto each span of a
    /// `+` row so the green tint extends across the row, including the
    /// trailing pad-to-width filler.
    pub(crate) fn diff_add_row(&self) -> Style {
        Style::default().bg(self.diff_add_bg)
    }

    /// Bg-only style for deleted diff rows. Mirror of [`diff_add_row`].
    ///
    /// [`diff_add_row`]: Self::diff_add_row
    pub(crate) fn diff_del_row(&self) -> Style {
        Style::default().bg(self.diff_del_bg)
    }

    // Composite helpers

    /// Left border for tool call blocks
    pub(crate) fn tool_border(&self) -> Style {
        Style::default().fg(self.fg_muted)
    }

    /// Tool icon accent (non-bold)
    pub(crate) fn tool_icon(&self) -> Style {
        Style::default().fg(self.accent)
    }

    /// Thinking text (dimmed italic)
    pub(crate) fn thinking(&self) -> Style {
        Style::default()
            .fg(self.fg_dim)
            .add_modifier(Modifier::ITALIC)
    }

    /// Styled pipe separator span (`" │ "`)
    pub(crate) fn separator_span(&self) -> Span<'static> {
        Span::styled(" │ ", self.separator())
    }

    /// Status bar separator style (dimmed pipe)
    pub(crate) fn separator(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }

    /// Border style for focused components
    pub(crate) fn border_focused(&self) -> Style {
        Style::default().fg(self.accent)
    }

    /// Border style for unfocused components — aliased to [`dim`] since
    /// both are "low-contrast UI chrome"; palette shifts apply in lockstep.
    ///
    /// [`dim`]: Self::dim
    pub(crate) fn border_unfocused(&self) -> Style {
        self.dim()
    }

    // Markdown rendering

    /// H1 — bold + underlined (most prominent heading)
    pub(crate) fn heading_h1(&self) -> Style {
        Style::default()
            .fg(self.fg)
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::UNDERLINED)
    }

    /// H2 — bold. Also the canonical style for bold section headers
    /// (reused by [`table_header`](Self::table_header)).
    pub(crate) fn heading_h2(&self) -> Style {
        Style::default().fg(self.fg).add_modifier(Modifier::BOLD)
    }

    /// H3 — bold italic
    pub(crate) fn heading_h3(&self) -> Style {
        Style::default()
            .fg(self.fg)
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::ITALIC)
    }

    /// H4–H6 — italic (demoted minor headings)
    pub(crate) fn heading_minor(&self) -> Style {
        Style::default().fg(self.fg).add_modifier(Modifier::ITALIC)
    }

    /// Inline code (`` `code` ``) — peach fg, no fill. A surface bg
    /// reads as a heavy block on transparent terminals.
    pub(crate) fn inline_code(&self) -> Style {
        Style::default().fg(self.user)
    }

    /// Fallback style for fenced code blocks with unknown languages.
    /// Shares the teal foreground with inline code but omits the
    /// background fill, which would paint only the content portion of
    /// each line and leave ragged edges.
    pub(crate) fn code_block_fallback(&self) -> Style {
        Style::default().fg(self.code)
    }

    /// Markdown link URL — accent color with underline
    pub(crate) fn link(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::UNDERLINED)
    }

    /// Blockquote marker (`> `) — uses the palette's success green as a
    /// distinctive accent; not a semantic "success" signal, and not
    /// intentionally aliased to other chrome roles.
    pub(crate) fn blockquote(&self) -> Style {
        Style::default().fg(self.success)
    }

    /// List item bullet / number marker — accent color
    pub(crate) fn list_marker(&self) -> Style {
        Style::default().fg(self.accent)
    }

    /// Markdown horizontal rule — aliased to [`dim`] as low-contrast chrome.
    ///
    /// [`dim`]: Self::dim
    pub(crate) fn horizontal_rule(&self) -> Style {
        self.dim()
    }

    /// Table header cell — aliased to [`heading_h2`] since both are
    /// "bold section header on primary foreground".
    ///
    /// [`heading_h2`]: Self::heading_h2
    pub(crate) fn table_header(&self) -> Style {
        self.heading_h2()
    }

    /// Table border glyphs — aliased to [`dim`] as low-contrast chrome.
    ///
    /// [`dim`]: Self::dim
    pub(crate) fn table_border(&self) -> Style {
        self.dim()
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
        assert_ne!(t.user, t.secondary);
        assert_ne!(t.success, t.error);
        assert_ne!(t.diff_add_bg, t.diff_del_bg);
    }

    // ── Style helpers ──

    #[test]
    fn style_helpers_return_expected_foreground() {
        let t = Theme::default();

        assert_eq!(t.text().fg, Some(t.fg));
        assert_eq!(t.muted().fg, Some(t.fg_muted));
        assert_eq!(t.dim().fg, Some(t.fg_dim));
        assert_eq!(t.accent().fg, Some(t.accent));
        assert_eq!(t.user().fg, Some(t.user));
        assert_eq!(t.secondary().fg, Some(t.secondary));
        assert_eq!(t.success().fg, Some(t.success));
        assert_eq!(t.warning().fg, Some(t.warning));
        assert_eq!(t.error().fg, Some(t.error));
        assert_eq!(t.inline_code().fg, Some(t.user));
        assert_eq!(t.inline_code().bg, None);
        assert_eq!(t.code_block_fallback().fg, Some(t.code));
        assert_eq!(t.code_block_fallback().bg, None);
    }

    #[test]
    fn diff_row_helpers_set_only_background() {
        // Bg-only is load-bearing: the helpers are patched onto each
        // span of a diff row, so setting fg here would override the
        // success / error / muted fg the row composes from.
        let t = Theme::default();

        let add = t.diff_add_row();
        assert_eq!(add.bg, Some(t.diff_add_bg));
        assert_eq!(add.fg, None);

        let del = t.diff_del_row();
        assert_eq!(del.bg, Some(t.diff_del_bg));
        assert_eq!(del.fg, None);
    }

    #[test]
    fn accent_is_bold() {
        let t = Theme::default();
        assert!(t.accent().add_modifier.contains(Modifier::BOLD));
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
    fn thinking_is_italic() {
        let t = Theme::default();
        assert!(t.thinking().add_modifier.contains(Modifier::ITALIC));
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

    // ── Markdown rendering ──

    #[test]
    fn heading_styles_use_fg_with_expected_modifiers() {
        let t = Theme::default();

        let h1 = t.heading_h1();
        assert_eq!(h1.fg, Some(t.fg));
        assert!(h1.add_modifier.contains(Modifier::BOLD));
        assert!(h1.add_modifier.contains(Modifier::UNDERLINED));

        let h2 = t.heading_h2();
        assert_eq!(h2.fg, Some(t.fg));
        assert!(h2.add_modifier.contains(Modifier::BOLD));
        assert!(!h2.add_modifier.contains(Modifier::UNDERLINED));

        let h3 = t.heading_h3();
        assert_eq!(h3.fg, Some(t.fg));
        assert!(h3.add_modifier.contains(Modifier::BOLD));
        assert!(h3.add_modifier.contains(Modifier::ITALIC));

        let h4 = t.heading_minor();
        assert_eq!(h4.fg, Some(t.fg));
        assert!(h4.add_modifier.contains(Modifier::ITALIC));
        assert!(!h4.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn link_uses_accent_with_underline() {
        let t = Theme::default();
        let link = t.link();
        assert_eq!(link.fg, Some(t.accent));
        assert!(link.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn blockquote_uses_success_color() {
        let t = Theme::default();
        assert_eq!(t.blockquote().fg, Some(t.success));
    }

    #[test]
    fn list_marker_uses_accent_color() {
        let t = Theme::default();
        assert_eq!(t.list_marker().fg, Some(t.accent));
    }

    #[test]
    fn horizontal_rule_uses_dim_color() {
        let t = Theme::default();
        assert_eq!(t.horizontal_rule().fg, Some(t.fg_dim));
    }

    #[test]
    fn table_header_is_bold_fg() {
        let t = Theme::default();
        let style = t.table_header();
        assert_eq!(style.fg, Some(t.fg));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn table_border_uses_dim_color() {
        let t = Theme::default();
        assert_eq!(t.table_border().fg, Some(t.fg_dim));
    }

    // ── Semantic aliases ──

    /// `border_unfocused`, `horizontal_rule`, `table_border` all share
    /// the "low-contrast UI chrome" role and must evolve together. If
    /// any diverges from `dim()`, pull it out of the aliased cluster
    /// intentionally rather than by accident.
    #[test]
    fn dim_aliases_stay_in_lockstep() {
        let t = Theme::default();
        let dim = t.dim();
        assert_eq!(t.border_unfocused(), dim);
        assert_eq!(t.horizontal_rule(), dim);
        assert_eq!(t.table_border(), dim);
    }

    /// Table headers and H2 both represent "bold section header on
    /// primary fg"; they must stay visually identical.
    #[test]
    fn table_header_matches_heading_h2() {
        let t = Theme::default();
        assert_eq!(t.table_header(), t.heading_h2());
    }
}
