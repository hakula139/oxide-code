use anyhow::{Result, bail};
use time::OffsetDateTime;
use uuid::Uuid;

use super::entry::Entry;
use super::store::{SessionStore, SessionWriter};
use crate::message::{ContentBlock, Message};

/// Maximum title length (in characters) derived from the first user prompt.
const MAX_TITLE_LEN: usize = 60;

// ── SessionManager ──

/// High-level session lifecycle, owned by the agent loop.
///
/// Wraps a [`SessionWriter`] to provide a simple record-oriented API:
/// start or resume a session, record each message, and write a summary
/// on exit.
pub(crate) struct SessionManager {
    writer: SessionWriter,
    session_id: String,
    message_count: u32,
    /// Captured from the first user message for the session title.
    first_user_prompt: Option<String>,
    finished: bool,
}

impl SessionManager {
    /// Start a new session. Writes the header entry immediately.
    pub(crate) fn start(store: &SessionStore, model: &str) -> Result<Self> {
        let (session_id, header) = new_header(model);
        let writer = store.create(&header)?;

        Ok(Self {
            writer,
            session_id,
            message_count: 0,
            first_user_prompt: None,
            finished: false,
        })
    }

    /// Resume a previous session. Loads its messages and reopens the
    /// existing session file in append mode.
    pub(crate) fn resume(store: &SessionStore, session_id: &str) -> Result<(Self, Vec<Message>)> {
        let messages = store.load_messages(session_id)?;
        if messages.is_empty() {
            bail!("session {session_id} has no messages to resume");
        }

        let writer = store.open_append(session_id)?;

        let first_user_prompt = messages
            .iter()
            .find_map(extract_user_text)
            .map(String::from);
        let message_count = u32::try_from(messages.len()).unwrap_or(u32::MAX);

        let manager = Self {
            writer,
            session_id: session_id.to_owned(),
            message_count,
            first_user_prompt,
            finished: false,
        };
        Ok((manager, messages))
    }

    /// Record a conversation message to the session file.
    pub(crate) fn record_message(&mut self, message: &Message) -> Result<()> {
        if self.first_user_prompt.is_none()
            && let Some(text) = extract_user_text(message)
        {
            self.first_user_prompt = Some(text.to_owned());
        }

        self.writer.append(&Entry::Message {
            message: message.clone(),
            timestamp: OffsetDateTime::now_utc(),
        })?;
        self.message_count = self.message_count.saturating_add(1);
        Ok(())
    }

    /// Write the summary entry. No-op if already called.
    pub(crate) fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;

        let title = self.first_user_prompt.as_deref().map_or_else(
            || "(empty session)".to_owned(),
            |s| truncate_title(s, MAX_TITLE_LEN),
        );

        self.writer.append(&Entry::Summary {
            title,
            updated_at: OffsetDateTime::now_utc(),
            message_count: self.message_count,
        })
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
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
    };
    (session_id, header)
}

fn current_dir_string() -> String {
    match std::env::current_dir() {
        Ok(p) => p.display().to_string(),
        Err(e) => {
            tracing::warn!("failed to read current directory: {e}");
            String::new()
        }
    }
}

/// Extract the first non-empty text content from a user message.
fn extract_user_text(message: &Message) -> Option<&str> {
    if message.role != crate::message::Role::User {
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn test_store(dir: &Path) -> SessionStore {
        // Bypass XDG resolution for tests.
        SessionStore::open_at(dir.to_path_buf()).unwrap()
    }

    // ── start ──

    #[test]
    fn start_creates_session_file_with_zero_count() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::start(&test_store(dir.path()), "test-model").unwrap();
        let path = dir.path().join(format!("{}.jsonl", manager.session_id()));
        assert!(path.exists());
        assert_eq!(manager.message_count, 0);
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
        drop(original); // release file lock

        let (resumed, messages) = SessionManager::resume(&store, &session_id).unwrap();
        assert_eq!(resumed.session_id(), session_id);
        assert_eq!(messages.len(), 2);
        assert_eq!(resumed.message_count, 2);
    }

    #[test]
    fn resume_works_on_unfinished_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionManager::start(&store, "m").unwrap();
        let session_id = original.session_id().to_owned();
        original.record_message(&Message::user("hello")).unwrap();
        // No finish() — simulates a crash.
        drop(original);

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
    fn resume_appends_to_existing_file_and_updates_summary() {
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

        // The tail scanner finds the latest summary.
        let sessions = store.list().unwrap();
        let session = sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .unwrap();
        let summary = session.summary.as_ref().unwrap();
        assert_eq!(summary.title, "Fix the auth bug");
        assert_eq!(summary.message_count, 2); // 1 original + 1 new

        // All messages (original + appended) are in the same file.
        let all_messages = store.load_messages(&session_id).unwrap();
        assert_eq!(all_messages.len(), 2);
    }

    // ── record_message ──

    #[test]
    fn record_message_increments_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m").unwrap();
        manager.record_message(&Message::user("hello")).unwrap();
        manager.record_message(&Message::assistant("hi")).unwrap();
        assert_eq!(manager.message_count, 2);
    }

    #[test]
    fn record_message_captures_first_user_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m").unwrap();

        manager.record_message(&Message::user("first")).unwrap();
        manager.record_message(&Message::user("second")).unwrap();

        assert_eq!(manager.first_user_prompt.as_deref(), Some("first"));
    }

    // ── finish ──

    #[test]
    fn finish_writes_summary_with_title_from_first_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m").unwrap();
        let sid = manager.session_id().to_owned();

        manager
            .record_message(&Message::user("Fix the auth bug"))
            .unwrap();
        manager.finish().unwrap();

        let store = test_store(dir.path());
        let sessions = store.list().unwrap();
        let session = sessions.iter().find(|s| s.session_id == sid).unwrap();
        let summary = session.summary.as_ref().unwrap();
        assert_eq!(summary.title, "Fix the auth bug");
        assert_eq!(summary.message_count, 1);
    }

    #[test]
    fn finish_empty_session_uses_placeholder_title() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m").unwrap();
        let sid = manager.session_id().to_owned();
        manager.finish().unwrap();

        let sessions = test_store(dir.path()).list().unwrap();
        let session = sessions.iter().find(|s| s.session_id == sid).unwrap();
        let summary = session.summary.as_ref().unwrap();
        assert_eq!(summary.title, "(empty session)");
        assert_eq!(summary.message_count, 0);
    }

    #[test]
    fn finish_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut manager = SessionManager::start(&store, "m").unwrap();
        let sid = manager.session_id().to_owned();
        manager.record_message(&Message::user("hi")).unwrap();
        manager.finish().unwrap();
        manager.finish().unwrap(); // second call is a no-op

        // Only one summary entry should exist in the file.
        let content = std::fs::read_to_string(dir.path().join(format!("{sid}.jsonl"))).unwrap();
        let summary_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"summary""#))
            .count();
        assert_eq!(summary_count, 1);
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
            role: crate::message::Role::User,
            content: vec![ContentBlock::Text {
                text: "  ".to_owned(),
            }],
        };
        assert_eq!(extract_user_text(&msg), None);
    }

    #[test]
    fn extract_user_text_returns_none_for_tool_result_only() {
        let msg = Message {
            role: crate::message::Role::User,
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
            role: crate::message::Role::User,
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
        // 17 a's + "..." = 20 characters exactly.
        assert_eq!(result, format!("{}...", "a".repeat(17)));
    }

    #[test]
    fn truncate_title_multibyte_respects_character_count() {
        // 61 two-byte characters → should truncate to 57 chars + "...".
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
}
