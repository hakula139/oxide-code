//! CLI `--continue` argument resolution.
//!
//! Translates the `clap`-parsed `Option<Option<String>>` into either a
//! freshly-started [`SessionManager`] or a resumed one with its
//! loaded messages, via [`resolve_session`]. Split out of `main.rs`
//! so the resolution logic (parsing, prefix matching, ambiguity
//! reporting) can be exercised by unit tests.

use anyhow::{Context, Result, bail};
use tracing::debug;

use super::manager::SessionManager;
use super::store::SessionStore;
use crate::message::Message;

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
}

/// Create or resume a session based on CLI flags.
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
) -> Result<(SessionManager, Vec<Message>)> {
    let mode = normalize_resume_arg(resume)?;
    if matches!(mode, ResumeMode::Fresh) {
        let session = SessionManager::start(store, model);
        return Ok((session, Vec::new()));
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
        ResumeMode::Fresh => unreachable!("handled above"),
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

    let (session, messages) = SessionManager::resume(store, &session_id).await?;
    debug!("resuming session {session_id}");
    Ok((session, messages))
}

/// Trim and classify a `--continue` argument into a [`ResumeMode`].
/// Empty / whitespace-only prefixes are rejected explicitly so they
/// cannot silently collapse into "resume latest" — the bare
/// `--continue` flag already expresses that intent.
pub(crate) fn normalize_resume_arg(resume: Option<&Option<String>>) -> Result<ResumeMode<'_>> {
    match resume {
        None => Ok(ResumeMode::Fresh),
        Some(None) => Ok(ResumeMode::Latest),
        Some(Some(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                bail!("empty session ID prefix; use `ox -c` (bare) to resume the latest session");
            }
            Ok(ResumeMode::Prefix(trimmed))
        }
    }
}

/// Join the first `MATCH_PREVIEW_LIMIT` session IDs (truncated to
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
        out.push_str(", ...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::store::{test_project_dir, test_store};
    use super::*;

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
    fn normalize_resume_arg_rejects_empty_and_whitespace_prefix() {
        for raw in ["", "   ", "\t\n"] {
            let arg = Some(raw.to_owned());
            let result = normalize_resume_arg(Some(&arg));
            let err = match result {
                Ok(_) => panic!("{raw:?} should have been rejected"),
                Err(e) => e.to_string(),
            };
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

    // ── resolve_session ──

    #[tokio::test]
    async fn resolve_session_starts_fresh_when_no_continue_flag() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let (_session, messages) = resolve_session(&store, "m", None, false).await.unwrap();
        assert!(messages.is_empty());
        assert!(
            std::fs::read_dir(test_project_dir(dir.path()))
                .unwrap()
                .next()
                .is_none(),
            "fresh session file should be deferred until the first record_message",
        );
    }

    #[tokio::test]
    async fn resolve_session_bare_continue_errors_without_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let err = match resolve_session(&store, "m", Some(&None), false).await {
            Ok(_) => panic!("expected failure — no sessions to resume"),
            Err(e) => e.to_string(),
        };
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
        let mut s = SessionManager::start(&store, "m");
        s.record_message(&Message::user("noop")).await.unwrap();
        let prefix_arg = Some("zzzz".to_owned());
        drop(s);

        let err = match resolve_session(&store, "m", Some(&prefix_arg), false).await {
            Ok(_) => panic!("expected prefix miss to bail"),
            Err(e) => e.to_string(),
        };
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
            let mut s = SessionManager::start(&store, "m");
            s.record_message(&Message::user("noop")).await.unwrap();
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
        let err = match resolve_session(&store, "m", Some(&prefix_arg), false).await {
            Ok(_) => panic!("expected ambiguous prefix to bail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("ambiguous prefix"), "got: {err}");
        assert!(err.contains(&prefix), "got: {err}");
    }

    #[tokio::test]
    async fn resolve_session_prefix_resumes_single_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let full_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("hello"))
            .await
            .unwrap();
        drop(original);

        // A 10-char UUID prefix is vanishingly unlikely to collide.
        let prefix = full_id[..10].to_owned();
        let arg = Some(prefix);
        let (resumed, messages) = resolve_session(&store, "m", Some(&arg), false)
            .await
            .unwrap();
        assert_eq!(resumed.session_id(), full_id);
        assert_eq!(messages.len(), 1);
    }
}
