//! Session-store access shared by the commands that act on saved sessions.

use super::context::LiveSessionInfo;
use crate::session::resolver::ResolveError;
use crate::session::store::SessionStore;

/// Opens the store scoped to the session's working directory.
pub(super) fn open_store(info: &LiveSessionInfo) -> Result<SessionStore, String> {
    let cwd = info
        .working_dir
        .as_deref()
        .ok_or("No working directory for this session")?;
    SessionStore::open(cwd).map_err(|e| format!("Session store unavailable: {e:#}"))
}

pub(super) fn resolve_error_message(error: ResolveError) -> String {
    match error {
        ResolveError::NoMatch { prefix } => format!("No session matching `{prefix}`"),
        ResolveError::Ambiguous {
            prefix,
            count,
            preview,
        } => format!("Ambiguous prefix `{prefix}` matches {count} sessions: {preview}"),
        ResolveError::List { scope, cause } => {
            format!("Failed to list sessions ({scope}): {cause:#}")
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
            assert_eq!(resolve_error_message(no_match), "No session matching `zz`");

            let ambiguous = ResolveError::Ambiguous {
                prefix: "ab".to_owned(),
                count: 7,
                preview: "ab1, ab2, ab3, ab4, ab5, and 2 more".to_owned(),
            };
            assert_eq!(
                resolve_error_message(ambiguous),
                "Ambiguous prefix `ab` matches 7 sessions: ab1, ab2, ab3, ab4, ab5, and 2 more",
            );

            let list = ResolveError::List {
                scope: "all projects",
                cause: anyhow::anyhow!("disk unavailable"),
            };
            assert_eq!(
                resolve_error_message(list),
                "Failed to list sessions (all projects): disk unavailable",
            );
        }
    }
}
