//! `/delete <id-prefix>` deletes a saved session through the
//! [`ConfirmDeleteSessionModal`] that Ctrl+D in the `/resume` picker also opens. Bare `/delete`
//! names no session, so it errors.

use std::path::Path;

use super::confirm::ConfirmDeleteSessionModal;
use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use super::sessions::{open_store, resolve_error_message};
use crate::session::display::{display_title, format_metadata_line};
use crate::session::entry::SessionInfo;
use crate::session::resolver::resolve_saved_prefix;
use crate::util::path::tildify;

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
            return Err("Missing session ID. \
                        Usage: `/delete <id-prefix>`, or press Ctrl+D in the `/resume` picker."
                .to_owned());
        }
        let store = open_store(ctx.info)?;
        let live_id = ctx.info.session_id.as_str();
        let info = resolve_saved_prefix(&store, arg, live_id)
            .map_err(resolve_error_message)?
            .ok_or_else(|| {
                format!(
                    "Session prefix `{arg}` matches the live session. \
                     Run `/clear` to start a new one, then delete it."
                )
            })?;
        Ok(SlashOutcome::Modal(Box::new(
            ConfirmDeleteSessionModal::new(
                info.path.clone(),
                info.session_id.clone(),
                display_title(&info),
                metadata_line(&info),
                live_id.to_owned(),
            ),
        )))
    }
}

// ── Display Formatting ──

/// Wraps [`format_metadata_line`] with the `/delete`-specific project rule: the confirm modal has
/// no scope context, so the project always shows when `cwd` is non-empty.
fn metadata_line(info: &SessionInfo) -> String {
    let project = tildify(Path::new(&info.cwd));
    format_metadata_line(
        &info.session_id,
        info.last_active_at,
        crate::util::time::local_offset(),
        info.exit.as_ref().map_or(0, |e| e.message_count),
        info.git_branch.as_deref(),
        (!project.is_empty()).then_some(project.as_str()),
    )
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::session::store::SessionStore;
    use crate::session::store::testing::seed_test_session;
    use crate::slash::registry::SlashCommand;
    use crate::slash::testing::{stamped_id, test_session_info, with_isolated_xdg};

    // ── DeleteCmd::name ──

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(DeleteCmd.name(), "delete");
        assert_eq!(DeleteCmd.aliases(), [] as [&str; 0]);
        assert_ne!(DeleteCmd.description(), "");
        assert_eq!(DeleteCmd.usage(), Some("<id-prefix>"));
    }

    // ── DeleteCmd::classify ──

    #[test]
    fn classify_is_always_mutating() {
        assert_eq!(DeleteCmd.classify(""), SlashKind::Mutating);
        assert_eq!(DeleteCmd.classify("ab"), SlashKind::Mutating);
    }

    // ── DeleteCmd::echoes_input ──

    #[test]
    fn echoes_input_suppressed_on_bare_form_and_active_on_typed_arg() {
        // The bare form's error block stands alone, while the typed form keeps its line above the
        // confirm modal so the user can re-read the target.
        assert!(!DeleteCmd.echoes_input(""));
        assert!(!DeleteCmd.echoes_input("   "), "whitespace counts as bare");
        assert!(DeleteCmd.echoes_input("abcd"));
    }

    // ── DeleteCmd::execute ──

    #[test]
    fn execute_bare_points_to_the_picker_and_typed_form() {
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
    fn execute_unknown_prefix_reports_no_match() {
        with_isolated_xdg(|_| {
            let info = test_session_info();
            let ctx = SlashContext::new(&info, None);
            let err = DeleteCmd.execute("zzz1234", &ctx).unwrap_err();
            assert_eq!(err, "No session matching `zzz1234`");
        });
    }

    #[test]
    fn execute_typed_arg_unique_match_pushes_confirm_modal() {
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

            DeleteCmd
                .execute(&target_id[..4], &ctx)
                .unwrap()
                .into_modal();
        });
    }

    #[test]
    fn execute_widens_to_other_projects_when_current_project_misses() {
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

            DeleteCmd
                .execute(&target_id[..4], &ctx)
                .unwrap()
                .into_modal();
        });
    }

    #[test]
    fn execute_then_confirm_deletes_the_file_the_prefix_matched() {
        // A renamed file keeps its header ID, so deleting by that ID would unlink whichever file
        // is named after it.
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
    fn execute_with_live_id_prefix_returns_distinct_live_session_error() {
        // The resolver drops the live ID from its matches, so a prefix that matches only the live
        // session gets its own explanation.
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
    fn execute_with_live_id_prefix_returns_other_match_when_one_exists() {
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

            DeleteCmd.execute("test-", &ctx).unwrap().into_modal();
        });
    }

    #[test]
    fn execute_ambiguous_prefix_lists_short_ids_and_counts_the_rest() {
        with_isolated_xdg(|_| {
            let store = open_store(&test_session_info()).unwrap();
            for byte in 0xa0..=0xa5 {
                seed_test_session(
                    &store,
                    &format!("aaaa{}", &stamped_id(byte)[4..]),
                    Some("t"),
                    Some(1),
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(byte)),
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

    // ── metadata_line ──

    #[test]
    fn metadata_line_includes_tildified_project_when_cwd_is_non_empty() {
        let info = SessionInfo {
            session_id: stamped_id(0xab),
            path: std::path::PathBuf::new(),
            cwd: "/work/oxide".to_owned(),
            last_active_at: datetime!(2026-05-08 09:00:00 UTC),
            title: None,
            exit: None,
            git_branch: None,
            archived: false,
        };
        let line = metadata_line(&info);
        assert!(line.contains("/work/oxide"), "project shown: {line}");
    }

    #[test]
    fn metadata_line_omits_project_when_cwd_is_empty() {
        let info = SessionInfo {
            session_id: stamped_id(0xab),
            path: std::path::PathBuf::new(),
            cwd: String::new(),
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
