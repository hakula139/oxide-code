//! `/archive` and `/unarchive` set whether a session shows in default listings and bare
//! `--continue`. Bare `/archive` forwards [`Op::Archive`], which archives the live session and
//! rolls to a fresh one. The typed-arg forms forward [`Op::SetArchive`] for the engine to resolve.

use ox_core::command;
use ox_core::protocol::{Op, SavedTarget};
use ox_core::session::store::listing::ArchiveState;

use super::registry::{SlashCommand, SlashOutcome};
use crate::action::Action;
use crate::session_info::LiveSessionInfo;

// ── ArchiveCmd ──

pub(super) struct ArchiveCmd;

impl SlashCommand for ArchiveCmd {
    fn name(&self) -> &'static str {
        "archive"
    }

    fn description(&self) -> &'static str {
        "Archive this session or a saved one"
    }

    fn idle_only(&self) -> bool {
        true
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<id-prefix>]")
    }

    fn execute(&self, prefix: &str, _info: &LiveSessionInfo) -> Result<SlashOutcome, String> {
        if prefix.is_empty() {
            return Ok(SlashOutcome::Forward(Action::Op(Op::Archive)));
        }
        Ok(set_archive(prefix, ArchiveState::Archived))
    }
}

// ── UnarchiveCmd ──

pub(super) struct UnarchiveCmd;

impl SlashCommand for UnarchiveCmd {
    fn name(&self) -> &'static str {
        "unarchive"
    }

    fn description(&self) -> &'static str {
        "Unarchive a saved session"
    }

    fn idle_only(&self) -> bool {
        true
    }

    fn usage(&self) -> Option<&'static str> {
        Some("<id-prefix>")
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.is_empty()
    }

    fn execute(&self, prefix: &str, _info: &LiveSessionInfo) -> Result<SlashOutcome, String> {
        if prefix.is_empty() {
            return Err(command::missing(
                "session ID",
                "/unarchive <id-prefix>",
                Some(
                    "press Ctrl+A in the `/resume` picker to list archived sessions and Ctrl+X to \
                     unarchive one",
                ),
            ));
        }
        Ok(set_archive(prefix, ArchiveState::Active))
    }
}

// ── Helpers ──

fn set_archive(prefix: &str, state: ArchiveState) -> SlashOutcome {
    SlashOutcome::Forward(Action::Op(Op::SetArchive {
        target: SavedTarget::Prefix(prefix.to_owned()),
        state,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::registry::testing::run_execute;

    mod execute {
        use super::*;

        #[test]
        fn bare_forwards_the_archive_action() {
            let outcome = run_execute(&ArchiveCmd, "").unwrap();

            assert_eq!(outcome.into_forward(), Action::Op(Op::Archive));
        }

        #[test]
        fn prefix_asks_the_engine_to_set_the_named_state() {
            for (cmd, state) in [
                (&ArchiveCmd as &dyn SlashCommand, ArchiveState::Archived),
                (&UnarchiveCmd, ArchiveState::Active),
            ] {
                let outcome = run_execute(cmd, "abab").unwrap();

                assert_eq!(
                    outcome.into_forward(),
                    Action::Op(Op::SetArchive {
                        target: SavedTarget::Prefix("abab".to_owned()),
                        state,
                    }),
                    "/{}",
                    cmd.name(),
                );
            }
        }
    }

    mod unarchive_cmd {
        use super::*;

        mod echoes_input {
            use super::*;

            #[test]
            fn only_with_a_prefix() {
                assert!(UnarchiveCmd.echoes_input("ab"));
                assert!(!UnarchiveCmd.echoes_input(""), "bare form stands alone");
            }
        }

        mod execute {
            use super::*;

            #[test]
            fn bare_points_to_the_archived_picker() {
                let err = run_execute(&UnarchiveCmd, "").unwrap_err();

                assert_eq!(
                    err,
                    "Missing session ID. \
                     Usage: `/unarchive <id-prefix>`, or press Ctrl+A in the `/resume` picker \
                     to list archived sessions and Ctrl+X to unarchive one.",
                );
            }
        }
    }
}
