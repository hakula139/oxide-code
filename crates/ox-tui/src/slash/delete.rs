//! `/delete <id-prefix>` deletes a saved session through the
//! [`ConfirmDeleteSessionModal`] that Ctrl+D in the `/resume` picker also opens. Bare `/delete`
//! names no session, so it errors.

use std::path::Path;

use ox_core::command;
use ox_core::session::display::{display_title, format_metadata_line};
use ox_core::session::entry::SessionInfo;
use ox_core::session::resolver::resolve_saved_prefix;
use ox_core::util::path::tildify;
use ox_core::util::time::local_offset;

use super::confirm::ConfirmDeleteSessionModal;
use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use super::sessions::{CLEAR_THEN_DELETE_HINT, open_store, resolve_error_message};

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

    fn echoes_input(&self, args: &str) -> bool {
        !args.trim().is_empty()
    }

    fn usage(&self) -> Option<&'static str> {
        Some("<id-prefix>")
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            return Err(command::missing(
                "session ID",
                "/delete <id-prefix>",
                Some("press Ctrl+D in the `/resume` picker"),
            ));
        }
        let store = open_store(ctx.info)?;
        let live_id = ctx.info.session_id.as_str();
        let info = resolve_saved_prefix(&store, arg, live_id)
            .map_err(|e| resolve_error_message(e, Some(CLEAR_THEN_DELETE_HINT)))?;
        let title = display_title(&info);
        let metadata = metadata_line(&info);
        Ok(SlashOutcome::Modal(Box::new(
            ConfirmDeleteSessionModal::new(
                info.path,
                info.session_id,
                title,
                metadata,
                live_id.to_owned(),
            ),
        )))
    }
}

// ── Display Formatting ──

/// Wraps [`format_metadata_line`] with the `/delete`-specific project rule: the confirm modal has
/// no scope context, so the project always shows when it is non-empty.
fn metadata_line(info: &SessionInfo) -> String {
    let project = tildify(Path::new(info.project()));
    format_metadata_line(
        &info.session_id,
        info.last_active_at,
        local_offset(),
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
    use ox_core::session::store::testing::seed_test_session;
    use ox_core::util::path::testing::HOME_VAR;

    use super::*;
    use crate::modal::Modal;
    use crate::modal::testing::render_rows;
    use crate::slash::registry::SlashCommand;
    use crate::slash::testing::{stamped_id, test_session_info, with_isolated_xdg};

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
            assert!(!DeleteCmd.echoes_input("   "), "whitespace counts as bare");
            assert!(DeleteCmd.echoes_input("abcd"));
        }
    }

    mod execute {
        use super::*;

        /// The title and metadata rows the confirm modal shows for the session it will delete.
        fn target_rows(modal: &dyn Modal) -> [String; 2] {
            let rows = render_rows(modal, 80, modal.height(80, u16::MAX));
            [rows[2].trim_end().to_owned(), rows[3].trim_end().to_owned()]
        }

        #[test]
        fn bare_points_to_the_picker_and_typed_form() {
            let info = test_session_info();
            let err = DeleteCmd
                .execute("", &SlashContext::new(&info, None))
                .unwrap_err();
            assert_eq!(
                err,
                "Missing session ID. \
                 Usage: `/delete <id-prefix>`, or press Ctrl+D in the `/resume` picker.",
            );
        }

        #[test]
        fn unknown_prefix_reports_no_match() {
            with_isolated_xdg(|_| {
                let info = test_session_info();
                let ctx = SlashContext::new(&info, None);
                let err = DeleteCmd.execute("zzz1234", &ctx).unwrap_err();
                assert_eq!(err, "No session matching `zzz1234`");
            });
        }

        #[test]
        fn typed_arg_unique_match_pushes_confirm_modal() {
            with_isolated_xdg(|_| {
                let store = open_store(&test_session_info()).unwrap();
                let target_id = stamped_id(0xab);
                seed_test_session(
                    &store,
                    &target_id,
                    Some("Doomed session"),
                    Some(3),
                    datetime!(2026-04-18 09:00:00 UTC),
                );
                let info = test_session_info();
                let ctx = SlashContext::new(&info, None);

                let modal = DeleteCmd
                    .execute(&target_id[..4], &ctx)
                    .unwrap()
                    .into_modal();

                let [title, metadata] = target_rows(&*modal);
                assert_eq!(title, "Doomed session");
                assert!(metadata.starts_with("abababab · "), "{metadata}");
            });
        }

        #[test]
        fn widens_to_other_projects_when_current_project_misses() {
            with_isolated_xdg(|dir| {
                let sessions_dir = dir.join("ox").join("sessions");
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
                let info = test_session_info();
                let ctx = SlashContext::new(&info, None);

                let modal = DeleteCmd
                    .execute(&target_id[..4], &ctx)
                    .unwrap()
                    .into_modal();

                let [title, metadata] = target_rows(&*modal);
                assert_eq!(title, "Foreign");
                assert!(metadata.starts_with("abababab · "), "{metadata}");
            });
        }

        #[test]
        fn confirming_deletes_the_file_the_prefix_matched() {
            // A renamed file keeps its header ID, so deleting by that ID would unlink whichever
            // file is named after it.
            with_isolated_xdg(|_| {
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
                let info = test_session_info();
                let ctx = SlashContext::new(&info, None);

                let mut modal = DeleteCmd.execute("renamed", &ctx).unwrap().into_modal();
                modal.handle_key(&crossterm::event::KeyEvent::from(
                    crossterm::event::KeyCode::Char('y'),
                ));

                assert!(!renamed.exists(), "the matched file is deleted");
                assert!(
                    bystander.exists(),
                    "the file named after the header ID survives"
                );
            });
        }

        #[test]
        fn with_live_id_prefix_returns_distinct_live_session_error() {
            // The resolver drops the live ID from its matches, so a prefix that matches only the
            // live session gets its own explanation.
            with_isolated_xdg(|_| {
                let store = open_store(&test_session_info()).unwrap();
                let live_id = "test-session";
                seed_test_session(
                    &store,
                    live_id,
                    Some("Live"),
                    Some(1),
                    datetime!(2026-04-18 09:00:00 UTC),
                );
                let info = test_session_info();
                let ctx = SlashContext::new(&info, None);

                let err = DeleteCmd.execute(live_id, &ctx).unwrap_err();
                assert_eq!(
                    err,
                    "Session prefix `test-session` matches the live session. \
                     Run `/clear` to start a new one, then delete it.",
                );
            });
        }

        #[test]
        fn with_live_id_prefix_returns_other_match_when_one_exists() {
            with_isolated_xdg(|_| {
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
                let info = test_session_info();
                let ctx = SlashContext::new(&info, None);

                let modal = DeleteCmd.execute("test-", &ctx).unwrap().into_modal();

                let [title, metadata] = target_rows(&*modal);
                assert_eq!(title, "Other");
                assert!(metadata.starts_with("test-oth · "), "{metadata}");
            });
        }

        #[test]
        fn ambiguous_prefix_lists_short_ids_and_counts_the_rest() {
            with_isolated_xdg(|_| {
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
                let info = test_session_info();
                let ctx = SlashContext::new(&info, None);

                let err = DeleteCmd.execute("aaaa", &ctx).unwrap_err();
                assert_eq!(
                    err,
                    "Ambiguous prefix `aaaa` matches 6 sessions: \
                     aaaaa5a5, aaaaa4a4, aaaaa3a3, aaaaa2a2, aaaaa1a1, and 1 more",
                );
            });
        }
    }

    mod metadata_line {
        use super::*;

        #[test]
        fn shows_the_count_branch_and_tildified_project() {
            let last_active_at = datetime!(2026-05-08 09:00:00 UTC);
            let info = SessionInfo {
                session_id: stamped_id(0xab),
                path: std::path::PathBuf::new(),
                cwd: "/work/oxide".to_owned(),
                project_root: None,
                last_active_at,
                title: None,
                exit: Some(ExitInfo {
                    message_count: 14,
                    updated_at: last_active_at,
                }),
                git_branch: Some("feat/login".to_owned()),
                archived: false,
            };

            let line = temp_env::with_var(HOME_VAR, Some("/work"), || metadata_line(&info));

            assert_eq!(
                line,
                "abababab · 2026-05-08 · 14 msgs · feat/login · ~/oxide"
            );
        }

        #[test]
        fn omits_project_when_cwd_is_empty() {
            let info = SessionInfo {
                session_id: stamped_id(0xab),
                path: std::path::PathBuf::new(),
                cwd: String::new(),
                project_root: None,
                last_active_at: datetime!(2026-05-08 09:00:00 UTC),
                title: None,
                exit: None,
                git_branch: None,
                archived: false,
            };
            let line = metadata_line(&info);
            assert!(!line.ends_with(" · "), "no trailing separator: {line}");
            assert!(!line.contains(" ·  · "), "no double separator: {line}");
        }
    }
}
