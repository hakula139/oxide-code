//! `/archive` and `/unarchive` set whether a session shows in default listings and bare
//! `--continue`. Bare `/archive` forwards [`Op::Archive`], which archives the live session and
//! rolls to a fresh one. The typed-arg forms send saved-session operations to the engine.

use ox_core::command;
use ox_core::protocol::Op;
use ox_core::session::display::id_prefix;
use ox_core::session::entry::{ArchiveState, SessionInfo};
use ox_core::session::resolver::{ResolveError, resolve_saved_prefix_blocking};

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use super::sessions::{BARE_ARCHIVE_HINT, open_store, resolve_error_message};
use crate::action::Action;

// ── ArchiveCmd ──

pub(super) struct ArchiveCmd;

impl SlashCommand for ArchiveCmd {
    fn name(&self) -> &'static str {
        "archive"
    }

    fn description(&self) -> &'static str {
        "Archive this session or a saved one"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<id-prefix>]")
    }

    fn execute(&self, prefix: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        if prefix.is_empty() {
            return Ok(SlashOutcome::Forward(Action::Op(Op::Archive)));
        }
        let store = open_store(ctx.info)?;
        let live_id = ctx.info.session_id.clone();
        let prefix = prefix.to_owned();
        Ok(SlashOutcome::Background(Box::pin(async move {
            let info = resolve_saved_prefix_blocking(store, prefix, live_id)
                .await
                .map_err(|e| resolve_error_message(&e, Some(BARE_ARCHIVE_HINT)))?;
            Ok(apply_archived(&info, ArchiveState::Archived))
        })))
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

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn usage(&self) -> Option<&'static str> {
        Some("<id-prefix>")
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.is_empty()
    }

    fn execute(&self, prefix: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
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
        let store = open_store(ctx.info)?;
        let live_id = ctx.info.session_id.clone();
        let prefix = prefix.to_owned();
        Ok(SlashOutcome::Background(Box::pin(async move {
            match resolve_saved_prefix_blocking(store, prefix, live_id).await {
                Ok(info) => Ok(apply_archived(&info, ArchiveState::Active)),
                Err(ResolveError::Live { .. }) => Ok(SlashOutcome::SystemMessage(
                    "The live session is not archived".to_owned(),
                )),
                Err(e) => Err(resolve_error_message(&e, None)),
            }
        })))
    }
}

// ── Helpers ──

fn apply_archived(info: &SessionInfo, target: ArchiveState) -> SlashOutcome {
    if info.archive_state == target {
        let state = match target {
            ArchiveState::Active => "not archived",
            ArchiveState::Archived => "already archived",
        };
        return SlashOutcome::SystemMessage(format!(
            "Session {} is {state}",
            id_prefix(&info.session_id),
        ));
    }
    SlashOutcome::Forward(Action::Op(Op::SetArchive {
        path: info.path.clone(),
        session_id: info.session_id.clone(),
        state: target,
    }))
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use ox_core::session::store::SessionStore;
    use ox_core::session::store::testing::{
        WriterBarrier, archive_test_session, isolated_store, listed_ids, seed_test_session,
        stamped_id, with_isolated_xdg_async,
    };

    use super::*;
    use crate::session_info::testing::test_session_info;
    use crate::slash::registry::testing::{run_background, run_execute};

    fn seed_saved(store: &SessionStore, session_id: &str) {
        seed_test_session(
            store,
            session_id,
            Some("Saved"),
            Some(1),
            datetime!(2026-04-18 09:00:00 UTC),
        );
    }

    mod classify {
        use super::*;

        #[test]
        fn is_always_mutating() {
            assert_eq!(ArchiveCmd.classify(""), SlashKind::Mutating);
            assert_eq!(ArchiveCmd.classify("ab"), SlashKind::Mutating);
        }
    }

    mod execute {
        use super::*;

        #[test]
        fn bare_forwards_the_archive_action() {
            let outcome = run_execute(&ArchiveCmd, "").unwrap();
            assert_eq!(outcome.into_forward(), Action::Op(Op::Archive));
        }

        #[tokio::test]
        async fn prefix_forwards_the_archive_for_the_resolved_file() {
            with_isolated_xdg_async(|_| async {
                let store = open_store(&test_session_info()).unwrap();
                let target_id = stamped_id(0xab);
                seed_saved(&store, &target_id);

                let action = run_background(&ArchiveCmd, &target_id[..4])
                    .await
                    .unwrap()
                    .into_forward();

                assert_eq!(
                    action,
                    Action::Op(Op::SetArchive {
                        path: store.find_session_path(&target_id).unwrap(),
                        session_id: target_id,
                        state: ArchiveState::Archived,
                    }),
                );
            })
            .await;
        }

        #[tokio::test]
        async fn prefix_reports_a_session_that_is_already_archived() {
            with_isolated_xdg_async(|_| async {
                let store = open_store(&test_session_info()).unwrap();
                let target_id = stamped_id(0xab);
                seed_saved(&store, &target_id);
                archive_test_session(&store, &target_id);

                let note = run_background(&ArchiveCmd, &target_id[..4]).await.unwrap();

                assert_eq!(
                    note.into_system_message(),
                    "Session abababab is already archived"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn prefix_matching_only_the_live_session_points_to_bare_archive() {
            with_isolated_xdg_async(|_| async {
                let store = open_store(&test_session_info()).unwrap();
                seed_saved(&store, "test-session");

                let err = run_background(&ArchiveCmd, "test-").await.unwrap_err();
                assert_eq!(
                    err,
                    "Session prefix `test-` matches the live session. \
                     Use bare `/archive` to archive it and start a new one.",
                );
                assert_eq!(listed_ids(&store, ArchiveState::Active), ["test-session"]);
            })
            .await;
        }

        #[tokio::test]
        async fn unknown_prefix_reports_no_match() {
            with_isolated_xdg_async(|_| async {
                let err = run_background(&ArchiveCmd, "zzzz").await.unwrap_err();
                assert_eq!(err, "No session matching `zzzz`");
            })
            .await;
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

        mod classify {
            use super::*;

            #[test]
            fn is_mutating() {
                assert_eq!(UnarchiveCmd.classify("ab"), SlashKind::Mutating);
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

            #[tokio::test]
            async fn prefix_forwards_the_unarchive_for_the_resolved_file() {
                with_isolated_xdg_async(|_| async {
                    let store = open_store(&test_session_info()).unwrap();
                    let target_id = stamped_id(0xab);
                    seed_saved(&store, &target_id);
                    archive_test_session(&store, &target_id);

                    let action = run_background(&UnarchiveCmd, &target_id[..4])
                        .await
                        .unwrap()
                        .into_forward();

                    assert_eq!(
                        action,
                        Action::Op(Op::SetArchive {
                            path: store.find_session_path(&target_id).unwrap(),
                            session_id: target_id,
                            state: ArchiveState::Active,
                        }),
                    );
                })
                .await;
            }

            #[tokio::test]
            async fn prefix_reports_a_session_that_is_not_archived() {
                with_isolated_xdg_async(|_| async {
                    let store = open_store(&test_session_info()).unwrap();
                    let target_id = stamped_id(0xab);
                    seed_saved(&store, &target_id);

                    let note = run_background(&UnarchiveCmd, &target_id[..4])
                        .await
                        .unwrap();

                    assert_eq!(
                        note.into_system_message(),
                        "Session abababab is not archived"
                    );
                })
                .await;
            }

            #[tokio::test]
            async fn live_session_prefix_is_a_no_op_note() {
                with_isolated_xdg_async(|_| async {
                    let note = run_background(&UnarchiveCmd, "test-").await.unwrap();
                    assert_eq!(
                        note.into_system_message(),
                        "The live session is not archived"
                    );
                })
                .await;
            }

            #[tokio::test]
            async fn unknown_prefix_reports_no_match() {
                with_isolated_xdg_async(|_| async {
                    let err = run_background(&UnarchiveCmd, "zzzz").await.unwrap_err();
                    assert_eq!(err, "No session matching `zzzz`");
                })
                .await;
            }
        }
    }

    mod apply_archived {
        use super::*;

        #[test]
        fn forwards_the_saved_archive_without_waiting_for_the_writer() {
            let (_dir, store) = isolated_store();
            seed_saved(&store, "saved");
            let info = store.list().unwrap().into_iter().next().unwrap();
            let barrier = WriterBarrier::hold(&info.path);

            let outcome = apply_archived(&info, ArchiveState::Archived);

            assert_eq!(
                outcome.into_forward(),
                Action::Op(Op::SetArchive {
                    path: info.path,
                    session_id: "saved".to_owned(),
                    state: ArchiveState::Archived,
                })
            );
            assert_eq!(listed_ids(&store, ArchiveState::Active), ["saved"]);
            barrier.release();
        }
    }
}
