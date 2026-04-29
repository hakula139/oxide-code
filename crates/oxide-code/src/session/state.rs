//! In-memory session state owned by the [`super::actor`] task.
//!
//! Pure data: no tokio, file I/O hidden behind [`WriterStatus`]. Split
//! out so lifecycle transitions (first-prompt detection, uuid chain,
//! finish idempotency) test without a runtime.

use std::sync::Arc;

use anyhow::Result;
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

use super::entry::{CURRENT_VERSION, Entry, TitleSource};
use super::store::{SessionStore, SessionWriter};
use crate::message::{ContentBlock, Message, Role};

/// Maximum title length (in characters) derived from the first user prompt.
///
/// Sized for wide terminals: the `--list` row is `ID(10) Last Active(19)
/// Msgs(6) Title`, so ~80 chars of title space on a 120-col terminal
/// and still truncates cleanly (`...`) on narrower ones.
const MAX_TITLE_LEN: usize = 80;

// ── SessionState ──

pub(super) struct SessionState {
    /// Cloned out by `SessionHandle::session_id` so `&str` deref stays
    /// off the cmd channel.
    pub(super) session_id: Arc<str>,
    /// Cloned at start so the first append can drive `Pending → Active`.
    pub(super) store: SessionStore,
    pub(super) writer_status: WriterStatus,
    /// `parent_uuid` for the next recorded message.
    pub(super) last_message_uuid: Option<Uuid>,
    /// Loaded message count for resumed sessions; `0` for fresh.
    /// `finish_entry` skips the summary when nothing was added.
    pub(super) initial_message_count: u32,
    pub(super) message_count: u32,
    /// Latched the first time a user-text message lands so we don't
    /// re-promote a later user message to a duplicate title.
    pub(super) first_user_prompt_seen: bool,
    pub(super) finished: bool,
}

/// Lifecycle states for the underlying file:
///
/// - `Pending` — header staged, file not yet on disk. A fresh session
///   that exits before the first record cmd leaves no file behind.
/// - `Active` — file open, [`BufWriter`][std::io::BufWriter] healthy.
///   Steady state.
/// - `Broken` — last batch errored mid-flush. `BufWriter`'s buffer
///   state is undefined per the std docs after a partial-write
///   failure, so we drop the writer and reopen the file with
///   [`SessionStore::open_append`] on the next batch. Without this,
///   a single transient I/O hiccup poisons every subsequent flush.
pub(super) enum WriterStatus {
    Pending { header: Entry },
    Active(SessionWriter),
    Broken,
}

impl SessionState {
    pub(super) fn fresh(store: SessionStore, model: &str) -> Self {
        let (session_id, header) = new_header(model);
        Self {
            session_id: Arc::from(session_id),
            store,
            writer_status: WriterStatus::Pending { header },
            last_message_uuid: None,
            initial_message_count: 0,
            message_count: 0,
            first_user_prompt_seen: false,
            finished: false,
        }
    }

    /// Resumed sessions land directly in `Active` because the loader
    /// already had to read the file.
    pub(super) fn resumed(
        store: SessionStore,
        session_id: String,
        writer: SessionWriter,
        last_message_uuid: Option<Uuid>,
        initial_message_count: u32,
        first_user_prompt_seen: bool,
    ) -> Self {
        Self {
            session_id: Arc::from(session_id),
            store,
            writer_status: WriterStatus::Active(writer),
            last_message_uuid,
            initial_message_count,
            message_count: initial_message_count,
            first_user_prompt_seen,
            finished: false,
        }
    }

    /// Build the entries one `record_message` would emit and update
    /// bookkeeping. Returns the AI-title seed only on a fresh session's
    /// first user-text message. Pure transform; flushing is the
    /// caller's job.
    pub(super) fn queue_message_entries(
        &mut self,
        message: &Message,
        now: OffsetDateTime,
    ) -> (Vec<Entry>, Option<String>) {
        let mut entries: Vec<Entry> = Vec::with_capacity(2);
        let mut ai_title_seed: Option<String> = None;

        if !self.first_user_prompt_seen
            && let Some(text) = extract_user_text(message)
        {
            // Latch the flag before the title push — if the later flush
            // fails we still won't promote the next user message to a
            // duplicate title.
            self.first_user_prompt_seen = true;
            ai_title_seed = Some(text.to_owned());
            entries.push(Entry::Title {
                title: truncate_title(text, MAX_TITLE_LEN),
                source: TitleSource::FirstPrompt,
                updated_at: now,
            });
        }

        let uuid = Uuid::new_v4();
        entries.push(Entry::Message {
            uuid,
            parent_uuid: self.last_message_uuid,
            message: message.clone(),
            timestamp: now,
        });
        self.last_message_uuid = Some(uuid);
        self.message_count = self.message_count.saturating_add(1);

        (entries, ai_title_seed)
    }

    /// Build the summary entry, or `None` for a no-op finish (already
    /// finished, nothing recorded, or resumed session that added
    /// nothing). Latches `finished = true` either way.
    pub(super) fn finish_entry(&mut self, now: OffsetDateTime) -> Option<Entry> {
        if self.finished {
            return None;
        }
        self.finished = true;
        // Check `message_count`, not `writer_status`: a `Record + Finish`
        // batch defers materialization to `flush_entries`, so the writer
        // is still `Pending` when this runs.
        if self.message_count == 0 {
            return None;
        }
        if self.initial_message_count > 0 && self.message_count == self.initial_message_count {
            return None;
        }
        Some(Entry::Summary {
            message_count: self.message_count,
            updated_at: now,
        })
    }

    /// Buffer every entry then flush once. Materializes or reopens the
    /// file as needed; a transient open / header / flush failure leaves
    /// the writer in a state that retries cleanly on the next batch.
    pub(super) fn flush_entries(&mut self, entries: &[Entry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        if !matches!(self.writer_status, WriterStatus::Active(_)) {
            // `Pending` materializes the file via the staged header;
            // `Broken` reopens the existing file in append mode. A
            // failure here propagates with the original status preserved
            // — the next batch retries.
            let writer = match &self.writer_status {
                WriterStatus::Pending { header } => self.store.create(header)?,
                WriterStatus::Broken => self.store.open_append(&self.session_id)?,
                WriterStatus::Active(_) => unreachable!("matched above"),
            };
            self.writer_status = WriterStatus::Active(writer);
        }
        let WriterStatus::Active(writer) = &mut self.writer_status else {
            unreachable!("writer_status is Active after materialization above");
        };
        let result = (|| -> Result<()> {
            for entry in entries {
                writer.append_no_flush(entry)?;
            }
            writer.flush()
        })();
        if result.is_err() {
            // `BufWriter` semantics on partial write are undefined; the
            // safe move is to drop the writer and reopen for the next
            // batch instead of feeding more entries into a poisoned
            // buffer.
            self.writer_status = WriterStatus::Broken;
        }
        result
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
pub(super) fn extract_user_text(message: &Message) -> Option<&str> {
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
    use super::super::store::test_store;
    use super::*;

    // ── SessionState::queue_message_entries ──

    #[test]
    fn queue_message_entries_first_user_text_emits_title_then_message_and_seeds_ai_title() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();

        let (entries, seed) = state.queue_message_entries(&Message::user("Fix the auth bug"), now);

        assert_eq!(entries.len(), 2, "title + message");
        assert!(matches!(
            &entries[0],
            Entry::Title { title, .. } if title == "Fix the auth bug",
        ));
        assert!(matches!(&entries[1], Entry::Message { .. }));
        assert_eq!(seed.as_deref(), Some("Fix the auth bug"));
        assert!(state.first_user_prompt_seen);
        assert_eq!(state.message_count, 1);
        assert!(state.last_message_uuid.is_some());
    }

    #[test]
    fn queue_message_entries_subsequent_user_message_skips_title_and_chains_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();
        _ = state.queue_message_entries(&Message::user("first"), now);
        let first_tip = state.last_message_uuid.unwrap();

        let (entries, seed) = state.queue_message_entries(&Message::user("second"), now);

        assert_eq!(entries.len(), 1, "no second title");
        let Entry::Message {
            parent_uuid, uuid, ..
        } = &entries[0]
        else {
            panic!("expected Message, got {:?}", entries[0]);
        };
        assert_eq!(*parent_uuid, Some(first_tip), "chains to previous tip");
        assert_eq!(state.last_message_uuid, Some(*uuid));
        assert!(seed.is_none(), "seed only fires once per session");
        assert_eq!(state.message_count, 2);
    }

    #[test]
    fn queue_message_entries_tool_result_only_user_does_not_seed_title() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();
        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t".to_owned(),
                content: "out".to_owned(),
                is_error: false,
            }],
        };

        let (entries, seed) = state.queue_message_entries(&msg, now);

        assert_eq!(entries.len(), 1, "message only");
        assert!(matches!(&entries[0], Entry::Message { .. }));
        assert!(seed.is_none());
        assert!(
            !state.first_user_prompt_seen,
            "tool-result-only does not consume the first-prompt slot",
        );
    }

    #[test]
    fn queue_message_entries_assistant_message_skips_title() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();

        let (entries, seed) = state.queue_message_entries(&Message::assistant("hi"), now);

        assert_eq!(entries.len(), 1);
        assert!(seed.is_none());
        assert!(!state.first_user_prompt_seen);
    }

    // ── SessionState::flush_entries ──

    #[test]
    fn flush_entries_broken_writer_reopens_file_and_appends() {
        // After a flush error transitions writer_status to Broken, the
        // next flush must reopen the existing file via store.open_append
        // and append cleanly — not lose entries, not start a new file.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store.clone(), "m");
        let now = OffsetDateTime::now_utc();
        let session_id = state.session_id.to_string();

        let (entries, _) = state.queue_message_entries(&Message::user("first"), now);
        state.flush_entries(&entries).unwrap();
        assert!(matches!(state.writer_status, WriterStatus::Active(_)));
        state.writer_status = WriterStatus::Broken;

        let (entries, _) = state.queue_message_entries(&Message::user("second"), now);
        state.flush_entries(&entries).unwrap();

        assert!(
            matches!(state.writer_status, WriterStatus::Active(_)),
            "Broken must transition back to Active after a successful reopen",
        );
        let path = super::super::store::test_session_file(dir.path(), &session_id);
        let content = std::fs::read_to_string(path).unwrap();
        let messages: Vec<&str> = content
            .lines()
            .filter(|l| l.contains(r#""type":"message""#))
            .collect();
        assert_eq!(
            messages.len(),
            2,
            "both messages on disk after reopen: {content}",
        );
    }

    #[test]
    fn flush_entries_pending_create_failure_keeps_pending_for_retry() {
        // A header-write failure must leave WriterStatus::Pending intact
        // so the next batch retries via store.create rather than trying
        // to open a file that was never created.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();
        let project_dir = super::super::store::test_project_dir(dir.path());
        std::fs::remove_dir_all(&project_dir).unwrap();

        let (entries, _) = state.queue_message_entries(&Message::user("first"), now);
        let result = state.flush_entries(&entries);

        assert!(result.is_err(), "create must fail with project dir gone");
        assert!(
            matches!(state.writer_status, WriterStatus::Pending { .. }),
            "create failure must leave Pending intact for next-batch retry",
        );
    }

    // ── SessionState::finish_entry ──

    #[test]
    fn finish_entry_pending_writer_returns_none_and_marks_finished() {
        // No record ever happened, so no file exists yet — there is
        // nothing to summarize. The flag still latches so a later
        // record cmd can no-op cleanly.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");

        let entry = state.finish_entry(OffsetDateTime::now_utc());

        assert!(entry.is_none());
        assert!(state.finished);
    }

    #[test]
    fn finish_entry_after_record_returns_summary_with_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();
        let (entries, _) = state.queue_message_entries(&Message::user("hello"), now);
        state.flush_entries(&entries).unwrap();

        let entry = state.finish_entry(now).expect("Some(Summary)");
        let Entry::Summary { message_count, .. } = entry else {
            panic!("expected Summary, got {entry:?}");
        };
        assert_eq!(message_count, 1);
    }

    #[test]
    fn finish_entry_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();
        let (entries, _) = state.queue_message_entries(&Message::user("hi"), now);
        state.flush_entries(&entries).unwrap();
        let _first = state.finish_entry(now);

        let second = state.finish_entry(now);

        assert!(second.is_none(), "second call short-circuits");
        assert!(state.finished);
    }

    #[test]
    fn finish_entry_skips_summary_on_empty_resume() {
        // Resumed session with no new messages must not write a
        // duplicate summary — would accumulate one line per resume
        // cycle on noisy runs.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionState::fresh(store.clone(), "m");
        let now = OffsetDateTime::now_utc();
        let (entries, _) = original.queue_message_entries(&Message::user("hi"), now);
        original.flush_entries(&entries).unwrap();
        let parent = original.last_message_uuid;
        let session_id = original.session_id.to_string();
        drop(original);

        let writer = store.open_append(&session_id).unwrap();
        let mut resumed = SessionState::resumed(store, session_id, writer, parent, 1, true);

        let entry = resumed.finish_entry(now);

        assert!(entry.is_none(), "no new messages → no summary");
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
