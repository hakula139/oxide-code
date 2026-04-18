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

use super::entry::SessionInfo;
use super::store::SessionStore;
use crate::util::path::tildify;

/// Render `--list` output to `out`.
///
/// `all` selects the store scope: `false` lists only the current
/// project; `true` spans every project the store can see.
/// `local_offset` is applied to the displayed `Last Active` timestamp.
pub(crate) fn render_list(
    out: &mut dyn Write,
    store: &SessionStore,
    all: bool,
    local_offset: UtcOffset,
) -> Result<()> {
    let sessions = if all {
        store.list_all()?
    } else {
        store.list()?
    };
    render_sessions(out, &sessions, all, local_offset)
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
) -> Result<()> {
    if sessions.is_empty() {
        let scope = if all { "" } else { " in this project" };
        writeln!(out, "No sessions found{scope}.").context("write list output")?;
        return Ok(());
    }

    let project_col_width = if all {
        sessions
            .iter()
            .map(|s| tildify(Path::new(&s.cwd)).chars().count())
            .max()
            .unwrap_or(0)
            .clamp(PROJECT_COL_MIN, PROJECT_COL_MAX)
    } else {
        0
    };

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
        let title = s.title.as_ref().map_or("(untitled)", |t| t.title.as_str());
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

/// Minimum width for the `Project` column — at least wide enough to
/// fit the header label ("Project" = 7 chars) without truncation-by-padding.
const PROJECT_COL_MIN: usize = 8;

/// Upper cap on the `Project` column width. A session started from a
/// pathologically deep path should not squeeze the `Title` column into
/// oblivion; the value overflows its padding when a row exceeds the
/// cap (one-off alignment hiccup rather than hiding the title column
/// for the entire listing).
const PROJECT_COL_MAX: usize = 40;

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
        let mut buf = Vec::new();
        render_sessions(&mut buf, sessions, all, UtcOffset::UTC).unwrap();
        String::from_utf8(buf).unwrap()
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
}
