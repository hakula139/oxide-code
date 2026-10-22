//! Store access, error messages, and metadata lines for the commands that act on saved sessions.

use std::fmt::Write as _;

use time::OffsetDateTime;

use ox_core::session::display::id_prefix;
use ox_core::session::resolver::ResolveError;
use ox_core::session::saved::SavedError;
use ox_core::session::store::SessionStore;
use ox_core::util::text::plural;

use super::context::LiveSessionInfo;

// ── Store ──

/// Opens the store scoped to the session's project.
pub(super) fn open_store(info: &LiveSessionInfo) -> Result<SessionStore, String> {
    let cwd = info
        .working_dir
        .as_deref()
        .ok_or("No working directory for this session")?;
    SessionStore::open(cwd, &info.project_root)
        .map_err(|e| format!("Session store unavailable: {e:#}"))
}

// ── Error Messages ──

pub(super) const BARE_ARCHIVE_HINT: &str = "Use bare `/archive` to archive it and start a new one.";

pub(super) const CLEAR_THEN_DELETE_HINT: &str = "Run `/clear` to start a new one, then delete it.";

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

// ── Metadata ──

const SEPARATOR: &str = " · ";

/// Widest [`format_short_relative_time`] output, an ISO date.
pub(super) const SHORT_RELATIVE_TIME_WIDTH: usize = "2026-01-01".len();

/// `{id_prefix} · {when} · {N msgs} · {branch} · {project}`,
/// omitting each trailing component whose input is zero or `None`.
/// `when` is relative to `now`, and a date is in its offset.
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

/// [`format_relative_time`] with each unit cut to its first letter, as in `3h ago`,
/// for a time column [`SHORT_RELATIVE_TIME_WIDTH`] wide.
/// The date is in `now`'s offset.
pub(super) fn format_short_relative_time(ts: OffsetDateTime, now: OffsetDateTime) -> String {
    let ts = ts.to_offset(now.offset());
    match elapsed(ts, now) {
        Some((n, (unit, _))) => format!("{n}{} ago", &unit[..1]),
        None => format_date(ts),
    }
}

/// Compact "N units ago", or the ISO date past 30 days.
/// A future `ts` reads as 0 seconds ago.
fn format_relative_time(ts: OffsetDateTime, now: OffsetDateTime) -> String {
    match elapsed(ts, now) {
        Some((n, (unit, units))) => format!("{n} {} ago", plural(n, unit, units)),
        None => format_date(ts),
    }
}

/// The time from `ts` to `now` in whole units of the largest unit that fits,
/// with the unit's singular and plural names, or `None` past 30 days.
/// A future `ts` reads as 0 seconds.
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
