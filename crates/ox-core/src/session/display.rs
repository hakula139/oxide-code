//! Display helpers for session IDs, titles, ages, and archive notices shared by every frontend.

use std::fmt;

use time::OffsetDateTime;
use unicode_width::UnicodeWidthStr as _;

use super::store::listing::{ArchiveState, SessionInfo};
use crate::util::text::{escape_line, plural, truncate_to_width};

pub const ID_PREFIX_WIDTH: usize = 8;
pub const UNTITLED_MARKER: &str = "(untitled)";

/// Column cap on every title, leaving room in a 120-column `--list` row after the fixed
/// `ID / Last Active / Msgs` prefix.
pub const MAX_TITLE_WIDTH: usize = 80;

/// Widest [`format_short_relative_time`] output, an ISO date.
pub const SHORT_RELATIVE_TIME_WIDTH: usize = "2026-01-01".len();

/// First [`ID_PREFIX_WIDTH`] bytes of `session_id`, or the full ID when shorter.
pub fn id_prefix(session_id: &str) -> &str {
    session_id.get(..ID_PREFIX_WIDTH).unwrap_or(session_id)
}

/// A session's title as listings show it: the title, or [`UNTITLED_MARKER`] when absent, escaped
/// onto one line. A fork, which starts with its source's title, is labeled `(fork of {id})` with
/// the source's [`id_prefix`].
#[derive(Debug, Clone)]
pub struct DisplayTitle {
    title: String,
    fork_label: Option<String>,
}

impl DisplayTitle {
    pub fn of(info: &SessionInfo) -> Self {
        Self {
            title: escape_line(info.title.as_deref().unwrap_or(UNTITLED_MARKER)),
            fork_label: info
                .forked_from
                .as_deref()
                .map(|source| format!(" (fork of {})", id_prefix(source))),
        }
    }

    /// The title fit to `width` columns. A fork's label stays whole while the title can shrink to
    /// one column, since the label tells the fork from its source.
    pub fn fit(&self, width: usize) -> String {
        let Some(label) = &self.fork_label else {
            return truncate_to_width(&self.title, width);
        };
        match width.checked_sub(label.width()) {
            Some(room) if room > 0 => format!("{}{label}", truncate_to_width(&self.title, room)),
            _ => truncate_to_width(&self.to_string(), width),
        }
    }
}

impl fmt::Display for DisplayTitle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.title)?;
        f.write_str(self.fork_label.as_deref().unwrap_or_default())
    }
}

/// The first non-blank line of `text`, trimmed and truncated to [`MAX_TITLE_WIDTH`] columns.
pub(crate) fn fit_title(text: &str) -> String {
    let first_line = text.trim().lines().next().unwrap_or_default();
    truncate_to_width(first_line.trim_end(), MAX_TITLE_WIDTH)
}

/// Notice that `session_id` moved to `state`.
pub fn archive_notice(session_id: &str, state: ArchiveState) -> String {
    let id = id_prefix(session_id);
    match state {
        ArchiveState::Archived => format!(
            "Archived session {id}. \
             Press Ctrl+A in the `/resume` picker to list archived sessions."
        ),
        ArchiveState::Active => format!("Unarchived session {id}"),
    }
}

/// [`format_relative_time`] with each unit cut to its first letter, as in `3h ago`, for a time
/// column [`SHORT_RELATIVE_TIME_WIDTH`] wide. The date is in `now`'s offset.
pub fn format_short_relative_time(ts: OffsetDateTime, now: OffsetDateTime) -> String {
    let ts = ts.to_offset(now.offset());
    match elapsed(ts, now) {
        Some((n, (unit, _))) => format!("{n}{} ago", &unit[..1]),
        None => format_date(ts),
    }
}

/// Compact "N units ago", or the ISO date past 30 days. A future `ts` reads as 0 seconds ago.
pub fn format_relative_time(ts: OffsetDateTime, now: OffsetDateTime) -> String {
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

    use super::*;
    use crate::session::store::testing::listed_session;

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
        fn shows_the_title_or_the_untitled_marker_and_labels_a_fork() {
            for (title, forked_from, expected) in [
                (None, None, UNTITLED_MARKER),
                (Some("Fix auth flow"), None, "Fix auth flow"),
                (
                    Some("Fix auth flow"),
                    Some("abcdefghij"),
                    "Fix auth flow (fork of abcdefgh)",
                ),
                (None, Some("abcd"), "(untitled) (fork of abcd)"),
            ] {
                let info = SessionInfo {
                    title: title.map(str::to_owned),
                    forked_from: forked_from.map(str::to_owned),
                    ..listed_session("abc")
                };

                let shown = DisplayTitle::of(&info).to_string();

                assert_eq!(shown, expected, "{title:?} {forked_from:?}");
            }
        }

        #[test]
        fn fits_the_title_before_a_whole_fork_label() {
            for (forked_from, width, expected) in [
                (None, 8, "Fix a..."),
                (Some("abcdefghij"), 40, "Fix auth flow (fork of abcdefgh)"),
                (Some("abcdefghij"), 25, "Fix... (fork of abcdefgh)"),
                (Some("abcdefghij"), 19, "Fix auth flow (f..."),
            ] {
                let info = SessionInfo {
                    title: Some("Fix auth flow".to_owned()),
                    forked_from: forked_from.map(str::to_owned),
                    ..listed_session("abc")
                };

                let fitted = DisplayTitle::of(&info).fit(width);

                assert_eq!(fitted, expected, "{forked_from:?} at {width}");
            }
        }
    }

    mod fit_title {
        use super::*;

        #[test]
        fn keeps_the_trimmed_first_nonblank_line() {
            for (case, input, expected) in [
                (
                    "trim first line",
                    "  Fix auth flow  \nDetails below",
                    "Fix auth flow",
                ),
                (
                    "leading blank lines",
                    "\n  \n Fix auth flow\nDetails",
                    "Fix auth flow",
                ),
                ("blank input", " \n\t\n ", ""),
            ] {
                assert_eq!(fit_title(input), expected, "{case}");
            }
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

    mod archive_notice {
        use super::*;

        #[test]
        fn names_the_id_prefix_and_where_archived_sessions_list() {
            assert_eq!(
                archive_notice("abcdefghij", ArchiveState::Archived),
                "Archived session abcdefgh. \
                 Press Ctrl+A in the `/resume` picker to list archived sessions.",
            );
            assert_eq!(
                archive_notice("abcdefghij", ArchiveState::Active),
                "Unarchived session abcdefgh"
            );
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
