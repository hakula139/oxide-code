//! Display helpers for session IDs, titles, and archive notices shared by every frontend.

use super::entry::{ArchiveState, SessionInfo};
use crate::util::text::truncate_to_width;

pub const ID_PREFIX_WIDTH: usize = 8;
pub const UNTITLED_MARKER: &str = "(untitled)";

/// Column cap on every title, leaving room in a 120-column `--list` row after the fixed
/// `ID / Last Active / Msgs` prefix.
pub const MAX_TITLE_WIDTH: usize = 80;

/// First [`ID_PREFIX_WIDTH`] bytes of `session_id`, or the full ID when shorter.
pub fn id_prefix(session_id: &str) -> &str {
    session_id.get(..ID_PREFIX_WIDTH).unwrap_or(session_id)
}

/// Title from `info`, or [`UNTITLED_MARKER`] when absent.
pub fn display_title(info: &SessionInfo) -> &str {
    info.title.as_ref().map_or(UNTITLED_MARKER, |t| &t.title)
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

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::session::entry::TitleInfo;
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
        fn uses_title_when_present_else_untitled_marker() {
            let mut info = listed_session("abc");
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
}
