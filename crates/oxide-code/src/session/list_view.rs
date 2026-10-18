//! Text rendering for `ox --list`, of either active or archived sessions.

use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use time::UtcOffset;
use unicode_width::UnicodeWidthStr;

use super::display::{UNTITLED_MARKER, id_prefix};
use super::store::SessionStore;
use super::store::listing::{ListPage, ListScope};
use crate::util::path::tildify;
use crate::util::text::truncate_to_width;

const ID_COL: usize = 10;
const LAST_ACTIVE_COL: usize = 19;
const MSGS_COL: usize = 6;
const FIXED_PREFIX_WIDTH: usize = ID_COL + 1 + LAST_ACTIVE_COL + 1 + MSGS_COL + 1;

/// Smallest title budget worth truncating to, since a narrower one chops titles to `F...`.
const MIN_TITLE_BUDGET: usize = 12;

/// The 7 columns of the "Project" header plus one.
const PROJECT_COL_MIN: usize = 8;

/// Project column cap. A deeper path overflows the column in its own row, so the title column
/// keeps its width in every other row.
const PROJECT_COL_MAX: usize = 40;

/// Renders `--list` output to `out`. `scope.all` spans every project, and `scope.archived`
/// selects archived sessions. A `term_width` of `None` skips title truncation for piped output.
/// `limit` caps the rows printed, unbounded for `None`.
pub(crate) fn render_list(
    out: &mut dyn Write,
    store: &SessionStore,
    scope: ListScope,
    local_offset: UtcOffset,
    term_width: Option<usize>,
    limit: Option<usize>,
) -> Result<()> {
    let page = store.list_paged(limit, scope)?;
    render_sessions(out, &page, scope, local_offset, term_width)
}

/// Writes `page` as `--list` rows, with a `Project` column when `scope.all` is set and a
/// `... more` footer when sessions follow the page.
fn render_sessions(
    out: &mut dyn Write,
    page: &ListPage,
    scope: ListScope,
    local_offset: UtcOffset,
    term_width: Option<usize>,
) -> Result<()> {
    let ListScope { all, archived } = scope;
    let sessions = page.sessions();
    if sessions.is_empty() {
        let kind = if archived { "archived " } else { "" };
        let scope = if all { "" } else { " in this project" };
        writeln!(out, "No {kind}sessions found{scope}").context("write list output")?;
        return Ok(());
    }

    let project_col_width = if all {
        sessions
            .iter()
            .map(|s| tildify(Path::new(&s.cwd)).width())
            .max()
            .unwrap_or(0)
            .clamp(PROJECT_COL_MIN, PROJECT_COL_MAX)
    } else {
        0
    };

    let prefix_width = FIXED_PREFIX_WIDTH + if all { project_col_width + 1 } else { 0 };
    let title_budget = term_width.and_then(|w| {
        let budget = w.checked_sub(prefix_width)?;
        (budget >= MIN_TITLE_BUDGET).then_some(budget)
    });

    let mut header = format!(
        "{:<ID_COL$} {:<LAST_ACTIVE_COL$} {:<MSGS_COL$} ",
        "ID", "Last Active", "Msgs"
    );
    if all {
        _ = write!(header, "{:<project_col_width$} ", "Project");
    }
    writeln!(out, "{header}Title").context("write list header")?;

    for s in sessions {
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
        let raw_title = s
            .title
            .as_ref()
            .map_or(UNTITLED_MARKER, |t| t.title.as_str());
        let title = match title_budget {
            Some(budget) => truncate_to_width(raw_title, budget),
            None => raw_title.to_owned(),
        };
        let mut row = format!("{id:<ID_COL$} {last_active:<LAST_ACTIVE_COL$} {msgs:<MSGS_COL$} ");
        if all {
            let project = tildify(Path::new(&s.cwd));
            _ = write!(row, "{project:<project_col_width$} ");
        }
        writeln!(out, "{row}{title}").context("write list row")?;
    }
    if page.has_more() {
        writeln!(
            out,
            "... more (use `--limit N` to raise the cap or `--limit 0` to list all)"
        )
        .context("write list footer")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::session::entry::{ExitInfo, SessionInfo, TitleInfo};
    use crate::session::store::set_archived_at;
    use crate::session::store::testing::{seed_test_session, test_store};

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

    fn render_with_width(sessions: &[SessionInfo], all: bool, term_width: Option<usize>) -> String {
        let mut buf = Vec::new();
        render_sessions(
            &mut buf,
            &ListPage::new(sessions.to_vec(), false),
            ListScope {
                all,
                ..ListScope::default()
            },
            UtcOffset::UTC,
            term_width,
        )
        .unwrap();
        String::from_utf8(buf).unwrap()
    }

    // ── render_list ──

    #[test]
    fn render_list_empty_store_shows_no_sessions_notice() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut buf = Vec::new();
        render_list(
            &mut buf,
            &store,
            ListScope::default(),
            UtcOffset::UTC,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "No sessions found in this project\n",
        );

        let mut buf = Vec::new();
        render_list(
            &mut buf,
            &store,
            ListScope {
                all: true,
                ..ListScope::default()
            },
            UtcOffset::UTC,
            None,
            None,
        )
        .unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No sessions found\n",);

        let mut buf = Vec::new();
        render_list(
            &mut buf,
            &store,
            ListScope {
                archived: true,
                ..ListScope::default()
            },
            UtcOffset::UTC,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "No archived sessions found in this project\n",
        );
    }

    #[test]
    fn render_list_archived_lists_only_archived_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        for id in ["aaaa1111", "bbbb2222"] {
            seed_test_session(
                &store,
                id,
                Some(id),
                Some(1),
                datetime!(2026-04-18 09:00:00 UTC),
            );
        }
        let archived = store.find_session_path("bbbb2222").unwrap();
        set_archived_at(&archived, true).unwrap();
        let render = |archived: bool| {
            let mut buf = Vec::new();
            render_list(
                &mut buf,
                &store,
                ListScope {
                    archived,
                    ..ListScope::default()
                },
                UtcOffset::UTC,
                None,
                None,
            )
            .unwrap();
            String::from_utf8(buf).unwrap()
        };

        let active = render(false);
        assert!(active.contains("aaaa1111"), "{active}");
        assert!(!active.contains("bbbb2222"), "{active}");
        let archived = render(true);
        assert!(archived.contains("bbbb2222"), "{archived}");
        assert!(!archived.contains("aaaa1111"), "{archived}");
    }

    #[test]
    fn render_list_appends_more_footer_only_when_capped() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        for id in ["aaaa1111", "bbbb2222"] {
            seed_test_session(
                &store,
                id,
                Some(id),
                Some(1),
                datetime!(2026-04-18 09:00:00 UTC),
            );
        }
        let render = |limit: usize| {
            let mut buf = Vec::new();
            render_list(
                &mut buf,
                &store,
                ListScope::default(),
                UtcOffset::UTC,
                None,
                Some(limit),
            )
            .unwrap();
            String::from_utf8(buf).unwrap()
        };

        let capped = render(1);
        assert_eq!(
            capped.lines().last(),
            Some("... more (use `--limit N` to raise the cap or `--limit 0` to list all)"),
            "{capped}",
        );
        let full = render(2);
        assert!(!full.contains("... more"), "{full}");
    }

    // ── render_sessions ──

    #[test]
    fn render_sessions_populated_row_has_header_prefix_and_title_defaults() {
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
    fn render_sessions_shows_message_count_and_title_when_available() {
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
    fn render_sessions_all_mode_inserts_project_column_aligned_to_widest_cwd() {
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
    fn render_sessions_project_col_width_respects_maximum() {
        let long_cwd: String = "/deep/"
            .chars()
            .chain(std::iter::repeat_n('x', 80))
            .collect();
        let mut s = session("cccccccccccc", datetime!(2026-04-18 09:00:00 UTC));
        s.cwd = long_cwd.clone();
        let out = render_to_string(&[s], true);

        let row = out.lines().nth(1).unwrap();
        let title_pos = row
            .rfind("(untitled)")
            .expect("row must render the default title");
        // The cwd exceeds the 40-col cap, so the title slips past the header column, but the row
        // must still render the full cwd.
        assert!(row.contains(&long_cwd), "cwd missing from row: {row:?}");
        assert!(title_pos > 0);
    }

    #[test]
    fn render_sessions_truncates_title_when_term_width_too_narrow() {
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
    fn render_sessions_leaves_title_untruncated_without_term_width() {
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
    fn render_sessions_skips_truncation_when_title_budget_below_minimum() {
        let mut s = session("0123456789ab", datetime!(2026-04-18 09:00:00 UTC));
        let full_title = "A very long session title that will not fit";
        s.title = Some(TitleInfo {
            title: full_title.to_owned(),
            updated_at: datetime!(2026-04-18 09:05:00 UTC),
        });
        // term_width 45 < prefix 38 + MIN_TITLE_BUDGET 12 → no truncation, so the terminal wraps.
        let out = render_with_width(&[s], false, Some(45));
        assert!(
            out.contains(full_title),
            "full title should render when budget too small: {out}",
        );
    }

    #[test]
    fn render_sessions_empty_project_scope_mentions_project() {
        let out = render_to_string(&[], false);
        assert_eq!(out, "No sessions found in this project\n");
    }

    #[test]
    fn render_sessions_empty_all_scope_omits_project_qualifier() {
        let out = render_to_string(&[], true);
        assert_eq!(out, "No sessions found\n");
    }
}
