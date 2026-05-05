//! `/help` — list every registered command. Aliases appear parenthesized after the canonical name.

use std::fmt::Write as _;

use super::context::SlashContext;
use super::format::write_kv_section;
use super::registry::{BUILT_INS, SlashCommand, SlashOutcome};

pub(super) struct HelpCmd;

impl SlashCommand for HelpCmd {
    fn name(&self) -> &'static str {
        "help"
    }

    fn description(&self) -> &'static str {
        "List the available slash commands and their usage hints"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        ctx.chat.push_system_message(render_help());
        Ok(SlashOutcome::Done)
    }
}

fn render_help() -> String {
    let labels: Vec<String> = BUILT_INS.iter().map(|c| display_label(*c)).collect();
    let rows = labels
        .iter()
        .zip(BUILT_INS)
        .map(|(label, cmd)| (label.as_str(), cmd.description()));
    let mut out = String::new();
    write_kv_section(&mut out, "Available Commands", rows);
    out.push_str(
        "\nTip: prefix with `//` to send a literal slash to the model (e.g., `//etc/hosts`).\n",
    );
    out
}

/// `/name (aliases) <usage>` — combines canonical name, optional alias list, and optional usage.
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

    // ── HelpCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(HelpCmd.name(), "help");
        assert!(HelpCmd.aliases().is_empty());
        assert!(!HelpCmd.description().is_empty());
    }

    // ── render_help ──

    #[test]
    fn render_help_starts_with_heading_and_lists_every_command() {
        let body = render_help();
        let mut lines = body.lines();
        assert_eq!(lines.next(), Some("Available Commands"));
        assert_eq!(lines.next(), Some(""), "heading separated by blank line");
        for cmd in BUILT_INS {
            let needle = format!("/{}", cmd.name());
            assert!(
                body.contains(&needle),
                "help body missing `{needle}`: {body}",
            );
        }
    }

    #[test]
    fn render_help_includes_escape_tip_footer() {
        let body = render_help();
        assert!(body.contains("`//`"), "footer missing tip body: {body}");
        assert!(
            body.trim_end().ends_with("`//etc/hosts`)."),
            "tip should be the last paragraph: {body}",
        );
    }

    #[test]
    fn render_help_aligns_descriptions_to_a_shared_gutter() {
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
        assert_eq!(display_label(&Fake::CLEAR), "/clear (new, reset)");
    }

    #[test]
    fn fake_fixture_stub_methods_satisfy_trait_contract() {
        use crate::tui::components::chat::ChatView;
        use crate::tui::theme::Theme;

        assert_eq!(Fake::CLEAR.description(), "");
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = crate::slash::test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
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
