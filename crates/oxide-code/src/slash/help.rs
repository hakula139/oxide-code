//! `/help` opens a read-only [`KvOverview`] of every registered command.

use std::fmt::Write as _;

use super::context::SlashContext;
use super::registry::{BUILT_INS, SlashCommand, SlashOutcome};
use crate::tui::modal::kv_overview::{KvOverview, KvSection};

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
    use crate::slash::clear::ClearCmd;
    use crate::slash::context::SlashContext;
    use crate::slash::model::ModelCmd;
    use crate::slash::resume::ResumeCmd;
    use crate::tui::modal::Modal;

    // ── HelpCmd::name ──

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(HelpCmd.name(), "help");
        assert_eq!(HelpCmd.aliases(), [] as [&str; 0]);
        assert_ne!(HelpCmd.description(), "");
    }

    // ── HelpCmd::execute ──

    #[test]
    fn execute_opens_the_overview_modal() {
        let info = crate::slash::testing::test_session_info();
        let outcome = HelpCmd.execute("", &SlashContext::new(&info, None));
        outcome.unwrap().into_modal();
    }

    // ── build_modal ──

    #[test]
    fn build_modal_renders_one_row_per_built_in() {
        let m = build_modal();
        // title + blank + N rows + blank + footer.
        let expected = u16::try_from(BUILT_INS.len() + 4).unwrap();
        assert_eq!(m.height(80, u16::MAX), expected);
    }

    // ── display_label ──

    #[test]
    fn display_label_combines_name_aliases_and_usage() {
        assert_eq!(display_label(&HelpCmd), "/help");
        assert_eq!(display_label(&ClearCmd), "/clear (new, reset)");
        assert_eq!(display_label(&ModelCmd), "/model [<id>]");
        assert_eq!(
            display_label(&ResumeCmd),
            "/resume (continue) [<id-prefix>]"
        );
    }
}
