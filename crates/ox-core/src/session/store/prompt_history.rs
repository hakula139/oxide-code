//! Bounded history queries over the ordinary user text retained in saved sessions.

use std::collections::HashSet;

use anyhow::{Context as _, Result};
use indoc::formatdoc;
use rusqlite::{Row, params};
use time::OffsetDateTime;

use super::listing::ProjectScope;
use super::{CONTENT_VERSION, SessionStore, from_millis};
use crate::message::ContentBlock;

/// A retained user prompt, with its original submission time and project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptEntry {
    pub text: String,
    pub created_at: OffsetDateTime,
    pub project_root: String,
}

/// A newest-first search of retained prompts. Forks and imports of the same logical message
/// appear once, while distinct submissions of identical text remain separate entries.
pub struct PromptWalk {
    store: SessionStore,
    scope: ProjectScope,
    query: String,
    after: Option<(i64, String, i64)>,
    seen: HashSet<String>,
    exhausted: bool,
}

impl PromptWalk {
    /// Returns whether every candidate row has been examined, including those outside the scope
    /// or without matching ordinary user text.
    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Examines at most `limit` stored user-role rows and returns their matching prompts. An
    /// empty page can have more candidates, so callers continue until [`Self::is_exhausted`].
    pub fn next_page(&mut self, limit: usize) -> Result<Vec<PromptEntry>> {
        if self.exhausted {
            return Ok(Vec::new());
        }
        let rows = candidates(&self.store, self.scope, self.after.as_ref(), limit)?;
        self.exhausted = rows.len() < limit || rows.is_empty();
        let mut prompts = Vec::new();
        for Candidate {
            created_at,
            id,
            seq,
            project_root,
            content,
        } in rows
        {
            self.after = Some((created_at, id.clone(), seq));
            let Some(content) = content else {
                continue;
            };
            let blocks = serde_json::from_str(&content).context("failed to read prompt history")?;
            let Some(text) = ordinary_text(blocks) else {
                continue;
            };
            if text.to_lowercase().contains(&self.query) && self.seen.insert(id) {
                prompts.push(PromptEntry {
                    text,
                    created_at: from_millis(created_at)?,
                    project_root,
                });
            }
        }
        Ok(prompts)
    }
}

struct Candidate {
    created_at: i64,
    id: String,
    seq: i64,
    project_root: String,
    content: Option<String>,
}

impl Candidate {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            created_at: row.get(0)?,
            id: row.get(1)?,
            seq: row.get(2)?,
            project_root: row.get(3)?,
            content: row.get(4)?,
        })
    }
}

fn candidates(
    store: &SessionStore,
    scope: ProjectScope,
    after: Option<&(i64, String, i64)>,
    limit: usize,
) -> Result<Vec<Candidate>> {
    let conn = store.connect()?;
    let sql = candidate_query(after.is_some());
    let project = (scope == ProjectScope::Current).then_some(&store.project.root);
    let mut statement = conn.prepare(&sql)?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = if let Some((at, id, seq)) = after {
        statement.query_map(
            params![CONTENT_VERSION, project, limit, at, id, seq],
            Candidate::read,
        )?
    } else {
        statement.query_map(params![CONTENT_VERSION, project, limit], Candidate::read)?
    };
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn candidate_query(continuation: bool) -> String {
    let cursor = if continuation {
        " AND (m.created_at, m.id, m.seq) < (?4, ?5, ?6)"
    } else {
        ""
    };
    formatdoc! {"
        SELECT m.created_at, m.id, m.seq, s.project_root,
            CASE WHEN s.format <= ?1 AND (?2 IS NULL OR s.project_root = ?2)
            THEN m.content END
        FROM messages m JOIN sessions s ON s.id = m.session_id
        WHERE m.role = 'user'{cursor}
        ORDER BY m.created_at DESC, m.id DESC, m.seq DESC LIMIT ?3
    "}
}

impl SessionStore {
    /// Searches retained ordinary user prompts in `scope` by a case-insensitive literal substring.
    pub fn prompt_history(&self, scope: ProjectScope, query: &str) -> PromptWalk {
        PromptWalk {
            store: self.clone(),
            scope,
            query: query.to_lowercase(),
            after: None,
            seen: HashSet::new(),
            exhausted: false,
        }
    }
}

fn ordinary_text(blocks: Vec<ContentBlock>) -> Option<String> {
    let text = blocks
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, origin: None } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::TextOrigin;
    use crate::session::store::testing::{isolated_store, seed_test_session};

    fn session(store: &SessionStore, id: &str) {
        seed_test_session(store, id, None, 0, OffsetDateTime::UNIX_EPOCH);
        store
            .connect()
            .unwrap()
            .execute("DELETE FROM messages WHERE session_id = ?1", [id])
            .unwrap();
    }

    fn save(store: &SessionStore, session: &str, id: &str, at: i64, role: &str, content: &str) {
        store
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO messages (session_id, id, role, content, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session, id, role, content, at],
            )
            .unwrap();
    }

    fn prompt(store: &SessionStore, session: &str, id: &str, at: i64, text: &str) {
        let content = serde_json::to_string(&vec![ContentBlock::text(text)]).unwrap();
        save(store, session, id, at, "user", &content);
    }

    fn collect(mut walk: PromptWalk, limit: usize) -> Vec<String> {
        let mut prompts = Vec::new();
        while !walk.is_exhausted() {
            prompts.extend(
                walk.next_page(limit)
                    .unwrap()
                    .into_iter()
                    .map(|entry| entry.text),
            );
        }
        assert_eq!(walk.next_page(limit).unwrap(), []);
        prompts
    }

    mod prompt_walk {
        use super::*;

        mod next_page {
            use super::*;

            #[test]
            fn scans_past_empty_batches_and_orders_original_messages_once() {
                let (_dir, store) = isolated_store();
                session(&store, "source");
                session(&store, "fork");
                prompt(&store, "source", "old", 10, "old prompt");
                prompt(&store, "source", "repeat-a", 20, "repeated prompt");
                prompt(&store, "source", "repeat-b", 20, "repeated prompt");
                prompt(&store, "source", "new", 30, "new prompt");
                prompt(&store, "fork", "old", 10, "old prompt");
                prompt(&store, "source", "blank", 40, " \n\t");
                save(
                    &store,
                    "source",
                    "result",
                    50,
                    "user",
                    &serde_json::to_string(&vec![ContentBlock::tool_result(
                        "call",
                        "tool output",
                        false,
                    )])
                    .unwrap(),
                );
                save(
                    &store,
                    "source",
                    "assistant",
                    60,
                    "assistant",
                    &serde_json::to_string(&vec![ContentBlock::text("assistant prompt")]).unwrap(),
                );
                let mut walk = store.prompt_history(ProjectScope::Current, "prompt");

                assert_eq!(walk.next_page(1).unwrap(), []);
                assert!(!walk.is_exhausted());
                assert_eq!(walk.next_page(1).unwrap(), []);
                let prompts = collect(walk, 1);

                assert_eq!(
                    prompts,
                    [
                        "new prompt",
                        "repeated prompt",
                        "repeated prompt",
                        "old prompt"
                    ]
                );
            }

            #[test]
            fn keeps_archived_and_detached_prompts_in_scope_and_drops_deleted_sessions() {
                let (dir, store) = isolated_store();
                let other = SessionStore::open_at(dir.path().to_path_buf(), "other").unwrap();
                session(&store, "local");
                session(&other, "other");
                session(&store, "future");
                prompt(&store, "local", "local", 10, "local draft\nsecond line");
                prompt(&other, "other", "other", 20, "other prompt");
                save(
                    &store,
                    "future",
                    "future",
                    30,
                    "user",
                    "[{\"type\":\"future\"}]",
                );
                let conn = store.connect().unwrap();
                conn.execute(
                    "UPDATE sessions SET format = ?1 WHERE id = 'future'",
                    [CONTENT_VERSION + 1],
                )
                .unwrap();
                conn.execute("UPDATE sessions SET archived_at = 5, head_id = NULL, message_count = 0 WHERE id = 'local'", []).unwrap();

                let local = collect(store.prompt_history(ProjectScope::Current, ""), 1);
                let everywhere = collect(store.prompt_history(ProjectScope::Every, ""), 1);

                assert_eq!(local, ["local draft\nsecond line"]);
                assert_eq!(everywhere, ["other prompt", "local draft\nsecond line"]);

                store.delete("local").unwrap();

                assert_eq!(
                    collect(store.prompt_history(ProjectScope::Every, ""), 1),
                    ["other prompt"]
                );
            }

            #[test]
            fn filters_full_text_with_unicode_case_and_literal_metacharacters() {
                let (_dir, store) = isolated_store();
                session(&store, "local");
                let text = "  First line\nÄBC %_*'\\ remain exact  ";
                prompt(&store, "local", "message", 10, text);

                for (query, expected) in [
                    ("first", vec![text]),
                    ("äbc", vec![text]),
                    ("%_*'\\", vec![text]),
                    ("absent", vec![]),
                ] {
                    let prompts = collect(store.prompt_history(ProjectScope::Current, query), 2);

                    assert_eq!(prompts, expected, "{query:?}");
                }
            }

            #[test]
            fn reports_store_and_supported_content_failures() {
                let missing = SessionStore::unopened("example");

                let error = missing
                    .prompt_history(ProjectScope::Current, "")
                    .next_page(1)
                    .unwrap_err();

                assert!(format!("{error:#}").contains("failed to open"));

                let (_dir, store) = isolated_store();
                session(&store, "local");
                save(&store, "local", "broken", 10, "user", "not JSON");

                let error = store
                    .prompt_history(ProjectScope::Current, "")
                    .next_page(1)
                    .unwrap_err();

                assert!(format!("{error:#}").starts_with("failed to read prompt history:"));
            }
        }
    }

    mod candidate_query {
        use super::*;

        #[test]
        fn uses_the_original_time_index_without_sorting() {
            let (_dir, store) = isolated_store();
            let conn = store.connect().unwrap();

            for continuation in [false, true] {
                let sql = format!("EXPLAIN QUERY PLAN {}", candidate_query(continuation));
                let mut statement = conn.prepare(&sql).unwrap();
                let params: Vec<rusqlite::types::Value> = if continuation {
                    vec![
                        CONTENT_VERSION.into(),
                        rusqlite::types::Value::Null,
                        30.into(),
                        100.into(),
                        "id".to_owned().into(),
                        5.into(),
                    ]
                } else {
                    vec![
                        CONTENT_VERSION.into(),
                        rusqlite::types::Value::Null,
                        30.into(),
                    ]
                };
                let plan = statement
                    .query_map(rusqlite::params_from_iter(params), |row| {
                        row.get::<_, String>(3)
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
                    .join("\n");

                assert!(plan.contains("messages_by_prompt_time"), "{plan}");
                assert!(!plan.contains("TEMP B-TREE"), "{plan}");
            }
        }
    }

    mod ordinary_text {
        use super::*;

        #[test]
        fn preserves_only_ordinary_blocks_in_order_without_trimming() {
            let generated = ContentBlock::generated_text("generated", TextOrigin::Interruption);
            for (blocks, expected) in [
                (
                    vec![
                        ContentBlock::text(" first "),
                        generated.clone(),
                        ContentBlock::text("second\n"),
                    ],
                    Some(" first \nsecond\n"),
                ),
                (vec![generated], None),
                (
                    vec![ContentBlock::tool_result("call", "output", false)],
                    None,
                ),
                (vec![ContentBlock::text(" \n\t")], None),
            ] {
                assert_eq!(ordinary_text(blocks).as_deref(), expected);
            }
        }
    }
}
