//! Slash-command trait and built-in registry. Adding a command: one file + one [`BUILT_INS`] entry.

use super::clear::ClearCmd;
use super::config::ConfigCmd;
use super::context::SlashContext;
use super::diff::DiffCmd;
use super::effort::EffortCmd;
use super::help::HelpCmd;
use super::init::InitCmd;
use super::model::ModelCmd;
use super::status::StatusCmd;
use crate::agent::event::UserAction;

/// `Done` for client-side work via `ctx`; `Forward` for state-mutating commands.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SlashOutcome {
    Done,
    Forward(UserAction),
}

/// Whether a slash command can run while the agent is busy.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SlashKind {
    ReadOnly,
    Mutating,
    /// Returned only by the free dispatcher, never by trait impls.
    Unknown,
}

/// Locally-dispatched `/name args` command. Each impl owns its display metadata.
pub(crate) trait SlashCommand: Sync {
    fn name(&self) -> &'static str;

    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    fn description(&self) -> &'static str;

    /// Per-form: `ReadOnly` or `Mutating`, never `Unknown`.
    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::ReadOnly
    }

    fn usage(&self) -> Option<&'static str> {
        None
    }

    /// `Err(msg)` renders as an `ErrorBlock`.
    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String>;
}

/// Alphabetical for stable `/help` and empty-query popup ordering.
pub(super) const BUILT_INS: &[&dyn SlashCommand] = &[
    &ClearCmd, &ConfigCmd, &DiffCmd, &EffortCmd, &HelpCmd, &InitCmd, &ModelCmd, &StatusCmd,
];

/// Resolves `name` against canonical names first, then aliases.
pub(super) fn lookup_in<'a>(
    commands: &'a [&'a dyn SlashCommand],
    name: &str,
) -> Option<&'a dyn SlashCommand> {
    commands
        .iter()
        .find(|cmd| cmd.name() == name || cmd.aliases().contains(&name))
        .copied()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::slash::context::SlashContext;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn run_execute(cmd: &dyn SlashCommand, args: &str) -> Result<SlashOutcome, String> {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = crate::slash::test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        cmd.execute(args, &mut ctx)
    }

    // ── BUILT_INS contract ──

    #[test]
    fn built_ins_have_unique_canonical_names() {
        let names: HashSet<_> = BUILT_INS.iter().map(|c| c.name()).collect();
        assert_eq!(
            names.len(),
            BUILT_INS.len(),
            "duplicate canonical name in BUILT_INS",
        );
    }

    fn alias_collisions<'a>(commands: &[&'a dyn SlashCommand]) -> Vec<(&'a str, &'a str)> {
        let names: HashSet<_> = commands.iter().map(|c| c.name()).collect();
        commands
            .iter()
            .flat_map(|cmd| {
                cmd.aliases()
                    .iter()
                    .filter(|alias| names.contains(*alias))
                    .map(move |alias| (cmd.name(), *alias))
            })
            .collect()
    }

    fn empty_metadata_offenders<'a>(commands: &[&'a dyn SlashCommand]) -> Vec<&'a str> {
        commands
            .iter()
            .filter(|c| c.name().is_empty() || c.description().is_empty())
            .map(|c| c.name())
            .collect()
    }

    #[test]
    fn built_ins_aliases_do_not_collide_with_any_canonical_name() {
        let collisions = alias_collisions(BUILT_INS);
        assert!(collisions.is_empty(), "alias collisions: {collisions:?}");
    }

    #[test]
    fn alias_collisions_finds_a_synthetic_overlap() {
        struct ColliderCmd;
        impl SlashCommand for ColliderCmd {
            fn name(&self) -> &'static str {
                "collider"
            }
            fn aliases(&self) -> &'static [&'static str] {
                &["help"]
            }
            fn description(&self) -> &'static str {
                "collide"
            }
            fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
                Ok(SlashOutcome::Done)
            }
        }
        let registry: &[&dyn SlashCommand] = &[&HelpCmd, &ColliderCmd];
        assert_eq!(alias_collisions(registry), vec![("collider", "help")]);

        assert_eq!(ColliderCmd.description(), "collide");
        assert_eq!(run_execute(&ColliderCmd, ""), Ok(SlashOutcome::Done));
    }

    #[test]
    fn built_ins_have_non_empty_name_and_description() {
        let offenders = empty_metadata_offenders(BUILT_INS);
        assert!(
            offenders.is_empty(),
            "commands with empty name or description: {offenders:?}",
        );
    }

    #[test]
    fn empty_metadata_offenders_flags_a_synthetic_violator() {
        struct EmptyDescCmd;
        impl SlashCommand for EmptyDescCmd {
            fn name(&self) -> &'static str {
                "no-desc"
            }
            fn description(&self) -> &'static str {
                ""
            }
            fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
                Ok(SlashOutcome::Done)
            }
        }
        assert_eq!(empty_metadata_offenders(&[&EmptyDescCmd]), vec!["no-desc"]);

        assert_eq!(run_execute(&EmptyDescCmd, ""), Ok(SlashOutcome::Done));
    }

    // ── lookup_in ──

    struct AliasedCmd;
    impl SlashCommand for AliasedCmd {
        fn name(&self) -> &'static str {
            "primary"
        }
        fn aliases(&self) -> &'static [&'static str] {
            &["alt", "shortcut"]
        }
        fn description(&self) -> &'static str {
            "fake"
        }
        fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
            Ok(SlashOutcome::Done)
        }
    }

    #[test]
    fn aliased_cmd_fixture_satisfies_trait_contract() {
        assert_eq!(AliasedCmd.name(), "primary");
        assert_eq!(AliasedCmd.aliases(), &["alt", "shortcut"]);
        assert_eq!(AliasedCmd.description(), "fake");
        assert_eq!(AliasedCmd.classify(""), SlashKind::ReadOnly);
        assert_eq!(AliasedCmd.classify("anything"), SlashKind::ReadOnly);
        assert_eq!(run_execute(&AliasedCmd, ""), Ok(SlashOutcome::Done));
    }

    #[test]
    fn lookup_in_resolves_canonical_name() {
        let cmd = lookup_in(BUILT_INS, "help").expect("/help is registered");
        assert_eq!(cmd.name(), "help");
    }

    #[test]
    fn lookup_in_resolves_each_alias_to_canonical_impl() {
        let registry: &[&dyn SlashCommand] = &[&AliasedCmd];
        for alias in ["alt", "shortcut"] {
            let cmd = lookup_in(registry, alias).expect("alias must resolve");
            assert_eq!(cmd.name(), "primary");
        }
    }

    #[test]
    fn lookup_in_unknown_name_is_absent() {
        assert!(lookup_in(BUILT_INS, "nonexistent").is_none());
    }
}
