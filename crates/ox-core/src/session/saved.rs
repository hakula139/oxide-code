//! Archive and delete for saved sessions, shared by the typed slash commands and the resume
//! picker.

use std::path::Path;

use tracing::warn;

use super::display::id_prefix;
use super::entry::ArchiveState;
use super::store::{delete_at, set_archived_at};

pub const BARE_ARCHIVE_HINT: &str = "Use bare `/archive` to archive it and start a new one.";

/// Why an operation on a saved session failed, where `id` is the session's [`id_prefix`]. Resume
/// refuses the live session through [`Self::Live`] too. `Display` is the lowercase `anyhow`
/// headline, which slash commands phrase as a sentence.
#[derive(Debug, thiserror::Error)]
pub enum SavedError {
    /// The target is the live session, which only a roll to a new session can leave.
    #[error("session {id} is the live session")]
    Live { id: String },
    /// The `action` (`archive`, `unarchive`, or `delete`) failed on the session's file.
    #[error("failed to {action} session {id}: {cause:#}")]
    Failed {
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
/// archive the live session, since only bare `/archive` can roll it to a new one. Blocks while
/// another writer owns the directory's lock, so async callers must use a blocking worker.
pub fn set_archived(
    path: &Path,
    session_id: &str,
    state: ArchiveState,
    live_id: &str,
) -> Result<(), SavedError> {
    let (action, archived) = match state {
        ArchiveState::Archived if session_id == live_id => {
            return Err(SavedError::live(session_id));
        }
        ArchiveState::Archived => ("archive", true),
        ArchiveState::Active => ("unarchive", false),
    };
    set_archived_at(path, archived).map_err(|e| SavedError::failed(action, session_id, e))
}

/// Deletes the session file at `path` and the session's artifact directory. Refuses the live
/// session's file, which callers already filter out, so a bug upstream cannot unlink the file under
/// its open writer.
pub fn delete(path: &Path, session_id: &str, live_id: &str) -> Result<(), SavedError> {
    if session_id == live_id {
        return Err(SavedError::live(session_id));
    }
    delete_at(path).map_err(|e| SavedError::failed("delete", session_id, e))
}

/// Phrases `error` as a sentence, ending a live-session refusal with `live_recovery` when given.
pub fn saved_error_message(error: SavedError, live_recovery: Option<&str>) -> String {
    match error {
        SavedError::Live { id } => {
            let live = format!("Session {id} is the live session");
            match live_recovery {
                Some(recovery) => format!("{live}. {recovery}"),
                None => live,
            }
        }
        SavedError::Failed { action, id, cause } => {
            format!("Failed to {action} session {id}: {cause:#}")
        }
    }
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

            let err = set_archived(
                &path,
                "live-session",
                ArchiveState::Archived,
                "live-session",
            )
            .unwrap_err();

            assert_eq!(err.to_string(), "session live-ses is the live session");
            assert_eq!(listed_ids(&store, ArchiveState::Active), ["live-session"]);

            set_archived(&path, "live-session", ArchiveState::Active, "live-session").unwrap();
        }

        #[test]
        fn names_the_action_the_session_and_the_cause_of_a_failed_write() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("missing.jsonl");

            let err = set_archived(
                &path,
                "missing-session",
                ArchiveState::Active,
                "live-session",
            )
            .unwrap_err();

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

    mod saved_error_message {
        use super::*;

        #[test]
        fn ends_a_live_refusal_with_the_recovery_and_capitalizes_a_failure() {
            let live = || SavedError::Live {
                id: "abababab".to_owned(),
            };
            assert_eq!(
                saved_error_message(live(), Some(BARE_ARCHIVE_HINT)),
                "Session abababab is the live session. \
                 Use bare `/archive` to archive it and start a new one.",
            );
            assert_eq!(
                saved_error_message(live(), None),
                "Session abababab is the live session",
            );

            let failed = SavedError::Failed {
                action: "unarchive",
                id: "abababab".to_owned(),
                cause: anyhow::anyhow!("disk unavailable"),
            };
            assert_eq!(
                saved_error_message(failed, None),
                "Failed to unarchive session abababab: disk unavailable",
            );
        }
    }
}
