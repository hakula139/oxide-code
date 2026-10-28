//! `ox --list`: a table of the active or archived sessions.

use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

use anyhow::{Context as _, Result};
use time::UtcOffset;
use unicode_width::UnicodeWidthStr as _;

use ox_core::session::display::{DisplayTitle, id_prefix};
use ox_core::session::store::SessionStore;
use ox_core::session::store::listing::{ArchiveState, SessionInfo};
use ox_core::session::store::listing::{ListScope, ProjectScope};
use ox_core::util;
use ox_core::util::path::tildify;
use ox_core::util::text::{escape_line, pad_to_width};

const ID_COL: usize = 10;
const LAST_ACTIVE_COL: usize = 19;
const MSGS_COL: usize = 6;
const FIXED_PREFIX_WIDTH: usize = ID_COL + 1 + LAST_ACTIVE_COL + 1 + MSGS_COL + 1;

/// Smallest title budget worth truncating to, since a narrower one chops titles to `F...`.
const MIN_TITLE_BUDGET: usize = 12;

const PROJECT_COL_MIN: usize = "Project".len() + 1;

/// Project column cap. A deeper path overflows the column in its own row, so the title column keeps
/// its width in every other row.
const PROJECT_COL_MAX: usize = 40;

/// Writes the sessions of `store`'s project, or of every project when `scope` spans them all, to
/// `out`. `limit` caps the rows, where `0` lists them all. Titles are cut to `term_width`, the
/// width of a terminal on stdout, or left whole without one.
pub(crate) fn write(
    out: &mut impl Write,
    store: &SessionStore,
    scope: ListScope,
    limit: usize,
    term_width: Option<usize>,
) -> Result<()> {
    let page = store.list_paged((limit > 0).then_some(limit), scope)?;
    let table = render(
        page.sessions(),
        page.has_more(),
        scope,
        util::time::local_offset(),
        term_width,
    );
    out.write_all(table.as_bytes())
        .context("failed to write the session list")
}

/// The `--list` table of `sessions`, with a `Project` column when `scope` spans every project and a
/// `... more` footer when `more` is set. A `term_width` of `None` skips title truncation for piped
/// output.
fn render(
    sessions: &[SessionInfo],
    more: bool,
    scope: ListScope,
    local_offset: UtcOffset,
    term_width: Option<usize>,
) -> String {
    let all = scope.projects == ProjectScope::Every;
    if sessions.is_empty() {
        let kind = match scope.archive {
            ArchiveState::Active => "",
            ArchiveState::Archived => "archived ",
        };
        let scope = if all {
            ""
        } else {
            " in this project (use --all to search every project)"
        };
        return format!("No {kind}sessions found{scope}\n");
    }

    let projects: Vec<String> = if all {
        sessions
            .iter()
            .map(|s| escape_line(&tildify(Path::new(&s.project_root))))
            .collect()
    } else {
        Vec::new()
    };
    let project_col_width = projects
        .iter()
        .map(|project| project.width())
        .max()
        .map_or(0, |widest| widest.clamp(PROJECT_COL_MIN, PROJECT_COL_MAX));

    let prefix_width = FIXED_PREFIX_WIDTH + if all { project_col_width + 1 } else { 0 };
    let title_budget = term_width.and_then(|w| {
        let budget = w.checked_sub(prefix_width)?;
        (budget >= MIN_TITLE_BUDGET).then_some(budget)
    });

    let mut table = format!(
        "{:<ID_COL$} {:<LAST_ACTIVE_COL$} {:<MSGS_COL$} ",
        "ID", "Last Active", "Msgs"
    );
    if all {
        _ = write!(table, "{:<project_col_width$} ", "Project");
    }
    table.push_str("Title\n");

    for (i, s) in sessions.iter().enumerate() {
        let id = id_prefix(&s.session_id);
        let last_active = s
            .last_active_at
            .to_offset(local_offset)
            .format(time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]"
            ))
            .unwrap_or_default();
        let msgs = s.message_count.to_string();
        let title = DisplayTitle::of(s);
        let title = match title_budget {
            Some(budget) => title.fit(budget),
            None => title.to_string(),
        };
        _ = write!(
            table,
            "{id:<ID_COL$} {last_active:<LAST_ACTIVE_COL$} {msgs:<MSGS_COL$} "
        );
        if let Some(project) = projects.get(i) {
            _ = write!(table, "{} ", pad_to_width(project, project_col_width));
        }
        _ = writeln!(table, "{title}");
    }
    if more {
        table.push_str("... more (use `--limit N` to raise the cap or `--limit 0` to list all)\n");
    }
    table
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use ox_core::session::store::testing::{
        archive_test_session, listed_session, seed_test_session,
    };

    use super::*;
    use crate::driver::testing::ClosedPipe;

    mod write {
        use super::*;

        /// The list `write` prints for `scope` and `limit` from `store`'s project.
        fn listed(store: &SessionStore, scope: ListScope, limit: usize) -> String {
            let mut out = Vec::new();
            write(&mut out, store, scope, limit, None).unwrap();
            String::from_utf8(out).unwrap()
        }

        #[test]
        fn lists_by_scope_limit_and_archive_state() {
            let dir = tempfile::tempdir().unwrap();
            let local = SessionStore::open_at(dir.path().to_path_buf(), "local").unwrap();
            seed_test_session(
                &local,
                "local-session",
                Some("Local session"),
                2,
                datetime!(2026-01-01 00:00 UTC),
            );
            let other = SessionStore::open_at(dir.path().to_path_buf(), "other").unwrap();
            seed_test_session(
                &other,
                "other-session",
                Some("Other session"),
                3,
                datetime!(2026-01-02 00:00 UTC),
            );
            seed_test_session(
                &other,
                "archived-session",
                Some("Archived session"),
                4,
                datetime!(2026-01-03 00:00 UTC),
            );
            archive_test_session(&other, "archived-session");
            let every = ListScope {
                projects: ProjectScope::Every,
                ..ListScope::default()
            };

            let local_list = listed(&local, ListScope::default(), 0);
            let all = listed(&local, every, 0);
            let capped = listed(&local, every, 1);
            let archived = listed(
                &local,
                ListScope {
                    archive: ArchiveState::Archived,
                    ..every
                },
                0,
            );

            assert!(local_list.contains("Local session"), "{local_list}");
            assert!(!local_list.contains("Other session"), "{local_list}");
            assert!(!local_list.contains("Project"), "{local_list}");
            assert!(all.contains("Local session"), "{all}");
            assert!(all.contains("Other session"), "{all}");
            assert!(!all.contains("Archived session"), "{all}");
            assert!(all.contains("Project"), "{all}");
            assert!(!all.contains("... more"), "{all}");
            assert!(capped.contains("Other session"), "{capped}");
            assert!(!capped.contains("Local session"), "{capped}");
            assert_eq!(
                capped.lines().last(),
                Some("... more (use `--limit N` to raise the cap or `--limit 0` to list all)"),
            );
            assert!(archived.contains("Archived session"), "{archived}");
            assert!(!archived.contains("Local session"), "{archived}");
            assert!(!archived.contains("Other session"), "{archived}");
        }

        #[test]
        fn names_the_failed_write() {
            let dir = tempfile::tempdir().unwrap();
            let store = SessionStore::open_at(dir.path().to_path_buf(), "local").unwrap();

            let error = write(&mut ClosedPipe, &store, ListScope::default(), 0, None).unwrap_err();

            let message = format!("{error:#}");
            assert!(
                message.starts_with("failed to write the session list: "),
                "{message}"
            );
        }
    }

    mod render {
        use super::*;

        fn session(session_id: &str, last_active_at: time::OffsetDateTime) -> SessionInfo {
            SessionInfo {
                project_root: "/work/project".to_owned(),
                last_active_at,
                ..listed_session(session_id)
            }
        }

        fn rendered(sessions: &[SessionInfo], all: bool, term_width: Option<usize>) -> String {
            render(
                sessions,
                false,
                ListScope {
                    projects: if all {
                        ProjectScope::Every
                    } else {
                        ProjectScope::Current
                    },
                    ..ListScope::default()
                },
                UtcOffset::UTC,
                term_width,
            )
        }

        #[test]
        fn populated_row_has_header_prefix_and_title_defaults() {
            let s = session("0123456789abcdef", datetime!(2026-04-18 13:45:00 UTC));

            let out = rendered(&[s], false, None);

            let mut lines = out.lines();
            assert_eq!(
                lines.next().unwrap(),
                "ID         Last Active         Msgs   Title"
            );
            let row = lines.next().unwrap();
            assert!(row.starts_with("01234567   "), "got: {row:?}");
            assert!(row.contains("2026-04-18 13:45"), "got: {row:?}");
            assert!(row.ends_with(" 0      (untitled)"), "got: {row:?}");
            assert!(lines.next().is_none(), "unexpected trailing line");
        }

        #[test]
        fn shows_message_count_and_title_when_available() {
            let mut s = session("feeddeadbeef0000", datetime!(2026-04-18 09:00:00 UTC));
            s.title = Some("Fix auth bug".to_owned());
            s.message_count = 42;

            let out = rendered(&[s], false, None);

            let row = out.lines().nth(1).unwrap();
            assert!(row.contains(" 42     "), "got: {row:?}");
            assert!(row.ends_with("Fix auth bug"), "got: {row:?}");
        }

        #[test]
        fn all_mode_inserts_project_column_aligned_to_widest_cwd() {
            let mut short = session("aaaaaaaaaaaa", datetime!(2026-04-18 09:00:00 UTC));
            short.project_root = "/a".to_owned();
            let mut longer = session("bbbbbbbbbbbb", datetime!(2026-04-18 09:05:00 UTC));
            longer.project_root = "/work/oxide-code".to_owned();

            let out = rendered(&[short, longer], true, None);

            let mut lines = out.lines();
            let header = lines.next().unwrap();
            assert!(
                header.contains("Project"),
                "header should mention Project: {header:?}"
            );
            let rows: Vec<&str> = lines.collect();
            assert_eq!(rows.len(), 2);
            assert!(rows[0].contains("/a"), "row 0 missing cwd: {:?}", rows[0]);
            assert!(
                rows[1].contains("/work/oxide-code"),
                "row 1 missing cwd: {:?}",
                rows[1],
            );
            let header_title_col = header.find("Title").expect("header must contain Title");
            let row_title_cols: Vec<usize> = rows
                .iter()
                .map(|r| {
                    r.rfind("(untitled)")
                        .expect("row must render default title")
                })
                .collect();
            assert_eq!(row_title_cols[0], row_title_cols[1], "titles misaligned");
            assert_eq!(
                row_title_cols[0], header_title_col,
                "title column misaligned with header",
            );
        }

        #[test]
        fn pads_wide_character_projects_by_display_width() {
            let mut wide = session("aaaaaaaaaaaa", datetime!(2026-04-18 09:00:00 UTC));
            wide.project_root = "/work/项目代码".to_owned();
            let mut ascii = session("bbbbbbbbbbbb", datetime!(2026-04-18 09:05:00 UTC));
            ascii.project_root = "/work/abcdefghij".to_owned();
            let widest = ascii.project_root.width();

            let out = rendered(&[wide, ascii], true, None);

            let title_cols: Vec<usize> = out
                .lines()
                .map(|line| {
                    let start = line.rfind("Title").or_else(|| line.rfind("(untitled)"));
                    line[..start.expect("every line has a title cell")].width()
                })
                .collect();
            assert_eq!(title_cols, [FIXED_PREFIX_WIDTH + widest + 1; 3], "{out}");
        }

        #[test]
        fn project_col_width_respects_minimum() {
            let mut s = session("cccccccccccc", datetime!(2026-04-18 09:00:00 UTC));
            s.project_root = "/a".to_owned();

            let out = rendered(&[s], true, None);

            let mut lines = out.lines();
            assert_eq!(
                lines.next(),
                Some("ID         Last Active         Msgs   Project  Title"),
            );
            let row = lines.next().unwrap();
            assert!(row.ends_with(" /a       (untitled)"), "{row:?}");
        }

        #[test]
        fn project_col_width_respects_maximum() {
            let long_cwd: String = "/deep/"
                .chars()
                .chain(std::iter::repeat_n('x', 80))
                .collect();
            let mut s = session("cccccccccccc", datetime!(2026-04-18 09:00:00 UTC));
            s.project_root = long_cwd.clone();

            let out = rendered(&[s], true, None);

            let mut lines = out.lines();
            let header = lines.next().unwrap();
            assert_eq!(
                header.find("Title"),
                Some(FIXED_PREFIX_WIDTH + PROJECT_COL_MAX + 1),
                "{header:?}",
            );
            let row = lines.next().unwrap();
            assert!(
                row.ends_with(&format!("{long_cwd} (untitled)")),
                "an overlong cwd overflows its column in full: {row:?}",
            );
        }

        #[test]
        fn shows_the_project_root_over_the_launch_directory() {
            let mut s = session("eeeeeeeeeeee", datetime!(2026-04-18 09:00:00 UTC));
            s.cwd = "/work/repo/crates/core".to_owned();
            s.project_root = "/work/repo".to_owned();

            let out = rendered(&[s], true, None);

            let projects: Vec<_> = out
                .lines()
                .skip(1)
                .filter_map(|row| row.split_whitespace().nth(4))
                .collect();
            assert_eq!(projects, ["/work/repo"], "{out}");
        }

        #[test]
        fn escapes_control_characters_in_titles_and_projects() {
            let mut s = session("dddddddddddd", datetime!(2026-04-18 09:00:00 UTC));
            s.project_root = "/work/a\nb".to_owned();
            s.title = Some("Fix\u{1b}]52;c;eA==\u{7} it\nnow".to_owned());

            let out = rendered(&[s], true, None);

            let rows: Vec<&str> = out.lines().collect();
            assert_eq!(rows.len(), 2, "{out}");
            assert_eq!(
                rows[1].split_once(" 0      ").map(|(_, cells)| cells),
                Some("/work/a\\nb Fix\\u{1b}]52;c;eA==\\u{7} it\\nnow"),
            );
        }

        #[test]
        fn truncates_title_when_term_width_too_narrow() {
            let mut s = session("0123456789ab", datetime!(2026-04-18 09:00:00 UTC));
            s.title = Some("A very long session title that will not fit".to_owned());

            // Prefix = 38, term_width = 60 → title budget = 22 (fits ~19 chars + `...`).
            let out = rendered(&[s], false, Some(60));

            let row = out.lines().nth(1).unwrap();
            assert!(row.ends_with("..."), "expected ellipsis, got: {row:?}");
            assert_eq!(row.width(), 60, "{row:?}");
        }

        #[test]
        fn title_budget_leaves_room_for_the_project_column() {
            let mut s = session("0123456789ab", datetime!(2026-04-18 09:00:00 UTC));
            s.title = Some("A very long session title that will not fit".to_owned());

            // Prefix = 38 + `/work/project` (13) + 1 = 52, term_width = 70 → title budget = 18.
            let out = rendered(&[s], true, Some(70));

            let row = out.lines().nth(1).unwrap();
            assert!(row.ends_with("..."), "expected ellipsis, got: {row:?}");
            assert_eq!(row.width(), 70, "{row:?}");
        }

        #[test]
        fn leaves_title_untruncated_without_term_width() {
            let mut s = session("0123456789ab", datetime!(2026-04-18 09:00:00 UTC));
            let full_title = "A very long session title that will not fit";
            s.title = Some(full_title.to_owned());

            let out = rendered(&[s], false, None);

            assert!(
                out.contains(full_title),
                "full title should render when term_width is None: {out}",
            );
        }

        #[test]
        fn skips_truncation_when_title_budget_below_minimum() {
            let mut s = session("0123456789ab", datetime!(2026-04-18 09:00:00 UTC));
            let full_title = "A very long session title that will not fit";
            s.title = Some(full_title.to_owned());

            // term_width 45 < prefix 38 + MIN_TITLE_BUDGET 12 → untruncated, so the terminal wraps.
            let out = rendered(&[s], false, Some(45));

            assert!(
                out.contains(full_title),
                "full title should render when budget too small: {out}",
            );
        }

        #[test]
        fn names_the_scope_and_archive_state_when_empty() {
            let empty = |projects, archive| {
                render(
                    &[],
                    false,
                    ListScope { projects, archive },
                    UtcOffset::UTC,
                    None,
                )
            };

            assert_eq!(
                empty(ProjectScope::Current, ArchiveState::Active),
                "No sessions found in this project (use --all to search every project)\n"
            );
            assert_eq!(
                empty(ProjectScope::Every, ArchiveState::Active),
                "No sessions found\n"
            );
            assert_eq!(
                empty(ProjectScope::Current, ArchiveState::Archived),
                "No archived sessions found in this project (use --all to search every project)\n"
            );
        }
    }
}
