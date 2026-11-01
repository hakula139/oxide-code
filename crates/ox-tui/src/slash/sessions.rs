//! Messages and metadata lines for the commands that act on saved sessions.

use std::fmt::Write as _;

use time::OffsetDateTime;

use ox_core::protocol::SessionRef;
use ox_core::session::display::{archive_notice, format_relative_time, id_prefix};
use ox_core::session::saved::{ArchiveUpdate, SavedError};
use ox_core::session::store::listing::ArchiveState;
use ox_core::util::text::{capitalize_first, plural};

// ── Messages ──

pub(crate) const CLEAR_THEN_DELETE_HINT: &str = "Run `/clear` to start a new one, then delete it.";

pub(super) const BARE_ARCHIVE_HINT: &str = "Use bare `/archive` to archive it and start a new one.";

/// Phrases `error` as a sentence, ending a refusal of the live session with `live_recovery`.
pub(crate) fn saved_error_message(error: &SavedError, live_recovery: &str) -> String {
    let sentence = capitalize_first(&error.to_string());
    if error.is_live() {
        format!("{sentence}. {live_recovery}")
    } else {
        sentence
    }
}

/// The chat line for the result of setting the archive state of the saved session `target` names
/// to `state`, as a note or, for a failure, as an error. An error for a typed prefix names the
/// command that sent it.
pub(crate) fn archive_message(
    target: &SessionRef,
    state: ArchiveState,
    result: &Result<ArchiveUpdate, SavedError>,
) -> Result<String, String> {
    match result {
        Ok(ArchiveUpdate { id, changed: true }) => Ok(archive_notice(id, state)),
        Ok(ArchiveUpdate { id, changed: false }) => {
            let already = match state {
                ArchiveState::Active => "not archived",
                ArchiveState::Archived => "already archived",
            };
            Ok(format!("Session {} is {already}", id_prefix(id)))
        }
        Err(error) if error.is_live() && state == ArchiveState::Active => {
            Ok("The live session is not archived".to_owned())
        }
        Err(error) => {
            let message = saved_error_message(error, BARE_ARCHIVE_HINT);
            Err(match (target, state) {
                (SessionRef::Id(_), _) => message,
                (SessionRef::Prefix(_), ArchiveState::Archived) => format!("/archive: {message}"),
                (SessionRef::Prefix(_), ArchiveState::Active) => format!("/unarchive: {message}"),
            })
        }
    }
}

/// The chat error for a `/delete` prefix that names no session it can delete.
pub(crate) fn delete_lookup_error(error: &SavedError) -> String {
    format!(
        "/delete: {}",
        saved_error_message(error, CLEAR_THEN_DELETE_HINT)
    )
}

// ── Metadata ──

const SEPARATOR: &str = " · ";

/// `{id_prefix} · {when} · {N msgs} · {branch} · {project}`, omitting each trailing component whose
/// input is zero or `None`. `when` is relative to `now`, and a date is in its offset.
pub(super) fn format_metadata_line(
    session_id: &str,
    last_active_at: OffsetDateTime,
    now: OffsetDateTime,
    message_count: u32,
    git_branch: Option<&str>,
    project: Option<&str>,
) -> String {
    let when = format_relative_time(last_active_at.to_offset(now.offset()), now);
    let prefix = id_prefix(session_id);
    let mut meta = format!("{prefix}{SEPARATOR}{when}");
    if message_count > 0 {
        let unit = plural(message_count, "msg", "msgs");
        _ = write!(meta, "{SEPARATOR}{message_count} {unit}");
    }
    if let Some(branch) = git_branch {
        meta.push_str(SEPARATOR);
        meta.push_str(branch);
    }
    if let Some(p) = project {
        meta.push_str(SEPARATOR);
        meta.push_str(p);
    }
    meta
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use ox_core::session::resolver::ResolveError;
    use ox_core::session::saved::SavedAction;

    use super::*;

    mod saved_error_message {
        use super::*;

        #[test]
        fn capitalizes_each_error_and_adds_the_recovery_only_to_a_live_refusal() {
            for (error, recovery, expected) in [
                (
                    SavedError::Resolve(ResolveError::NoMatch {
                        prefix: "zz".to_owned(),
                    }),
                    CLEAR_THEN_DELETE_HINT,
                    "No session matching `zz`",
                ),
                (
                    SavedError::Resolve(ResolveError::Live {
                        prefix: "test-".to_owned(),
                    }),
                    BARE_ARCHIVE_HINT,
                    "Session prefix `test-` matches the live session. \
                     Use bare `/archive` to archive it and start a new one.",
                ),
                (
                    SavedError::Live {
                        id: "test-ses".to_owned(),
                    },
                    CLEAR_THEN_DELETE_HINT,
                    "Session test-ses is the live session. \
                     Run `/clear` to start a new one, then delete it.",
                ),
                (
                    SavedError::Resolve(ResolveError::Ambiguous {
                        prefix: "ab".to_owned(),
                        count: 7,
                        preview: "ab1, ab2, ab3, ab4, ab5, and 2 more".to_owned(),
                    }),
                    CLEAR_THEN_DELETE_HINT,
                    "Ambiguous prefix `ab` matches 7 sessions: ab1, ab2, ab3, ab4, ab5, and 2 more",
                ),
                (
                    SavedError::Resolve(ResolveError::List {
                        scope: "all projects",
                        cause: "disk unavailable".to_owned(),
                    }),
                    CLEAR_THEN_DELETE_HINT,
                    "Failed to list sessions in all projects: disk unavailable",
                ),
            ] {
                assert_eq!(saved_error_message(&error, recovery), expected, "{error:?}");
            }
        }
    }

    mod archive_message {
        use super::*;

        const ID: &str = "abababab-abab-abab-abab-abababababab";

        #[test]
        fn notes_a_change_or_its_absence_and_reports_a_failure_as_an_error() {
            let update = |changed| {
                Ok(ArchiveUpdate {
                    id: ID.to_owned(),
                    changed,
                })
            };
            let live_prefix = || {
                Err(SavedError::Resolve(ResolveError::Live {
                    prefix: "test-".to_owned(),
                }))
            };
            let failed = Err(SavedError::Failed {
                action: SavedAction::Unarchive,
                id: "abababab".to_owned(),
                cause: "disk full".to_owned(),
            });

            let typed = SessionRef::Prefix("abab".to_owned());
            let picked = SessionRef::Id(ID.to_owned());
            let no_match = || {
                Err(SavedError::Resolve(ResolveError::NoMatch {
                    prefix: "zzzz".to_owned(),
                }))
            };

            for (target, state, result, expected) in [
                (
                    &typed,
                    ArchiveState::Archived,
                    update(true),
                    Ok(archive_notice(ID, ArchiveState::Archived)),
                ),
                (
                    &picked,
                    ArchiveState::Active,
                    update(true),
                    Ok(archive_notice(ID, ArchiveState::Active)),
                ),
                (
                    &typed,
                    ArchiveState::Archived,
                    update(false),
                    Ok("Session abababab is already archived".to_owned()),
                ),
                (
                    &typed,
                    ArchiveState::Active,
                    update(false),
                    Ok("Session abababab is not archived".to_owned()),
                ),
                (
                    &typed,
                    ArchiveState::Active,
                    live_prefix(),
                    Ok("The live session is not archived".to_owned()),
                ),
                (
                    &typed,
                    ArchiveState::Archived,
                    live_prefix(),
                    Err(
                        "/archive: Session prefix `test-` matches the live session. \
                         Use bare `/archive` to archive it and start a new one."
                            .to_owned(),
                    ),
                ),
                (
                    &typed,
                    ArchiveState::Active,
                    no_match(),
                    Err("/unarchive: No session matching `zzzz`".to_owned()),
                ),
                (
                    &typed,
                    ArchiveState::Archived,
                    no_match(),
                    Err("/archive: No session matching `zzzz`".to_owned()),
                ),
                (
                    &picked,
                    ArchiveState::Active,
                    failed,
                    Err("Failed to unarchive session abababab: disk full".to_owned()),
                ),
            ] {
                assert_eq!(
                    archive_message(target, state, &result),
                    expected,
                    "{target:?} {state:?}: {result:?}"
                );
            }
        }
    }

    mod format_metadata_line {
        use super::*;

        const ID: &str = "abababab-abab-abab-abab-abababababab";

        #[test]
        fn joins_every_supplied_component() {
            let now = datetime!(2026-05-08 12:00:00 UTC);

            let line = format_metadata_line(
                ID,
                now - time::Duration::hours(3),
                now,
                14,
                Some("feat/login"),
                Some("~/work/oxide"),
            );

            assert_eq!(
                line,
                "abababab · 3 hours ago · 14 msgs · feat/login · ~/work/oxide"
            );
        }

        #[test]
        fn counts_one_msg_in_the_singular() {
            let now = datetime!(2026-05-08 12:00:00 UTC);

            let line = format_metadata_line(ID, now, now, 1, None, None);

            assert_eq!(line, "abababab · 0 seconds ago · 1 msg");
        }

        #[test]
        fn omits_absent_components() {
            let now = datetime!(2026-05-08 12:00:00 UTC);

            let line =
                format_metadata_line(ID, datetime!(2026-01-02 09:00:00 UTC), now, 0, None, None);

            assert_eq!(line, "abababab · 2026-01-02");
        }

        #[test]
        fn dates_in_the_offset_of_now() {
            let now = datetime!(2026-02-01 12:00:00 +09:00);

            let line =
                format_metadata_line(ID, datetime!(2026-01-01 20:00:00 UTC), now, 0, None, None);

            assert_eq!(line, "abababab · 2026-01-02");
        }
    }
}
