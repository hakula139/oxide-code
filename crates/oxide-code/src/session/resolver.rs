//! CLI `--continue` argument resolution.
//!
//! Translates the `clap`-parsed `Option<Option<String>>` into either a
//! freshly-started [`SessionHandle`] or a resumed one with its loaded
//! messages, via [`resolve_session`]. Split out of `main.rs` so the
//! resolution logic (parsing, prefix matching, ambiguity reporting)
//! can be exercised by unit tests.

use std::path::Path;

use anyhow::{Context, Result, bail};
use tracing::debug;

use super::handle::{self, ResumedSession};
use super::store::SessionStore;
use crate::util::text::ELLIPSIS;

/// Normalized form of the CLI `--continue` argument. Built by
/// [`normalize_resume_arg`] and consumed by [`resolve_session`].
pub(crate) enum ResumeMode<'a> {
    /// No `--continue` was passed — start a brand new session.
    Fresh,
    /// Bare `--continue` — resume the most recent session in scope.
    Latest,
    /// `--continue <prefix>` — resume the single session whose ID
    /// starts with the (trimmed, non-empty) prefix.
    Prefix(&'a str),
    /// `--continue <path.jsonl>` — resume a session file by explicit
    /// filesystem path, bypassing the XDG project subdirectory lookup.
    /// Selected when the argument contains a path separator or ends with
    /// `.jsonl`; any UUID-shaped token is still classified as
    /// [`Prefix`][Self::Prefix].
    Path(&'a Path),
}

/// Creates or resumes a session based on CLI flags.
///
/// `resume`:
/// - `None`: no `--continue` flag → new session.
/// - `Some(None)`: `--continue` without value → resume latest.
/// - `Some(Some(id))`: `--continue <id>` → resume specific session.
///
/// `all` widens the search scope for `--continue` from the current
/// project to every project. A specific session ID is always resolved
/// across projects once matched, so `--all` mainly changes which
/// sessions are eligible in the prefix / latest lookup.
pub(crate) async fn resolve_session(
    store: &SessionStore,
    model: &str,
    resume: Option<&Option<String>>,
    all: bool,
) -> Result<ResumedSession> {
    let mode = normalize_resume_arg(resume)?;

    // Path resumes bypass the store's project-subdir lookup entirely and
    // can be resolved without listing anything.
    if let ResumeMode::Path(path) = mode {
        let resumed = handle::resume_from_path(store, path)?;
        debug!("resuming session from {}", path.display());
        return Ok(resumed);
    }

    if matches!(mode, ResumeMode::Fresh) {
        return Ok(ResumedSession {
            handle: handle::start(store, model),
            messages: Vec::new(),
            title: None,
            tool_result_metadata: std::collections::HashMap::new(),
            file_snapshots: Vec::new(),
        });
    }

    let sessions = if all {
        store.list_all()?
    } else {
        store.list()?
    };

    let scope_hint = if all {
        ""
    } else {
        " in this project (use --all to search every project)"
    };

    let session_id = match mode {
        ResumeMode::Fresh | ResumeMode::Path(_) => unreachable!("handled above"),
        ResumeMode::Latest => sessions
            .into_iter()
            .next()
            .map(|s| s.session_id)
            .with_context(|| format!("no sessions to resume{scope_hint}"))?,
        ResumeMode::Prefix(prefix) => {
            let mut matched = sessions
                .into_iter()
                .filter(|s| s.session_id.starts_with(prefix))
                .map(|s| s.session_id);
            let first = matched.next();
            let second = matched.next();
            match (first, second) {
                (None, _) => bail!("no session matching prefix '{prefix}'{scope_hint}"),
                (Some(only), None) => only,
                (Some(a), Some(b)) => {
                    let rest: Vec<_> = matched.collect();
                    bail!(
                        "ambiguous prefix '{prefix}' matches {} sessions: {}",
                        2 + rest.len(),
                        format_session_id_preview([a, b].into_iter().chain(rest)),
                    );
                }
            }
        }
    };

    let resumed = handle::resume(store, &session_id)?;
    debug!("resuming session {session_id}");
    Ok(resumed)
}

/// Trims and classifies a `--continue` argument into a [`ResumeMode`].
/// Empty / whitespace-only prefixes are rejected explicitly so they
/// cannot silently collapse into "resume latest" — the bare
/// `--continue` flag already expresses that intent.
///
/// An argument that looks like a path (contains a path separator or ends
/// with `.jsonl`) is classified as [`ResumeMode::Path`]; otherwise it is
/// a UUID-shaped prefix. UUIDs contain only hex + `-`, neither of which
/// trigger the path heuristic, so prefix resume stays unchanged.
pub(crate) fn normalize_resume_arg(resume: Option<&Option<String>>) -> Result<ResumeMode<'_>> {
    match resume {
        None => Ok(ResumeMode::Fresh),
        Some(None) => Ok(ResumeMode::Latest),
        Some(Some(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                bail!("empty session ID prefix; use `ox -c` (bare) to resume the latest session");
            }
            if looks_like_path(trimmed) {
                Ok(ResumeMode::Path(Path::new(trimmed)))
            } else {
                Ok(ResumeMode::Prefix(trimmed))
            }
        }
    }
}

/// Classify a `--continue` argument as a path when it either contains a
/// path separator or uses the `.jsonl` extension (case-insensitive, in
/// case a user hands us `.JSONL`). UUID v4 strings contain only hex
/// digits and `-`, so this classifier never mis-routes a valid
/// session-ID prefix.
fn looks_like_path(arg: &str) -> bool {
    arg.contains('/')
        || arg.contains('\\')
        || Path::new(arg)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
}

/// Joins the first `MATCH_PREVIEW_LIMIT` session IDs (truncated to
/// 8 chars each) as `aaaaaaaa, bbbbbbbb`, appending `, ...` when more
/// matches were provided. Drives the ambiguous-prefix error in
/// [`resolve_session`].
fn format_session_id_preview(ids: impl IntoIterator<Item = String>) -> String {
    const MATCH_PREVIEW_LIMIT: usize = 5;
    let mut iter = ids.into_iter();
    let first_batch: Vec<String> = iter
        .by_ref()
        .take(MATCH_PREVIEW_LIMIT)
        .map(|id| id[..id.len().min(8)].to_owned())
        .collect();
    let mut out = first_batch.join(", ");
    if iter.next().is_some() {
        out.push_str(", ");
        out.push_str(ELLIPSIS);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::store::{test_project_dir, test_session_file, test_store};
    use super::*;
    use crate::message::Message;

    // ── resolve_session ──

    #[tokio::test]
    async fn resolve_session_starts_fresh_when_no_continue_flag() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let resumed = resolve_session(&store, "m", None, false).await.unwrap();
        assert!(resumed.messages.is_empty());
        assert!(resumed.title.is_none());
        assert!(resumed.tool_result_metadata.is_empty());
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
        // A session file living somewhere the store wouldn't search.
        // The path-based resume must still pick up the title, messages,
        // and session_id recorded in the header.
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

        // Copy the file outside the project directory to simulate the
        // "imported from another machine" scenario.
        let external_dir = tempfile::tempdir().unwrap();
        let external_path = external_dir.path().join("copied.jsonl");
        std::fs::copy(&path, &external_path).unwrap();

        let arg = Some(external_path.to_string_lossy().into_owned());
        let resumed = resolve_session(&store, "m", Some(&arg), false)
            .await
            .unwrap();
        assert_eq!(resumed.handle.session_id(), full_id);
        assert_eq!(resumed.messages.len(), 1);
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
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(resumed.title.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn resolve_session_all_widens_scope_to_list_all() {
        // `--all` flips the listing from `store.list()` (current project only)
        // to `store.list_all()` (every project subdir). The prefix match
        // then resolves across projects and the error hint drops the
        // "in this project" qualifier. Exercising the `all = true` branch
        // here also covers the empty `scope_hint`.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = handle::start(&store, "m");
        let full_id = original.session_id().to_owned();
        original.record_message(Message::user("all scope")).await;
        original.finish(Vec::new()).await;
        drop(original);

        let arg = Some(full_id[..10].to_owned());
        let resumed = resolve_session(&store, "m", Some(&arg), true)
            .await
            .unwrap();
        assert_eq!(resumed.handle.session_id(), full_id);
        assert_eq!(resumed.messages.len(), 1);
        assert_eq!(resumed.title.as_deref(), Some("all scope"));

        // Error path under `all = true` omits the "use --all" hint.
        let missing = Some("zzzz".to_owned());
        let err = resolve_session(&store, "m", Some(&missing), true)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            !err.contains("use --all"),
            "hint should not suggest --all when already set: {err}",
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
        assert!(err.contains("no sessions"), "got: {err}");
    }

    #[tokio::test]
    async fn resolve_session_prefix_errors_on_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // Materialize a session file so the project listing is
        // non-empty; without `record_message` the lazy creation
        // never touches disk and the prefix lookup would short-circuit
        // on "no sessions" instead of testing the no-match path.
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
        assert!(err.contains("no session matching prefix"), "got: {err}");
    }

    #[tokio::test]
    async fn resolve_session_prefix_reports_ambiguous_matches() {
        // Spin up 20 sessions and record a message in each so any
        // of them is resumable. Then pick the hex char that the
        // most session IDs start with — with 20 v4 UUIDs across 16
        // characters, this is guaranteed ≥ 2 by pigeonhole, so the
        // prefix lookup must bail with "ambiguous".
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());

        for _ in 0..20 {
            let s = handle::start(&store, "m");
            s.record_message(Message::user("noop")).await;
            s.finish(Vec::new()).await;
        }

        let listed = store.list().unwrap();
        let mut counts = std::collections::HashMap::<char, usize>::new();
        for info in &listed {
            *counts
                .entry(info.session_id.chars().next().unwrap())
                .or_default() += 1;
        }
        let (prefix_char, count) = counts.into_iter().max_by_key(|&(_, n)| n).unwrap();
        assert!(count >= 2, "pigeonhole violation: 20 UUIDs over 16 chars");
        let prefix = prefix_char.to_string();

        let prefix_arg = Some(prefix.clone());
        let err = resolve_session(&store, "m", Some(&prefix_arg), false)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("ambiguous prefix"), "got: {err}");
        assert!(err.contains(&prefix), "got: {err}");
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
        assert!(err.contains("session not found"), "got: {err}");
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
        // A v4 UUID prefix uses only hex + `-`; neither triggers the path
        // heuristic, so bare prefixes still resume through the store.
        let arg = Some("a1b2c3d4-e5f6-7890".to_owned());
        let mode = normalize_resume_arg(Some(&arg)).unwrap();
        assert!(matches!(mode, ResumeMode::Prefix("a1b2c3d4-e5f6-7890")));
    }

    #[test]
    fn normalize_resume_arg_rejects_empty_and_whitespace_prefix() {
        for raw in ["", "   ", "\t\n"] {
            let arg = Some(raw.to_owned());
            let err = normalize_resume_arg(Some(&arg)).err().unwrap().to_string();
            assert!(err.contains("empty session ID prefix"), "{raw:?} → {err:?}");
            assert!(err.contains("bare"), "{raw:?} → {err:?}");
        }
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
    fn format_session_id_preview_caps_at_five_and_appends_ellipsis() {
        let ids: Vec<String> = (0..7).map(|i| format!("abcdefgh{i}")).collect();
        let out = format_session_id_preview(ids);
        let short_count = out.split(", ").filter(|s| *s != "...").count();
        assert_eq!(short_count, 5, "{out:?}");
        assert!(out.ends_with(", ..."), "{out:?}");
    }

    #[test]
    fn format_session_id_preview_no_ellipsis_at_limit() {
        let ids: Vec<String> = (0..5).map(|i| format!("id{i}")).collect();
        let out = format_session_id_preview(ids);
        assert!(!out.ends_with(", ..."), "{out:?}");
    }
}
