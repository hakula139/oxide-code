//! Connection pragmas, the schema, and its revisions under `PRAGMA user_version`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use indoc::indoc;
use rusqlite::{Connection, ErrorCode, TransactionBehavior};

use crate::util::fs::open_private_file;

/// The schema revision this build creates. A revision only adds tables, indexes, and nullable or
/// defaulted columns, so a build keeps working on a store a newer build migrated.
pub(super) const REVISION: i64 = 1;

/// How long a write waits for another connection's write lock before it fails.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Revision 1. Timestamps are Unix milliseconds, and JSON text columns hold the serde forms of the
/// payloads only Rust reads.
const SCHEMA_V1: &str = indoc! {"
    CREATE TABLE sessions (
        id             TEXT PRIMARY KEY,
        forked_from    TEXT,
        project_root   TEXT NOT NULL,
        cwd            TEXT NOT NULL,
        git_branch     TEXT,
        model          TEXT NOT NULL,
        format         INTEGER NOT NULL,
        created_at     INTEGER NOT NULL,
        updated_at     INTEGER NOT NULL,
        title          TEXT,
        title_manual   INTEGER NOT NULL,
        archived_at    INTEGER,
        head_id        TEXT,
        message_count  INTEGER NOT NULL,
        file_snapshots TEXT
    ) STRICT;
    CREATE INDEX sessions_by_project ON sessions (project_root, updated_at DESC);

    CREATE TABLE messages (
        seq           INTEGER PRIMARY KEY,
        session_id    TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
        id            TEXT NOT NULL,
        parent_id     TEXT,
        role          TEXT NOT NULL,
        content       TEXT NOT NULL,
        context       TEXT,
        compact       TEXT,
        tool_metadata TEXT,
        created_at    INTEGER NOT NULL,
        UNIQUE (session_id, id),
        FOREIGN KEY (session_id, parent_id) REFERENCES messages (session_id, id)
    ) STRICT;

    CREATE TABLE usage (
        id         TEXT PRIMARY KEY,
        session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
        record     TEXT NOT NULL,
        created_at INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX usage_by_session ON usage (session_id);
"};

/// Opens the store at `path`, switching a fresh file to WAL and creating its schema. A file SQLite
/// cannot read fails with the steps that recover from it. Two processes that set up a fresh file at
/// once must hold a lock around this call, because SQLite fails the switch at once, without waiting
/// on the busy timeout, when another connection holds the file.
pub(super) fn open(path: &Path) -> Result<Connection> {
    set_up(path).map_err(|e| {
        let Some(damage) = damage(&e) else {
            return e;
        };
        let path = path.display();
        anyhow!(
            "failed to open {path}: {damage}. Quit every `ox` process, run \
             `sqlite3 \"{path}\" .recover | sqlite3 recovered.db`, then move \"{path}\" with its \
             -wal and -shm files aside and move recovered.db to \"{path}\". To start with an \
             empty store instead, skip `.recover` and move the three files aside."
        )
    })
}

fn set_up(path: &Path) -> Result<Connection> {
    // SQLite gives the `-wal` and `-shm` files the main file's mode.
    open_private_file(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut conn = connect(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .with_context(|| format!("failed to enable WAL on {}", path.display()))?;
    migrate(&mut conn).with_context(|| format!("failed to set up {}", path.display()))?;
    Ok(conn)
}

/// The cause in `error` that reports a file that is not a readable SQLite database.
fn damage(error: &anyhow::Error) -> Option<&rusqlite::Error> {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<rusqlite::Error>())
        .find(|cause| {
            matches!(
                cause.sqlite_error_code(),
                Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt)
            )
        })
}

/// Opens a connection to the store at `path` with the per-connection settings.
pub(super) fn connect(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // In WAL mode, NORMAL survives a process crash, and power loss may roll back the last commits
    // without corrupting the file as long as checkpoints reach the disk in order. macOS orders
    // them only with `F_FULLFSYNC`, which `checkpoint_fullfsync` turns on there and others ignore.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "checkpoint_fullfsync", "ON")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// Creates the schema of a fresh store under the write lock, which a store at this revision or a
/// newer one never takes. A store a newer build migrated stays as it is.
fn migrate(conn: &mut Connection) -> Result<()> {
    if revision(conn)? >= REVISION {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if revision(&tx)? == 0 {
        tx.execute_batch(SCHEMA_V1)?;
        tx.pragma_update(None, "user_version", REVISION)?;
    }
    tx.commit()?;
    Ok(())
}

fn revision(conn: &Connection) -> rusqlite::Result<i64> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("sessions.db")
    }

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    mod open {
        use super::*;

        #[test]
        fn creates_the_schema_of_a_fresh_file_in_wal_mode() {
            let dir = tempfile::tempdir().unwrap();

            let conn = open(&store_path(&dir)).unwrap();

            assert_eq!(revision(&conn).unwrap(), REVISION);
            assert_eq!(tables(&conn), ["messages", "sessions", "usage"]);
            let journal: String = conn
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .unwrap();
            assert_eq!(journal, "wal");
        }

        #[test]
        fn opens_a_set_up_store_without_waiting_for_another_connections_write() {
            let dir = tempfile::tempdir().unwrap();
            let path = store_path(&dir);
            open(&path).unwrap();
            let mut writer = connect(&path).unwrap();
            let write = writer
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();

            let conn = open(&path).unwrap();

            assert_eq!(revision(&conn).unwrap(), REVISION);
            write.commit().unwrap();
        }

        #[test]
        fn leaves_a_store_a_newer_build_migrated_as_it_is() {
            let dir = tempfile::tempdir().unwrap();
            let path = store_path(&dir);
            open(&path)
                .unwrap()
                .execute_batch("CREATE TABLE later (id TEXT); PRAGMA user_version = 2;")
                .unwrap();

            let conn = open(&path).unwrap();

            assert_eq!(revision(&conn).unwrap(), 2);
            assert_eq!(tables(&conn), ["later", "messages", "sessions", "usage"]);
        }

        #[test]
        fn names_the_way_out_of_a_damaged_file_only() {
            let dir = tempfile::tempdir().unwrap();
            let damaged = store_path(&dir);
            std::fs::write(&damaged, [b'x'; 4096]).unwrap();

            let damaged_error = format!("{:#}", open(&damaged).unwrap_err());
            let other_error = format!("{:#}", open(dir.path()).unwrap_err());

            let path = damaged.display();
            assert_eq!(
                damaged_error,
                format!(
                    "failed to open {path}: file is not a database. Quit every `ox` process, run \
                     `sqlite3 \"{path}\" .recover | sqlite3 recovered.db`, then move \"{path}\" \
                     with its -wal and -shm files aside and move recovered.db to \"{path}\". To \
                     start with an empty store instead, skip `.recover` and move the three files \
                     aside."
                ),
            );
            assert!(!other_error.contains(".recover"), "{other_error}");
        }

        #[cfg(unix)]
        #[test]
        fn keeps_the_database_and_its_wal_files_private() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();

            let conn = open(&store_path(&dir)).unwrap();
            conn.execute_batch("CREATE TABLE scratch (id TEXT); INSERT INTO scratch VALUES ('x');")
                .unwrap();

            for name in ["sessions.db", "sessions.db-wal", "sessions.db-shm"] {
                let mode = std::fs::metadata(dir.path().join(name))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{name}");
            }
        }
    }

    mod damage {
        use super::*;

        #[test]
        fn matches_only_the_errors_of_an_unreadable_file() {
            let cases = [
                (rusqlite::ffi::SQLITE_NOTADB, true),
                (rusqlite::ffi::SQLITE_CORRUPT, true),
                (rusqlite::ffi::SQLITE_BUSY, false),
                (rusqlite::ffi::SQLITE_READONLY, false),
                (rusqlite::ffi::SQLITE_FULL, false),
            ];

            for (code, damaged) in cases {
                let error = anyhow::Error::from(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(code),
                    None,
                ))
                .context("failed to set up the store");

                let found = damage(&error);

                assert_eq!(found.is_some(), damaged, "SQLite code {code}");
            }
        }
    }

    mod connect {
        use super::*;

        #[test]
        fn sets_the_per_connection_pragmas() {
            let dir = tempfile::tempdir().unwrap();
            let path = store_path(&dir);
            open(&path).unwrap();

            let conn = connect(&path).unwrap();

            let pragma = |name| -> i64 {
                conn.pragma_query_value(None, name, |row| row.get(0))
                    .unwrap()
            };
            assert_eq!(pragma("busy_timeout"), 5000);
            assert_eq!(pragma("synchronous"), 1, "NORMAL");
            assert_eq!(pragma("checkpoint_fullfsync"), 1);
            assert_eq!(pragma("foreign_keys"), 1);
            let journal: String = conn
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .unwrap();
            assert_eq!(journal, "wal", "the file keeps WAL for every connection");
        }
    }
}
