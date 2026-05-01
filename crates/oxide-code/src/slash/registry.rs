//! Slash-command trait and built-in registry.
//!
//! Each built-in command lives in its own module under `slash/`,
//! implements [`SlashCommand`], and lands in [`BUILT_INS`]. Adding a
//! new command is one file plus one slice entry — no central match
//! arm, no enum variant.

use super::config::ConfigCmd;
use super::context::SlashContext;
use super::diff::DiffCmd;
use super::help::HelpCmd;
use super::status::StatusCmd;

/// A locally-dispatched command typed at the input as `/name args`.
///
/// Each command owns its display metadata (name, aliases, description,
/// optional usage hint) so the help renderer and popup can drive their
/// rows from the trait alone — no parallel switch.
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

    /// Optional usage hint used by the error message when the command
    /// is invoked with malformed arguments. `None` means no args are
    /// expected.
    fn usage(&self) -> Option<&'static str> {
        None
    }

    /// Runs the command. Mutations land through `ctx` (push to chat,
    /// flip status, mutate session) so the trait stays sync — no
    /// channel round-trip to the agent loop is needed for v1.
    ///
    /// Failures return `Err(message)`. The dispatcher renders one
    /// `ErrorBlock` per failure — commands must not call
    /// `ctx.chat.push_error(...)` themselves. Successful runs push
    /// their own informational output (typically a `SystemMessageBlock`)
    /// before returning `Ok`.
    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<(), String>;
}

/// Every built-in v1 command. Order is presentation order in `/help`
/// and the popup, so the most frequently-used commands sit first.
pub(super) const BUILT_INS: &[&dyn SlashCommand] = &[&HelpCmd, &StatusCmd, &ConfigCmd, &DiffCmd];

/// Resolves `name` against `commands` by canonical name first, then
/// aliases. Returns `None` for unknown names — the dispatcher renders
/// an `ErrorBlock` in that case. Generic over the slice so tests can
/// drive it against a synthetic registry.
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

    // ── BUILT_INS contract ──

    #[test]
    fn built_ins_have_unique_canonical_names() {
        // Two commands sharing a canonical name would make `lookup`
        // ambiguous — the first registered wins, silently shadowing
        // later entries. Pin uniqueness so a well-meaning bump that
        // duplicates a name fails CI here.
        let names: HashSet<_> = BUILT_INS.iter().map(|c| c.name()).collect();
        assert_eq!(
            names.len(),
            BUILT_INS.len(),
            "duplicate canonical name in BUILT_INS",
        );
    }

    #[test]
    fn built_ins_aliases_do_not_collide_with_any_canonical_name() {
        // An alias overlapping another command's canonical name routes
        // typed `/foo` to the wrong impl. The alias / name namespace is
        // shared on lookup; pin disjointness across the registry.
        let names: HashSet<_> = BUILT_INS.iter().map(|c| c.name()).collect();
        for cmd in BUILT_INS {
            for alias in cmd.aliases() {
                assert!(
                    !names.contains(alias),
                    "alias `{alias}` of `/{}` collides with another canonical name",
                    cmd.name(),
                );
            }
        }
    }

    #[test]
    fn built_ins_have_non_empty_name_and_description() {
        for cmd in BUILT_INS {
            assert!(!cmd.name().is_empty(), "empty canonical name");
            assert!(
                !cmd.description().is_empty(),
                "/{}: empty description",
                cmd.name(),
            );
        }
    }

    // ── lookup_in ──

    /// Synthetic command with a canonical name and two aliases — used
    /// to pin alias-branch behavior without depending on whether any
    /// real built-in carries an alias.
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
        fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn aliased_cmd_fixture_satisfies_trait_contract() {
        // The lookup_in tests below rely on `AliasedCmd` having
        // exactly two aliases and a description. Pin the fixture
        // directly so a drifted edit (one alias, missing description)
        // breaks here rather than silently misleading later tests —
        // and so the trait's required slots don't sit as uncovered
        // stub bodies.
        use crate::slash::context::SlashContext;
        use crate::tui::components::chat::ChatView;
        use crate::tui::theme::Theme;

        assert_eq!(AliasedCmd.name(), "primary");
        assert_eq!(AliasedCmd.aliases(), &["alt", "shortcut"]);
        assert_eq!(AliasedCmd.description(), "fake");
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = crate::slash::test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        assert_eq!(AliasedCmd.execute("", &mut ctx), Ok(()));
    }

    #[test]
    fn lookup_in_resolves_canonical_name() {
        let cmd = lookup_in(BUILT_INS, "help").expect("/help is registered");
        assert_eq!(cmd.name(), "help");
    }

    #[test]
    fn lookup_in_resolves_each_alias_to_canonical_impl() {
        // The alias branch is dead in the live registry today (no
        // built-in carries an alias), so a mutation that flipped the
        // OR to AND would survive without this test. Drive a
        // synthetic registry to pin both branches.
        let registry: &[&dyn SlashCommand] = &[&AliasedCmd];
        for alias in ["alt", "shortcut"] {
            let cmd =
                lookup_in(registry, alias).unwrap_or_else(|| panic!("alias `{alias}` resolved"));
            assert_eq!(cmd.name(), "primary");
        }
    }

    #[test]
    fn lookup_in_unknown_name_is_absent() {
        assert!(lookup_in(BUILT_INS, "nonexistent").is_none());
    }
}
