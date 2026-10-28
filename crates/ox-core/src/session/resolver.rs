//! Resolution of the CLI `--continue` and `--resume` targets, of resume targets to session IDs, and
//! of session IDs and their prefixes.

use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use tracing::debug;

use super::display::id_prefix;
use super::handle::{self, LoadedTranscript, ResumedSession};
use super::store::SessionStore;
use super::store::listing::{ArchiveState, ProjectScope, SessionInfo};
use crate::protocol::ResumeTarget;

/// The saved session a run continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTarget<'a> {
    /// The most recent unarchived session in scope, as `--continue` picks.
    Latest,
    /// The session an ID prefix or an export path names, as `--resume <session>` gives.
    Named(&'a str),
}

/// Normalized form of the CLI's session target.
enum ResumeMode {
    /// No target starts a new session.
    Fresh,
    /// The most recent unarchived session in scope.
    Latest,
    /// The session an ID prefix or an export path names.
    Named(ResumeTarget),
}

/// Starts a new session, or resumes the one `target` names. A fresh session records `git_branch`. A
/// prefix matches in the current project first and then in every project, as `/resume <prefix>`
/// does, while [`ProjectScope::Every`] searches every project at once for both a prefix and the
/// latest session.
pub async fn resolve_session(
    store: &SessionStore,
    model: &str,
    git_branch: Option<String>,
    target: Option<SessionTarget<'_>>,
    projects: ProjectScope,
) -> Result<ResumedSession> {
    let target = match normalize_target(target)? {
        ResumeMode::Fresh => {
            return Ok(ResumedSession {
                handle: handle::start(store, model, git_branch),
                transcript: LoadedTranscript::default(),
                title: None,
                prior: ArchiveState::Active,
            });
        }
        ResumeMode::Latest => {
            let scope_hint = match projects {
                ProjectScope::Current => " in this project (use --all to search every project)",
                ProjectScope::Every => "",
            };
            let latest = store
                .latest(projects)?
                .with_context(|| format!("no sessions to resume{scope_hint}"))?;
            ResumeTarget::Id(latest.session_id)
        }
        ResumeMode::Named(target) => target,
    };

    debug!(?target, "resuming session");
    let store = store.clone();
    let model = model.to_owned();
    tokio::task::spawn_blocking(move || {
        let session_id = resolve_target(&store, target, None, projects)?;
        handle::resume(&store, &session_id, &model)
    })
    .await
    .context("session resume task failed")?
}

/// Trims and classifies `target` into a [`ResumeMode`], rejecting a blank name, which names no
/// session.
fn normalize_target(target: Option<SessionTarget<'_>>) -> Result<ResumeMode> {
    match target {
        None => Ok(ResumeMode::Fresh),
        Some(SessionTarget::Latest) => Ok(ResumeMode::Latest),
        Some(SessionTarget::Named(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                bail!(
                    "empty session ID prefix \
                     (run `ox -c` to continue the latest session or `ox -r` to pick one)"
                );
            }
            Ok(ResumeMode::Named(if looks_like_path(trimmed) {
                ResumeTarget::Path(PathBuf::from(trimmed))
            } else {
                ResumeTarget::Prefix(trimmed.to_owned())
            }))
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

/// The ID of the session `target` names. A prefix matches sessions other than `live_id` in
/// `projects`, as [`resolve_prefix`] does, and a path names an export, which this imports first.
pub(super) fn resolve_target(
    store: &SessionStore,
    target: ResumeTarget,
    live_id: Option<&str>,
    projects: ProjectScope,
) -> Result<String> {
    match target {
        ResumeTarget::Id(session_id) => Ok(session_id),
        ResumeTarget::Prefix(prefix) => {
            Ok(resolve_prefix(store, &prefix, live_id, projects)?.session_id)
        }
        ResumeTarget::Path(path) => {
            let file =
                File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
            store
                .import(BufReader::new(file))
                .with_context(|| format!("failed to import {}", path.display()))
        }
    }
}

/// Writes the saved session `prefix` names to `out` as JSONL, matching in the current project first
/// and then in every project.
pub fn export_session(store: &SessionStore, prefix: &str, out: &mut dyn Write) -> Result<()> {
    let info = resolve_prefix(store, prefix, None, ProjectScope::Current)?;
    store.export(&info.session_id, out)
}

/// Why a session ID or its prefix resolved to no single session. `Display` is the lowercase
/// `anyhow` headline for startup, which slash commands rephrase as a sentence.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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
    #[error("failed to list sessions in {scope}: {cause}")]
    List { scope: &'static str, cause: String },
}

impl ResolveError {
    fn list(projects: ProjectScope, cause: &anyhow::Error) -> Self {
        Self::List {
            scope: projects.label(),
            cause: format!("{cause:#}"),
        }
    }
}

/// The saved session `session_id`, in any project.
pub(super) fn resolve_id(
    store: &SessionStore,
    session_id: &str,
) -> Result<SessionInfo, ResolveError> {
    store
        .find(session_id)
        .map_err(|cause| ResolveError::list(ProjectScope::Every, &cause))?
        .ok_or_else(|| ResolveError::NoMatch {
            prefix: session_id.to_owned(),
        })
}

/// The one saved session other than `live_id` whose ID starts with `prefix`, telling a prefix that
/// matches only the live session from one that matches nothing. [`ProjectScope::Current`] matches
/// in the current project first and then in every project, while [`ProjectScope::Every`] weighs
/// every project at once.
pub(super) fn resolve_prefix(
    store: &SessionStore,
    prefix: &str,
    live_id: Option<&str>,
    projects: ProjectScope,
) -> Result<SessionInfo, ResolveError> {
    if projects == ProjectScope::Current
        && let Some(info) = match_in_scope(store, prefix, live_id, ProjectScope::Current)?
    {
        return Ok(info);
    }
    match_in_scope(store, prefix, live_id, ProjectScope::Every)?.ok_or_else(|| {
        let prefix = prefix.to_owned();
        if live_id.is_some_and(|live_id| live_id.starts_with(&prefix)) {
            ResolveError::Live { prefix }
        } else {
            ResolveError::NoMatch { prefix }
        }
    })
}

fn match_in_scope(
    store: &SessionStore,
    prefix: &str,
    live_id: Option<&str>,
    projects: ProjectScope,
) -> Result<Option<SessionInfo>, ResolveError> {
    let mut matches = store
        .list_matching(prefix, projects)
        .map_err(|cause| ResolveError::list(projects, &cause))?;
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
    use crate::session::store::testing::{
        SEEDED_PROMPT, archive_test_session, isolated_store, seed_test_session, test_store,
    };

    mod resolve_session {
        use super::*;

        #[tokio::test]
        async fn starts_fresh_when_no_continue_flag() {
            let (_dir, store) = isolated_store();
            let resumed = resolve_session(&store, "m", None, None, ProjectScope::Current)
                .await
                .unwrap();
            assert_eq!(resumed.transcript.messages, []);
            assert!(resumed.title.is_none());
            assert!(resumed.transcript.tool_metadata.is_empty());
            assert_eq!(
                store.list().unwrap(),
                [],
                "the first message saves a fresh session"
            );
        }

        #[tokio::test]
        async fn imports_and_resumes_an_export_file() {
            let (_source_dir, source) = isolated_store();
            let original = handle::start(&source, "m", None);
            let full_id = original.session_id();
            original.record_message(Message::user("exported")).await;
            original.finalize(Vec::new()).await;
            let export_dir = tempfile::tempdir().unwrap();
            let export = export_dir.path().join("session.jsonl");
            source
                .export(&full_id, &mut File::create(&export).unwrap())
                .unwrap();
            let (_dir, store) = isolated_store();

            let arg = export.to_string_lossy().into_owned();
            let resumed = resolve_session(
                &store,
                "m",
                None,
                Some(SessionTarget::Named(&arg)),
                ProjectScope::Current,
            )
            .await
            .unwrap();

            assert_eq!(resumed.handle.session_id(), full_id);
            assert_eq!(resumed.transcript.messages, [Message::user("exported")]);
            assert_eq!(resumed.title.as_deref(), Some("exported"));
        }

        #[tokio::test]
        async fn prefix_resumes_single_match() {
            let (_dir, store) = isolated_store();
            let original = handle::start(&store, "m", None);
            let full_id = original.session_id();
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
            .await
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
            seed_test_session(&store, "abcd-local", Some("Local"), 1, at);
            seed_test_session(&other, "abcd-other", Some("Other"), 1, at);
            let resume = async |prefix: &str, projects: ProjectScope| {
                resolve_session(
                    &store,
                    "m",
                    None,
                    Some(SessionTarget::Named(prefix)),
                    projects,
                )
                .await
                .map(|resumed| resumed.title)
            };

            assert_eq!(
                resume("abcd", ProjectScope::Current)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("Local")
            );
            assert_eq!(
                resume("abcd-o", ProjectScope::Current)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("Other")
            );
            assert_eq!(
                resume("abcd", ProjectScope::Every)
                    .await
                    .err()
                    .unwrap()
                    .to_string(),
                "ambiguous prefix `abcd` matches 2 sessions: abcd-oth, abcd-loc",
                "--all weighs every project at once",
            );
        }

        #[tokio::test]
        async fn continue_resumes_the_newest_resumable_unarchived_session() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            seed_test_session(
                &store,
                "older",
                Some("Older"),
                1,
                time::macros::datetime!(2026-04-18 09:00:00 UTC),
            );
            seed_test_session(
                &store,
                "newer",
                Some("Newer"),
                1,
                time::macros::datetime!(2026-04-18 10:00:00 UTC),
            );
            seed_test_session(
                &store,
                "archived",
                Some("Archived"),
                1,
                time::macros::datetime!(2026-04-18 11:00:00 UTC),
            );
            archive_test_session(&store, "archived");
            seed_test_session(
                &store,
                "newer-build",
                Some("Newer build"),
                1,
                time::macros::datetime!(2026-04-18 12:00:00 UTC),
            );
            store
                .connect()
                .unwrap()
                .execute(
                    "UPDATE sessions SET format = format + 1 WHERE id = 'newer-build'",
                    [],
                )
                .unwrap();

            let resumed = resolve_session(
                &store,
                "m",
                None,
                Some(SessionTarget::Latest),
                ProjectScope::Current,
            )
            .await
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
                1,
                time::macros::datetime!(2026-04-18 09:00:00 UTC),
            );
            archive_test_session(&store, "archived");

            let resumed = resolve_session(
                &store,
                "m",
                None,
                Some(SessionTarget::Named("arch")),
                ProjectScope::Current,
            )
            .await
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

        #[tokio::test]
        async fn continue_errors_without_sessions() {
            let (_dir, store) = isolated_store();
            let latest = async |projects: ProjectScope| {
                resolve_session(&store, "m", None, Some(SessionTarget::Latest), projects)
                    .await
                    .err()
                    .unwrap()
                    .to_string()
            };

            assert_eq!(
                latest(ProjectScope::Current).await,
                "no sessions to resume in this project (use --all to search every project)",
            );
            assert_eq!(latest(ProjectScope::Every).await, "no sessions to resume");
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
            .await
            .err()
            .unwrap()
            .to_string();
            assert_eq!(err, "no session matching `zzzz`");
        }

        #[tokio::test]
        async fn prefix_reports_ambiguous_matches_newest_first() {
            let (_dir, store) = isolated_store();
            for i in 0..7 {
                seed_test_session(
                    &store,
                    &format!("ab{i}-session"),
                    None,
                    1,
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
        async fn prefix_reports_a_listing_failure() {
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
                .await
                .err()
                .unwrap();

                assert_eq!(
                    err.to_string(),
                    format!("failed to list sessions in {label}: {cause:#}"),
                );
            }
        }

        #[tokio::test]
        async fn path_errors_on_missing_file() {
            let (_dir, store) = isolated_store();
            let err = resolve_session(
                &store,
                "m",
                None,
                Some(SessionTarget::Named("/does/not/exist.jsonl")),
                ProjectScope::Current,
            )
            .await
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
            assert!(matches!(
                mode,
                ResumeMode::Named(ResumeTarget::Prefix(prefix)) if prefix == "abc123"
            ));
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
                    matches!(mode, ResumeMode::Named(ResumeTarget::Path(p)) if p == Path::new(raw)),
                    "{raw:?} should classify as Path",
                );
            }
        }

        #[test]
        fn keeps_uuid_shaped_prefix_as_prefix() {
            let mode = normalize_target(Some(SessionTarget::Named("a1b2c3d4-e5f6-7890"))).unwrap();
            assert!(matches!(
                mode,
                ResumeMode::Named(ResumeTarget::Prefix(prefix)) if prefix == "a1b2c3d4-e5f6-7890"
            ));
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
                    "empty session ID prefix \
                     (run `ox -c` to continue the latest session or `ox -r` to pick one)",
                    "{raw:?}",
                );
            }
        }
    }

    mod resolve_target {
        use super::*;

        const ID: &str = "abcd1234-eeff-1122-3344-556677889900";

        #[test]
        fn resolves_an_id_a_prefix_and_an_imported_export() {
            let (dir, source) = isolated_store();
            seed_test_session(
                &source,
                ID,
                Some("Saved"),
                1,
                time::macros::datetime!(2026-04-18 09:00:00 UTC),
            );
            let export = dir.path().join("session.jsonl");
            source
                .export(ID, &mut File::create(&export).unwrap())
                .unwrap();
            let (_dir, imported) = isolated_store();

            for (store, target) in [
                (&source, ResumeTarget::Id(ID.to_owned())),
                (&source, ResumeTarget::Prefix("abcd".to_owned())),
                (&imported, ResumeTarget::Path(export)),
            ] {
                let id = resolve_target(store, target.clone(), None, ProjectScope::Current);
                assert_eq!(id.unwrap(), ID, "{target:?}");
            }
            assert_eq!(
                saved_title(&imported, ID),
                Some("Saved".to_owned()),
                "the import saves the session"
            );
        }

        #[test]
        fn names_a_file_it_cannot_open_or_import() {
            let (dir, store) = isolated_store();
            let missing = dir.path().join("missing.jsonl");
            let foreign = dir.path().join("foreign.jsonl");
            std::fs::write(&foreign, "{\"type\":\"header\"}\n").unwrap();

            let resolve = |path: &Path| {
                let target = ResumeTarget::Path(path.to_path_buf());
                let err = resolve_target(&store, target, None, ProjectScope::Current).unwrap_err();
                format!("{err:#}")
            };

            assert!(
                resolve(&missing).starts_with(&format!("failed to open {}: ", missing.display())),
                "{}",
                resolve(&missing),
            );
            assert_eq!(
                resolve(&foreign),
                format!(
                    "failed to import {}: line 1: not an ox session export, which starts with a \
                     session record",
                    foreign.display()
                ),
            );
        }

        fn saved_title(store: &SessionStore, id: &str) -> Option<String> {
            store.find(id).unwrap()?.title
        }
    }

    mod export_session {
        use super::*;

        #[test]
        fn writes_the_session_a_prefix_names_in_any_project() {
            let dir = tempfile::tempdir().unwrap();
            let other = SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
            let id = "abcd1234-eeff-1122-3344-556677889900";
            seed_test_session(
                &other,
                id,
                Some("Foreign"),
                1,
                time::macros::datetime!(2026-04-18 09:00:00 UTC),
            );
            let current = test_store(dir.path());
            let mut by_prefix = Vec::new();
            let mut by_id = Vec::new();

            export_session(&current, "abcd", &mut by_prefix).unwrap();
            other.export(id, &mut by_id).unwrap();

            assert_eq!(by_prefix, by_id);
            assert!(
                String::from_utf8(by_prefix)
                    .unwrap()
                    .contains(&format!(r#""text":"{SEEDED_PROMPT}""#)),
            );
            assert_eq!(
                export_session(&current, "zzzz", &mut Vec::new())
                    .unwrap_err()
                    .to_string(),
                "no session matching `zzzz`",
            );
        }
    }

    mod resolve_id {
        use super::*;

        #[test]
        fn finds_the_session_in_another_project_and_tells_an_unknown_id() {
            let dir = tempfile::tempdir().unwrap();
            let other = SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
            let id = "abcd1234-eeff-1122-3344-556677889900";
            seed_test_session(
                &other,
                id,
                Some("Foreign"),
                1,
                time::macros::datetime!(2026-04-18 09:00:00 UTC),
            );
            let current = test_store(dir.path());

            assert_eq!(resolve_id(&current, id).unwrap().session_id, id);
            assert_eq!(
                resolve_id(&current, "abcd1234"),
                Err(ResolveError::NoMatch {
                    prefix: "abcd1234".to_owned()
                }),
                "an ID matches only in full",
            );
        }
    }

    mod resolve_prefix {
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
                1,
                time::macros::datetime!(2026-04-18 09:00:00 UTC),
            );
            let current = test_store(dir.path());

            let info = resolve_prefix(&current, &target[..6], Some("live"), ProjectScope::Current)
                .unwrap();

            assert_eq!(info.session_id, target);
        }

        #[test]
        fn tells_a_live_only_match_from_no_match() {
            let (_dir, store) = isolated_store();
            let live_id = "abcd1234-eeff-1122-3344-556677889900";
            let saved_id = "abcd5678-eeff-1122-3344-556677889900";
            for id in [live_id, saved_id] {
                seed_test_session(
                    &store,
                    id,
                    Some("Saved"),
                    1,
                    time::macros::datetime!(2026-04-18 09:00:00 UTC),
                );
            }
            let resolve =
                |prefix| resolve_prefix(&store, prefix, Some(live_id), ProjectScope::Current);

            assert_eq!(resolve("abcd").unwrap().session_id, saved_id);
            assert_eq!(
                resolve("abcd1"),
                Err(ResolveError::Live {
                    prefix: "abcd1".to_owned()
                })
            );
            assert_eq!(
                resolve("zzz"),
                Err(ResolveError::NoMatch {
                    prefix: "zzz".to_owned()
                })
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
                seed_test_session(&store, id, Some("dup"), 1, created_at);
            }

            let err = resolve_prefix(&store, "abcd", Some("live"), ProjectScope::Current);

            assert_eq!(
                err,
                Err(ResolveError::Ambiguous {
                    prefix: "abcd".to_owned(),
                    count: 2,
                    preview: "abcd2222, abcd1111".to_owned(),
                })
            );
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
