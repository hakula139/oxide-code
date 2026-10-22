//! Resolution of the CLI `--continue` and `--resume` targets and of session-ID prefixes.

use std::path::Path;

use anyhow::{Context, Result, bail};
use tracing::debug;

use super::display::id_prefix;
use super::entry::{ArchiveState, SessionInfo};
use super::handle::{self, LoadedTranscript, ResumedSession};
use super::store::SessionStore;
use super::store::listing::ProjectScope;

/// The saved session a run continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTarget<'a> {
    /// The most recent unarchived session in scope, as `--continue` picks.
    Latest,
    /// The session an ID prefix or a session file path names, as `--resume <session>` gives.
    Named(&'a str),
}

/// Normalized form of the CLI's session target.
enum ResumeMode<'a> {
    /// No target starts a new session.
    Fresh,
    /// The most recent unarchived session in scope.
    Latest,
    /// The unique session whose ID starts with the prefix.
    Prefix(&'a str),
    /// The session file at the path, bypassing the XDG lookup.
    Path(&'a Path),
}

/// Starts a new session, or resumes the one `target` names.
/// A fresh session records `git_branch`.
/// A prefix matches in the current project first and then in every project,
/// as `/resume <prefix>` does, while [`ProjectScope::Every`] searches every project at once
/// for both a prefix and the latest session.
pub fn resolve_session(
    store: &SessionStore,
    model: &str,
    git_branch: Option<String>,
    target: Option<SessionTarget<'_>>,
    projects: ProjectScope,
) -> Result<ResumedSession> {
    let path = match normalize_target(target)? {
        ResumeMode::Fresh => {
            return Ok(ResumedSession {
                handle: handle::start(store, model, git_branch),
                transcript: LoadedTranscript::default(),
                title: None,
                prior: ArchiveState::Active,
            });
        }
        ResumeMode::Path(path) => path.to_owned(),
        ResumeMode::Latest => {
            let scope_hint = match projects {
                ProjectScope::Current => " in this project (use --all to search every project)",
                ProjectScope::Every => "",
            };
            store
                .latest(projects)?
                .with_context(|| format!("no sessions to resume{scope_hint}"))?
                .path
        }
        ResumeMode::Prefix(prefix) => {
            let info = match projects {
                ProjectScope::Current => resolve_prefix_to_info(store, prefix, None)?,
                ProjectScope::Every => match_in_scope(store, prefix, None, ProjectScope::Every)?,
            };
            info.ok_or_else(|| ResolveError::NoMatch {
                prefix: prefix.to_owned(),
            })?
            .path
        }
    };

    let resumed = handle::resume_from_path(store, &path)?;
    debug!("resuming session from {}", path.display());
    Ok(resumed)
}

/// Trims and classifies `target` into a [`ResumeMode`], rejecting a blank name, which names no
/// session.
fn normalize_target(target: Option<SessionTarget<'_>>) -> Result<ResumeMode<'_>> {
    match target {
        None => Ok(ResumeMode::Fresh),
        Some(SessionTarget::Latest) => Ok(ResumeMode::Latest),
        Some(SessionTarget::Named(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                bail!(
                    "empty session ID prefix (run `ox -c` to continue the latest session or `ox -r` \
                     to pick one)"
                );
            }
            if looks_like_path(trimmed) {
                Ok(ResumeMode::Path(Path::new(trimmed)))
            } else {
                Ok(ResumeMode::Prefix(trimmed))
            }
        }
    }
}

/// Returns whether `arg` contains a path separator or has a `.jsonl` extension, which no session-ID
/// prefix does.
fn looks_like_path(arg: &str) -> bool {
    arg.contains('/')
        || arg.contains('\\')
        || Path::new(arg)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
}

/// Why a session-ID prefix resolved to no single session. `Display` is the lowercase `anyhow`
/// headline for startup, which slash commands rephrase as a sentence.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no session matching `{prefix}`")]
    NoMatch { prefix: String },
    /// Only the live session matched, which commands on saved sessions exclude.
    #[error("session prefix `{prefix}` matches the live session")]
    Live { prefix: String },
    /// `preview` names the first matching IDs in short form, then how many more matched.
    #[error("ambiguous prefix `{prefix}` matches {count} sessions: {preview}")]
    Ambiguous {
        prefix: String,
        count: usize,
        preview: String,
    },
    /// `scope` names where the listing looked: `this project` or `all projects`.
    #[error("failed to list sessions in {scope}: {cause:#}")]
    List {
        scope: &'static str,
        cause: anyhow::Error,
    },
}

/// Matches `prefix` against session IDs other than `live_id`, first in the current project, then
/// in every project.
fn resolve_prefix_to_info(
    store: &SessionStore,
    prefix: &str,
    live_id: Option<&str>,
) -> Result<Option<SessionInfo>, ResolveError> {
    if let Some(info) = match_in_scope(store, prefix, live_id, ProjectScope::Current)? {
        return Ok(Some(info));
    }
    match_in_scope(store, prefix, live_id, ProjectScope::Every)
}

/// [`resolve_prefix_to_info`] for commands that act on a saved session, telling a prefix that
/// matches only the live session from one that matches nothing.
pub fn resolve_saved_prefix(
    store: &SessionStore,
    prefix: &str,
    live_id: &str,
) -> Result<SessionInfo, ResolveError> {
    if let Some(info) = resolve_prefix_to_info(store, prefix, Some(live_id))? {
        return Ok(info);
    }
    let prefix = prefix.to_owned();
    Err(if live_id.starts_with(&prefix) {
        ResolveError::Live { prefix }
    } else {
        ResolveError::NoMatch { prefix }
    })
}

fn match_in_scope(
    store: &SessionStore,
    prefix: &str,
    live_id: Option<&str>,
    projects: ProjectScope,
) -> Result<Option<SessionInfo>, ResolveError> {
    let mut matches =
        store
            .list_matching(prefix, projects)
            .map_err(|cause| ResolveError::List {
                scope: projects.label(),
                cause,
            })?;
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

/// Renders up to `MATCH_PREVIEW_LIMIT` IDs as [`id_prefix`] short forms, then how many remain.
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
    use crate::session::store::local_testing::{test_session_file, write_newest_corrupt_session};
    use crate::session::store::set_archived_at;
    use crate::session::store::testing::{
        isolated_store, seed_test_session, test_project_dir, test_store,
    };

    mod resolve_session {
        use super::*;

        #[tokio::test]
        async fn starts_fresh_when_no_continue_flag() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let resumed = resolve_session(&store, "m", None, None, ProjectScope::Current).unwrap();
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
        async fn resumes_from_external_path() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let original = handle::start(&store, "m", None);
            let full_id = original.session_id().to_owned();
            original
                .record_message(Message::user("external path test"))
                .await;
            original.finish(Vec::new()).await;
            let path = test_session_file(dir.path(), &full_id);
            drop(original);

            let external_dir = tempfile::tempdir().unwrap();
            let external_path = external_dir.path().join("copied.jsonl");
            std::fs::copy(&path, &external_path).unwrap();

            let arg = external_path.to_string_lossy().into_owned();
            let resumed = resolve_session(
                &store,
                "m",
                None,
                Some(SessionTarget::Named(&arg)),
                ProjectScope::Current,
            )
            .unwrap();
            assert_eq!(resumed.handle.session_id(), full_id);
            assert_eq!(resumed.transcript.messages.len(), 1);
            assert_eq!(resumed.title.as_deref(), Some("external path test"));
        }

        #[tokio::test]
        async fn prefix_resumes_single_match() {
            let (_dir, store) = isolated_store();
            let original = handle::start(&store, "m", None);
            let full_id = original.session_id().to_owned();
            original.record_message(Message::user("hello")).await;
            original.finish(Vec::new()).await;
            drop(original);

            let prefix = &full_id[..10];
            let resumed = resolve_session(
                &store,
                "m",
                None,
                Some(SessionTarget::Named(prefix)),
                ProjectScope::Current,
            )
            .unwrap();
            assert_eq!(resumed.handle.session_id(), full_id);
            assert_eq!(resumed.transcript.messages.len(), 1);
            assert_eq!(resumed.title.as_deref(), Some("hello"));
        }

        #[tokio::test]
        async fn prefix_matches_the_current_project_first_then_every_project() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let other = SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
            let at = time::macros::datetime!(2026-04-18 09:00:00 UTC);
            seed_test_session(&store, "abcd-local", Some("Local"), Some(1), at);
            seed_test_session(&other, "abcd-other", Some("Other"), Some(1), at);
            let resume = |prefix: &str, projects: ProjectScope| {
                resolve_session(
                    &store,
                    "m",
                    None,
                    Some(SessionTarget::Named(prefix)),
                    projects,
                )
                .map(|resumed| resumed.title)
            };

            assert_eq!(
                resume("abcd", ProjectScope::Current).unwrap().as_deref(),
                Some("Local")
            );
            assert_eq!(
                resume("abcd-o", ProjectScope::Current).unwrap().as_deref(),
                Some("Other")
            );
            assert_eq!(
                resume("abcd", ProjectScope::Every)
                    .err()
                    .unwrap()
                    .to_string(),
                "ambiguous prefix `abcd` matches 2 sessions: abcd-oth, abcd-loc",
                "--all weighs every project at once",
            );
        }

        #[tokio::test]
        async fn continue_resumes_the_newest_readable_unarchived_session() {
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

            let resumed = resolve_session(
                &store,
                "m",
                None,
                Some(SessionTarget::Latest),
                ProjectScope::Current,
            )
            .unwrap();
            assert_eq!(resumed.handle.session_id(), "newer");
            assert_eq!(resumed.title.as_deref(), Some("Newer"));
            assert_eq!(resumed.prior, ArchiveState::Active);
        }

        #[tokio::test]
        async fn prefix_resumes_and_unarchives_an_archived_session() {
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

            let resumed = resolve_session(
                &store,
                "m",
                None,
                Some(SessionTarget::Named("arch")),
                ProjectScope::Current,
            )
            .unwrap();
            assert_eq!(resumed.handle.session_id(), "archived");
            assert_eq!(resumed.prior, ArchiveState::Archived);
            assert_eq!(
                store
                    .latest(ProjectScope::Current)
                    .unwrap()
                    .map(|s| s.session_id)
                    .as_deref(),
                Some("archived"),
                "--continue finds the session again",
            );
        }

        #[test]
        fn continue_errors_without_sessions() {
            let (_dir, store) = isolated_store();
            let latest = |projects: ProjectScope| {
                resolve_session(&store, "m", None, Some(SessionTarget::Latest), projects)
                    .err()
                    .unwrap()
                    .to_string()
            };

            assert_eq!(
                latest(ProjectScope::Current),
                "no sessions to resume in this project (use --all to search every project)",
            );
            assert_eq!(latest(ProjectScope::Every), "no sessions to resume");
        }

        #[tokio::test]
        async fn prefix_errors_on_no_match() {
            let (_dir, store) = isolated_store();
            let s = handle::start(&store, "m", None);
            s.record_message(Message::user("noop")).await;
            s.finish(Vec::new()).await;
            drop(s);

            let err = resolve_session(
                &store,
                "m",
                None,
                Some(SessionTarget::Named("zzzz")),
                ProjectScope::Current,
            )
            .err()
            .unwrap()
            .to_string();
            assert_eq!(err, "no session matching `zzzz`");
        }

        #[test]
        fn prefix_reports_ambiguous_matches_newest_first() {
            let (_dir, store) = isolated_store();
            for i in 0..7 {
                seed_test_session(
                    &store,
                    &format!("ab{i}-session"),
                    None,
                    Some(1),
                    time::macros::datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i),
                );
            }

            let err = resolve_session(
                &store,
                "m",
                None,
                Some(SessionTarget::Named("ab")),
                ProjectScope::Current,
            )
            .err()
            .unwrap()
            .to_string();
            assert_eq!(
                err,
                "ambiguous prefix `ab` matches 7 sessions: \
                 ab6-sess, ab5-sess, ab4-sess, ab3-sess, ab2-sess, and 2 more",
            );
        }

        #[test]
        fn prefix_reports_a_listing_failure() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            std::fs::remove_dir_all(dir.path()).unwrap();

            for (projects, label) in [
                (ProjectScope::Current, "this project"),
                (ProjectScope::Every, "all projects"),
            ] {
                let cause = store.list_matching("ab", projects).unwrap_err();
                let err = resolve_session(
                    &store,
                    "m",
                    None,
                    Some(SessionTarget::Named("ab")),
                    projects,
                )
                .err()
                .unwrap();

                assert_eq!(
                    err.to_string(),
                    format!("failed to list sessions in {label}: {cause:#}"),
                );
            }
        }

        #[test]
        fn path_errors_on_missing_file() {
            let (_dir, store) = isolated_store();
            let err = resolve_session(
                &store,
                "m",
                None,
                Some(SessionTarget::Named("/does/not/exist.jsonl")),
                ProjectScope::Current,
            )
            .err()
            .unwrap();
            let cause = std::fs::metadata("/does/not/exist.jsonl").unwrap_err();
            assert_eq!(
                format!("{err:#}"),
                format!("failed to open /does/not/exist.jsonl: {cause}")
            );
        }
    }

    mod normalize_target {
        use super::*;

        #[test]
        fn maps_no_flag_to_fresh() {
            assert!(matches!(normalize_target(None).unwrap(), ResumeMode::Fresh));
        }

        #[test]
        fn maps_continue_to_latest() {
            assert!(matches!(
                normalize_target(Some(SessionTarget::Latest)).unwrap(),
                ResumeMode::Latest
            ));
        }

        #[test]
        fn trims_valid_prefix() {
            let mode = normalize_target(Some(SessionTarget::Named("  abc123 "))).unwrap();
            assert!(matches!(mode, ResumeMode::Prefix("abc123")));
        }

        #[test]
        fn classifies_path_arguments() {
            for raw in [
                "/abs/path.jsonl",
                "./relative.jsonl",
                "sub/dir/session.jsonl",
                "name.jsonl",           // no separator but has .jsonl suffix
                r"C:\Users\me\s.jsonl", // windows-shaped
            ] {
                let mode = normalize_target(Some(SessionTarget::Named(raw))).unwrap();
                assert!(
                    matches!(mode, ResumeMode::Path(p) if p == Path::new(raw)),
                    "{raw:?} should classify as Path",
                );
            }
        }

        #[test]
        fn keeps_uuid_shaped_prefix_as_prefix() {
            let mode = normalize_target(Some(SessionTarget::Named("a1b2c3d4-e5f6-7890"))).unwrap();
            assert!(matches!(mode, ResumeMode::Prefix("a1b2c3d4-e5f6-7890")));
        }

        #[test]
        fn rejects_empty_and_whitespace_prefix() {
            for raw in ["", "   ", "\t\n"] {
                let err = normalize_target(Some(SessionTarget::Named(raw)))
                    .err()
                    .unwrap()
                    .to_string();
                assert_eq!(
                    err,
                    "empty session ID prefix (run `ox -c` to continue the latest session or \
                     `ox -r` to pick one)",
                    "{raw:?}",
                );
            }
        }
    }

    mod resolve_prefix_to_info {
        use super::*;

        #[test]
        fn widens_to_other_projects_when_current_misses() {
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
            let info = resolve_prefix_to_info(&current, &target[..6], Some("live"))
                .unwrap()
                .expect("widening pass must surface the foreign session");
            assert_eq!(info.session_id, target);
        }

        #[test]
        fn no_match_resolves_to_no_session() {
            let (_dir, store) = isolated_store();
            assert!(
                resolve_prefix_to_info(&store, "zzz", Some("live"))
                    .unwrap()
                    .is_none(),
                "missing prefix is Ok(None) so callers can layer their own error",
            );
        }

        #[test]
        fn filters_live_id_so_caller_layers_its_own_message() {
            let (_dir, store) = isolated_store();
            let live_id = "abcd1234-eeff-1122-3344-556677889900";
            seed_test_session(
                &store,
                live_id,
                Some("Live"),
                Some(1),
                time::macros::datetime!(2026-04-18 09:00:00 UTC),
            );

            assert!(
                resolve_prefix_to_info(&store, &live_id[..6], Some(live_id))
                    .unwrap()
                    .is_none(),
                "live-id-only matches don't surface to the caller",
            );
        }

        #[test]
        fn ambiguous_lists_short_ids() {
            let (_dir, store) = isolated_store();
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

            let err = resolve_prefix_to_info(&store, "abcd", Some("live")).unwrap_err();

            assert!(
                matches!(
                    &err,
                    ResolveError::Ambiguous { prefix, count: 2, preview }
                        if prefix == "abcd" && preview == "abcd2222, abcd1111"
                ),
                "{err:?}",
            );
        }
    }

    mod resolve_saved_prefix {
        use super::*;

        #[test]
        fn tells_a_live_only_match_from_no_match() {
            let (_dir, store) = isolated_store();
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
            assert_eq!(saved.session_id, saved_id);
            assert!(matches!(
                resolve_saved_prefix(&store, "abcd1", live_id),
                Err(ResolveError::Live { prefix }) if prefix == "abcd1",
            ));
            assert!(matches!(
                resolve_saved_prefix(&store, "zzz", live_id),
                Err(ResolveError::NoMatch { prefix }) if prefix == "zzz",
            ));
        }
    }

    mod format_session_id_preview {
        use super::*;

        #[test]
        fn truncates_ids_to_eight_chars() {
            let ids = [
                "aaaaaaaaaaa".to_owned(),
                "bbbbbbbbbbb".to_owned(),
                "c".to_owned(),
            ];
            assert_eq!(format_session_id_preview(ids), "aaaaaaaa, bbbbbbbb, c");
        }

        #[test]
        fn caps_at_five_and_counts_the_rest() {
            let ids = (0..6).map(|i| format!("id{i}"));
            assert_eq!(
                format_session_id_preview(ids),
                "id0, id1, id2, id3, id4, and 1 more",
            );
        }

        #[test]
        fn lists_every_id_at_the_limit() {
            let ids = (0..5).map(|i| format!("id{i}"));
            assert_eq!(format_session_id_preview(ids), "id0, id1, id2, id3, id4");
        }
    }
}
