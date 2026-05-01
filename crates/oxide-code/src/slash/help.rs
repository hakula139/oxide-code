//! `/help` — list every registered command.
//!
//! Renders one row per command (not one row per name): aliases live
//! parenthesized after the canonical name, matching the popup's
//! display rule.

use std::fmt::Write as _;

use super::context::SlashContext;
use super::registry::{BUILT_INS, SlashCommand};

pub(crate) struct Help;

impl SlashCommand for Help {
    fn name(&self) -> &'static str {
        "help"
    }

    fn description(&self) -> &'static str {
        "list available commands"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) {
        ctx.chat.push_system_message(render_help());
    }
}

/// Plain-text help body. The first line is a heading; subsequent rows
/// are `<label><gap><description>`, padded so descriptions form a
/// clean second column. The label folds aliases into the canonical
/// name (`name (alias, alias)`) and appends the usage hint when the
/// command has one (`name <arg>`).
fn render_help() -> String {
    let labels: Vec<String> = BUILT_INS.iter().map(|c| display_label(*c)).collect();
    let gutter = labels.iter().map(String::len).max().unwrap_or(0);

    let mut out = String::from("Available commands:\n");
    for (cmd, label) in BUILT_INS.iter().zip(&labels) {
        let pad = gutter.saturating_sub(label.len());
        let _ = writeln!(
            out,
            "  {label}{spaces}  {desc}",
            spaces = " ".repeat(pad),
            desc = cmd.description(),
        );
    }
    out
}

/// Display label combining canonical name, optional alias list, and
/// optional usage hint into one cell:
///
/// - `/help` — no aliases, no args.
/// - `/clear (new, reset)` — aliases only.
/// - `/model <model-id>` — usage only.
/// - `/clear (new, reset) <args>` — both.
///
/// Shared with the popup row renderer (added in a later commit).
pub(crate) fn display_label(cmd: &dyn SlashCommand) -> String {
    let mut out = format!("/{}", cmd.name());
    if !cmd.aliases().is_empty() {
        let _ = write!(out, " ({})", cmd.aliases().join(", "));
    }
    if let Some(usage) = cmd.usage() {
        out.push(' ');
        out.push_str(usage);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── display_label ──

    #[test]
    fn display_label_no_aliases_is_just_slashed_name() {
        assert_eq!(display_label(&Help), "/help");
    }

    #[test]
    fn display_label_with_aliases_lists_them_in_parens() {
        // Pinned with a fake command instead of a registered one so
        // the test doesn't break when /clear's alias list later
        // changes — the format rule itself is what we're locking.
        struct Fake;
        impl SlashCommand for Fake {
            fn name(&self) -> &'static str {
                "clear"
            }
            fn aliases(&self) -> &'static [&'static str] {
                &["new", "reset"]
            }
            fn description(&self) -> &'static str {
                ""
            }
            fn execute(&self, _: &str, _: &mut SlashContext<'_>) {}
        }
        assert_eq!(display_label(&Fake), "/clear (new, reset)");
    }

    // ── render_help ──

    #[test]
    fn render_help_starts_with_heading_and_lists_every_command() {
        let body = render_help();
        let mut lines = body.lines();
        assert_eq!(lines.next(), Some("Available commands:"));
        // Each registered command appears as a row whose body contains
        // its slash-prefixed canonical name. Skipping the popup-format
        // alias presentation here — `display_label` already covers it.
        for cmd in BUILT_INS {
            let needle = format!("/{}", cmd.name());
            assert!(
                body.contains(&needle),
                "help body missing `{needle}`: {body}",
            );
        }
    }

    #[test]
    fn render_help_aligns_descriptions_to_a_shared_gutter() {
        // The longest label sets the gutter; every row must position
        // its description at that column so the second column reads
        // as a clean stripe. A regression that drops the padding
        // would land descriptions ragged-right under the names.
        let body = render_help();
        // Descriptions follow exactly two spaces after the label cell.
        // Verify by checking the description column index is uniform
        // across rows.
        let cols: Vec<_> = body
            .lines()
            .skip(1) // heading
            .filter(|l| !l.is_empty())
            .map(|l| {
                // Each row begins with two leading spaces. The
                // description starts at the first non-space char after
                // the label run + its trailing two-space gap. Locate
                // it as the byte index of the second double-space.
                l.strip_prefix("  ")
                    .unwrap_or(l)
                    .find("  ")
                    .expect("each row should have a label/desc gap")
            })
            .collect();
        if let Some(first) = cols.first() {
            assert!(
                cols.iter().all(|c| c == first),
                "row gutters not aligned: {cols:?}",
            );
        }
    }
}
