//! Session listing: stat-only directory walks that parse only the transcripts a caller needs.

use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::vec;

use anyhow::{Context as _, Result};
use time::OffsetDateTime;
use tracing::warn;

use super::{JsonlLines, Project, SessionStore, file_session_id, read_header};
use crate::session::entry::{
    ArchiveState, Entry, ExitInfo, Header, SessionInfo, TitleInfo, keep_newer,
};

pub const DEFAULT_SESSION_LIST_LIMIT: usize = 30;

// ── ListScope ──

/// Which sessions a listing walks: the home project or every project, and active or archived ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListScope {
    pub projects: ProjectScope,
    pub archive: ArchiveState,
}

/// Which projects a listing or lookup covers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProjectScope {
    /// The project the store was opened for.
    #[default]
    Current,
    Every,
}

impl ProjectScope {
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Current => Self::Every,
            Self::Every => Self::Current,
        }
    }

    /// Where a listing in this scope looks, as error messages name it.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Current => "this project",
            Self::Every => "all projects",
        }
    }
}

// ── ListPage ──

/// One page of sessions, and whether more matching sessions follow it.
#[derive(Debug)]
pub struct ListPage {
    sessions: Vec<SessionInfo>,
    more: bool,
}

impl ListPage {
    pub fn sessions(&self) -> &[SessionInfo] {
        &self.sessions
    }

    #[cfg(test)]
    pub(crate) fn into_sessions(self) -> Vec<SessionInfo> {
        self.sessions
    }

    pub fn has_more(&self) -> bool {
        self.more
    }
}

// ── SessionWalk ──

/// The readable sessions in one archive state, most recently active first, each parsed as it is
/// drawn. The walk lists and sorts the files when it starts, so it never draws a session created
/// after that.
pub struct SessionWalk {
    paths: vec::IntoIter<(PathBuf, OffsetDateTime)>,
    archive: ArchiveState,
    /// The project a home-project walk keeps, `None` for a walk of every project.
    project: Option<Project>,
}

impl SessionWalk {
    /// Whether the walk has drawn every file it listed.
    pub fn is_exhausted(&self) -> bool {
        self.paths.as_slice().is_empty()
    }

    /// The next session, like [`Iterator::next`], or `None` early once `stop` returns true. `stop`
    /// runs before each file is parsed, so a caller can end a long scan between files.
    pub fn next_unless(&mut self, stop: impl Fn() -> bool) -> Option<SessionInfo> {
        while !stop() {
            let (path, modified) = self.paths.next()?;
            if let Some(info) = read_readable(&path, modified, self.project.as_ref())
                && info.archive_state() == self.archive
            {
                return Some(info);
            }
        }
        None
    }
}

impl Iterator for SessionWalk {
    type Item = SessionInfo;

    fn next(&mut self) -> Option<SessionInfo> {
        self.next_unless(|| false)
    }
}

// ── Listing ──

impl SessionStore {
    /// Walks the sessions matching `scope`.
    pub fn walk(&self, scope: ListScope) -> Result<SessionWalk> {
        Ok(SessionWalk {
            paths: self.session_paths(scope.projects)?.into_iter(),
            archive: scope.archive,
            project: self.scope_project(scope.projects),
        })
    }

    /// Up to `limit` sessions from the [`walk`](Self::walk) of `scope`, unbounded for `None`.
    /// Archive state lives in each file, so the walk reads on to the next match past the page.
    pub fn list_paged(&self, limit: Option<usize>, scope: ListScope) -> Result<ListPage> {
        let limit = limit.unwrap_or(usize::MAX);
        let mut sessions: Vec<_> = self.walk(scope)?.take(limit.saturating_add(1)).collect();
        let more = sessions.len() > limit;
        sessions.truncate(limit);
        Ok(ListPage { sessions, more })
    }

    /// The most recently active readable unarchived session.
    pub(crate) fn latest(&self, projects: ProjectScope) -> Result<Option<SessionInfo>> {
        let scope = ListScope {
            projects,
            archive: ArchiveState::Active,
        };
        Ok(self.walk(scope)?.next())
    }

    /// Readable sessions whose ID starts with `prefix`, most recently active first. IDs are
    /// matched on file names, so only the matching files are parsed.
    pub(crate) fn list_matching(
        &self,
        prefix: &str,
        projects: ProjectScope,
    ) -> Result<Vec<SessionInfo>> {
        let mut paths = self.session_paths(projects)?;
        paths.retain(|(path, _)| file_session_id(path).is_some_and(|id| id.starts_with(prefix)));
        let project = self.scope_project(projects);
        Ok(paths
            .into_iter()
            .filter_map(|(path, modified)| read_readable(&path, modified, project.as_ref()))
            .collect())
    }

    /// The project a walk of the home project keeps, or `None` for every project.
    fn scope_project(&self, projects: ProjectScope) -> Option<Project> {
        (projects == ProjectScope::Current).then(|| self.project.clone())
    }

    /// Session files paired with their mtimes, most recently active first. The home project's
    /// files are in its own directory and, for older sessions, the launch directory's.
    fn session_paths(&self, projects: ProjectScope) -> Result<Vec<(PathBuf, OffsetDateTime)>> {
        let mut paths = match projects {
            ProjectScope::Current => {
                let mut paths = collect_session_paths(&self.project_dir)?;
                if let Some(cwd_dir) = self.cwd_dir.as_ref().filter(|dir| dir.is_dir()) {
                    paths.append(&mut collect_session_paths(cwd_dir)?);
                }
                paths
            }
            ProjectScope::Every => self.collect_paths_all_projects()?,
        };
        sort_paths_recent_first(&mut paths);
        Ok(paths)
    }

    fn collect_paths_all_projects(&self) -> Result<Vec<(PathBuf, OffsetDateTime)>> {
        let mut out = Vec::new();
        for entry in read_dir_entries(&self.sessions_dir)? {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                match collect_session_paths(&entry.path()) {
                    Ok(mut paths) => out.append(&mut paths),
                    Err(e) => warn!("skipping project dir {}: {e}", entry.path().display()),
                }
            }
        }
        Ok(out)
    }
}

/// Pairs each `.jsonl` file in `dir` with its mtime. A failed stat is warn-skipped, because an
/// epoch fallback would quietly sort the file out of every page.
fn collect_session_paths(dir: &Path) -> Result<Vec<(PathBuf, OffsetDateTime)>> {
    Ok(read_dir_entries(dir)?
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|e| {
            let path = e.path();
            e.metadata()
                .and_then(|m| m.modified())
                .inspect_err(|err| {
                    warn!("skipping session {} (stat failed): {err}", path.display());
                })
                .ok()
                .map(|t| (path, OffsetDateTime::from(t)))
        })
        .collect())
}

/// The entries of `dir`, warn-skipping any the OS fails to read.
fn read_dir_entries(dir: &Path) -> Result<impl Iterator<Item = fs::DirEntry>> {
    let entries = fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?;
    Ok(entries.filter_map(|entry| {
        entry
            .inspect_err(|e| warn!("skipping directory entry: {e}"))
            .ok()
    }))
}

fn sort_paths_recent_first(paths: &mut [(PathBuf, OffsetDateTime)]) {
    paths
        .sort_by(|(p1, m1), (p2, m2)| m2.cmp(m1).then_with(|| p2.file_name().cmp(&p1.file_name())));
}

/// [`read_session_info`], warn-skipping an unreadable file.
fn read_readable(
    path: &Path,
    modified: OffsetDateTime,
    project: Option<&Project>,
) -> Option<SessionInfo> {
    read_session_info(path, modified, project)
        .inspect_err(|e| warn!("skipping unreadable session file {}: {e:#}", path.display()))
        .ok()
        .flatten()
}

// ── Session Info Extraction ──

// Cheap pre-filter: `serde(tag = "type")` always emits `type` first.
const TITLE_LINE_PREFIX: &str = r#"{"type":"title""#;
const ARCHIVE_LINE_PREFIX: &str = r#"{"type":"archive""#;
const SUMMARY_LINE_PREFIX: &str = r#"{"type":"summary""#;

/// Reads the header, the latest title and summary, and the archive state, or `None` past the
/// header for a session that `project` does not own. `last_active_at` is the mtime the walk
/// sorted by, so the listed time matches the order.
fn read_session_info(
    path: &Path,
    last_active_at: OffsetDateTime,
    project: Option<&Project>,
) -> Result<Option<SessionInfo>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut lines = JsonlLines::new(BufReader::new(&file));
    let Header {
        session_id,
        cwd,
        project_root,
        git_branch,
        ..
    } = read_header(&mut lines, path)?;
    if project.is_some_and(|project| !project.owns(&cwd, project_root.as_deref())) {
        return Ok(None);
    }

    let mut title: Option<TitleInfo> = None;
    let mut exit: Option<ExitInfo> = None;
    let mut archived = false;
    while let Some((_, line)) = lines.next_line()? {
        if line.starts_with(TITLE_LINE_PREFIX)
            && let Ok(Entry::Title {
                title: text,
                updated_at,
            }) = serde_json::from_str(line)
        {
            keep_newer(
                &mut title,
                TitleInfo {
                    title: text,
                    updated_at,
                },
            );
        } else if line.starts_with(SUMMARY_LINE_PREFIX)
            && let Ok(Entry::Summary {
                message_count,
                updated_at,
            }) = serde_json::from_str(line)
        {
            keep_newer(
                &mut exit,
                ExitInfo {
                    message_count,
                    updated_at,
                },
            );
        } else if line.starts_with(ARCHIVE_LINE_PREFIX)
            && let Ok(Entry::Archive {
                archived: state, ..
            }) = serde_json::from_str(line)
        {
            archived = state;
        }
    }

    Ok(Some(SessionInfo {
        session_id,
        path: path.to_owned(),
        cwd,
        project_root,
        last_active_at,
        title,
        exit,
        git_branch,
        archived,
    }))
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;
    use crate::session::store::local_testing::{
        sample_header, sample_message_entry, sample_summary_entry, sample_title_entry,
        test_session_file, write_newest_corrupt_session,
    };
    use crate::session::store::set_archived_at;
    use crate::session::store::testing::{
        isolated_store, listed_ids, seed_test_session, set_modified, test_header, test_project_dir,
        test_store,
    };
    use crate::util::path::testing::HOME_VAR;

    /// Builds `count` sessions whose mtimes and file names both ascend in seed order.
    fn seed_n_sessions(store: &SessionStore, count: usize) {
        for i in 0..count {
            let id = format!("session-{i:04}");
            let offset = i64::try_from(i).expect("test count fits i64");
            let created_at = datetime!(2026-04-15 10:00:00 UTC) + time::Duration::seconds(offset);
            let mut writer = store
                .create(&test_header(store, &id, created_at), &[])
                .unwrap();
            writer
                .append(&[sample_title_entry(&format!("Title {i}"))])
                .unwrap();
            set_modified(writer.path(), created_at);
        }
    }

    mod session_walk {
        use super::*;

        mod next_unless {
            use super::*;

            #[test]
            fn stops_before_parsing_once_asked() {
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                seed_n_sessions(&store, 3);
                set_archived_at(&test_session_file(dir.path(), "session-0002"), true).unwrap();
                let mut walk = store.walk(ListScope::default()).unwrap();
                let checks = std::cell::Cell::new(0);
                let stop_at_second_file = || {
                    checks.set(checks.get() + 1);
                    checks.get() > 1
                };

                assert!(
                    walk.next_unless(stop_at_second_file).is_none(),
                    "the archived file came first",
                );
                assert_eq!(
                    walk.next().map(|info| info.session_id).as_deref(),
                    Some("session-0001"),
                    "the walk resumes at the file it stopped before",
                );
            }
        }
    }

    mod walk {
        use super::*;
        use crate::session::entry::CURRENT_VERSION;

        /// A header that records `cwd` and, from releases that record one, `project_root`.
        fn header_in(session_id: &str, cwd: &str, project_root: Option<&str>) -> Header {
            Header {
                session_id: session_id.to_owned(),
                cwd: cwd.to_owned(),
                project_root: project_root.map(str::to_owned),
                model: "m".to_owned(),
                created_at: datetime!(2026-04-15 10:00:00 UTC),
                version: CURRENT_VERSION,
                git_branch: None,
            }
        }

        /// Runs `f` with session storage under a fresh `XDG_DATA_HOME`.
        fn with_isolated_data(f: impl FnOnce()) {
            let xdg = tempfile::tempdir().unwrap();
            let home = tempfile::tempdir().unwrap();
            temp_env::with_vars(
                [
                    ("XDG_DATA_HOME", Some(xdg.path().as_os_str())),
                    (HOME_VAR, Some(home.path().as_os_str())),
                ],
                f,
            );
        }

        #[test]
        fn leaves_out_another_project_whose_directory_name_collides() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                "home",
                None,
                None,
                datetime!(2026-04-15 10:00:00 UTC),
            );
            let spaced = "/work/test project";
            for (id, project_root) in [("recorded", Some(spaced)), ("older", None)] {
                store
                    .create(&header_in(id, spaced, project_root), &[])
                    .unwrap();
            }

            let mut all: Vec<_> = store
                .walk(ListScope {
                    projects: ProjectScope::Every,
                    archive: ArchiveState::Active,
                })
                .unwrap()
                .map(|info| info.session_id)
                .collect();
            all.sort_unstable();

            assert_eq!(listed_ids(&store, ArchiveState::Active), ["home"]);
            assert_eq!(all, ["home", "older", "recorded"]);
        }

        #[test]
        fn includes_older_sessions_saved_under_the_launch_directory() {
            with_isolated_data(|| {
                let (cwd, root) = (Path::new("/repo/sub"), Path::new("/repo"));
                let legacy = SessionStore::open(cwd, cwd).unwrap();
                let older = legacy
                    .create(&header_in("older", "/repo/sub", None), &[])
                    .unwrap();
                set_modified(older.path(), datetime!(2026-04-15 10:00:00 UTC));
                let store = SessionStore::open(cwd, root).unwrap();
                seed_test_session(
                    &store,
                    "newer",
                    None,
                    None,
                    datetime!(2026-04-16 10:00:00 UTC),
                );

                assert_eq!(listed_ids(&store, ArchiveState::Active), ["newer", "older"]);
                assert_eq!(
                    listed_ids(
                        &SessionStore::open(root, root).unwrap(),
                        ArchiveState::Active
                    ),
                    ["newer"],
                    "the root alone has no launch directory to read older sessions from",
                );
            });
        }

        #[test]
        fn lists_a_subdirectory_without_older_sessions() {
            with_isolated_data(|| {
                let root = Path::new("/repo");
                seed_test_session(
                    &SessionStore::open(root, root).unwrap(),
                    "at-root",
                    None,
                    None,
                    datetime!(2026-04-15 10:00:00 UTC),
                );

                let store = SessionStore::open(Path::new("/repo/sub"), root).unwrap();

                assert_eq!(listed_ids(&store, ArchiveState::Active), ["at-root"]);
            });
        }
    }

    mod list {
        use super::*;

        #[test]
        fn orders_sessions_by_mtime_newest_first() {
            let (_dir, store) = isolated_store();

            let mut wa = store
                .create(
                    &test_header(&store, "aaa", datetime!(2026-04-15 10:00:00 UTC)),
                    &[],
                )
                .unwrap();
            wa.append(&[sample_title_entry("Older")]).unwrap();
            wa.append(&[sample_summary_entry(3)]).unwrap();
            set_modified(wa.path(), datetime!(2026-04-15 10:00:00 UTC));

            let mut wb = store
                .create(
                    &test_header(&store, "bbb", datetime!(2026-04-16 12:00:00 UTC)),
                    &[],
                )
                .unwrap();
            wb.append(&[sample_title_entry("Newer")]).unwrap();
            wb.append(&[sample_summary_entry(5)]).unwrap();
            set_modified(wb.path(), datetime!(2026-04-16 12:00:00 UTC));

            let sessions = store.list().unwrap();
            assert_eq!(sessions.len(), 2);
            assert_eq!(sessions[0].session_id, "bbb");
            assert_eq!(sessions[0].title.as_ref().unwrap().title, "Newer");
            assert_eq!(sessions[0].exit.as_ref().unwrap().message_count, 5);
            assert_eq!(sessions[1].session_id, "aaa");
        }

        #[test]
        fn mtime_overrides_header_created_at_order() {
            let (_dir, store) = isolated_store();

            let mut w_old = store
                .create(
                    &test_header(&store, "aaa-old-header", datetime!(2026-01-01 10:00:00 UTC)),
                    &[],
                )
                .unwrap();
            w_old.append(&[sample_title_entry("Old")]).unwrap();
            set_modified(w_old.path(), datetime!(2026-04-18 10:00:00 UTC));

            let mut w_new = store
                .create(
                    &test_header(&store, "zzz-new-header", datetime!(2026-04-17 10:00:00 UTC)),
                    &[],
                )
                .unwrap();
            w_new.append(&[sample_title_entry("New")]).unwrap();
            set_modified(w_new.path(), datetime!(2026-04-17 10:00:00 UTC));

            let sessions = store.list().unwrap();
            assert_eq!(sessions.len(), 2);
            assert_eq!(
                sessions[0].session_id, "aaa-old-header",
                "the file with the later mtime comes first despite its older header"
            );
            assert!(
                sessions[0].last_active_at > sessions[1].last_active_at,
                "mtime drives ordering"
            );
            assert_eq!(sessions[1].session_id, "zzz-new-header");
        }

        #[test]
        fn picks_latest_title_when_re_appended() {
            let (_dir, store) = isolated_store();
            let mut writer = store.create(&sample_header("retitled"), &[]).unwrap();
            writer
                .append(&[Entry::Title {
                    title: "original prompt".to_owned(),
                    updated_at: datetime!(2026-04-16 12:00:00 UTC),
                }])
                .unwrap();
            writer
                .append(&[Entry::Title {
                    title: "AI generated".to_owned(),
                    updated_at: datetime!(2026-04-16 12:01:00 UTC),
                }])
                .unwrap();

            let sessions = store.list().unwrap();
            let title = sessions[0].title.as_ref().unwrap();
            assert_eq!(title.title, "AI generated");
        }

        #[test]
        fn keeps_the_newest_summary_whatever_its_place_in_the_file() {
            let (_dir, store) = isolated_store();
            let mut writer = store.create(&sample_header("resumed"), &[]).unwrap();
            writer
                .append(&[
                    sample_summary_entry(4),
                    Entry::Summary {
                        message_count: 2,
                        updated_at: datetime!(2026-04-16 12:00:00 UTC),
                    },
                ])
                .unwrap();

            let sessions = store.list().unwrap();

            assert_eq!(sessions[0].exit.as_ref().unwrap().message_count, 4);
        }

        #[test]
        fn finds_ai_title_buried_between_head_and_tail() {
            let (_dir, store) = isolated_store();
            let mut writer = store.create(&sample_header("buried"), &[]).unwrap();
            writer
                .append(&[Entry::Title {
                    title: "first prompt".to_owned(),
                    updated_at: datetime!(2026-04-16 12:00:00 UTC),
                }])
                .unwrap();
            writer
                .append(&[sample_message_entry(Uuid::new_v4(), "short user text")])
                .unwrap();
            writer
                .append(&[Entry::Title {
                    title: "AI picked".to_owned(),
                    updated_at: datetime!(2026-04-16 12:00:05 UTC),
                }])
                .unwrap();
            let bulky_body = "x".repeat(16_000);
            writer
                .append(&[sample_message_entry(Uuid::new_v4(), &bulky_body)])
                .unwrap();
            writer.append(&[sample_summary_entry(2)]).unwrap();

            let sessions = store.list().unwrap();
            let title = sessions[0].title.as_ref().unwrap();
            assert_eq!(title.title, "AI picked");
            let exit = sessions[0].exit.as_ref().unwrap();
            assert_eq!(exit.message_count, 2);
        }

        #[test]
        fn leaves_absent_title_and_exit_fields_unset() {
            let (_dir, store) = isolated_store();
            let _writer = store.create(&sample_header("bare"), &[]).unwrap();

            let sessions = store.list().unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, "bare");
            assert!(sessions[0].title.is_none());
            assert!(sessions[0].exit.is_none());
        }

        #[test]
        fn skips_non_session_files_and_malformed_jsonl() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let project_dir = test_project_dir(dir.path());
            fs::write(project_dir.join("notes.txt"), "not a session").unwrap();
            fs::write(
                project_dir.join("bad.jsonl"),
                r#"{"type":"message","uuid":"00000000-0000-0000-0000-000000000000","message":{"role":"user","content":[]},"timestamp":"2026-01-01T00:00:00Z"}"#,
            )
            .unwrap();
            assert!(store.list().unwrap().is_empty());
        }

        #[test]
        fn is_scoped_to_current_project() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let _own = store.create(&sample_header("own"), &[]).unwrap();

            let sibling_store =
                SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
            let _foreign = sibling_store
                .create(
                    &test_header(
                        &sibling_store,
                        "foreign",
                        datetime!(2026-04-16 12:00:00 UTC),
                    ),
                    &[],
                )
                .unwrap();
            drop(sibling_store);

            let sessions = store.list().unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, "own");
        }

        #[test]
        fn missing_project_reports_directory_error() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let project_dir = test_project_dir(dir.path());
            fs::remove_dir(&project_dir).unwrap();

            let error = store.list().unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("failed to read {}", project_dir.display())
            );
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::NotFound,
            );
        }
    }

    mod list_paged {
        use super::*;

        #[test]
        fn caps_at_limit_keeping_most_recent() {
            let (_dir, store) = isolated_store();
            seed_n_sessions(&store, 10);

            let page = store.list_paged(Some(3), ListScope::default()).unwrap();
            assert_eq!(page.sessions().len(), 3, "cap honored");
            assert!(page.has_more(), "seven sessions follow the page");
            assert_eq!(page.sessions()[0].session_id, "session-0009");
            assert_eq!(page.sessions()[2].session_id, "session-0007");
        }

        #[test]
        fn unbounded_includes_every_session_without_more() {
            let (_dir, store) = isolated_store();
            seed_n_sessions(&store, 4);

            let page = store.list_paged(None, ListScope::default()).unwrap();
            assert_eq!(page.sessions().len(), 4);
            assert!(!page.has_more());
        }

        #[test]
        fn all_spans_every_project_subdirectory() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let _own = store.create(&sample_header("own"), &[]).unwrap();

            let foreign_store =
                SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
            let _foreign = foreign_store
                .create(
                    &test_header(
                        &foreign_store,
                        "foreign",
                        datetime!(2026-04-16 12:00:00 UTC),
                    ),
                    &[],
                )
                .unwrap();
            drop(foreign_store);

            let all = store
                .list_paged(
                    None,
                    ListScope {
                        projects: ProjectScope::Every,
                        ..ListScope::default()
                    },
                )
                .unwrap()
                .into_sessions();
            let mut ids: Vec<_> = all.iter().map(|s| s.session_id.as_str()).collect();
            ids.sort_unstable();
            assert_eq!(ids, vec!["foreign", "own"]);
        }

        #[test]
        fn lists_one_archive_state_by_its_latest_entry() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            seed_n_sessions(&store, 4);
            let path = |id: &str| test_session_file(dir.path(), id);
            set_archived_at(&path("session-0001"), true).unwrap();
            set_archived_at(&path("session-0002"), true).unwrap();
            set_archived_at(&path("session-0003"), true).unwrap();
            set_archived_at(&path("session-0003"), false).unwrap();
            let ids = |archive: ArchiveState| -> Vec<String> {
                let mut ids: Vec<_> = store
                    .list_paged(
                        None,
                        ListScope {
                            archive,
                            ..ListScope::default()
                        },
                    )
                    .unwrap()
                    .into_sessions()
                    .into_iter()
                    .map(|s| s.session_id)
                    .collect();
                ids.sort_unstable();
                ids
            };

            assert_eq!(ids(ArchiveState::Active), ["session-0000", "session-0003"]);
            assert_eq!(
                ids(ArchiveState::Archived),
                ["session-0001", "session-0002"]
            );
            assert!(
                store
                    .list_paged(
                        Some(1),
                        ListScope {
                            archive: ArchiveState::Archived,
                            ..ListScope::default()
                        }
                    )
                    .unwrap()
                    .has_more()
            );
            assert!(
                !store
                    .list_paged(
                        Some(2),
                        ListScope {
                            archive: ArchiveState::Archived,
                            ..ListScope::default()
                        }
                    )
                    .unwrap()
                    .has_more(),
                "active sessions past the page are not more archived ones",
            );
        }

        #[test]
        fn skips_unreadable_files() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            seed_n_sessions(&store, 3);
            write_newest_corrupt_session(dir.path());

            let page = store.list_paged(Some(2), ListScope::default()).unwrap();
            let ids: Vec<_> = page
                .sessions()
                .iter()
                .map(|s| s.session_id.as_str())
                .collect();
            assert_eq!(ids, ["session-0002", "session-0001"]);
            assert!(page.has_more());

            let full = store.list_paged(Some(3), ListScope::default()).unwrap();
            assert_eq!(full.sessions().len(), 3);
            assert!(
                !full.has_more(),
                "an unreadable file is not a session past the page"
            );
        }

        #[test]
        fn zero_limit_yields_no_rows_but_flags_more() {
            // The CLI maps `--limit 0` to `None`, so `Some(0)` reaching the store means zero rows.
            let (_dir, store) = isolated_store();
            seed_n_sessions(&store, 3);

            let page = store.list_paged(Some(0), ListScope::default()).unwrap();
            assert!(page.sessions().is_empty());
            assert!(page.has_more());
        }

        #[cfg(unix)]
        #[test]
        fn all_skips_stray_files_and_unreadable_project_dirs() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let _own = store.create(&sample_header("own"), &[]).unwrap();
            fs::write(dir.path().join("stray.jsonl"), "").unwrap();
            let locked = dir.path().join("locked-project");
            fs::create_dir(&locked).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

            let page = store.list_paged(
                None,
                ListScope {
                    projects: ProjectScope::Every,
                    ..ListScope::default()
                },
            );
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();

            let ids: Vec<_> = page
                .unwrap()
                .into_sessions()
                .into_iter()
                .map(|s| s.session_id)
                .collect();
            assert_eq!(ids, ["own"]);
        }
    }

    mod latest {
        use super::*;

        #[test]
        fn skips_archived_sessions() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            seed_n_sessions(&store, 2);
            let latest_id = || {
                store
                    .latest(ProjectScope::Current)
                    .unwrap()
                    .map(|s| s.session_id)
            };
            assert_eq!(latest_id().as_deref(), Some("session-0001"));

            set_archived_at(&test_session_file(dir.path(), "session-0001"), true).unwrap();
            assert_eq!(latest_id().as_deref(), Some("session-0000"));

            set_archived_at(&test_session_file(dir.path(), "session-0000"), true).unwrap();
            assert_eq!(latest_id(), None);
        }
    }

    mod list_matching {
        use super::*;

        #[test]
        fn leaves_out_another_project_whose_directory_name_collides() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                "abcd-home",
                None,
                None,
                datetime!(2026-04-15 10:00:00 UTC),
            );
            let mut foreign = sample_header("abcd-coll");
            foreign.cwd = "/work/test project".to_owned();
            foreign.project_root = Some(foreign.cwd.clone());
            store.create(&foreign, &[]).unwrap();

            let ids: Vec<_> = store
                .list_matching("abcd", ProjectScope::Current)
                .unwrap()
                .into_iter()
                .map(|s| s.session_id)
                .collect();

            assert_eq!(ids, ["abcd-home"]);
        }

        #[test]
        fn matches_the_id_part_of_file_names_newest_first() {
            let (_dir, store) = isolated_store();
            for (id, created_at) in [
                ("abc-older", datetime!(2026-04-16 12:00:00 UTC)),
                ("abd-newer", datetime!(2026-04-16 12:00:01 UTC)),
                ("xyz", datetime!(2026-04-16 12:00:02 UTC)),
            ] {
                seed_test_session(&store, id, None, None, created_at);
            }
            let ids = |prefix: &str| -> Vec<String> {
                let sessions = store.list_matching(prefix, ProjectScope::Current).unwrap();
                sessions.into_iter().map(|s| s.session_id).collect()
            };

            assert_eq!(ids("ab"), ["abd-newer", "abc-older"]);
            assert_eq!(ids("abc-older"), ["abc-older"]);
            assert!(
                ids("older").is_empty(),
                "a prefix must not match the end of an id"
            );
            assert!(
                ids("1776").is_empty(),
                "the timestamp in a file name is not part of the id",
            );
        }

        #[test]
        fn skips_unreadable_matches() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            seed_test_session(
                &store,
                "corrupt-twin",
                None,
                None,
                datetime!(2026-04-16 12:00:00 UTC),
            );
            write_newest_corrupt_session(dir.path());

            let ids: Vec<_> = store
                .list_matching("corrupt", ProjectScope::Current)
                .unwrap()
                .into_iter()
                .map(|s| s.session_id)
                .collect();
            assert_eq!(ids, ["corrupt-twin"]);
        }
    }
}
