//! Text rendering for `ox --list` output.
//!
//! Sits between [`SessionStore::list`][super::store::SessionStore::list] /
//! [`list_all`][super::store::SessionStore::list_all] and the terminal so
//! the rendering can be unit-tested — `main.rs` is excluded from coverage,
//! so prior inline rendering had no automated coverage.

use std::io::Write;

use anyhow::{Context, Result};
use time::UtcOffset;

use super::entry::SessionInfo;
use super::store::SessionStore;

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

    writeln!(
        out,
        "{:<10} {:<19} {:<6} Title",
        "ID", "Last Active", "Msgs"
    )
    .context("write list header")?;
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
        writeln!(out, "{id_prefix:<10} {last_active:<19} {msgs:<6} {title}")
            .context("write list row")?;
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
}
