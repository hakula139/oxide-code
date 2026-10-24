//! Session fixtures and ID-keyed lookups that only this crate's tests use.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::Result;
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;

use super::testing::{TEST_PROJECT, find_session_in, set_modified, test_project_dir};
use super::{SessionStore, SessionWriter, open_append_at};
use crate::message::Message;
use crate::session::entry::{CURRENT_VERSION, Entry, Header};

// ── SessionStore ──

impl SessionStore {
    pub(crate) fn open_append(&self, session_id: &str) -> Result<SessionWriter> {
        open_append_at(&self.find_session_path(session_id)?)
    }
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
}

// ── Fixtures ──

pub(crate) fn test_session_file(dir: &Path, session_id: &str) -> PathBuf {
    let project_dir = test_project_dir(dir);
    find_session_in(&project_dir, session_id)
        .unwrap_or_else(|| panic!("no session file for id {session_id} in {project_dir:?}"))
}

/// Every entry of the session `session_id` in the test project under `dir`, in file order.
pub(crate) fn saved_entries(dir: &Path, session_id: &str) -> Vec<Entry> {
    fs::read_to_string(test_session_file(dir, session_id))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Text of each title entry, in file order.
pub(crate) fn saved_titles(entries: &[Entry]) -> Vec<&str> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Title { title, .. } => Some(title.as_str()),
            _ => None,
        })
        .collect()
}

/// Path of each file snapshot entry, in file order.
pub(crate) fn saved_snapshot_paths(entries: &[Entry]) -> Vec<&Path> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::FileSnapshot { snapshot } => Some(snapshot.path.as_path()),
            _ => None,
        })
        .collect()
}

/// Writes an unparsable session file with ID `corrupt`, whose name and mtime both carry a time past
/// every seeded session, so it sorts first.
pub(crate) fn write_newest_corrupt_session(dir: &Path) {
    const NEWEST: i64 = 9_999_999_999;
    let path = test_project_dir(dir).join(format!("{NEWEST}-corrupt.jsonl"));
    fs::write(&path, "not a session\n").unwrap();
    set_modified(&path, OffsetDateTime::from_unix_timestamp(NEWEST).unwrap());
}

/// A header for session `session_id` in the [`test_store`](super::testing::test_store) project.
pub(crate) fn sample_header(session_id: &str) -> Header {
    let project = format!("/work/{TEST_PROJECT}");
    Header {
        session_id: session_id.to_owned(),
        cwd: project.clone(),
        project_root: Some(project),
        model: "claude-opus-5-5".to_owned(),
        created_at: datetime!(2026-04-16 12:00:00 UTC),
        version: CURRENT_VERSION,
        git_branch: None,
    }
}

pub(super) fn sample_message_entry(uuid: Uuid, text: &str) -> Entry {
    sample_message_at(uuid, None, datetime!(2026-04-16 12:00:01 UTC), text)
}

pub(super) fn sample_message_at(
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

pub(super) fn sample_title_entry(title: &str) -> Entry {
    Entry::Title {
        title: title.to_owned(),
        updated_at: datetime!(2026-04-16 12:00:01 UTC),
    }
}

pub(super) fn sample_summary_entry(message_count: u32) -> Entry {
    Entry::Summary {
        message_count,
        updated_at: datetime!(2026-04-16 12:05:00 UTC),
    }
}
