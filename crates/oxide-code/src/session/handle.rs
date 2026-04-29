//! Public-facing session API.
//!
//! [`SessionHandle`] is held by the agent loop, the title generator,
//! and the TUI shutdown path. Every write method sends a
//! [`super::actor::SessionCmd`] and awaits the actor's oneshot ack;
//! callers never hold a lock across `await`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use tokio::sync::{mpsc, oneshot};

use super::actor::{self, SessionCmd};
use super::sanitize::sanitize_resumed_messages;
use super::state::{SessionState, extract_user_text};
use super::store::{
    SessionData, SessionStore, load_session_data_from_path, open_append_at,
    read_session_id_from_path,
};
use crate::message::Message;
use crate::tool::ToolMetadata;

/// Sized for tool-result bursts (a turn can stack a dozen sidecars on
/// top of a tool-result message). Codex uses 256; we budget higher
/// because the in-process actor only drains between agent yields.
const CHANNEL_CAPACITY: usize = 1024;

// ── SessionHandle ──

/// Cheap to clone — clones share one actor task. All methods are async
/// and return after the batch flush this cmd lands in.
#[derive(Clone)]
pub(crate) struct SessionHandle {
    cmd_tx: mpsc::Sender<SessionCmd>,
    session_id: Arc<str>,
    shared: Arc<SharedState>,
}

/// Single most-recent-failure slot the actor populates and the handle
/// drains. `std::sync::Mutex` is fine here — locks are held for
/// microseconds and there's no cross-task workflow to coordinate.
#[derive(Default)]
pub(super) struct SharedState {
    failure_seen: AtomicBool,
    failure_surfaced: AtomicBool,
    last_failure: std::sync::Mutex<Option<String>>,
}

impl SharedState {
    pub(super) fn record_failure(&self, msg: &str) {
        self.failure_seen.store(true, Ordering::Release);
        if let Ok(mut slot) = self.last_failure.lock() {
            *slot = Some(msg.to_owned());
        }
    }

    /// `true` on the first call after [`Self::record_failure`]; sticky
    /// `false` afterwards. Mirrors the pre-actor `write_failed` flag.
    pub(super) fn surface_first_failure(&self) -> bool {
        !self.failure_surfaced.swap(true, Ordering::AcqRel)
    }
}

/// Combines the AI-title seed and the first-failure surface into one
/// ack so callers don't have to make a second channel round-trip after
/// `record_message`.
pub(crate) struct RecordOutcome {
    /// `Some` only on the first user-text message of a fresh session.
    pub(crate) ai_title_seed: Option<String>,
    /// First I/O failure once per session; subsequent failures stay
    /// silent (warn-logged instead). `None` on healthy writes.
    pub(crate) failure: Option<String>,
}

pub(crate) struct Outcome {
    pub(crate) failure: Option<String>,
}

impl SessionHandle {
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Record a conversation message. Returns after the batch flush
    /// containing this cmd completes.
    pub(crate) async fn record_message(&self, msg: Message) -> RecordOutcome {
        let (ack, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCmd::Record { msg, ack })
            .await
            .is_err()
        {
            return RecordOutcome {
                ai_title_seed: None,
                failure: self.actor_gone_failure(),
            };
        }
        rx.await.unwrap_or(RecordOutcome {
            ai_title_seed: None,
            failure: self.actor_gone_failure(),
        })
    }

    /// Record display-only sidecar metadata. No-ops at the actor when
    /// `metadata == ToolMetadata::default()`.
    pub(crate) async fn record_tool_metadata(
        &self,
        tool_use_id: &str,
        metadata: &ToolMetadata,
    ) -> Outcome {
        let (ack, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCmd::ToolMetadata {
                tool_use_id: tool_use_id.to_owned(),
                metadata: metadata.clone(),
                ack,
            })
            .await
            .is_err()
        {
            return Outcome {
                failure: self.actor_gone_failure(),
            };
        }
        rx.await.unwrap_or(Outcome {
            failure: self.actor_gone_failure(),
        })
    }

    /// Append an AI-generated session title. Tail-scan picks the
    /// latest title (max `updated_at`), so this supersedes the
    /// first-prompt title on listings and resumes.
    pub(crate) async fn append_ai_title(&self, title: String) -> Outcome {
        let (ack, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCmd::AppendAiTitle { title, ack })
            .await
            .is_err()
        {
            return Outcome {
                failure: self.actor_gone_failure(),
            };
        }
        rx.await.unwrap_or(Outcome {
            failure: self.actor_gone_failure(),
        })
    }

    /// Write the session summary and finalize. Idempotent; no-op on
    /// fresh sessions that never recorded anything.
    pub(crate) async fn finish(&self) -> Outcome {
        let (ack, rx) = oneshot::channel();
        if self.cmd_tx.send(SessionCmd::Finish { ack }).await.is_err() {
            return Outcome {
                failure: self.actor_gone_failure(),
            };
        }
        rx.await.unwrap_or(Outcome {
            failure: self.actor_gone_failure(),
        })
    }

    /// "Actor task is unreachable" surfaced exactly once; subsequent
    /// drops are warn-log only. The actor never restarts, so this is
    /// almost always a panic-path artifact.
    fn actor_gone_failure(&self) -> Option<String> {
        self.shared.record_failure("session actor is gone");
        if self.shared.surface_first_failure() {
            tracing::warn!("session actor task is gone — recording dropped");
            Some(
                "Session writer task has stopped. Conversation history may be incomplete; \
                 further write errors will be silent."
                    .to_owned(),
            )
        } else {
            None
        }
    }
}

// ── ResumedSession ──

/// Live handle plus display-only extras the TUI uses to reconstruct
/// what the user saw live.
pub(crate) struct ResumedSession {
    pub(crate) handle: SessionHandle,
    pub(crate) messages: Vec<Message>,
    pub(crate) title: Option<String>,
    pub(crate) tool_result_metadata: HashMap<String, ToolMetadata>,
}

// ── Constructors ──

/// Start a fresh session and spawn its actor. The file materializes
/// lazily on the first record cmd.
pub(crate) fn start(store: &SessionStore, model: &str) -> SessionHandle {
    spawn_actor(SessionState::fresh(store.clone(), model))
}

/// Resume by session ID — loads, sanitizes, opens for append, spawns
/// the actor.
pub(crate) fn resume(store: &SessionStore, session_id: &str) -> Result<ResumedSession> {
    let data = store.load_session_data(session_id)?;
    let writer = store.open_append(session_id)?;
    from_resumed_data(store, session_id.to_owned(), data, writer)
}

/// Resume by explicit path, bypassing the XDG project lookup. Used by
/// `ox -c <path.jsonl>` for sessions copied between machines.
pub(crate) fn resume_from_path(store: &SessionStore, path: &Path) -> Result<ResumedSession> {
    let session_id = read_session_id_from_path(path)?;
    let data = load_session_data_from_path(path)?;
    let writer = open_append_at(path)?;
    from_resumed_data(store, session_id, data, writer)
}

fn from_resumed_data(
    store: &SessionStore,
    session_id: String,
    mut data: SessionData,
    writer: super::store::SessionWriter,
) -> Result<ResumedSession> {
    sanitize_resumed_messages(&mut data.messages);
    // After-sanitize check: a file that loaded non-empty but emptied
    // out (all unresolved tool_use + orphan tool_result) would
    // otherwise slip through with `last_message_uuid` pointing at a
    // dropped message, and the next record would chain to a missing
    // UUID.
    if data.messages.is_empty() {
        bail!("session {session_id} has no messages to resume");
    }

    let first_user_prompt_seen = data.messages.iter().any(|m| extract_user_text(m).is_some());
    let message_count = u32::try_from(data.messages.len()).unwrap_or(u32::MAX);
    let title = data.title.map(|t| t.title);

    let state = SessionState::resumed(
        store.clone(),
        session_id,
        writer,
        data.last_uuid,
        message_count,
        first_user_prompt_seen,
    );
    let handle = spawn_actor(state);
    Ok(ResumedSession {
        handle,
        messages: data.messages,
        title,
        tool_result_metadata: data.tool_result_metadata,
    })
}

fn spawn_actor(state: SessionState) -> SessionHandle {
    let session_id = Arc::clone(&state.session_id);
    let shared = Arc::new(SharedState::default());
    let (cmd_tx, cmd_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let actor_shared = Arc::clone(&shared);
    tokio::spawn(actor::run(state, cmd_rx, actor_shared));
    SessionHandle {
        cmd_tx,
        session_id,
        shared,
    }
}

/// Creates a [`SessionHandle`] whose actor channel is already closed, so
/// every write call immediately returns the actor-gone failure. Exposed
/// to sibling `session::*` and `agent` test modules that need a dead handle
/// without being able to access [`SessionHandle`]'s private fields directly.
#[cfg(test)]
pub(crate) fn dead_handle_for_tests(session_id: &str) -> SessionHandle {
    let (cmd_tx, _) = mpsc::channel(1);
    SessionHandle {
        cmd_tx,
        session_id: Arc::from(session_id),
        shared: Arc::new(SharedState::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::{test_session_file, test_store};
    use super::*;
    use crate::message::{ContentBlock, Role};

    // ── start ──

    #[tokio::test]
    async fn start_does_not_materialize_file_until_first_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "test-model");

        let project_dir = super::super::store::test_project_dir(dir.path());
        assert!(
            std::fs::read_dir(&project_dir).unwrap().next().is_none(),
            "fresh session must not create a file before the first record",
        );

        handle.record_message(Message::user("first")).await;
        // Awaiting record_message means the actor has flushed the
        // batch — file is on disk.
        assert!(
            test_session_file(dir.path(), handle.session_id()).exists(),
            "first record_message should materialize the session file",
        );
    }

    // ── resume ──

    #[tokio::test]
    async fn resume_loads_messages_and_keeps_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = start(&store, "m");
        let session_id = original.session_id().to_owned();
        original.record_message(Message::user("hello")).await;
        original.record_message(Message::assistant("hi")).await;
        original.finish().await;
        drop(original);

        let ResumedSession {
            handle: resumed,
            messages,
            ..
        } = resume(&store, &session_id).unwrap();
        assert_eq!(resumed.session_id(), session_id);
        assert_eq!(messages.len(), 2);
    }

    #[tokio::test]
    async fn resume_works_on_unfinished_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = start(&store, "m");
        let session_id = original.session_id().to_owned();
        original.record_message(Message::user("hello")).await;
        // Drop the handle without finish — actor task still drains
        // queued cmds before exiting (mpsc::Receiver returns None
        // when all senders drop).
        drop(original);
        // Yield long enough for the spawned actor to drain.
        tokio::task::yield_now().await;

        let ResumedSession { messages, .. } = resume(&store, &session_id).unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn resume_empty_session_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = start(&store, "m");
        let session_id = original.session_id().to_owned();
        original.finish().await;
        drop(original);

        assert!(resume(&store, &session_id).is_err());
    }

    #[tokio::test]
    async fn resume_all_messages_sanitized_returns_error() {
        // A file that loads non-empty but whose only message is an
        // unresolved assistant tool_use. Sanitization removes it,
        // leaving an empty message list that would corrupt the UUID
        // chain on the next record. The bail! on line 236 guards this.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(crate::message::Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "unresolved".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::Value::Null,
                }],
            })
            .await;
        // The ack means the actor flushed — file is on disk.
        drop(original);

        let err = resume(&store, &session_id)
            .err()
            .expect("all messages sanitized must be an error");
        assert!(
            format!("{err:#}").contains("no messages to resume"),
            "error explains why: {err:#}",
        );
    }

    #[tokio::test]
    async fn resume_drops_assistant_message_with_only_unresolved_tool_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = start(&store, "m");
        let session_id = original.session_id().to_owned();
        original.record_message(Message::user("do X")).await;
        original
            .record_message(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "unresolved".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::Value::Null,
                }],
            })
            .await;
        drop(original);
        tokio::task::yield_now().await;

        let ResumedSession { messages, .. } = resume(&store, &session_id).unwrap();
        assert_eq!(
            messages.len(),
            1,
            "assistant-only-tool-use should be dropped"
        );
        assert_eq!(messages[0].role, Role::User);
    }

    #[tokio::test]
    async fn resume_preserves_parent_chain_on_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = start(&store, "m");
        let session_id = original.session_id().to_owned();
        original.record_message(Message::user("hello")).await;
        original.record_message(Message::assistant("hi")).await;
        original.finish().await;
        drop(original);

        let resumed = resume(&store, &session_id).unwrap().handle;
        resumed.record_message(Message::user("follow up")).await;
        resumed.finish().await;
        drop(resumed);

        // Read the file and find the new message's parent_uuid — should
        // match the last uuid from the original run.
        let content = std::fs::read_to_string(test_session_file(dir.path(), &session_id)).unwrap();
        let entries: Vec<super::super::entry::Entry> = content
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let msg_uuids: Vec<_> = entries
            .iter()
            .filter_map(|e| match e {
                super::super::entry::Entry::Message {
                    uuid, parent_uuid, ..
                } => Some((*uuid, *parent_uuid)),
                _ => None,
            })
            .collect();
        assert_eq!(msg_uuids.len(), 3);
        assert!(msg_uuids[0].1.is_none());
        assert_eq!(msg_uuids[1].1, Some(msg_uuids[0].0));
        assert_eq!(
            msg_uuids[2].1,
            Some(msg_uuids[1].0),
            "post-resume message chains to pre-resume tail",
        );
    }

    // ── record_message ──

    #[tokio::test]
    async fn record_message_writes_title_before_first_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();

        handle.record_message(Message::user("First prompt")).await;
        handle.finish().await;
        drop(handle);

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        // Line 0: header. Line 1: title. Line 2: message.
        assert!(lines[1].contains(r#""type":"title""#));
        assert!(lines[2].contains(r#""type":"message""#));
    }

    #[tokio::test]
    async fn record_message_returns_ai_title_seed_only_for_first_user_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");

        let outcome_first = handle.record_message(Message::user("Fix login bug")).await;
        let outcome_second = handle.record_message(Message::user("follow up")).await;

        assert_eq!(
            outcome_first.ai_title_seed.as_deref(),
            Some("Fix login bug"),
        );
        assert!(
            outcome_second.ai_title_seed.is_none(),
            "subsequent records do not re-seed the AI title generator",
        );
    }

    #[tokio::test]
    async fn record_message_actor_gone_surfaces_failure_once_then_silences() {
        // First call must return Some (the sticky one-time message); the
        // second call returns None so the user sees the error only once.
        let handle = dead_handle_for_tests("dead");

        let first = handle
            .record_message(crate::message::Message::user("a"))
            .await;
        let second = handle
            .record_message(crate::message::Message::user("b"))
            .await;

        assert!(
            first.failure.is_some(),
            "first call after actor gone must surface failure",
        );
        assert!(
            second.failure.is_none(),
            "subsequent calls must be silent after first surface",
        );
    }

    #[tokio::test]
    async fn record_message_resumed_session_does_not_seed_ai_title() {
        // Resumed sessions already have a first-prompt title on disk;
        // we don't try to regenerate the AI title here.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = start(&store, "m");
        let session_id = original.session_id().to_owned();
        original.record_message(Message::user("hello")).await;
        original.finish().await;
        drop(original);

        let resumed = resume(&store, &session_id).unwrap().handle;
        let outcome = resumed.record_message(Message::user("more text")).await;
        assert!(outcome.ai_title_seed.is_none());
    }

    // ── record_tool_metadata ──

    #[tokio::test]
    async fn record_tool_metadata_actor_gone_surfaces_failure_once_then_silences() {
        let handle = dead_handle_for_tests("dead");
        let meta = crate::tool::ToolMetadata {
            title: Some("f.rs".to_owned()),
            ..crate::tool::ToolMetadata::default()
        };

        let first = handle.record_tool_metadata("t1", &meta).await;
        let second = handle.record_tool_metadata("t2", &meta).await;

        assert!(first.failure.is_some(), "first call must surface failure");
        assert!(second.failure.is_none(), "subsequent calls must be silent");
    }

    #[tokio::test]
    async fn record_tool_metadata_round_trips_title_and_replacements() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();
        handle.record_message(Message::user("edit something")).await;

        handle
            .record_tool_metadata(
                "t1",
                &ToolMetadata {
                    title: Some("Edited f.rs".to_owned()),
                    replacements: Some(4),
                    ..ToolMetadata::default()
                },
            )
            .await;
        handle.finish().await;
        drop(handle);

        let data = store.load_session_data(&sid).unwrap();
        let metadata = data.tool_result_metadata.get("t1").unwrap();
        assert_eq!(metadata.title.as_deref(), Some("Edited f.rs"));
        assert_eq!(metadata.replacements, Some(4));
    }

    #[tokio::test]
    async fn record_tool_metadata_skips_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();
        handle.record_message(Message::user("trigger")).await;

        handle
            .record_tool_metadata("t1", &ToolMetadata::default())
            .await;
        handle.finish().await;
        drop(handle);

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        assert!(
            !content.contains(r#""type":"tool_result_metadata""#),
            "default metadata must not be written: {content}",
        );
    }

    // ── append_ai_title ──

    #[tokio::test]
    async fn append_ai_title_actor_gone_surfaces_failure_once_then_silences() {
        let handle = dead_handle_for_tests("dead");

        let first = handle.append_ai_title("Fix auth".to_owned()).await;
        let second = handle.append_ai_title("Fix auth".to_owned()).await;

        assert!(first.failure.is_some(), "first call must surface failure");
        assert!(second.failure.is_none(), "subsequent calls must be silent");
    }

    #[tokio::test]
    async fn append_ai_title_writes_title_entry_and_supersedes_first_prompt_on_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();
        handle.record_message(Message::user("Fix login bug")).await;

        handle
            .append_ai_title("Fix auth flow for mobile".to_owned())
            .await;
        handle.finish().await;
        drop(handle);

        let sessions = store.list().unwrap();
        let session = sessions.iter().find(|s| s.session_id == sid).unwrap();
        assert_eq!(
            session.title.as_ref().unwrap().title,
            "Fix auth flow for mobile",
        );
    }

    // ── finish ──

    #[tokio::test]
    async fn finish_actor_gone_surfaces_failure_once_then_silences() {
        let handle = dead_handle_for_tests("dead");

        let first = handle.finish().await;
        let second = handle.finish().await;

        assert!(first.failure.is_some(), "first call must surface failure");
        assert!(second.failure.is_none(), "subsequent calls must be silent");
    }

    #[tokio::test]
    async fn finish_writes_summary_with_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();

        handle
            .record_message(Message::user("Fix the auth bug"))
            .await;
        handle.finish().await;
        drop(handle);

        let sessions = store.list().unwrap();
        let session = sessions.iter().find(|s| s.session_id == sid).unwrap();
        assert_eq!(session.title.as_ref().unwrap().title, "Fix the auth bug");
        assert_eq!(session.exit.as_ref().unwrap().message_count, 1);
    }

    #[tokio::test]
    async fn finish_empty_session_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let _sid = handle.session_id().to_owned();
        handle.finish().await;
        drop(handle);

        let project_dir = super::super::store::test_project_dir(dir.path());
        assert!(
            std::fs::read_dir(&project_dir).unwrap().next().is_none(),
            "empty session must not write a file",
        );
        assert!(
            store.list().unwrap().is_empty(),
            "empty session must not appear in --list",
        );
    }

    #[tokio::test]
    async fn finish_skips_summary_on_empty_resume() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = start(&store, "m");
        let session_id = original.session_id().to_owned();
        original.record_message(Message::user("hello")).await;
        original.finish().await;
        drop(original);

        let resumed = resume(&store, &session_id).unwrap().handle;
        resumed.finish().await;
        drop(resumed);

        let content = std::fs::read_to_string(test_session_file(dir.path(), &session_id)).unwrap();
        let summary_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"summary""#))
            .count();
        assert_eq!(summary_count, 1);
    }
}
