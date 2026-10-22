//! Session-store access shared by the commands that act on saved sessions.

use ox_core::session::resolver::ResolveError;
use ox_core::session::saved::SavedError;
use ox_core::session::store::SessionStore;

use super::context::LiveSessionInfo;

pub(super) const BARE_ARCHIVE_HINT: &str = "Use bare `/archive` to archive it and start a new one.";

pub(super) const CLEAR_THEN_DELETE_HINT: &str = "Run `/clear` to start a new one, then delete it.";

/// Opens the store scoped to the session's project.
pub(super) fn open_store(info: &LiveSessionInfo) -> Result<SessionStore, String> {
    let cwd = info
        .working_dir
        .as_deref()
        .ok_or("No working directory for this session")?;
    SessionStore::open(cwd, &info.project_root)
        .map_err(|e| format!("Session store unavailable: {e:#}"))
}

/// Phrases `error` as a sentence, ending a live-session match with `live_recovery` when given.
pub(super) fn resolve_error_message(error: ResolveError, live_recovery: Option<&str>) -> String {
    match error {
        ResolveError::NoMatch { prefix } => format!("No session matching `{prefix}`"),
        ResolveError::Live { prefix } => {
            let live = format!("Session prefix `{prefix}` matches the live session");
            match live_recovery {
                Some(recovery) => format!("{live}. {recovery}"),
                None => live,
            }
        }
        ResolveError::Ambiguous {
            prefix,
            count,
            preview,
        } => format!("Ambiguous prefix `{prefix}` matches {count} sessions: {preview}"),
        ResolveError::List { scope, cause } => {
            format!("Failed to list sessions in {scope}: {cause:#}")
        }
    }
}

/// Phrases `error` as a sentence, ending a live-session refusal with `live_recovery` when given.
pub(super) fn saved_error_message(error: SavedError, live_recovery: Option<&str>) -> String {
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
    use super::*;
    use crate::slash::testing::test_session_info;

    mod open_store {
        use super::*;

        #[test]
        fn reports_a_data_dir_it_cannot_create() {
            let dir = tempfile::tempdir().unwrap();
            let data_home = dir.path().join("data");
            std::fs::write(&data_home, "").unwrap();
            let sessions_dir = data_home.join("ox").join("sessions");
            let cause = std::fs::create_dir_all(&sessions_dir).unwrap_err();

            let err = temp_env::with_var("XDG_DATA_HOME", Some(data_home.as_os_str()), || {
                open_store(&test_session_info()).err().unwrap()
            });
            assert_eq!(
                err,
                format!(
                    "Session store unavailable: failed to create {}: {cause}",
                    sessions_dir.display(),
                ),
            );
        }
    }

    mod resolve_error_message {
        use super::*;

        #[test]
        fn capitalizes_each_variant() {
            let no_match = ResolveError::NoMatch {
                prefix: "zz".to_owned(),
            };
            assert_eq!(
                resolve_error_message(no_match, None),
                "No session matching `zz`"
            );

            let live = || ResolveError::Live {
                prefix: "test-".to_owned(),
            };
            assert_eq!(
                resolve_error_message(live(), Some(BARE_ARCHIVE_HINT)),
                "Session prefix `test-` matches the live session. \
                 Use bare `/archive` to archive it and start a new one.",
            );
            assert_eq!(
                resolve_error_message(live(), None),
                "Session prefix `test-` matches the live session",
            );

            let ambiguous = ResolveError::Ambiguous {
                prefix: "ab".to_owned(),
                count: 7,
                preview: "ab1, ab2, ab3, ab4, ab5, and 2 more".to_owned(),
            };
            assert_eq!(
                resolve_error_message(ambiguous, None),
                "Ambiguous prefix `ab` matches 7 sessions: ab1, ab2, ab3, ab4, ab5, and 2 more",
            );

            let list = ResolveError::List {
                scope: "all projects",
                cause: anyhow::anyhow!("disk unavailable"),
            };
            assert_eq!(
                resolve_error_message(list, None),
                "Failed to list sessions in all projects: disk unavailable",
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
