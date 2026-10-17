//! CLI `--continue` argument resolution. Translates the `clap`-parsed `Option<Option<String>>`
//! into a fresh or resumed [`SessionHandle`] via [`resolve_session`].

use std::path::Path;

use anyhow::{Context, Result, bail};
use tracing::debug;

use super::display::id_prefix;
use super::entry::SessionInfo;
use super::handle::{self, LoadedTranscript, ResumedSession};
use super::store::SessionStore;

/// Normalized form of the CLI `--continue` argument.
enum ResumeMode<'a> {
    /// Omitting `--continue` starts a new session.
    Fresh,
    /// Bare `--continue` resumes the most recent unarchived session in scope.
    Latest,
    /// `--continue <prefix>` resumes the unique session whose ID starts with `prefix`.
    Prefix(&'a str),
    /// `--continue <path.jsonl>` resumes by filesystem path, bypassing the XDG lookup.
    Path(&'a Path),
}

/// Creates or resumes a session per CLI flags. `all` widens prefix / latest lookup from the
/// current project to every project subdir.
pub(crate) async fn resolve_session(
    store: &SessionStore,
    model: &str,
    resume: Option<&Option<String>>,
    all: bool,
) -> Result<ResumedSession> {
    let scope_hint = if all {
        ""
    } else {
        " in this project (use --all to search every project)"
    };

    let path = match normalize_resume_arg(resume)? {
        ResumeMode::Fresh => {
            return Ok(ResumedSession {
                handle: handle::start(store, model),
                transcript: LoadedTranscript::default(),
                title: None,
                unarchived: false,
                file_snapshots: Vec::new(),
            });
        }
        ResumeMode::Path(path) => path.to_owned(),
        ResumeMode::Latest => {
            store
                .latest(all)?
                .with_context(|| format!("no sessions to resume{scope_hint}"))?
                .path
        }
        ResumeMode::Prefix(prefix) => {
            match_in_scope(store, prefix, None, all)?
                .with_context(|| format!("no session matching prefix `{prefix}`{scope_hint}"))?
                .path
        }
    };

    let resumed = handle::resume_from_path(store, &path)?;
    debug!("resuming session from {}", path.display());
    Ok(resumed)
}

/// Trims and classifies a `--continue` argument into a [`ResumeMode`]. Empty / whitespace
/// prefixes are rejected explicitly to avoid silently collapsing into "resume latest", which bare
/// `--continue` already expresses.
fn normalize_resume_arg(resume: Option<&Option<String>>) -> Result<ResumeMode<'_>> {
    match resume {
        None => Ok(ResumeMode::Fresh),
        Some(None) => Ok(ResumeMode::Latest),
        Some(Some(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                bail!("empty session ID prefix (run bare `ox -c` to resume the latest session)");
            }
            if looks_like_path(trimmed) {
                Ok(ResumeMode::Path(Path::new(trimmed)))
            } else {
                Ok(ResumeMode::Prefix(trimmed))
            }
        }
    }
}

/// True if the argument contains a path separator or has a `.jsonl` extension. UUIDs contain
/// only hex + `-`, so this never mis-routes a valid session-ID prefix.
fn looks_like_path(arg: &str) -> bool {
    arg.contains('/')
        || arg.contains('\\')
        || Path::new(arg)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
}

/// Why a session-id prefix resolved to no single session. `Display` is the lowercase `anyhow`
/// headline for startup, which slash commands rephrase as a sentence.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ResolveError {
    #[error("no session matching `{prefix}`")]
    NoMatch { prefix: String },
    /// `preview` names the first matching ids in short form, then how many more matched.
    #[error("ambiguous prefix `{prefix}` matches {count} sessions: {preview}")]
    Ambiguous {
        prefix: String,
        count: usize,
        preview: String,
    },
    #[error("list sessions ({scope}): {cause:#}")]
    List {
        scope: &'static str,
        cause: anyhow::Error,
    },
}

/// Matches `prefix` against session ids other than `live_id`, first in the current project, then
/// in every project.
pub(crate) fn resolve_prefix_to_info(
    store: &SessionStore,
    prefix: &str,
    live_id: &str,
) -> Result<Option<SessionInfo>, ResolveError> {
    if let Some(info) = match_in_scope(store, prefix, Some(live_id), false)? {
        return Ok(Some(info));
    }
    match_in_scope(store, prefix, Some(live_id), true)
}

/// [`resolve_prefix_to_info`] for commands that act on a saved session. `Ok(None)` means only the
/// live session matched, so the caller can explain why it is excluded.
pub(crate) fn resolve_saved_prefix(
    store: &SessionStore,
    prefix: &str,
    live_id: &str,
) -> Result<Option<SessionInfo>, ResolveError> {
    if let Some(info) = resolve_prefix_to_info(store, prefix, live_id)? {
        return Ok(Some(info));
    }
    if live_id.starts_with(prefix) {
        return Ok(None);
    }
    Err(ResolveError::NoMatch {
        prefix: prefix.to_owned(),
    })
}

fn match_in_scope(
    store: &SessionStore,
    prefix: &str,
    live_id: Option<&str>,
    all: bool,
) -> Result<Option<SessionInfo>, ResolveError> {
    let scope = if all {
        "all projects"
    } else {
        "current project"
    };
    let mut matches = store
        .list_matching(prefix, all)
        .map_err(|cause| ResolveError::List { scope, cause })?;
    matches.retain(|info| live_id != Some(info.session_id.as_str()));
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        count => Err(ResolveError::Ambiguous {
            prefix: prefix.to_owned(),
            count,
            preview: format_session_id_preview(matches.into_iter().map(|s| s.session_id)),
        }),
    }
}

/// Renders up to `MATCH_PREVIEW_LIMIT` ids as [`id_prefix`] short forms, then how many remain.
fn format_session_id_preview(ids: impl IntoIterator<Item = String>) -> String {
    const MATCH_PREVIEW_LIMIT: usize = 5;
    let mut iter = ids.into_iter();
    let mut parts: Vec<String> = iter
        .by_ref()
        .take(MATCH_PREVIEW_LIMIT)
        .map(|id| id_prefix(&id).to_owned())
        .collect();
    let more = iter.count();
    if more > 0 {
        parts.push(format!("and {more} more"));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::session::store::set_archived_at;
    use crate::session::store::testing::{
        seed_test_session, test_project_dir, test_session_file, test_store,
        write_newest_corrupt_session,
    };

    // ── resolve_session ──

    #[tokio::test]
    async fn resolve_session_starts_fresh_when_no_continue_flag() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let resumed = resolve_session(&store, "m", None, false).await.unwrap();
        assert_eq!(resumed.transcript.messages, []);
        assert!(resumed.title.is_none());
        assert!(resumed.transcript.tool_metadata.is_empty());
        assert!(
            std::fs::read_dir(test_project_dir(dir.path()))
                .unwrap()
                .next()
                .is_none(),
            "fresh session file should be deferred until the first record_message",
        );
    }

    #[tokio::test]
    async fn resolve_session_resumes_from_external_path() {
        // Path-based resume must pick up title, messages, and session_id from the header
        // even when the file lives outside any store-searched directory.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = handle::start(&store, "m");
        let full_id = original.session_id().to_owned();
        original
            .record_message(Message::user("external path test"))
            .await;
        original.finish(Vec::new()).await;
        let path = test_session_file(dir.path(), &full_id);
        drop(original);

        // Imported-from-another-machine scenario.
        let external_dir = tempfile::tempdir().unwrap();
        let external_path = external_dir.path().join("copied.jsonl");
        std::fs::copy(&path, &external_path).unwrap();

        let arg = Some(external_path.to_string_lossy().into_owned());
        let resumed = resolve_session(&store, "m", Some(&arg), false)
            .await
            .unwrap();
        assert_eq!(resumed.handle.session_id(), full_id);
        assert_eq!(resumed.transcript.messages.len(), 1);
        assert_eq!(resumed.title.as_deref(), Some("external path test"));
    }

    #[tokio::test]
    async fn resolve_session_prefix_resumes_single_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = handle::start(&store, "m");
        let full_id = original.session_id().to_owned();
        original.record_message(Message::user("hello")).await;
        original.finish(Vec::new()).await;
        drop(original);

        // A 10-char UUID prefix is vanishingly unlikely to collide.
        let prefix = full_id[..10].to_owned();
        let arg = Some(prefix);
        let resumed = resolve_session(&store, "m", Some(&arg), false)
            .await
            .unwrap();
        assert_eq!(resumed.handle.session_id(), full_id);
        assert_eq!(resumed.transcript.messages.len(), 1);
        assert_eq!(resumed.title.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn resolve_session_all_widens_scope_to_every_project() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let other = SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let original = handle::start(&other, "m");
        let full_id = original.session_id().to_owned();
        original.record_message(Message::user("all scope")).await;
        original.finish(Vec::new()).await;
        drop(original);
        let arg = Some(full_id[..10].to_owned());

        let err = resolve_session(&store, "m", Some(&arg), false)
            .await
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            err,
            format!(
                "no session matching prefix `{}` in this project \
                 (use --all to search every project)",
                &full_id[..10],
            ),
        );

        let resumed = resolve_session(&store, "m", Some(&arg), true)
            .await
            .unwrap();
        assert_eq!(resumed.handle.session_id(), full_id);
        assert_eq!(resumed.transcript.messages.len(), 1);
        assert_eq!(resumed.title.as_deref(), Some("all scope"));

        let missing = Some("zzzz".to_owned());
        let err = resolve_session(&store, "m", Some(&missing), true)
            .await
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            err, "no session matching prefix `zzzz`",
            "the hint drops --all once it is set",
        );
    }

    #[tokio::test]
    async fn resolve_session_bare_continue_resumes_the_newest_readable_unarchived_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        seed_test_session(
            &store,
            "older",
            Some("Older"),
            Some(1),
            time::macros::datetime!(2026-04-18 09:00:00 UTC),
        );
        seed_test_session(
            &store,
            "newer",
            Some("Newer"),
            Some(1),
            time::macros::datetime!(2026-04-18 10:00:00 UTC),
        );
        seed_test_session(
            &store,
            "archived",
            Some("Archived"),
            Some(1),
            time::macros::datetime!(2026-04-18 11:00:00 UTC),
        );
        set_archived_at(&test_session_file(dir.path(), "archived"), true).unwrap();
        write_newest_corrupt_session(dir.path());

        let resumed = resolve_session(&store, "m", Some(&None), false)
            .await
            .unwrap();
        assert_eq!(resumed.handle.session_id(), "newer");
        assert_eq!(resumed.title.as_deref(), Some("Newer"));
        assert!(!resumed.unarchived);
    }

    #[tokio::test]
    async fn resolve_session_prefix_resumes_and_unarchives_an_archived_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        seed_test_session(
            &store,
            "archived",
            Some("Archived"),
            Some(1),
            time::macros::datetime!(2026-04-18 09:00:00 UTC),
        );
        let path = test_session_file(dir.path(), "archived");
        set_archived_at(&path, true).unwrap();

        let arg = Some("arch".to_owned());
        let resumed = resolve_session(&store, "m", Some(&arg), false)
            .await
            .unwrap();
        assert_eq!(resumed.handle.session_id(), "archived");
        assert!(resumed.unarchived);
        assert_eq!(
            store
                .latest(false)
                .unwrap()
                .map(|s| s.session_id)
                .as_deref(),
            Some("archived"),
            "bare --continue finds the session again",
        );
    }

    #[tokio::test]
    async fn resolve_session_bare_continue_errors_without_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let err = resolve_session(&store, "m", Some(&None), false)
            .await
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            err,
            "no sessions to resume in this project (use --all to search every project)",
        );
    }

    #[tokio::test]
    async fn resolve_session_prefix_errors_on_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let s = handle::start(&store, "m");
        s.record_message(Message::user("noop")).await;
        s.finish(Vec::new()).await;
        let prefix_arg = Some("zzzz".to_owned());
        drop(s);

        let err = resolve_session(&store, "m", Some(&prefix_arg), false)
            .await
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            err,
            "no session matching prefix `zzzz` in this project (use --all to search every project)",
        );
    }

    #[tokio::test]
    async fn resolve_session_prefix_reports_ambiguous_matches_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        for i in 0..7 {
            seed_test_session(
                &store,
                &format!("ab{i}-session"),
                None,
                Some(1),
                time::macros::datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i),
            );
        }

        let prefix_arg = Some("ab".to_owned());
        let err = resolve_session(&store, "m", Some(&prefix_arg), false)
            .await
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            err,
            "ambiguous prefix `ab` matches 7 sessions: \
             ab6-sess, ab5-sess, ab4-sess, ab3-sess, ab2-sess, and 2 more",
        );
    }

    #[tokio::test]
    async fn resolve_session_prefix_reports_a_listing_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        std::fs::remove_dir_all(test_project_dir(dir.path())).unwrap();
        let cause = store.list_matching("ab", false).unwrap_err();

        let prefix_arg = Some("ab".to_owned());
        let err = resolve_session(&store, "m", Some(&prefix_arg), false)
            .await
            .err()
            .unwrap()
            .to_string();
        assert_eq!(err, format!("list sessions (current project): {cause:#}"));
    }

    #[tokio::test]
    async fn resolve_session_path_errors_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let arg = Some("/does/not/exist.jsonl".to_owned());
        let err = resolve_session(&store, "m", Some(&arg), false)
            .await
            .err()
            .unwrap()
            .to_string();
        assert_eq!(err, "session not found: /does/not/exist.jsonl");
    }

    // ── normalize_resume_arg ──

    #[test]
    fn normalize_resume_arg_maps_none_to_fresh() {
        assert!(matches!(
            normalize_resume_arg(None).unwrap(),
            ResumeMode::Fresh
        ));
    }

    #[test]
    fn normalize_resume_arg_maps_bare_flag_to_latest() {
        assert!(matches!(
            normalize_resume_arg(Some(&None)).unwrap(),
            ResumeMode::Latest
        ));
    }

    #[test]
    fn normalize_resume_arg_trims_valid_prefix() {
        let arg = Some("  abc123 ".to_owned());
        let mode = normalize_resume_arg(Some(&arg)).unwrap();
        assert!(matches!(mode, ResumeMode::Prefix("abc123")));
    }

    #[test]
    fn normalize_resume_arg_classifies_path_arguments() {
        for raw in [
            "/abs/path.jsonl",
            "./relative.jsonl",
            "sub/dir/session.jsonl",
            "name.jsonl",           // no separator but has .jsonl suffix
            r"C:\Users\me\s.jsonl", // windows-shaped
        ] {
            let arg = Some(raw.to_owned());
            let mode = normalize_resume_arg(Some(&arg)).unwrap();
            assert!(
                matches!(mode, ResumeMode::Path(p) if p == Path::new(raw)),
                "{raw:?} should classify as Path",
            );
        }
    }

    #[test]
    fn normalize_resume_arg_keeps_uuid_shaped_prefix_as_prefix() {
        // A UUID prefix has no `/` or `.jsonl`, so it must not classify as a path.
        let arg = Some("a1b2c3d4-e5f6-7890".to_owned());
        let mode = normalize_resume_arg(Some(&arg)).unwrap();
        assert!(matches!(mode, ResumeMode::Prefix("a1b2c3d4-e5f6-7890")));
    }

    #[test]
    fn normalize_resume_arg_rejects_empty_and_whitespace_prefix() {
        for raw in ["", "   ", "\t\n"] {
            let arg = Some(raw.to_owned());
            let err = normalize_resume_arg(Some(&arg)).err().unwrap().to_string();
            assert_eq!(
                err, "empty session ID prefix (run bare `ox -c` to resume the latest session)",
                "{raw:?}",
            );
        }
    }

    // ── resolve_prefix_to_info ──

    #[test]
    fn resolve_prefix_to_info_widens_to_other_projects_when_current_misses() {
        // The shared resolver tries the current project first, then falls back to all projects.
        // Without the widening pass `/delete` and `/resume` would refuse a real cross-project id.
        let dir = tempfile::tempdir().unwrap();
        let other = SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let target = "abcd1234-eeff-1122-3344-556677889900";
        seed_test_session(
            &other,
            target,
            Some("Foreign"),
            Some(1),
            time::macros::datetime!(2026-04-18 09:00:00 UTC),
        );

        let current = test_store(dir.path());
        let info = resolve_prefix_to_info(&current, &target[..6], "live")
            .unwrap()
            .expect("widening pass must surface the foreign session");
        assert_eq!(info.session_id, target);
    }

    #[test]
    fn resolve_prefix_to_info_no_match_is_ok_absent() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        assert!(
            resolve_prefix_to_info(&store, "zzz", "live")
                .unwrap()
                .is_none(),
            "missing prefix is Ok(None) so callers can layer their own error",
        );
    }

    #[test]
    fn resolve_prefix_to_info_filters_live_id_so_caller_layers_its_own_message() {
        // The resolver excludes the live id silently, so `/delete` adds its dedicated "Cannot
        // delete the live session" error and `/resume` falls through to "No session matching".
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let live_id = "abcd1234-eeff-1122-3344-556677889900";
        seed_test_session(
            &store,
            live_id,
            Some("Live"),
            Some(1),
            time::macros::datetime!(2026-04-18 09:00:00 UTC),
        );

        assert!(
            resolve_prefix_to_info(&store, &live_id[..6], live_id)
                .unwrap()
                .is_none(),
            "live-id-only matches don't surface to the caller",
        );
    }

    #[test]
    fn resolve_prefix_to_info_ambiguous_lists_short_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        for (id, created_at) in [
            (
                "abcd1111-eeff-1122-3344-556677889900",
                time::macros::datetime!(2026-04-18 09:00:00 UTC),
            ),
            (
                "abcd2222-eeff-1122-3344-556677889900",
                time::macros::datetime!(2026-04-18 09:00:01 UTC),
            ),
        ] {
            seed_test_session(&store, id, Some("dup"), Some(1), created_at);
        }

        let err = resolve_prefix_to_info(&store, "abcd", "live").unwrap_err();

        assert!(
            matches!(
                &err,
                ResolveError::Ambiguous { prefix, count: 2, preview }
                    if prefix == "abcd" && preview == "abcd2222, abcd1111"
            ),
            "{err:?}",
        );
    }

    // ── resolve_saved_prefix ──

    #[test]
    fn resolve_saved_prefix_tells_a_live_only_match_from_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let live_id = "abcd1234-eeff-1122-3344-556677889900";
        let saved_id = "abcd5678-eeff-1122-3344-556677889900";
        seed_test_session(
            &store,
            saved_id,
            Some("Saved"),
            Some(1),
            time::macros::datetime!(2026-04-18 09:00:00 UTC),
        );

        let saved = resolve_saved_prefix(&store, "abcd", live_id).unwrap();
        assert_eq!(saved.map(|info| info.session_id).as_deref(), Some(saved_id));
        assert!(
            resolve_saved_prefix(&store, "abcd1", live_id)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            resolve_saved_prefix(&store, "zzz", live_id),
            Err(ResolveError::NoMatch { prefix }) if prefix == "zzz",
        ));
    }

    // ── format_session_id_preview ──

    #[test]
    fn format_session_id_preview_truncates_ids_to_eight_chars() {
        let ids = [
            "aaaaaaaaaaa".to_owned(),
            "bbbbbbbbbbb".to_owned(),
            "c".to_owned(),
        ];
        assert_eq!(format_session_id_preview(ids), "aaaaaaaa, bbbbbbbb, c");
    }

    #[test]
    fn format_session_id_preview_caps_at_five_and_counts_the_rest() {
        let ids = (0..6).map(|i| format!("id{i}"));
        assert_eq!(
            format_session_id_preview(ids),
            "id0, id1, id2, id3, id4, and 1 more",
        );
    }

    #[test]
    fn format_session_id_preview_lists_every_id_at_the_limit() {
        let ids = (0..5).map(|i| format!("id{i}"));
        assert_eq!(format_session_id_preview(ids), "id0, id1, id2, id3, id4");
    }
}
