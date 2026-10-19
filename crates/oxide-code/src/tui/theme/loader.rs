//! Theme TOML parsing and resolution.
//!
//! A theme document is a flat TOML body with one entry per slot plus an optional `syntax_theme`
//! naming a bundled syntect theme for highlighted code. Each slot value is either:
//!
//! - a **bare color string** (`text = "#cdd6f4"`): sets `fg` only.
//! - an **inline table** (`accent = { fg = "#89b4fa", bold = true }`): explicit `fg` / `bg` plus
//!   modifiers (`bold`, `italic`, `underlined`, `dim`, `reversed`).
//!
//! Every slot must be present. `deny_unknown_fields` catches typos. Per-slot parse errors are
//! wrapped with the slot name.

use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use ratatui::style::Modifier;
use serde::Deserialize;
use syntect::highlighting::{Theme as SyntaxTheme, ThemeSet};

use super::{Slot, Theme, builtin, color::parse_color};
use crate::config::ThemeConfig;
use crate::util::path::expand_user;

// ── Resolution ──

/// A theme resolved from `[tui.theme]`, with one notice for each override it ignored.
#[derive(Debug)]
pub(crate) struct ResolvedTheme {
    pub(crate) theme: Theme,
    pub(crate) notices: Vec<String>,
}

/// Resolves the `[tui.theme]` base and applies its per-slot overrides. An unknown or unreadable
/// base is an error, while a bad override, including a malformed patch, keeps the base value and
/// adds a notice for the caller to report, so the TUI still launches.
pub(crate) fn resolve_theme(config: &ThemeConfig) -> Result<ResolvedTheme> {
    let base_name = config.base.as_str();
    let body = load_base_body(base_name)?;
    let mut theme =
        parse_theme(&body).with_context(|| format!("parsing base theme {base_name:?}"))?;

    let mut notices = Vec::new();
    for (slot_name, patch) in &config.overrides {
        if let Err(e) = patch_slot(&mut theme, slot_name, patch) {
            let reason = format!("{e:#}");
            notices.push(format!(
                "Ignored the `{slot_name}` theme override: {}",
                reason.trim_end(),
            ));
        }
    }
    Ok(ResolvedTheme { theme, notices })
}

/// Theme bodies hold one line per slot, so a larger file is not a theme.
const MAX_THEME_FILE_BYTES: u64 = 64 * 1024;

/// Resolves a `base` to a TOML body. Built-in catalog first, then a filesystem path with
/// `~/` expanded to `$HOME`.
fn load_base_body(name: &str) -> Result<String> {
    if let Some(body) = builtin::lookup(name) {
        return Ok(body.to_owned());
    }
    let path = expand_user(name).with_context(|| format!("theme {name:?}"))?;
    let unreadable = || {
        format!(
            "theme {name:?}: not a built-in name and failed to read as file {}",
            path.display(),
        )
    };

    // A project config can name any path, so a device or FIFO must fail before the read blocks.
    let meta = std::fs::metadata(&path).with_context(unreadable)?;
    if !meta.is_file() {
        bail!(
            "theme {name:?}: not a built-in name, and {} is not a file",
            path.display(),
        );
    }
    if meta.len() > MAX_THEME_FILE_BYTES {
        bail!(
            "theme {name:?}: {} is {} bytes, over the {MAX_THEME_FILE_BYTES}-byte theme file limit",
            path.display(),
            meta.len(),
        );
    }
    std::fs::read_to_string(&path).with_context(unreadable)
}

/// Applies one override patch. Errors on unknown slot name, a malformed patch, or a bad color.
fn patch_slot(theme: &mut Theme, slot_name: &str, patch: &toml::Value) -> Result<()> {
    let slot =
        slot_for_name(theme, slot_name).with_context(|| format!("unknown slot {slot_name:?}"))?;
    let patch = SlotPatch::deserialize(patch.clone())
        .context("expected a color string or a table of fg, bg, and modifier flags")?;
    if matches!(&patch, SlotPatch::Inline(inline) if inline.is_empty()) {
        bail!("empty patch (no fg, bg, or modifier flags)");
    }
    *slot = patch.apply(*slot)?;
    Ok(())
}

/// Generated mutable slot lookup. The mapping can't drift from [`Theme`]'s fields.
macro_rules! define_slot_for_name {
    ( $( ($name:ident, $doc:literal), )* ) => {
        fn slot_for_name<'a>(theme: &'a mut Theme, name: &str) -> Option<&'a mut Slot> {
            match name {
                $( stringify!($name) => Some(&mut theme.$name), )*
                _ => None,
            }
        }
    };
}

super::for_each_slot!(define_slot_for_name);

// ── SlotPatch ──

/// One slot value from a theme file or `[tui.theme.overrides]`, applied over a base slot. A bare
/// string sets `fg` only, and an inline table changes only the fields it names, so
/// `accent = { bold = false }` clears bold without touching `fg`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SlotPatch {
    Bare(String),
    Inline(InlinePatch),
}

/// Inline TOML patch with every field optional. `Option<bool>` distinguishes "no change" / "set" /
/// "clear". [`patch_slot`] rejects an empty override (`error = {}`), so the slot warns and keeps
/// its base value. Serde's untagged enum dispatcher would swallow a `Deserialize`-time message,
/// so that check runs after deserialization.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InlinePatch {
    fg: Option<String>,
    bg: Option<String>,
    bold: Option<bool>,
    italic: Option<bool>,
    underlined: Option<bool>,
    dim: Option<bool>,
    reversed: Option<bool>,
}

impl SlotPatch {
    fn apply(&self, base: Slot) -> Result<Slot> {
        match self {
            Self::Bare(s) => Ok(Slot {
                fg: Some(parse_color(s)?),
                bg: base.bg,
                modifiers: base.modifiers,
            }),
            Self::Inline(p) => p.apply(base),
        }
    }
}

impl InlinePatch {
    fn is_empty(&self) -> bool {
        self.fg.is_none()
            && self.bg.is_none()
            && self.bold.is_none()
            && self.italic.is_none()
            && self.underlined.is_none()
            && self.dim.is_none()
            && self.reversed.is_none()
    }

    fn apply(&self, base: Slot) -> Result<Slot> {
        let fg = self
            .fg
            .as_deref()
            .map(parse_color)
            .transpose()
            .context("fg")?
            .or(base.fg);
        let bg = self
            .bg
            .as_deref()
            .map(parse_color)
            .transpose()
            .context("bg")?
            .or(base.bg);
        let mut modifiers = base.modifiers;
        for (flag, modifier) in [
            (self.bold, Modifier::BOLD),
            (self.italic, Modifier::ITALIC),
            (self.underlined, Modifier::UNDERLINED),
            (self.dim, Modifier::DIM),
            (self.reversed, Modifier::REVERSED),
        ] {
            if let Some(set) = flag {
                modifiers = if set {
                    modifiers | modifier
                } else {
                    modifiers & !modifier
                };
            }
        }
        Ok(Slot { fg, bg, modifiers })
    }
}

// ── Parsing ──

/// Where every slot is described, linked from a missing-slot error.
const THEMING_GUIDE: &str =
    "https://github.com/hakula139/oxide-code/blob/main/docs/guide/theming.md";

pub(super) fn parse_theme(content: &str) -> Result<Theme> {
    let file: ThemeFile = match toml::from_str(content) {
        Ok(file) => file,
        Err(e) => {
            if let Some(slot) = missing_slot(&e) {
                bail!(
                    "the theme has no `{slot}` slot. \
                     Add one, such as `{}`. \
                     {THEMING_GUIDE} lists every slot.",
                    mocha_line(slot),
                );
            }
            return Err(e).context("invalid theme TOML");
        }
    };
    file.into_theme()
}

/// The slot a theme body left out, when that is why it failed to parse.
fn missing_slot(error: &toml::de::Error) -> Option<&str> {
    error
        .message()
        .strip_prefix("missing field `")?
        .strip_suffix('`')
}

/// The line that sets `slot` in the Mocha theme, which defines every slot.
fn mocha_line(slot: &str) -> &'static str {
    builtin::MOCHA
        .lines()
        .find(|line| {
            line.strip_prefix(slot)
                .is_some_and(|rest| rest.starts_with(" ="))
        })
        .expect("mocha.toml sets every slot")
}

/// `ThemeFile` deserialization and its `into_theme` converter, which wraps each slot's parse error
/// with the slot name. `deny_unknown_fields` catches typos.
macro_rules! define_theme_file {
    ( $( ($name:ident, $doc:literal), )* ) => {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ThemeFile {
            $( $name: SlotPatch, )*
            syntax_theme: Option<String>,
        }

        impl ThemeFile {
            fn into_theme(self) -> Result<Theme> {
                let parse = |patch: SlotPatch, name: &'static str| -> Result<Slot> {
                    patch
                        .apply(Slot::default())
                        .with_context(|| format!("slot {name:?}"))
                };
                Ok(Theme {
                    $( $name: parse(self.$name, stringify!($name))?, )*
                    syntax: syntax_theme(self.syntax_theme.as_deref())?,
                })
            }
        }
    };
}

super::for_each_slot!(define_theme_file);

static SYNTAX_THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Looks up a bundled syntect theme by name, defaulting to `base16-ocean.dark` when unset.
fn syntax_theme(name: Option<&str>) -> Result<&'static SyntaxTheme> {
    let name = name.unwrap_or("base16-ocean.dark");
    SYNTAX_THEMES.themes.get(name).with_context(|| {
        let known: Vec<&str> = SYNTAX_THEMES.themes.keys().map(String::as_str).collect();
        format!(
            "syntax_theme {name:?}: expected one of {}",
            known.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use indoc::{formatdoc, indoc};
    use ratatui::style::Color;

    use super::*;
    use crate::tui::theme::SLOT_NAMES;

    const COLOR_FORMATS_HINT: &str = "expected hex (#rrggbb), ANSI name (red, bright_blue, ...), \
                                      indexed (ansi:0..255), or reset";

    mod resolve_theme {
        use super::*;

        fn config(base: &str, overrides: &str) -> ThemeConfig {
            ThemeConfig {
                base: base.to_owned(),
                overrides: overrides.parse().unwrap(),
            }
        }

        /// The theme `config` resolves to, after checking that it ignored no override.
        fn resolved(config: &ThemeConfig) -> Theme {
            let ResolvedTheme { theme, notices } = resolve_theme(config).unwrap();
            assert_eq!(notices, [] as [String; 0], "{config:?}");
            theme
        }

        #[test]
        fn named_builtin_loads_that_palette() {
            let t = resolved(&config("latte", ""));
            assert_eq!(t.text.fg, Some(Color::Rgb(0x4c, 0x4f, 0x69)));
        }

        #[test]
        fn unknown_name_with_no_matching_file_errors() {
            let err = resolve_theme(&config("solarized", ""))
                .expect_err("unknown built-in and not a path");
            let msg = format!("{err:#}");
            assert!(
                msg.starts_with(
                    r#"theme "solarized": not a built-in name and failed to read as file solarized: "#
                ),
                "{msg}",
            );
        }

        #[test]
        fn rejects_a_base_path_that_is_not_a_theme_sized_file() {
            let dir = tempfile::tempdir().unwrap();
            let dir_str = dir.path().to_string_lossy().into_owned();
            let err = resolve_theme(&config(&dir_str, "")).expect_err("a directory is not a theme");
            assert_eq!(
                format!("{err:#}"),
                format!("theme {dir_str:?}: not a built-in name, and {dir_str} is not a file"),
            );

            let path = dir.path().join("huge.toml");
            let len = MAX_THEME_FILE_BYTES + 1;
            std::fs::File::create(&path).unwrap().set_len(len).unwrap();
            let path_str = path.to_string_lossy().into_owned();
            let err =
                resolve_theme(&config(&path_str, "")).expect_err("an oversized file is rejected");
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "theme {path_str:?}: {path_str} is {len} bytes, \
                     over the {MAX_THEME_FILE_BYTES}-byte theme file limit"
                ),
            );
        }

        #[test]
        fn loads_from_file_path() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("custom.toml");
            let body = builtin::MOCHA.replace(r##"error = "#f38ba8""##, r##"error = "#ff0000""##);
            std::fs::write(&path, body).unwrap();

            let t = resolved(&config(&path.to_string_lossy(), ""));
            assert_eq!(t.error.fg, Some(Color::Rgb(0xff, 0x00, 0x00)));
        }

        #[test]
        fn file_path_with_bad_body_wraps_with_base_name() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("broken.toml");
            let body = builtin::MOCHA.replace(r##"error = "#f38ba8""##, r#"error = "orange""#);
            std::fs::write(&path, body).unwrap();
            let path_str = path.to_string_lossy().into_owned();

            let err = resolve_theme(&config(&path_str, "")).expect_err("bad slot color must error");
            let msg = format!("{err:#}");
            assert!(
                msg.contains(&format!("parsing base theme {path_str:?}")),
                "{msg}"
            );
            assert!(msg.contains("orange"), "{msg}");
        }

        #[test]
        fn bare_string_override_patches_only_fg() {
            // Mocha's accent is bold blue, so the override must replace the fg and keep the bold.
            let t = resolved(&config("mocha", r##"accent = "#ff0000""##));
            assert_eq!(t.accent.fg, Some(Color::Rgb(0xff, 0x00, 0x00)));
            assert_eq!(t.accent.bg, None);
            assert!(
                t.accent.modifiers.contains(Modifier::BOLD),
                "bold from base must survive a bare-string override",
            );
        }

        #[test]
        fn inline_override_clears_modifier_with_false() {
            let t = resolved(&config("mocha", "accent = { bold = false }"));
            assert_eq!(t.accent.fg, Some(Color::Rgb(0x89, 0xb4, 0xfa)));
            assert!(!t.accent.modifiers.contains(Modifier::BOLD));
        }

        #[test]
        fn inline_override_can_add_a_modifier() {
            let t = resolved(&config("mocha", "success = { italic = true }"));
            assert!(t.success.modifiers.contains(Modifier::ITALIC));
            assert_eq!(t.success.fg, Some(Color::Rgb(0xa6, 0xe3, 0xa1)));
        }

        #[test]
        fn unknown_slot_override_keeps_the_base_with_a_notice() {
            let ResolvedTheme { theme, notices } =
                resolve_theme(&config("mocha", r##"purple_thing = "#ff0000""##)).unwrap();

            assert_eq!(theme.error.fg, Some(Color::Rgb(0xf3, 0x8b, 0xa8)));
            assert_eq!(
                notices,
                [r#"Ignored the `purple_thing` theme override: unknown slot "purple_thing""#]
            );
        }

        #[test]
        fn invalid_color_override_keeps_the_base_with_a_notice() {
            let ResolvedTheme { theme, notices } =
                resolve_theme(&config("mocha", r#"error = "not-a-color""#)).unwrap();

            assert_eq!(theme.error.fg, Some(Color::Rgb(0xf3, 0x8b, 0xa8)));
            assert_eq!(
                notices,
                [format!(
                    "Ignored the `error` theme override: unknown color \"not-a-color\": \
                     {COLOR_FORMATS_HINT}"
                )]
            );
        }

        #[test]
        fn malformed_patches_keep_the_base_with_a_notice_each() {
            let ResolvedTheme { theme, notices } = resolve_theme(&config(
                "mocha",
                indoc! {r##"
                    error = 5
                    accent = { fg = "#ff0000", glow = true }
                "##},
            ))
            .unwrap();

            assert_eq!(theme.error.fg, Some(Color::Rgb(0xf3, 0x8b, 0xa8)));
            assert_eq!(theme.accent.fg, Some(Color::Rgb(0x89, 0xb4, 0xfa)));
            let reason = "expected a color string or a table of fg, bg, and modifier flags: \
                          data did not match any variant of untagged enum SlotPatch";
            assert_eq!(
                notices,
                [
                    format!("Ignored the `accent` theme override: {reason}"),
                    format!("Ignored the `error` theme override: {reason}"),
                ]
            );
        }

        #[test]
        fn inline_override_with_bad_fg_keeps_the_base_with_a_notice() {
            let ResolvedTheme { theme, notices } =
                resolve_theme(&config("mocha", r#"accent = { fg = "not-a-color" }"#)).unwrap();

            assert_eq!(theme.accent.fg, Some(Color::Rgb(0x89, 0xb4, 0xfa)));
            assert!(
                theme.accent.modifiers.contains(Modifier::BOLD),
                "base modifiers preserved on fallback",
            );
            assert_eq!(
                notices,
                [format!(
                    "Ignored the `accent` theme override: fg: unknown color \"not-a-color\": \
                     {COLOR_FORMATS_HINT}"
                )]
            );
        }

        #[test]
        fn inline_override_with_bad_bg_keeps_the_base_with_a_notice() {
            let ResolvedTheme { theme, notices } =
                resolve_theme(&config("mocha", r#"diff_add = { bg = "not-a-color" }"#)).unwrap();

            assert_eq!(theme.diff_add.bg, Some(Color::Rgb(0x2a, 0x3a, 0x37)));
            assert_eq!(
                notices,
                [format!(
                    "Ignored the `diff_add` theme override: bg: unknown color \"not-a-color\": \
                     {COLOR_FORMATS_HINT}"
                )]
            );
        }

        #[test]
        fn multiple_overrides_apply_independently() {
            let t = resolved(&config(
                "mocha",
                indoc! {r##"
                    error = "#ff0000"
                    success = "#00ff00"
                "##},
            ));
            assert_eq!(t.error.fg, Some(Color::Rgb(0xff, 0x00, 0x00)));
            assert_eq!(t.success.fg, Some(Color::Rgb(0x00, 0xff, 0x00)));
        }
    }

    mod patch_slot {
        use super::*;

        #[test]
        fn empty_table_is_rejected_and_keeps_base() {
            let mut theme = Theme::default();
            let accent = theme.accent;

            let err = patch_slot(
                &mut theme,
                "accent",
                &toml::Value::Table(toml::Table::new()),
            )
            .unwrap_err();

            assert_eq!(
                format!("{err:#}"),
                "empty patch (no fg, bg, or modifier flags)"
            );
            assert_eq!(theme.accent, accent);
        }
    }

    mod slot_for_name {
        use super::*;

        #[test]
        fn each_name_resolves_to_a_unique_slot() {
            let sentinel = Slot {
                fg: Some(Color::Rgb(0xde, 0xad, 0xbe)),
                bg: None,
                modifiers: Modifier::empty(),
            };
            for &target in SLOT_NAMES {
                let mut patched = Theme::default();
                let original = patched.clone();
                *slot_for_name(&mut patched, target)
                    .unwrap_or_else(|| panic!("unknown slot {target:?}")) = sentinel;
                for &other in SLOT_NAMES {
                    let mut p = patched.clone();
                    let mut o = original.clone();
                    let post = *slot_for_name(&mut p, other).expect("slot must exist");
                    let pre = *slot_for_name(&mut o, other).expect("slot must exist");
                    if other == target {
                        assert_eq!(post, sentinel, "patched slot {target} should hold sentinel");
                    } else {
                        assert_eq!(
                            post, pre,
                            "patching {target} must not affect {other} (slot_for_name mis-routing)",
                        );
                    }
                }
            }
        }
    }

    mod slot_patch {
        use super::*;

        mod apply {
            use super::*;

            #[test]
            fn inline_with_bg_only_keeps_base_fg_and_modifiers() {
                let base = Slot {
                    fg: Some(Color::Red),
                    bg: None,
                    modifiers: Modifier::BOLD,
                };
                let patch = SlotPatch::Inline(InlinePatch {
                    bg: Some("#000000".to_owned()),
                    ..InlinePatch::default()
                });

                let out = patch.apply(base).unwrap();

                assert_eq!(
                    out,
                    Slot {
                        bg: Some(Color::Rgb(0, 0, 0)),
                        ..base
                    }
                );
            }

            #[test]
            fn inline_none_modifiers_preserve_every_base_flag() {
                // A patch that names no modifier leaves every base modifier set. Reading an absent
                // flag as `false` would clear them whenever an unrelated field changes.
                let base = Slot {
                    fg: Some(Color::Red),
                    bg: None,
                    modifiers: Modifier::BOLD
                        | Modifier::ITALIC
                        | Modifier::UNDERLINED
                        | Modifier::DIM
                        | Modifier::REVERSED,
                };
                let patch = SlotPatch::Inline(InlinePatch {
                    fg: Some("#abcdef".to_owned()),
                    ..InlinePatch::default()
                });

                let out = patch.apply(base).unwrap();

                assert_eq!(
                    out,
                    Slot {
                        fg: Some(Color::Rgb(0xab, 0xcd, 0xef)),
                        ..base
                    }
                );
            }

            #[test]
            fn inline_with_fg_overwrites_base_fg() {
                let base = Slot {
                    fg: Some(Color::Red),
                    bg: Some(Color::Rgb(0x10, 0x10, 0x10)),
                    modifiers: Modifier::ITALIC,
                };
                let patch = SlotPatch::Inline(InlinePatch {
                    fg: Some("#abcdef".to_owned()),
                    ..InlinePatch::default()
                });

                let out = patch.apply(base).unwrap();

                assert_eq!(
                    out,
                    Slot {
                        fg: Some(Color::Rgb(0xab, 0xcd, 0xef)),
                        ..base
                    }
                );
            }
        }
    }

    mod parse_theme {
        use super::*;

        /// Replaces one line of the Mocha body, for a fixture one line away from a valid theme.
        fn mocha_with_slot_replacement(from: &str, to: &str) -> String {
            let body = builtin::MOCHA.replace(from, to);
            assert_ne!(body, builtin::MOCHA, "fixture marker {from:?} not found");
            body
        }

        #[test]
        fn all_builtins() {
            for builtin::BuiltinTheme { name, body, .. } in builtin::BUILT_IN {
                parse_theme(body).unwrap_or_else(|e| panic!("built-in {name:?} failed: {e:#}"));
            }
        }

        /// `surface` is the bg-only panel fill, and `selection` the bg-only highlight that keeps
        /// each selected cell's own text color. A built-in that declared either as a bare string
        /// would set `fg` instead and repaint the text it sits under.
        #[test]
        fn surface_and_selection_are_bg_only_in_every_builtin() {
            for builtin::BuiltinTheme { name, body, .. } in builtin::BUILT_IN {
                let t = parse_theme(body).unwrap_or_else(|e| panic!("{name} parse: {e:#}"));
                for (slot, style) in [("surface", t.surface), ("selection", t.selection)] {
                    assert_eq!(style.fg, None, "{name}: {slot} must be bg-only, never fg");
                    assert!(
                        style.bg.is_some(),
                        r#"{name}: {slot} must declare a bg (use "reset" for terminal default)"#,
                    );
                }
            }
        }

        #[test]
        fn table_header_is_the_bold_accent_in_every_builtin() {
            for builtin::BuiltinTheme { name, body, .. } in builtin::BUILT_IN {
                let t = parse_theme(body).unwrap_or_else(|e| panic!("{name} parse: {e:#}"));
                assert_eq!(t.table_header, t.accent, "{name}");
                assert_eq!(t.table_header.modifiers, Modifier::BOLD, "{name}");
            }
        }

        #[test]
        fn mocha_matches_known_palette() {
            let t = parse_theme(builtin::MOCHA).unwrap();
            assert_eq!(t.text.fg, Some(Color::Rgb(0xcd, 0xd6, 0xf4)), "Text");
            assert_eq!(t.muted.fg, Some(Color::Rgb(0xa6, 0xad, 0xc8)), "Subtext0");
            assert_eq!(t.dim.fg, Some(Color::Rgb(0x58, 0x5b, 0x70)), "Surface2");
            assert_eq!(t.accent.fg, Some(Color::Rgb(0x89, 0xb4, 0xfa)), "Blue");
            assert_eq!(t.user.fg, Some(Color::Rgb(0xfa, 0xb3, 0x87)), "Peach");
            assert_eq!(
                t.assistant.fg,
                Some(Color::Rgb(0xb4, 0xbe, 0xfe)),
                "Lavender",
            );
            assert_eq!(t.success.fg, Some(Color::Rgb(0xa6, 0xe3, 0xa1)), "Green");
            assert_eq!(t.warning.fg, Some(Color::Rgb(0xf9, 0xe2, 0xaf)), "Yellow");
            assert_eq!(t.error.fg, Some(Color::Rgb(0xf3, 0x8b, 0xa8)), "Red");
            assert_eq!(
                t.diff_add.bg,
                Some(Color::Rgb(0x2a, 0x3a, 0x37)),
                "delta plus",
            );
            assert_eq!(
                t.diff_del.bg,
                Some(Color::Rgb(0x38, 0x2c, 0x34)),
                "delta minus",
            );
        }

        /// `text` is the one slot every palette gives a different shade.
        #[test]
        fn non_mocha_palettes_match_known_text_color() {
            for (name, expected) in [
                ("latte", Color::Rgb(0x4c, 0x4f, 0x69)),
                ("frappe", Color::Rgb(0xc6, 0xd0, 0xf5)),
                ("macchiato", Color::Rgb(0xca, 0xd3, 0xf5)),
                ("material", Color::Rgb(0xde, 0xde, 0xde)),
            ] {
                let body = builtin::lookup(name).unwrap();

                let t = parse_theme(body).unwrap_or_else(|e| panic!("{name} parse: {e:#}"));

                assert_eq!(t.text.fg, Some(expected), "{name} text.fg");
            }
        }

        #[test]
        fn mocha_modifiers_match_default() {
            let t = parse_theme(builtin::MOCHA).unwrap();
            assert!(t.accent.modifiers.contains(Modifier::BOLD));
            assert!(t.thinking.modifiers.contains(Modifier::ITALIC));
            assert!(t.heading_h1.modifiers.contains(Modifier::BOLD));
            assert!(t.heading_h1.modifiers.contains(Modifier::UNDERLINED));
            assert!(t.link.modifiers.contains(Modifier::UNDERLINED));
            assert!(t.success.modifiers.is_empty());
            assert!(t.error.modifiers.is_empty());
        }

        #[test]
        fn missing_required_slot_names_it_with_a_value_to_add() {
            let body = builtin::MOCHA.replace(r##"selection = { bg = "#313244" }"##, "");

            let err = parse_theme(&body).expect_err("missing required slot");

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "the theme has no `selection` slot. \
                     Add one, such as `selection = {{ bg = \"#313244\" }}`. \
                     {THEMING_GUIDE} lists every slot."
                ),
            );
        }

        #[test]
        fn unknown_slot_key() {
            let mocha = builtin::MOCHA;
            let body = formatdoc! {r##"
                {mocha}

                unknown_slot = "#000000"
            "##};
            let err = parse_theme(&body).expect_err("unknown slot rejected");
            let msg = format!("{err:#}");
            assert!(msg.contains("unknown field"), "{msg}");
            assert!(msg.contains("unknown_slot"), "{msg}");
        }

        #[test]
        fn invalid_color_in_slot_names_the_slot() {
            let body = mocha_with_slot_replacement(r##"error = "#f38ba8""##, r#"error = "orange""#);

            let err = parse_theme(&body).unwrap_err();

            assert_eq!(
                format!("{err:#}"),
                format!(r#"slot "error": unknown color "orange": {COLOR_FORMATS_HINT}"#),
            );
        }

        #[test]
        fn invalid_inline_fg_color_names_the_slot() {
            let body = mocha_with_slot_replacement(
                r##"accent = { fg = "#89b4fa", bold = true }"##,
                r#"accent = { fg = "lavender", bold = true }"#,
            );

            let err = parse_theme(&body).unwrap_err();

            assert_eq!(
                format!("{err:#}"),
                format!(r#"slot "accent": fg: unknown color "lavender": {COLOR_FORMATS_HINT}"#),
            );
        }

        #[test]
        fn invalid_inline_bg_color_names_the_slot() {
            let body = mocha_with_slot_replacement(
                r##"diff_add = { bg = "#2a3a37" }"##,
                r#"diff_add = { bg = "magenta-ish" }"#,
            );

            let err = parse_theme(&body).unwrap_err();

            assert_eq!(
                format!("{err:#}"),
                format!(
                    r#"slot "diff_add": bg: unknown color "magenta-ish": {COLOR_FORMATS_HINT}"#
                ),
            );
        }

        #[test]
        fn bare_string_slot_yields_fg_only_no_modifiers() {
            let t = parse_theme(builtin::MOCHA).unwrap();
            // `user = "#fab387"` is a bare string in mocha.toml.
            assert_eq!(t.user.fg, Some(Color::Rgb(0xfa, 0xb3, 0x87)));
            assert_eq!(t.user.bg, None);
            assert!(t.user.modifiers.is_empty());
        }

        #[test]
        fn empty_inline_slot_uses_terminal_defaults() {
            let body = mocha_with_slot_replacement(
                r##"accent = { fg = "#89b4fa", bold = true }"##,
                "accent = {}",
            );

            let theme = parse_theme(&body).unwrap();

            assert_eq!(theme.accent, Slot::default());
        }

        #[test]
        fn inline_table_slot_carries_modifiers() {
            let t = parse_theme(builtin::MOCHA).unwrap();
            // `accent = { fg = "#89b4fa", bold = true }` is inline.
            assert_eq!(t.accent.fg, Some(Color::Rgb(0x89, 0xb4, 0xfa)));
            assert_eq!(t.accent.bg, None);
            assert!(t.accent.modifiers.contains(Modifier::BOLD));
        }

        #[test]
        fn bg_only_inline_slot_leaves_fg_unset() {
            let t = parse_theme(builtin::MOCHA).unwrap();
            // `diff_add = { bg = "#2a3a37" }` is bg-only.
            assert_eq!(t.diff_add.fg, None);
            assert_eq!(t.diff_add.bg, Some(Color::Rgb(0x2a, 0x3a, 0x37)));
            assert!(t.diff_add.modifiers.is_empty());
        }

        #[test]
        fn inline_supports_every_modifier_flag() {
            let replacement = indoc! {r##"
                thinking = { fg = "#585b70", bold = true, italic = true, underlined = true, dim = true, reversed = true }
            "##}
            .trim();
            let body = mocha_with_slot_replacement(
                r##"thinking = { fg = "#585b70", italic = true }"##,
                replacement,
            );
            let t = parse_theme(&body).unwrap();
            assert!(t.thinking.modifiers.contains(Modifier::BOLD));
            assert!(t.thinking.modifiers.contains(Modifier::ITALIC));
            assert!(t.thinking.modifiers.contains(Modifier::UNDERLINED));
            assert!(t.thinking.modifiers.contains(Modifier::DIM));
            assert!(t.thinking.modifiers.contains(Modifier::REVERSED));
        }

        #[test]
        fn inline_rejects_unknown_modifier_key() {
            // `sparkle` is not a modifier, so `deny_unknown_fields` rejects it.
            let body = mocha_with_slot_replacement(
                r##"accent = { fg = "#89b4fa", bold = true }"##,
                r##"accent = { fg = "#89b4fa", sparkle = true }"##,
            );
            let err = parse_theme(&body).expect_err("unknown modifier rejected");
            let msg = format!("{err:#}");
            // The untagged enum reports a generic "did not match any variant" error, but the
            // rendered TOML context still quotes the offending line.
            assert!(msg.contains("sparkle"), "{msg}");
        }

        #[test]
        fn builtins_pick_syntax_theme_for_their_background() {
            for (name, expected) in [
                ("mocha", "base16-ocean.dark"),
                ("macchiato", "base16-ocean.dark"),
                ("frappe", "base16-ocean.dark"),
                ("latte", "InspiredGitHub"),
                ("material", "base16-ocean.dark"),
            ] {
                let body = builtin::lookup(name).unwrap();

                let t = parse_theme(body).unwrap_or_else(|e| panic!("{name} parse: {e:#}"));

                assert_eq!(
                    t.syntax.name, SYNTAX_THEMES.themes[expected].name,
                    "{name} should use {expected}",
                );
            }
        }

        #[test]
        fn without_syntax_theme_defaults_to_base16_ocean_dark() {
            let body = mocha_with_slot_replacement(r#"syntax_theme = "base16-ocean.dark""#, "");
            let t = parse_theme(&body).unwrap();
            assert_eq!(t.syntax.name.as_deref(), Some("Base16 Ocean Dark"));
        }

        #[test]
        fn unknown_syntax_theme_lists_bundled_names() {
            let body = mocha_with_slot_replacement(
                r#"syntax_theme = "base16-ocean.dark""#,
                r#"syntax_theme = "dracula""#,
            );
            let err = parse_theme(&body).expect_err("unknown syntax theme rejected");
            let msg = format!("{err:#}");
            assert!(msg.contains("dracula"), "{msg}");
            assert!(msg.contains("InspiredGitHub"), "{msg}");
        }
    }
}
