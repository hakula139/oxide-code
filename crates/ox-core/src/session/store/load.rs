//! Resume reads: a session's chain from its head back to its root, decoded into [`SessionData`].

use std::collections::HashMap;

use anyhow::{Context as _, Result, bail};
use rusqlite::{Connection, OptionalExtension as _, Row};
use uuid::Uuid;

use super::commit::{MESSAGE_COLUMNS, MessageRow, parse_id};
use super::listing::ArchiveState;
use super::{CHAIN_CTE, CompactInfo, UsageRecord, check_format};
use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::prompt::SessionContext;
use crate::session::state::extract_user_text;
use crate::tool::ToolMetadata;

// ── SessionData ──

/// A session as resume reads it: the active branch since its last compaction and the state that
/// branch carries.
#[derive(Debug)]
pub(crate) struct SessionData {
    pub(crate) session_id: String,
    pub(crate) forked_from: Option<String>,
    pub(crate) git_branch: Option<String>,
    /// Unix milliseconds.
    pub(crate) created_at: i64,
    pub(crate) messages: Vec<Message>,
    /// The message the next one chains from, `None` for an empty branch.
    pub(crate) head: Option<Uuid>,
    /// The newest prompt state the branch records, `None` when no message recorded one.
    pub(crate) context: Option<SessionContext>,
    /// The compaction the branch starts from.
    pub(crate) compact: Option<CompactInfo>,
    pub(crate) title: Option<String>,
    pub(crate) title_manual: bool,
    /// Whether the branch already used the first-prompt title opportunity.
    pub(crate) first_user_prompt_seen: bool,
    pub(crate) archive_state: ArchiveState,
    pub(crate) tool_metadata: HashMap<String, ToolMetadata>,
    /// Every billed request the session made, on any branch and across compactions.
    pub(crate) usage: Vec<UsageRecord>,
    pub(crate) file_snapshots: Vec<FileSnapshot>,
}

/// Loads session `session_id` for resume. Fails for a missing session and for one a newer build
/// wrote, which this build cannot read.
pub(crate) fn load(conn: &mut Connection, session_id: &str) -> Result<SessionData> {
    let tx = conn.transaction()?;
    let Some(row) = tx
        .query_row(
            "SELECT format, head_id, title, title_manual, archived_at, file_snapshots, forked_from,
                 git_branch, created_at
             FROM sessions WHERE id = ?1",
            [session_id],
            SessionRow::read,
        )
        .optional()?
    else {
        bail!("session not found: {session_id}");
    };
    check_format(row.format, "open")?;
    let head = row.head_id.as_deref().map(parse_id).transpose()?;

    let mut chain = Chain::default();
    if let Some(head) = &row.head_id {
        let mut stmt = tx.prepare(&chain_query())?;
        let mut rows = stmt.query((session_id, head))?;
        while let Some(message) = rows.next()? {
            chain.push(MessageRow::read(message)?);
        }
    }
    let usage = {
        let mut stmt =
            tx.prepare("SELECT record FROM usage WHERE session_id = ?1 ORDER BY rowid")?;
        stmt.query_map([session_id], |row| row.get::<_, String>(0))?
            .map(|record| serde_json::from_str(&record?).context("invalid usage record"))
            .collect::<Result<Vec<UsageRecord>>>()?
    };
    let file_snapshots = row
        .file_snapshots
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .context("invalid file snapshots")?
        .unwrap_or_default();

    let first_user_prompt_seen = chain.has_prompt || chain.compact.is_some() || row.title_manual;
    Ok(SessionData {
        session_id: session_id.to_owned(),
        forked_from: row.forked_from,
        git_branch: row.git_branch,
        created_at: row.created_at,
        messages: chain.messages,
        head,
        context: chain.context,
        compact: chain.compact,
        title: row.title,
        title_manual: row.title_manual,
        first_user_prompt_seen,
        archive_state: row.archived_at.is_some().into(),
        tool_metadata: chain.tool_metadata,
        usage,
        file_snapshots,
    })
}

/// The messages of the [`CHAIN_CTE`] chain, in commit order.
fn chain_query() -> String {
    format!(
        "{CHAIN_CTE}
         SELECT {MESSAGE_COLUMNS} FROM messages WHERE seq IN (SELECT seq FROM chain) ORDER BY seq"
    )
}

/// The `sessions` columns [`load`] reads.
struct SessionRow {
    format: u32,
    head_id: Option<String>,
    title: Option<String>,
    title_manual: bool,
    archived_at: Option<i64>,
    file_snapshots: Option<String>,
    forked_from: Option<String>,
    git_branch: Option<String>,
    created_at: i64,
}

impl SessionRow {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            format: row.get(0)?,
            head_id: row.get(1)?,
            title: row.get(2)?,
            title_manual: row.get(3)?,
            archived_at: row.get(4)?,
            file_snapshots: row.get(5)?,
            forked_from: row.get(6)?,
            git_branch: row.get(7)?,
            created_at: row.get(8)?,
        })
    }
}

/// The messages of a chain and the state they carry, folded in commit order.
#[derive(Default)]
struct Chain {
    messages: Vec<Message>,
    context: Option<SessionContext>,
    compact: Option<CompactInfo>,
    tool_metadata: HashMap<String, ToolMetadata>,
    has_prompt: bool,
}

impl Chain {
    fn push(&mut self, row: MessageRow) {
        if row.context.is_some() {
            self.context = row.context;
        }
        if self.messages.is_empty() {
            self.compact = row.compact;
        }
        self.tool_metadata.extend(row.tool_metadata);
        self.has_prompt |= extract_user_text(&row.message).is_some();
        self.messages.push(row.message);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::message::{ContentBlock, Role};
    use crate::prompt::PromptParts;
    use crate::session::store::commit::{Record, UsageRow};
    use crate::session::store::local_testing::{self, message_row};
    use crate::session::store::testing::isolated_store;
    use crate::session::store::{CONTENT_VERSION, CompactRecord, SessionStore};

    const SESSION: &str = "session";

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn context(date: &str) -> SessionContext {
        let mut prompt = PromptParts::default();
        date.clone_into(&mut prompt.seen.date);
        prompt.session_context()
    }

    fn metadata(title: &str) -> BTreeMap<String, ToolMetadata> {
        BTreeMap::from([(
            format!("for-{title}"),
            ToolMetadata {
                title: Some(title.to_owned()),
                result_view: None,
            },
        )])
    }

    /// A compaction root whose summary is `summary`.
    fn root(n: u128, summary: &str) -> MessageRow {
        MessageRow {
            compact: Some(CompactInfo {
                record: CompactRecord {
                    summary: summary.to_owned(),
                    instructions: None,
                    tasks: Vec::new(),
                },
                pre_message_count: 2,
            }),
            ..message_row(n, None, Message::assistant(summary))
        }
    }

    fn save(store: &SessionStore, rows: Vec<MessageRow>, records: Vec<Record>, head: Option<u128>) {
        local_testing::save(store, SESSION, rows, records, head);
    }

    fn usage(input_tokens: u32) -> Record {
        Record::Usage(UsageRow {
            id: Uuid::new_v4(),
            record: serde_json::from_value(serde_json::json!({
                "model": "m",
                "cache_ttl": "5m",
                "input_tokens": input_tokens,
            }))
            .unwrap(),
            created_at: 0,
        })
    }

    fn load_saved(store: &SessionStore) -> Result<SessionData> {
        load(&mut store.connect().unwrap(), SESSION)
    }

    mod load {
        use super::*;

        #[test]
        fn reads_the_chain_from_the_head_to_its_root_with_the_state_it_carries() {
            let (_dir, store) = isolated_store();
            let tool_result = Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("t1", "ok", false)],
            };
            save(
                &store,
                vec![
                    MessageRow {
                        context: Some(context("before-compaction")),
                        tool_metadata: metadata("before-compaction"),
                        ..message_row(1, None, Message::user("before compaction"))
                    },
                    message_row(2, Some(1), Message::assistant("summarized")),
                ],
                Vec::new(),
                Some(2),
            );
            save(
                &store,
                vec![
                    MessageRow {
                        context: Some(context("root")),
                        ..root(3, "summary")
                    },
                    MessageRow {
                        tool_metadata: metadata("kept"),
                        ..message_row(4, Some(3), tool_result.clone())
                    },
                    MessageRow {
                        context: Some(context("abandoned")),
                        tool_metadata: metadata("abandoned"),
                        ..message_row(5, Some(4), Message::assistant("abandoned"))
                    },
                ],
                Vec::new(),
                Some(5),
            );
            save(
                &store,
                vec![MessageRow {
                    context: Some(context("newest")),
                    tool_metadata: metadata("head"),
                    ..message_row(6, Some(4), Message::assistant("head"))
                }],
                Vec::new(),
                Some(6),
            );

            let data = load_saved(&store).unwrap();

            assert_eq!(
                data.messages,
                [
                    Message::assistant("summary"),
                    tool_result,
                    Message::assistant("head")
                ]
            );
            assert_eq!(data.head, Some(id(6)));
            assert_eq!(data.compact.unwrap().record.summary, "summary");
            assert_eq!(data.context, Some(context("newest")));
            assert_eq!(
                data.tool_metadata,
                metadata("kept")
                    .into_iter()
                    .chain(metadata("head"))
                    .collect::<HashMap<_, _>>()
            );
        }

        #[test]
        fn latches_the_first_prompt_title_from_what_the_branch_holds() {
            let summarized_root = || root(1, "summary");
            for (name, rows, manual, head, latched) in [
                (
                    "prompt on the branch",
                    vec![message_row(1, None, Message::user("prompt"))],
                    false,
                    Some(1),
                    true,
                ),
                (
                    "compaction root",
                    vec![summarized_root()],
                    false,
                    Some(1),
                    true,
                ),
                (
                    "manual title",
                    vec![message_row(1, None, Message::user("rewound"))],
                    true,
                    None,
                    true,
                ),
                (
                    "prompt rewound off the branch",
                    vec![message_row(1, None, Message::user("rewound"))],
                    false,
                    None,
                    false,
                ),
            ] {
                let (_dir, store) = isolated_store();
                let title = Record::Title {
                    title: "Title".to_owned(),
                    manual,
                };
                save(&store, rows, vec![title], Some(1));
                save(&store, Vec::new(), Vec::new(), head);

                let data = load_saved(&store).unwrap();

                assert_eq!(data.first_user_prompt_seen, latched, "{name}");
                assert_eq!(data.title_manual, manual, "{name}");
            }
        }

        #[test]
        fn returns_every_billed_request_across_compactions() {
            let (_dir, store) = isolated_store();
            save(
                &store,
                vec![message_row(1, None, Message::user("prompt"))],
                vec![usage(1)],
                Some(1),
            );
            save(
                &store,
                vec![root(2, "summary")],
                vec![usage(2), usage(3)],
                Some(2),
            );

            let data = load_saved(&store).unwrap();

            let input_tokens: Vec<u32> = data
                .usage
                .iter()
                .map(|usage| usage.tokens.input_tokens)
                .collect();
            assert_eq!(input_tokens, [1, 2, 3]);
        }

        #[test]
        fn refuses_a_session_a_newer_build_wrote() {
            let (_dir, store) = isolated_store();
            save(
                &store,
                vec![message_row(1, None, Message::user("prompt"))],
                Vec::new(),
                Some(1),
            );
            store
                .connect()
                .unwrap()
                .execute("UPDATE sessions SET format = ?1", [CONTENT_VERSION + 1])
                .unwrap();

            let err = load_saved(&store).unwrap_err();

            assert_eq!(
                err.to_string(),
                "session format version 2 is newer than supported (1). Upgrade oxide-code to open it."
            );
        }

        #[test]
        fn names_a_missing_session() {
            let (_dir, store) = isolated_store();

            let err = load_saved(&store).unwrap_err();

            assert_eq!(err.to_string(), "session not found: session");
        }
    }
}
