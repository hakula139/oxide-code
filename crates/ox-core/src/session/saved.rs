//! Lookup, archive, and delete for saved sessions by ID, which the engine runs for the typed slash
//! commands and the resume picker.

use tracing::warn;

use super::display::id_prefix;
use super::entry::{ArchiveState, SessionInfo};
use super::resolver::{ResolveError, resolve_id, resolve_prefix};
use super::store::SessionStore;
use super::store::listing::ProjectScope;
use crate::protocol::SavedTarget;

/// Why an operation on a saved session failed, where `id` is the session's [`id_prefix`]. Resume
/// refuses the live session through [`Self::Live`] too. `Display` is the lowercase `anyhow`
/// headline, which each frontend phrases as a sentence.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SavedError {
    /// The target resolved to no single saved session.
    #[error(transparent)]
    Resolve(#[from] ResolveError),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum SavedAction {
    Archive,
    Unarchive,
    Delete,
}

impl SavedError {
    /// Returns whether the operation refused the live session, whether it named the session by its
    /// ID or by a prefix that matched only it.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            Self::Live { .. } | Self::Resolve(ResolveError::Live { .. })
        )
    }

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

/// The saved session an archive operation resolved, and whether the operation changed its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveUpdate {
    pub id: String,
    /// Whether the session was in the other state, so the operation wrote the change.
    pub changed: bool,
}

/// The one saved session other than `live_id` whose ID starts with `prefix`, in the current project
/// first and then in every project.
pub(crate) fn find(
    store: &SessionStore,
    prefix: &str,
    live_id: &str,
) -> Result<SessionInfo, SavedError> {
    Ok(resolve_prefix(
        store,
        prefix,
        Some(live_id),
        ProjectScope::Current,
    )?)
}

/// Archives or unarchives the saved session `target` names, keeping its last-active time, and
/// leaves a session already in `state` alone. Refuses the live session, since only bare `/archive`
/// can roll it to a new one and resuming it already unarchived it. Blocks while another writer owns
/// the session's lock, so async callers must use a blocking worker.
pub(crate) fn set_archived(
    store: &SessionStore,
    target: &SavedTarget,
    state: ArchiveState,
    live_id: &str,
) -> Result<ArchiveUpdate, SavedError> {
    let info = match target {
        SavedTarget::Id(id) if id == live_id => return Err(SavedError::live(id)),
        SavedTarget::Id(id) => resolve_id(store, id)?,
        SavedTarget::Prefix(prefix) => find(store, prefix, live_id)?,
    };
    let changed = info.archive_state != state;
    if changed {
        let action = match state {
            ArchiveState::Archived => SavedAction::Archive,
            ArchiveState::Active => SavedAction::Unarchive,
        };
        store
            .set_archived(&info.session_id, state)
            .map_err(|e| SavedError::failed(action, &info.session_id, &e))?;
    }
    Ok(ArchiveUpdate {
        id: info.session_id,
        changed,
    })
}

/// Deletes saved session `session_id` and its artifact directory. Refuses the live session, so a
/// stale target cannot unlink the file under its open writer.
pub(crate) fn delete(
    store: &SessionStore,
    session_id: &str,
    live_id: &str,
) -> Result<(), SavedError> {
    if session_id == live_id {
        return Err(SavedError::live(session_id));
    }
    store
        .delete(session_id)
        .map_err(|e| SavedError::failed(SavedAction::Delete, session_id, &e))
}

// ── Testing ──

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::{ArchiveState, ArchiveUpdate, SavedError, SavedTarget, SessionStore};

    /// [`super::set_archived`] for a frontend test that stands in for the engine.
    pub fn set_archived(
        store: &SessionStore,
        target: &SavedTarget,
        state: ArchiveState,
        live_id: &str,
    ) -> Result<ArchiveUpdate, SavedError> {
        super::set_archived(store, target, state, live_id)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use time::macros::datetime;

    use super::*;
    use crate::session::store::local_testing::test_session_file;
    use crate::session::store::testing::{listed_ids, seed_test_session, test_store};
    use crate::util::fs::testing::make_read_only;
    use crate::util::log::testing::LogCapture;

    const LIVE_ID: &str = "live-session";
    const SAVED_ID: &str = "saved-session";

    /// A store holding the live session and one saved session, both unarchived.
    fn seeded_store(dir: &std::path::Path) -> SessionStore {
        let store = test_store(dir);
        for (id, title) in [(LIVE_ID, "Live"), (SAVED_ID, "Saved")] {
            seed_test_session(
                &store,
                id,
                Some(title),
                Some(1),
                datetime!(2026-04-18 09:00:00 UTC),
            );
        }
        store
    }

    mod find {
        use super::*;

        #[test]
        fn resolves_a_saved_prefix_and_refuses_the_live_session() {
            let dir = tempfile::tempdir().unwrap();
            let store = seeded_store(dir.path());

            assert_eq!(find(&store, "saved", LIVE_ID).unwrap().session_id, SAVED_ID);
            assert!(find(&store, "live", LIVE_ID).unwrap_err().is_live());
        }
    }

    mod set_archived {
        use super::*;

        #[test]
        fn resolves_the_target_and_writes_only_a_change() {
            let dir = tempfile::tempdir().unwrap();
            let store = seeded_store(dir.path());
            let file = test_session_file(dir.path(), SAVED_ID);

            for (target, state, changed) in [
                (
                    SavedTarget::Id(SAVED_ID.to_owned()),
                    ArchiveState::Archived,
                    true,
                ),
                (
                    SavedTarget::Prefix("saved".to_owned()),
                    ArchiveState::Archived,
                    false,
                ),
                (
                    SavedTarget::Prefix("saved".to_owned()),
                    ArchiveState::Active,
                    true,
                ),
                (
                    SavedTarget::Id(SAVED_ID.to_owned()),
                    ArchiveState::Active,
                    false,
                ),
            ] {
                let len = fs::metadata(&file).unwrap().len();

                let update = set_archived(&store, &target, state, LIVE_ID);

                let case = format!("{target:?} to {state:?}");
                assert_eq!(
                    update,
                    Ok(ArchiveUpdate {
                        id: SAVED_ID.to_owned(),
                        changed,
                    }),
                    "{case}",
                );
                assert_eq!(fs::metadata(&file).unwrap().len() > len, changed, "{case}");
                let expected: &[&str] = match state {
                    ArchiveState::Archived => &[SAVED_ID],
                    ArchiveState::Active => &[],
                };
                assert_eq!(
                    listed_ids(&store, ArchiveState::Archived),
                    expected,
                    "{case}"
                );
            }
        }

        #[test]
        fn refuses_the_live_session_and_names_an_unmatched_target() {
            let dir = tempfile::tempdir().unwrap();
            let store = seeded_store(dir.path());

            for (target, expected) in [
                (
                    SavedTarget::Id(LIVE_ID.to_owned()),
                    SavedError::Live {
                        id: "live-ses".to_owned(),
                    },
                ),
                (
                    SavedTarget::Prefix("live".to_owned()),
                    SavedError::Resolve(ResolveError::Live {
                        prefix: "live".to_owned(),
                    }),
                ),
                (
                    SavedTarget::Id("saved".to_owned()),
                    SavedError::Resolve(ResolveError::NoMatch {
                        prefix: "saved".to_owned(),
                    }),
                ),
                (
                    SavedTarget::Prefix("zzz".to_owned()),
                    SavedError::Resolve(ResolveError::NoMatch {
                        prefix: "zzz".to_owned(),
                    }),
                ),
            ] {
                let result = set_archived(&store, &target, ArchiveState::Archived, LIVE_ID);

                assert_eq!(result, Err(expected), "{target:?}");
            }
            assert_eq!(listed_ids(&store, ArchiveState::Archived), [] as [&str; 0]);
        }

        #[test]
        fn names_the_action_the_session_and_the_cause_of_a_failed_write() {
            let dir = tempfile::tempdir().unwrap();
            let store = seeded_store(dir.path());
            make_read_only(&test_session_file(dir.path(), SAVED_ID));
            let target = SavedTarget::Id(SAVED_ID.to_owned());

            let err = set_archived(&store, &target, ArchiveState::Archived, LIVE_ID).unwrap_err();

            let cause = store
                .set_archived(SAVED_ID, ArchiveState::Archived)
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("failed to archive session saved-se: {cause:#}")
            );
        }
    }

    mod delete {
        use super::*;

        #[test]
        fn deletes_a_saved_session_and_refuses_the_live_one() {
            let dir = tempfile::tempdir().unwrap();
            let store = seeded_store(dir.path());

            delete(&store, SAVED_ID, LIVE_ID).unwrap();
            let err = delete(&store, LIVE_ID, LIVE_ID).unwrap_err();

            assert_eq!(err.to_string(), "session live-ses is the live session");
            assert_eq!(listed_ids(&store, ArchiveState::Active), [LIVE_ID]);
        }

        #[test]
        fn names_the_session_and_the_cause_of_a_failed_delete() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let logs = LogCapture::default();

            let err = tracing::dispatcher::with_default(&logs.dispatch(), || {
                delete(&store, "missing-session", LIVE_ID)
            })
            .unwrap_err();

            let cause = "session not found: missing-session";
            assert_eq!(
                err.to_string(),
                format!("failed to delete session missing-: {cause}")
            );
            assert_eq!(
                logs.events(),
                [BTreeMap::from([
                    ("action".to_owned(), "delete".to_owned()),
                    ("session_id".to_owned(), "missing-session".to_owned()),
                    ("error".to_owned(), cause.to_owned()),
                    ("message".to_owned(), "session operation failed".to_owned()),
                ])]
            );
        }
    }
}
