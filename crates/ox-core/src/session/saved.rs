//! Archive and delete for saved sessions, shared by the typed slash commands and the resume
//! picker. Each failure is a message ready to show the user.

use std::path::Path;

use tracing::warn;

use super::display::id_prefix;
use super::store::{delete_at, set_archived_at};

/// Ends each refusal to archive the live session, which has to roll to a new one.
pub const BARE_ARCHIVE_HINT: &str = "Use bare `/archive` to archive it and start a new one.";

/// Archives or unarchives the session file at `path`, keeping its last-active time. Refuses to
/// archive the live session, since only bare `/archive` can roll it to a new one.
pub fn set_archived(
    path: &Path,
    session_id: &str,
    archived: bool,
    live_id: &str,
) -> Result<(), String> {
    let id = id_prefix(session_id);
    if archived && session_id == live_id {
        return Err(format!(
            "Session {id} is the live session. {BARE_ARCHIVE_HINT}"
        ));
    }
    set_archived_at(path, archived).map_err(|e| {
        warn!("archive toggle failed for session {session_id}: {e:#}");
        format!("Failed to update session {id}: {e:#}")
    })
}

/// Deletes the session file at `path` and the session's artifact directory, refusing the live
/// session's file.
pub fn delete(path: &Path, session_id: &str, live_id: &str) -> Result<(), String> {
    delete_at(path, live_id).map_err(|e| {
        warn!("delete failed for session {session_id}: {e:#}");
        format!("Failed to delete session {}: {e:#}", id_prefix(session_id))
    })
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::session::store::local_testing::test_session_file;
    use crate::session::store::testing::{listed_ids, seed_test_session, test_store};

    mod set_archived {
        use super::*;

        #[test]
        fn refuses_only_to_archive_the_live_session() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            seed_test_session(
                &store,
                "live-session",
                Some("Live"),
                Some(1),
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let path = test_session_file(dir.path(), "live-session");

            let err = set_archived(&path, "live-session", true, "live-session").unwrap_err();

            assert_eq!(
                err,
                "Session live-ses is the live session. \
                 Use bare `/archive` to archive it and start a new one.",
            );
            assert_eq!(listed_ids(&store, false), ["live-session"]);

            set_archived(&path, "live-session", false, "live-session").unwrap();
        }

        #[test]
        fn names_the_session_and_the_cause_of_a_failed_write() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("missing.jsonl");

            let err = set_archived(&path, "missing-session", true, "live-session").unwrap_err();

            let cause = set_archived_at(&path, true).unwrap_err();
            assert_eq!(err, format!("Failed to update session missing-: {cause:#}"));
        }
    }
}
