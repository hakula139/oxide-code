//! Store fixtures only this crate's tests use: the deterministic SQL dump and export masks the
//! session format tests snapshot, and the loaders that read them back.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use regex::{Captures, Regex};
use rusqlite::Connection;
use rusqlite::types::Value;
use uuid::Uuid;

use super::commit::{Commit, Head, MessageRow, Record, Saved, SessionRow, commit, find_head};
use super::testing::test_store;
use super::{DB_FILE, SessionStore};
use crate::message::Message;

/// The columns that hold Unix-millisecond timestamps.
const TIME_COLUMNS: [&str; 3] = ["created_at", "updated_at", "archived_at"];

/// A deterministic SQL dump of `store`: its revision, the schema from `sqlite_schema`, then every
/// row of each table in insertion order. UUIDs are numbered `<UUID-n>` in order of appearance, so a
/// dump keeps which rows share an ID. Each timestamp is numbered `<TS-n>` in order of appearance,
/// since two writes may or may not share a millisecond. [`load_dump`] reads it back once [`unmask`]
/// fills the placeholders.
pub(crate) fn sql_dump(store: &SessionStore) -> String {
    let conn = store.connect().unwrap();
    let revision: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let mut dump = format!("PRAGMA user_version = {revision};\n");
    let mut schema = conn
        .prepare("SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY rowid")
        .unwrap();
    for sql in schema.query_map([], |row| row.get::<_, String>(0)).unwrap() {
        writeln!(dump, "{};", sql.unwrap()).unwrap();
    }

    let mut times = 0;
    for table in ["sessions", "messages", "usage"] {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let columns: Vec<String> = stmt.column_names().map(str::to_owned).collect();
        let rows = stmt
            .query_map([], |row| {
                (0..columns.len())
                    .map(|i| row.get::<_, Value>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap();
        for values in rows {
            let literals: Vec<String> = columns
                .iter()
                .zip(values.unwrap())
                .map(|(column, value)| match value {
                    Value::Null => "NULL".to_owned(),
                    Value::Integer(_) if TIME_COLUMNS.contains(&column.as_str()) => {
                        times += 1;
                        format!("<TS-{times}>")
                    }
                    Value::Integer(n) => n.to_string(),
                    Value::Text(text) => format!("'{}'", text.replace('\'', "''")),
                    Value::Real(_) | Value::Blob(_) => unreachable!("the schema stores neither"),
                })
                .collect();
            writeln!(
                dump,
                "INSERT INTO {table} VALUES ({});",
                literals.join(", ")
            )
            .unwrap();
        }
    }
    mask_uuids(&dump)
}

/// Masks the per-run UUIDs and RFC 3339 timestamps of an export as [`sql_dump`] masks a dump.
pub(crate) fn mask_export(export: &str) -> String {
    let timestamp = Regex::new(r#""\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z""#).unwrap();
    let mut times = 0;
    let masked = timestamp.replace_all(export, |_: &Captures<'_>| {
        times += 1;
        format!(r#""<TS-{times}>""#)
    });
    mask_uuids(&masked)
}

fn mask_uuids(text: &str) -> String {
    let uuid = Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let mut seen: Vec<&str> = Vec::new();
    for found in uuid.find_iter(text) {
        if !seen.contains(&found.as_str()) {
            seen.push(found.as_str());
        }
    }
    uuid.replace_all(text, |caps: &Captures<'_>| {
        let n = seen.iter().position(|id| *id == &caps[0]).unwrap() + 1;
        format!("<UUID-{n}>")
    })
    .into_owned()
}

/// Fills each placeholder of a masked dump or export with one fixed value, giving a higher `<TS-n>`
/// a later time: Unix milliseconds in a dump and RFC 3339 in an export.
pub(crate) fn unmask(fixture: &str) -> String {
    let placeholder = Regex::new(r#""<TS-(\d+)>"|<TS-(\d+)>|<UUID-(\d+)>"#).unwrap();
    placeholder
        .replace_all(fixture, |caps: &Captures<'_>| {
            let n = |i: usize| caps.get(i).map(|n| n.as_str().parse::<u64>().unwrap());
            if let Some(n) = n(1) {
                format!(r#""2026-01-01T00:{:02}:{:02}Z""#, n / 60, n % 60)
            } else if let Some(n) = n(2) {
                (1_767_225_600_000 + n * 1000).to_string()
            } else {
                format!("00000000-0000-4000-8000-{:012x}", n(3).unwrap())
            }
        })
        .into_owned()
}

/// Makes every later write to `store` fail with "disk full", as a full disk would.
pub(crate) fn fail_writes(store: &SessionStore) {
    let mut triggers = String::new();
    for (table, event) in [
        ("sessions", "INSERT"),
        ("sessions", "UPDATE"),
        ("sessions", "DELETE"),
        ("messages", "INSERT"),
        ("usage", "INSERT"),
    ] {
        writeln!(
            triggers,
            "CREATE TRIGGER fail_{table}_{event} BEFORE {event} ON {table} \
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;"
        )
        .unwrap();
    }
    store.connect().unwrap().execute_batch(&triggers).unwrap();
}

/// Message `n` of a test chain, with ID `Uuid::from_u128(n)`, under message `parent`.
pub(crate) fn message_row(n: u128, parent: Option<u128>, message: Message) -> MessageRow {
    MessageRow {
        id: Uuid::from_u128(n),
        parent_id: parent.map(Uuid::from_u128),
        message,
        context: None,
        compact: None,
        tool_metadata: BTreeMap::new(),
        created_at: 0,
    }
}

/// Commits `rows` and then `records` to session `session_id`, creating it with the first commit,
/// and moves its head to message `head`.
pub(crate) fn save(
    store: &SessionStore,
    session_id: &str,
    rows: Vec<MessageRow>,
    records: Vec<Record>,
    head: Option<u128>,
) {
    let mut conn = store.connect().unwrap();
    let base = {
        let tx = conn.transaction().unwrap();
        find_head(&tx, session_id).unwrap()
    };
    let session = SessionRow {
        id: session_id.to_owned(),
        forked_from: None,
        cwd: "/repo".to_owned(),
        project_root: "/repo".to_owned(),
        git_branch: None,
        model: "m".to_owned(),
        created_at: 0,
        title: None,
        title_manual: false,
    };
    let records: Vec<Record> = rows
        .into_iter()
        .map(|row| Record::Message(Box::new(row)))
        .chain(records)
        .collect();
    let head = Head {
        id: head.map(Uuid::from_u128),
        message_count: 0,
    };

    commit(
        &mut conn,
        store,
        &Commit {
            session: &session,
            saved: base.map_or(Saved::Fresh, |base| Saved::At { base }),
            records: &records,
            head: Some(head),
        },
    )
    .unwrap();
}

/// A [`test_store`] in `dir` whose database is the unmasked `dump`.
pub(crate) fn load_dump(dir: &Path, dump: &str) -> SessionStore {
    Connection::open(dir.join(DB_FILE))
        .unwrap()
        .execute_batch(dump)
        .unwrap();
    test_store(dir)
}
