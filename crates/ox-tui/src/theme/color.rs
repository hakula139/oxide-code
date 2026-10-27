//! Color string parsing for theme TOML values. Case-insensitive.
//!
//! - **6-digit hex**: `"#rrggbb"` for true color (24-bit RGB).
//! - **ANSI 16 named**: `"red"`, `"bright_blue"`, `"dark_gray"`, ... (`light_X` aliases `bright_X`,
//!   `grey` aliases `gray`).
//! - **Indexed 256-color**: `"ansi:N"` where `N` is 0-255.
//! - **Terminal default**: `"reset"` follows the user's terminal palette.
//!
//! Three-digit hex shorthand (`#fff`) is rejected to keep the format unambiguous.

use anyhow::{Context as _, Result, anyhow, bail};
use ratatui::style::Color;

/// Every accepted color format, which an unknown color lists.
pub(super) const COLOR_FORMATS_HINT: &str =
    "expected hex (#rrggbb), ANSI name (red, bright_blue, ...), indexed (ansi:0-255), or reset";

/// Parses a theme color string. Trims whitespace and surfaces actionable errors that name the
/// offending input and supported formats.
pub(super) fn parse_color(input: &str) -> Result<Color> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("empty color value");
    }

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
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("non-hex characters in color");
    }
    let n = u32::from_str_radix(hex, 16).expect("six ASCII hex digits fit in u32");
    let [_, r, g, b] = n.to_be_bytes();
    Ok(Color::Rgb(r, g, b))
}

fn parse_indexed(s: &str) -> Result<Color> {
    let n: u8 = s
        .parse()
        .map_err(|_| anyhow!("expected 0-255 (e.g., ansi:174)"))?;
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

        _ => bail!(COLOR_FORMATS_HINT),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod parse_color {
        use super::*;

        fn error_text(input: &str) -> String {
            format!("{:#}", parse_color(input).unwrap_err())
        }

        #[test]
        fn parses_every_accepted_form() {
            for (input, expected) in [
                ("  red  ", Color::Red),
                (" #cdd6f4 ", Color::Rgb(0xcd, 0xd6, 0xf4)),
                ("\tansi:5\n", Color::Indexed(5)),
                ("#cdd6f4", Color::Rgb(0xcd, 0xd6, 0xf4)),
                ("#CDD6F4", Color::Rgb(0xcd, 0xd6, 0xf4)),
                ("#ff0000", Color::Rgb(255, 0, 0)),
                ("#00ff00", Color::Rgb(0, 255, 0)),
                ("#0000ff", Color::Rgb(0, 0, 255)),
                ("ansi:0", Color::Indexed(0)),
                ("ansi:174", Color::Indexed(174)),
                ("ansi:255", Color::Indexed(255)),
                ("ANSI:5", Color::Indexed(5)),
                ("Ansi:174", Color::Indexed(174)),
                ("black", Color::Black),
                ("red", Color::Red),
                ("green", Color::Green),
                ("yellow", Color::Yellow),
                ("blue", Color::Blue),
                ("magenta", Color::Magenta),
                ("cyan", Color::Cyan),
                ("gray", Color::Gray),
                ("grey", Color::Gray),
                ("dark_gray", Color::DarkGray),
                ("dark_grey", Color::DarkGray),
                ("bright_red", Color::LightRed),
                ("light_red", Color::LightRed),
                ("bright_green", Color::LightGreen),
                ("bright_yellow", Color::LightYellow),
                ("bright_blue", Color::LightBlue),
                ("light_blue", Color::LightBlue),
                ("bright_magenta", Color::LightMagenta),
                ("bright_cyan", Color::LightCyan),
                ("white", Color::White),
                ("bright_white", Color::White),
                ("light_white", Color::White),
                ("RED", Color::Red),
                ("Bright_Blue", Color::LightBlue),
                ("reset", Color::Reset),
                ("Reset", Color::Reset),
            ] {
                assert_eq!(parse_color(input).unwrap(), expected, "{input:?}");
            }
        }

        #[test]
        fn rejects_every_invalid_form_with_its_reason() {
            let unknown = |input: &str| format!("unknown color {input:?}: {COLOR_FORMATS_HINT}");
            let bad_hex =
                |input: &str, reason: &str| format!("invalid hex color {input:?}: {reason}");
            let bad_index = |input: &str| {
                format!("invalid indexed color {input:?}: expected 0-255 (e.g., ansi:174)")
            };

            for (input, expected) in [
                ("", "empty color value".to_owned()),
                (
                    "#fff",
                    bad_hex("#fff", "expected 6-digit hex (e.g., #cdd6f4)"),
                ),
                ("#zzzzzz", bad_hex("#zzzzzz", "non-hex characters in color")),
                ("#+abcde", bad_hex("#+abcde", "non-hex characters in color")),
                ("cdd6f4", unknown("cdd6f4")),
                ("ansi:256", bad_index("ansi:256")),
                ("ansi:abc", bad_index("ansi:abc")),
                ("orange", unknown("orange")),
            ] {
                assert_eq!(error_text(input), expected, "{input:?}");
            }
        }
    }
}
