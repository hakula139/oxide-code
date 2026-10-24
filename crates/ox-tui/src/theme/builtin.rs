//! Built-in theme catalog. TOML bodies are embedded via `include_str!` so the binary stays
//! self-contained and the vendored files in `crates/ox-tui/themes/` double as user examples.

use super::{Theme, loader};

pub(super) const MOCHA: &str = include_str!("../../themes/mocha.toml");

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BuiltinTheme {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(super) body: &'static str,
}

impl BuiltinTheme {
    /// Parses the vendored palette. Panics on a parse failure, which the loader test
    /// `parse_theme::parses_every_builtin` rules out.
    pub(crate) fn load(&self) -> Theme {
        loader::parse_theme(self.body).expect("vendored builtin theme TOML must parse")
    }
}

/// Catppuccin variants listed dark → light, then non-Catppuccin palettes. `/theme` lists them in
/// this order.
pub(crate) const BUILT_IN: &[BuiltinTheme] = &[
    BuiltinTheme {
        name: "mocha",
        description: "Catppuccin dark (default)",
        body: MOCHA,
    },
    BuiltinTheme {
        name: "macchiato",
        description: "Catppuccin dark (medium)",
        body: include_str!("../../themes/macchiato.toml"),
    },
    BuiltinTheme {
        name: "frappe",
        description: "Catppuccin dark (soft)",
        body: include_str!("../../themes/frappe.toml"),
    },
    BuiltinTheme {
        name: "latte",
        description: "Catppuccin light",
        body: include_str!("../../themes/latte.toml"),
    },
    BuiltinTheme {
        name: "material",
        description: "Material dark",
        body: include_str!("../../themes/material.toml"),
    },
];

pub(crate) fn find(name: &str) -> Option<&'static BuiltinTheme> {
    BUILT_IN.iter().find(|theme| theme.name == name)
}

/// The built-in names in [`BUILT_IN`] order, joined with commas for messages.
pub(crate) fn names() -> String {
    BUILT_IN
        .iter()
        .map(|theme| theme.name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;
    use crate::theme::Slot;

    mod find {
        use super::*;

        #[test]
        fn finds_each_builtin_by_name() {
            for theme in BUILT_IN {
                assert_eq!(find(theme.name), Some(theme));
            }
        }

        #[test]
        fn unknown_name_is_absent() {
            assert_eq!(find("solarized"), None);
            assert_eq!(find(""), None);
        }
    }

    mod built_in {
        use super::*;

        /// Background for each palette, since built-ins leave `surface` to the terminal.
        const BASES: &[(&str, Color)] = &[
            ("mocha", Color::Rgb(0x1e, 0x1e, 0x2e)),
            ("macchiato", Color::Rgb(0x24, 0x27, 0x3a)),
            ("frappe", Color::Rgb(0x30, 0x34, 0x46)),
            ("latte", Color::Rgb(0xef, 0xf1, 0xf5)),
            ("material", Color::Rgb(0x12, 0x12, 0x12)),
        ];

        /// Every built-in with its parsed palette and base.
        fn palettes() -> impl Iterator<Item = (&'static str, Theme, Color)> {
            BUILT_IN.iter().map(|theme| {
                let name = theme.name;
                let base = BASES
                    .iter()
                    .find_map(|&(base_name, base)| (base_name == name).then_some(base))
                    .unwrap_or_else(|| panic!("add the {name:?} base color to BASES"));
                (name, theme.load(), base)
            })
        }

        /// WCAG 2 contrast ratio, from 1:1 for equal colors to 21:1 for black on white.
        fn contrast(a: Color, b: Color) -> f64 {
            let (a, b) = (luminance(a), luminance(b));
            (a.max(b) + 0.05) / (a.min(b) + 0.05)
        }

        /// WCAG 2 relative luminance of an sRGB color.
        fn luminance(color: Color) -> f64 {
            let Color::Rgb(r, g, b) = color else {
                panic!("built-in colors are hex, got {color:?}");
            };
            let linear = |channel: u8| {
                let c = f64::from(channel) / 255.0;
                if c <= 0.040_45 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
        }

        fn fg(slot: Slot) -> Color {
            slot.fg.expect("slot sets a foreground")
        }

        /// `#767676` is the lightest gray that reaches WCAG AA's 4.5:1 on white.
        #[test]
        fn contrast_matches_wcag_reference_ratios() {
            let white = Color::Rgb(255, 255, 255);

            let black_on_white = contrast(Color::Rgb(0, 0, 0), white);
            let gray_on_white = contrast(white, Color::Rgb(0x76, 0x76, 0x76));

            assert!((black_on_white - 21.0).abs() < 1e-9, "{black_on_white}");
            assert!((gray_on_white - 4.54).abs() < 0.005, "{gray_on_white}");
        }

        #[test]
        fn dim_and_thinking_are_no_fainter_than_the_tool_border() {
            for (name, theme, base) in palettes() {
                let border = contrast(fg(theme.tool_border), base);
                for (slot, value) in [("dim", theme.dim), ("thinking", theme.thinking)] {
                    let ratio = contrast(fg(value), base);
                    assert!(
                        ratio >= border,
                        "{name}: {slot} reaches {ratio:.2}:1, below tool_border's {border:.2}:1",
                    );
                }
            }
        }

        #[test]
        fn dim_and_thinking_reach_4_to_1_on_dark_bases() {
            for (name, theme, base) in palettes() {
                if luminance(fg(theme.text)) < luminance(base) {
                    continue;
                }
                for (slot, value) in [("dim", theme.dim), ("thinking", theme.thinking)] {
                    let ratio = contrast(fg(value), base);
                    assert!(ratio >= 4.0, "{name}: {slot} reaches only {ratio:.2}:1");
                }
            }
        }

        #[test]
        fn inline_code_differs_from_user() {
            for (name, theme, _) in palettes() {
                assert_ne!(theme.inline_code.fg, theme.user.fg, "{name}");
            }
        }

        #[test]
        fn chrome_slots_reuse_accent_and_tool_border_colors() {
            for (name, theme, _) in palettes() {
                assert_eq!(theme.tool_icon.fg, theme.accent.fg, "{name}");
                assert_eq!(theme.list_marker.fg, theme.accent.fg, "{name}");
                assert_eq!(theme.blockquote.fg, theme.tool_border.fg, "{name}");
                assert_eq!(theme.border_focused.fg, theme.tool_border.fg, "{name}");
            }
        }

        #[test]
        fn rules_and_unfocused_borders_share_a_color_fainter_than_dim() {
            for (name, theme, base) in palettes() {
                let rule = theme.border_unfocused;
                for slot in [theme.horizontal_rule, theme.table_border, theme.separator] {
                    assert_eq!(slot, rule, "{name}");
                }
                let rule_ratio = contrast(fg(rule), base);
                let dim_ratio = contrast(fg(theme.dim), base);
                assert!(
                    rule_ratio < dim_ratio,
                    "{name}: rules reach {rule_ratio:.2}:1, not below dim's {dim_ratio:.2}:1",
                );
            }
        }

        /// Contrast ratios multiply across a color between two others, so the strongest highlight
        /// that keeps `dim` at 3:1 reaches a third of `dim`'s contrast with the base.
        #[test]
        fn selection_comes_within_2_percent_of_the_strongest_highlight() {
            for (name, theme, base) in palettes() {
                let selection = theme.selection.bg.expect("selection sets a background");
                let ratio = contrast(selection, base);
                let strongest = contrast(fg(theme.dim), base) / 3.0;
                assert!(
                    ratio >= 0.98 * strongest,
                    "{name}: selection reaches {ratio:.3}:1, short of {strongest:.3}:1",
                );
            }
        }

        #[test]
        fn dim_and_thinking_stay_at_3_to_1_on_the_selection() {
            for (name, theme, _) in palettes() {
                let selection = theme.selection.bg.expect("selection sets a background");
                for (slot, value) in [("dim", theme.dim), ("thinking", theme.thinking)] {
                    let ratio = contrast(fg(value), selection);
                    assert!(ratio >= 3.0, "{name}: {slot} reaches only {ratio:.2}:1");
                }
            }
        }
    }
}
