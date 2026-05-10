//! `/delete <id-prefix>` — direct typed-arg form for session deletion. Bare `/delete` is rejected
//! because "what gets deleted?" is ambiguous; the picker form (`/resume` then Ctrl+D on a row) is
//! the discoverable path. Both forms share [`super::confirm::ConfirmDeleteSessionModal`].

use std::path::Path;

use super::confirm::ConfirmDeleteSessionModal;
use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::session::entry::SessionInfo;
use crate::session::store::SessionStore;
use crate::util::path::tildify;

/// `/delete <id-prefix>` typed-arg form.
pub(super) struct DeleteCmd;

impl SlashCommand for DeleteCmd {
    fn name(&self) -> &'static str {
        "delete"
    }

    fn description(&self) -> &'static str {
        "Delete a saved session"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        // Mutating in both forms: bare emits an error block; typed-arg opens a confirm modal that
        // performs the unlink. The agent loop drops mid-turn user actions, so the gating must
        // wait for idle even though deletion never touches live-session state.
        SlashKind::Mutating
    }

    fn echoes_input(&self, _args: &str) -> bool {
        // Typed `/delete <id>` is interesting in chat — the user might want to see what they
        // tried to delete. The bare form's error block has its own context, but the line is
        // also useful there.
        true
    }

    fn usage(&self) -> Option<&'static str> {
        Some("<id-prefix>")
    }

    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            return Err(
                "missing id. Use `/resume` then Ctrl+D, or `/delete <id-prefix>` for direct removal."
                    .to_owned(),
            );
        }
        let store =
            SessionStore::open().map_err(|e| format!("session store unavailable: {e:#}"))?;
        let live_id = ctx.info.session_id.as_str();
        let info = resolve_prefix_to_info(&store, arg, live_id)?;
        ctx.open_modal(Box::new(ConfirmDeleteSessionModal::new(
            store,
            info.session_id.clone(),
            display_title(&info),
            metadata_line(&info),
            live_id.to_owned(),
        )));
        Ok(SlashOutcome::Done)
    }
}

// ── Resolution ──

/// Match `prefix` against current-project sessions first; widen to all projects on no match.
/// Excludes the live session id (deleting the open writer's file is refused at the store layer
/// anyway, but filtering here gives a clearer error message).
fn resolve_prefix_to_info(
    store: &SessionStore,
    prefix: &str,
    live_id: &str,
) -> Result<SessionInfo, String> {
    if let Some(info) = match_in_scope(store, prefix, live_id, false)? {
        return Ok(info);
    }
    match_in_scope(store, prefix, live_id, true)?
        .ok_or_else(|| format!("no session matching `{prefix}`"))
}

fn match_in_scope(
    store: &SessionStore,
    prefix: &str,
    live_id: &str,
    all: bool,
) -> Result<Option<SessionInfo>, String> {
    let page = store
        .list_paged(None, all)
        .map_err(|e| format!("list sessions: {e:#}"))?;
    let mut matches: Vec<SessionInfo> = page
        .into_sessions()
        .into_iter()
        .filter(|info| info.session_id != live_id && info.session_id.starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        n => {
            let ids: Vec<String> = matches.into_iter().map(|s| s.session_id).collect();
            let preview = crate::session::resolver::format_session_id_preview(ids);
            Err(format!(
                "ambiguous prefix `{prefix}` matches {n} sessions: {preview}",
            ))
        }
    }
}

// ── Display Formatting ──

fn display_title(info: &SessionInfo) -> String {
    info.title
        .as_ref()
        .map_or_else(|| "(untitled)".to_owned(), |t| t.title.clone())
}

/// Single-line summary mirroring [`super::resume`]'s picker row metadata: `{id_prefix} ·
/// {when} · {N msgs} · {branch} · {project}`.
fn metadata_line(info: &SessionInfo) -> String {
    use std::fmt::Write as _;
    const SEP: &str = " · ";
    const ID_WIDTH: usize = 8;

    let local_offset = crate::util::time::local_offset();
    let now = time::OffsetDateTime::now_utc().to_offset(local_offset);
    let when = format_relative(info.last_active_at.to_offset(local_offset), now);
    let prefix = info
        .session_id
        .get(..ID_WIDTH)
        .unwrap_or(info.session_id.as_str());
    let mut meta = format!("{prefix}{SEP}{when}");
    let count = info.exit.as_ref().map_or(0, |e| e.message_count);
    if count > 0 {
        let unit = if count == 1 { "msg" } else { "msgs" };
        _ = write!(meta, "{SEP}{count} {unit}");
    }
    if let Some(branch) = info.git_branch.as_deref() {
        meta.push_str(SEP);
        meta.push_str(branch);
    }
    let project = tildify(Path::new(&info.cwd));
    if !project.is_empty() {
        meta.push_str(SEP);
        meta.push_str(&project);
    }
    meta
}

/// Compact relative-time matching the picker's footer ("3 minutes ago"); falls back to ISO date
/// past 30 days. Negative deltas (clock skew) collapse to 0.
fn format_relative(ts: time::OffsetDateTime, now: time::OffsetDateTime) -> String {
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
            .expect("static format never fails");
    };
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural} ago")
}

#[cfg(test)]
mod tests {
    use temp_env::with_var;
    use time::macros::datetime;

    use super::*;
    use crate::session::store::seed_test_session;
    use crate::slash::registry::SlashCommand;
    use crate::slash::test_session_info;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn stamped_id(byte: u8) -> String {
        let s = format!("{byte:02x}");
        format!(
            "{s}{s}1111-2222-3333-4444-{s}{s}{s}{s}{s}{s}",
            s = s.repeat(2),
        )
    }

    fn with_isolated_xdg<R>(f: impl FnOnce(&Path) -> R) -> R {
        let dir = tempfile::tempdir().unwrap();
        with_var("XDG_DATA_HOME", Some(dir.path().as_os_str()), || {
            f(dir.path())
        })
    }

    // ── DeleteCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(DeleteCmd.name(), "delete");
        assert!(DeleteCmd.aliases().is_empty());
        assert!(!DeleteCmd.description().is_empty());
        assert_eq!(DeleteCmd.usage(), Some("<id-prefix>"));
    }

    #[test]
    fn classify_is_always_mutating() {
        // Both forms touch SessionStore (typed-arg pushes a modal that performs the delete; bare
        // emits an error block). Bare's error is informational but still a state-aware response,
        // so Mutating keeps it idle-gated alongside the active form.
        assert_eq!(DeleteCmd.classify(""), SlashKind::Mutating);
        assert_eq!(DeleteCmd.classify("ab"), SlashKind::Mutating);
    }

    // ── DeleteCmd::execute ──

    #[test]
    fn execute_bare_returns_friendly_error_about_missing_id() {
        with_isolated_xdg(|_| {
            let mut chat = ChatView::new(&Theme::default(), false);
            let info = test_session_info();
            let mut ctx = SlashContext::new(&mut chat, &info);
            let err = DeleteCmd.execute("", &mut ctx).unwrap_err();
            assert!(err.contains("missing id"), "{err}");
            assert!(ctx.take_modal().is_none(), "no modal pushed on bare form");
        });
    }

    #[test]
    fn execute_with_unknown_prefix_returns_friendly_error() {
        with_isolated_xdg(|_| {
            let mut chat = ChatView::new(&Theme::default(), false);
            let info = test_session_info();
            let mut ctx = SlashContext::new(&mut chat, &info);
            let err = DeleteCmd.execute("zzz1234", &mut ctx).unwrap_err();
            assert!(err.contains("no session matching"), "{err}");
            assert!(ctx.take_modal().is_none());
        });
    }

    #[test]
    fn execute_typed_arg_unique_match_pushes_confirm_modal() {
        with_isolated_xdg(|_| {
            let store = SessionStore::open().unwrap();
            let target_id = stamped_id(0xab);
            seed_test_session(
                &store,
                &target_id,
                Some("Doomed session"),
                Some(3),
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut chat = ChatView::new(&Theme::default(), false);
            let info = test_session_info();
            let mut ctx = SlashContext::new(&mut chat, &info);

            let outcome = DeleteCmd.execute(&target_id[..4], &mut ctx).unwrap();
            assert_eq!(outcome, SlashOutcome::Done);
            assert!(
                ctx.take_modal().is_some(),
                "typed-arg must push a confirm modal",
            );
        });
    }

    #[test]
    fn execute_with_live_id_prefix_excludes_self_and_returns_no_match() {
        // The live session id is filtered out of the resolve path so a typed `/delete <live-id>`
        // surfaces as "no session matching" rather than a confirm modal that would then refuse
        // at the store layer.
        with_isolated_xdg(|_| {
            let store = SessionStore::open().unwrap();
            let live_id = "test-session"; // matches `test_session_info().session_id`.
            seed_test_session(
                &store,
                live_id,
                Some("Live"),
                Some(1),
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut chat = ChatView::new(&Theme::default(), false);
            let info = test_session_info();
            let mut ctx = SlashContext::new(&mut chat, &info);

            let err = DeleteCmd.execute(live_id, &mut ctx).unwrap_err();
            assert!(err.contains("no session matching"), "{err}");
        });
    }

    #[test]
    fn execute_ambiguous_prefix_lists_short_ids_in_error() {
        with_isolated_xdg(|_| {
            let store = SessionStore::open().unwrap();
            let id_a = format!("aaaa{}", &stamped_id(0xaa)[4..]);
            let id_b = format!("aaaa{}", &stamped_id(0xab)[4..]);
            for id in [&id_a, &id_b] {
                seed_test_session(
                    &store,
                    id,
                    Some("t"),
                    Some(1),
                    datetime!(2026-04-18 09:00:00 UTC),
                );
            }
            let mut chat = ChatView::new(&Theme::default(), false);
            let info = test_session_info();
            let mut ctx = SlashContext::new(&mut chat, &info);

            let err = DeleteCmd.execute("aaaa", &mut ctx).unwrap_err();
            assert!(err.contains("ambiguous prefix"), "{err}");
            assert!(err.contains(&id_a[..8]), "first id in {err}");
            assert!(err.contains(&id_b[..8]), "second id in {err}");
        });
    }

    // ── format_relative ──

    #[test]
    fn format_relative_pluralizes_units_and_falls_back_to_iso_date_past_30_days() {
        let now = datetime!(2026-05-08 12:00:00 UTC);
        assert_eq!(
            format_relative(now - time::Duration::seconds(1), now),
            "1 second ago"
        );
        assert_eq!(
            format_relative(now - time::Duration::seconds(45), now),
            "45 seconds ago"
        );
        assert_eq!(
            format_relative(now - time::Duration::minutes(1), now),
            "1 minute ago"
        );
        assert_eq!(
            format_relative(now - time::Duration::hours(2), now),
            "2 hours ago"
        );
        assert_eq!(
            format_relative(now - time::Duration::days(5), now),
            "5 days ago"
        );
        assert_eq!(
            format_relative(now - time::Duration::days(60), now),
            "2026-03-09"
        );
        assert_eq!(
            format_relative(now + time::Duration::seconds(30), now),
            "0 seconds ago",
            "future stamps collapse to 0 rather than negative",
        );
    }

    // ── metadata_line ──

    #[test]
    fn metadata_line_includes_msg_count_and_branch_when_present() {
        let info = SessionInfo {
            session_id: stamped_id(0xab),
            cwd: "/work/oxide".to_owned(),
            last_active_at: datetime!(2026-05-08 09:00:00 UTC),
            title: None,
            exit: Some(crate::session::entry::ExitInfo {
                message_count: 14,
                updated_at: datetime!(2026-05-08 09:00:00 UTC),
            }),
            git_branch: Some("feat/login".to_owned()),
        };
        let line = metadata_line(&info);
        assert!(line.contains(&info.session_id[..8]), "id prefix: {line}");
        assert!(line.contains("14 msgs"), "plural msg count: {line}");
        assert!(line.contains("feat/login"), "branch: {line}");
    }

    #[test]
    fn metadata_line_omits_msg_count_when_session_never_finalized() {
        let info = SessionInfo {
            session_id: stamped_id(0xcd),
            cwd: "/work".to_owned(),
            last_active_at: datetime!(2026-05-08 09:00:00 UTC),
            title: None,
            exit: None,
            git_branch: None,
        };
        let line = metadata_line(&info);
        assert!(!line.contains("msgs"), "no plural: {line}");
        assert!(!line.contains("msg"), "no singular: {line}");
    }
}
