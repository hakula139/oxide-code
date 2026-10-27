//! Theme palette and style helpers.

pub(crate) mod builtin;
mod color;
mod loader;

pub(crate) use self::loader::ThemeOverrides;
pub use self::loader::{ResolvedTheme, resolve_theme};

use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::highlighting::Theme as SyntaxTheme;

use ox_core::model::Effort;
use ox_core::tool::tasks::TaskStatus;

use crate::glyphs::SEGMENT_SEPARATOR;

/// Which color channel a slot paints.
///
/// A background-only slot fills a region behind text, so its bare-string form and its accessor set
/// [`bg`](Slot::bg), and an inline patch may set only `bg`. A foreground slot may set both channels
/// and carries modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotKind {
    Fg,
    Bg,
}

/// A theme slot whose optional colors and modifiers compose into a [`Style`].
///
/// Overrides patch each component independently. Unset colors use the terminal defaults.
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
            (text, Fg, "Primary text"),
            (muted, Fg, "Secondary text and labels, dimmer than text and brighter than dim"),
            (dim, Fg, "Low-emphasis text that stays readable, such as tool output and hints"),

            // Surfaces
            (surface, Bg, "Chat / input / status panel background"),

            // Semantic accents
            (accent, Fg, "Highlights and active borders"),
            (user, Fg, "User messages and icon"),
            (queued, Fg, "Queued user prompts in the preview panel"),
            (assistant, Fg, "Assistant messages and icon"),

            // Status indicators (ascending severity)
            (info, Fg, "Informational highlight (in-progress / neutral signals)"),
            (success, Fg, "Successful tools, added diff lines, checks, approvals, and notices"),
            (warning, Fg, "Warnings, caution status"),
            (error, Fg, "Errors, failed tools, critical status"),

            // Effort gradient
            (effort_low, Fg, "Effort `low` in the speed / intelligence slider"),
            (effort_medium, Fg, "Effort `medium` in the speed / intelligence slider"),
            (effort_high, Fg, "Effort `high` in the speed / intelligence slider"),
            (effort_xhigh, Fg, "Effort `xhigh` in the speed / intelligence slider"),
            (effort_max, Fg, "Effort `max` in the speed / intelligence slider"),

            // Code
            (code, Fg, "Fenced code blocks with no recognized language"),
            (inline_code, Fg, "Inline code spans (`` `code` ``)"),

            // Diff backgrounds
            (diff_add, Bg, "Background fill for added diff rows"),
            (diff_del, Bg, "Background fill for deleted diff rows"),

            // Markdown headings
            (heading_h1, Fg, "H1, the most prominent heading"),
            (heading_h2, Fg, "H2 section headers"),
            (heading_h3, Fg, "H3 subsection headers"),
            (heading_minor, Fg, "H4 to H6 minor headings"),

            // Markdown body
            (thinking, Fg, "Thinking text"),
            (link, Fg, "Markdown links"),
            (blockquote, Fg, "Markdown blockquote bar (`│`)"),
            (list_marker, Fg, "List item bullet / number marker"),

            // Markdown chrome
            (horizontal_rule, Fg, "Markdown horizontal rule (`---`)"),
            (table_header, Fg, "Markdown table header cells"),
            (table_border, Fg, "Markdown table borders and row rules"),

            // UI chrome
            (tool_border, Fg, "Tool block borders and the read and grep line-number separator"),
            (tool_icon, Fg, "Tool icon accent"),
            (border_focused, Fg, "Input rules while idle"),
            (border_unfocused, Fg, "Unfocused component border"),
            (separator, Fg, "Status-line segment separator"),
            (selection, Bg, "Chat text selected with the mouse"),
        }
    };
}

use for_each_slot;

/// Theme palette, with one slot per semantic role.
macro_rules! define_theme_struct {
    ( $( ($name:ident, $kind:ident, $doc:literal), )* ) => {
        #[derive(Debug, Clone)]
        pub(crate) struct Theme {
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
        static MOCHA: LazyLock<Theme> = LazyLock::new(|| {
            loader::parse_theme(builtin::MOCHA).expect("vendored mocha.toml must parse")
        });
        MOCHA.clone()
    }
}

// ── Style Helpers ──

macro_rules! define_slot_accessor {
    ($name:ident, Bg) => {
        pub(crate) fn $name(&self) -> Style {
            Style::default().bg(self.$name.bg.unwrap_or(Color::Reset))
        }
    };
    ($name:ident, Fg) => {
        pub(crate) fn $name(&self) -> Style {
            self.$name.style()
        }
    };
}

macro_rules! define_slot_accessors {
    ( $( ($name:ident, $kind:ident, $doc:literal), )* ) => {
        impl Theme {
            $( define_slot_accessor!($name, $kind); )*
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

    /// Style of one effort level on the speed / intelligence slider's gradient.
    pub(crate) fn effort_level(&self, effort: Effort) -> Style {
        match effort {
            Effort::Low => self.effort_low(),
            Effort::Medium => self.effort_medium(),
            Effort::High => self.effort_high(),
            Effort::Xhigh => self.effort_xhigh(),
            Effort::Max => self.effort_max(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    mod effort_level {
        use super::*;

        #[test]
        fn maps_each_level_to_its_own_slot() {
            let mut theme = Theme::default();
            let colors = [
                Color::Rgb(1, 1, 1),
                Color::Rgb(2, 2, 2),
                Color::Rgb(3, 3, 3),
                Color::Rgb(4, 4, 4),
                Color::Rgb(5, 5, 5),
            ];
            theme.effort_low.fg = Some(colors[0]);
            theme.effort_medium.fg = Some(colors[1]);
            theme.effort_high.fg = Some(colors[2]);
            theme.effort_xhigh.fg = Some(colors[3]);
            theme.effort_max.fg = Some(colors[4]);

            for (effort, color) in [
                (Effort::Low, colors[0]),
                (Effort::Medium, colors[1]),
                (Effort::High, colors[2]),
                (Effort::Xhigh, colors[3]),
                (Effort::Max, colors[4]),
            ] {
                assert_eq!(
                    theme.effort_level(effort),
                    Style::default().fg(color),
                    "{effort}",
                );
            }
        }
    }
}
