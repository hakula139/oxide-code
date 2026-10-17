//! `/archive` and `/unarchive` set whether a session shows in default listings and bare
//! `--continue`. Bare `/archive` forwards [`Op::Archive`], which archives the live session and
//! rolls to a fresh one. The typed-arg forms update a saved session's file directly.

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use super::sessions::{open_store, resolve_error_message};
use crate::protocol::Op;
use crate::session::display::{archive_notice, id_prefix};
use crate::session::entry::SessionInfo;
use crate::session::resolver::resolve_saved_prefix;
use crate::session::store::set_archived_at;
use crate::tui::action::Action;

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

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let prefix = args.trim();
        if prefix.is_empty() {
            return Ok(SlashOutcome::Forward(Action::Op(Op::Archive)));
        }
        match resolve_saved(prefix, &ctx.info.session_id)? {
            Some(info) => set_archived(&info, true),
            None => Err(format!(
                "Session prefix `{prefix}` matches the live session. Use bare `/archive` to archive \
                 it and start a new one.",
            )),
        }
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

    fn echoes_input(&self, args: &str) -> bool {
        !args.trim().is_empty()
    }

    fn usage(&self) -> Option<&'static str> {
        Some("<id-prefix>")
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let prefix = args.trim();
        if prefix.is_empty() {
            return Err(
                "Missing id. Use `/resume` then Ctrl+A to browse archived sessions, or \
                 `/unarchive <id-prefix>`."
                    .to_owned(),
            );
        }
        match resolve_saved(prefix, &ctx.info.session_id)? {
            Some(info) => set_archived(&info, false),
            None => Ok(SlashOutcome::SystemMessage(
                "The live session is not archived.".to_owned(),
            )),
        }
    }
}

// ── Helpers ──

fn resolve_saved(prefix: &str, live_id: &str) -> Result<Option<SessionInfo>, String> {
    let store = open_store()?;
    resolve_saved_prefix(&store, prefix, live_id).map_err(resolve_error_message)
}

fn set_archived(info: &SessionInfo, archived: bool) -> Result<SlashOutcome, String> {
    let id = id_prefix(&info.session_id);
    if info.archived == archived {
        let state = if archived {
            "already archived"
        } else {
            "not archived"
        };
        return Ok(SlashOutcome::SystemMessage(format!(
            "Session {id} is {state}."
        )));
    }
    set_archived_at(&info.path, archived)
        .map_err(|e| format!("Failed to update session {id}: {e:#}"))?;
    Ok(SlashOutcome::SystemMessage(archive_notice(
        &info.session_id,
        archived,
    )))
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::session::store::SessionStore;
    use crate::session::store::testing::seed_test_session;
    use crate::slash::{stamped_id, test_session_info, with_isolated_xdg};

    fn seed_saved(store: &SessionStore, session_id: &str) {
        seed_test_session(
            store,
            session_id,
            Some("Saved"),
            Some(1),
            datetime!(2026-04-18 09:00:00 UTC),
        );
    }

    fn listed_ids(store: &SessionStore) -> Vec<String> {
        let sessions = store.list().unwrap();
        sessions.into_iter().map(|s| s.session_id).collect()
    }

    // ── ArchiveCmd::name ──

    #[test]
    fn name_and_metadata_of_archive_match_built_ins_contract() {
        assert_eq!(ArchiveCmd.name(), "archive");
        assert_ne!(ArchiveCmd.description(), "");
        assert_eq!(ArchiveCmd.usage(), Some("[<id-prefix>]"));
    }

    // ── ArchiveCmd::classify ──

    #[test]
    fn classify_archive_is_always_mutating() {
        assert_eq!(ArchiveCmd.classify(""), SlashKind::Mutating);
        assert_eq!(ArchiveCmd.classify("ab"), SlashKind::Mutating);
    }

    // ── ArchiveCmd::execute ──

    #[test]
    fn execute_archive_bare_forwards_the_archive_action() {
        let info = test_session_info();
        let outcome = ArchiveCmd
            .execute("", &SlashContext::new(&info, None))
            .unwrap();
        assert_eq!(outcome.into_forward(), Action::Op(Op::Archive));
    }

    #[test]
    fn execute_archive_prefix_hides_a_saved_session_from_listings_once() {
        with_isolated_xdg(|_| {
            let store = SessionStore::open().unwrap();
            let target_id = stamped_id(0xab);
            seed_saved(&store, &target_id);
            let info = test_session_info();
            let ctx = SlashContext::new(&info, None);

            let note = ArchiveCmd.execute(&target_id[..4], &ctx).unwrap();
            assert_eq!(note.into_system_message(), archive_notice(&target_id, true));
            assert_eq!(listed_ids(&store), [] as [String; 0]);

            let again = ArchiveCmd.execute(&target_id[..4], &ctx).unwrap();
            assert_eq!(
                again.into_system_message(),
                "Session abababab is already archived."
            );
        });
    }

    #[test]
    fn execute_archive_prefix_matching_only_the_live_session_points_to_bare_archive() {
        with_isolated_xdg(|_| {
            let store = SessionStore::open().unwrap();
            seed_saved(&store, "test-session");
            let info = test_session_info();
            let ctx = SlashContext::new(&info, None);

            let err = ArchiveCmd.execute("test-", &ctx).unwrap_err();
            assert_eq!(
                err,
                "Session prefix `test-` matches the live session. Use bare `/archive` to archive \
                 it and start a new one.",
            );
            assert_eq!(listed_ids(&store), ["test-session"]);
        });
    }

    #[test]
    fn execute_archive_unknown_prefix_reports_no_match() {
        with_isolated_xdg(|_| {
            let info = test_session_info();
            let err = ArchiveCmd
                .execute("zzzz", &SlashContext::new(&info, None))
                .unwrap_err();
            assert_eq!(err, "No session matching `zzzz`.");
        });
    }

    #[cfg(unix)]
    #[test]
    fn execute_archive_prefix_reports_a_failed_write() {
        use std::os::unix::fs::PermissionsExt;

        with_isolated_xdg(|_| {
            let store = SessionStore::open().unwrap();
            let target_id = stamped_id(0xab);
            seed_saved(&store, &target_id);
            let path = store.find_session_path(&target_id).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
            let info = test_session_info();

            let err = ArchiveCmd
                .execute(&target_id[..4], &SlashContext::new(&info, None))
                .unwrap_err();
            let cause = set_archived_at(&path, true).unwrap_err();
            assert_eq!(err, format!("Failed to update session abababab: {cause:#}"));
        });
    }

    // ── UnarchiveCmd::name ──

    #[test]
    fn name_and_metadata_of_unarchive_match_built_ins_contract() {
        assert_eq!(UnarchiveCmd.name(), "unarchive");
        assert_ne!(UnarchiveCmd.description(), "");
        assert_eq!(UnarchiveCmd.usage(), Some("<id-prefix>"));
    }

    // ── UnarchiveCmd::classify ──

    #[test]
    fn classify_unarchive_is_mutating() {
        assert_eq!(UnarchiveCmd.classify("ab"), SlashKind::Mutating);
    }

    // ── UnarchiveCmd::echoes_input ──

    #[test]
    fn echoes_input_unarchive_only_with_a_prefix() {
        assert!(!UnarchiveCmd.echoes_input("  "), "bare form stands alone");
        assert!(UnarchiveCmd.echoes_input("ab"));
    }

    // ── UnarchiveCmd::execute ──

    #[test]
    fn execute_unarchive_bare_points_to_the_archived_picker() {
        let info = test_session_info();
        let err = UnarchiveCmd
            .execute("", &SlashContext::new(&info, None))
            .unwrap_err();
        assert_eq!(
            err,
            "Missing id. Use `/resume` then Ctrl+A to browse archived sessions, or \
             `/unarchive <id-prefix>`.",
        );
    }

    #[test]
    fn execute_unarchive_prefix_lists_an_archived_session_again_once() {
        with_isolated_xdg(|_| {
            let store = SessionStore::open().unwrap();
            let target_id = stamped_id(0xab);
            seed_saved(&store, &target_id);
            set_archived_at(&store.find_session_path(&target_id).unwrap(), true).unwrap();
            let info = test_session_info();
            let ctx = SlashContext::new(&info, None);

            let note = UnarchiveCmd.execute(&target_id[..4], &ctx).unwrap();
            assert_eq!(
                note.into_system_message(),
                archive_notice(&target_id, false)
            );
            assert_eq!(listed_ids(&store), [target_id.as_str()]);

            let again = UnarchiveCmd.execute(&target_id[..4], &ctx).unwrap();
            assert_eq!(
                again.into_system_message(),
                "Session abababab is not archived."
            );
        });
    }

    #[test]
    fn execute_unarchive_live_session_prefix_is_a_no_op_note() {
        with_isolated_xdg(|_| {
            let info = test_session_info();
            let note = UnarchiveCmd
                .execute("test-", &SlashContext::new(&info, None))
                .unwrap();
            assert_eq!(
                note.into_system_message(),
                "The live session is not archived."
            );
        });
    }
}
