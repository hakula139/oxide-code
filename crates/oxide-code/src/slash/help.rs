//! `/help` — open a read-only [`KvOverview`] of every registered command. Aliases parenthesize
//! after the canonical name; the optional `usage()` placeholder appends after that.

use std::fmt::Write as _;

use super::context::SlashContext;
use super::registry::{BUILT_INS, SlashCommand, SlashOutcome};
use crate::tui::modal::kv_overview::{KvOverview, KvSection};

pub(super) struct HelpCmd;

impl SlashCommand for HelpCmd {
    fn name(&self) -> &'static str {
        "help"
    }

    fn description(&self) -> &'static str {
        "List the available slash commands and their usage hints"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        ctx.open_modal(Box::new(build_modal()));
        Ok(SlashOutcome::Done)
    }
}

fn build_modal() -> KvOverview {
    let rows = BUILT_INS
        .iter()
        .map(|cmd| (display_label(*cmd), cmd.description().to_owned()))
        .collect();
    KvOverview::new("Help", vec![KvSection::new(rows)])
}

/// `/name (aliases) <usage>` — alias list and usage are optional.
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
    use crate::slash::context::SlashContext;
    use crate::tui::components::chat::ChatView;
    use crate::tui::modal::Modal;
    use crate::tui::theme::Theme;

    // ── HelpCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(HelpCmd.name(), "help");
        assert!(HelpCmd.aliases().is_empty());
        assert!(!HelpCmd.description().is_empty());
    }

    // ── HelpCmd::execute ──

    #[test]
    fn execute_opens_a_modal_via_ctx_and_pushes_no_chat_block() {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = crate::slash::test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        HelpCmd.execute("", &mut ctx).unwrap();
        assert!(
            ctx.take_modal().is_some(),
            "/help must populate the modal slot",
        );
        assert_eq!(chat.entry_count(), 0, "chat must stay clean on open");
    }

    // ── build_modal ──

    #[test]
    fn build_modal_renders_one_row_per_built_in() {
        let m = build_modal();
        // title + blank + N rows + blank + footer.
        let expected = u16::try_from(BUILT_INS.len() + 4).unwrap();
        assert_eq!(m.height(80), expected);
    }

    // ── display_label ──

    #[test]
    fn display_label_no_aliases_is_just_slashed_name() {
        assert_eq!(display_label(&HelpCmd), "/help");
    }

    #[test]
    fn display_label_with_aliases_lists_them_in_parens() {
        assert_eq!(display_label(&Fake::CLEAR), "/clear (new, reset)");
    }

    #[test]
    fn fake_fixture_stub_methods_satisfy_trait_contract() {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = crate::slash::test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        assert_eq!(Fake::CLEAR.description(), "");
        assert_eq!(Fake::CLEAR.execute("", &mut ctx), Ok(SlashOutcome::Done));
    }

    #[test]
    fn display_label_with_usage_appends_hint_after_name() {
        assert_eq!(display_label(&Fake::MODEL), "/model <model-id>");
        assert_eq!(
            display_label(&Fake::CLEAR_WITH_USAGE),
            "/clear (new, reset) <args>",
        );
    }

    /// Covers the no-alias / alias-only / usage-only / both-present matrix.
    struct Fake {
        name: &'static str,
        aliases: &'static [&'static str],
        usage: Option<&'static str>,
    }

    impl Fake {
        const CLEAR: Self = Self {
            name: "clear",
            aliases: &["new", "reset"],
            usage: None,
        };
        const MODEL: Self = Self {
            name: "model",
            aliases: &[],
            usage: Some("<model-id>"),
        };
        const CLEAR_WITH_USAGE: Self = Self {
            name: "clear",
            aliases: &["new", "reset"],
            usage: Some("<args>"),
        };
    }

    impl SlashCommand for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn aliases(&self) -> &'static [&'static str] {
            self.aliases
        }
        fn description(&self) -> &'static str {
            ""
        }
        fn usage(&self) -> Option<&'static str> {
            self.usage
        }
        fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
            Ok(SlashOutcome::Done)
        }
    }
}
