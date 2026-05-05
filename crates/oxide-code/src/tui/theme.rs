//! Theme palette and style helpers.

use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

mod builtin;
mod color;
mod loader;

pub(crate) use loader::{SlotPatch, resolve_theme};

/// Resolve a built-in theme name (mocha / latte / ...) to a parsed [`Theme`]. `None` for unknown
/// names; never reads the filesystem. Used by `/theme` for live preview where each cursor move
/// must be cheap.
pub(crate) fn load_builtin(name: &str) -> Option<Theme> {
    let body = builtin::lookup(name)?;
    loader::parse_theme(body).ok()
}

/// One theme slot — optional fg, bg, and modifiers composed into a [`Style`].
///
/// Each component is independently optional so an override can patch one axis (e.g., bg only)
/// without forcing callers to also restate the other two. [`Slot::style`] folds the three into
/// a ratatui `Style`, leaving unset axes as the terminal's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(crate) fg: Option<Color>,
    pub(crate) bg: Option<Color>,
    pub(crate) modifiers: Modifier,
}

impl Slot {
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

/// Canonical slot list. Adding or renaming a slot is a one-place edit.
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
            (queued, "Queued user prompts in the preview panel"),
            (assistant, "Assistant messages and icon"),

            // Status indicators (ascending severity)
            (info, "Informational highlight (in-progress / neutral signals)"),
            (success, "Successful tool results, ready status"),
            (warning, "Warnings, caution status"),
            (error, "Errors, failed tools, critical status"),

            // Code
            (code, "Fenced code blocks with no recognized language"),
            (inline_code, "Inline code spans (`` `code` ``)"),

            // Diff backgrounds
            (diff_add, "Background fill for added diff rows (Catppuccin Mocha plus-style)"),
            (diff_del, "Background fill for deleted diff rows (Catppuccin Mocha minus-style)"),

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
            (tool_icon, "Tool icon accent"),
            (border_focused, "Focused component border"),
            (border_unfocused, "Unfocused component border (default-aligned with `dim`)"),
            (separator, "Status bar separator (dimmed pipe)"),
        }
    };
}

pub(super) use for_each_slot;

/// Theme palette. Each slot is a semantic role, not a raw color.
macro_rules! define_theme_struct {
    ( $( ($name:ident, $doc:literal), )* ) => {
        #[derive(Debug, Clone)]
        pub(crate) struct Theme {
            $(
                #[doc = $doc]
                pub(crate) $name: Slot,
            )*
        }

        #[cfg(test)]
        pub(crate) const SLOT_NAMES: &[&str] = &[ $(stringify!($name),)* ];
    };
}

for_each_slot!(define_theme_struct);

impl Default for Theme {
    fn default() -> Self {
        // Cache the parsed Mocha palette in a `LazyLock` so the TOML parse runs at most once per
        // process; clones are cheap (every `Slot` is `Copy`).
        static MOCHA: LazyLock<Theme> = LazyLock::new(|| {
            loader::parse_theme(builtin::MOCHA).expect("vendored mocha.toml must parse")
        });
        MOCHA.clone()
    }
}

// ── Style Helpers ──

impl Theme {
    pub(crate) fn text(&self) -> Style {
        self.text.style()
    }

    pub(crate) fn muted(&self) -> Style {
        self.muted.style()
    }

    pub(crate) fn dim(&self) -> Style {
        self.dim.style()
    }

    pub(crate) fn surface(&self) -> Style {
        self.surface.style()
    }

    pub(crate) fn accent(&self) -> Style {
        self.accent.style()
    }

    pub(crate) fn user(&self) -> Style {
        self.user.style()
    }

    pub(crate) fn queued(&self) -> Style {
        self.queued.style()
    }

    pub(crate) fn assistant(&self) -> Style {
        self.assistant.style()
    }

    pub(crate) fn info(&self) -> Style {
        self.info.style()
    }

    pub(crate) fn success(&self) -> Style {
        self.success.style()
    }

    pub(crate) fn warning(&self) -> Style {
        self.warning.style()
    }

    pub(crate) fn error(&self) -> Style {
        self.error.style()
    }

    pub(crate) fn diff_add_row(&self) -> Style {
        Style::default().bg(self.diff_add.bg.unwrap_or(Color::Reset))
    }

    pub(crate) fn diff_del_row(&self) -> Style {
        Style::default().bg(self.diff_del.bg.unwrap_or(Color::Reset))
    }

    pub(crate) fn tool_border(&self) -> Style {
        self.tool_border.style()
    }

    pub(crate) fn tool_icon(&self) -> Style {
        self.tool_icon.style()
    }

    pub(crate) fn thinking(&self) -> Style {
        self.thinking.style()
    }

    pub(crate) fn separator_span(&self) -> Span<'static> {
        Span::styled(" │ ", self.separator())
    }

    pub(crate) fn separator(&self) -> Style {
        self.separator.style()
    }

    pub(crate) fn border_focused(&self) -> Style {
        self.border_focused.style()
    }

    pub(crate) fn border_unfocused(&self) -> Style {
        self.border_unfocused.style()
    }

    pub(crate) fn heading_h1(&self) -> Style {
        self.heading_h1.style()
    }

    pub(crate) fn heading_h2(&self) -> Style {
        self.heading_h2.style()
    }

    pub(crate) fn heading_h3(&self) -> Style {
        self.heading_h3.style()
    }

    pub(crate) fn heading_minor(&self) -> Style {
        self.heading_minor.style()
    }

    pub(crate) fn inline_code(&self) -> Style {
        self.inline_code.style()
    }

    pub(crate) fn code(&self) -> Style {
        self.code.style()
    }

    pub(crate) fn link(&self) -> Style {
        self.link.style()
    }

    pub(crate) fn blockquote(&self) -> Style {
        self.blockquote.style()
    }

    pub(crate) fn list_marker(&self) -> Style {
        self.list_marker.style()
    }

    pub(crate) fn horizontal_rule(&self) -> Style {
        self.horizontal_rule.style()
    }

    pub(crate) fn table_header(&self) -> Style {
        self.table_header.style()
    }

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
    fn slot_style_carries_modifiers() {
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
        assert_ne!(t.accent.fg, t.assistant.fg);
        assert_ne!(t.user.fg, t.assistant.fg);
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
        assert_eq!(t.assistant().fg, t.assistant.fg);
        assert_eq!(t.success().fg, t.success.fg);
        assert_eq!(t.warning().fg, t.warning.fg);
        assert_eq!(t.error().fg, t.error.fg);
        assert_eq!(t.inline_code().fg, t.inline_code.fg);
        assert_eq!(t.inline_code().bg, None);
        assert_eq!(t.code().fg, t.code.fg);
        assert_eq!(t.code().bg, None);
    }

    #[test]
    fn diff_row_helpers_set_only_background() {
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

    #[test]
    fn default_dim_cluster_matches_dim() {
        let t = Theme::default();
        let dim = t.dim();
        assert_eq!(t.border_unfocused(), dim);
        assert_eq!(t.horizontal_rule(), dim);
        assert_eq!(t.table_border(), dim);
    }

    #[test]
    fn default_table_header_matches_heading_h2() {
        let t = Theme::default();
        assert_eq!(t.table_header(), t.heading_h2());
    }
}
