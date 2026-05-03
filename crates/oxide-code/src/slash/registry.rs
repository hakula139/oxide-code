//! Slash-command trait and built-in registry.
//!
//! Each built-in command lives in its own module under `slash/`,
//! implements [`SlashCommand`], and lands in [`BUILT_INS`]. Adding a
//! new command is one file plus one slice entry — no central match
//! arm, no enum variant.

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

/// What [`SlashCommand::execute`] returns. `Local` for client-side
/// work that finishes via `ctx`; `Action` for state-mutating
/// commands that hand a [`UserAction`] back for the dispatcher to
/// forward to the agent loop. The trait stays the only seam — slash
/// impls never reach into `user_tx` themselves.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SlashOutcome {
    Local,
    Action(UserAction),
}

/// A locally-dispatched command typed as `/name args`. Each command
/// owns its display metadata so help and popup rows render from the
/// trait alone — no parallel switch.
pub(crate) trait SlashCommand: Sync {
    /// Canonical name shown first in help and popup rows. No leading
    /// `/`. ASCII letters / digits plus `_`, `-`, `:`, `.` are allowed.
    fn name(&self) -> &'static str;

    /// Alternate names that route to the same impl. Display is
    /// consolidated as `/name (alias1, alias2)` — alias rows do not
    /// appear separately. Default is empty.
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// One-line description for help and the popup gutter.
    fn description(&self) -> &'static str;

    /// Whether this invocation is safe to run mid-turn. Mutating
    /// commands return `false` to refuse instead of racing the live
    /// turn. `args` enables per-form classification — `/model` lists
    /// when bare and mutates when given an id.
    fn is_read_only(&self, _args: &str) -> bool {
        true
    }

    /// Optional usage hint used by the error message when the command
    /// is invoked with malformed arguments. `None` means no args are
    /// expected.
    fn usage(&self) -> Option<&'static str> {
        None
    }

    /// Runs the command. Mutations land through `ctx`. `Err(msg)` is
    /// rendered by the dispatcher as a single `ErrorBlock` — commands
    /// must not push errors themselves. `Ok(Local)` commands push
    /// their own informational block; `Ok(Action(_))` commands hand
    /// a `UserAction` back for the dispatcher to forward.
    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String>;
}

/// Every built-in command. Alphabetical for stable presentation in
/// `/help` and the empty-query popup; the matcher already sorts
/// alphabetically within each tier when filtering, so this keeps
/// every popup state consistent.
pub(super) const BUILT_INS: &[&dyn SlashCommand] = &[
    &ClearCmd, &ConfigCmd, &DiffCmd, &EffortCmd, &HelpCmd, &InitCmd, &ModelCmd, &StatusCmd,
];

/// Resolves `name` by canonical name first, then aliases. Generic
/// over the slice so tests can drive it against a synthetic registry.
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

    /// Runs `cmd.execute` against a fresh in-memory chat. Lets synthetic
    /// fixtures pin their trait stubs without per-test boilerplate.
    fn run_execute(cmd: &dyn SlashCommand, args: &str) -> Result<SlashOutcome, String> {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = crate::slash::test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        cmd.execute(args, &mut ctx)
    }

    // ── BUILT_INS contract ──

    #[test]
    fn built_ins_have_unique_canonical_names() {
        // Duplicate names would silently shadow — first wins.
        let names: HashSet<_> = BUILT_INS.iter().map(|c| c.name()).collect();
        assert_eq!(
            names.len(),
            BUILT_INS.len(),
            "duplicate canonical name in BUILT_INS",
        );
    }

    /// Every `(canonical, alias)` pair where the alias overlaps a
    /// canonical name in `commands`. Empty ⇒ namespace is disjoint.
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

    /// Canonical names of commands missing a non-empty name or
    /// description. Empty ⇒ every command satisfies the metadata contract.
    fn empty_metadata_offenders<'a>(commands: &[&'a dyn SlashCommand]) -> Vec<&'a str> {
        commands
            .iter()
            .filter(|c| c.name().is_empty() || c.description().is_empty())
            .map(|c| c.name())
            .collect()
    }

    #[test]
    fn built_ins_aliases_do_not_collide_with_any_canonical_name() {
        // Alias / name namespace is shared on lookup; an overlap routes
        // a typed name to the wrong impl.
        let collisions = alias_collisions(BUILT_INS);
        assert!(collisions.is_empty(), "alias collisions: {collisions:?}");
    }

    #[test]
    fn alias_collisions_finds_a_synthetic_overlap() {
        // BUILT_INS has no aliases today, so the collision branch needs
        // a synthetic registry to execute. `ColliderCmd`'s alias `help`
        // overlaps `HelpCmd`'s canonical name.
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
                Ok(SlashOutcome::Local)
            }
        }
        let registry: &[&dyn SlashCommand] = &[&HelpCmd, &ColliderCmd];
        assert_eq!(alias_collisions(registry), vec![("collider", "help")]);

        // Exercise the trait stubs the helper doesn't reach.
        assert_eq!(ColliderCmd.description(), "collide");
        assert_eq!(run_execute(&ColliderCmd, ""), Ok(SlashOutcome::Local));
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
        // Drive the offender-collection branch — BUILT_INS satisfies
        // the contract, so a positive case is needed for coverage.
        struct EmptyDescCmd;
        impl SlashCommand for EmptyDescCmd {
            fn name(&self) -> &'static str {
                "no-desc"
            }
            fn description(&self) -> &'static str {
                ""
            }
            fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
                Ok(SlashOutcome::Local)
            }
        }
        assert_eq!(empty_metadata_offenders(&[&EmptyDescCmd]), vec!["no-desc"]);

        // Exercise the execute stub the offender helper doesn't reach.
        assert_eq!(run_execute(&EmptyDescCmd, ""), Ok(SlashOutcome::Local));
    }

    // ── lookup_in ──

    /// Synthetic alias-bearing command for `lookup_in` tests; lets
    /// them pin the alias branch independent of the live registry.
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
            Ok(SlashOutcome::Local)
        }
    }

    #[test]
    fn aliased_cmd_fixture_satisfies_trait_contract() {
        // Pin so a fixture drift fails here rather than silently
        // misleading the lookup_in tests. The `is_read_only` calls
        // also exercise the trait's default body — `AliasedCmd`
        // doesn't override it.
        assert_eq!(AliasedCmd.name(), "primary");
        assert_eq!(AliasedCmd.aliases(), &["alt", "shortcut"]);
        assert_eq!(AliasedCmd.description(), "fake");
        assert!(AliasedCmd.is_read_only(""));
        assert!(AliasedCmd.is_read_only("anything"));
        assert_eq!(run_execute(&AliasedCmd, ""), Ok(SlashOutcome::Local));
    }

    #[test]
    fn lookup_in_resolves_canonical_name() {
        let cmd = lookup_in(BUILT_INS, "help").expect("/help is registered");
        assert_eq!(cmd.name(), "help");
    }

    #[test]
    fn lookup_in_resolves_each_alias_to_canonical_impl() {
        // Alias branch is dead in the live registry — drive a synthetic
        // one so a mutation flipping `||` to `&&` fails here.
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
