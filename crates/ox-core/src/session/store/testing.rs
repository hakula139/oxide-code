//! Session fixtures and ID-keyed lookups, shared with other crates' tests.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use rusqlite::TransactionBehavior;
use time::OffsetDateTime;

use super::listing::{ArchiveState, ListScope, ProjectScope, SessionInfo};
use super::load::{SessionData, load};
use super::{CONTENT_VERSION, SessionStore, to_millis};
use crate::message::Message;

pub(crate) const TEST_PROJECT: &str = "test-project";

/// The user prompt each [`seed_test_session`] saves.
pub const SEEDED_PROMPT: &str = "seeded prompt";

// ── SessionStore ──

impl SessionStore {
    /// Opens the store in `data_dir` for `project_name`, recording `/work/<project_name>` as both
    /// the launch directory and the project root.
    pub fn open_at(data_dir: PathBuf, project_name: &str) -> Result<Self> {
        let root = PathBuf::from(format!("/work/{project_name}"));
        Self::open_in(data_dir, &root, &root)
    }

    /// A store for `project_name` whose database was never set up, so every read fails.
    pub fn unopened(project_name: &str) -> Self {
        let root = PathBuf::from(format!("/work/{project_name}"));
        Self::at(PathBuf::from("/nonexistent/ox"), &root, &root)
    }

    /// Every resumable unarchived session in the home project, most recently active first.
    pub fn list(&self) -> Result<Vec<SessionInfo>> {
        Ok(self.list_paged(None, ListScope::default())?.into_sessions())
    }

    pub(crate) fn load_session_data(&self, session_id: &str) -> Result<SessionData> {
        load(&mut self.connect()?, session_id)
    }
}

// ── Fixtures ──

/// Holds the store's write lock on another connection until the test releases it, as a competing
/// process would. The deadline fails a blocked runtime or UI test before it can strand the suite.
pub struct WriterBarrier {
    release: mpsc::Sender<()>,
    completed: JoinHandle<()>,
}

impl WriterBarrier {
    pub fn hold(store: &SessionStore) -> Self {
        let store = store.clone();
        let (ready, acquired) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let completed = thread::spawn(move || {
            let mut conn = store.connect().expect("the competing writer connects");
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("the competing writer takes the write lock");
            ready.send(()).expect("the test waits for the writer");
            released
                .recv_timeout(Duration::from_secs(10))
                .expect("the test remains responsive while the writer holds the lock");
            tx.commit().expect("the competing writer commits");
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

/// Session ID with every byte set to `byte`, in the hyphenated UUID form real session IDs use.
pub fn stamped_id(byte: u8) -> String {
    uuid::Uuid::from_bytes([byte; 16]).to_string()
}

/// Session `session_id` as a listing reports it: in the [`test_store`] project, last active at
/// 2026-04-18 09:00 UTC, untitled, unarchived, and empty.
pub fn listed_session(session_id: &str) -> SessionInfo {
    let project = format!("/work/{TEST_PROJECT}");
    SessionInfo {
        session_id: session_id.to_owned(),
        forked_from: None,
        cwd: project.clone(),
        project_root: project,
        last_active_at: time::macros::datetime!(2026-04-18 09:00:00 UTC),
        title: None,
        message_count: 0,
        git_branch: None,
        archive_state: ArchiveState::Active,
    }
}

/// Seeds a session in `store`'s project, created and last active at `created_at`, with `title` and
/// the listed `message_count`. Its one saved message is the user prompt [`SEEDED_PROMPT`], so it
/// resumes like any session.
pub fn seed_test_session(
    store: &SessionStore,
    session_id: &str,
    title: Option<&str>,
    message_count: u32,
    created_at: OffsetDateTime,
) {
    let project = store.project();
    let at = to_millis(created_at);
    let message_id = uuid::Uuid::new_v4().to_string();
    let content = serde_json::to_string(&Message::user(SEEDED_PROMPT).content)
        .expect("the prompt serializes");
    let conn = store.connect().expect("the store connects");
    conn.execute(
        "INSERT INTO sessions (
             id, project_root, cwd, model, format, created_at, updated_at, title, title_manual,
             head_id, message_count
         ) VALUES (?1, ?2, ?3, 'claude-opus-5-5', ?4, ?5, ?5, ?6, 0, ?7, ?8)",
        rusqlite::params![
            session_id,
            project.root,
            project.cwd,
            CONTENT_VERSION,
            at,
            title,
            message_id,
            message_count,
        ],
    )
    .expect("the session is seeded");
    conn.execute(
        "INSERT INTO messages (session_id, id, role, content, created_at)
         VALUES (?1, ?2, 'user', ?3, ?4)",
        rusqlite::params![session_id, message_id, content, at],
    )
    .expect("the seeded prompt is saved");
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

/// The messages that resuming the session `session_id` loads, in their JSON form.
pub fn saved_messages(store: &SessionStore, session_id: &str) -> Vec<serde_json::Value> {
    store
        .load_session_data(session_id)
        .expect("the session loads")
        .messages
        .iter()
        .map(|message| serde_json::to_value(message).expect("a message serializes"))
        .collect()
}

/// IDs of the home project's sessions in the given archive state, most recently active first.
pub fn listed_ids(store: &SessionStore, archive: ArchiveState) -> Vec<String> {
    let scope = ListScope {
        projects: ProjectScope::Current,
        archive,
    };
    store
        .list_paged(None, scope)
        .expect("the home project's sessions can be listed")
        .into_sessions()
        .into_iter()
        .map(|info| info.session_id)
        .collect()
}
