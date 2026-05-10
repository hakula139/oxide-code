//! Shared display-layer helpers for session listings, used by the resume picker and the
//! `/delete` confirm modal.

use std::fmt::Write as _;

use time::{OffsetDateTime, UtcOffset};

use super::entry::SessionInfo;

const SEPARATOR: &str = " · ";
pub(crate) const ID_PREFIX_WIDTH: usize = 8;
pub(crate) const UNTITLED_MARKER: &str = "(untitled)";

/// First [`ID_PREFIX_WIDTH`] bytes of `session_id`, falling back to the full id when shorter.
pub(crate) fn id_prefix(session_id: &str) -> &str {
    session_id.get(..ID_PREFIX_WIDTH).unwrap_or(session_id)
}

/// Title from `info`, or [`UNTITLED_MARKER`] when absent. Owned so callers can pass it into
/// `Box<dyn Modal>` constructors that outlive the borrow.
pub(crate) fn display_title(info: &SessionInfo) -> String {
    info.title
        .as_ref()
        .map_or_else(|| UNTITLED_MARKER.to_owned(), |t| t.title.clone())
}

/// Compact "N units ago" with an ISO-date fallback past 30 days. Negative deltas (clock skew or
/// future stamps) collapse to 0 so the singular / plural axis stays sane.
pub(crate) fn format_relative_time(ts: OffsetDateTime, now: OffsetDateTime) -> String {
    let secs = (now - ts).whole_seconds().max(0);
    let (n, unit) = if secs < 60 {
        (secs, "second")
    } else if secs < 3600 {
        (secs / 60, "minute")
    } else if secs < 86_400 {
        (secs / 3600, "hour")
    } else if secs < 30 * 86_400 {
        (secs / 86_400, "day")
    } else {
        return ts
            .format(time::macros::format_description!("[year]-[month]-[day]"))
            .expect("static `[year]-[month]-[day]` description never fails on a valid ts");
    };
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural} ago")
}

/// `{id_prefix} · {when} · {N msgs} · {branch} · {project}`. Trailing components are omitted when
/// their inputs are zero or `None`. Caller decides whether to include the project: the picker
/// hides it in scoped mode (every row shares the project), the delete confirm always shows it.
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
        let unit = if message_count == 1 { "msg" } else { "msgs" };
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
    use super::*;
    use crate::session::entry::TitleInfo;

    use time::macros::datetime;

    // ── id_prefix ──

    #[test]
    fn id_prefix_returns_first_eight_bytes_or_full_id_when_shorter() {
        // Pin both the long-id slicing and the short-id fallback so a future change to direct
        // `&id[..8]` slicing would panic on short ids and break this test.
        assert_eq!(id_prefix("abcdefghij"), "abcdefgh");
        assert_eq!(id_prefix("abcd"), "abcd");
        assert_eq!(id_prefix(""), "");
    }

    // ── display_title ──

    #[test]
    fn display_title_uses_title_when_present_else_untitled_marker() {
        let mut info = SessionInfo {
            session_id: "abc".to_owned(),
            cwd: String::new(),
            last_active_at: datetime!(2026-05-08 09:00:00 UTC),
            title: None,
            exit: None,
            git_branch: None,
        };
        assert_eq!(display_title(&info), UNTITLED_MARKER);

        info.title = Some(TitleInfo {
            title: "Fix auth flow".to_owned(),
            updated_at: datetime!(2026-05-08 09:00:00 UTC),
        });
        assert_eq!(display_title(&info), "Fix auth flow");
    }

    // ── format_relative_time ──

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
            "future stamps collapse to 0 rather than negative",
        );
    }

    // ── format_metadata_line ──

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
