use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

mod builtin;
mod color;
mod loader;

pub(crate) use loader::{SlotPatch, resolve_theme};

/// A single theme slot — composes optional foreground, optional
/// background, and modifiers into a ratatui [`Style`].
///
/// Most slots are `fg`-only; a few (`diff_add`, `code_bg`) are
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

/// Canonical slot list. `Theme`, `ThemeFile`, `into_theme`, and
/// `slot_for_name` are all generated from this — adding or renaming
/// a slot is a one-place edit.
macro_rules! for_each_slot {
    ($callback:ident) => {
        $callback! {
            // Text hierarchy
            (text, "Primary text"),
            (muted, "Secondary text, labels, borders"),
            (dim, "Dimmed metadata, timestamps"),

            // Surfaces
            (surface, "Chat / input / status panel background (bg-only)"),

            // Semantic accents
            (accent, "Highlights, active borders (bold by default)"),
            (user, "User messages and icon"),
            (secondary, "Assistant messages and icon"),

            // Code
            (code, "Fenced code blocks with no recognized language"),
            (inline_code, "Inline code spans (`` `code` ``)"),

            // Diff backgrounds
            (diff_add, "Background fill for added diff rows (Catppuccin Mocha plus-style)"),
            (diff_del, "Background fill for deleted diff rows (Catppuccin Mocha minus-style)"),

            // Status indicators (ascending severity)
            (info, "Informational highlight (in-progress / neutral signals)"),
            (success, "Successful tool results, ready status"),
            (warning, "Warnings, caution status"),
            (error, "Errors, failed tools, critical status"),

            // Markdown headings
            (heading_h1, "H1 — most prominent heading (bold + underlined)"),
            (heading_h2, "H2 — bold section header"),
            (heading_h3, "H3 — bold italic"),
            (heading_minor, "H4–H6 — italic (demoted minor headings)"),

            // Markdown body
            (thinking, "Thinking text (dimmed italic)"),
            (link, "Markdown links — underlined"),
            (blockquote, "Markdown blockquote marker (`> `)"),
            (list_marker, "List item bullet / number marker"),

            // Markdown chrome (default-aligned with `dim` / `heading_h2`)
            (horizontal_rule, "Markdown horizontal rule (`---`)"),
            (table_header, "Markdown table header cell"),
            (table_border, "Markdown table border glyphs"),

            // UI chrome
            (tool_border, "Left border for tool call blocks"),
            (tool_icon, "Tool icon accent (non-bold by default)"),
            (border_focused, "Focused component border"),
            (border_unfocused, "Unfocused component border (default-aligned with `dim`)"),
            (separator, "Status bar separator (dimmed pipe)"),
        }
    };
}
pub(crate) use for_each_slot;

/// Theme palette. Each slot is one role — `error` is "errors", not
/// "red". `Default::default()` parses the vendored `themes/mocha.toml`
/// once on first access.
macro_rules! define_theme_struct {
    ( $( ($name:ident, $doc:literal), )* ) => {
        #[derive(Debug, Clone)]
        pub(crate) struct Theme {
            $(
                #[doc = $doc]
                pub(crate) $name: Slot,
            )*
        }
    };
}
for_each_slot!(define_theme_struct);

impl Default for Theme {
    /// Catppuccin Mocha. Parsed once from the embedded TOML; each call
    /// clones the cached [`Theme`].
    fn default() -> Self {
        static MOCHA: LazyLock<Theme> = LazyLock::new(|| {
            loader::parse_theme(builtin::MOCHA).expect("vendored mocha.toml must parse")
        });
        MOCHA.clone()
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

    /// Chat / input / status panel background. Bg-only; default
    /// `Color::Reset` keeps the terminal background showing through,
    /// so users on transparent terminals see no change. Override
    /// `surface = { bg = "..." }` to give the panels an opaque tint.
    pub(crate) fn surface(&self) -> Style {
        self.surface.style()
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

    /// Info indicator (in-progress / neutral signals)
    pub(crate) fn info(&self) -> Style {
        self.info.style()
    }

    /// Success indicator
    pub(crate) fn success(&self) -> Style {
        self.success.style()
    }

    /// Warning indicator — caution / non-fatal issues. No production
    /// consumer yet; kept for API symmetry with the rest of the
    /// status set so users can pre-style.
    #[allow(dead_code, reason = "API surface; consumed in tests")]
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
        Style::default().bg(self.diff_add.bg.unwrap_or(Color::Reset))
    }

    /// Bg-only style for deleted diff rows. Mirror of [`diff_add_row`].
    ///
    /// [`diff_add_row`]: Self::diff_add_row
    pub(crate) fn diff_del_row(&self) -> Style {
        Style::default().bg(self.diff_del.bg.unwrap_or(Color::Reset))
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

    /// Fenced code blocks with no recognized language.
    pub(crate) fn code(&self) -> Style {
        self.code.style()
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
        let s = Slot {
            fg: Some(Color::Red),
            bg: None,
            modifiers: Modifier::empty(),
        };
        let style = s.style();
        assert_eq!(style.fg, Some(Color::Red));
        assert_eq!(style.bg, None);
        assert!(style.add_modifier.is_empty());
    }

    #[test]
    fn slot_bg_only_sets_only_background() {
        let s = Slot {
            fg: None,
            bg: Some(Color::Blue),
            modifiers: Modifier::empty(),
        };
        let style = s.style();
        assert_eq!(style.fg, None);
        assert_eq!(style.bg, Some(Color::Blue));
        assert!(style.add_modifier.is_empty());
    }

    #[test]
    fn slot_styled_carries_modifiers() {
        let s = Slot {
            fg: Some(Color::Green),
            bg: None,
            modifiers: Modifier::BOLD.union(Modifier::ITALIC),
        };
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
        assert_ne!(t.diff_add.bg, t.diff_del.bg);
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
        assert_eq!(t.error().fg, t.error.fg);
        assert_eq!(t.inline_code().fg, t.inline_code.fg);
        assert_eq!(t.inline_code().bg, None);
        assert_eq!(t.code().fg, t.code.fg);
        assert_eq!(t.code().bg, None);
    }

    #[test]
    fn diff_row_helpers_set_only_background() {
        // Bg-only is load-bearing: helpers are patched onto each span
        // of a diff row, so setting fg here would override the
        // success / error / muted fg the row composes from.
        let t = Theme::default();

        let add = t.diff_add_row();
        assert_eq!(add.bg, t.diff_add.bg);
        assert_eq!(add.fg, None);

        let del = t.diff_del_row();
        assert_eq!(del.bg, t.diff_del.bg);
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
