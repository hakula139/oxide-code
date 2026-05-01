//! Slash-command trait and built-in registry.
//!
//! Each built-in command lives in its own module under `slash/`,
//! implements [`SlashCommand`], and lands in [`BUILT_INS`]. Adding a
//! new command is one file plus one slice entry — no central match
//! arm, no enum variant.

use super::config::Config;
use super::context::SlashContext;
use super::diff::Diff;
use super::help::Help;
use super::status::Status;

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
    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>);
}

/// Every built-in v1 command. Order is presentation order in `/help`
/// and the popup, so the most frequently-used commands sit first.
pub(crate) const BUILT_INS: &[&dyn SlashCommand] = &[&Help, &Status, &Config, &Diff];

/// Resolves `name` against canonical names then aliases. Returns
/// `None` for unknown commands — the dispatcher renders an
/// `ErrorBlock` in that case.
pub(crate) fn lookup(name: &str) -> Option<&'static dyn SlashCommand> {
    BUILT_INS
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

    // ── lookup ──

    #[test]
    fn lookup_resolves_canonical_name() {
        let cmd = lookup("help").expect("/help is registered");
        assert_eq!(cmd.name(), "help");
    }

    #[test]
    fn lookup_unknown_name_is_absent() {
        assert!(lookup("nonexistent").is_none());
    }
}
