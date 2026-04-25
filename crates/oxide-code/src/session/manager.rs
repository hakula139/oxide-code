//! Session lifecycle.
//!
//! [`SessionManager`] owns the on-disk session file handle for the
//! lifetime of one `ox` run: [`start`][SessionManager::start] creates
//! a fresh session, [`resume`][SessionManager::resume] loads an
//! existing one and reopens it for append,
//! [`record_message`][SessionManager::record_message] appends each
//! assistant / tool-result message, and
//! [`finish`][SessionManager::finish] writes the exit summary.
//!
//! Resume-time transcript repair lives in
//! [`super::sanitize::sanitize_resumed_messages`] — that module turns a
//! mid-turn crash or partial JSONL write into a transcript the API
//! will accept as the prefix of a new turn.

use std::path::Path;

use anyhow::{Context, Result, bail};
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

use super::entry::{CURRENT_VERSION, Entry, TitleSource};
use super::sanitize::sanitize_resumed_messages;
use super::store::{
    SessionData, SessionStore, SessionWriter, load_session_data_from_path, open_append_at,
    read_session_id_from_path,
};
use crate::message::{ContentBlock, Message, Role};
use crate::tool::ToolMetadata;

/// Data handed back by [`SessionManager::resume`] +
/// [`SessionManager::resume_from_path`]: the ready-to-use manager,
/// the replayed message log, and display-only extras
/// (session title, per-tool-result metadata) the TUI needs to
/// reconstruct the same view the user saw live.
pub(crate) struct ResumedSession {
    pub(crate) manager: SessionManager,
    pub(crate) messages: Vec<Message>,
    pub(crate) title: Option<String>,
    pub(crate) tool_result_metadata: std::collections::HashMap<String, ToolMetadata>,
}

/// Maximum title length (in characters) derived from the first user prompt.
///
/// Sized for wide terminals: the `--list` row is `ID(10) Last Active(19)
/// Msgs(6) Title`, so ~80 chars of title space on a 120-col terminal
/// and still truncates cleanly (`...`) on narrower ones.
const MAX_TITLE_LEN: usize = 80;

// ── SessionManager ──

/// High-level session lifecycle, owned by the agent loop.
///
/// Wraps a [`SessionWriter`] to provide a simple record-oriented API:
/// start or resume a session, record each message, and write a summary
/// on exit.
///
/// Files are created lazily — [`start`][Self::start] only allocates a
/// session ID and stages the header in memory, and the on-disk file is
/// materialized by the first [`record_message`][Self::record_message]
/// call. A session that exits before any message is recorded leaves no
/// artifact behind, so `ox` then quit never litters the session list
/// with empty, unresumable rows.
pub(crate) struct SessionManager {
    /// Cloned at construction so [`record_message`] can lazily call
    /// `store.create()` once the first message arrives. `SessionStore`
    /// is a thin handle over two `PathBuf`s, so the clone is cheap.
    store: SessionStore,
    /// Header staged at [`start`][Self::start] time; `take()`-en when
    /// the writer is materialized so the file's `created_at` reflects
    /// the moment `ox` was launched, not the moment the user typed.
    /// Always `None` on a resumed session (the writer is opened
    /// eagerly by [`resume`][Self::resume]).
    pending_header: Option<Entry>,
    /// `None` until the first [`record_message`][Self::record_message]
    /// (fresh session) or eagerly populated by
    /// [`resume`][Self::resume].
    writer: Option<SessionWriter>,
    session_id: String,
    /// Message count at construction time. For fresh sessions this is
    /// `0`; for resumed sessions it equals the loaded message count.
    /// Used by [`finish`][Self::finish] to skip writing a duplicate
    /// summary when no new messages were recorded.
    initial_message_count: u32,
    message_count: u32,
    /// Captured from the first user text seen. Doubles as a flag: if
    /// `Some`, the initial [`Entry::Title`] was already written (either
    /// this run or by the previous run before resume).
    first_user_prompt: Option<String>,
    /// Populated exactly once, when [`record_message`][Self::record_message]
    /// writes the first-prompt title on a fresh session. Callers drain it
    /// via [`take_ai_title_seed`][Self::take_ai_title_seed] to kick off
    /// background AI-title generation. Stays `None` on resumed sessions
    /// (the first-prompt title is already on disk) so we don't overwrite
    /// a previous run's AI title after resume.
    ai_title_seed: Option<String>,
    /// UUID of the last recorded [`Entry::Message`]. Used as
    /// `parent_uuid` for the next recorded message, forming the
    /// conversation chain.
    last_message_uuid: Option<Uuid>,
    finished: bool,
    /// Latched the first time a [`record_message`][Self::record_message] or
    /// [`finish`][Self::finish] call returns an error. Lets callers
    /// surface the failure to the user exactly once per session —
    /// subsequent failures warn-log only so we don't spam the UI with
    /// repeated disk-full / permission errors mid-conversation.
    write_failed: bool,
}

impl SessionManager {
    /// Starts a new session. Allocates a session ID and stages the
    /// header in memory; the on-disk file is created by the first
    /// [`record_message`][Self::record_message]. A session that exits
    /// without recording any message therefore leaves no file behind,
    /// keeping `ox --list` clear of empty sessions from `ox`-then-quit
    /// flows.
    pub(crate) fn start(store: &SessionStore, model: &str) -> Self {
        let (session_id, header) = new_header(model);
        Self {
            store: store.clone(),
            pending_header: Some(header),
            writer: None,
            session_id,
            initial_message_count: 0,
            message_count: 0,
            first_user_prompt: None,
            ai_title_seed: None,
            last_message_uuid: None,
            finished: false,
            write_failed: false,
        }
    }

    /// Resume a previous session. Loads its messages, sanitizes them to
    /// a resumable state (drops unresolved `tool_use` blocks and pairs
    /// orphan `tool_result` turns with a sentinel), and reopens the
    /// existing session file in append mode.
    ///
    /// Note: there is a small TOCTOU window between `load_session_data`
    /// (read, no lock) and `open_append` (no lock either, since
    /// concurrent resume is supported via the UUID DAG). Another
    /// process could append messages in between, making the loaded
    /// messages stale; the loser branch survives in the file but is
    /// invisible to later resumes.
    pub(crate) async fn resume(store: &SessionStore, session_id: &str) -> Result<ResumedSession> {
        let data = store.load_session_data(session_id)?;
        let writer = store.open_append(session_id).await?;
        Self::from_resumed_data(store, session_id.to_owned(), data, writer)
    }

    /// Resume a session from an explicit file path, bypassing the XDG
    /// project subdirectory lookup entirely. Used by `ox -c <path.jsonl>`
    /// to pick up sessions that were copied between machines or that live
    /// outside the configured store root.
    ///
    /// The manager still carries the current store so downstream code (like
    /// future slash commands that create sibling sessions) has a reference
    /// point; it is never used on the resumed path since
    /// [`record_message`][Self::record_message] skips the
    /// `store.create()` branch when `pending_header` is `None`.
    pub(crate) fn resume_from_path(store: &SessionStore, path: &Path) -> Result<ResumedSession> {
        let session_id = read_session_id_from_path(path)?;
        let data = load_session_data_from_path(path)?;
        let writer = open_append_at(path)?;
        Self::from_resumed_data(store, session_id, data, writer)
    }

    /// Shared tail of [`resume`][Self::resume] and
    /// [`resume_from_path`][Self::resume_from_path]: sanitize the loaded
    /// transcript, reject empty results, and build the manager.
    fn from_resumed_data(
        store: &SessionStore,
        session_id: String,
        mut data: SessionData,
        writer: SessionWriter,
    ) -> Result<ResumedSession> {
        sanitize_resumed_messages(&mut data.messages);
        // Run the emptiness check *after* sanitization. Otherwise a file
        // that loads into a non-empty vector but becomes empty after
        // sanitize (all unresolved tool_use + orphan tool_result, or
        // sanitization dropped every turn) would slip through with
        // `last_message_uuid = data.last_uuid`, and the next recorded
        // message would chain to a UUID that's no longer in view.
        if data.messages.is_empty() {
            bail!("session {session_id} has no messages to resume");
        }

        let first_user_prompt = data
            .messages
            .iter()
            .find_map(extract_user_text)
            .map(String::from);
        let message_count = u32::try_from(data.messages.len()).unwrap_or(u32::MAX);
        let title = data.title.map(|t| t.title);

        let manager = Self {
            store: store.clone(),
            pending_header: None,
            writer: Some(writer),
            session_id,
            initial_message_count: message_count,
            message_count,
            first_user_prompt,
            ai_title_seed: None,
            last_message_uuid: data.last_uuid,
            finished: false,
            write_failed: false,
        };
        Ok(ResumedSession {
            manager,
            messages: data.messages,
            title,
            tool_result_metadata: data.tool_result_metadata,
        })
    }

    /// Record a conversation message to the session file.
    ///
    /// Materializes the on-disk file on the first call by writing the
    /// staged header before the message. On the first user message
    /// that carries text, also writes an initial [`Entry::Title`]
    /// *before* the [`Entry::Message`] so listings show the correct
    /// title even if the process crashes before any further progress.
    ///
    /// File materialization is async (header write + Unix `0o600`
    /// chmod); a transient failure (e.g., disk full, permission
    /// error) leaves `pending_header` populated so a later retry can
    /// still create the file.
    pub(crate) async fn record_message(&mut self, message: &Message) -> Result<()> {
        let now = OffsetDateTime::now_utc();

        if let Some(header) = self.pending_header.as_ref() {
            // Materialize on success only; on error, leave
            // `pending_header` populated so a later retry can still
            // create the file.
            let writer = self.store.create(header).await?;
            self.writer = Some(writer);
            self.pending_header = None;
        }
        let writer = self
            .writer
            .as_mut()
            .expect("writer is materialized after pending_header is consumed");

        if self.first_user_prompt.is_none()
            && let Some(text) = extract_user_text(message)
        {
            // Cache the prompt before the title append. If the append
            // fails we still remember that the first user prompt has
            // been seen, so a later retry does not promote the second
            // user message as the session title. Consistency trumps
            // persistence here — the in-memory title is lost either
            // way; we just refuse to silently replace it.
            self.first_user_prompt = Some(text.to_owned());
            // Seed the AI title generator with the full prompt (not the
            // first-prompt truncation) so Haiku has full context. Taken
            // at most once per session; resumed sessions don't set this.
            self.ai_title_seed = Some(text.to_owned());
            writer.append(&Entry::Title {
                title: truncate_title(text, MAX_TITLE_LEN),
                source: TitleSource::FirstPrompt,
                updated_at: now,
            })?;
        }

        let uuid = Uuid::new_v4();
        writer.append(&Entry::Message {
            uuid,
            parent_uuid: self.last_message_uuid,
            message: message.clone(),
            timestamp: now,
        })?;
        self.last_message_uuid = Some(uuid);
        self.message_count = self.message_count.saturating_add(1);
        Ok(())
    }

    /// Writes the summary entry. No-op if already called, if no message
    /// was ever recorded (fresh session that never materialized a
    /// file), or if this is a resumed session and no new messages were
    /// recorded (avoids accumulating duplicate summaries on empty
    /// resume cycles).
    pub(crate) fn finish(&mut self) -> Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            // No record_message ever succeeded → no file exists →
            // nothing to summarize. The session leaves no trace.
            self.finished = true;
            return Ok(());
        };
        if self.finished
            || (self.initial_message_count > 0 && self.message_count == self.initial_message_count)
        {
            return Ok(());
        }
        self.finished = true;

        writer.append(&Entry::Summary {
            message_count: self.message_count,
            updated_at: OffsetDateTime::now_utc(),
        })
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns (and clears) the seed text that should be passed to the AI
    /// title generator. Returns `Some` at most once per session — right
    /// after the first user prompt has been recorded on a fresh session.
    /// Subsequent calls return `None`, as do all calls on resumed
    /// sessions.
    pub(crate) fn take_ai_title_seed(&mut self) -> Option<String> {
        self.ai_title_seed.take()
    }

    /// Records display metadata for a completed tool call. Written
    /// as [`Entry::ToolResultMetadata`], keyed by `tool_use_id` so
    /// resume can reattach it to the matching
    /// [`ContentBlock::ToolResult`](crate::message::ContentBlock)
    /// without polluting the API-facing wire format.
    ///
    /// No-ops when `metadata` carries nothing to display (avoids
    /// writing a stream of empty sidecar entries for tools that
    /// don't set any metadata).
    pub(crate) fn record_tool_result_metadata(
        &mut self,
        tool_use_id: &str,
        metadata: &ToolMetadata,
    ) -> Result<()> {
        if metadata == &ToolMetadata::default() {
            return Ok(());
        }
        let writer = self
            .writer
            .as_mut()
            .context("cannot record tool metadata before the session file is materialized")?;
        writer.append(&Entry::ToolResultMetadata {
            tool_use_id: tool_use_id.to_owned(),
            metadata: metadata.clone(),
            timestamp: OffsetDateTime::now_utc(),
        })
    }

    /// Appends an AI-generated title to the session file. Latest
    /// [`Entry::Title`] wins on tail scan, so this supersedes the
    /// first-prompt title for both `--list` output and later resumes.
    pub(crate) fn append_ai_title(&mut self, title: &str) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .context("cannot append AI title before the session file is materialized")?;
        writer.append(&Entry::Title {
            title: title.to_owned(),
            source: TitleSource::AiGenerated,
            updated_at: OffsetDateTime::now_utc(),
        })
    }

    /// Mark a write failure and report whether this is the first one.
    ///
    /// Returns `true` the first time it's called after a failed write;
    /// `false` on every subsequent call. Callers use the `true` return
    /// to surface a one-off UI error without re-reporting on each later
    /// failed write.
    pub(crate) fn record_write_failure(&mut self) -> bool {
        let first = !self.write_failed;
        self.write_failed = true;
        first
    }
}

// ── Helpers ──

fn new_header(model: &str) -> (String, Entry) {
    let session_id = Uuid::new_v4().to_string();
    let header = Entry::Header {
        session_id: session_id.clone(),
        cwd: current_dir_string(),
        model: model.to_owned(),
        created_at: OffsetDateTime::now_utc(),
        version: CURRENT_VERSION,
    };
    (session_id, header)
}

fn current_dir_string() -> String {
    match std::env::current_dir() {
        Ok(p) => p.display().to_string(),
        Err(e) => {
            warn!("failed to read current directory: {e}");
            "<unknown>".to_owned()
        }
    }
}

/// Extracts the first non-empty text content from a user message.
fn extract_user_text(message: &Message) -> Option<&str> {
    if message.role != Role::User {
        return None;
    }
    message.content.iter().find_map(|b| match b {
        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })
}

/// Truncates a title to `max_len` characters, adding "..." if truncated.
///
/// `max_len` must be at least 4 (three for the ellipsis, one for at least one
/// character of the title). Only internal callers drive this with
/// [`MAX_TITLE_LEN`] = 80, so the precondition is a sanity check, not user
/// input handling.
fn truncate_title(s: &str, max_len: usize) -> String {
    debug_assert!(max_len >= 4, "truncate_title: max_len must be >= 4");
    let trimmed = s.lines().next().unwrap_or(s).trim();
    if trimmed.chars().count() <= max_len {
        trimmed.to_owned()
    } else {
        let boundary = trimmed
            .char_indices()
            .nth(max_len - 3)
            .map_or(trimmed.len(), |(i, _)| i);
        format!("{}...", &trimmed[..boundary])
    }
}

#[cfg(test)]
mod tests {
    use super::super::sanitize::RESUME_CONTINUATION_SENTINEL;
    use super::super::store::{test_project_dir, test_session_file, test_store};
    use super::*;

    // ── start ──

    #[tokio::test]
    async fn start_does_not_materialize_file_until_first_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "test-model");
        assert!(
            std::fs::read_dir(test_project_dir(dir.path()))
                .unwrap()
                .next()
                .is_none(),
            "fresh session must not create a file before the first record_message",
        );
        assert_eq!(manager.message_count, 0);
        assert!(manager.last_message_uuid.is_none());

        manager
            .record_message(&Message::user("first"))
            .await
            .unwrap();
        assert!(
            test_session_file(dir.path(), manager.session_id()).exists(),
            "first record_message should materialize the session file",
        );
    }

    // ── resume ──

    #[tokio::test]
    async fn resume_loads_messages_and_keeps_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("hello"))
            .await
            .unwrap();
        original
            .record_message(&Message::assistant("hi"))
            .await
            .unwrap();
        original.finish().unwrap();
        drop(original);

        let ResumedSession {
            manager: resumed,
            messages,
            ..
        } = SessionManager::resume(&store, &session_id).await.unwrap();
        assert_eq!(resumed.session_id(), session_id);
        assert_eq!(messages.len(), 2);
        assert_eq!(resumed.message_count, 2);
        assert!(resumed.last_message_uuid.is_some());
    }

    #[tokio::test]
    async fn resume_works_on_unfinished_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("hello"))
            .await
            .unwrap();
        drop(original); // no finish() — simulates a crash

        let ResumedSession {
            manager: resumed,
            messages,
            ..
        } = SessionManager::resume(&store, &session_id).await.unwrap();
        assert_eq!(resumed.session_id(), session_id);
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn resume_after_mid_stream_abort_yields_clean_user_ending_transcript() {
        // Regression test for D5.4: the TUI records the user message,
        // then Ctrl+C during streaming aborts the agent task before
        // the assistant message can be recorded. `run_tui` still
        // calls `finish()` on the post-abort path, writing a Summary.
        // The next resume must see exactly one user message with no
        // sanitization heroics needed.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("what does this do?"))
            .await
            .unwrap();
        original.finish().unwrap();
        drop(original);

        let ResumedSession {
            manager: resumed,
            messages,
            ..
        } = SessionManager::resume(&store, &session_id).await.unwrap();
        assert_eq!(resumed.session_id(), session_id);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        let ContentBlock::Text { text } = &messages[0].content[0] else {
            panic!("expected text block, got {:?}", messages[0].content);
        };
        assert_eq!(text, "what does this do?");
        assert_eq!(
            resumed.message_count, 1,
            "resumed count should reflect the one recorded user turn"
        );
        assert!(
            resumed.last_message_uuid.is_some(),
            "parent_uuid chain must carry over to the next recorded message"
        );
    }

    #[tokio::test]
    async fn resume_empty_session_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original.finish().unwrap();
        drop(original);

        assert!(SessionManager::resume(&store, &session_id).await.is_err());
    }

    #[tokio::test]
    async fn resume_drops_unresolved_trailing_tool_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("do X"))
            .await
            .unwrap();
        original
            .record_message(&Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Let me check".to_owned(),
                    },
                    ContentBlock::ToolUse {
                        id: "unresolved_tool".to_owned(),
                        name: "bash".to_owned(),
                        input: serde_json::json!({"cmd": "ls"}),
                    },
                ],
            })
            .await
            .unwrap();
        drop(original); // crash before tool_result

        let ResumedSession { messages, .. } =
            SessionManager::resume(&store, &session_id).await.unwrap();
        assert_eq!(messages.len(), 2);
        let assistant = &messages[1];
        assert_eq!(assistant.role, Role::Assistant);
        assert!(
            !assistant
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. })),
            "unresolved tool_use should be dropped"
        );
        assert!(
            assistant
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { .. })),
            "text block should be preserved"
        );
    }

    #[tokio::test]
    async fn resume_drops_assistant_message_with_only_unresolved_tool_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("do X"))
            .await
            .unwrap();
        original
            .record_message(&Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "unresolved".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::Value::Null,
                }],
            })
            .await
            .unwrap();
        drop(original);

        let ResumedSession { messages, .. } =
            SessionManager::resume(&store, &session_id).await.unwrap();
        assert_eq!(
            messages.len(),
            1,
            "assistant-only-tool-use should be dropped"
        );
        assert_eq!(messages[0].role, Role::User);
    }

    #[tokio::test]
    async fn resume_errors_when_sanitize_empties_transcript() {
        // Single assistant message with nothing but an unresolved
        // tool_use — sanitize drops it, leaving an empty vec. The
        // pre-fix code accepted this but kept `last_message_uuid =
        // Some(dropped_uuid)`, so the next recorded message would
        // parent-chain to a UUID no longer present in memory.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "unresolved".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::Value::Null,
                }],
            })
            .await
            .unwrap();
        drop(original);

        let err = SessionManager::resume(&store, &session_id)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("no messages to resume"), "got: {err}");
    }

    #[tokio::test]
    async fn resume_appends_sentinel_when_last_is_user_tool_results_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("do X"))
            .await
            .unwrap();
        original
            .record_message(&Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::Value::Null,
                }],
            })
            .await
            .unwrap();
        original
            .record_message(&Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_owned(),
                    content: "ok".to_owned(),
                    is_error: false,
                }],
            })
            .await
            .unwrap();
        drop(original); // crash before next assistant response

        let ResumedSession { messages, .. } =
            SessionManager::resume(&store, &session_id).await.unwrap();
        assert_eq!(messages.len(), 4, "sentinel should be appended");
        assert_eq!(messages[3].role, Role::Assistant);
        assert!(
            matches!(&messages[3].content[0], ContentBlock::Text { text } if text == RESUME_CONTINUATION_SENTINEL)
        );
    }

    #[tokio::test]
    async fn resume_preserves_parent_chain_on_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("hello"))
            .await
            .unwrap();
        original
            .record_message(&Message::assistant("hi"))
            .await
            .unwrap();
        drop(original);

        let mut resumed = SessionManager::resume(&store, &session_id)
            .await
            .unwrap()
            .manager;
        resumed
            .record_message(&Message::user("follow up"))
            .await
            .unwrap();

        // Read the file and find the new message's parent_uuid — should
        // match the last uuid from the original run.
        let content = std::fs::read_to_string(test_session_file(dir.path(), &session_id)).unwrap();
        let entries: Vec<Entry> = content
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let msg_uuids: Vec<_> = entries
            .iter()
            .filter_map(|e| match e {
                Entry::Message {
                    uuid, parent_uuid, ..
                } => Some((*uuid, *parent_uuid)),
                _ => None,
            })
            .collect();
        assert_eq!(msg_uuids.len(), 3);
        assert!(msg_uuids[0].1.is_none(), "first message has no parent");
        assert_eq!(
            msg_uuids[1].1,
            Some(msg_uuids[0].0),
            "second message chains to first"
        );
        assert_eq!(
            msg_uuids[2].1,
            Some(msg_uuids[1].0),
            "post-resume message chains to pre-resume tail"
        );
    }

    #[tokio::test]
    async fn resume_appends_and_updates_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("Fix the auth bug"))
            .await
            .unwrap();
        original.finish().unwrap();
        drop(original);

        let mut resumed = SessionManager::resume(&store, &session_id)
            .await
            .unwrap()
            .manager;
        resumed
            .record_message(&Message::assistant("Done."))
            .await
            .unwrap();
        resumed.finish().unwrap();
        drop(resumed);

        let sessions = store.list().unwrap();
        let session = sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .unwrap();
        let title = session.title.as_ref().unwrap();
        assert_eq!(title.title, "Fix the auth bug");
        let exit = session.exit.as_ref().unwrap();
        assert_eq!(exit.message_count, 2);

        let data = store.load_session_data(&session_id).unwrap();
        assert_eq!(data.messages.len(), 2);
    }

    // ── record_message ──

    #[tokio::test]
    async fn record_message_increments_count_and_chains_parent() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m");
        let sid = manager.session_id().to_owned();

        manager
            .record_message(&Message::user("hello"))
            .await
            .unwrap();
        manager
            .record_message(&Message::assistant("hi"))
            .await
            .unwrap();
        assert_eq!(manager.message_count, 2);

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        let entries: Vec<Entry> = content
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let msgs: Vec<_> = entries
            .iter()
            .filter_map(|e| match e {
                Entry::Message {
                    uuid, parent_uuid, ..
                } => Some((*uuid, *parent_uuid)),
                _ => None,
            })
            .collect();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].1.is_none());
        assert_eq!(msgs[1].1, Some(msgs[0].0));
    }

    #[tokio::test]
    async fn record_message_writes_title_before_first_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m");
        let sid = manager.session_id().to_owned();
        manager
            .record_message(&Message::user("First prompt"))
            .await
            .unwrap();

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        // Line 0: header. Line 1: title. Line 2: message.
        assert!(lines[1].contains(r#""type":"title""#));
        assert!(lines[2].contains(r#""type":"message""#));
    }

    #[tokio::test]
    async fn record_message_writes_title_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m");
        let sid = manager.session_id().to_owned();

        manager
            .record_message(&Message::user("first"))
            .await
            .unwrap();
        manager
            .record_message(&Message::user("second"))
            .await
            .unwrap();

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        let title_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"title""#))
            .count();
        assert_eq!(title_count, 1);
        assert_eq!(manager.first_user_prompt.as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn record_message_no_title_for_tool_result_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m");
        let sid = manager.session_id().to_owned();

        manager
            .record_message(&Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t".to_owned(),
                    content: "out".to_owned(),
                    is_error: false,
                }],
            })
            .await
            .unwrap();

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        assert!(!content.contains(r#""type":"title""#));
    }

    // ── finish ──

    #[tokio::test]
    async fn finish_writes_summary_with_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m");
        let sid = manager.session_id().to_owned();

        manager
            .record_message(&Message::user("Fix the auth bug"))
            .await
            .unwrap();
        manager.finish().unwrap();

        let sessions = store.list().unwrap();
        let session = sessions.iter().find(|s| s.session_id == sid).unwrap();
        assert_eq!(session.title.as_ref().unwrap().title, "Fix the auth bug");
        assert_eq!(session.exit.as_ref().unwrap().message_count, 1);
    }

    #[tokio::test]
    async fn finish_empty_session_leaves_no_file() {
        // A session that exits without recording any message must not
        // appear in `--list` or be resumable — the file is materialized
        // lazily on the first append. See the bug report on PR #13:
        // `ox` then quit produced an unresumable "(untitled), 0 msgs"
        // entry.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m");
        let _sid = manager.session_id().to_owned();
        manager.finish().unwrap();

        assert!(
            std::fs::read_dir(test_project_dir(dir.path()))
                .unwrap()
                .next()
                .is_none(),
            "empty session must not write a file",
        );
        assert!(
            store.list().unwrap().is_empty(),
            "empty session must not appear in --list",
        );
    }

    #[tokio::test]
    async fn finish_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m");
        let sid = manager.session_id().to_owned();
        manager.record_message(&Message::user("hi")).await.unwrap();
        manager.finish().unwrap();
        manager.finish().unwrap();

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        let summary_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"summary""#))
            .count();
        assert_eq!(summary_count, 1);
    }

    #[tokio::test]
    async fn finish_skips_summary_on_empty_resume() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("hello"))
            .await
            .unwrap();
        original.finish().unwrap();
        drop(original);

        let mut resumed = SessionManager::resume(&store, &session_id)
            .await
            .unwrap()
            .manager;
        resumed.finish().unwrap();
        drop(resumed);

        let content = std::fs::read_to_string(test_session_file(dir.path(), &session_id)).unwrap();
        let summary_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"summary""#))
            .count();
        assert_eq!(summary_count, 1);
    }

    // ── take_ai_title_seed ──

    #[tokio::test]
    async fn take_ai_title_seed_yields_first_prompt_once_on_fresh_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m");

        // No messages recorded yet → no seed.
        assert!(manager.take_ai_title_seed().is_none());

        // First user message seeds; second call drains to None.
        manager
            .record_message(&Message::user("Fix login bug"))
            .await
            .unwrap();
        assert_eq!(
            manager.take_ai_title_seed().as_deref(),
            Some("Fix login bug")
        );
        assert!(manager.take_ai_title_seed().is_none());

        // Subsequent user messages don't re-seed.
        manager
            .record_message(&Message::user("follow up"))
            .await
            .unwrap();
        assert!(manager.take_ai_title_seed().is_none());
    }

    #[tokio::test]
    async fn take_ai_title_seed_is_empty_on_resume_even_when_file_has_first_prompt_only() {
        // Resumed sessions already have a first-prompt title on disk;
        // we don't try to regenerate the AI title here. (If the original
        // run's AI generation failed, resume still skips — AI titles are
        // one-shot per session.)
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m");
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("hello"))
            .await
            .unwrap();
        drop(original);

        let mut resumed = SessionManager::resume(&store, &session_id)
            .await
            .unwrap()
            .manager;
        assert!(resumed.take_ai_title_seed().is_none());
    }

    // ── record_tool_result_metadata ──

    #[tokio::test]
    async fn record_tool_result_metadata_round_trips_title_and_replacements() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m");
        let sid = manager.session_id().to_owned();
        // Materialize the session file first; the metadata entry
        // writer needs an open `SessionWriter`.
        manager
            .record_message(&Message::user("edit something"))
            .await
            .unwrap();

        manager
            .record_tool_result_metadata(
                "t1",
                &ToolMetadata {
                    title: Some("Edited f.rs".to_owned()),
                    replacements: Some(4),
                    ..ToolMetadata::default()
                },
            )
            .unwrap();
        manager.finish().unwrap();
        drop(manager);

        let data = store.load_session_data(&sid).unwrap();
        let metadata = data
            .tool_result_metadata
            .get("t1")
            .expect("metadata entry should round-trip");
        assert_eq!(metadata.title.as_deref(), Some("Edited f.rs"));
        assert_eq!(metadata.replacements, Some(4));
    }

    #[tokio::test]
    async fn record_tool_result_metadata_skips_default_metadata() {
        // Don't emit a sidecar for tools that attached nothing —
        // otherwise every bash call would bloat the transcript with
        // empty `{"type":"tool_result_metadata"}` lines.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m");
        let sid = manager.session_id().to_owned();
        manager
            .record_message(&Message::user("trigger"))
            .await
            .unwrap();

        manager
            .record_tool_result_metadata("t1", &ToolMetadata::default())
            .unwrap();
        manager.finish().unwrap();
        drop(manager);

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        assert!(
            !content.contains(r#""type":"tool_result_metadata""#),
            "default metadata must not be written: {content}",
        );
    }

    #[tokio::test]
    async fn record_tool_result_metadata_errors_before_session_file_materializes() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m");
        let err = manager
            .record_tool_result_metadata(
                "t1",
                &ToolMetadata {
                    title: Some("x".to_owned()),
                    ..ToolMetadata::default()
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("materialized"), "got: {err}");
    }

    // ── append_ai_title ──

    #[tokio::test]
    async fn append_ai_title_writes_title_entry_and_supersedes_first_prompt_on_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m");
        let sid = manager.session_id().to_owned();
        manager
            .record_message(&Message::user("Fix login bug"))
            .await
            .unwrap();

        manager.append_ai_title("Fix auth flow for mobile").unwrap();
        manager.finish().unwrap();
        drop(manager);

        // Tail scan picks the latest-updated_at title, so the AI title
        // wins over the first-prompt title.
        let sessions = store.list().unwrap();
        let session = sessions.iter().find(|s| s.session_id == sid).unwrap();
        assert_eq!(
            session.title.as_ref().unwrap().title,
            "Fix auth flow for mobile"
        );
    }

    #[tokio::test]
    async fn append_ai_title_errors_before_session_file_materializes() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m");
        let err = manager.append_ai_title("whatever").unwrap_err().to_string();
        assert!(err.contains("materialized"), "got: {err}");
    }

    // ── record_write_failure ──

    #[tokio::test]
    async fn record_write_failure_first_call_returns_true_then_false() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m");
        assert!(
            manager.record_write_failure(),
            "first failure should be reported"
        );
        assert!(
            !manager.record_write_failure(),
            "subsequent failures should be silenced"
        );
        assert!(
            !manager.record_write_failure(),
            "flag is sticky across further calls"
        );
    }

    // ── extract_user_text ──

    #[test]
    fn extract_user_text_from_user_message() {
        let msg = Message::user("hello");
        assert_eq!(extract_user_text(&msg), Some("hello"));
    }

    #[test]
    fn extract_user_text_skips_assistant() {
        let msg = Message::assistant("hello");
        assert_eq!(extract_user_text(&msg), None);
    }

    #[test]
    fn extract_user_text_skips_empty() {
        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "  ".to_owned(),
            }],
        };
        assert_eq!(extract_user_text(&msg), None);
    }

    #[test]
    fn extract_user_text_returns_none_for_tool_result_only() {
        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: "output".to_owned(),
                is_error: false,
            }],
        };
        assert_eq!(extract_user_text(&msg), None);
    }

    #[test]
    fn extract_user_text_finds_text_after_tool_result() {
        let msg = Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".to_owned(),
                    content: "output".to_owned(),
                    is_error: false,
                },
                ContentBlock::Text {
                    text: "follow-up".to_owned(),
                },
            ],
        };
        assert_eq!(extract_user_text(&msg), Some("follow-up"));
    }

    // ── truncate_title ──

    #[test]
    fn truncate_title_short_string_unchanged() {
        assert_eq!(truncate_title("hello world", 60), "hello world");
    }

    #[test]
    fn truncate_title_exact_max_len_unchanged() {
        let s = "a".repeat(60);
        assert_eq!(truncate_title(&s, 60), s);
    }

    #[test]
    fn truncate_title_long_string_adds_ellipsis() {
        let long = "a".repeat(100);
        let result = truncate_title(&long, 20);
        assert_eq!(result, format!("{}...", "a".repeat(17)));
    }

    #[test]
    fn truncate_title_multibyte_respects_character_count() {
        let s = "\u{00e9}".repeat(61);
        let result = truncate_title(&s, 60);
        // Exact char count: 57 é + "..." = 60, not "<= 60".
        assert_eq!(result.chars().count(), 60);
        assert_eq!(
            result,
            format!("{}...", "\u{00e9}".repeat(57)),
            "truncated body should be 57 é followed by ellipsis",
        );
    }

    #[test]
    fn truncate_title_empty_string() {
        assert_eq!(truncate_title("", 60), "");
    }

    #[test]
    fn truncate_title_takes_first_line_only() {
        assert_eq!(truncate_title("first line\nsecond line", 60), "first line");
    }

    #[test]
    fn truncate_title_trims_whitespace() {
        assert_eq!(truncate_title("  padded  ", 60), "padded");
    }
}
