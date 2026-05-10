//! `/delete <id-prefix>` — direct typed-arg form for session deletion. Bare `/delete` is rejected
//! because "what gets deleted?" is ambiguous; the picker form (`/resume` then Ctrl+D on a row) is
//! the discoverable path. Both forms share [`super::confirm::ConfirmDeleteSessionModal`].

use std::path::Path;

use super::confirm::ConfirmDeleteSessionModal;
use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::session::display::format_metadata_line;
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
        // Mutating so the typed-arg confirm modal and bare-form error block aren't dropped
        // mid-turn by the agent loop's user-action drop.
        SlashKind::Mutating
    }

    fn echoes_input(&self, _args: &str) -> bool {
        // Echo so the user sees what they tried to delete on both the typed-arg success path
        // and the bare-form error block.
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

/// Match `prefix` against current-project sessions first, widen to all projects on no match,
/// and surface a distinct error when the prefix matches only the live id.
fn resolve_prefix_to_info(
    store: &SessionStore,
    prefix: &str,
    live_id: &str,
) -> Result<SessionInfo, String> {
    if let Some(info) = match_in_scope(store, prefix, live_id, false)? {
        return Ok(info);
    }
    if let Some(info) = match_in_scope(store, prefix, live_id, true)? {
        return Ok(info);
    }
    if live_id.starts_with(prefix) {
        return Err(format!(
            "cannot delete the live session: `{prefix}` matches the active session",
        ));
    }
    Err(format!("no session matching `{prefix}`"))
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

/// Wraps [`format_metadata_line`] with the `/delete`-specific project rule: the confirm modal has
/// no scope context, so the project always shows when `cwd` is non-empty.
fn metadata_line(info: &SessionInfo) -> String {
    let project = tildify(Path::new(&info.cwd));
    format_metadata_line(
        &info.session_id,
        info.last_active_at,
        crate::util::time::local_offset(),
        info.exit.as_ref().map_or(0, |e| e.message_count),
        info.git_branch.as_deref(),
        (!project.is_empty()).then_some(project.as_str()),
    )
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
        // Both forms touch SessionStore. Typed-arg pushes a modal that performs the delete, bare
        // emits an error block. Mutating keeps both idle-gated.
        assert_eq!(DeleteCmd.classify(""), SlashKind::Mutating);
        assert_eq!(DeleteCmd.classify("ab"), SlashKind::Mutating);
    }

    #[test]
    fn echoes_input_is_true_for_both_bare_and_typed_arg() {
        // The bare form's error block and the typed-arg success path both benefit from the
        // user-typed line landing in chat for context.
        assert!(DeleteCmd.echoes_input(""));
        assert!(DeleteCmd.echoes_input("abcd"));
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
    fn execute_widens_to_other_projects_when_current_project_misses() {
        // Pin the widening path: scoped resolution misses, the all-projects retry finds the
        // target. Without widening the user would see "no session matching" for a real session.
        with_isolated_xdg(|dir| {
            let sessions_dir = dir.join("ox").join("sessions");
            std::fs::create_dir_all(&sessions_dir).unwrap();
            let other = SessionStore::open_at(sessions_dir, "other-project").unwrap();
            let target_id = stamped_id(0xab);
            seed_test_session(
                &other,
                &target_id,
                Some("Foreign"),
                Some(1),
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut chat = ChatView::new(&Theme::default(), false);
            let info = test_session_info();
            let mut ctx = SlashContext::new(&mut chat, &info);

            let outcome = DeleteCmd.execute(&target_id[..4], &mut ctx).unwrap();
            assert_eq!(outcome, SlashOutcome::Done);
            assert!(
                ctx.take_modal().is_some(),
                "widened resolution must still push the confirm modal",
            );
        });
    }

    #[test]
    fn execute_with_live_id_prefix_returns_distinct_live_session_error() {
        // The live session id is filtered out of match results, but a prefix that matches only
        // the live id surfaces a distinct error so the user sees the real reason rather than the
        // generic "no session matching" they'd get for a typo.
        with_isolated_xdg(|_| {
            let store = SessionStore::open().unwrap();
            let live_id = "test-session";
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
            assert!(
                err.contains("cannot delete the live session"),
                "live-only match must surface the dedicated message: {err}",
            );
            assert!(ctx.take_modal().is_none(), "no modal pushed for refusal");
        });
    }

    #[test]
    fn execute_with_live_id_prefix_returns_other_match_when_one_exists() {
        // Live + a non-live session both match the prefix. The non-live one wins, no error.
        with_isolated_xdg(|_| {
            let store = SessionStore::open().unwrap();
            let live_id = "test-session";
            seed_test_session(
                &store,
                live_id,
                Some("Live"),
                Some(1),
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let other_id = format!("test-other-{}", &stamped_id(0xab)[10..]);
            seed_test_session(
                &store,
                &other_id,
                Some("Other"),
                Some(1),
                datetime!(2026-04-18 09:01:00 UTC),
            );
            let mut chat = ChatView::new(&Theme::default(), false);
            let info = test_session_info();
            let mut ctx = SlashContext::new(&mut chat, &info);

            let outcome = DeleteCmd.execute("test-", &mut ctx).unwrap();
            assert_eq!(outcome, SlashOutcome::Done);
            assert!(ctx.take_modal().is_some(), "non-live match opens the modal");
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

    // ── display_title ──

    #[test]
    fn display_title_falls_back_to_untitled_marker_when_title_absent() {
        let info = SessionInfo {
            session_id: stamped_id(0xab),
            cwd: "/work".to_owned(),
            last_active_at: datetime!(2026-05-08 09:00:00 UTC),
            title: None,
            exit: None,
            git_branch: None,
        };
        assert_eq!(display_title(&info), "(untitled)");
    }

    // ── metadata_line ──

    #[test]
    fn metadata_line_includes_tildified_project_when_cwd_is_non_empty() {
        // The wrapper's only contribution over the shared formatter is the always-show-project
        // rule (the picker hides project in scoped mode, the confirm modal always shows it).
        let info = SessionInfo {
            session_id: stamped_id(0xab),
            cwd: "/work/oxide".to_owned(),
            last_active_at: datetime!(2026-05-08 09:00:00 UTC),
            title: None,
            exit: None,
            git_branch: None,
        };
        let line = metadata_line(&info);
        assert!(line.contains("/work/oxide"), "project shown: {line}");
    }

    #[test]
    fn metadata_line_omits_project_when_cwd_is_empty() {
        let info = SessionInfo {
            session_id: stamped_id(0xab),
            cwd: String::new(),
            last_active_at: datetime!(2026-05-08 09:00:00 UTC),
            title: None,
            exit: None,
            git_branch: None,
        };
        let line = metadata_line(&info);
        assert!(!line.ends_with(" · "), "no trailing separator: {line}");
        assert!(!line.contains(" ·  · "), "no double separator: {line}");
    }
}
