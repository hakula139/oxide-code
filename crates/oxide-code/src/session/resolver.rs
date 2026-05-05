//! CLI `--continue` argument resolution. Translates the `clap`-parsed `Option<Option<String>>`
//! into a fresh or resumed [`SessionHandle`] via [`resolve_session`].

use std::path::Path;

use anyhow::{Context, Result, bail};
use tracing::debug;

use super::handle::{self, ResumedSession};
use super::store::SessionStore;
use crate::util::text::ELLIPSIS;

/// Normalized form of the CLI `--continue` argument.
pub(crate) enum ResumeMode<'a> {
    /// No `--continue` flag — start a new session.
    Fresh,
    /// Bare `--continue` — resume the most recent session in scope.
    Latest,
    /// `--continue <prefix>` — resume the unique session whose ID starts with `prefix`.
    Prefix(&'a str),
    /// `--continue <path.jsonl>` — resume by filesystem path, bypassing the XDG lookup.
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
    let mode = normalize_resume_arg(resume)?;

    // Path resumes bypass project-subdir listing entirely.
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

/// Trims and classifies a `--continue` argument into a [`ResumeMode`]. Empty / whitespace
/// prefixes are rejected explicitly to avoid silently collapsing into "resume latest" — bare
/// `--continue` already expresses that.
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

/// True if the argument contains a path separator or has a `.jsonl` extension. UUIDs contain
/// only hex + `-`, so this never mis-routes a valid session-ID prefix.
fn looks_like_path(arg: &str) -> bool {
    arg.contains('/')
        || arg.contains('\\')
        || Path::new(arg)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
}

/// Renders up to `MATCH_PREVIEW_LIMIT` IDs (truncated to 8 chars), with `, ...` for overflow.
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
        // `--all` flips listing to `list_all`; the error hint also drops "in this project".
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
        // Need a real file on disk so the listing is non-empty; otherwise the prefix lookup
        // short-circuits on "no sessions" before exercising the no-match path.
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
        // 20 v4 UUIDs across 16 hex chars guarantees ≥ 2 collisions by pigeonhole.
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
        // UUID prefix has no `/` and no `.jsonl`; must not classify as path.
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
