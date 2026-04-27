//! Theme TOML parsing and resolution.
//!
//! A theme document is a flat TOML body with one entry per slot.
//! Each slot value is either:
//!
//! - a **bare color string** (`text = "#cdd6f4"`) — interpreted as
//!   the slot's `fg` with no `bg` and no modifiers; or
//! - an **inline table** (`accent = { fg = "#89b4fa", bold = true }`)
//!   — explicit `fg` / `bg` / modifier flags. Recognized modifier
//!   keys: `bold`, `italic`, `underlined`, `dim`, `reversed`.
//!
//! Every slot must be present; `deny_unknown_fields` catches typos.
//! Per-slot color parse errors are wrapped with the slot name so a
//! bad value in `theme.toml` points at the offending entry.
//!
//! [`resolve_theme`] applies a base + per-slot overrides from
//! `[tui.theme]` config to produce a final [`Theme`]. Theme-selection
//! errors (unknown name, missing file) hard-fail; per-slot value
//! errors warn and fall back to the base value so the TUI still
//! launches.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use ratatui::style::Modifier;
use serde::Deserialize;
use tracing::warn;

use super::{Slot, Theme, builtin, color::parse_color};

/// Resolve a theme from an optional base + per-slot overrides.
///
/// Errors:
///
/// - Unknown built-in name with no matching file path → `Err`.
/// - File path that fails to read or parse → `Err`.
/// - Per-slot override with bad color or unknown slot name → warn
///   to stderr (via `tracing`), keep the base slot's value.
pub(crate) fn resolve_theme(
    base: Option<&str>,
    overrides: &HashMap<String, SlotPatch>,
) -> Result<Theme> {
    let base_name = base.unwrap_or("mocha");
    let body = load_base_body(base_name)?;
    let mut theme =
        parse_theme(&body).with_context(|| format!("parsing base theme {base_name:?}"))?;

    for (slot_name, patch) in overrides {
        if let Err(e) = patch_slot(&mut theme, slot_name, patch) {
            warn!(
                slot = slot_name.as_str(),
                error = format!("{e:#}"),
                "ignoring theme override; falling back to base value",
            );
        }
    }
    Ok(theme)
}

/// Resolve a `base` value to a TOML body. Tries the built-in catalogue
/// first, then a filesystem path (with `~/` expanded to `$HOME`).
fn load_base_body(name: &str) -> Result<String> {
    if let Some(body) = builtin::lookup(name) {
        return Ok(body.to_owned());
    }
    let path = expand_tilde(name);
    std::fs::read_to_string(&path).with_context(|| {
        format!(
            "theme {name:?}: not a built-in name and failed to read as file {}",
            path.display(),
        )
    })
}

/// Expand a leading `~/` to `$HOME` for theme file paths. Other path
/// prefixes are returned as-is.
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(s)
}

/// Apply one override patch to the named slot. Returns `Err` for
/// an unknown slot name or a bad color value in the patch.
fn patch_slot(theme: &mut Theme, slot_name: &str, patch: &SlotPatch) -> Result<()> {
    let slot = slot_for_name(theme, slot_name)
        .ok_or_else(|| anyhow::anyhow!("unknown slot {slot_name:?}"))?;
    *slot = patch.apply(*slot)?;
    Ok(())
}

/// Mutable slot lookup by name. Generated so the mapping can't drift
/// from [`Theme`]'s fields.
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

/// Per-slot override from `[tui.theme.overrides]` in user config.
///
/// Patches are *additive* on top of the base slot:
///
/// - bare-string `error = "#hex"` overwrites the slot's `fg` only;
///   `bg` and modifiers are preserved from the base.
/// - inline-table overrides patch only the fields that appear, so
///   `accent = { bold = false }` removes bold from the base accent
///   without touching its `fg`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum SlotPatch {
    Bare(String),
    Inline(InlinePatch),
}

/// Inline TOML patch — every field optional. `Option<bool>` modifier
/// flags distinguish "no change" (`None`), "set" (`Some(true)`), and
/// "clear" (`Some(false)`).
///
/// An entirely empty patch (`error = {}`) would silently re-write the
/// base value with itself — almost certainly a config bug. [`apply`]
/// rejects it so the slot warns and falls back to the base value
/// instead. Per-slot warnings are the right severity for a per-slot
/// config mistake; a `SlotPatch`-level custom `Deserialize` would
/// catch it at parse time, but serde's untagged enum dispatcher
/// swallows inner messages, so the warn path produces a clearer
/// diagnostic.
///
/// [`apply`]: Self::apply
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InlinePatch {
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
        if self.is_empty() {
            bail!("empty patch (no fg, bg, or modifier flags)");
        }

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

/// Parse a theme TOML document into a [`Theme`].
pub(super) fn parse_theme(content: &str) -> Result<Theme> {
    let file: ThemeFile = toml::from_str(content).context("invalid theme TOML")?;
    file.into_theme()
}

/// `ThemeFile` deserialization shape + `into_theme` converter.
/// `deny_unknown_fields` catches typos; `into_theme` wraps each
/// slot's parse error with its name.
macro_rules! define_theme_file {
    ( $( ($name:ident, $doc:literal), )* ) => {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ThemeFile {
            $( $name: SlotDef, )*
        }

        impl ThemeFile {
            fn into_theme(self) -> Result<Theme> {
                let parse = |def: SlotDef, name: &'static str| -> Result<Slot> {
                    def.into_slot().with_context(|| format!("slot {name:?}"))
                };
                Ok(Theme {
                    $( $name: parse(self.$name, stringify!($name))?, )*
                })
            }
        }
    };
}

super::for_each_slot!(define_theme_file);

/// One slot's TOML representation. The `untagged` enum lets serde
/// accept either form transparently — a bare string or an inline
/// table.
#[derive(Deserialize)]
#[serde(untagged)]
enum SlotDef {
    Bare(String),
    Inline(InlineSlot),
}

/// Inline TOML form of a slot — flat struct of `fg` / `bg` / one
/// boolean per recognized text modifier.
#[expect(
    clippy::struct_excessive_bools,
    reason = "modifiers are independent flags by design (matches ratatui::style::Modifier)"
)]
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct InlineSlot {
    fg: Option<String>,
    bg: Option<String>,
    #[serde(default)]
    bold: bool,
    #[serde(default)]
    italic: bool,
    #[serde(default)]
    underlined: bool,
    #[serde(default)]
    dim: bool,
    #[serde(default)]
    reversed: bool,
}

impl SlotDef {
    fn into_slot(self) -> Result<Slot> {
        match self {
            Self::Bare(s) => Ok(Slot {
                fg: Some(parse_color(&s)?),
                bg: None,
                modifiers: Modifier::empty(),
            }),
            Self::Inline(i) => i.into_slot(),
        }
    }
}

impl InlineSlot {
    fn into_slot(self) -> Result<Slot> {
        let fg = self
            .fg
            .as_deref()
            .map(parse_color)
            .transpose()
            .context("fg")?;
        let bg = self
            .bg
            .as_deref()
            .map(parse_color)
            .transpose()
            .context("bg")?;
        let mut modifiers = Modifier::empty();
        if self.bold {
            modifiers |= Modifier::BOLD;
        }
        if self.italic {
            modifiers |= Modifier::ITALIC;
        }
        if self.underlined {
            modifiers |= Modifier::UNDERLINED;
        }
        if self.dim {
            modifiers |= Modifier::DIM;
        }
        if self.reversed {
            modifiers |= Modifier::REVERSED;
        }
        Ok(Slot { fg, bg, modifiers })
    }
}

#[cfg(test)]
mod tests {
    use indoc::{formatdoc, indoc};
    use ratatui::style::Color;

    use super::super::builtin;
    use super::*;

    // ── resolve_theme: base resolution ──

    #[test]
    fn resolve_theme_no_args_returns_default_mocha() {
        let t = resolve_theme(None, &HashMap::new()).unwrap();
        assert_eq!(t.text.fg, Some(Color::Rgb(0xcd, 0xd6, 0xf4)));
        assert_eq!(t.error.fg, Some(Color::Rgb(0xf3, 0x8b, 0xa8)));
    }

    #[test]
    fn resolve_theme_named_builtin_loads_that_palette() {
        let t = resolve_theme(Some("latte"), &HashMap::new()).unwrap();
        // Latte's text is dark (#4c4f69), unlike Mocha's light text.
        assert_eq!(t.text.fg, Some(Color::Rgb(0x4c, 0x4f, 0x69)));
    }

    #[test]
    fn resolve_theme_unknown_name_with_no_matching_file_errors() {
        let err = resolve_theme(Some("solarized"), &HashMap::new())
            .expect_err("unknown built-in and not a path");
        let msg = format!("{err:#}");
        assert!(msg.contains("solarized"), "{msg}");
        assert!(
            msg.contains("not a built-in name") || msg.contains("failed to read"),
            "{msg}",
        );
    }

    #[test]
    fn resolve_theme_loads_from_file_path() {
        // Write a minimal theme file (modify mocha) and resolve via
        // its absolute path. Confirms the file-path branch works
        // end-to-end and that the override pathway can hand a file
        // through.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.toml");
        let body = builtin::MOCHA.replace(r##"error = "#f38ba8""##, r##"error = "#ff0000""##);
        std::fs::write(&path, body).unwrap();

        let t = resolve_theme(Some(&path.to_string_lossy()), &HashMap::new()).unwrap();
        assert_eq!(t.error.fg, Some(Color::Rgb(0xff, 0x00, 0x00)));
    }

    /// File loaded successfully but its body fails to parse — the
    /// error must be wrapped with the base name so the user sees
    /// which theme is broken (not just an opaque slot diagnostic).
    #[test]
    fn resolve_theme_file_path_with_bad_body_wraps_with_base_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.toml");
        let body = builtin::MOCHA.replace(r##"error = "#f38ba8""##, r#"error = "orange""#);
        std::fs::write(&path, body).unwrap();
        let path_str = path.to_string_lossy().into_owned();

        let err =
            resolve_theme(Some(&path_str), &HashMap::new()).expect_err("bad slot color must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("parsing base theme"), "{msg}");
        assert!(msg.contains(&path_str), "{msg}");
        assert!(msg.contains("orange"), "{msg}");
    }

    // ── resolve_theme: per-slot overrides ──

    #[test]
    fn resolve_theme_bare_string_override_patches_only_fg() {
        // accent in mocha is bold blue. A bare-string override should
        // replace fg only, leaving bold intact.
        let mut overrides = HashMap::new();
        overrides.insert("accent".to_owned(), SlotPatch::Bare("#ff0000".to_owned()));
        let t = resolve_theme(None, &overrides).unwrap();
        assert_eq!(t.accent.fg, Some(Color::Rgb(0xff, 0x00, 0x00)));
        assert_eq!(t.accent.bg, None);
        assert!(
            t.accent.modifiers.contains(Modifier::BOLD),
            "bold from base must survive a bare-string override",
        );
    }

    #[test]
    fn resolve_theme_inline_override_clears_modifier_with_false() {
        // Explicit `bold = false` removes BOLD from the base slot.
        let mut overrides = HashMap::new();
        overrides.insert(
            "accent".to_owned(),
            SlotPatch::Inline(InlinePatch {
                bold: Some(false),
                ..InlinePatch::default()
            }),
        );
        let t = resolve_theme(None, &overrides).unwrap();
        // fg from base stays.
        assert_eq!(t.accent.fg, Some(Color::Rgb(0x89, 0xb4, 0xfa)));
        assert!(!t.accent.modifiers.contains(Modifier::BOLD));
    }

    #[test]
    fn resolve_theme_inline_override_can_add_a_modifier() {
        // success in mocha has no modifiers; add ITALIC via override.
        let mut overrides = HashMap::new();
        overrides.insert(
            "success".to_owned(),
            SlotPatch::Inline(InlinePatch {
                italic: Some(true),
                ..InlinePatch::default()
            }),
        );
        let t = resolve_theme(None, &overrides).unwrap();
        assert!(t.success.modifiers.contains(Modifier::ITALIC));
        // fg from base unchanged.
        assert_eq!(t.success.fg, Some(Color::Rgb(0xa6, 0xe3, 0xa1)));
    }

    #[test]
    fn resolve_theme_unknown_slot_in_override_warns_and_resolves() {
        install_permissive_global_subscriber();
        // Unknown slot name in overrides must NOT fail the resolve;
        // it warns to stderr and the rest of the theme loads cleanly.
        let mut overrides = HashMap::new();
        overrides.insert(
            "purple_thing".to_owned(),
            SlotPatch::Bare("#ff0000".to_owned()),
        );
        let t = resolve_theme(None, &overrides).expect("unknown slot should warn, not error");
        // The base mocha values must come through unchanged.
        assert_eq!(t.error.fg, Some(Color::Rgb(0xf3, 0x8b, 0xa8)));
    }

    /// Verify the warn-and-fallback path actually emits a
    /// `tracing::warn!` event naming the offending slot. Without this
    /// assertion, a regression to `if let Err(_) = … {}` would
    /// silently restore the silent-failure pattern the contract was
    /// designed to prevent — and every other warn-path test would
    /// still pass because they only check that resolution succeeds.
    #[test]
    fn resolve_theme_unknown_slot_emits_tracing_warn_with_slot_name() {
        use std::io;
        use std::sync::{Arc, Mutex};

        use tracing_subscriber::fmt::{self, MakeWriter};

        #[derive(Clone)]
        struct Capture(Arc<Mutex<Vec<u8>>>);

        impl io::Write for Capture {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl<'a> MakeWriter<'a> for Capture {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        install_permissive_global_subscriber();

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let subscriber = fmt::Subscriber::builder()
            .with_writer(Capture(Arc::clone(&buf)))
            .with_max_level(tracing::Level::WARN)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let mut overrides = HashMap::new();
            overrides.insert(
                "purple_thing".to_owned(),
                SlotPatch::Bare("#ff0000".to_owned()),
            );
            resolve_theme(None, &overrides).expect("warn-and-fallback shouldn't error");
        });

        let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            captured.contains("purple_thing"),
            "warn output must name the slot: {captured:?}",
        );
        assert!(
            captured.to_ascii_uppercase().contains("WARN"),
            "warn level must reach the writer: {captured:?}",
        );
    }

    #[test]
    fn resolve_theme_invalid_color_in_override_warns_and_keeps_base() {
        install_permissive_global_subscriber();
        // Bad color string in an override must NOT fail the resolve;
        // the slot's base value must be preserved.
        let mut overrides = HashMap::new();
        overrides.insert(
            "error".to_owned(),
            SlotPatch::Bare("not-a-color".to_owned()),
        );
        let t = resolve_theme(None, &overrides).expect("bad color should warn, not error");
        // error stays at the mocha base.
        assert_eq!(t.error.fg, Some(Color::Rgb(0xf3, 0x8b, 0xa8)));
    }

    /// Inline-form override with a bad `fg` color exercises the
    /// `InlinePatch::apply` fg parse path that the bare-string sibling
    /// can't reach.
    #[test]
    fn resolve_theme_inline_override_with_bad_fg_warns_and_keeps_base() {
        install_permissive_global_subscriber();
        let mut overrides = HashMap::new();
        overrides.insert(
            "accent".to_owned(),
            SlotPatch::Inline(InlinePatch {
                fg: Some("not-a-color".to_owned()),
                ..InlinePatch::default()
            }),
        );
        let t = resolve_theme(None, &overrides).expect("bad inline fg should warn, not error");
        assert_eq!(t.accent.fg, Some(Color::Rgb(0x89, 0xb4, 0xfa)));
        assert!(
            t.accent.modifiers.contains(Modifier::BOLD),
            "base modifiers preserved on warn-fallback",
        );
    }

    /// Sibling to the inline-fg test for the `bg` parse path.
    #[test]
    fn resolve_theme_inline_override_with_bad_bg_warns_and_keeps_base() {
        install_permissive_global_subscriber();
        let mut overrides = HashMap::new();
        overrides.insert(
            "diff_add".to_owned(),
            SlotPatch::Inline(InlinePatch {
                bg: Some("not-a-color".to_owned()),
                ..InlinePatch::default()
            }),
        );
        let t = resolve_theme(None, &overrides).expect("bad inline bg should warn, not error");
        assert_eq!(t.diff_add.bg, Some(Color::Rgb(0x2a, 0x3a, 0x37)));
    }

    #[test]
    fn resolve_theme_multiple_overrides_apply_independently() {
        let mut overrides = HashMap::new();
        overrides.insert("error".to_owned(), SlotPatch::Bare("#ff0000".to_owned()));
        overrides.insert("success".to_owned(), SlotPatch::Bare("#00ff00".to_owned()));
        let t = resolve_theme(None, &overrides).unwrap();
        assert_eq!(t.error.fg, Some(Color::Rgb(0xff, 0x00, 0x00)));
        assert_eq!(t.success.fg, Some(Color::Rgb(0x00, 0xff, 0x00)));
    }

    /// Install a permissive global tracing subscriber so the warn
    /// callsite in `resolve_theme` registers as `Interest::Always`
    /// regardless of which warn-firing test fires it first. Without
    /// this, parallel tests racing the default noop subscriber lock
    /// the per-callsite Interest cache to `Never`, after which any
    /// per-test `with_default` capture sees nothing.
    fn install_permissive_global_subscriber() {
        use std::sync::OnceLock;

        use tracing_subscriber::fmt;

        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            _ = tracing::subscriber::set_global_default(
                fmt::Subscriber::builder()
                    .with_writer(std::io::sink)
                    .with_max_level(tracing::Level::WARN)
                    .finish(),
            );
        });
    }

    // ── expand_tilde ──

    #[test]
    fn expand_tilde_rewrites_leading_tilde_to_home() {
        // Force a stable HOME so the assertion is deterministic.
        temp_env::with_var("HOME", Some("/tmp/oxide-fake-home"), || {
            let path = expand_tilde("~/themes/dark.toml");
            assert_eq!(path, PathBuf::from("/tmp/oxide-fake-home/themes/dark.toml"),);
        });
    }

    #[test]
    fn expand_tilde_passes_non_tilde_paths_through_unchanged() {
        let path = expand_tilde("/abs/themes/dark.toml");
        assert_eq!(path, PathBuf::from("/abs/themes/dark.toml"));
    }

    // ── slot_for_name ──

    /// Every slot name must route to a unique slot. Catches the
    /// "typo in match arm" class of bug — e.g.,
    /// `"tool_icon" => &mut theme.tool_border` would compile and
    /// pass a happy-path override test, but this assertion fails
    /// because patching `tool_icon` would visibly alter `tool_border`.
    #[test]
    fn slot_for_name_routes_each_name_to_a_unique_slot() {
        let sentinel = Slot {
            fg: Some(Color::Rgb(0xde, 0xad, 0xbe)),
            bg: None,
            modifiers: Modifier::empty(),
        };
        for &target in super::super::SLOT_NAMES {
            let mut patched = Theme::default();
            let original = patched.clone();
            *slot_for_name(&mut patched, target)
                .unwrap_or_else(|| panic!("unknown slot {target:?}")) = sentinel;
            for &other in super::super::SLOT_NAMES {
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

    // ── SlotPatch::apply ──

    #[test]
    fn slot_patch_inline_with_bg_only_keeps_base_fg_and_modifiers() {
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
        assert_eq!(out.fg, Some(Color::Red), "fg from base");
        assert_eq!(out.bg, Some(Color::Rgb(0, 0, 0)), "bg from patch");
        assert!(out.modifiers.contains(Modifier::BOLD), "modifier from base");
    }

    #[test]
    fn slot_patch_inline_none_modifiers_preserve_every_base_flag() {
        // Locks down the three-state contract: a patch with no
        // modifier fields (all `None`) must leave every base modifier
        // untouched. Catches the regression where the loop is
        // refactored to `flag.unwrap_or(false)` and silently clears
        // every base modifier whenever an unrelated patch field is
        // set.
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
            out.modifiers, base.modifiers,
            "every base modifier survives"
        );
    }

    #[test]
    fn slot_patch_inline_with_fg_overwrites_base_fg() {
        // Sibling to the bg-only test: an inline patch carrying `fg`
        // replaces the base fg while preserving bg and modifiers.
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
        assert_eq!(out.fg, Some(Color::Rgb(0xab, 0xcd, 0xef)), "fg from patch");
        assert_eq!(
            out.bg,
            Some(Color::Rgb(0x10, 0x10, 0x10)),
            "bg from base survives",
        );
        assert!(
            out.modifiers.contains(Modifier::ITALIC),
            "modifier from base survives",
        );
    }

    #[test]
    fn inline_patch_empty_table_apply_errors_with_actionable_message() {
        // `accent = {}` parses fine — serde's untagged enum dispatcher
        // would swallow a `Deserialize`-time message — but `apply`
        // refuses to re-write the base with itself. The error reaches
        // `resolve_theme`, which warns and falls back to base.
        let base = Slot {
            fg: Some(Color::Rgb(0xab, 0xcd, 0xef)),
            bg: None,
            modifiers: Modifier::empty(),
        };
        let err = InlinePatch::default()
            .apply(base)
            .expect_err("empty patch must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("empty patch"), "{msg}");
    }

    // ── parse_theme: built-ins ──

    /// All four vendored Catppuccin TOMLs must parse without error.
    #[test]
    fn parse_theme_all_builtins() {
        for (name, body) in builtin::BUILT_IN {
            parse_theme(body).unwrap_or_else(|e| panic!("built-in {name:?} failed: {e:#}"));
        }
    }

    /// `surface` is the bg-only panel fill — a regression where any
    /// built-in declares it as a bare-string fg (which `SlotDef::Bare`
    /// would route to `fg`, leaving `bg = None`) would silently paint
    /// every panel's text in the surface color. Pin the contract.
    #[test]
    fn parse_theme_surface_is_bg_only_in_every_builtin() {
        for (name, body) in builtin::BUILT_IN {
            let t = parse_theme(body).unwrap_or_else(|e| panic!("{name} parse: {e:#}"));
            assert_eq!(
                t.surface.fg, None,
                "{name}: surface must be bg-only, never fg"
            );
            assert!(
                t.surface.bg.is_some(),
                "{name}: surface must declare a bg (use \"reset\" for terminal default)",
            );
        }
    }

    /// Pin a representative subset of Catppuccin Mocha's hex codes
    /// so an accidental palette edit in `mocha.toml` surfaces as a
    /// failing test rather than a silent visual regression.
    #[test]
    fn parse_theme_mocha_matches_known_palette() {
        let t = parse_theme(builtin::MOCHA).unwrap();
        assert_eq!(t.text.fg, Some(Color::Rgb(0xcd, 0xd6, 0xf4)), "Text");
        assert_eq!(t.muted.fg, Some(Color::Rgb(0x6c, 0x70, 0x86)), "Overlay0");
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

    /// Pin one distinctive hex per non-Mocha palette so a botched
    /// edit in those TOMLs (typo, dropped digit, copy-pasted from a
    /// sibling palette) surfaces as a failing test. `text` is the
    /// load-bearing slot — every variant assigns it a different shade.
    #[test]
    fn parse_theme_non_mocha_palettes_match_known_text_color() {
        for (name, body, expected) in [
            ("latte", builtin::LATTE, Color::Rgb(0x4c, 0x4f, 0x69)),
            ("frappe", builtin::FRAPPE, Color::Rgb(0xc6, 0xd0, 0xf5)),
            (
                "macchiato",
                builtin::MACCHIATO,
                Color::Rgb(0xca, 0xd3, 0xf5),
            ),
            ("material", builtin::MATERIAL, Color::Rgb(0xde, 0xde, 0xde)),
        ] {
            let t = parse_theme(body).unwrap_or_else(|e| panic!("{name} parse: {e:#}"));
            assert_eq!(t.text.fg, Some(expected), "{name} text.fg");
        }
    }

    /// Default modifier semantics: `accent` is bold, `thinking` is
    /// italic, `heading_h1` is bold + underlined, `link` is
    /// underlined. Pinned so a vendored TOML edit can't silently
    /// demote them.
    #[test]
    fn parse_theme_mocha_modifiers_match_default() {
        let t = parse_theme(builtin::MOCHA).unwrap();
        assert!(t.accent.modifiers.contains(Modifier::BOLD));
        assert!(t.thinking.modifiers.contains(Modifier::ITALIC));
        assert!(t.heading_h1.modifiers.contains(Modifier::BOLD));
        assert!(t.heading_h1.modifiers.contains(Modifier::UNDERLINED));
        assert!(t.link.modifiers.contains(Modifier::UNDERLINED));
        // Status colors carry no modifiers by default.
        assert!(t.success.modifiers.is_empty());
        assert!(t.error.modifiers.is_empty());
    }

    // ── parse_theme: error cases ──

    #[test]
    fn parse_theme_missing_required_slot() {
        // Drop `text` → serde reports "missing field `text`".
        let body = builtin::MOCHA.replace(r##"text = "#cdd6f4""##, "");
        let err = parse_theme(&body).expect_err("missing required slot");
        let msg = format!("{err:#}");
        assert!(msg.contains("missing field"), "{msg}");
        assert!(msg.contains("text"), "{msg}");
    }

    #[test]
    fn parse_theme_unknown_slot_key() {
        let body = formatdoc! {r##"
            {mocha}

            unknown_slot = "#000000"
        "##, mocha = builtin::MOCHA};
        let err = parse_theme(&body).expect_err("unknown slot rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown field"), "{msg}");
        assert!(msg.contains("unknown_slot"), "{msg}");
    }

    #[test]
    fn parse_theme_invalid_color_in_slot_names_the_slot() {
        let body = mocha_with_slot_replacement(r##"error = "#f38ba8""##, r#"error = "orange""#);
        let err = parse_theme(&body).expect_err("bad color rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("error"), "{msg}");
        assert!(msg.contains("orange"), "{msg}");
    }

    /// Sibling to the bare-string parse-error test, but the offending
    /// value is the `fg` field of an inline-table slot — exercises the
    /// `InlineSlot::into_slot` fg parse path.
    #[test]
    fn parse_theme_invalid_inline_fg_color_names_the_slot() {
        let body = mocha_with_slot_replacement(
            r##"accent = { fg = "#89b4fa", bold = true }"##,
            r#"accent = { fg = "lavender", bold = true }"#,
        );
        let err = parse_theme(&body).expect_err("bad inline fg rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("accent"), "{msg}");
        assert!(msg.contains("fg"), "{msg}");
        assert!(msg.contains("lavender"), "{msg}");
    }

    /// Inline `bg` parse error — exercises the `InlineSlot::into_slot`
    /// bg parse path that the bare-string and inline-fg tests miss.
    #[test]
    fn parse_theme_invalid_inline_bg_color_names_the_slot() {
        let body = mocha_with_slot_replacement(
            r##"diff_add = { bg = "#2a3a37" }"##,
            r#"diff_add = { bg = "magenta-ish" }"#,
        );
        let err = parse_theme(&body).expect_err("bad inline bg rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("diff_add"), "{msg}");
        assert!(msg.contains("bg"), "{msg}");
        assert!(msg.contains("magenta-ish"), "{msg}");
    }

    // ── parse_theme: SlotDef forms ──

    #[test]
    fn parse_theme_bare_string_slot_yields_fg_only_no_modifiers() {
        let t = parse_theme(builtin::MOCHA).unwrap();
        // `user = "#fab387"` is a bare string in mocha.toml.
        assert_eq!(t.user.fg, Some(Color::Rgb(0xfa, 0xb3, 0x87)));
        assert_eq!(t.user.bg, None);
        assert!(t.user.modifiers.is_empty());
    }

    #[test]
    fn parse_theme_inline_table_slot_carries_modifiers() {
        let t = parse_theme(builtin::MOCHA).unwrap();
        // `accent = { fg = "#89b4fa", bold = true }` is inline.
        assert_eq!(t.accent.fg, Some(Color::Rgb(0x89, 0xb4, 0xfa)));
        assert_eq!(t.accent.bg, None);
        assert!(t.accent.modifiers.contains(Modifier::BOLD));
    }

    #[test]
    fn parse_theme_bg_only_inline_slot_leaves_fg_unset() {
        let t = parse_theme(builtin::MOCHA).unwrap();
        // `diff_add = { bg = "#2a3a37" }` is bg-only.
        assert_eq!(t.diff_add.fg, None);
        assert_eq!(t.diff_add.bg, Some(Color::Rgb(0x2a, 0x3a, 0x37)));
        assert!(t.diff_add.modifiers.is_empty());
    }

    #[test]
    fn parse_theme_inline_supports_every_modifier_flag() {
        // Hand-craft a body that exercises every modifier flag and
        // verifies they all reach the resulting Slot.
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
    fn parse_theme_inline_rejects_unknown_modifier_key() {
        // `sparkle = true` is not a recognized modifier; the
        // section's `deny_unknown_fields` rejects it.
        let body = mocha_with_slot_replacement(
            r##"accent = { fg = "#89b4fa", bold = true }"##,
            r##"accent = { fg = "#89b4fa", sparkle = true }"##,
        );
        let err = parse_theme(&body).expect_err("unknown modifier rejected");
        let msg = format!("{err:#}");
        // serde's `untagged` enum reports a generic "did not match
        // any variant" rather than the inner `unknown field`
        // diagnostic, but the offending line (with `sparkle`) is
        // included in the rendered TOML error context.
        assert!(msg.contains("sparkle"), "{msg}");
    }

    /// Replace one `accent` / `thinking` / etc. line in the embedded
    /// mocha body. Use to craft minimal-diff test fixtures without
    /// duplicating the full 31-slot file.
    fn mocha_with_slot_replacement(from: &str, to: &str) -> String {
        let body = builtin::MOCHA.replace(from, to);
        assert_ne!(body, builtin::MOCHA, "fixture marker {from:?} not found");
        body
    }
}
