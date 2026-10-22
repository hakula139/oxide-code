//! `/help` opens a read-only [`KvOverview`] of every registered command.

use std::fmt::Write as _;

use ox_core::command;

use super::context::SlashContext;
use super::registry::{BUILT_INS, SlashCommand, SlashOutcome};
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
    KvOverview::new("Help", vec![KvSection::new(rows)])
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
    use crate::modal::Modal;
    use crate::modal::testing::render_rows;
    use crate::slash::registry::testing::run_execute;

    mod execute {
        use super::*;

        #[test]
        fn opens_the_overview_modal() {
            let outcome = run_execute(&HelpCmd, "");
            outcome.unwrap().into_modal();
        }
    }

    mod build_modal {
        use super::*;

        #[test]
        fn lists_every_built_in_with_its_aliases_usage_and_description() {
            let modal = build_modal();

            insta::assert_debug_snapshot!(render_rows(&modal, 100, modal.height(100, u16::MAX)));
        }
    }
}
