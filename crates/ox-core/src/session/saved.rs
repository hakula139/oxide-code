//! Archive and delete for saved sessions, shared by the typed slash commands and the resume
//! picker.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::display::id_prefix;
use super::entry::ArchiveState;
use super::store::{delete_at, set_archived_at};

/// Why an operation on a saved session failed, where `id` is the session's [`id_prefix`]. Resume
/// refuses the live session through [`Self::Live`] too. `Display` is the lowercase `anyhow`
/// headline, which each frontend phrases as a sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum SavedError {
    /// The target is the live session, which only a roll to a new session can leave.
    #[error("session {id} is the live session")]
    Live { id: String },
    /// The action failed on the session's file.
    #[error("failed to {action} session {id}: {cause}")]
    Failed {
        action: SavedAction,
        id: String,
        cause: String,
    },
    /// The blocking task that performs a saved-session operation failed.
    #[error("session operation task failed: {0}")]
    Task(String),
}

/// The saved-session write that can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum SavedAction {
    Archive,
    Unarchive,
    Delete,
}

impl SavedError {
    pub(crate) fn live(session_id: &str) -> Self {
        Self::Live {
            id: id_prefix(session_id).to_owned(),
        }
    }

    fn failed(action: SavedAction, session_id: &str, cause: &anyhow::Error) -> Self {
        let cause = format!("{cause:#}");
        warn!(
            action = %action,
            session_id,
            error = %cause,
            "session operation failed"
        );
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
        ArchiveState::Archived => (SavedAction::Archive, true),
        ArchiveState::Active => (SavedAction::Unarchive, false),
    };
    set_archived_at(path, archived).map_err(|e| SavedError::failed(action, session_id, &e))
}

/// Deletes the session file at `path` and the session's artifact directory. Refuses the live
/// session's file, which callers already filter out, so a bug upstream cannot unlink the file under
/// its open writer.
pub fn delete(path: &Path, session_id: &str, live_id: &str) -> Result<(), SavedError> {
    if session_id == live_id {
        return Err(SavedError::live(session_id));
    }
    delete_at(path).map_err(|e| SavedError::failed(SavedAction::Delete, session_id, &e))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use time::macros::datetime;

    use super::*;
    use crate::session::store::local_testing::test_session_file;
    use crate::session::store::testing::{listed_ids, seed_test_session, test_store};
    use crate::util::log::testing::LogCapture;

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
            let logs = LogCapture::default();

            let err = tracing::dispatcher::with_default(&logs.dispatch(), || {
                delete(&path, "missing-session", "live-session")
            })
            .unwrap_err();

            let cause = delete_at(&path).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("failed to delete session missing-: {cause:#}")
            );
            assert_eq!(
                logs.events(),
                [BTreeMap::from([
                    ("action".to_owned(), "delete".to_owned()),
                    ("session_id".to_owned(), "missing-session".to_owned()),
                    ("error".to_owned(), format!("{cause:#}")),
                    ("message".to_owned(), "session operation failed".to_owned()),
                ])]
            );
        }
    }
}
