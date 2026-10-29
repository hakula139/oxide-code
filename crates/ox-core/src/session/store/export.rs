//! The JSONL interchange form of a session: one record per line, the session first and every parent
//! before its children. Import decides the head by which messages each side holds.

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Write};

use anyhow::{Context as _, Result, bail};
use rusqlite::{Row, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::commit::{
    MESSAGE_COLUMNS, MessageRow, UsageRow, find_head, insert_message, insert_usage, parse_id,
};
use super::{
    CompactInfo, Project, SessionStore, UsageRecord, check_format, from_millis, time_column,
    to_millis, validate_session_id,
};
use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::prompt::SessionContext;
use crate::tool::ToolMetadata;

// ── Records ──

/// One line of an export.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line {
    Session(SessionLine),
    Message(MessageLine),
    Usage(UsageLine),
}

/// The `sessions` row.
#[derive(Debug, Serialize, Deserialize)]
struct SessionLine {
    id: String,
    forked_from: Option<String>,
    project_root: String,
    cwd: String,
    git_branch: Option<String>,
    model: String,
    format: u32,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    title: Option<String>,
    title_manual: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    archived_at: Option<OffsetDateTime>,
    head_id: Option<Uuid>,
    message_count: u32,
    file_snapshots: Option<Vec<FileSnapshot>>,
}

/// A `messages` row.
#[derive(Debug, Serialize, Deserialize)]
struct MessageLine {
    id: Uuid,
    parent_id: Option<Uuid>,
    #[serde(flatten)]
    message: Message,
    context: Option<SessionContext>,
    compact: Option<CompactInfo>,
    #[serde(default)]
    tool_metadata: BTreeMap<String, ToolMetadata>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

/// A `usage` row.
#[derive(Debug, Serialize, Deserialize)]
struct UsageLine {
    id: Uuid,
    record: UsageRecord,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

impl TryFrom<MessageRow> for MessageLine {
    type Error = time::error::ComponentRange;

    fn try_from(row: MessageRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            parent_id: row.parent_id,
            message: row.message,
            context: row.context,
            compact: row.compact,
            tool_metadata: row.tool_metadata,
            created_at: from_millis(row.created_at)?,
        })
    }
}

impl From<MessageLine> for MessageRow {
    fn from(line: MessageLine) -> Self {
        Self {
            id: line.id,
            parent_id: line.parent_id,
            message: line.message,
            context: line.context,
            compact: line.compact,
            tool_metadata: line.tool_metadata,
            created_at: to_millis(line.created_at),
        }
    }
}

impl From<UsageLine> for UsageRow {
    fn from(line: UsageLine) -> Self {
        Self {
            id: line.id,
            record: line.record,
            created_at: to_millis(line.created_at),
        }
    }
}

// ── Export ──

impl SessionStore {
    /// Writes session `session_id` to `out` as JSONL, with every branch and every message before
    /// its compactions.
    pub(crate) fn export(&self, session_id: &str, out: &mut dyn Write) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "SELECT id, forked_from, project_root, cwd, git_branch, model, format, created_at,
                 updated_at, title, title_manual, archived_at, head_id, message_count,
                 file_snapshots
             FROM sessions WHERE id = ?1",
        )?;
        let mut rows = stmt.query([session_id])?;
        let Some(row) = rows.next()? else {
            bail!("session not found: {session_id}");
        };
        write_line(out, &Line::Session(read_session_line(row)?))?;

        let mut stmt = tx.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 ORDER BY seq"
        ))?;
        let mut rows = stmt.query([session_id])?;
        while let Some(row) = rows.next()? {
            write_line(out, &Line::Message(MessageRow::read(row)?.try_into()?))?;
        }

        let mut stmt = tx.prepare(
            "SELECT id, record, created_at FROM usage WHERE session_id = ?1 ORDER BY rowid",
        )?;
        let mut rows = stmt.query([session_id])?;
        while let Some(row) = rows.next()? {
            let line = UsageLine {
                id: parse_id(&row.get::<_, String>(0)?)?,
                record: serde_json::from_str(&row.get::<_, String>(1)?)
                    .context("invalid usage record")?,
                created_at: time_column(row, 2)?,
            };
            write_line(out, &Line::Usage(line))?;
        }
        out.flush()?;
        Ok(())
    }

    /// Imports the export `input` holds and returns the session that holds its head: the exported
    /// session itself, or a fork of it when the local session diverged. A session the import
    /// creates belongs to this store's project.
    pub(crate) fn import(&self, input: impl BufRead) -> Result<String> {
        let export = Export::parse(input)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session_id = export.apply(&tx, &self.project)?;
        tx.commit()?;
        Ok(session_id)
    }
}

/// Decodes the `sessions` columns [`SessionStore::export`] selects.
fn read_session_line(row: &Row<'_>) -> Result<SessionLine> {
    let archived_at: Option<i64> = row.get(11)?;
    let head_id: Option<String> = row.get(12)?;
    let file_snapshots: Option<String> = row.get(14)?;
    Ok(SessionLine {
        id: row.get(0)?,
        forked_from: row.get(1)?,
        project_root: row.get(2)?,
        cwd: row.get(3)?,
        git_branch: row.get(4)?,
        model: row.get(5)?,
        format: row.get(6)?,
        created_at: time_column(row, 7)?,
        updated_at: time_column(row, 8)?,
        title: row.get(9)?,
        title_manual: row.get(10)?,
        archived_at: archived_at.map(from_millis).transpose()?,
        head_id: head_id.as_deref().map(parse_id).transpose()?,
        message_count: row.get(13)?,
        file_snapshots: file_snapshots
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .context("invalid file snapshots")?,
    })
}

fn write_line(out: &mut dyn Write, line: &Line) -> Result<()> {
    serde_json::to_writer(&mut *out, line)?;
    out.write_all(b"\n")?;
    Ok(())
}

// ── Import ──

/// A parsed and validated export.
struct Export {
    session: SessionLine,
    messages: Vec<MessageRow>,
    usage: Vec<UsageRow>,
}

impl Export {
    /// Parses `input`, checking that a session record of a supported format comes first, each
    /// parent precedes its children, and the head names an exported message. Errors name the line.
    fn parse(input: impl BufRead) -> Result<Self> {
        let mut lines = input.lines().enumerate().map(|(i, line)| (i + 1, line));
        let Some((_, first)) = lines.next() else {
            bail!("not an ox session export: the input is empty");
        };
        let Ok(Line::Session(session)) = serde_json::from_str(&first?) else {
            bail!("line 1: not an ox session export, which starts with a session record");
        };
        validate_session_id(&session.id).context("line 1")?;
        if let Some(source) = &session.forked_from {
            validate_session_id(source).context("line 1: forked_from")?;
        }
        check_format(session.format, "import").context("line 1")?;

        let mut messages = Vec::new();
        let mut usage = Vec::new();
        let mut seen = HashSet::new();
        for (number, line) in lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line).with_context(|| format!("line {number}"))? {
                Line::Session(_) => bail!("line {number}: a second session record"),
                Line::Message(message) => {
                    if let Some(parent) = message.parent_id
                        && !seen.contains(&parent)
                    {
                        bail!(
                            "line {number}: message {} precedes its parent {parent}",
                            message.id,
                        );
                    }
                    seen.insert(message.id);
                    messages.push(message.into());
                }
                Line::Usage(line) => usage.push(line.into()),
            }
        }
        if let Some(head) = session.head_id
            && !seen.contains(&head)
        {
            bail!("the session's head {head} is not among its messages");
        }
        Ok(Self {
            session,
            messages,
            usage,
        })
    }

    /// Writes the export under the session receiving its head, which [`SessionStore::import`]
    /// returns. A session the import creates records `project`, as a fresh session does.
    fn apply(self, tx: &Transaction<'_>, project: &Project) -> Result<String> {
        let id = &self.session.id;
        let target = match find_head(tx, id)? {
            None => {
                insert_session(tx, &self.session, id, None, project)?;
                id.clone()
            }
            Some(head) => match self.landing(tx, id, head)? {
                Some(landing) => self.land(tx, id, landing)?,
                None => self.land_diverged(tx, project)?,
            },
        };
        for message in &self.messages {
            insert_message(tx, &target, message)?;
        }
        for usage in &self.usage {
            insert_usage(tx, &target, usage)?;
        }
        Ok(target)
    }

    /// How the export lands on local session `session_id` at `head`, or `None` when they diverged.
    /// An export holds every message its session ever had, so it saw the local head exactly when
    /// that head is among its messages, even across compactions and rewinds.
    fn landing(
        &self,
        tx: &Transaction<'_>,
        session_id: &str,
        head: Option<Uuid>,
    ) -> Result<Option<Landing>> {
        if head.is_none_or(|head| self.messages.iter().any(|message| message.id == head)) {
            return Ok(Some(Landing::FastForward));
        }
        let Some(exported) = self.session.head_id else {
            return Ok(Some(Landing::Keep));
        };
        let holds_exported: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE session_id = ?1 AND id = ?2)",
            (session_id, exported.to_string()),
            |row| row.get(0),
        )?;
        Ok(holds_exported.then_some(Landing::Keep))
    }

    fn land(&self, tx: &Transaction<'_>, session_id: &str, landing: Landing) -> Result<String> {
        if landing == Landing::FastForward {
            self.fast_forward(tx, session_id)?;
        }
        Ok(session_id.to_owned())
    }

    /// Lands the export on the newest fork of its session that it relates to, so importing newer
    /// exports of a diverged session keeps extending one fork, or on a new fork.
    fn land_diverged(&self, tx: &Transaction<'_>, project: &Project) -> Result<String> {
        let mut stmt = tx.prepare(
            "SELECT id, head_id FROM sessions WHERE forked_from = ?1 ORDER BY updated_at DESC",
        )?;
        let forks = stmt
            .query_map([&self.session.id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (fork, head) in forks {
            let head = head.as_deref().map(parse_id).transpose()?;
            if let Some(landing) = self.landing(tx, &fork, head)? {
                return self.land(tx, &fork, landing);
            }
        }
        let fork = Uuid::new_v4().to_string();
        insert_session(tx, &self.session, &fork, Some(&self.session.id), project)?;
        Ok(fork)
    }

    /// Moves local session `session_id` to the exported head and metadata.
    fn fast_forward(&self, tx: &Transaction<'_>, session_id: &str) -> Result<()> {
        let session = &self.session;
        tx.execute(
            "UPDATE sessions SET format = MAX(format, ?2), updated_at = ?3, title = ?4,
                 title_manual = ?5, archived_at = ?6, head_id = ?7, message_count = ?8,
                 file_snapshots = ?9
             WHERE id = ?1",
            rusqlite::params![
                session_id,
                session.format,
                to_millis(session.updated_at),
                session.title,
                session.title_manual,
                session.archived_at.map(to_millis),
                session.head_id.map(|id| id.to_string()),
                session.message_count,
                file_snapshots_column(session)?,
            ],
        )?;
        Ok(())
    }
}

/// How an export lands on a local session it relates to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Landing {
    /// The export saw the local head, so the session moves to the exported one.
    FastForward,
    /// The session holds the exported head, so it stays as it is.
    Keep,
}

/// Inserts the exported row `session` under `id`, launched from `project`.
fn insert_session(
    tx: &Transaction<'_>,
    session: &SessionLine,
    id: &str,
    forked_from: Option<&str>,
    project: &Project,
) -> Result<()> {
    tx.execute(
        "INSERT INTO sessions (
             id, forked_from, project_root, cwd, git_branch, model, format, created_at,
             updated_at, title, title_manual, archived_at, head_id, message_count, file_snapshots
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        rusqlite::params![
            id,
            forked_from.or(session.forked_from.as_deref()),
            project.root,
            project.cwd,
            session.git_branch,
            session.model,
            session.format,
            to_millis(session.created_at),
            to_millis(session.updated_at),
            session.title,
            session.title_manual,
            session.archived_at.map(to_millis),
            session.head_id.map(|id| id.to_string()),
            session.message_count,
            file_snapshots_column(session)?,
        ],
    )?;
    Ok(())
}

fn file_snapshots_column(session: &SessionLine) -> Result<Option<String>> {
    Ok(session
        .file_snapshots
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::commit::Record;
    use crate::session::store::listing::ArchiveState;
    use crate::session::store::local_testing::{message_row, save};
    use crate::session::store::testing::{TEST_PROJECT, isolated_store, listed_ids};

    const SESSION: &str = "session";

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    /// Messages `n`, each under its `parent`, with the text `m{n}`.
    fn rows(chain: &[(u128, Option<u128>)]) -> Vec<MessageRow> {
        chain
            .iter()
            .map(|&(n, parent)| message_row(n, parent, Message::user(format!("m{n}"))))
            .collect()
    }

    fn title(title: &str) -> Record {
        Record::Title {
            title: title.to_owned(),
            manual: false,
        }
    }

    fn texts(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .map(|message| crate::session::state::extract_user_text(message).unwrap())
            .collect()
    }

    /// Messages `n`, each under its parent.
    type Chain = &'static [(u128, Option<u128>)];

    const CHAIN: Chain = &[(1, None), (2, Some(1)), (3, Some(2))];

    /// [`CHAIN`] continued by message 4.
    const EXTENDED: Chain = &[(1, None), (2, Some(1)), (3, Some(2)), (4, Some(3))];

    /// [`CHAIN`] branched after message 2 into message 6.
    const DIVERGED: Chain = &[(1, None), (2, Some(1)), (6, Some(2))];

    /// [`CHAIN`] compacted after message 3 into root 4, continued by message 5.
    const COMPACTED: Chain = &[
        (1, None),
        (2, Some(1)),
        (3, Some(2)),
        (4, None),
        (5, Some(4)),
    ];

    /// The export of [`SESSION`] titled "Exported", whose chain runs 1 ← 2 ← 3.
    fn exported() -> Vec<u8> {
        exported_from(TEST_PROJECT, CHAIN, 3)
    }

    /// The export of [`SESSION`] in `project`, titled "Exported", holding `messages` with its head
    /// at message `head`.
    fn exported_from(project: &str, messages: Chain, head: u128) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf(), project).unwrap();
        save(
            &store,
            SESSION,
            rows(messages),
            vec![title("Exported")],
            Some(head),
        );
        let mut out = Vec::new();
        store.export(SESSION, &mut out).unwrap();
        out
    }

    mod export {
        use super::*;

        #[test]
        fn names_a_missing_session() {
            let (_dir, store) = isolated_store();
            let mut out = Vec::new();

            let err = store.export(SESSION, &mut out).unwrap_err();

            assert_eq!(err.to_string(), "session not found: session");
            assert_eq!(out, b"", "nothing is written before the lookup fails");
        }
    }

    mod import {
        use super::*;

        /// What an import does with the local session.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Outcome {
            /// Moves it to the exported head, or creates it.
            Forward,
            Keep,
            Fork,
        }

        #[test]
        fn lands_the_exported_head_by_which_messages_each_side_holds() {
            use Outcome::{Fork, Forward, Keep};

            const M123: &[&str] = &["m1", "m2", "m3"];

            for (name, (exported, exported_head), (local, local_head), outcome, landed_texts) in [
                ("new", (CHAIN, 3), (&[][..], None), Forward, M123),
                ("behind", (CHAIN, 3), (&CHAIN[..2], Some(2)), Forward, M123),
                (
                    "empty local branch",
                    (CHAIN, 3),
                    (&CHAIN[..1], None),
                    Forward,
                    M123,
                ),
                (
                    "ahead",
                    (CHAIN, 3),
                    (EXTENDED, Some(4)),
                    Keep,
                    &["m1", "m2", "m3", "m4"],
                ),
                ("diverged", (CHAIN, 3), (DIVERGED, Some(6)), Fork, M123),
                (
                    "export compacted",
                    (COMPACTED, 5),
                    (CHAIN, Some(3)),
                    Forward,
                    &["m4", "m5"],
                ),
                (
                    "local compacted",
                    (CHAIN, 3),
                    (COMPACTED, Some(5)),
                    Keep,
                    &["m4", "m5"],
                ),
                (
                    "export rewound",
                    (CHAIN, 2),
                    (CHAIN, Some(3)),
                    Forward,
                    &["m1", "m2"],
                ),
            ] {
                let (_dir, store) = isolated_store();
                if !local.is_empty() {
                    save(
                        &store,
                        SESSION,
                        rows(local),
                        vec![title("Local")],
                        local_head,
                    );
                }
                let export = exported_from(TEST_PROJECT, exported, exported_head);

                let target = store.import(export.as_slice()).unwrap();

                assert_eq!(target != SESSION, outcome == Fork, "{name}");
                let landed = store.load_session_data(&target).unwrap();
                assert_eq!(texts(&landed.messages), landed_texts, "{name}");
                let forked_from = (outcome == Fork).then(|| SESSION.to_owned());
                let listed = store.find(&target).unwrap().unwrap();
                assert_eq!(listed.forked_from, forked_from, "{name}");
                let landed_title = if outcome == Keep { "Local" } else { "Exported" };
                assert_eq!(landed.title.as_deref(), Some(landed_title), "{name}");
                if outcome == Fork {
                    let source = store.load_session_data(SESSION).unwrap();
                    assert_eq!(source.head, local_head.map(id), "{name}: the source stays");
                }
            }
        }

        #[test]
        fn extends_one_fork_across_imports_of_a_diverged_session() {
            let (_dir, store) = isolated_store();
            save(&store, SESSION, rows(DIVERGED), Vec::new(), Some(6));
            let fork = store.import(exported().as_slice()).unwrap();

            let again = store.import(exported().as_slice()).unwrap();
            let extended = store
                .import(exported_from(TEST_PROJECT, EXTENDED, 4).as_slice())
                .unwrap();

            assert_ne!(fork, SESSION);
            assert_eq!([&again, &extended], [&fork, &fork]);
            let landed = store.load_session_data(&fork).unwrap();
            assert_eq!(texts(&landed.messages), ["m1", "m2", "m3", "m4"]);
            let source = store.load_session_data(SESSION).unwrap();
            assert_eq!(texts(&source.messages), ["m1", "m2", "m6"]);
        }

        #[test]
        fn forks_again_for_an_export_no_fork_relates_to() {
            let (_dir, store) = isolated_store();
            save(&store, SESSION, rows(DIVERGED), Vec::new(), Some(6));
            let first = store.import(exported().as_slice()).unwrap();
            let branched_early = exported_from(TEST_PROJECT, &[(1, None), (7, Some(1))], 7);

            let second = store.import(branched_early.as_slice()).unwrap();

            assert!(
                ![SESSION, first.as_str()].contains(&second.as_str()),
                "{second}"
            );
            let landed = store.load_session_data(&second).unwrap();
            assert_eq!(texts(&landed.messages), ["m1", "m7"]);
            let kept = store.load_session_data(&first).unwrap();
            assert_eq!(texts(&kept.messages), ["m1", "m2", "m3"]);
        }

        #[test]
        fn files_the_sessions_it_creates_under_this_project() {
            let export = exported_from("elsewhere", CHAIN, 3);
            let (_fresh_dir, fresh) = isolated_store();
            let (_local_dir, diverged) = isolated_store();
            save(&diverged, SESSION, rows(DIVERGED), Vec::new(), Some(6));

            let created = fresh.import(export.as_slice()).unwrap();
            let fork = diverged.import(export.as_slice()).unwrap();

            assert_eq!(listed_ids(&fresh, ArchiveState::Active), [created]);
            let listed = listed_ids(&diverged, ArchiveState::Active);
            assert!(listed.contains(&fork), "{listed:?}");
        }

        #[test]
        fn keeps_a_local_branch_that_an_export_without_one_cannot_extend() {
            let (_source_dir, source) = isolated_store();
            save(&source, SESSION, rows(&[(1, None)]), Vec::new(), None);
            let mut empty = Vec::new();
            source.export(SESSION, &mut empty).unwrap();
            let (_dir, store) = isolated_store();
            let local = rows(&[(1, None), (2, Some(1))]);
            save(&store, SESSION, local, vec![title("Local")], Some(2));

            let target = store.import(empty.as_slice()).unwrap();

            assert_eq!(target, SESSION);
            let kept = store.load_session_data(SESSION).unwrap();
            assert_eq!(texts(&kept.messages), ["m1", "m2"]);
            assert_eq!(kept.title.as_deref(), Some("Local"));
        }

        #[test]
        fn skips_blank_lines() {
            let export = String::from_utf8(exported()).unwrap();
            let (_dir, store) = isolated_store();

            store
                .import(export.replace('\n', "\n\n").as_bytes())
                .unwrap();

            let landed = store.load_session_data(SESSION).unwrap();
            assert_eq!(texts(&landed.messages), ["m1", "m2", "m3"]);
        }

        #[test]
        fn rejects_input_that_is_not_a_valid_export_naming_the_line() {
            let export = String::from_utf8(exported()).unwrap();
            let [session, m1, m2, _] = <[&str; 4]>::try_from(export.lines().collect::<Vec<_>>())
                .expect("the export holds the session and three messages");
            let edited = |field: &str, value: serde_json::Value| {
                let mut line: serde_json::Value = serde_json::from_str(session).unwrap();
                line[field] = value;
                line.to_string()
            };
            let escaping_id = edited("id", "../escape".into());
            let escaping_source = edited("forked_from", "\u{1b}[2J".into());
            let newer = edited("format", 2.into());

            for (name, input, expected) in [
                (
                    "empty",
                    vec![],
                    "not an ox session export: the input is empty".to_owned(),
                ),
                (
                    "message first",
                    vec![m1],
                    "line 1: not an ox session export, which starts with a session record"
                        .to_owned(),
                ),
                (
                    "ID that names no file",
                    vec![&escaping_id, m1],
                    r#"line 1: invalid session ID "../escape""#.to_owned(),
                ),
                (
                    "source that is no session ID",
                    vec![&escaping_source, m1],
                    r#"line 1: forked_from: invalid session ID "\u{1b}[2J""#.to_owned(),
                ),
                (
                    "newer format",
                    vec![&newer, m1],
                    "line 1: session format version 2 is newer than supported (1). \
                     Upgrade oxide-code to import it."
                        .to_owned(),
                ),
                (
                    "second session",
                    vec![session, session],
                    "line 2: a second session record".to_owned(),
                ),
                (
                    "malformed line",
                    vec![session, m1, "{"],
                    "line 3: EOF while parsing an object at line 1 column 1".to_owned(),
                ),
                (
                    "child before its parent",
                    vec![session, m2],
                    format!("line 2: message {} precedes its parent {}", id(2), id(1)),
                ),
                (
                    "head not exported",
                    vec![session, m1, m2],
                    format!("the session's head {} is not among its messages", id(3)),
                ),
            ] {
                let (_dir, store) = isolated_store();

                let err = store.import(input.join("\n").as_bytes()).unwrap_err();

                assert_eq!(format!("{err:#}"), expected, "{name}");
                assert_eq!(store.find(SESSION).unwrap(), None, "{name}");
            }
        }

        #[tokio::test]
        async fn moving_the_head_of_an_open_session_forks_its_next_write() {
            let (_dir, store) = isolated_store();
            save(
                &store,
                SESSION,
                rows(&[(1, None), (2, Some(1))]),
                Vec::new(),
                Some(2),
            );
            let resumed = crate::session::handle::resume(&store, SESSION, "m").unwrap();

            store.import(exported().as_slice()).unwrap();
            let next = resumed.handle.record_message(Message::user("m6")).await;
            resumed.handle.shutdown().await;

            let fork = next.report.fork.expect("the stale handle forks");
            assert_eq!(fork.from, SESSION);
            let source = store.load_session_data(SESSION).unwrap();
            assert_eq!(texts(&source.messages), ["m1", "m2", "m3"]);
            let forked = store.load_session_data(&fork.to).unwrap();
            assert_eq!(texts(&forked.messages), ["m1", "m2", "m6"]);
        }
    }
}
