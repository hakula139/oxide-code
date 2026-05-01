//! `/help` — list every registered command.
//!
//! Renders one row per command (not one row per name): aliases live
//! parenthesized after the canonical name, matching the popup's
//! display rule.

use std::fmt::Write as _;

use super::context::SlashContext;
use super::format::write_kv_table;
use super::registry::{BUILT_INS, SlashCommand};

pub(crate) struct HelpCmd;

impl SlashCommand for HelpCmd {
    fn name(&self) -> &'static str {
        "help"
    }

    fn description(&self) -> &'static str {
        "list available commands"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<(), String> {
        ctx.chat.push_system_message(render_help());
        Ok(())
    }
}

/// Plain-text help body. Heading on its own line, blank separator,
/// then a key-value table where the key is the display label and the
/// value is the command description. Heading shape matches `/status`
/// and `/config` so the three commands feel parallel.
fn render_help() -> String {
    let labels: Vec<String> = BUILT_INS.iter().map(|c| display_label(*c)).collect();
    let mut out = String::from("Available commands\n\n");
    write_kv_table(
        &mut out,
        labels
            .iter()
            .zip(BUILT_INS)
            .map(|(label, cmd)| (label.as_str(), cmd.description())),
    );
    out
}

/// Display label combining canonical name, optional alias list, and
/// optional usage hint into one cell:
///
/// - `/help` — no aliases, no args.
/// - `/clear (new, reset)` — aliases only.
/// - `/model <model-id>` — usage only.
/// - `/clear (new, reset) <args>` — both.
fn display_label(cmd: &dyn SlashCommand) -> String {
    let mut out = format!("/{}", cmd.name());
    if !cmd.aliases().is_empty() {
        _ = write!(out, " ({})", cmd.aliases().join(", "));
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

    // ── render_help ──

    #[test]
    fn render_help_starts_with_heading_and_lists_every_command() {
        let body = render_help();
        let mut lines = body.lines();
        assert_eq!(lines.next(), Some("Available commands"));
        assert_eq!(lines.next(), Some(""), "heading separated by blank line");
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
        // Pin the actual gutter column the renderer agreed on, not just
        // "all rows match each other" — a uniformly broken renderer
        // would still pass the latter. Expected column = "  " row
        // prefix + longest label width + "  " gap.
        let body = render_help();
        let longest = BUILT_INS
            .iter()
            .map(|c| display_label(*c).len())
            .max()
            .unwrap_or(0);
        let expected = "  ".len() + longest + "  ".len();
        for (line, desc) in body
            .lines()
            .skip(2)
            .filter(|l| !l.is_empty())
            .zip(BUILT_INS.iter().map(|c| c.description()))
        {
            let col = line.find(desc).expect("description missing from row");
            assert_eq!(col, expected, "row mis-aligned: {line:?}");
        }
    }

    // ── display_label ──

    #[test]
    fn display_label_no_aliases_is_just_slashed_name() {
        assert_eq!(display_label(&HelpCmd), "/help");
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
            fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<(), String> {
                Ok(())
            }
        }
        assert_eq!(display_label(&Fake), "/clear (new, reset)");
    }
}
