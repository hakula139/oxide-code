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
//! All 31 slots must be present; `deny_unknown_fields` catches typos.
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

use anyhow::{Context, Result};
use ratatui::style::Modifier;
use serde::Deserialize;
use tracing::warn;

use super::Slot;
use super::Theme;
use super::builtin;
use super::color::parse_color;

/// Resolve a theme from an optional base + per-slot overrides.
///
/// Errors:
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

/// Mutable slot lookup by name. The mapping mirrors [`Theme`]'s
/// fields exactly; an unknown name returns `None`.
fn slot_for_name<'a>(theme: &'a mut Theme, name: &str) -> Option<&'a mut Slot> {
    Some(match name {
        "text" => &mut theme.text,
        "muted" => &mut theme.muted,
        "dim" => &mut theme.dim,
        "surface" => &mut theme.surface,
        "accent" => &mut theme.accent,
        "user" => &mut theme.user,
        "secondary" => &mut theme.secondary,
        "code" => &mut theme.code,
        "code_bg" => &mut theme.code_bg,
        "inline_code" => &mut theme.inline_code,
        "code_block_fallback" => &mut theme.code_block_fallback,
        "diff_add_bg" => &mut theme.diff_add_bg,
        "diff_del_bg" => &mut theme.diff_del_bg,
        "info" => &mut theme.info,
        "success" => &mut theme.success,
        "warning" => &mut theme.warning,
        "error" => &mut theme.error,
        "heading_h1" => &mut theme.heading_h1,
        "heading_h2" => &mut theme.heading_h2,
        "heading_h3" => &mut theme.heading_h3,
        "heading_minor" => &mut theme.heading_minor,
        "thinking" => &mut theme.thinking,
        "link" => &mut theme.link,
        "blockquote" => &mut theme.blockquote,
        "list_marker" => &mut theme.list_marker,
        "horizontal_rule" => &mut theme.horizontal_rule,
        "table_header" => &mut theme.table_header,
        "table_border" => &mut theme.table_border,
        "tool_border" => &mut theme.tool_border,
        "tool_icon" => &mut theme.tool_icon,
        "border_focused" => &mut theme.border_focused,
        "border_unfocused" => &mut theme.border_unfocused,
        "separator" => &mut theme.separator,
        _ => return None,
    })
}

/// Per-slot override from `[tui.theme.overrides]` in user config.
///
/// Patches are *additive* on top of the base slot:
/// - bare-string `error = "#hex"` overwrites the slot's `fg` only;
///   `bg` and modifiers are preserved from the base.
/// - inline-table overrides patch only the fields that appear, so
///   `accent = { bold = false }` removes bold from the base accent
///   without touching its `fg`.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub(crate) enum SlotPatch {
    Bare(String),
    Inline(InlinePatch),
}

/// Inline TOML patch — every field optional. `Option<bool>` modifier
/// flags distinguish "no change" (`None`), "set" (`Some(true)`), and
/// "clear" (`Some(false)`).
#[derive(Debug, Deserialize, Clone, Default)]
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
    fn apply(&self, base: Slot) -> Result<Slot> {
        let fg = match &self.fg {
            Some(s) => Some(parse_color(s)?),
            None => base.fg,
        };
        let bg = match &self.bg {
            Some(s) => Some(parse_color(s)?),
            None => base.bg,
        };
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    text: SlotDef,
    muted: SlotDef,
    dim: SlotDef,
    surface: SlotDef,
    accent: SlotDef,
    user: SlotDef,
    secondary: SlotDef,
    code: SlotDef,
    code_bg: SlotDef,
    inline_code: SlotDef,
    code_block_fallback: SlotDef,
    diff_add_bg: SlotDef,
    diff_del_bg: SlotDef,
    info: SlotDef,
    success: SlotDef,
    warning: SlotDef,
    error: SlotDef,
    heading_h1: SlotDef,
    heading_h2: SlotDef,
    heading_h3: SlotDef,
    heading_minor: SlotDef,
    thinking: SlotDef,
    link: SlotDef,
    blockquote: SlotDef,
    list_marker: SlotDef,
    horizontal_rule: SlotDef,
    table_header: SlotDef,
    table_border: SlotDef,
    tool_border: SlotDef,
    tool_icon: SlotDef,
    border_focused: SlotDef,
    border_unfocused: SlotDef,
    separator: SlotDef,
}

impl ThemeFile {
    fn into_theme(self) -> Result<Theme> {
        let parse = |def: SlotDef, name: &'static str| -> Result<Slot> {
            def.into_slot().with_context(|| format!("slot {name:?}"))
        };
        Ok(Theme {
            text: parse(self.text, "text")?,
            muted: parse(self.muted, "muted")?,
            dim: parse(self.dim, "dim")?,
            surface: parse(self.surface, "surface")?,
            accent: parse(self.accent, "accent")?,
            user: parse(self.user, "user")?,
            secondary: parse(self.secondary, "secondary")?,
            code: parse(self.code, "code")?,
            code_bg: parse(self.code_bg, "code_bg")?,
            inline_code: parse(self.inline_code, "inline_code")?,
            code_block_fallback: parse(self.code_block_fallback, "code_block_fallback")?,
            diff_add_bg: parse(self.diff_add_bg, "diff_add_bg")?,
            diff_del_bg: parse(self.diff_del_bg, "diff_del_bg")?,
            info: parse(self.info, "info")?,
            success: parse(self.success, "success")?,
            warning: parse(self.warning, "warning")?,
            error: parse(self.error, "error")?,
            heading_h1: parse(self.heading_h1, "heading_h1")?,
            heading_h2: parse(self.heading_h2, "heading_h2")?,
            heading_h3: parse(self.heading_h3, "heading_h3")?,
            heading_minor: parse(self.heading_minor, "heading_minor")?,
            thinking: parse(self.thinking, "thinking")?,
            link: parse(self.link, "link")?,
            blockquote: parse(self.blockquote, "blockquote")?,
            list_marker: parse(self.list_marker, "list_marker")?,
            horizontal_rule: parse(self.horizontal_rule, "horizontal_rule")?,
            table_header: parse(self.table_header, "table_header")?,
            table_border: parse(self.table_border, "table_border")?,
            tool_border: parse(self.tool_border, "tool_border")?,
            tool_icon: parse(self.tool_icon, "tool_icon")?,
            border_focused: parse(self.border_focused, "border_focused")?,
            border_unfocused: parse(self.border_unfocused, "border_unfocused")?,
            separator: parse(self.separator, "separator")?,
        })
    }
}

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
        let fg = self.fg.as_deref().map(parse_color).transpose()?;
        let bg = self.bg.as_deref().map(parse_color).transpose()?;
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
    use indoc::indoc;
    use ratatui::style::Color;

    use super::super::builtin;
    use super::*;

    // ── parse_theme: built-ins ──

    /// All four vendored Catppuccin TOMLs must parse without error.
    #[test]
    fn parse_theme_all_builtins() {
        for (name, body) in builtin::BUILT_IN {
            parse_theme(body).unwrap_or_else(|e| panic!("built-in {name:?} failed: {e:#}"));
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
            t.secondary.fg,
            Some(Color::Rgb(0xb4, 0xbe, 0xfe)),
            "Lavender",
        );
        assert_eq!(t.success.fg, Some(Color::Rgb(0xa6, 0xe3, 0xa1)), "Green");
        assert_eq!(t.warning.fg, Some(Color::Rgb(0xf9, 0xe2, 0xaf)), "Yellow");
        assert_eq!(t.error.fg, Some(Color::Rgb(0xf3, 0x8b, 0xa8)), "Red");
        assert_eq!(
            t.diff_add_bg.bg,
            Some(Color::Rgb(0x2a, 0x3a, 0x37)),
            "delta plus",
        );
        assert_eq!(
            t.diff_del_bg.bg,
            Some(Color::Rgb(0x38, 0x2c, 0x34)),
            "delta minus",
        );
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
        let body = builtin::MOCHA.replace("text = \"#cdd6f4\"", "");
        let err = parse_theme(&body).expect_err("missing required slot");
        let msg = format!("{err:#}");
        assert!(msg.contains("missing field"), "{msg}");
        assert!(msg.contains("text"), "{msg}");
    }

    #[test]
    fn parse_theme_unknown_slot_key() {
        let body = format!("{}\nunknown_slot = \"#000000\"\n", builtin::MOCHA);
        let err = parse_theme(&body).expect_err("unknown slot rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown field"), "{msg}");
        assert!(msg.contains("unknown_slot"), "{msg}");
    }

    #[test]
    fn parse_theme_invalid_color_in_slot_names_the_slot() {
        let body = mocha_with_slot_replacement("error = \"#f38ba8\"", "error = \"orange\"");
        let err = parse_theme(&body).expect_err("bad color rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("orange"), "names the value: {msg}");
        assert!(msg.contains("error"), "names the slot: {msg}");
    }

    // ── SlotDef forms ──

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
        // `diff_add_bg = { bg = "#2a3a37" }` is bg-only.
        assert_eq!(t.diff_add_bg.fg, None);
        assert_eq!(t.diff_add_bg.bg, Some(Color::Rgb(0x2a, 0x3a, 0x37)));
        assert!(t.diff_add_bg.modifiers.is_empty());
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
            "thinking = { fg = \"#585b70\", italic = true }",
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
            "accent = { fg = \"#89b4fa\", bold = true }",
            "accent = { fg = \"#89b4fa\", sparkle = true }",
        );
        let err = parse_theme(&body).expect_err("unknown modifier rejected");
        let msg = format!("{err:#}");
        // serde's `untagged` enum reports a generic "did not match
        // any variant" rather than the inner `unknown field`
        // diagnostic, but the offending line (with `sparkle`) is
        // included in the rendered TOML error context.
        assert!(msg.contains("sparkle"), "names the offending key: {msg}");
    }

    /// Replace one `accent` / `thinking` / etc. line in the embedded
    /// mocha body. Use to craft minimal-diff test fixtures without
    /// duplicating the full 31-slot file.
    fn mocha_with_slot_replacement(from: &str, to: &str) -> String {
        let body = builtin::MOCHA.replace(from, to);
        assert_ne!(body, builtin::MOCHA, "fixture marker {from:?} not found");
        body
    }

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
        assert!(msg.contains("solarized"), "names the input: {msg}");
        assert!(
            msg.contains("not a built-in name") || msg.contains("failed to read"),
            "explains the failure: {msg}",
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
        let body = builtin::MOCHA.replace("error = \"#f38ba8\"", "error = \"#ff0000\"");
        std::fs::write(&path, body).unwrap();

        let t = resolve_theme(Some(&path.to_string_lossy()), &HashMap::new()).unwrap();
        assert_eq!(t.error.fg, Some(Color::Rgb(0xff, 0x00, 0x00)));
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

    #[test]
    fn resolve_theme_invalid_color_in_override_warns_and_keeps_base() {
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

    #[test]
    fn resolve_theme_multiple_overrides_apply_independently() {
        let mut overrides = HashMap::new();
        overrides.insert("error".to_owned(), SlotPatch::Bare("#ff0000".to_owned()));
        overrides.insert("success".to_owned(), SlotPatch::Bare("#00ff00".to_owned()));
        let t = resolve_theme(None, &overrides).unwrap();
        assert_eq!(t.error.fg, Some(Color::Rgb(0xff, 0x00, 0x00)));
        assert_eq!(t.success.fg, Some(Color::Rgb(0x00, 0xff, 0x00)));
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
}
