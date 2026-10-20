//! Session fixtures and ID-keyed lookups, shared with other crates' tests.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use time::OffsetDateTime;

use super::listing::ListScope;
use super::{
    SessionData, SessionStore, SessionWriter, encode_lines, file_session_id,
    load_session_data_from_path, open_append_file, set_archived_at,
};
use crate::session::entry::{CURRENT_VERSION, Entry, TitleSource};

pub(crate) const TEST_PROJECT: &str = "test-project";

// ── SessionStore ──

impl SessionStore {
    /// Opens `project_name` under `sessions_dir`, recording `/work/<project_name>` as the cwd.
    pub fn open_at(sessions_dir: PathBuf, project_name: &str) -> Result<Self> {
        fs::create_dir_all(&sessions_dir)?;
        let project_dir = sessions_dir.join(project_name);
        fs::create_dir_all(&project_dir)?;
        Ok(Self {
            sessions_dir,
            project_dir,
            cwd: Path::new("/work").join(project_name),
        })
    }

    pub(crate) fn load_session_data(&self, session_id: &str) -> Result<SessionData> {
        load_session_data_from_path(&self.find_session_path(session_id)?)
    }

    /// Finds a session file by the ID in its name, in any project.
    pub fn find_session_path(&self, session_id: &str) -> Result<PathBuf> {
        for entry in fs::read_dir(&self.sessions_dir)
            .with_context(|| format!("cannot read {}", self.sessions_dir.display()))?
        {
            let dir = entry?.path();
            if dir.is_dir()
                && let Some(path) = find_session_in(&dir, session_id)
            {
                return Ok(path);
            }
        }
        bail!("session not found: {session_id}")
    }
}

pub(super) fn find_session_in(dir: &Path, session_id: &str) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| file_session_id(path) == Some(session_id))
}

// ── SessionWriter ──

impl SessionWriter {
    pub(crate) fn append(&mut self, entries: &[Entry]) -> Result<()> {
        self.append_lines(&encode_lines(entries)?)
    }
}

// ── Fixtures ──

pub fn test_store(dir: &Path) -> SessionStore {
    SessionStore::open_at(dir.to_path_buf(), TEST_PROJECT).expect("the test store opens")
}

pub fn test_project_dir(dir: &Path) -> PathBuf {
    dir.join(TEST_PROJECT)
}

/// Sets the mtime, which orders listings, of the session file at `path` to `at`.
pub(super) fn set_modified(path: &Path, at: OffsetDateTime) {
    open_append_file(path)
        .expect("the session file opens")
        .set_modified(at.into())
        .expect("the mtime is set");
}

/// Seeds a session created and last active at `created_at`, holding a header and, when given, a
/// `UserProvided` title and a summary of `message_count`, so a `None` `message_count` leaves the
/// session unfinished.
pub fn seed_test_session(
    store: &SessionStore,
    session_id: &str,
    title: Option<&str>,
    message_count: Option<u32>,
    created_at: OffsetDateTime,
) {
    let mut writer = store
        .create(
            &Entry::Header {
                session_id: session_id.to_owned(),
                cwd: "/work/proj".to_owned(),
                model: "claude-opus-5-5".to_owned(),
                created_at,
                version: CURRENT_VERSION,
                git_branch: None,
            },
            &[],
        )
        .expect("the session is created");
    if let Some(t) = title {
        writer
            .append(&[Entry::Title {
                title: t.to_owned(),
                source: TitleSource::UserProvided,
                updated_at: created_at,
            }])
            .expect("the title is appended");
    }
    if let Some(count) = message_count {
        writer
            .append(&[Entry::Summary {
                message_count: count,
                updated_at: created_at,
            }])
            .expect("the summary is appended");
    }
    set_modified(writer.path(), created_at);
}

/// Archives the seeded session `session_id`, keeping the last-active time that orders listings.
pub fn archive_test_session(store: &SessionStore, session_id: &str) {
    let path = store
        .find_session_path(session_id)
        .expect("the session is seeded");
    set_archived_at(&path, true).expect("the session archives");
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
pub fn listed_ids(store: &SessionStore, archived: bool) -> Vec<String> {
    let scope = ListScope {
        all: false,
        archived,
    };
    store
        .walk(scope)
        .expect("the project's sessions list")
        .map(|info| info.session_id)
        .collect()
}
