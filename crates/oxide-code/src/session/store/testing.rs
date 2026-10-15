//! Test-only session fixtures and id-keyed lookups. A child of `store`, so it can build
//! [`SessionWriter`] from its private fields.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;

use super::super::entry::{CURRENT_VERSION, Entry, TitleSource};
use super::{
    SessionData, SessionStore, SessionWriter, encode_lines, file_session_id,
    load_session_data_from_path, open_append_at,
};
use crate::message::Message;

pub(crate) const TEST_PROJECT: &str = "test-project";

// ── SessionStore ──

impl SessionStore {
    pub(crate) fn open_at(sessions_dir: PathBuf, project_name: &str) -> Result<Self> {
        fs::create_dir_all(&sessions_dir)?;
        let project_dir = sessions_dir.join(project_name);
        fs::create_dir_all(&project_dir)?;
        Ok(Self {
            sessions_dir,
            project_dir,
        })
    }

    pub(crate) fn open_append(&self, session_id: &str) -> Result<SessionWriter> {
        open_append_at(&self.find_session_path(session_id)?)
    }

    pub(crate) fn load_session_data(&self, session_id: &str) -> Result<SessionData> {
        load_session_data_from_path(&self.find_session_path(session_id)?)
    }

    /// Finds a session file by the id in its name, in any project.
    pub(crate) fn find_session_path(&self, session_id: &str) -> Result<PathBuf> {
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

fn find_session_in(dir: &Path, session_id: &str) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| file_session_id(path) == Some(session_id))
}

// ── SessionWriter ──

impl SessionWriter {
    /// Writer whose first append fails on a read-only handle and whose next one reopens `path`.
    pub(crate) fn failing_once(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            file: Some(File::open(path).unwrap()),
        }
    }

    pub(crate) fn append(&mut self, entries: &[Entry]) -> Result<()> {
        self.append_lines(&encode_lines(entries)?)
    }
}

// ── Fixtures ──

pub(crate) fn test_store(dir: &Path) -> SessionStore {
    SessionStore::open_at(dir.to_path_buf(), TEST_PROJECT).unwrap()
}

pub(crate) fn test_project_dir(dir: &Path) -> PathBuf {
    dir.join(TEST_PROJECT)
}

pub(crate) fn test_session_file(dir: &Path, session_id: &str) -> PathBuf {
    let project_dir = test_project_dir(dir);
    find_session_in(&project_dir, session_id)
        .unwrap_or_else(|| panic!("no session file for id {session_id} in {project_dir:?}"))
}

/// Seed a session for cross-module tests. `last_active_at` derives from `created_at`. Title and
/// summary are written only when their args are `Some`, so `message_count: None` leaves the
/// session unfinished. Title source is hard-coded to `UserProvided`.
pub(crate) fn seed_test_session(
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
        .unwrap();
    if let Some(t) = title {
        writer
            .append(&[Entry::Title {
                title: t.to_owned(),
                source: TitleSource::UserProvided,
                updated_at: created_at,
            }])
            .unwrap();
    }
    if let Some(count) = message_count {
        writer
            .append(&[Entry::Summary {
                message_count: count,
                updated_at: created_at,
            }])
            .unwrap();
    }
}

/// Writes an unparsable session file with id `corrupt`. Call it last: it then has the newest
/// mtime, and its name wins any mtime tie, so it always sorts first.
pub(crate) fn write_newest_corrupt_session(dir: &Path) {
    fs::write(
        test_project_dir(dir).join("9999999999-corrupt.jsonl"),
        "not a session\n",
    )
    .unwrap();
}

pub(crate) fn sample_header(session_id: &str) -> Entry {
    Entry::Header {
        session_id: session_id.to_owned(),
        cwd: "/tmp/project".to_owned(),
        model: "claude-opus-5-5".to_owned(),
        created_at: datetime!(2026-04-16 12:00:00 UTC),
        version: CURRENT_VERSION,
        git_branch: None,
    }
}

pub(crate) fn sample_message_entry(uuid: Uuid, text: &str) -> Entry {
    sample_message_at(uuid, None, datetime!(2026-04-16 12:00:01 UTC), text)
}

pub(crate) fn sample_message_at(
    uuid: Uuid,
    parent_uuid: Option<Uuid>,
    timestamp: OffsetDateTime,
    text: &str,
) -> Entry {
    Entry::Message {
        uuid,
        parent_uuid,
        message: Message::user(text),
        timestamp,
    }
}

pub(crate) fn sample_title_entry(title: &str) -> Entry {
    Entry::Title {
        title: title.to_owned(),
        source: TitleSource::FirstPrompt,
        updated_at: datetime!(2026-04-16 12:00:01 UTC),
    }
}

pub(crate) fn sample_summary_entry(message_count: u32) -> Entry {
    Entry::Summary {
        message_count,
        updated_at: datetime!(2026-04-16 12:05:00 UTC),
    }
}
