//! `/delete <id-prefix>` deletes a saved session through the
//! [`ConfirmDeleteSessionModal`] that Ctrl+D in the `/resume` picker also opens. Bare `/delete`
//! names no session, so it errors.

use std::path::Path;

use time::OffsetDateTime;

use ox_core::command;
use ox_core::session::display::display_title;
use ox_core::session::entry::SessionInfo;
use ox_core::session::resolver::resolve_saved_prefix_blocking;
use ox_core::util::path::tildify;
use ox_core::util::time::now_local;

use super::confirm::ConfirmDeleteSessionModal;
use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use super::sessions::{
    CLEAR_THEN_DELETE_HINT, format_metadata_line, open_store, resolve_error_message,
};

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
        // Mutating keeps both forms idle-gated, since the typed-arg form deletes a session file.
        SlashKind::Mutating
    }

    fn usage(&self) -> Option<&'static str> {
        Some("<id-prefix>")
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.is_empty()
    }

    fn execute(&self, arg: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        if arg.is_empty() {
            return Err(command::missing(
                "session ID",
                "/delete <id-prefix>",
                Some("press Ctrl+D in the `/resume` picker"),
            ));
        }
        let store = open_store(ctx.info)?;
        let live_id = ctx.info.session_id.clone();
        let arg = arg.to_owned();
        Ok(SlashOutcome::Background(Box::pin(async move {
            let info = resolve_saved_prefix_blocking(store, arg, live_id.clone())
                .await
                .map_err(|e| resolve_error_message(&e, Some(CLEAR_THEN_DELETE_HINT)))?;
            let title = display_title(&info).to_owned();
            let metadata = metadata_line(&info, now_local());
            Ok(SlashOutcome::Modal(Box::new(
                ConfirmDeleteSessionModal::new(
                    info.path,
                    info.session_id,
                    title,
                    metadata,
                    live_id,
                ),
            )))
        })))
    }
}

// ── Display Formatting ──

/// Wraps [`format_metadata_line`] with the `/delete`-specific project rule: the confirm modal has
/// no scope context, so the project always shows when it is non-empty.
fn metadata_line(info: &SessionInfo, now: OffsetDateTime) -> String {
    let project = tildify(Path::new(info.project()));
    format_metadata_line(
        &info.session_id,
        info.last_active_at,
        now,
        info.exit.as_ref().map_or(0, |e| e.message_count),
        info.git_branch.as_deref(),
        (!project.is_empty()).then_some(project.as_str()),
    )
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use ox_core::session::entry::ExitInfo;
    use ox_core::session::store::SessionStore;
    use ox_core::session::store::testing::{
        listed_session, seed_test_session, stamped_id, with_isolated_xdg_async,
    };
    use ox_core::util::path::testing::HOME_VAR;

    use super::*;
    use crate::modal::Modal;
    use crate::modal::testing::render_rows;
    use crate::session_info::testing::test_session_info;
    use crate::slash::registry::testing::run_background;

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(DeleteCmd.name(), "delete");
        assert_ne!(DeleteCmd.description(), "");
        assert_eq!(DeleteCmd.usage(), Some("<id-prefix>"));
    }

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

        /// Runs `/delete` and awaits the background lookup behind its confirm modal.
        async fn run_execute(args: &str) -> Result<SlashOutcome, String> {
            run_background(&DeleteCmd, args).await
        }

        /// The title and metadata rows the confirm modal shows for the session it will delete.
        fn target_rows(modal: &dyn Modal) -> [String; 2] {
            let rows = render_rows(modal, 80, modal.height(80, u16::MAX));
            [rows[2].trim_end().to_owned(), rows[3].trim_end().to_owned()]
        }

        #[tokio::test]
        async fn bare_points_to_the_picker_and_typed_form() {
            let err = run_execute("").await.unwrap_err();
            assert_eq!(
                err,
                "Missing session ID. \
                 Usage: `/delete <id-prefix>`, or press Ctrl+D in the `/resume` picker.",
            );
        }

        #[tokio::test]
        async fn unknown_prefix_reports_no_match() {
            with_isolated_xdg_async(|_| async {
                let err = run_execute("zzz1234").await.unwrap_err();
                assert_eq!(err, "No session matching `zzz1234`");
            })
            .await;
        }

        #[tokio::test]
        async fn typed_arg_unique_match_pushes_confirm_modal() {
            with_isolated_xdg_async(|_| async {
                let store = open_store(&test_session_info()).unwrap();
                let target_id = stamped_id(0xab);
                seed_test_session(
                    &store,
                    &target_id,
                    Some("Doomed session"),
                    Some(3),
                    datetime!(2026-04-18 09:00:00 UTC),
                );

                let modal = run_execute(&target_id[..4]).await.unwrap().into_modal();

                let [title, metadata] = target_rows(&*modal);
                assert_eq!(title, "Doomed session");
                assert!(metadata.starts_with("abababab · "), "{metadata}");
            })
            .await;
        }

        #[tokio::test]
        async fn widens_to_other_projects_when_current_project_misses() {
            with_isolated_xdg_async(|dir| {
                let sessions_dir = dir.join("ox").join("sessions");
                async move {
                    std::fs::create_dir_all(&sessions_dir).unwrap();
                    let other = SessionStore::open_at(sessions_dir, "other-project").unwrap();
                    let target_id = stamped_id(0xab);
                    seed_test_session(
                        &other,
                        &target_id,
                        Some("Foreign"),
                        Some(1),
                        datetime!(2026-04-18 09:00:00 UTC),
                    );

                    let modal = run_execute(&target_id[..4]).await.unwrap().into_modal();

                    let [title, metadata] = target_rows(&*modal);
                    assert_eq!(title, "Foreign");
                    assert!(metadata.starts_with("abababab · "), "{metadata}");
                }
            })
            .await;
        }

        #[tokio::test(start_paused = true)]
        async fn confirming_deletes_the_file_the_prefix_matched() {
            // A renamed file keeps its header ID, so deleting by that ID would unlink whichever
            // file is named after it.
            let dir = tempfile::tempdir().unwrap();
            let (mut modal, renamed, bystander) = temp_env::async_with_vars(
                [("XDG_DATA_HOME", Some(dir.path().as_os_str()))],
                async {
                    let store = open_store(&test_session_info()).unwrap();
                    let header_id = stamped_id(0xab);
                    seed_test_session(
                        &store,
                        &header_id,
                        Some("Renamed"),
                        Some(3),
                        datetime!(2026-04-18 09:00:00 UTC),
                    );
                    let original = store.find_session_path(&header_id).unwrap();
                    let renamed = original.with_file_name("1776502800-renamed.jsonl");
                    std::fs::rename(&original, &renamed).unwrap();
                    seed_test_session(
                        &store,
                        &header_id,
                        Some("Bystander"),
                        Some(1),
                        datetime!(2026-04-18 09:00:05 UTC),
                    );
                    let bystander = store.find_session_path(&header_id).unwrap();

                    let modal = run_execute("renamed").await.unwrap().into_modal();
                    (modal, renamed, bystander)
                },
            )
            .await;
            target_rows(&*modal);
            tokio::time::advance(crate::modal::acceptance::TYPE_AHEAD_GUARD).await;

            let outcome = modal.handle_key(&crossterm::event::KeyEvent::from(
                crossterm::event::KeyCode::Char('y'),
            ));

            assert!(
                matches!(outcome, crate::modal::ModalKey::Submitted(crate::modal::ModalAction::SystemMessage(ref text))
                    if text == "Deleted session abababab: Renamed"),
                "{outcome:?}"
            );
            assert!(!renamed.exists(), "the matched file is deleted");
            assert!(
                bystander.exists(),
                "the file named after the header ID survives"
            );
        }

        #[tokio::test]
        async fn with_live_id_prefix_explains_the_live_session() {
            // The resolver drops the live ID from its matches, so a prefix that matches only the
            // live session gets its own explanation.
            with_isolated_xdg_async(|_| async {
                let store = open_store(&test_session_info()).unwrap();
                let live_id = "test-session";
                seed_test_session(
                    &store,
                    live_id,
                    Some("Live"),
                    Some(1),
                    datetime!(2026-04-18 09:00:00 UTC),
                );

                let err = run_execute(live_id).await.unwrap_err();
                assert_eq!(
                    err,
                    "Session prefix `test-session` matches the live session. \
                     Run `/clear` to start a new one, then delete it.",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn with_live_id_prefix_targets_the_other_match() {
            with_isolated_xdg_async(|_| async {
                let store = open_store(&test_session_info()).unwrap();
                let live_id = "test-session";
                seed_test_session(
                    &store,
                    live_id,
                    Some("Live"),
                    Some(1),
                    datetime!(2026-04-18 09:00:00 UTC),
                );
                let other_id = format!("test-other-{}", &stamped_id(0xab)[10..]);
                seed_test_session(
                    &store,
                    &other_id,
                    Some("Other"),
                    Some(1),
                    datetime!(2026-04-18 09:01:00 UTC),
                );

                let modal = run_execute("test-").await.unwrap().into_modal();

                let [title, metadata] = target_rows(&*modal);
                assert_eq!(title, "Other");
                assert!(metadata.starts_with("test-oth · "), "{metadata}");
            })
            .await;
        }

        #[tokio::test]
        async fn ambiguous_prefix_lists_short_ids_and_counts_the_rest() {
            with_isolated_xdg_async(|_| async {
                let store = open_store(&test_session_info()).unwrap();
                for byte in 0xa0..=0xa5 {
                    seed_test_session(
                        &store,
                        &format!("aaaa{}", &stamped_id(byte)[4..]),
                        Some("t"),
                        Some(1),
                        datetime!(2026-04-18 09:00:00 UTC)
                            + time::Duration::seconds(i64::from(byte)),
                    );
                }

                let err = run_execute("aaaa").await.unwrap_err();
                assert_eq!(
                    err,
                    "Ambiguous prefix `aaaa` matches 6 sessions: \
                     aaaaa5a5, aaaaa4a4, aaaaa3a3, aaaaa2a2, aaaaa1a1, and 1 more",
                );
            })
            .await;
        }
    }

    mod metadata_line {
        use super::*;

        const NOW: time::OffsetDateTime = datetime!(2026-04-18 12:00:00 UTC);

        #[test]
        fn shows_the_count_branch_and_tildified_project() {
            let info = SessionInfo {
                cwd: "/work/oxide".to_owned(),
                exit: Some(ExitInfo {
                    message_count: 14,
                    updated_at: NOW,
                }),
                git_branch: Some("feat/login".to_owned()),
                ..listed_session(&stamped_id(0xab))
            };

            let line = temp_env::with_var(HOME_VAR, Some("/work"), || metadata_line(&info, NOW));

            assert_eq!(
                line,
                "abababab · 3 hours ago · 14 msgs · feat/login · ~/oxide"
            );
        }

        #[test]
        fn omits_project_when_cwd_is_empty() {
            let info = SessionInfo {
                cwd: String::new(),
                ..listed_session(&stamped_id(0xab))
            };

            assert_eq!(metadata_line(&info, NOW), "abababab · 3 hours ago");
        }
    }
}
