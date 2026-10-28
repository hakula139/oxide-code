//! The session store: one SQLite database in the data directory, the liveness locks of the sessions
//! processes hold open, and archive and delete by ID.

pub(crate) mod commit;
mod export;
pub mod listing;
pub(crate) mod load;
#[cfg(test)]
pub(crate) mod local_testing;
mod schema;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::debug;

use self::listing::ArchiveState;
use crate::model::{PromptCacheTtl, TokenUsage};
use crate::tool::tasks::Task;
use crate::util::fs::{FileLock, create_private_dir_all, open_private_file};
use crate::util::path::{XdgBase, app_dir};

const DB_FILE: &str = "sessions.db";
const ARTIFACTS_DIR: &str = "artifacts";
const LOCKS_DIR: &str = "locks";
/// The lock every process holds around [`schema::open`]. Its dot keeps it apart from the liveness
/// locks, since no session ID holds one.
const SETUP_LOCK_FILE: &str = "sessions.db.lock";

/// The content version this build writes with each message commit. It bumps when a closed serde
/// enum in message content, such as a content block type, gains a value, so an older build skips
/// or refuses a session it cannot read.
pub(crate) const CONTENT_VERSION: u32 = 1;

/// The `chain(seq)` table of the messages from message `?2` of session `?1` back through parents to
/// its root, which a statement appends to.
const CHAIN_CTE: &str = "
    WITH RECURSIVE chain(seq, parent_id) AS (
        SELECT seq, parent_id FROM messages WHERE session_id = ?1 AND id = ?2
        UNION ALL
        SELECT m.seq, m.parent_id FROM messages m JOIN chain c
            ON m.session_id = ?1 AND m.id = c.parent_id
    )";

/// Fails for a session of content version `format` that a newer build wrote, which this build
/// cannot `verb`.
fn check_format(format: u32, verb: &str) -> Result<()> {
    if format > CONTENT_VERSION {
        bail!(
            "session format version {format} is newer than supported ({CONTENT_VERSION}). \
             Upgrade oxide-code to {verb} it."
        );
    }
    Ok(())
}

// ── SessionStore ──

/// The store of every saved session, opened for one project: the Git repository root of the launch
/// directory, or the launch directory itself outside a repository.
#[derive(Clone)]
pub struct SessionStore {
    data_dir: PathBuf,
    project: Project,
}

/// A launch directory and its project root, as session rows record them.
#[derive(Clone)]
pub(crate) struct Project {
    pub(crate) cwd: String,
    pub(crate) root: String,
    root_path: PathBuf,
}

impl SessionStore {
    /// Opens the store for the project at `project_root`, launched in `cwd`, creating the data
    /// directory and the database when missing. New sessions record both directories.
    pub fn open(cwd: &Path, project_root: &Path) -> Result<Self> {
        let data_dir =
            app_dir(XdgBase::Data).context("failed to determine the session storage directory")?;
        Self::open_in(data_dir, cwd, project_root)
    }

    fn open_in(data_dir: PathBuf, cwd: &Path, project_root: &Path) -> Result<Self> {
        create_private_dir_all(&data_dir)?;
        let store = Self::at(data_dir, cwd, project_root);
        let setup_path = store.data_dir.join(LOCKS_DIR).join(SETUP_LOCK_FILE);
        let setup = FileLock::lock(open_lock_file(&setup_path)?)
            .with_context(|| format!("failed to lock {}", setup_path.display()))?;
        schema::open(&store.data_dir.join(DB_FILE))?;
        drop(setup);
        debug!(path = %store.data_dir.display(), "opened session store");
        Ok(store)
    }

    fn at(data_dir: PathBuf, cwd: &Path, project_root: &Path) -> Self {
        Self {
            data_dir,
            project: Project {
                cwd: cwd.display().to_string(),
                root: project_root.display().to_string(),
                root_path: project_root.to_path_buf(),
            },
        }
    }

    pub(crate) fn project_root(&self) -> &Path {
        &self.project.root_path
    }

    pub(crate) fn project(&self) -> &Project {
        &self.project
    }

    /// A new connection to the database, which the caller owns for as long as it needs.
    pub(crate) fn connect(&self) -> Result<Connection> {
        schema::connect(&self.data_dir.join(DB_FILE))
    }

    /// The directory that holds the artifacts of session `session_id`, such as spilled tool output.
    /// It exists only once the session writes an artifact, and [`Self::delete`] removes it.
    pub(crate) fn artifact_dir(&self, session_id: &str) -> PathBuf {
        self.data_dir.join(ARTIFACTS_DIR).join(session_id)
    }

    /// Takes the shared liveness lock of session `session_id`, which a process holds while it has
    /// the session open for writing, so [`Self::delete`] refuses it. The kernel releases the lock
    /// when the process dies.
    pub(crate) fn hold(&self, session_id: &str) -> Result<FileLock> {
        let path = self.lock_path(session_id)?;
        FileLock::shared(open_lock_file(&path)?)
            .with_context(|| format!("failed to lock {}", path.display()))
    }

    /// Sets the archive state of session `session_id`, keeping its last-active time.
    pub(crate) fn set_archived(&self, session_id: &str, state: ArchiveState) -> Result<()> {
        let archived_at = (state == ArchiveState::Archived).then(now_millis);
        let updated = self.connect()?.execute(
            "UPDATE sessions SET archived_at = ?2 WHERE id = ?1",
            (session_id, archived_at),
        )?;
        if updated == 0 {
            bail!("session not found: {session_id}");
        }
        Ok(())
    }

    /// Deletes session `session_id` with its rows, artifact directory, and lock file. Refuses a
    /// session another process holds, so a delete never pulls the rows from under a live writer.
    pub(crate) fn delete(&self, session_id: &str) -> Result<()> {
        let lock_path = self.lock_path(session_id)?;
        let lock = FileLock::try_exclusive(open_lock_file(&lock_path)?)
            .with_context(|| format!("failed to lock {}", lock_path.display()))?;
        let Some(lock) = lock else {
            bail!("it is open in another ox process. Quit that process first.");
        };
        let deleted = self
            .connect()?
            .execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;
        let artifact_dir = self.artifact_dir(session_id);
        ignore_missing(fs::remove_dir_all(&artifact_dir))
            .with_context(|| format!("failed to remove {}", artifact_dir.display()))?;
        drop(lock);
        ignore_missing(fs::remove_file(&lock_path))
            .with_context(|| format!("failed to remove {}", lock_path.display()))?;
        if deleted == 0 {
            bail!("session not found: {session_id}");
        }
        Ok(())
    }

    /// The liveness lock file of session `session_id`, whose ID names a file.
    fn lock_path(&self, session_id: &str) -> Result<PathBuf> {
        validate_session_id(session_id)?;
        Ok(self
            .data_dir
            .join(LOCKS_DIR)
            .join(format!("{session_id}.lock")))
    }
}

/// Treats a removal that found nothing to remove as done.
fn ignore_missing(removal: io::Result<()>) -> io::Result<()> {
    match removal {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn open_lock_file(path: &Path) -> Result<fs::File> {
    let dir = path
        .parent()
        .context("a lock file has a parent directory")?;
    create_private_dir_all(dir)?;
    open_private_file(path).with_context(|| format!("failed to open {}", path.display()))
}

const MAX_SESSION_ID_LEN: usize = 64;

/// Rejects an ID that could not name a file, since lock files and artifact directories carry it.
pub(crate) fn validate_session_id(session_id: &str) -> Result<()> {
    let ok = !session_id.is_empty()
        && session_id.len() <= MAX_SESSION_ID_LEN
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        bail!("invalid session ID {session_id:?}");
    }
    Ok(())
}

// ── Timestamps ──

/// The current time in Unix milliseconds, the unit every timestamp column holds.
pub(crate) fn now_millis() -> i64 {
    to_millis(OffsetDateTime::now_utc())
}

pub(crate) fn to_millis(at: OffsetDateTime) -> i64 {
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).expect("a session time fits i64 millis")
}

pub(crate) fn from_millis(millis: i64) -> Result<OffsetDateTime, time::error::ComponentRange> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
}

/// Reads the timestamp column `idx` of `row`.
pub(crate) fn time_column(row: &Row<'_>, idx: usize) -> rusqlite::Result<OffsetDateTime> {
    from_millis(row.get(idx)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Integer, e.into())
    })
}

// ── Payloads ──

/// A compaction's summary text, its `/compact` focus, and the task list the summarized transcript
/// left, which the summary message carries over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompactRecord {
    pub(crate) summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tasks: Vec<Task>,
}

/// The compaction record a compaction root stores, with the message count it replaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompactInfo {
    #[serde(flatten)]
    pub(crate) record: CompactRecord,
    pub(crate) pre_message_count: u32,
}

/// One billed request's token usage, with the model and prompt-cache TTL that price it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UsageRecord {
    pub(crate) model: String,
    pub(crate) cache_ttl: PromptCacheTtl,
    #[serde(flatten)]
    pub(crate) tokens: TokenUsage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::testing::isolated_store;

    mod open_in {
        use std::sync::Barrier;
        use std::thread;

        use super::*;

        #[test]
        fn sets_up_a_fresh_store_once_when_two_first_runs_race() {
            for race in 0..100 {
                let dir = tempfile::tempdir().unwrap();
                let start = &Barrier::new(2);
                let data_dir = dir.path();

                let opened: Vec<Result<SessionStore>> = thread::scope(|scope| {
                    let runs = ["first", "second"].map(|project| {
                        scope.spawn(move || {
                            start.wait();
                            SessionStore::open_at(data_dir.to_path_buf(), project)
                        })
                    });
                    runs.into_iter().map(|run| run.join().unwrap()).collect()
                });

                for store in opened {
                    let store = store.unwrap_or_else(|e| panic!("race {race}: {e:#}"));
                    let revision: i64 = store
                        .connect()
                        .unwrap()
                        .pragma_query_value(None, "user_version", |row| row.get(0))
                        .unwrap();
                    assert_eq!(revision, schema::REVISION, "race {race}");
                }
            }
        }
    }

    mod set_archived {
        use super::*;

        #[test]
        fn names_a_missing_session() {
            let (_dir, store) = isolated_store();

            let err = store
                .set_archived("missing", ArchiveState::Archived)
                .unwrap_err();

            assert_eq!(err.to_string(), "session not found: missing");
        }
    }

    mod ignore_missing {
        use super::*;

        #[test]
        fn passes_every_failure_but_a_missing_target() {
            for (name, removal, expected) in [
                ("removed", Ok(()), None),
                ("missing", Err(io::ErrorKind::NotFound.into()), None),
                (
                    "denied",
                    Err(io::ErrorKind::PermissionDenied.into()),
                    Some(io::ErrorKind::PermissionDenied),
                ),
            ] {
                let result = ignore_missing(removal);

                assert_eq!(result.err().map(|e| e.kind()), expected, "{name}");
            }
        }
    }

    mod now_millis {
        use super::*;

        #[test]
        fn reads_the_current_time_in_milliseconds() {
            let before = OffsetDateTime::now_utc();

            let now = now_millis();

            let after = OffsetDateTime::now_utc();
            assert!(to_millis(before) <= now && now <= to_millis(after), "{now}");
        }
    }
}
