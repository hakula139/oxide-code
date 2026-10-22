//! Theme palette and style helpers.

pub(crate) mod builtin;
mod color;
mod loader;

pub use loader::{ResolvedTheme, ThemeOverrides, resolve_theme};

use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::highlighting::Theme as SyntaxTheme;

use ox_core::tool::tasks::TaskStatus;

use crate::glyphs::SEGMENT_SEPARATOR;

/// One theme slot with optional fg, bg, and modifiers composed into a [`Style`].
///
/// Each component is independently optional so an override can patch one axis (e.g., bg only)
/// without forcing callers to also restate the other two. [`Slot::style`] folds the three into
/// a ratatui `Style`, leaving unset axes as the terminal's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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

/// Canonical slot list. A new slot also needs a value in every built-in theme TOML, since the
/// loader requires every slot, and a row in `docs/guide/theming.md`.
macro_rules! for_each_slot {
    ($callback:ident) => {
        $callback! {
            // Text hierarchy
            (text, "Primary text"),
            (muted, "Secondary text and labels, dimmer than text and brighter than dim"),
            (dim, "Low-emphasis text that stays readable, such as tool output and hints"),

            // Surfaces
            (surface, "Chat / input / status panel background (bg-only)"),

            // Semantic accents
            (accent, "Highlights and active borders"),
            (user, "User messages and icon"),
            (queued, "Queued user prompts in the preview panel"),
            (assistant, "Assistant messages and icon"),

            // Status indicators (ascending severity)
            (info, "Informational highlight (in-progress / neutral signals)"),
            (success, "Successful tools, added diff lines, passing checks and approvals, notices"),
            (warning, "Warnings, caution status"),
            (error, "Errors, failed tools, critical status"),

            // Code
            (code, "Fenced code blocks with no recognized language"),
            (inline_code, "Inline code spans (`` `code` ``)"),

            // Diff backgrounds
            (diff_add, "Background fill for added diff rows (bg-only)"),
            (diff_del, "Background fill for deleted diff rows (bg-only)"),

            // Markdown headings
            (heading_h1, "H1, the most prominent heading"),
            (heading_h2, "H2 section headers"),
            (heading_h3, "H3 subsection headers"),
            (heading_minor, "H4 to H6 minor headings"),

            // Markdown body
            (thinking, "Thinking text"),
            (link, "Markdown links"),
            (blockquote, "Markdown blockquote bar (`│`)"),
            (list_marker, "List item bullet / number marker"),

            // Markdown chrome
            (horizontal_rule, "Markdown horizontal rule (`---`)"),
            (table_header, "Markdown table header cells"),
            (table_border, "Markdown table borders and row rules"),

            // UI chrome
            (tool_border, "Tool call block borders and the read and grep line-number separator"),
            (tool_icon, "Tool icon accent"),
            (border_focused, "Input rules while idle"),
            (border_unfocused, "Unfocused component border"),
            (separator, "Status-line segment separator"),
            (selection, "Chat text selected with the mouse (bg-only)"),
        }
    };
}

use for_each_slot;

/// Theme palette, with one slot per semantic role.
macro_rules! define_theme_struct {
    ( $( ($name:ident, $doc:literal), )* ) => {
        #[derive(Debug, Clone)]
        pub struct Theme {
            $(
                #[doc = $doc]
                pub(crate) $name: Slot,
            )*
            /// Syntect theme that colors tokens in fenced code with a recognized language.
            pub(crate) syntax: &'static SyntaxTheme,
        }

        #[cfg(test)]
        const SLOT_NAMES: &[&str] = &[ $(stringify!($name),)* ];
    };
}

for_each_slot!(define_theme_struct);

impl Default for Theme {
    fn default() -> Self {
        // Cache the parsed Mocha palette in a `LazyLock` so the TOML parse runs at most once per
        // process. Clones are cheap (every `Slot` is `Copy`).
        static MOCHA: LazyLock<Theme> = LazyLock::new(|| {
            loader::parse_theme(builtin::MOCHA).expect("vendored mocha.toml must parse")
        });
        MOCHA.clone()
    }
}

// ── Style Helpers ──

macro_rules! define_slot_accessor {
    (diff_add) => {
        pub(crate) fn diff_add_row(&self) -> Style {
            Style::default().bg(self.diff_add.bg.unwrap_or(Color::Reset))
        }
    };
    (diff_del) => {
        pub(crate) fn diff_del_row(&self) -> Style {
            Style::default().bg(self.diff_del.bg.unwrap_or(Color::Reset))
        }
    };
    ($name:ident) => {
        pub(crate) fn $name(&self) -> Style {
            self.$name.style()
        }
    };
}

macro_rules! define_slot_accessors {
    ( $( ($name:ident, $doc:literal), )* ) => {
        impl Theme {
            $( define_slot_accessor!($name); )*
        }
    };
}

for_each_slot!(define_slot_accessors);

impl Theme {
    pub(crate) fn row_style(&self, selected: bool) -> Style {
        if selected {
            self.text().add_modifier(Modifier::BOLD)
        } else {
            self.dim()
        }
    }

    pub(crate) fn separator_span(&self) -> Span<'static> {
        Span::styled(SEGMENT_SEPARATOR, self.separator())
    }

    /// Style of a task row, which highlights the task in progress and dims the finished ones.
    pub(crate) fn task_style(&self, status: TaskStatus) -> Style {
        match status {
            TaskStatus::Pending => self.text(),
            TaskStatus::InProgress => self.info(),
            TaskStatus::Completed => self.dim(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_add_row_and_diff_del_row_set_only_background() {
        let t = Theme::default();

        assert_eq!(
            t.diff_add_row(),
            Style::default().bg(t.diff_add.bg.unwrap())
        );
        assert_eq!(
            t.diff_del_row(),
            Style::default().bg(t.diff_del.bg.unwrap())
        );
    }

    mod slot {
        use super::*;

        mod style {
            use super::*;

            #[test]
            fn fg_only_sets_only_foreground() {
                let slot = Slot {
                    fg: Some(Color::Red),
                    ..Slot::default()
                };

                assert_eq!(slot.style(), Style::default().fg(Color::Red));
            }

            #[test]
            fn bg_only_sets_only_background() {
                let slot = Slot {
                    bg: Some(Color::Blue),
                    ..Slot::default()
                };

                assert_eq!(slot.style(), Style::default().bg(Color::Blue));
            }

            #[test]
            fn carries_modifiers() {
                let slot = Slot {
                    fg: Some(Color::Green),
                    bg: None,
                    modifiers: Modifier::BOLD | Modifier::ITALIC,
                };

                assert_eq!(
                    slot.style(),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC)
                );
            }
        }
    }

    mod default {
        use super::*;

        #[test]
        fn has_distinct_colors() {
            let t = Theme::default();

            assert_ne!(t.text.fg, t.muted.fg);
            assert_ne!(t.muted.fg, t.dim.fg);
            assert_ne!(t.accent.fg, t.assistant.fg);
            assert_ne!(t.user.fg, t.assistant.fg);
            assert_ne!(t.success.fg, t.error.fg);
            assert_ne!(t.diff_add.bg, t.diff_del.bg);
        }

        #[test]
        fn accent_is_bold() {
            let t = Theme::default();

            assert_eq!(t.accent.modifiers, Modifier::BOLD);
        }

        #[test]
        fn headings_step_down_from_bold_underlined_to_italic() {
            let t = Theme::default();
            let text = t.text();

            assert_eq!(
                t.heading_h1(),
                text.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            );
            assert_eq!(t.heading_h2(), text.add_modifier(Modifier::BOLD));
            assert_eq!(
                t.heading_h3(),
                text.add_modifier(Modifier::BOLD | Modifier::ITALIC)
            );
            assert_eq!(t.heading_minor(), text.add_modifier(Modifier::ITALIC));
        }

        #[test]
        fn link_is_underlined_accent() {
            let t = Theme::default();

            assert_eq!(
                t.link(),
                Style::default()
                    .fg(t.accent.fg.unwrap())
                    .add_modifier(Modifier::UNDERLINED)
            );
        }

        #[test]
        fn thinking_is_italic() {
            let t = Theme::default();

            assert!(t.thinking.modifiers.contains(Modifier::ITALIC));
        }

        #[test]
        fn status_slots_carry_no_modifiers() {
            let t = Theme::default();

            assert_eq!(t.success.modifiers, Modifier::empty());
            assert_eq!(t.error.modifiers, Modifier::empty());
        }
    }

    mod row_style {
        use super::*;

        #[test]
        fn selection_uses_bold_text_and_other_rows_are_dim() {
            let theme = Theme::default();

            assert_eq!(
                theme.row_style(true),
                theme.text().add_modifier(Modifier::BOLD)
            );
            assert_eq!(theme.row_style(false), theme.dim());
        }
    }

    mod separator_span {
        use super::*;

        #[test]
        fn pads_a_pipe_in_the_separator_style() {
            let t = Theme::default();

            assert_eq!(t.separator_span(), Span::styled(" │ ", t.separator()));
        }
    }

    mod task_style {
        use super::*;

        #[test]
        fn highlights_the_task_in_progress_and_dims_completed_ones() {
            let t = Theme::default();

            assert_eq!(t.task_style(TaskStatus::Pending), t.text());
            assert_eq!(t.task_style(TaskStatus::InProgress), t.info());
            assert_eq!(t.task_style(TaskStatus::Completed), t.dim());
        }
    }
}
