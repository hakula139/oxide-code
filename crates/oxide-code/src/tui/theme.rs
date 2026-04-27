use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

mod color;

// Hold a reference to the color parser so dead-code analysis treats
// it as live before Step 4 wires it into the loader. Removing this
// shim is part of the Step 4 commit.
const _: fn(&str) -> anyhow::Result<Color> = color::parse_color;

/// A single theme slot — composes optional foreground, optional
/// background, and modifiers into a ratatui [`Style`].
///
/// Most slots are `fg`-only; a few (`diff_add_bg`, `code_bg`) are
/// `bg`-only and leave `fg` unset. Modifiers default to empty unless
/// the role's purpose is to add style (e.g., `accent` is bold,
/// `thinking` is italic, `link` is underlined).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) fg: Option<Color>,
    pub(crate) bg: Option<Color>,
    pub(crate) modifiers: Modifier,
}

impl Slot {
    /// Foreground-only slot with no modifiers.
    const fn fg_only(color: Color) -> Self {
        Self {
            fg: Some(color),
            bg: None,
            modifiers: Modifier::empty(),
        }
    }

    /// Background-only slot with no modifiers.
    const fn bg_only(color: Color) -> Self {
        Self {
            fg: None,
            bg: Some(color),
            modifiers: Modifier::empty(),
        }
    }

    /// Foreground slot with modifiers (bold, italic, underlined, ...).
    const fn styled(color: Color, modifiers: Modifier) -> Self {
        Self {
            fg: Some(color),
            bg: None,
            modifiers,
        }
    }

    /// Compose this slot's fields into a ratatui [`Style`]. Unset
    /// `fg` / `bg` leave the terminal's default in place.
    pub(crate) fn style(&self) -> Style {
        let mut style = Style::default().add_modifier(self.modifiers);
        if let Some(fg) = self.fg {
            style = style.fg(fg);
        }
        if let Some(bg) = self.bg {
            style = style.bg(bg);
        }
        style
    }
}

// ── Theme ──

/// Theme palette and style hooks for the TUI.
///
/// Each slot maps to one role — `error` is "the color used for
/// errors", not "the color red". Cohesion clusters that used to be
/// hard-aliased in code (`border_unfocused = dim`, `table_border =
/// dim`, `horizontal_rule = dim`, `table_header = heading_h2`) are
/// now first-class slots with default values that keep them aligned;
/// users can break the alignment by overriding one without the
/// others.
///
/// The default constructor returns Catppuccin Mocha with a transparent
/// background (respects the user's terminal theme). Step 3 of the
/// themable-config track will swap this hardcoded default for parsing
/// the embedded `themes/mocha.toml`.
#[expect(
    dead_code,
    reason = "all slots are part of the theme API; some are unused by current components"
)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Theme {
    // Text hierarchy
    /// Primary text
    pub(crate) text: Slot,
    /// Secondary text, labels, borders
    pub(crate) muted: Slot,
    /// Dimmed metadata, timestamps
    pub(crate) dim: Slot,

    // Surfaces
    /// Elevated surfaces (reserved; no live consumer)
    pub(crate) surface: Slot,

    // Semantic accents
    /// Highlights, active borders (bold by default)
    pub(crate) accent: Slot,
    /// User messages and icon
    pub(crate) user: Slot,
    /// Assistant messages and icon
    pub(crate) secondary: Slot,

    // Code
    /// Code foreground (reserved palette role)
    pub(crate) code: Slot,
    /// Code block background (reserved; bg-only)
    pub(crate) code_bg: Slot,
    /// Inline code spans (`` `code` ``)
    pub(crate) inline_code: Slot,
    /// Fallback for fenced code blocks with unknown languages
    pub(crate) code_block_fallback: Slot,

    // Diff backgrounds
    /// Background fill for added diff rows (Catppuccin Mocha plus-style)
    pub(crate) diff_add_bg: Slot,
    /// Background fill for deleted diff rows (Catppuccin Mocha minus-style)
    pub(crate) diff_del_bg: Slot,

    // Status indicators (ascending severity)
    /// Informational highlights (reserved; no live consumer)
    pub(crate) info: Slot,
    /// Successful tool results, normal status
    pub(crate) success: Slot,
    /// Warnings, caution status
    pub(crate) warning: Slot,
    /// Errors, failed tools, critical status
    pub(crate) error: Slot,

    // Markdown headings
    /// H1 — most prominent heading (bold + underlined)
    pub(crate) heading_h1: Slot,
    /// H2 — bold section header
    pub(crate) heading_h2: Slot,
    /// H3 — bold italic
    pub(crate) heading_h3: Slot,
    /// H4–H6 — italic (demoted minor headings)
    pub(crate) heading_minor: Slot,

    // Markdown elements
    /// Thinking text (dimmed italic)
    pub(crate) thinking: Slot,
    /// Markdown links — underlined
    pub(crate) link: Slot,
    /// Markdown blockquote marker (`> `)
    pub(crate) blockquote: Slot,
    /// List item bullet / number marker
    pub(crate) list_marker: Slot,

    // Markdown chrome (default-aligned with `dim` / `heading_h2`)
    /// Markdown horizontal rule (`---`)
    pub(crate) horizontal_rule: Slot,
    /// Markdown table header cell
    pub(crate) table_header: Slot,
    /// Markdown table border glyphs
    pub(crate) table_border: Slot,

    // UI chrome
    /// Left border for tool call blocks
    pub(crate) tool_border: Slot,
    /// Tool icon accent (non-bold by default)
    pub(crate) tool_icon: Slot,
    /// Focused component border
    pub(crate) border_focused: Slot,
    /// Unfocused component border (default-aligned with `dim`)
    pub(crate) border_unfocused: Slot,
    /// Status bar separator (dimmed pipe)
    pub(crate) separator: Slot,
}

impl Default for Theme {
    /// Catppuccin Mocha palette with transparent terminal background.
    ///
    /// Step 3 of the themable-config track will replace this with
    /// `parse(include_str!("../../themes/mocha.toml"))`, eliminating
    /// the dual-source-of-truth between this file and the vendored
    /// TOML.
    fn default() -> Self {
        // Catppuccin Mocha palette references
        const TEXT: Color = Color::from_u32(0x00cd_d6f4);
        const OVERLAY0: Color = Color::from_u32(0x006c_7086);
        const SURFACE2: Color = Color::from_u32(0x0058_5b70);
        const SURFACE0: Color = Color::from_u32(0x0031_3244);
        const BLUE: Color = Color::from_u32(0x0089_b4fa);
        const PEACH: Color = Color::from_u32(0x00fa_b387);
        const LAVENDER: Color = Color::from_u32(0x00b4_befe);
        const TEAL: Color = Color::from_u32(0x0094_e2d5);
        const BASE: Color = Color::from_u32(0x001e_1e2e);
        const SKY: Color = Color::from_u32(0x0089_dceb);
        const GREEN: Color = Color::from_u32(0x00a6_e3a1);
        const YELLOW: Color = Color::from_u32(0x00f9_e2af);
        const RED: Color = Color::from_u32(0x00f3_8ba8);
        const DIFF_ADD: Color = Color::from_u32(0x002a_3a37); // catppuccin/delta plus-style
        const DIFF_DEL: Color = Color::from_u32(0x0038_2c34); // catppuccin/delta minus-style

        Self {
            text: Slot::fg_only(TEXT),
            muted: Slot::fg_only(OVERLAY0),
            dim: Slot::fg_only(SURFACE2),

            surface: Slot::fg_only(SURFACE0),

            accent: Slot::styled(BLUE, Modifier::BOLD),
            user: Slot::fg_only(PEACH),
            secondary: Slot::fg_only(LAVENDER),

            code: Slot::fg_only(TEAL),
            code_bg: Slot::bg_only(BASE),
            inline_code: Slot::fg_only(PEACH),
            code_block_fallback: Slot::fg_only(TEAL),

            diff_add_bg: Slot::bg_only(DIFF_ADD),
            diff_del_bg: Slot::bg_only(DIFF_DEL),

            info: Slot::fg_only(SKY),
            success: Slot::fg_only(GREEN),
            warning: Slot::fg_only(YELLOW),
            error: Slot::fg_only(RED),

            heading_h1: Slot::styled(TEXT, Modifier::BOLD.union(Modifier::UNDERLINED)),
            heading_h2: Slot::styled(TEXT, Modifier::BOLD),
            heading_h3: Slot::styled(TEXT, Modifier::BOLD.union(Modifier::ITALIC)),
            heading_minor: Slot::styled(TEXT, Modifier::ITALIC),

            thinking: Slot::styled(SURFACE2, Modifier::ITALIC),
            link: Slot::styled(BLUE, Modifier::UNDERLINED),
            blockquote: Slot::fg_only(GREEN),
            list_marker: Slot::fg_only(BLUE),

            horizontal_rule: Slot::fg_only(SURFACE2),
            table_header: Slot::styled(TEXT, Modifier::BOLD),
            table_border: Slot::fg_only(SURFACE2),

            tool_border: Slot::fg_only(OVERLAY0),
            tool_icon: Slot::fg_only(BLUE),
            border_focused: Slot::fg_only(BLUE),
            border_unfocused: Slot::fg_only(SURFACE2),
            separator: Slot::fg_only(SURFACE2),
        }
    }
}

// ── Style Helpers ──

impl Theme {
    // Text styles

    /// Primary text style (no background override)
    pub(crate) fn text(&self) -> Style {
        self.text.style()
    }

    /// Muted / secondary text
    pub(crate) fn muted(&self) -> Style {
        self.muted.style()
    }

    /// Dimmed metadata
    pub(crate) fn dim(&self) -> Style {
        self.dim.style()
    }

    // Semantic accents

    /// Bold accent (highlights, active borders)
    pub(crate) fn accent(&self) -> Style {
        self.accent.style()
    }

    /// User message bar and icon
    pub(crate) fn user(&self) -> Style {
        self.user.style()
    }

    /// Assistant message bar and icon
    pub(crate) fn secondary(&self) -> Style {
        self.secondary.style()
    }

    // Status indicators

    /// Info / cost indicator
    #[expect(
        dead_code,
        reason = "part of the theme API; no component reads this slot"
    )]
    pub(crate) fn info(&self) -> Style {
        self.info.style()
    }

    /// Success indicator
    pub(crate) fn success(&self) -> Style {
        self.success.style()
    }

    /// Warning indicator
    pub(crate) fn warning(&self) -> Style {
        self.warning.style()
    }

    /// Error indicator
    pub(crate) fn error(&self) -> Style {
        self.error.style()
    }

    // Diff row backgrounds

    /// Bg-only style for added diff rows. Patched onto each span of a
    /// `+` row so the green tint extends across the row, including the
    /// trailing pad-to-width filler.
    pub(crate) fn diff_add_row(&self) -> Style {
        Style::default().bg(self.diff_add_bg.bg.unwrap_or(Color::Reset))
    }

    /// Bg-only style for deleted diff rows. Mirror of [`diff_add_row`].
    ///
    /// [`diff_add_row`]: Self::diff_add_row
    pub(crate) fn diff_del_row(&self) -> Style {
        Style::default().bg(self.diff_del_bg.bg.unwrap_or(Color::Reset))
    }

    // Composite helpers

    /// Left border for tool call blocks
    pub(crate) fn tool_border(&self) -> Style {
        self.tool_border.style()
    }

    /// Tool icon accent (non-bold)
    pub(crate) fn tool_icon(&self) -> Style {
        self.tool_icon.style()
    }

    /// Thinking text (dimmed italic)
    pub(crate) fn thinking(&self) -> Style {
        self.thinking.style()
    }

    /// Styled pipe separator span (`" │ "`)
    pub(crate) fn separator_span(&self) -> Span<'static> {
        Span::styled(" │ ", self.separator())
    }

    /// Status bar separator style (dimmed pipe)
    pub(crate) fn separator(&self) -> Style {
        self.separator.style()
    }

    /// Border style for focused components
    pub(crate) fn border_focused(&self) -> Style {
        self.border_focused.style()
    }

    /// Border style for unfocused components — default-aligned with
    /// [`dim`] but independently overridable.
    ///
    /// [`dim`]: Self::dim
    pub(crate) fn border_unfocused(&self) -> Style {
        self.border_unfocused.style()
    }

    // Markdown rendering

    /// H1 — bold + underlined (most prominent heading)
    pub(crate) fn heading_h1(&self) -> Style {
        self.heading_h1.style()
    }

    /// H2 — bold
    pub(crate) fn heading_h2(&self) -> Style {
        self.heading_h2.style()
    }

    /// H3 — bold italic
    pub(crate) fn heading_h3(&self) -> Style {
        self.heading_h3.style()
    }

    /// H4–H6 — italic (demoted minor headings)
    pub(crate) fn heading_minor(&self) -> Style {
        self.heading_minor.style()
    }

    /// Inline code (`` `code` ``) — peach fg, no fill. A surface bg
    /// reads as a heavy block on transparent terminals.
    pub(crate) fn inline_code(&self) -> Style {
        self.inline_code.style()
    }

    /// Fallback style for fenced code blocks with unknown languages.
    /// Shares the teal foreground with inline code but omits the
    /// background fill, which would paint only the content portion of
    /// each line and leave ragged edges.
    pub(crate) fn code_block_fallback(&self) -> Style {
        self.code_block_fallback.style()
    }

    /// Markdown link URL — accent color with underline
    pub(crate) fn link(&self) -> Style {
        self.link.style()
    }

    /// Blockquote marker (`> `) — uses the palette's success green as a
    /// distinctive accent; not a semantic "success" signal.
    pub(crate) fn blockquote(&self) -> Style {
        self.blockquote.style()
    }

    /// List item bullet / number marker — accent color
    pub(crate) fn list_marker(&self) -> Style {
        self.list_marker.style()
    }

    /// Markdown horizontal rule — default-aligned with [`dim`] but
    /// independently overridable.
    ///
    /// [`dim`]: Self::dim
    pub(crate) fn horizontal_rule(&self) -> Style {
        self.horizontal_rule.style()
    }

    /// Table header cell — default-aligned with [`heading_h2`] but
    /// independently overridable.
    ///
    /// [`heading_h2`]: Self::heading_h2
    pub(crate) fn table_header(&self) -> Style {
        self.table_header.style()
    }

    /// Table border glyphs — default-aligned with [`dim`] but
    /// independently overridable.
    ///
    /// [`dim`]: Self::dim
    pub(crate) fn table_border(&self) -> Style {
        self.table_border.style()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Slot ──

    #[test]
    fn slot_fg_only_sets_only_foreground() {
        let s = Slot::fg_only(Color::Red);
        let style = s.style();
        assert_eq!(style.fg, Some(Color::Red));
        assert_eq!(style.bg, None);
        assert!(style.add_modifier.is_empty());
    }

    #[test]
    fn slot_bg_only_sets_only_background() {
        let s = Slot::bg_only(Color::Blue);
        let style = s.style();
        assert_eq!(style.fg, None);
        assert_eq!(style.bg, Some(Color::Blue));
        assert!(style.add_modifier.is_empty());
    }

    #[test]
    fn slot_styled_carries_modifiers() {
        let s = Slot::styled(Color::Green, Modifier::BOLD.union(Modifier::ITALIC));
        let style = s.style();
        assert_eq!(style.fg, Some(Color::Green));
        assert_eq!(style.bg, None);
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::ITALIC));
    }

    // ── Default ──

    #[test]
    fn default_theme_has_distinct_colors() {
        let t = Theme::default();
        assert_ne!(t.text.fg, t.muted.fg);
        assert_ne!(t.muted.fg, t.dim.fg);
        assert_ne!(t.accent.fg, t.secondary.fg);
        assert_ne!(t.user.fg, t.secondary.fg);
        assert_ne!(t.success.fg, t.error.fg);
        assert_ne!(t.diff_add_bg.bg, t.diff_del_bg.bg);
    }

    // ── Style helpers ──

    #[test]
    fn style_helpers_return_expected_foreground() {
        let t = Theme::default();
        assert_eq!(t.text().fg, t.text.fg);
        assert_eq!(t.muted().fg, t.muted.fg);
        assert_eq!(t.dim().fg, t.dim.fg);
        assert_eq!(t.accent().fg, t.accent.fg);
        assert_eq!(t.user().fg, t.user.fg);
        assert_eq!(t.secondary().fg, t.secondary.fg);
        assert_eq!(t.success().fg, t.success.fg);
        assert_eq!(t.warning().fg, t.warning.fg);
        assert_eq!(t.error().fg, t.error.fg);
        assert_eq!(t.inline_code().fg, t.inline_code.fg);
        assert_eq!(t.inline_code().bg, None);
        assert_eq!(t.code_block_fallback().fg, t.code_block_fallback.fg);
        assert_eq!(t.code_block_fallback().bg, None);
    }

    #[test]
    fn diff_row_helpers_set_only_background() {
        // Bg-only is load-bearing: helpers are patched onto each span
        // of a diff row, so setting fg here would override the
        // success / error / muted fg the row composes from.
        let t = Theme::default();

        let add = t.diff_add_row();
        assert_eq!(add.bg, t.diff_add_bg.bg);
        assert_eq!(add.fg, None);

        let del = t.diff_del_row();
        assert_eq!(del.bg, t.diff_del_bg.bg);
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
        assert_eq!(t.tool_border().fg, t.muted.fg);
    }

    #[test]
    fn tool_icon_uses_accent_foreground() {
        let t = Theme::default();
        assert_eq!(t.tool_icon().fg, t.accent.fg);
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
        assert_eq!(t.border_focused().fg, t.accent.fg);
    }

    #[test]
    fn border_unfocused_uses_dim() {
        let t = Theme::default();
        assert_eq!(t.border_unfocused().fg, t.dim.fg);
    }

    // ── Markdown rendering ──

    #[test]
    fn heading_styles_use_fg_with_expected_modifiers() {
        let t = Theme::default();

        let h1 = t.heading_h1();
        assert_eq!(h1.fg, t.text.fg);
        assert!(h1.add_modifier.contains(Modifier::BOLD));
        assert!(h1.add_modifier.contains(Modifier::UNDERLINED));

        let h2 = t.heading_h2();
        assert_eq!(h2.fg, t.text.fg);
        assert!(h2.add_modifier.contains(Modifier::BOLD));
        assert!(!h2.add_modifier.contains(Modifier::UNDERLINED));

        let h3 = t.heading_h3();
        assert_eq!(h3.fg, t.text.fg);
        assert!(h3.add_modifier.contains(Modifier::BOLD));
        assert!(h3.add_modifier.contains(Modifier::ITALIC));

        let h4 = t.heading_minor();
        assert_eq!(h4.fg, t.text.fg);
        assert!(h4.add_modifier.contains(Modifier::ITALIC));
        assert!(!h4.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn link_uses_accent_with_underline() {
        let t = Theme::default();
        let link = t.link();
        assert_eq!(link.fg, t.accent.fg);
        assert!(link.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn blockquote_uses_success_color() {
        let t = Theme::default();
        assert_eq!(t.blockquote().fg, t.success.fg);
    }

    #[test]
    fn list_marker_uses_accent_color() {
        let t = Theme::default();
        assert_eq!(t.list_marker().fg, t.accent.fg);
    }

    #[test]
    fn horizontal_rule_uses_dim_color() {
        let t = Theme::default();
        assert_eq!(t.horizontal_rule().fg, t.dim.fg);
    }

    #[test]
    fn table_header_is_bold_fg() {
        let t = Theme::default();
        let style = t.table_header();
        assert_eq!(style.fg, t.text.fg);
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn table_border_uses_dim_color() {
        let t = Theme::default();
        assert_eq!(t.table_border().fg, t.dim.fg);
    }

    // ── Default theme cohesion ──

    /// `border_unfocused`, `horizontal_rule`, `table_border` are
    /// independent slots but ship aligned with `dim` in the default
    /// theme. If a future palette change diverges any from `dim` by
    /// accident, this test surfaces it.
    #[test]
    fn default_dim_cluster_matches_dim() {
        let t = Theme::default();
        let dim = t.dim();
        assert_eq!(t.border_unfocused(), dim);
        assert_eq!(t.horizontal_rule(), dim);
        assert_eq!(t.table_border(), dim);
    }

    /// Table headers and H2 ship visually identical in the default
    /// theme. Independent slots, aligned defaults.
    #[test]
    fn default_table_header_matches_heading_h2() {
        let t = Theme::default();
        assert_eq!(t.table_header(), t.heading_h2());
    }
}
