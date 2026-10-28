//! `/delete <id-prefix>` asks the engine to resolve the prefix, then deletes the session it found
//! through the [`ConfirmDeleteSessionModal`] that Ctrl+D in the `/resume` picker also opens. Bare
//! `/delete` names no session, so it errors.

use std::path::Path;

use time::OffsetDateTime;

use ox_core::command;
use ox_core::protocol::Op;
use ox_core::session::display::DisplayTitle;
use ox_core::session::store::listing::SessionInfo;
use ox_core::util::path::tildify;
use ox_core::util::time::now_local;

use super::confirm::ConfirmDeleteSessionModal;
use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use super::sessions::format_metadata_line;
use crate::action::Action;
use crate::modal::Modal;

// ── DeleteCmd ──

pub(super) struct DeleteCmd;

impl SlashCommand for DeleteCmd {
    fn name(&self) -> &'static str {
        "delete"
    }

    fn description(&self) -> &'static str {
        "Delete a saved session"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        // Mutating keeps both forms idle-gated, since the typed-arg form deletes a session.
        SlashKind::Mutating
    }

    fn usage(&self) -> Option<&'static str> {
        Some("<id-prefix>")
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.is_empty()
    }

    fn execute(&self, arg: &str, _ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        if arg.is_empty() {
            return Err(command::missing(
                "session ID",
                "/delete <id-prefix>",
                Some("press Ctrl+D in the `/resume` picker"),
            ));
        }
        Ok(SlashOutcome::Forward(Action::Op(Op::FindSaved {
            prefix: arg.to_owned(),
        })))
    }
}

// ── Confirmation ──

/// The confirm modal for deleting `info`, the session the engine resolved a `/delete` prefix to.
pub(crate) fn confirm_modal(info: &SessionInfo) -> Box<dyn Modal> {
    Box::new(ConfirmDeleteSessionModal::new(
        info.session_id.clone(),
        DisplayTitle::of(info),
        metadata_line(info, now_local()),
    ))
}

// ── Display Formatting ──

/// Wraps [`format_metadata_line`] with the `/delete`-specific project rule: the confirm modal has
/// no scope context, so the project always shows.
fn metadata_line(info: &SessionInfo, now: OffsetDateTime) -> String {
    let project = tildify(Path::new(&info.project_root));
    format_metadata_line(
        &info.session_id,
        info.last_active_at,
        now,
        info.message_count,
        info.git_branch.as_deref(),
        Some(&project),
    )
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use ox_core::session::store::testing::{listed_session, stamped_id};
    use ox_core::util::path::testing::HOME_VAR;

    use super::*;
    use crate::modal::testing::render_modal;
    use crate::slash::registry::testing::run_execute;

    mod classify {
        use super::*;

        #[test]
        fn is_always_mutating() {
            assert_eq!(DeleteCmd.classify(""), SlashKind::Mutating);
            assert_eq!(DeleteCmd.classify("ab"), SlashKind::Mutating);
        }
    }

    mod echoes_input {
        use super::*;

        #[test]
        fn suppressed_on_bare_form_and_active_on_typed_arg() {
            // The bare form's error block stands alone, while the typed form keeps its line above
            // the confirm modal so the user can re-read the target.
            assert!(!DeleteCmd.echoes_input(""));
            assert!(DeleteCmd.echoes_input("abcd"));
        }
    }

    mod execute {
        use super::*;

        #[test]
        fn bare_points_to_the_picker_and_typed_form() {
            let err = run_execute(&DeleteCmd, "").unwrap_err();

            assert_eq!(
                err,
                "Missing session ID. \
                 Usage: `/delete <id-prefix>`, or press Ctrl+D in the `/resume` picker.",
            );
        }

        #[test]
        fn typed_arg_asks_the_engine_to_find_the_session() {
            let outcome = run_execute(&DeleteCmd, "abab").unwrap();

            assert_eq!(
                outcome.into_forward(),
                Action::Op(Op::FindSaved {
                    prefix: "abab".to_owned(),
                })
            );
        }
    }

    mod confirm_modal {
        use super::*;

        #[test]
        fn names_the_found_session_by_title_above_its_metadata() {
            let info = SessionInfo {
                title: Some("Doomed session".to_owned()),
                ..listed_session(&stamped_id(0xab))
            };

            let mut modal = confirm_modal(&info);

            let rows = render_modal(&mut *modal, 80, u16::MAX);
            assert_eq!(rows[2], "Doomed session");
            assert!(rows[3].starts_with("abababab · "), "{}", rows[3]);
        }
    }

    mod metadata_line {
        use super::*;

        const NOW: time::OffsetDateTime = datetime!(2026-04-18 12:00:00 UTC);

        #[test]
        fn shows_the_count_branch_and_tildified_project() {
            let info = SessionInfo {
                project_root: "/work/oxide".to_owned(),
                message_count: 14,
                git_branch: Some("feat/login".to_owned()),
                ..listed_session(&stamped_id(0xab))
            };

            let line = temp_env::with_var(HOME_VAR, Some("/work"), || metadata_line(&info, NOW));

            assert_eq!(
                line,
                "abababab · 3 hours ago · 14 msgs · feat/login · ~/oxide"
            );
        }
    }
}
