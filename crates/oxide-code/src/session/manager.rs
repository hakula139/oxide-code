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
/// Wraps a [`SessionStore`] and [`SessionWriter`] to provide a simple
/// record-oriented API: start or resume a session, record each message,
/// and write a summary on exit.
pub(crate) struct SessionManager {
    writer: SessionWriter,
    session_id: String,
    message_count: u32,
    /// Captured from the first user message for the session title.
    first_user_prompt: Option<String>,
}

impl SessionManager {
    /// Start a new session. Writes the header entry immediately.
    pub(crate) fn start(store: &SessionStore, model: &str) -> Result<Self> {
        let (session_id, header) = new_header(None, model);
        let writer = store.create(&header)?;

        Ok(Self {
            writer,
            session_id,
            message_count: 0,
            first_user_prompt: None,
        })
    }

    /// Resume a previous session. Loads its messages and starts a new
    /// session file with a `parent_id` link.
    pub(crate) fn resume(
        store: &SessionStore,
        parent_id: &str,
        model: &str,
    ) -> Result<(Self, Vec<Message>)> {
        let messages = store.load_messages(parent_id)?;
        if messages.is_empty() {
            bail!("session {parent_id} has no messages to resume");
        }

        let (session_id, header) = new_header(Some(parent_id), model);
        let writer = store.create(&header)?;

        // Capture title from the first user message of the parent session.
        let first_user_prompt = messages
            .iter()
            .find_map(extract_user_text)
            .map(String::from);
        let message_count = u32::try_from(messages.len()).unwrap_or(u32::MAX);

        let manager = Self {
            writer,
            session_id,
            message_count,
            first_user_prompt,
        };
        Ok((manager, messages))
    }

    /// Record a conversation message to the session file.
    pub(crate) fn record_message(&mut self, message: &Message) -> Result<()> {
        // Capture the first user prompt for the session title.
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

    /// Write the summary entry. Call this before the session ends.
    pub(crate) fn finish(&mut self) -> Result<()> {
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

fn new_header(parent_id: Option<&str>, model: &str) -> (String, Entry) {
    let session_id = Uuid::new_v4().to_string();
    let header = Entry::Header {
        session_id: session_id.clone(),
        parent_id: parent_id.map(str::to_owned),
        cwd: current_dir_string(),
        model: model.to_owned(),
        created_at: OffsetDateTime::now_utc(),
    };
    (session_id, header)
}

fn current_dir_string() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Extract the first text content from a user message.
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
    fn start_creates_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::start(&test_store(dir.path()), "test-model").unwrap();
        let path = dir.path().join(format!("{}.jsonl", manager.session_id()));
        assert!(path.exists());
    }

    #[test]
    fn start_initializes_with_zero_count() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::start(&test_store(dir.path()), "claude-opus-4-6").unwrap();
        assert_eq!(manager.message_count, 0);
    }

    // ── resume ──

    #[test]
    fn resume_loads_parent_messages_and_creates_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut original = SessionManager::start(&test_store(dir.path()), "m").unwrap();
        let parent_id = original.session_id().to_owned();
        original.record_message(&Message::user("hello")).unwrap();
        original.record_message(&Message::assistant("hi")).unwrap();
        original.finish().unwrap();

        let (resumed, messages) =
            SessionManager::resume(&test_store(dir.path()), &parent_id, "m").unwrap();
        assert_ne!(resumed.session_id(), parent_id);
        assert_eq!(messages.len(), 2);
        assert_eq!(resumed.message_count, 2);
    }

    #[test]
    fn resume_empty_session_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut original = SessionManager::start(&test_store(dir.path()), "m").unwrap();
        let parent_id = original.session_id().to_owned();
        original.finish().unwrap();

        assert!(SessionManager::resume(&test_store(dir.path()), &parent_id, "m").is_err());
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
        assert_eq!(session.title.as_deref(), Some("Fix the auth bug"));
        assert_eq!(session.message_count, Some(1));
    }

    #[test]
    fn finish_empty_session_uses_placeholder_title() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m").unwrap();
        let sid = manager.session_id().to_owned();
        manager.finish().unwrap();

        let sessions = test_store(dir.path()).list().unwrap();
        let session = sessions.iter().find(|s| s.session_id == sid).unwrap();
        assert_eq!(session.title.as_deref(), Some("(empty session)"));
        assert_eq!(session.message_count, Some(0));
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
        assert!(result.chars().count() <= 20);
        assert!(result.ends_with("..."));
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
