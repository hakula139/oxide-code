//! Archive and delete for saved sessions, shared by the typed slash commands and the resume
//! picker.

use std::path::Path;

use tracing::warn;

use super::display::id_prefix;
use super::store::{delete_at, set_archived_at};

/// Why an archive or delete of a saved session failed. `Display` is the lowercase `anyhow`
/// headline, which slash commands phrase as a sentence.
#[derive(Debug, thiserror::Error)]
pub enum SavedError {
    /// The session is the live one, which only a roll to a new session can leave. `id` is its
    /// [`id_prefix`].
    #[error("session {id} is the live session")]
    Live { id: String },
    #[error("failed to {action} session {id}: {cause:#}")]
    Failed {
        /// The verb that failed: `archive`, `unarchive`, or `delete`.
        action: &'static str,
        id: String,
        cause: anyhow::Error,
    },
}

impl SavedError {
    pub(crate) fn live(session_id: &str) -> Self {
        Self::Live {
            id: id_prefix(session_id).to_owned(),
        }
    }

    fn failed(action: &'static str, session_id: &str, cause: anyhow::Error) -> Self {
        warn!("failed to {action} session {session_id}: {cause:#}");
        Self::Failed {
            action,
            id: id_prefix(session_id).to_owned(),
            cause,
        }
    }
}

/// Archives or unarchives the session file at `path`, keeping its last-active time. Refuses to
/// archive the live session, since only bare `/archive` can roll it to a new one.
pub fn set_archived(
    path: &Path,
    session_id: &str,
    archived: bool,
    live_id: &str,
) -> Result<(), SavedError> {
    if archived && session_id == live_id {
        return Err(SavedError::live(session_id));
    }
    let action = if archived { "archive" } else { "unarchive" };
    set_archived_at(path, archived).map_err(|e| SavedError::failed(action, session_id, e))
}

/// Deletes the session file at `path` and the session's artifact directory, refusing the live
/// session's file so a stale caller cannot unlink the file under its open writer.
pub fn delete(path: &Path, session_id: &str, live_id: &str) -> Result<(), SavedError> {
    if session_id == live_id {
        return Err(SavedError::live(session_id));
    }
    delete_at(path).map_err(|e| SavedError::failed("delete", session_id, e))
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

            assert_eq!(err.to_string(), "session live-ses is the live session");
            assert_eq!(listed_ids(&store, false), ["live-session"]);

            set_archived(&path, "live-session", false, "live-session").unwrap();
        }

        #[test]
        fn names_the_action_the_session_and_the_cause_of_a_failed_write() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("missing.jsonl");

            let err = set_archived(&path, "missing-session", false, "live-session").unwrap_err();

            let cause = set_archived_at(&path, false).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("failed to unarchive session missing-: {cause:#}")
            );
        }
    }

    mod delete {
        use super::*;

        #[test]
        fn refuses_the_live_session_and_keeps_its_file() {
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

            let err = delete(&path, "live-session", "live-session").unwrap_err();

            assert_eq!(err.to_string(), "session live-ses is the live session");
            assert!(path.exists());
        }

        #[test]
        fn names_the_session_and_the_cause_of_a_failed_delete() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("missing.jsonl");

            let err = delete(&path, "missing-session", "live-session").unwrap_err();

            let cause = delete_at(&path).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("failed to delete session missing-: {cause:#}")
            );
        }
    }
}
