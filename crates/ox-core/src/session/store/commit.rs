//! The write transaction: one [`Commit`] runs under `BEGIN IMMEDIATE`, checks the head it builds
//! on, forks a stale session, and writes its records in order.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OptionalExtension as _, Row, Transaction, TransactionBehavior};
use serde::de::DeserializeOwned;
use uuid::Uuid;

use super::listing::ArchiveState;
use super::{CHAIN_CTE, CONTENT_VERSION, CompactInfo, SessionStore, UsageRecord, now_millis};
use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::prompt::SessionContext;
use crate::tool::ToolMetadata;
use crate::util::fs::FileLock;

// ── Commit ──

/// The input of one write transaction.
pub(crate) struct Commit<'a> {
    pub(crate) session: &'a SessionRow,
    pub(crate) saved: Saved,
    pub(crate) records: &'a [Record],
    /// The head the batch leaves, `None` when it moves no head.
    pub(crate) head: Option<Head>,
}

/// The `sessions` row the writing process holds in memory, which a fresh session or fork inserts.
#[derive(Debug, Clone)]
pub(crate) struct SessionRow {
    pub(crate) id: String,
    pub(crate) forked_from: Option<String>,
    pub(crate) cwd: String,
    pub(crate) project_root: String,
    pub(crate) git_branch: Option<String>,
    pub(crate) model: String,
    /// Unix milliseconds.
    pub(crate) created_at: i64,
    pub(crate) title: Option<String>,
    pub(crate) title_manual: bool,
}

/// Whether the session has a row yet.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Saved {
    /// No row exists, so the commit inserts one with its first message.
    Fresh,
    /// The row exists, and `base` is the head this process last committed or loaded.
    At { base: Option<Uuid> },
}

/// A head move: the new tip and the message count of the branch it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Head {
    pub(crate) id: Option<Uuid>,
    pub(crate) message_count: u32,
}

/// One write of a batch, applied in order.
#[derive(Debug, Clone)]
pub(crate) enum Record {
    Message(Box<MessageRow>),
    Usage(UsageRow),
    Title { title: String, manual: bool },
    Archive,
    FileSnapshots(Vec<FileSnapshot>),
}

/// The `messages` columns [`MessageRow::read`] decodes, in its order.
pub(super) const MESSAGE_COLUMNS: &str =
    "id, parent_id, role, content, context, compact, tool_metadata, created_at";

/// A `messages` row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MessageRow {
    pub(crate) id: Uuid,
    pub(crate) parent_id: Option<Uuid>,
    pub(crate) message: Message,
    /// The prompt state this message is the first to carry.
    pub(crate) context: Option<SessionContext>,
    /// The compaction this message opens as a root.
    pub(crate) compact: Option<CompactInfo>,
    /// Metadata of the tool results this message holds, keyed by tool-use ID.
    pub(crate) tool_metadata: BTreeMap<String, ToolMetadata>,
    /// Unix milliseconds.
    pub(crate) created_at: i64,
}

impl MessageRow {
    /// Decodes a row of [`MESSAGE_COLUMNS`].
    pub(super) fn read(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: parse_id(&row.get::<_, String>(0)?)?,
            parent_id: row
                .get::<_, Option<String>>(1)?
                .as_deref()
                .map(parse_id)
                .transpose()?,
            message: Message {
                role: row
                    .get::<_, String>(2)?
                    .parse()
                    .context("invalid message role")?,
                content: serde_json::from_str(&row.get::<_, String>(3)?)
                    .context("invalid message content")?,
            },
            context: json_column(row, 4).context("invalid prompt context")?,
            compact: json_column(row, 5).context("invalid compaction")?,
            tool_metadata: json_column(row, 6)
                .context("invalid tool metadata")?
                .unwrap_or_default(),
            created_at: row.get(7)?,
        })
    }
}

/// A `usage` row: one billed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsageRow {
    pub(crate) id: Uuid,
    pub(crate) record: UsageRecord,
    /// Unix milliseconds.
    pub(crate) created_at: i64,
}

/// Parses a message ID column.
pub(super) fn parse_id(id: &str) -> Result<Uuid> {
    Uuid::parse_str(id).with_context(|| format!("invalid message ID {id:?}"))
}

/// Decodes the nullable JSON column `idx` of `row`.
fn json_column<T: DeserializeOwned>(row: &Row<'_>, idx: usize) -> Result<Option<T>> {
    let Some(text) = row.get::<_, Option<String>>(idx)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(&text)?))
}

/// Where a committed batch landed.
pub(crate) enum Landed {
    /// The batch landed on the row it found, or wrote nothing.
    InPlace,
    /// The batch inserted the fresh session's row, whose liveness lock the commit took.
    Created(FileLock),
    /// Another process moved the head since this one last saw it, so the batch landed on session
    /// `id`, copied from this process's view with the source's title and manual flag, whose
    /// liveness lock the commit took.
    Forked {
        id: String,
        lock: FileLock,
        title: Option<String>,
        title_manual: bool,
    },
}

/// Runs `commit` as one transaction. A fresh commit must hold a message, which the row it inserts
/// waits for.
pub(crate) fn commit(
    conn: &mut Connection,
    store: &SessionStore,
    commit: &Commit<'_>,
) -> Result<Landed> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_millis();
    let (session_id, landed) = match commit.saved {
        Saved::Fresh => {
            let lock = store.hold(&commit.session.id)?;
            insert_session(&tx, commit.session, now)?;
            (commit.session.id.clone(), Landed::Created(lock))
        }
        Saved::At { base } => {
            if commit.head.is_some() && stored_head(&tx, &commit.session.id)? != base {
                fork(&tx, store, commit.session, base, now)?
            } else {
                (commit.session.id.clone(), Landed::InPlace)
            }
        }
    };

    let mut snapshots = None;
    for record in commit.records {
        match record {
            Record::Message(row) => insert_message(&tx, &session_id, row, Duplicate::Fail)?,
            Record::Usage(row) => insert_usage(&tx, &session_id, row, Duplicate::Fail)?,
            Record::Title { title, manual } => {
                tx.execute(
                    "UPDATE sessions SET title = ?2, title_manual = ?3 WHERE id = ?1",
                    (&session_id, title, manual),
                )?;
            }
            Record::Archive => {
                set_archive_state(&tx, &session_id, ArchiveState::Archived, now)?;
            }
            Record::FileSnapshots(files) => snapshots = Some(files),
        }
    }
    if let Some(head) = commit.head {
        tx.execute(
            "UPDATE sessions SET head_id = ?2, message_count = ?3, updated_at = ?4, format = ?5
             WHERE id = ?1",
            (
                &session_id,
                head.id.map(|id| id.to_string()),
                head.message_count,
                now,
                CONTENT_VERSION,
            ),
        )?;
    }
    if let Some(files) = snapshots {
        let tip = match (commit.head, commit.saved) {
            (Some(head), _) => head.id,
            (None, Saved::At { base }) => base,
            (None, Saved::Fresh) => unreachable!("a fresh commit moves the head"),
        };
        tx.execute(
            "UPDATE sessions SET file_snapshots = ?2 WHERE id = ?1 AND head_id IS ?3",
            (
                &session_id,
                serde_json::to_string(files)?,
                tip.map(|id| id.to_string()),
            ),
        )?;
    }
    tx.commit()?;
    Ok(landed)
}

/// The head of session `session_id`.
fn stored_head(tx: &Transaction<'_>, session_id: &str) -> Result<Option<Uuid>> {
    find_head(tx, session_id)?.with_context(|| format!("session not found: {session_id}"))
}

/// The head of session `session_id`, `None` when no such session exists.
#[expect(
    clippy::option_option,
    reason = "a missing session (None) differs from one whose branch is empty (Some(None))"
)]
pub(super) fn find_head(tx: &Transaction<'_>, session_id: &str) -> Result<Option<Option<Uuid>>> {
    let head: Option<Option<String>> = tx
        .query_row(
            "SELECT head_id FROM sessions WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?;
    head.map(|head| head.as_deref().map(parse_id).transpose())
        .transpose()
}

fn insert_session(tx: &Transaction<'_>, session: &SessionRow, now: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO sessions (
             id, forked_from, project_root, cwd, git_branch, model, format, created_at,
             updated_at, title, title_manual, message_count
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)",
        rusqlite::params![
            session.id,
            session.forked_from,
            session.project_root,
            session.cwd,
            session.git_branch,
            session.model,
            CONTENT_VERSION,
            session.created_at,
            now,
            session.title,
            session.title_manual,
        ],
    )?;
    Ok(())
}

/// Forks `session` to a new session holding the chain from `base` back to its root, with the
/// source's title, and takes the fork's liveness lock. Returns the fork's ID beside it.
fn fork(
    tx: &Transaction<'_>,
    store: &SessionStore,
    session: &SessionRow,
    base: Option<Uuid>,
    now: i64,
) -> Result<(String, Landed)> {
    let id = Uuid::new_v4().to_string();
    let lock = store.hold(&id)?;
    let (title, title_manual) = tx.query_row(
        "SELECT title, title_manual FROM sessions WHERE id = ?1",
        [&session.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let row = SessionRow {
        id: id.clone(),
        forked_from: Some(session.id.clone()),
        created_at: now,
        title,
        title_manual,
        ..session.clone()
    };
    insert_session(tx, &row, now)?;
    if let Some(base) = base {
        tx.execute(
            &format!(
                "{CHAIN_CTE}
                 INSERT INTO messages (session_id, {MESSAGE_COLUMNS})
                 SELECT ?3, {MESSAGE_COLUMNS} FROM messages
                 WHERE seq IN (SELECT seq FROM chain) ORDER BY seq"
            ),
            (&session.id, base.to_string(), &id),
        )?;
    }
    let landed = Landed::Forked {
        id: id.clone(),
        lock,
        title: row.title,
        title_manual: row.title_manual,
    };
    Ok((id, landed))
}

/// Moves session `session_id` into archive `state`, archived at `now`, and returns whether it was
/// in the other state. A session already in `state` keeps its archive time.
pub(super) fn set_archive_state(
    conn: &Connection,
    session_id: &str,
    state: ArchiveState,
    now: i64,
) -> Result<bool> {
    let archive = state == ArchiveState::Archived;
    let changed = conn.execute(
        "UPDATE sessions SET archived_at = ?2 WHERE id = ?1 AND (archived_at IS NULL) = ?3",
        (session_id, archive.then_some(now), archive),
    )?;
    Ok(changed > 0)
}

/// What an insert does with a row whose key the table already holds: a message's session and ID,
/// or a request's ID.
#[derive(Debug, Clone, Copy)]
pub(super) enum Duplicate {
    /// Fails the commit, since a live commit inserts only fresh IDs.
    Fail,
    /// Keeps the stored row, since rows never change and an import repeats rows the store holds.
    Keep,
}

impl Duplicate {
    fn verb(self) -> &'static str {
        match self {
            Self::Fail => "INSERT",
            Self::Keep => "INSERT OR IGNORE",
        }
    }
}

/// Inserts `row` into session `session_id`, handling a held ID as `duplicate` says.
pub(super) fn insert_message(
    tx: &Transaction<'_>,
    session_id: &str,
    row: &MessageRow,
    duplicate: Duplicate,
) -> Result<()> {
    tx.execute(
        &format!(
            "{} INTO messages (session_id, {MESSAGE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            duplicate.verb(),
        ),
        rusqlite::params![
            session_id,
            row.id.to_string(),
            row.parent_id.map(|id| id.to_string()),
            <&str>::from(row.message.role),
            serde_json::to_string(&row.message.content)?,
            row.context
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            row.compact
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            (!row.tool_metadata.is_empty())
                .then(|| serde_json::to_string(&row.tool_metadata))
                .transpose()?,
            row.created_at,
        ],
    )?;
    Ok(())
}

/// Inserts `row` into session `session_id`, handling a held ID as `duplicate` says.
pub(super) fn insert_usage(
    tx: &Transaction<'_>,
    session_id: &str,
    row: &UsageRow,
    duplicate: Duplicate,
) -> Result<()> {
    tx.execute(
        &format!(
            "{} INTO usage (id, session_id, record, created_at) VALUES (?1, ?2, ?3, ?4)",
            duplicate.verb(),
        ),
        (
            row.id.to_string(),
            session_id,
            serde_json::to_string(&row.record)?,
            row.created_at,
        ),
    )?;
    Ok(())
}
