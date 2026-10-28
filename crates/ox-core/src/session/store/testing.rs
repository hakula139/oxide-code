//! Session fixtures and ID-keyed lookups, shared with other crates' tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use time::OffsetDateTime;

use super::listing::{ListScope, ProjectScope};
use super::{
    Project, SessionData, SessionStore, SessionWriter, encode_lines, load_session_data_from_path,
    lock_writer, open_append_file,
};
use crate::session::entry::{
    ArchiveState, CURRENT_VERSION, Entry, ExitInfo, Header, SessionInfo, TitleInfo,
};

pub(crate) const TEST_PROJECT: &str = "test-project";

// ── SessionStore ──

impl SessionStore {
    /// Opens `project_name` under `sessions_dir`, recording `/work/<project_name>` as both the
    /// launch directory and the project root.
    pub fn open_at(sessions_dir: PathBuf, project_name: &str) -> Result<Self> {
        fs::create_dir_all(&sessions_dir)?;
        let project_dir = sessions_dir.join(project_name);
        fs::create_dir_all(&project_dir)?;
        let root = format!("/work/{project_name}");
        Ok(Self {
            sessions_dir,
            project_dir,
            cwd_dir: None,
            project_root: PathBuf::from(&root),
            project: Project {
                cwd: root.clone(),
                root,
            },
        })
    }

    /// Every readable unarchived session in the home project, most recently active first.
    pub fn list(&self) -> Result<Vec<SessionInfo>> {
        Ok(self.walk(ListScope::default())?.collect())
    }

    /// The file of session `session_id`, which must exist.
    pub fn saved_file(&self, session_id: &str) -> PathBuf {
        self.find_session_path(session_id)
            .expect("the session has a file")
    }

    pub(crate) fn load_session_data(&self, session_id: &str) -> Result<SessionData> {
        load_session_data_from_path(&self.find_session_path(session_id)?)
    }
}

// ── SessionWriter ──

impl SessionWriter {
    pub(super) fn append(&mut self, entries: &[Entry]) -> Result<()> {
        self.append_lines(&encode_lines(entries)?)
    }
}

// ── Fixtures ──

/// Holds a competing writer's lock until the test releases it. The deadline fails a blocked
/// runtime or UI test before it can strand the suite.
pub struct WriterBarrier {
    release: mpsc::Sender<()>,
    completed: JoinHandle<()>,
}

impl WriterBarrier {
    pub fn hold(path: &Path) -> Self {
        let path = path.to_owned();
        let (ready, acquired) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let completed = thread::spawn(move || {
            let _lock = lock_writer(&path).expect("the competing writer acquires the lock");
            ready.send(()).expect("the test waits for the writer");
            released
                .recv_timeout(Duration::from_secs(10))
                .expect("the test remains responsive while the writer holds the lock");
        });
        acquired.recv().expect("the competing writer starts");
        Self { release, completed }
    }

    pub fn release(self) {
        self.release
            .send(())
            .expect("the competing writer is still waiting");
        self.completed
            .join()
            .expect("the competing writer finishes");
    }
}

pub fn test_store(dir: &Path) -> SessionStore {
    SessionStore::open_at(dir.to_path_buf(), TEST_PROJECT).expect("the test store opens")
}

/// A [`test_store`] in a fresh temp dir, which lives as long as the returned guard.
pub fn isolated_store() -> (tempfile::TempDir, SessionStore) {
    let dir = tempfile::tempdir().expect("a temp dir opens");
    let store = test_store(dir.path());
    (dir, store)
}

pub fn test_project_dir(dir: &Path) -> PathBuf {
    dir.join(TEST_PROJECT)
}

/// Session ID with every byte set to `byte`, in the hyphenated UUID form real session IDs use.
pub fn stamped_id(byte: u8) -> String {
    uuid::Uuid::from_bytes([byte; 16]).to_string()
}

/// Runs `f` with `XDG_DATA_HOME` set to a fresh temp dir, so [`SessionStore::open`] lands there,
/// and passes `f` the dir so it can add stores beneath it.
pub fn with_isolated_xdg<R>(f: impl FnOnce(&Path) -> R) -> R {
    let dir = tempfile::tempdir().expect("a temp dir is created");
    temp_env::with_var("XDG_DATA_HOME", Some(dir.path().as_os_str()), || {
        f(dir.path())
    })
}

/// Session `session_id` as a listing reports it: in the [`test_store`] project, last active at
/// 2026-04-18 09:00 UTC, untitled, unarchived, and never finalized.
pub fn listed_session(session_id: &str) -> SessionInfo {
    SessionInfo {
        session_id: session_id.to_owned(),
        cwd: format!("/work/{TEST_PROJECT}"),
        project_root: None,
        last_active_at: time::macros::datetime!(2026-04-18 09:00:00 UTC),
        title: None,
        exit: None,
        git_branch: None,
        archive_state: ArchiveState::Active,
    }
}

/// Sets the mtime, which orders listings, of the session file at `path` to `at`.
pub(super) fn set_modified(path: &Path, at: OffsetDateTime) {
    open_append_file(path)
        .expect("the session file opens")
        .set_modified(at.into())
        .expect("the mtime is set");
}

/// Seeds a session created and last active at `created_at`, holding a header and, when given, a
/// title and a summary of `message_count`, so a `None` `message_count` leaves the session
/// unfinished.
pub fn seed_test_session(
    store: &SessionStore,
    session_id: &str,
    title: Option<&str>,
    message_count: Option<u32>,
    created_at: OffsetDateTime,
) {
    let mut writer = store
        .create(&test_header(store, session_id, created_at), &[])
        .expect("the session is created");
    if let Some(t) = title {
        writer
            .append(&[Entry::Title(TitleInfo {
                title: t.to_owned(),
                updated_at: created_at,
            })])
            .expect("the title is appended");
    }
    if let Some(count) = message_count {
        writer
            .append(&[Entry::Summary(ExitInfo {
                message_count: count,
                updated_at: created_at,
            })])
            .expect("the summary is appended");
    }
    set_modified(writer.path(), created_at);
}

/// The header `store` writes for session `session_id` created at `created_at`.
pub(super) fn test_header(
    store: &SessionStore,
    session_id: &str,
    created_at: OffsetDateTime,
) -> Header {
    Header {
        session_id: session_id.to_owned(),
        cwd: store.project.cwd.clone(),
        project_root: Some(store.project.root.clone()),
        model: "claude-opus-5-5".to_owned(),
        created_at,
        version: CURRENT_VERSION,
        git_branch: None,
    }
}

/// Archives the seeded session `session_id`, keeping the last-active time that orders listings.
pub fn archive_test_session(store: &SessionStore, session_id: &str) {
    store
        .set_archived(session_id, ArchiveState::Archived)
        .expect("the seeded session archives");
}

/// Number of messages that resuming the session `session_id` loads.
pub fn saved_message_count(store: &SessionStore, session_id: &str) -> usize {
    store
        .load_session_data(session_id)
        .expect("the session loads")
        .messages
        .len()
}

/// IDs of the home project's sessions in the given archive state, most recently active first.
pub fn listed_ids(store: &SessionStore, archive: ArchiveState) -> Vec<String> {
    let scope = ListScope {
        projects: ProjectScope::Current,
        archive,
    };
    store
        .walk(scope)
        .expect("the home project's sessions can be listed")
        .map(|info| info.session_id)
        .collect()
}
