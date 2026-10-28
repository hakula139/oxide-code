//! Display helpers for session IDs, titles, and archive notices shared by every frontend.

use std::borrow::Cow;

use super::store::listing::{ArchiveState, SessionInfo};
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

/// Title from `info`, or [`UNTITLED_MARKER`] when absent. A fork, which starts with its source's
/// title, is labeled `fork of {id}` with the source's [`id_prefix`].
pub fn display_title(info: &SessionInfo) -> Cow<'_, str> {
    let title = info.title.as_deref().unwrap_or(UNTITLED_MARKER);
    match &info.forked_from {
        Some(source) => Cow::Owned(format!("{title} (fork of {})", id_prefix(source))),
        None => Cow::Borrowed(title),
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

#[cfg(test)]
mod tests {
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

                assert_eq!(display_title(&info), expected, "{title:?} {forked_from:?}");
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
}
