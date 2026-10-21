//! `ox --list`: a table of the active or archived sessions.

use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use time::UtcOffset;
use unicode_width::UnicodeWidthStr;

use ox_core::session::display::{UNTITLED_MARKER, id_prefix};
use ox_core::session::entry::SessionInfo;
use ox_core::session::store::SessionStore;
use ox_core::session::store::listing::ListScope;
use ox_core::util;
use ox_core::util::path::tildify;
use ox_core::util::text::{escape_line, pad_to_width, truncate_to_width};
use ox_tui::terminal;

const ID_COL: usize = 10;
const LAST_ACTIVE_COL: usize = 19;
const MSGS_COL: usize = 6;
const FIXED_PREFIX_WIDTH: usize = ID_COL + 1 + LAST_ACTIVE_COL + 1 + MSGS_COL + 1;

/// Smallest title budget worth truncating to, since a narrower one chops titles to `F...`.
const MIN_TITLE_BUDGET: usize = 12;

const PROJECT_COL_MIN: usize = "Project".len() + 1;

/// Project column cap. A deeper path overflows the column in its own row, so the title column
/// keeps its width in every other row.
const PROJECT_COL_MAX: usize = 40;

/// Writes the sessions of the project at `cwd`, or of every project when `scope.all` is set, to
/// `out`. `limit` caps the rows, where `0` lists them all. Titles are cut to the width of a
/// terminal on stdout.
pub(crate) fn write(
    out: &mut impl Write,
    cwd: &Path,
    scope: ListScope,
    limit: usize,
) -> Result<()> {
    let store = SessionStore::open(cwd)?;
    let page = store.list_paged((limit > 0).then_some(limit), scope)?;
    let table = render(
        page.sessions(),
        page.has_more(),
        scope,
        util::time::local_offset(),
        terminal::stdout_width(),
    );
    out.write_all(table.as_bytes())
        .context("failed to write the session list")
}

/// The `--list` table of `sessions`, with a `Project` column when `scope.all` is set and a
/// `... more` footer when `more` sessions follow. A `term_width` of `None` skips title truncation
/// for piped output.
fn render(
    sessions: &[SessionInfo],
    more: bool,
    scope: ListScope,
    local_offset: UtcOffset,
    term_width: Option<usize>,
) -> String {
    let ListScope { all, archived } = scope;
    if sessions.is_empty() {
        let kind = if archived { "archived " } else { "" };
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
            .map(|s| escape_line(&tildify(Path::new(&s.cwd))))
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
        let msgs = s
            .exit
            .as_ref()
            .map_or("-".to_owned(), |e| e.message_count.to_string());
        let raw_title = escape_line(
            s.title
                .as_ref()
                .map_or(UNTITLED_MARKER, |t| t.title.as_str()),
        );
        let title = match title_budget {
            Some(budget) => truncate_to_width(&raw_title, budget),
            None => raw_title,
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

    use ox_core::session::entry::{ExitInfo, TitleInfo};
    use ox_core::session::store::testing::{archive_test_session, seed_test_session};

    use super::*;

    mod write {
        use super::*;

        /// The list `write` prints for `scope` and `limit` from the project at `cwd`.
        fn listed(cwd: &Path, scope: ListScope, limit: usize) -> String {
            let mut out = Vec::new();
            write(&mut out, cwd, scope, limit).unwrap();
            String::from_utf8(out).unwrap()
        }

        #[test]
        fn lists_by_scope_limit_and_archive_state() {
            let dir = tempfile::tempdir().unwrap();
            let cwd = Path::new("/work/local");
            let every = ListScope {
                all: true,
                ..ListScope::default()
            };
            temp_env::with_var("XDG_DATA_HOME", Some(dir.path()), || {
                let store = SessionStore::open(cwd).unwrap();
                seed_test_session(
                    &store,
                    "local-session",
                    Some("Local session"),
                    Some(2),
                    datetime!(2026-01-01 00:00 UTC),
                );
                let other = SessionStore::open_at(dir.path().join("ox/sessions"), "other").unwrap();
                seed_test_session(
                    &other,
                    "other-session",
                    Some("Other session"),
                    Some(3),
                    datetime!(2026-01-02 00:00 UTC),
                );

                let local = listed(cwd, ListScope::default(), 0);
                let all = listed(cwd, every, 0);
                let capped = listed(cwd, every, 1);
                archive_test_session(&other, "other-session");
                let archived = listed(
                    cwd,
                    ListScope {
                        archived: true,
                        ..every
                    },
                    0,
                );

                assert!(local.contains("Local session"), "{local}");
                assert!(!local.contains("Other session"), "{local}");
                assert!(!local.contains("Project"), "{local}");
                assert!(all.contains("Local session"), "{all}");
                assert!(all.contains("Other session"), "{all}");
                assert!(all.contains("Project"), "{all}");
                assert!(!all.contains("... more"), "{all}");
                assert!(capped.contains("Other session"), "{capped}");
                assert!(!capped.contains("Local session"), "{capped}");
                assert_eq!(
                    capped.lines().last(),
                    Some("... more (use `--limit N` to raise the cap or `--limit 0` to list all)"),
                );
                assert!(archived.contains("Other session"), "{archived}");
                assert!(!archived.contains("Local session"), "{archived}");
            });
        }
    }

    mod render {
        use super::*;

        fn session(session_id: &str, last_active_at: time::OffsetDateTime) -> SessionInfo {
            SessionInfo {
                session_id: session_id.to_owned(),
                path: std::path::PathBuf::new(),
                cwd: "/work/project".to_owned(),
                last_active_at,
                title: None,
                exit: None,
                git_branch: None,
                archived: false,
            }
        }

        fn render_to_string(sessions: &[SessionInfo], all: bool) -> String {
            render_with_width(sessions, all, None)
        }

        fn render_with_width(
            sessions: &[SessionInfo],
            all: bool,
            term_width: Option<usize>,
        ) -> String {
            render(
                sessions,
                false,
                ListScope {
                    all,
                    ..ListScope::default()
                },
                UtcOffset::UTC,
                term_width,
            )
        }

        #[test]
        fn populated_row_has_header_prefix_and_title_defaults() {
            let s = session("0123456789abcdef", datetime!(2026-04-18 13:45:00 UTC));

            let out = render_to_string(&[s], false);

            let mut lines = out.lines();
            assert_eq!(
                lines.next().unwrap(),
                "ID         Last Active         Msgs   Title"
            );
            let row = lines.next().unwrap();
            assert!(row.starts_with("01234567   "), "got: {row:?}");
            assert!(row.contains("2026-04-18 13:45"), "got: {row:?}");
            assert!(row.ends_with(" -      (untitled)"), "got: {row:?}");
            assert!(lines.next().is_none(), "unexpected trailing line");
        }

        #[test]
        fn shows_message_count_and_title_when_available() {
            let mut s = session("feeddeadbeef0000", datetime!(2026-04-18 09:00:00 UTC));
            s.title = Some(TitleInfo {
                title: "Fix auth bug".to_owned(),
                updated_at: datetime!(2026-04-18 09:05:00 UTC),
            });
            s.exit = Some(ExitInfo {
                message_count: 42,
                updated_at: datetime!(2026-04-18 09:30:00 UTC),
            });

            let out = render_to_string(&[s], false);

            let row = out.lines().nth(1).unwrap();
            assert!(row.contains(" 42     "), "got: {row:?}");
            assert!(row.ends_with("Fix auth bug"), "got: {row:?}");
        }

        #[test]
        fn all_mode_inserts_project_column_aligned_to_widest_cwd() {
            let mut short = session("aaaaaaaaaaaa", datetime!(2026-04-18 09:00:00 UTC));
            short.cwd = "/a".to_owned();
            let mut longer = session("bbbbbbbbbbbb", datetime!(2026-04-18 09:05:00 UTC));
            longer.cwd = "/work/oxide-code".to_owned();

            let out = render_to_string(&[short, longer], true);

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
            wide.cwd = "/work/项目代码".to_owned();
            let mut ascii = session("bbbbbbbbbbbb", datetime!(2026-04-18 09:05:00 UTC));
            ascii.cwd = "/work/abcdefghij".to_owned();
            let widest = ascii.cwd.width();

            let out = render_to_string(&[wide, ascii], true);

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
            s.cwd = "/a".to_owned();

            let out = render_to_string(&[s], true);

            let mut lines = out.lines();
            let header = lines.next().unwrap();
            let row = lines.next().unwrap();
            let title_col = FIXED_PREFIX_WIDTH + PROJECT_COL_MIN + 1;
            assert_eq!(header.find("Title"), Some(title_col), "{header:?}");
            assert_eq!(row.find("(untitled)"), Some(title_col), "{row:?}");
        }

        #[test]
        fn project_col_width_respects_maximum() {
            let long_cwd: String = "/deep/"
                .chars()
                .chain(std::iter::repeat_n('x', 80))
                .collect();
            let mut s = session("cccccccccccc", datetime!(2026-04-18 09:00:00 UTC));
            s.cwd = long_cwd.clone();

            let out = render_to_string(&[s], true);

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
        fn escapes_control_characters_in_titles_and_projects() {
            let mut s = session("dddddddddddd", datetime!(2026-04-18 09:00:00 UTC));
            s.cwd = "/work/a\nb".to_owned();
            s.title = Some(TitleInfo {
                title: "Fix\u{1b}]52;c;eA==\u{7} it\nnow".to_owned(),
                updated_at: datetime!(2026-04-18 09:05:00 UTC),
            });

            let out = render_to_string(&[s], true);

            let rows: Vec<&str> = out.lines().collect();
            assert_eq!(rows.len(), 2, "{out}");
            assert_eq!(
                rows[1].split_once(" -      ").map(|(_, cells)| cells),
                Some("/work/a\\nb Fix\\u{1b}]52;c;eA==\\u{7} it\\nnow"),
            );
        }

        #[test]
        fn truncates_title_when_term_width_too_narrow() {
            let mut s = session("0123456789ab", datetime!(2026-04-18 09:00:00 UTC));
            s.title = Some(TitleInfo {
                title: "A very long session title that will not fit".to_owned(),
                updated_at: datetime!(2026-04-18 09:05:00 UTC),
            });

            // Prefix = 38, term_width = 60 → title budget = 22 (fits ~19 chars + `...`).
            let out = render_with_width(&[s], false, Some(60));

            let row = out.lines().nth(1).unwrap();
            let title = row
                .split_once("-      ")
                .map(|(_, t)| t)
                .expect("row must have the Msgs cell");
            assert!(title.ends_with("..."), "expected ellipsis, got: {title:?}");
            assert!(
                title.width() <= 22,
                "title width {} exceeds budget for {title:?}",
                title.width(),
            );
        }

        #[test]
        fn leaves_title_untruncated_without_term_width() {
            let mut s = session("0123456789ab", datetime!(2026-04-18 09:00:00 UTC));
            let full_title = "A very long session title that will not fit";
            s.title = Some(TitleInfo {
                title: full_title.to_owned(),
                updated_at: datetime!(2026-04-18 09:05:00 UTC),
            });

            let out = render_with_width(&[s], false, None);

            assert!(
                out.contains(full_title),
                "full title should render when term_width is None: {out}",
            );
        }

        #[test]
        fn skips_truncation_when_title_budget_below_minimum() {
            let mut s = session("0123456789ab", datetime!(2026-04-18 09:00:00 UTC));
            let full_title = "A very long session title that will not fit";
            s.title = Some(TitleInfo {
                title: full_title.to_owned(),
                updated_at: datetime!(2026-04-18 09:05:00 UTC),
            });

            // term_width 45 < prefix 38 + MIN_TITLE_BUDGET 12 → untruncated, so the terminal wraps.
            let out = render_with_width(&[s], false, Some(45));

            assert!(
                out.contains(full_title),
                "full title should render when budget too small: {out}",
            );
        }

        #[test]
        fn names_the_scope_and_archive_state_when_empty() {
            let empty = |all, archived| {
                render(
                    &[],
                    false,
                    ListScope { all, archived },
                    UtcOffset::UTC,
                    None,
                )
            };

            assert_eq!(
                empty(false, false),
                "No sessions found in this project (use --all to search every project)\n"
            );
            assert_eq!(empty(true, false), "No sessions found\n");
            assert_eq!(
                empty(false, true),
                "No archived sessions found in this project (use --all to search every project)\n"
            );
        }
    }
}
