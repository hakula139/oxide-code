//! Color string parsing for theme TOML values. Case-insensitive.
//!
//! - **6-digit hex**: `"#rrggbb"` — true color (24-bit RGB).
//! - **ANSI 16 named**: `"red"`, `"bright_blue"`, `"dark_gray"`, ... (`light_X` aliases
//!   `bright_X`; `grey` aliases `gray`).
//! - **Indexed 256-color**: `"ansi:N"` where `N` is 0–255.
//! - **Terminal default**: `"reset"` — follows the user's terminal palette.
//!
//! Three-digit hex shorthand (`#fff`) is rejected to keep the format unambiguous.

use anyhow::{Context, Result, bail};
use ratatui::style::Color;

/// Parses a theme color string. Trims whitespace and surfaces actionable errors that name the
/// offending input and supported formats.
pub(super) fn parse_color(input: &str) -> Result<Color> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("empty color value");
    }

    // Lowercase once so `ANSI:5` and `#FFAABB` route to their parsers instead of the named lookup.
    let s = trimmed.to_ascii_lowercase();

    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex).with_context(|| format!("invalid hex color {input:?}"));
    }

    if let Some(idx) = s.strip_prefix("ansi:") {
        return parse_indexed(idx).with_context(|| format!("invalid indexed color {input:?}"));
    }

    parse_named(&s).with_context(|| format!("unknown color {input:?}"))
}

fn parse_hex(hex: &str) -> Result<Color> {
    if hex.len() != 6 {
        bail!("expected 6-digit hex (e.g., #cdd6f4)");
    }
    let n = u32::from_str_radix(hex, 16).context("non-hex characters in color")?;
    let (r, g, b) = (
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    );
    Ok(Color::Rgb(r, g, b))
}

fn parse_indexed(s: &str) -> Result<Color> {
    let n: u8 = s.parse().context("expected 0-255 (e.g., ansi:174)")?;
    Ok(Color::Indexed(n))
}

fn parse_named(s: &str) -> Result<Color> {
    Ok(match s {
        "reset" => Color::Reset,

        // Standard 8 (ANSI 0-7)
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,

        // Bright 8 (ANSI 8-15)
        "dark_gray" | "dark_grey" => Color::DarkGray,
        "bright_red" | "light_red" => Color::LightRed,
        "bright_green" | "light_green" => Color::LightGreen,
        "bright_yellow" | "light_yellow" => Color::LightYellow,
        "bright_blue" | "light_blue" => Color::LightBlue,
        "bright_magenta" | "light_magenta" => Color::LightMagenta,
        "bright_cyan" | "light_cyan" => Color::LightCyan,
        "white" | "bright_white" | "light_white" => Color::White,

        _ => bail!(
            "expected hex (#rrggbb), ANSI name (red, bright_blue, ...), \
             indexed (ansi:0..255), or reset"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_color: hex ──

    #[test]
    fn parse_color_hex_lowercase() {
        assert_eq!(
            parse_color("#cdd6f4").unwrap(),
            Color::Rgb(0xcd, 0xd6, 0xf4)
        );
    }

    #[test]
    fn parse_color_hex_uppercase() {
        assert_eq!(
            parse_color("#CDD6F4").unwrap(),
            Color::Rgb(0xcd, 0xd6, 0xf4)
        );
    }

    #[test]
    fn parse_color_hex_pure_red_green_blue() {
        assert_eq!(parse_color("#ff0000").unwrap(), Color::Rgb(255, 0, 0));
        assert_eq!(parse_color("#00ff00").unwrap(), Color::Rgb(0, 255, 0));
        assert_eq!(parse_color("#0000ff").unwrap(), Color::Rgb(0, 0, 255));
    }

    #[test]
    fn parse_color_hex_rejects_three_digit_shorthand() {
        let err = parse_color("#fff").expect_err("3-digit hex rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("#fff"), "{msg}");
        assert!(msg.contains("6-digit"), "{msg}");
    }

    #[test]
    fn parse_color_hex_rejects_non_hex_chars() {
        let err = parse_color("#zzzzzz").expect_err("invalid characters rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("#zzzzzz"), "{msg}");
    }

    #[test]
    fn parse_color_hex_rejects_missing_hash_prefix() {
        let err = parse_color("cdd6f4").expect_err("bare hex without # rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("cdd6f4"), "{msg}");
    }

    // ── parse_color: indexed ──

    #[test]
    fn parse_color_indexed_min_max() {
        assert_eq!(parse_color("ansi:0").unwrap(), Color::Indexed(0));
        assert_eq!(parse_color("ansi:255").unwrap(), Color::Indexed(255));
    }

    #[test]
    fn parse_color_indexed_mid_value() {
        assert_eq!(parse_color("ansi:174").unwrap(), Color::Indexed(174));
    }

    #[test]
    fn parse_color_indexed_rejects_out_of_range() {
        let err = parse_color("ansi:256").expect_err("256 is out of u8 range");
        let msg = format!("{err:#}");
        assert!(msg.contains("ansi:256"), "{msg}");
    }

    #[test]
    fn parse_color_indexed_rejects_non_numeric() {
        let err = parse_color("ansi:abc").expect_err("non-numeric rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("ansi:abc"), "{msg}");
    }

    // Module doc claims `ansi:N` is case-insensitive; pin it so a regression that drops the
    // lowercase pass and fails on `"ANSI:5"` shows up here.
    #[test]
    fn parse_color_indexed_prefix_is_case_insensitive() {
        assert_eq!(parse_color("ANSI:5").unwrap(), Color::Indexed(5));
        assert_eq!(parse_color("Ansi:174").unwrap(), Color::Indexed(174));
    }

    // ── parse_color: named ──

    #[test]
    fn parse_color_named_standard_8() {
        assert_eq!(parse_color("black").unwrap(), Color::Black);
        assert_eq!(parse_color("red").unwrap(), Color::Red);
        assert_eq!(parse_color("green").unwrap(), Color::Green);
        assert_eq!(parse_color("yellow").unwrap(), Color::Yellow);
        assert_eq!(parse_color("blue").unwrap(), Color::Blue);
        assert_eq!(parse_color("magenta").unwrap(), Color::Magenta);
        assert_eq!(parse_color("cyan").unwrap(), Color::Cyan);
        assert_eq!(parse_color("gray").unwrap(), Color::Gray);
    }

    #[test]
    fn parse_color_named_bright_8() {
        assert_eq!(parse_color("dark_gray").unwrap(), Color::DarkGray);
        assert_eq!(parse_color("bright_red").unwrap(), Color::LightRed);
        assert_eq!(parse_color("bright_green").unwrap(), Color::LightGreen);
        assert_eq!(parse_color("bright_yellow").unwrap(), Color::LightYellow);
        assert_eq!(parse_color("bright_blue").unwrap(), Color::LightBlue);
        assert_eq!(parse_color("bright_magenta").unwrap(), Color::LightMagenta);
        assert_eq!(parse_color("bright_cyan").unwrap(), Color::LightCyan);
        assert_eq!(parse_color("white").unwrap(), Color::White);
    }

    #[test]
    fn parse_color_named_light_alias_matches_bright() {
        assert_eq!(parse_color("light_red").unwrap(), Color::LightRed);
        assert_eq!(parse_color("light_blue").unwrap(), Color::LightBlue);
        assert_eq!(parse_color("light_white").unwrap(), Color::White);
    }

    #[test]
    fn parse_color_named_grey_alias_matches_gray() {
        assert_eq!(parse_color("grey").unwrap(), Color::Gray);
        assert_eq!(parse_color("dark_grey").unwrap(), Color::DarkGray);
    }

    #[test]
    fn parse_color_named_case_insensitive() {
        assert_eq!(parse_color("RED").unwrap(), Color::Red);
        assert_eq!(parse_color("Bright_Blue").unwrap(), Color::LightBlue);
        assert_eq!(parse_color("Reset").unwrap(), Color::Reset);
    }

    #[test]
    fn parse_color_named_reset_is_terminal_default() {
        assert_eq!(parse_color("reset").unwrap(), Color::Reset);
    }

    #[test]
    fn parse_color_named_rejects_unknown_color() {
        let err = parse_color("orange").expect_err("orange is not a 16-color name");
        let msg = format!("{err:#}");
        assert!(msg.contains("orange"), "{msg}");
        // Error must hint at every supported format so a typo is recoverable without the docs.
        assert!(msg.contains("hex") || msg.contains("#rrggbb"), "{msg}");
        assert!(msg.contains("ANSI") || msg.contains("ansi:"), "{msg}");
        assert!(msg.contains("reset"), "{msg}");
    }

    // ── parse_color: edge cases ──

    #[test]
    fn parse_color_rejects_empty_string() {
        let err = parse_color("").expect_err("empty string rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("empty"), "{msg}");
    }

    #[test]
    fn parse_color_trims_surrounding_whitespace() {
        assert_eq!(parse_color("  red  ").unwrap(), Color::Red);
        assert_eq!(
            parse_color(" #cdd6f4 ").unwrap(),
            Color::Rgb(0xcd, 0xd6, 0xf4),
        );
        assert_eq!(parse_color("\tansi:5\n").unwrap(), Color::Indexed(5));
    }
}
