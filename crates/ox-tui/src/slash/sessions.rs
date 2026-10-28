//! Store access, messages, and metadata lines for the commands that act on saved sessions.

use std::fmt::Write as _;

use time::OffsetDateTime;

use ox_core::protocol::SavedTarget;
use ox_core::session::display::{archive_notice, id_prefix};
use ox_core::session::entry::ArchiveState;
use ox_core::session::saved::{ArchiveUpdate, SavedError};
use ox_core::session::store::SessionStore;
use ox_core::util::text::{capitalize_first, plural};

use crate::session_info::LiveSessionInfo;

// ── Store ──

/// Opens the store scoped to the session's project.
pub(super) fn open_store(info: &LiveSessionInfo) -> Result<SessionStore, String> {
    SessionStore::open(&info.cwd, &info.project_root)
        .map_err(|e| format!("Session store unavailable: {e:#}"))
}

// ── Messages ──

pub(crate) const CLEAR_THEN_DELETE_HINT: &str = "Run `/clear` to start a new one, then delete it.";

pub(super) const BARE_ARCHIVE_HINT: &str = "Use bare `/archive` to archive it and start a new one.";

/// Phrases `error` as a sentence, ending a refusal of the live session with `live_recovery` when
/// given.
pub(crate) fn saved_error_message(error: &SavedError, live_recovery: Option<&str>) -> String {
    let sentence = capitalize_first(&error.to_string());
    match live_recovery {
        Some(recovery) if error.is_live() => format!("{sentence}. {recovery}"),
        _ => sentence,
    }
}

/// The chat line for the result of setting the archive state of the saved session `target` names
/// to `state`, as a note or, for a failure, as an error. An error for a typed prefix names the
/// command that sent it.
pub(crate) fn archive_message(
    target: &SavedTarget,
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
            let message = saved_error_message(error, Some(BARE_ARCHIVE_HINT));
            Err(match (target, state) {
                (SavedTarget::Id(_), _) => message,
                (SavedTarget::Prefix(_), ArchiveState::Archived) => format!("/archive: {message}"),
                (SavedTarget::Prefix(_), ArchiveState::Active) => format!("/unarchive: {message}"),
            })
        }
    }
}

/// The chat error for a `/delete` prefix that names no session it can delete.
pub(crate) fn delete_lookup_error(error: &SavedError) -> String {
    format!(
        "/delete: {}",
        saved_error_message(error, Some(CLEAR_THEN_DELETE_HINT))
    )
}

// ── Metadata ──

const SEPARATOR: &str = " · ";

/// Widest [`format_short_relative_time`] output, an ISO date.
pub(super) const SHORT_RELATIVE_TIME_WIDTH: usize = "2026-01-01".len();

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

/// [`format_relative_time`] with each unit cut to its first letter, as in `3h ago`, for a time
/// column [`SHORT_RELATIVE_TIME_WIDTH`] wide. The date is in `now`'s offset.
pub(super) fn format_short_relative_time(ts: OffsetDateTime, now: OffsetDateTime) -> String {
    let ts = ts.to_offset(now.offset());
    match elapsed(ts, now) {
        Some((n, (unit, _))) => format!("{n}{} ago", &unit[..1]),
        None => format_date(ts),
    }
}

/// Compact "N units ago", or the ISO date past 30 days. A future `ts` reads as 0 seconds ago.
fn format_relative_time(ts: OffsetDateTime, now: OffsetDateTime) -> String {
    match elapsed(ts, now) {
        Some((n, (unit, units))) => format!("{n} {} ago", plural(n, unit, units)),
        None => format_date(ts),
    }
}

/// The time from `ts` to `now` in whole units of the largest unit that fits, with the unit's
/// singular and plural names, or `None` past 30 days. A future `ts` reads as 0 seconds.
fn elapsed(ts: OffsetDateTime, now: OffsetDateTime) -> Option<(i64, (&'static str, &'static str))> {
    let secs = (now - ts).whole_seconds().max(0);
    Some(if secs < 60 {
        (secs, ("second", "seconds"))
    } else if secs < 3600 {
        (secs / 60, ("minute", "minutes"))
    } else if secs < 86_400 {
        (secs / 3600, ("hour", "hours"))
    } else if secs < 30 * 86_400 {
        (secs / 86_400, ("day", "days"))
    } else {
        return None;
    })
}

fn format_date(ts: OffsetDateTime) -> String {
    ts.format(time::macros::format_description!("[year]-[month]-[day]"))
        .expect("static `[year]-[month]-[day]` description never fails on a valid ts")
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use ox_core::session::resolver::ResolveError;
    use ox_core::session::saved::SavedAction;

    use super::*;
    use crate::session_info::testing::test_session_info;

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

    mod saved_error_message {
        use super::*;

        #[test]
        fn capitalizes_each_error_and_adds_the_recovery_only_to_a_live_refusal() {
            for (error, recovery, expected) in [
                (
                    SavedError::Resolve(ResolveError::NoMatch {
                        prefix: "zz".to_owned(),
                    }),
                    Some(CLEAR_THEN_DELETE_HINT),
                    "No session matching `zz`",
                ),
                (
                    SavedError::Resolve(ResolveError::Live {
                        prefix: "test-".to_owned(),
                    }),
                    Some(BARE_ARCHIVE_HINT),
                    "Session prefix `test-` matches the live session. \
                     Use bare `/archive` to archive it and start a new one.",
                ),
                (
                    SavedError::Live {
                        id: "test-ses".to_owned(),
                    },
                    Some(CLEAR_THEN_DELETE_HINT),
                    "Session test-ses is the live session. \
                     Run `/clear` to start a new one, then delete it.",
                ),
                (
                    SavedError::Live {
                        id: "test-ses".to_owned(),
                    },
                    None,
                    "Session test-ses is the live session",
                ),
                (
                    SavedError::Resolve(ResolveError::Ambiguous {
                        prefix: "ab".to_owned(),
                        count: 7,
                        preview: "ab1, ab2, ab3, ab4, ab5, and 2 more".to_owned(),
                    }),
                    None,
                    "Ambiguous prefix `ab` matches 7 sessions: ab1, ab2, ab3, ab4, ab5, and 2 more",
                ),
                (
                    SavedError::Resolve(ResolveError::List {
                        scope: "all projects",
                        cause: "disk unavailable".to_owned(),
                    }),
                    None,
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

            let typed = SavedTarget::Prefix("abab".to_owned());
            let picked = SavedTarget::Id(ID.to_owned());
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

    mod format_short_relative_time {
        use super::*;

        #[test]
        fn cuts_each_unit_to_its_letter_and_keeps_the_date_past_30_days() {
            let now = datetime!(2026-05-08 12:00:00 UTC);
            let ago = |elapsed: time::Duration| format_short_relative_time(now - elapsed, now);

            assert_eq!(ago(time::Duration::seconds(45)), "45s ago");
            assert_eq!(ago(time::Duration::minutes(5)), "5m ago");
            assert_eq!(ago(time::Duration::hours(23)), "23h ago");
            assert_eq!(ago(time::Duration::days(29)), "29d ago");
            assert_eq!(ago(time::Duration::days(30)), "2026-04-08");
            assert_eq!(SHORT_RELATIVE_TIME_WIDTH, "2026-04-08".len());
        }

        #[test]
        fn dates_in_the_offset_of_now() {
            let now = datetime!(2026-05-08 12:00:00 +09:00);

            let when = format_short_relative_time(datetime!(2026-03-31 20:00:00 UTC), now);

            assert_eq!(when, "2026-04-01");
        }
    }

    mod format_relative_time {
        use super::*;

        #[test]
        fn pluralizes_units_and_falls_back_to_iso_date_past_30_days() {
            let now = datetime!(2026-05-08 12:00:00 UTC);
            assert_eq!(
                format_relative_time(now - time::Duration::seconds(1), now),
                "1 second ago",
            );
            assert_eq!(
                format_relative_time(now - time::Duration::seconds(3), now),
                "3 seconds ago",
            );
            assert_eq!(
                format_relative_time(now - time::Duration::minutes(1), now),
                "1 minute ago",
            );
            assert_eq!(
                format_relative_time(now - time::Duration::minutes(2), now),
                "2 minutes ago",
            );
            assert_eq!(
                format_relative_time(now - time::Duration::hours(1), now),
                "1 hour ago",
            );
            assert_eq!(
                format_relative_time(now - time::Duration::hours(5), now),
                "5 hours ago",
            );
            assert_eq!(
                format_relative_time(now - time::Duration::days(1), now),
                "1 day ago",
            );
            assert_eq!(
                format_relative_time(now - time::Duration::days(3), now),
                "3 days ago",
            );
            assert_eq!(
                format_relative_time(now - time::Duration::days(60), now),
                "2026-03-09",
            );
            assert_eq!(
                format_relative_time(now + time::Duration::seconds(30), now),
                "0 seconds ago",
                "future stamps collapse to 0",
            );
        }
    }
}
