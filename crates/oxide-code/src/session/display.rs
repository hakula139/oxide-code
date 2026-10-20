//! Display helpers for session titles, metadata lines, relative times, and archive notices.

use std::fmt::Write as _;

use time::{OffsetDateTime, UtcOffset};

use super::entry::SessionInfo;
use crate::util::text::{plural, truncate_to_width};

const SEPARATOR: &str = " · ";
pub(crate) const ID_PREFIX_WIDTH: usize = 8;
pub(crate) const UNTITLED_MARKER: &str = "(untitled)";

/// Widest [`format_short_relative_time`] output, an ISO date.
pub(crate) const SHORT_RELATIVE_TIME_WIDTH: usize = "2026-01-01".len();

/// Column cap on first-prompt and manual titles, leaving room in a 120-column `--list` row after
/// the fixed `ID / Last Active / Msgs` prefix.
pub(crate) const MAX_TITLE_WIDTH: usize = 80;

/// First [`ID_PREFIX_WIDTH`] bytes of `session_id`, or the full ID when shorter.
pub(crate) fn id_prefix(session_id: &str) -> &str {
    session_id.get(..ID_PREFIX_WIDTH).unwrap_or(session_id)
}

/// Title from `info`, or [`UNTITLED_MARKER`] when absent.
pub(crate) fn display_title(info: &SessionInfo) -> String {
    info.title
        .as_ref()
        .map_or_else(|| UNTITLED_MARKER.to_owned(), |t| t.title.clone())
}

/// The first non-blank line of `text`, trimmed and truncated to [`MAX_TITLE_WIDTH`] columns.
pub(crate) fn fit_title(text: &str) -> String {
    let first_line = text.trim().lines().next().unwrap_or_default();
    truncate_to_width(first_line.trim_end(), MAX_TITLE_WIDTH)
}

/// Compact "N units ago", or the ISO date past 30 days. A future `ts` reads as 0 seconds ago.
fn format_relative_time(ts: OffsetDateTime, now: OffsetDateTime) -> String {
    match elapsed(ts, now) {
        Some((n, unit)) => {
            let suffix = plural(n, "", "s");
            format!("{n} {unit}{suffix} ago")
        }
        None => format_date(ts),
    }
}

/// [`format_relative_time`] with each unit cut to its first letter, as in `3h ago`, for a time
/// column [`SHORT_RELATIVE_TIME_WIDTH`] wide.
pub(crate) fn format_short_relative_time(ts: OffsetDateTime, local_offset: UtcOffset) -> String {
    let now = OffsetDateTime::now_utc().to_offset(local_offset);
    short_relative_time(ts.to_offset(local_offset), now)
}

fn short_relative_time(ts: OffsetDateTime, now: OffsetDateTime) -> String {
    match elapsed(ts, now) {
        Some((n, unit)) => format!("{n}{} ago", &unit[..1]),
        None => format_date(ts),
    }
}

/// The time from `ts` to `now` in whole units of the largest unit that fits, or `None` past 30
/// days. A future `ts` reads as 0 seconds.
fn elapsed(ts: OffsetDateTime, now: OffsetDateTime) -> Option<(i64, &'static str)> {
    let secs = (now - ts).whole_seconds().max(0);
    Some(if secs < 60 {
        (secs, "second")
    } else if secs < 3600 {
        (secs / 60, "minute")
    } else if secs < 86_400 {
        (secs / 3600, "hour")
    } else if secs < 30 * 86_400 {
        (secs / 86_400, "day")
    } else {
        return None;
    })
}

fn format_date(ts: OffsetDateTime) -> String {
    ts.format(time::macros::format_description!("[year]-[month]-[day]"))
        .expect("static `[year]-[month]-[day]` description never fails on a valid ts")
}

/// `{id_prefix} · {when} · {N msgs} · {branch} · {project}`, omitting each trailing component
/// whose input is zero or `None`.
pub(crate) fn format_metadata_line(
    session_id: &str,
    last_active_at: OffsetDateTime,
    local_offset: UtcOffset,
    message_count: u32,
    git_branch: Option<&str>,
    project: Option<&str>,
) -> String {
    let now = OffsetDateTime::now_utc().to_offset(local_offset);
    let when = format_relative_time(last_active_at.to_offset(local_offset), now);
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

/// Notice that `session_id` was archived or unarchived.
pub(crate) fn archive_notice(session_id: &str, archived: bool) -> String {
    let id = id_prefix(session_id);
    if archived {
        format!(
            "Archived session {id}. Press Ctrl+A in the /resume picker to list archived sessions."
        )
    } else {
        format!("Unarchived session {id}")
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::session::entry::TitleInfo;

    mod id_prefix {
        use super::*;

        #[test]
        fn uses_first_eight_bytes_or_full_short_id() {
            assert_eq!(id_prefix("abcdefghij"), "abcdefgh");
            assert_eq!(id_prefix("abcd"), "abcd");
            assert_eq!(id_prefix(""), "");
        }
    }

    mod display_title {
        use super::*;

        #[test]
        fn uses_title_when_present_else_untitled_marker() {
            let mut info = SessionInfo {
                session_id: "abc".to_owned(),
                path: std::path::PathBuf::new(),
                cwd: String::new(),
                last_active_at: datetime!(2026-05-08 09:00:00 UTC),
                title: None,
                exit: None,
                git_branch: None,
                archived: false,
            };
            assert_eq!(display_title(&info), UNTITLED_MARKER);

            info.title = Some(TitleInfo {
                title: "Fix auth flow".to_owned(),
                updated_at: datetime!(2026-05-08 09:00:00 UTC),
            });
            assert_eq!(display_title(&info), "Fix auth flow");
        }
    }

    mod fit_title {
        use super::*;

        #[test]
        fn keeps_the_trimmed_first_line() {
            assert_eq!(
                fit_title("  Fix auth flow  \nDetails below"),
                "Fix auth flow"
            );
        }

        #[test]
        fn skips_leading_blank_lines() {
            assert_eq!(fit_title("\n  \n Fix auth flow\nDetails"), "Fix auth flow");
        }

        #[test]
        fn blank_text_gives_an_empty_title() {
            assert_eq!(fit_title(" \n\t\n "), "");
        }

        #[test]
        fn truncates_past_the_column_cap() {
            let exact = "\u{00e9}".repeat(MAX_TITLE_WIDTH);
            assert_eq!(fit_title(&exact), exact);

            let long = "\u{00e9}".repeat(MAX_TITLE_WIDTH + 1);
            assert_eq!(
                fit_title(&long),
                format!("{}...", "\u{00e9}".repeat(MAX_TITLE_WIDTH - 3)),
            );
        }

        #[test]
        fn counts_wide_chars_by_column() {
            assert_eq!(
                fit_title(&"界".repeat(MAX_TITLE_WIDTH)),
                format!("{}...", "界".repeat(38)),
            );
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

    mod short_relative_time {
        use super::*;

        #[test]
        fn cuts_each_unit_to_its_letter_and_keeps_the_date_past_30_days() {
            let now = datetime!(2026-05-08 12:00:00 UTC);
            let ago = |elapsed: time::Duration| short_relative_time(now - elapsed, now);

            assert_eq!(ago(time::Duration::seconds(45)), "45s ago");
            assert_eq!(ago(time::Duration::minutes(5)), "5m ago");
            assert_eq!(ago(time::Duration::hours(23)), "23h ago");
            assert_eq!(ago(time::Duration::days(29)), "29d ago");
            assert_eq!(ago(time::Duration::days(30)), "2026-04-08");
            assert_eq!(SHORT_RELATIVE_TIME_WIDTH, "2026-04-08".len());
        }
    }

    mod format_metadata_line {
        use super::*;

        #[test]
        fn includes_msg_count_branch_and_project_when_supplied() {
            let id = "aabbccdd-eeff-1122-3344-556677889900";
            let line = format_metadata_line(
                id,
                datetime!(2026-05-08 09:00:00 UTC),
                UtcOffset::UTC,
                14,
                Some("feat/login"),
                Some("~/work/oxide"),
            );
            assert!(line.contains(&id[..ID_PREFIX_WIDTH]), "id prefix: {line}");
            assert!(line.contains("14 msgs"), "plural msg count: {line}");
            assert!(line.contains("feat/login"), "branch: {line}");
            assert!(line.contains("~/work/oxide"), "project: {line}");
        }

        #[test]
        fn singular_msg_unit_at_count_one() {
            let line = format_metadata_line(
                "aabbccdd-eeff-1122-3344-556677889900",
                datetime!(2026-05-08 09:00:00 UTC),
                UtcOffset::UTC,
                1,
                None,
                None,
            );
            assert!(line.contains("1 msg"), "singular: {line}");
            assert!(!line.contains("1 msgs"), "no plural slip: {line}");
        }

        #[test]
        fn omits_msg_count_branch_and_project_when_absent() {
            let line = format_metadata_line(
                "aabbccdd-eeff-1122-3344-556677889900",
                datetime!(2026-05-08 09:00:00 UTC),
                UtcOffset::UTC,
                0,
                None,
                None,
            );
            assert!(!line.contains("msg"), "no msg segment: {line}");
            assert_eq!(
                line.matches(SEPARATOR).count(),
                1,
                "exactly one separator between prefix and when: {line}",
            );
        }
    }

    mod archive_notice {
        use super::*;

        #[test]
        fn names_the_id_prefix_and_where_archived_sessions_list() {
            assert_eq!(
                archive_notice("abcdefghij", true),
                "Archived session abcdefgh. \
                 Press Ctrl+A in the /resume picker to list archived sessions.",
            );
            assert_eq!(
                archive_notice("abcdefghij", false),
                "Unarchived session abcdefgh"
            );
        }
    }
}
