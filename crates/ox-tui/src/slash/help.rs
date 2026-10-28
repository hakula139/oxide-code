//! `/help` opens a read-only [`KvOverview`] of every registered command.

use std::fmt::Write as _;

use ox_core::command;

use super::context::SlashContext;
use super::registry::{BUILT_INS, SlashCommand, SlashOutcome};
use crate::action::TOGGLE_EXPANDED;
use crate::keymap::{CTRL_C, CTRL_D};
use crate::modal::kv_overview::{KvOverview, KvSection};

// ── HelpCmd ──

pub(super) struct HelpCmd;

impl SlashCommand for HelpCmd {
    fn name(&self) -> &'static str {
        "help"
    }

    fn description(&self) -> &'static str {
        "List slash commands"
    }

    fn echoes_input(&self, _args: &str) -> bool {
        false
    }

    fn execute(&self, _args: &str, _ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        Ok(SlashOutcome::Modal(Box::new(build_modal())))
    }
}

// ── Helpers ──

fn build_modal() -> KvOverview {
    let rows = BUILT_INS
        .iter()
        .map(|cmd| (display_label(*cmd), cmd.description().to_owned()))
        .collect();
    KvOverview::new(
        "Help",
        vec![
            KvSection::new(rows).with_heading("Commands"),
            KvSection::new(vec![
                (
                    "Tab / Shift+Tab".to_owned(),
                    "Switch the permission mode".to_owned(),
                ),
                ("Esc".to_owned(), "Stop the running turn".to_owned()),
                (
                    TOGGLE_EXPANDED.to_string(),
                    "Expand or collapse long output".to_owned(),
                ),
                (
                    format!("{CTRL_C} twice"),
                    "Exit, even with a draft".to_owned(),
                ),
                (CTRL_D.to_string(), "Exit with an empty input".to_owned()),
                (
                    "//".to_owned(),
                    "Send text that starts with a slash".to_owned(),
                ),
            ])
            .with_heading("Keys"),
        ],
    )
}

/// `/name (aliases) <usage>`, where the aliases and usage are optional.
fn display_label(cmd: &dyn SlashCommand) -> String {
    let mut out = format!("/{}", cmd.name());
    let aliases = command::aliases(cmd.name());
    if !aliases.is_empty() {
        _ = write!(out, " ({})", aliases.join(", "));
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
    use crate::modal::testing::render_modal;

    mod build_modal {
        use super::*;

        #[test]
        fn lists_every_built_in_with_its_aliases_usage_and_description() {
            let mut modal = build_modal();

            insta::assert_debug_snapshot!(render_modal(&mut modal, 100));
        }
    }
}
