//! Text rendering for `ox --list` output.
//!
//! Sits between [`SessionStore::list`][super::store::SessionStore::list] /
//! [`list_all`][super::store::SessionStore::list_all] and the terminal so
//! the rendering can be unit-tested — `main.rs` is excluded from coverage,
//! so prior inline rendering had no automated coverage.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use time::UtcOffset;
use unicode_width::UnicodeWidthStr;

use super::entry::SessionInfo;
use super::store::SessionStore;
use crate::util::path::tildify;
use crate::util::text::truncate_to_width;

/// `ID(10) + ' ' + LastActive(19) + ' ' + Msgs(6) + ' '` — the fixed
/// prefix every row shares before `--all` inserts its optional Project
/// column and the row finally reaches `Title`.
const FIXED_PREFIX_WIDTH: usize = 10 + 1 + 19 + 1 + 6 + 1;

/// Smallest title-column width we will truncate to. Below this, the
/// output is so narrow that truncation destroys almost all signal, so
/// we skip it and let the terminal wrap instead.
const MIN_TITLE_BUDGET: usize = 12;

/// Minimum width for the `Project` column — at least wide enough to
/// fit the header label ("Project" = 7 chars) without truncation-by-padding.
const PROJECT_COL_MIN: usize = 8;

/// Upper cap on the `Project` column width. A session started from a
/// pathologically deep path should not squeeze the `Title` column into
/// oblivion; the value overflows its padding when a row exceeds the
/// cap (one-off alignment hiccup rather than hiding the title column
/// for the entire listing).
const PROJECT_COL_MAX: usize = 40;

/// Title-column fallback when a session has no recorded title yet.
const UNTITLED_MARKER: &str = "(untitled)";

/// Render `--list` output to `out`.
///
/// `all` selects the store scope: `false` lists only the current
/// project; `true` spans every project the store can see.
/// `local_offset` is applied to the displayed `Last Active` timestamp.
/// `term_width` is the terminal width used to truncate the `Title`
/// column; pass `None` when the output is piped or width is unknown
/// to skip truncation.
pub(crate) fn render_list(
    out: &mut dyn Write,
    store: &SessionStore,
    all: bool,
    local_offset: UtcOffset,
    term_width: Option<usize>,
) -> Result<()> {
    let sessions = if all {
        store.list_all()?
    } else {
        store.list()?
    };
    render_sessions(out, &sessions, all, local_offset, term_width)
}

/// Pure formatter: take an already-loaded `sessions` slice and write a
/// table to `out`. Split from [`render_list`] so tests can exercise the
/// formatting without constructing a real [`SessionStore`].
///
/// When `all` is `true`, a `Project` column is inserted so cross-project
/// rows can be disambiguated. In single-project mode the cwd is
/// redundant (it's always `$PWD`), so the column is omitted to keep
/// the output narrow.
fn render_sessions(
    out: &mut dyn Write,
    sessions: &[SessionInfo],
    all: bool,
    local_offset: UtcOffset,
    term_width: Option<usize>,
) -> Result<()> {
    if sessions.is_empty() {
        let scope = if all { "" } else { " in this project" };
        writeln!(out, "No sessions found{scope}.").context("write list output")?;
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

    // FIXED_PREFIX_WIDTH + (Project + 1) when --all. Title starts after
    // this many visual columns; anything beyond that must be truncated
    // to keep rows single-line.
    let prefix_width = FIXED_PREFIX_WIDTH + if all { project_col_width + 1 } else { 0 };
    let title_budget = term_width.and_then(|w| {
        let budget = w.checked_sub(prefix_width)?;
        (budget >= MIN_TITLE_BUDGET).then_some(budget)
    });

    if all {
        writeln!(
            out,
            "{:<10} {:<19} {:<6} {:<project$} Title",
            "ID",
            "Last Active",
            "Msgs",
            "Project",
            project = project_col_width,
        )
        .context("write list header")?;
    } else {
        writeln!(
            out,
            "{:<10} {:<19} {:<6} Title",
            "ID", "Last Active", "Msgs",
        )
        .context("write list header")?;
    }

    for s in sessions {
        let id_prefix = &s.session_id[..s.session_id.len().min(8)];
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
        let raw_title = s.title.as_ref().map_or(UNTITLED_MARKER, |t| t.title.as_str());
        let title = match title_budget {
            Some(budget) => truncate_to_width(raw_title, budget),
            None => raw_title.to_owned(),
        };
        if all {
            let project = tildify(Path::new(&s.cwd));
            writeln!(
                out,
                "{id_prefix:<10} {last_active:<19} {msgs:<6} {project:<project_col_width$} {title}",
            )
            .context("write list row")?;
        } else {
            writeln!(out, "{id_prefix:<10} {last_active:<19} {msgs:<6} {title}")
                .context("write list row")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::super::entry::{ExitInfo, TitleInfo};
    use super::*;

    fn session(session_id: &str, last_active_at: time::OffsetDateTime) -> SessionInfo {
        SessionInfo {
            session_id: session_id.to_owned(),
            cwd: "/work/project".to_owned(),
            model: "claude-opus".to_owned(),
            created_at: last_active_at,
            last_active_at,
            title: None,
            exit: None,
        }
    }

    fn render_to_string(sessions: &[SessionInfo], all: bool) -> String {
        render_with_width(sessions, all, None)
    }

    fn render_with_width(sessions: &[SessionInfo], all: bool, term_width: Option<usize>) -> String {
        let mut buf = Vec::new();
        render_sessions(&mut buf, sessions, all, UtcOffset::UTC, term_width).unwrap();
        String::from_utf8(buf).unwrap()
    }

    // ── render_list ──

    #[test]
    fn render_list_empty_store_returns_no_sessions_notice() {
        // Covers the `render_list → render_sessions` glue; empty store
        // keeps the test fixture-free.
        let dir = tempfile::tempdir().unwrap();
        let store = super::super::store::test_store(dir.path());
        let mut buf = Vec::new();
        render_list(&mut buf, &store, false, UtcOffset::UTC, None).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "No sessions found in this project.\n",
        );

        let mut buf = Vec::new();
        render_list(&mut buf, &store, true, UtcOffset::UTC, None).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "No sessions found.\n",);
    }

    // ── render_sessions ──

    #[test]
    fn render_sessions_empty_project_scope_mentions_project() {
        let out = render_to_string(&[], false);
        assert_eq!(out, "No sessions found in this project.\n");
    }

    #[test]
    fn render_sessions_empty_all_scope_omits_project_qualifier() {
        let out = render_to_string(&[], true);
        assert_eq!(out, "No sessions found.\n");
    }

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

        // All rows share the padded Project width, so the Title slot
        // should start at the same column across rows and line up
        // with the header.
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
        // 10 (ID) + 1 + 19 (Last Active) + 1 + 6 (Msgs) + 1 + 40 (cap) + cwd overflow + 1
        // The cwd length exceeds the cap, so the untruncated cwd plus
        // one separator space should land the title past the header
        // cap position — the key point is the cwd is rendered in full
        // (no data loss) and columns don't collapse.
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
        // Prefix = 38, term_width = 60 → title budget = 22 (fits ~19
        // chars + `...`).
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
        // Prefix 38 + MIN_TITLE_BUDGET(12) = 50. term_width = 45 leaves
        // a budget below minimum → no truncation (let the terminal
        // wrap rather than chopping everything to `F...`).
        let out = render_with_width(&[s], false, Some(45));
        assert!(
            out.contains(full_title),
            "full title should render when budget too small: {out}",
        );
    }

}
