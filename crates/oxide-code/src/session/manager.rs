use std::collections::HashSet;

use anyhow::{Result, bail};
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

use super::entry::{CURRENT_VERSION, Entry, TitleSource};
use super::store::{SessionStore, SessionWriter};
use crate::message::{ContentBlock, Message, Role, strip_trailing_thinking};

/// Maximum title length (in characters) derived from the first user prompt.
///
/// Sized for wide terminals: the `--list` row is `ID(8) Last Active(16)
/// Msgs(6) Title`, so ~80 chars of title space on a 120-col terminal
/// and still truncates cleanly (`...`) on narrower ones.
const MAX_TITLE_LEN: usize = 80;

/// Synthetic assistant content injected when resume detects a trailing
/// user turn with only `tool_results` (i.e., the previous run crashed
/// between writing the `tool_result` message and the next assistant
/// response). Keeps role alternation valid for the next API call.
const RESUME_CONTINUATION_SENTINEL: &str = "[Previous turn was interrupted; continuing.]";

// ── SessionManager ──

/// High-level session lifecycle, owned by the agent loop.
///
/// Wraps a [`SessionWriter`] to provide a simple record-oriented API:
/// start or resume a session, record each message, and write a summary
/// on exit.
pub(crate) struct SessionManager {
    writer: SessionWriter,
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
    /// UUID of the last recorded [`Entry::Message`]. Used as
    /// `parent_uuid` for the next recorded message, forming the
    /// conversation chain.
    last_message_uuid: Option<Uuid>,
    finished: bool,
    /// Set the first time a [`record_message`][Self::record_message] or
    /// [`finish`][Self::finish] call returns an error. Lets callers
    /// surface the failure to the user exactly once per session —
    /// subsequent failures warn-log only so we don't spam the UI with
    /// repeated disk-full / permission errors mid-conversation.
    write_failed: bool,
}

impl SessionManager {
    /// Start a new session. Writes the header entry immediately.
    pub(crate) fn start(store: &SessionStore, model: &str) -> Result<Self> {
        let (session_id, header) = new_header(model);
        let writer = store.create(&header)?;

        Ok(Self {
            writer,
            session_id,
            initial_message_count: 0,
            message_count: 0,
            first_user_prompt: None,
            last_message_uuid: None,
            finished: false,
            write_failed: false,
        })
    }

    /// Resume a previous session. Loads its messages, sanitizes them to
    /// a resumable state (drops unresolved `tool_use` blocks and pairs
    /// orphan `tool_result` turns with a sentinel), and reopens the
    /// existing session file in append mode.
    ///
    /// Note: there is a small TOCTOU window between `load_session_data`
    /// (read, no lock) and `open_append` (lock). Another process could
    /// append messages in between, making the loaded messages stale.
    /// In practice this is a non-issue for a single-user CLI tool — the
    /// lock still prevents concurrent *writers*, just not a reader
    /// seeing the latest state before acquiring the lock.
    pub(crate) fn resume(store: &SessionStore, session_id: &str) -> Result<(Self, Vec<Message>)> {
        let mut data = store.load_session_data(session_id)?;
        sanitize_resumed_messages(&mut data.messages);
        // Run the emptiness check *after* sanitization. Otherwise a
        // file that loads into a non-empty vector but becomes empty
        // after sanitize (all unresolved tool_use + orphan tool_result,
        // or sanitization dropped every turn) would slip through with
        // `last_message_uuid = data.last_uuid`, and the next recorded
        // message would chain to a UUID that's no longer in view.
        if data.messages.is_empty() {
            bail!("session {session_id} has no messages to resume");
        }

        let writer = store.open_append(session_id)?;

        let first_user_prompt = data
            .messages
            .iter()
            .find_map(extract_user_text)
            .map(String::from);
        let message_count = u32::try_from(data.messages.len()).unwrap_or(u32::MAX);

        let manager = Self {
            writer,
            session_id: session_id.to_owned(),
            initial_message_count: message_count,
            message_count,
            first_user_prompt,
            last_message_uuid: data.last_uuid,
            finished: false,
            write_failed: false,
        };
        Ok((manager, data.messages))
    }

    /// Record a conversation message to the session file.
    ///
    /// On the first user message that carries text, writes an initial
    /// [`Entry::Title`] *before* the [`Entry::Message`] — so listings
    /// show the correct title even if the process crashes before any
    /// further progress.
    pub(crate) fn record_message(&mut self, message: &Message) -> Result<()> {
        let now = OffsetDateTime::now_utc();

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
            self.writer.append(&Entry::Title {
                title: truncate_title(text, MAX_TITLE_LEN),
                source: TitleSource::FirstPrompt,
                updated_at: now,
            })?;
        }

        let uuid = Uuid::new_v4();
        self.writer.append(&Entry::Message {
            uuid,
            parent_uuid: self.last_message_uuid,
            message: message.clone(),
            timestamp: now,
        })?;
        self.last_message_uuid = Some(uuid);
        self.message_count = self.message_count.saturating_add(1);
        Ok(())
    }

    /// Write the summary entry. No-op if already called or if this is a
    /// resumed session and no new messages were recorded (avoids
    /// accumulating duplicate summaries on empty resume cycles).
    pub(crate) fn finish(&mut self) -> Result<()> {
        if self.finished
            || (self.initial_message_count > 0 && self.message_count == self.initial_message_count)
        {
            return Ok(());
        }
        self.finished = true;

        self.writer.append(&Entry::Summary {
            message_count: self.message_count,
            updated_at: OffsetDateTime::now_utc(),
        })
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
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

/// Extract the first non-empty text content from a user message.
fn extract_user_text(message: &Message) -> Option<&str> {
    if message.role != Role::User {
        return None;
    }
    message.content.iter().find_map(|b| match b {
        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })
}

/// Truncate a title to `max_len` characters, adding "..." if truncated.
fn truncate_title(s: &str, max_len: usize) -> String {
    let trimmed = s.lines().next().unwrap_or(s).trim();
    if trimmed.chars().count() <= max_len {
        trimmed.to_owned()
    } else {
        let boundary = trimmed
            .char_indices()
            .nth(max_len.saturating_sub(3))
            .map_or(trimmed.len(), |(i, _)| i);
        format!("{}...", &trimmed[..boundary])
    }
}

// ── Resume Sanitization ──

/// Normalize a loaded conversation to a state the API will accept as
/// the prefix of a new turn.
///
/// Fixes common crash-induced inconsistencies:
///
/// 1. Drops trailing `thinking` / `redacted_thinking` blocks (API
///    rejects assistant messages that end with thinking).
/// 2. Drops unresolved `tool_use` blocks — assistant tool calls that
///    never received a matching `tool_result`. Happens when the
///    process crashed between `tool_use` stream end and tool execution
///    (or between tool execution and `tool_result` write).
/// 3. Drops orphan `tool_result` blocks — user `tool_result`s whose
///    `tool_use_id` does not match any surviving assistant `tool_use`.
///    Happens when a corrupted JSONL line drops a `tool_use` during
///    load (or when step 2 removes one), leaving its paired
///    `tool_result` pointing at nothing. The API rejects orphan
///    `tool_result`s, so the symmetric filter keeps the transcript
///    valid.
/// 4. Drops messages that became empty after (2) or (3).
/// 5. Collapses adjacent same-role messages left over after (4). The
///    API requires strict user / assistant alternation; dropping an
///    assistant turn with only unresolved `tool_use`, or a user turn
///    with only orphan `tool_result`, can leave two same-role turns
///    adjacent. Merging their content preserves every block while
///    restoring alternation.
/// 6. Appends a synthetic assistant sentinel when the last remaining
///    message is a user turn containing only `tool_result` blocks — the
///    crash window between writing `tool_results` and the next assistant
///    response. Prevents two-user-turns-in-a-row on the next API call.
fn sanitize_resumed_messages(messages: &mut Vec<Message>) {
    strip_trailing_thinking(messages);

    // tool_use_ids for which any tool_result exists somewhere in the log.
    let resolved_ids: HashSet<String> = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect();

    for msg in &mut *messages {
        if msg.role == Role::Assistant {
            msg.content.retain(|b| match b {
                ContentBlock::ToolUse { id, .. } | ContentBlock::ServerToolUse { id, .. } => {
                    resolved_ids.contains(id)
                }
                _ => true,
            });
        }
    }

    // Symmetric pass: collect the tool_use ids that actually survived
    // the assistant filter above, then drop user tool_results whose id
    // isn't in that set. An orphan tool_result would fail API validation.
    let surviving_tool_use_ids: HashSet<String> = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } | ContentBlock::ServerToolUse { id, .. } => {
                Some(id.clone())
            }
            _ => None,
        })
        .collect();

    for msg in &mut *messages {
        if msg.role == Role::User {
            msg.content.retain(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    surviving_tool_use_ids.contains(tool_use_id)
                }
                _ => true,
            });
        }
    }

    messages.retain(|m| !m.content.is_empty());
    collapse_consecutive_same_role(messages);

    if let Some(last) = messages.last()
        && last.role == Role::User
        && last
            .content
            .iter()
            .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
    {
        messages.push(Message::assistant(RESUME_CONTINUATION_SENTINEL));
    }

    strip_trailing_thinking(messages);
}

/// Merge every pair of consecutive same-role messages by extending the
/// earlier message's content with the later one's and dropping the
/// later one. Called after filtering / drop passes so that sanitization
/// can never leave the transcript with two user or two assistant
/// messages in a row.
fn collapse_consecutive_same_role(messages: &mut Vec<Message>) {
    let mut i = 0;
    while i + 1 < messages.len() {
        if messages[i].role == messages[i + 1].role {
            let next = messages.remove(i + 1);
            messages[i].content.extend(next.content);
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const TEST_PROJECT: &str = "test-project";

    fn test_store(dir: &Path) -> SessionStore {
        SessionStore::open_at(dir.to_path_buf(), TEST_PROJECT).unwrap()
    }

    /// Resolve the on-disk path of a session file inside [`TEST_PROJECT`]
    /// by its session ID. Files are named `{epoch}-{session_id}.jsonl`,
    /// so we look up by the `-{session_id}.jsonl` suffix.
    fn test_session_file(dir: &Path, session_id: &str) -> std::path::PathBuf {
        let project_dir = dir.join(TEST_PROJECT);
        let suffix = format!("-{session_id}.jsonl");
        for entry in std::fs::read_dir(&project_dir).unwrap().flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(&suffix) {
                return entry.path();
            }
        }
        panic!("no session file matching {suffix} in {project_dir:?}");
    }

    // ── start ──

    #[test]
    fn start_creates_session_file_with_zero_count() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::start(&test_store(dir.path()), "test-model").unwrap();
        let path = test_session_file(dir.path(), manager.session_id());
        assert!(path.exists());
        assert_eq!(manager.message_count, 0);
        assert!(manager.last_message_uuid.is_none());
    }

    // ── resume ──

    #[test]
    fn resume_loads_messages_and_keeps_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m").unwrap();
        let session_id = original.session_id().to_owned();
        original.record_message(&Message::user("hello")).unwrap();
        original.record_message(&Message::assistant("hi")).unwrap();
        original.finish().unwrap();
        drop(original);

        let (resumed, messages) = SessionManager::resume(&store, &session_id).unwrap();
        assert_eq!(resumed.session_id(), session_id);
        assert_eq!(messages.len(), 2);
        assert_eq!(resumed.message_count, 2);
        assert!(resumed.last_message_uuid.is_some());
    }

    #[test]
    fn resume_works_on_unfinished_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m").unwrap();
        let session_id = original.session_id().to_owned();
        original.record_message(&Message::user("hello")).unwrap();
        drop(original); // no finish() — simulates a crash

        let (resumed, messages) = SessionManager::resume(&store, &session_id).unwrap();
        assert_eq!(resumed.session_id(), session_id);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn resume_empty_session_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m").unwrap();
        let session_id = original.session_id().to_owned();
        original.finish().unwrap();
        drop(original);

        assert!(SessionManager::resume(&store, &session_id).is_err());
    }

    #[test]
    fn resume_drops_unresolved_trailing_tool_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m").unwrap();
        let session_id = original.session_id().to_owned();
        original.record_message(&Message::user("do X")).unwrap();
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
            .unwrap();
        drop(original); // crash before tool_result

        let (_resumed, messages) = SessionManager::resume(&store, &session_id).unwrap();
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

    #[test]
    fn resume_drops_assistant_message_with_only_unresolved_tool_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m").unwrap();
        let session_id = original.session_id().to_owned();
        original.record_message(&Message::user("do X")).unwrap();
        original
            .record_message(&Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "unresolved".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::Value::Null,
                }],
            })
            .unwrap();
        drop(original);

        let (_resumed, messages) = SessionManager::resume(&store, &session_id).unwrap();
        assert_eq!(
            messages.len(),
            1,
            "assistant-only-tool-use should be dropped"
        );
        assert_eq!(messages[0].role, Role::User);
    }

    #[test]
    fn resume_errors_when_sanitize_empties_transcript() {
        // Single assistant message with nothing but an unresolved
        // tool_use — sanitize drops it, leaving an empty vec. The
        // pre-fix code accepted this but kept `last_message_uuid =
        // Some(dropped_uuid)`, so the next recorded message would
        // parent-chain to a UUID no longer present in memory.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m").unwrap();
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
            .unwrap();
        drop(original);

        let result = SessionManager::resume(&store, &session_id);
        let err = match result {
            Ok(_) => panic!("expected resume to bail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("no messages to resume"), "got: {err}");
    }

    #[test]
    fn resume_appends_sentinel_when_last_is_user_tool_results_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m").unwrap();
        let session_id = original.session_id().to_owned();
        original.record_message(&Message::user("do X")).unwrap();
        original
            .record_message(&Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::Value::Null,
                }],
            })
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
            .unwrap();
        drop(original); // crash before next assistant response

        let (_resumed, messages) = SessionManager::resume(&store, &session_id).unwrap();
        assert_eq!(messages.len(), 4, "sentinel should be appended");
        assert_eq!(messages[3].role, Role::Assistant);
        assert!(
            matches!(&messages[3].content[0], ContentBlock::Text { text } if text == RESUME_CONTINUATION_SENTINEL)
        );
    }

    #[test]
    fn resume_preserves_parent_chain_on_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m").unwrap();
        let session_id = original.session_id().to_owned();
        original.record_message(&Message::user("hello")).unwrap();
        original.record_message(&Message::assistant("hi")).unwrap();
        drop(original);

        let (mut resumed, _) = SessionManager::resume(&store, &session_id).unwrap();
        resumed.record_message(&Message::user("follow up")).unwrap();

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

    #[test]
    fn resume_appends_and_updates_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m").unwrap();
        let session_id = original.session_id().to_owned();
        original
            .record_message(&Message::user("Fix the auth bug"))
            .unwrap();
        original.finish().unwrap();
        drop(original);

        let (mut resumed, _) = SessionManager::resume(&store, &session_id).unwrap();
        resumed
            .record_message(&Message::assistant("Done."))
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

    #[test]
    fn record_message_increments_count_and_chains_parent() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m").unwrap();
        let sid = manager.session_id().to_owned();

        manager.record_message(&Message::user("hello")).unwrap();
        manager.record_message(&Message::assistant("hi")).unwrap();
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

    #[test]
    fn record_message_writes_title_before_first_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m").unwrap();
        let sid = manager.session_id().to_owned();
        manager
            .record_message(&Message::user("First prompt"))
            .unwrap();

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        // Line 0: header. Line 1: title. Line 2: message.
        assert!(lines[1].contains(r#""type":"title""#));
        assert!(lines[2].contains(r#""type":"message""#));
    }

    #[test]
    fn record_message_writes_title_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m").unwrap();
        let sid = manager.session_id().to_owned();

        manager.record_message(&Message::user("first")).unwrap();
        manager.record_message(&Message::user("second")).unwrap();

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        let title_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"title""#))
            .count();
        assert_eq!(title_count, 1);
        assert_eq!(manager.first_user_prompt.as_deref(), Some("first"));
    }

    #[test]
    fn record_message_no_title_for_tool_result_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m").unwrap();
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
            .unwrap();

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        assert!(!content.contains(r#""type":"title""#));
    }

    // ── finish ──

    #[test]
    fn finish_writes_summary_with_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m").unwrap();
        let sid = manager.session_id().to_owned();

        manager
            .record_message(&Message::user("Fix the auth bug"))
            .unwrap();
        manager.finish().unwrap();

        let sessions = store.list().unwrap();
        let session = sessions.iter().find(|s| s.session_id == sid).unwrap();
        assert_eq!(session.title.as_ref().unwrap().title, "Fix the auth bug");
        assert_eq!(session.exit.as_ref().unwrap().message_count, 1);
    }

    #[test]
    fn finish_empty_session_writes_summary_without_title() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m").unwrap();
        let sid = manager.session_id().to_owned();
        manager.finish().unwrap();

        let sessions = store.list().unwrap();
        let session = sessions.iter().find(|s| s.session_id == sid).unwrap();
        assert!(session.title.is_none(), "no user prompt means no title");
        assert_eq!(session.exit.as_ref().unwrap().message_count, 0);
    }

    #[test]
    fn finish_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m").unwrap();
        let sid = manager.session_id().to_owned();
        manager.record_message(&Message::user("hi")).unwrap();
        manager.finish().unwrap();
        manager.finish().unwrap();

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        let summary_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"summary""#))
            .count();
        assert_eq!(summary_count, 1);
    }

    #[test]
    fn finish_skips_summary_on_empty_resume() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m").unwrap();
        let session_id = original.session_id().to_owned();
        original.record_message(&Message::user("hello")).unwrap();
        original.finish().unwrap();
        drop(original);

        let (mut resumed, _) = SessionManager::resume(&store, &session_id).unwrap();
        resumed.finish().unwrap();
        drop(resumed);

        let content = std::fs::read_to_string(test_session_file(dir.path(), &session_id)).unwrap();
        let summary_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"summary""#))
            .count();
        assert_eq!(summary_count, 1);
    }

    // ── record_write_failure ──

    #[test]
    fn record_write_failure_first_call_returns_true_then_false() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m").unwrap();
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
        assert!(result.chars().count() <= 60);
        assert!(result.ends_with("..."));
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

    // ── sanitize_resumed_messages ──

    #[test]
    fn sanitize_noop_for_clean_transcript() {
        let mut messages = vec![
            Message::user("hello"),
            Message::assistant("hi"),
            Message::user("bye"),
        ];
        let before = messages.len();
        sanitize_resumed_messages(&mut messages);
        assert_eq!(messages.len(), before);
    }

    #[test]
    fn sanitize_pairs_tool_use_with_result() {
        let mut messages = vec![
            Message::user("do X"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "checking".to_owned(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".to_owned(),
                        name: "bash".to_owned(),
                        input: serde_json::Value::Null,
                    },
                    ContentBlock::ToolUse {
                        id: "t2".to_owned(),
                        name: "bash".to_owned(),
                        input: serde_json::Value::Null,
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_owned(),
                    content: "ok".to_owned(),
                    is_error: false,
                }],
            },
        ];
        sanitize_resumed_messages(&mut messages);

        // Assistant has text + t1 (resolved), t2 dropped.
        let assistant_blocks = &messages[1].content;
        assert_eq!(assistant_blocks.len(), 2);
        assert!(matches!(&assistant_blocks[0], ContentBlock::Text { .. }));
        assert!(matches!(&assistant_blocks[1], ContentBlock::ToolUse { id, .. } if id == "t1"));
        // Last message is still user with tool_result → sentinel appended.
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3].role, Role::Assistant);
    }

    #[test]
    fn sanitize_drops_orphan_tool_result_block_and_keeps_siblings() {
        // User turn has one tool_result with no matching tool_use (the
        // preceding assistant only produced text). The orphan block
        // should be dropped; the sibling text should survive.
        let mut messages = vec![
            Message::user("do X"),
            Message::assistant("done, no tool needed"),
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "orphan".to_owned(),
                        content: "ghost".to_owned(),
                        is_error: false,
                    },
                    ContentBlock::Text {
                        text: "follow-up".to_owned(),
                    },
                ],
            },
        ];
        sanitize_resumed_messages(&mut messages);

        assert_eq!(messages.len(), 3);
        let last = &messages[2];
        assert_eq!(last.role, Role::User);
        assert_eq!(last.content.len(), 1);
        assert!(matches!(&last.content[0], ContentBlock::Text { text } if text == "follow-up"));
    }

    #[test]
    fn sanitize_drops_user_message_with_only_orphan_tool_result() {
        // The user turn contains nothing but an orphan tool_result;
        // once the orphan is dropped, the message is empty and the
        // whole turn is removed.
        let mut messages = vec![
            Message::user("do X"),
            Message::assistant("all clear"),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "ghost".to_owned(),
                    content: "nobody asked".to_owned(),
                    is_error: false,
                }],
            },
        ];
        sanitize_resumed_messages(&mut messages);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
    }

    #[test]
    fn sanitize_collapses_adjacent_users_after_empty_assistant_drop() {
        // Unresolved tool_use is the assistant's only content, so the
        // assistant message is dropped; the two surrounding user turns
        // would then be adjacent and invalid without the collapse pass.
        let mut messages = vec![
            Message::user("do X"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "unresolved".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::Value::Null,
                }],
            },
            Message::user("and now Y"),
        ];
        sanitize_resumed_messages(&mut messages);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].content.len(), 2);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "do X"));
        assert!(
            matches!(&messages[0].content[1], ContentBlock::Text { text } if text == "and now Y")
        );
    }

    #[test]
    fn sanitize_collapses_adjacent_assistants_after_orphan_user_drop() {
        // User turn is an orphan tool_result surrounded by two
        // assistant text turns. The orphan filter empties the user;
        // retain drops it; without collapse we'd have two adjacent
        // assistants.
        let mut messages = vec![
            Message::user("do X"),
            Message::assistant("first answer"),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "ghost".to_owned(),
                    content: "stale".to_owned(),
                    is_error: false,
                }],
            },
            Message::assistant("second answer"),
        ];
        sanitize_resumed_messages(&mut messages);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        let answer = &messages[1].content;
        assert_eq!(answer.len(), 2);
        assert!(matches!(&answer[0], ContentBlock::Text { text } if text == "first answer"));
        assert!(matches!(&answer[1], ContentBlock::Text { text } if text == "second answer"));
    }

    #[test]
    fn sanitize_drops_orphan_when_assistant_tool_use_was_dropped() {
        // Mismatched IDs: the user tool_result references "t2", but the
        // assistant never had "t2". Step 2 drops the assistant's "t1"
        // (unresolved), leaving no surviving tool_use ids; step 3 then
        // drops the user's "t2" tool_result as orphan. Text sibling on
        // the assistant survives so both turns remain.
        let mut messages = vec![
            Message::user("do X"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "checking".to_owned(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".to_owned(),
                        name: "bash".to_owned(),
                        input: serde_json::Value::Null,
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t2".to_owned(),
                    content: "stale".to_owned(),
                    is_error: false,
                }],
            },
        ];
        sanitize_resumed_messages(&mut messages);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        // Assistant text survives, tool_use "t1" dropped.
        let assistant = &messages[1].content;
        assert_eq!(assistant.len(), 1);
        assert!(matches!(&assistant[0], ContentBlock::Text { text } if text == "checking"));
    }

    // ── collapse_consecutive_same_role ──

    #[test]
    fn collapse_consecutive_same_role_merges_runs_and_preserves_alternation() {
        // Mixed input: a run of three assistants, then a lone user,
        // then a run of two users. The helper should leave exactly
        // assistant → user after merging both runs.
        let mut messages = vec![
            Message::assistant("a1"),
            Message::assistant("a2"),
            Message::assistant("a3"),
            Message::user("u1"),
            Message::user("u2"),
        ];
        collapse_consecutive_same_role(&mut messages);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::Assistant);
        assert_eq!(messages[0].content.len(), 3);
        assert_eq!(messages[1].role, Role::User);
        assert_eq!(messages[1].content.len(), 2);
    }

    #[test]
    fn collapse_consecutive_same_role_noop_on_alternating_transcript() {
        let mut messages = vec![
            Message::user("u"),
            Message::assistant("a"),
            Message::user("u2"),
        ];
        collapse_consecutive_same_role(&mut messages);

        assert_eq!(messages.len(), 3);
    }
}
