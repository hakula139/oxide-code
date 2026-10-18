//! Shared display-layer helpers for session titles, listings, and archive notices, used by the
//! resume picker, the `/delete` confirm modal, the archive commands, and `/rename`.

use std::fmt::Write as _;

use time::{OffsetDateTime, UtcOffset};

use super::entry::SessionInfo;
use crate::util::text::{plural, truncate_to_width};

const SEPARATOR: &str = " · ";
const ID_PREFIX_WIDTH: usize = 8;
pub(crate) const UNTITLED_MARKER: &str = "(untitled)";

/// Column cap on first-prompt and manual titles, leaving room in a 120-column `--list` row after
/// the fixed `ID / Last Active / Msgs` prefix.
pub(crate) const MAX_TITLE_WIDTH: usize = 80;

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

/// The first non-blank line of `text`, trimmed and truncated to [`MAX_TITLE_WIDTH`] columns.
pub(crate) fn fit_title(text: &str) -> String {
    let first_line = text.trim().lines().next().unwrap_or_default();
    truncate_to_width(first_line.trim_end(), MAX_TITLE_WIDTH)
}

/// Compact "N units ago" with an ISO-date fallback past 30 days. Negative deltas (clock skew or
/// future stamps) collapse to 0 so the singular / plural axis stays sane.
fn format_relative_time(ts: OffsetDateTime, now: OffsetDateTime) -> String {
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
    let suffix = plural(n, "", "s");
    format!("{n} {unit}{suffix} ago")
}

/// `{id_prefix} · {when} · {N msgs} · {branch} · {project}`. Trailing components are omitted when
/// their inputs are zero or `None`. Caller decides whether to include the project: the picker
/// hides it in scoped mode (every row shares the project), and the delete confirm always shows it.
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

    // ── id_prefix ──

    #[test]
    fn id_prefix_uses_first_eight_bytes_or_full_short_id() {
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

    // ── fit_title ──

    #[test]
    fn fit_title_keeps_the_trimmed_first_line() {
        assert_eq!(
            fit_title("  Fix auth flow  \nDetails below"),
            "Fix auth flow"
        );
    }

    #[test]
    fn fit_title_skips_leading_blank_lines() {
        assert_eq!(fit_title("\n  \n Fix auth flow\nDetails"), "Fix auth flow");
    }

    #[test]
    fn fit_title_of_blank_text_is_empty() {
        assert_eq!(fit_title(" \n\t\n "), "");
    }

    #[test]
    fn fit_title_truncates_past_the_column_cap() {
        let exact = "\u{00e9}".repeat(MAX_TITLE_WIDTH);
        assert_eq!(fit_title(&exact), exact);

        let long = "\u{00e9}".repeat(MAX_TITLE_WIDTH + 1);
        assert_eq!(
            fit_title(&long),
            format!("{}...", "\u{00e9}".repeat(MAX_TITLE_WIDTH - 3)),
        );
    }

    #[test]
    fn fit_title_counts_wide_chars_by_column() {
        assert_eq!(
            fit_title(&"界".repeat(MAX_TITLE_WIDTH)),
            format!("{}...", "界".repeat(38)),
        );
    }

    // ── format_relative_time ──

    #[test]
    fn format_relative_time_pluralizes_units_and_falls_back_to_iso_date_past_30_days() {
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

    // ── format_metadata_line ──

    #[test]
    fn format_metadata_line_includes_msg_count_branch_and_project_when_supplied() {
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
    fn format_metadata_line_singular_msg_unit_at_count_one() {
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
    fn format_metadata_line_omits_msg_count_branch_and_project_when_absent() {
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

    // ── archive_notice ──

    #[test]
    fn archive_notice_names_the_id_prefix_and_where_archived_sessions_list() {
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
