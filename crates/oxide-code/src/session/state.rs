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
use crate::file_tracker::FileSnapshot;
use crate::message::{ContentBlock, Message, Role};
use crate::util::text::{ELLIPSIS, ELLIPSIS_WIDTH};

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
    store: SessionStore,
    writer_status: WriterStatus,
    /// `parent_uuid` for the next recorded message.
    last_message_uuid: Option<Uuid>,
    /// Loaded message count for resumed sessions; `0` for fresh.
    /// `finish_entries` skips the summary when nothing was added.
    initial_message_count: u32,
    message_count: u32,
    /// Latched the first time a user-text message lands so we don't
    /// re-promote a later user message to a duplicate title.
    first_user_prompt_seen: bool,
    finished: bool,
}

/// Writer lifecycle: lazy-create, healthy, or poisoned after a partial write.
enum WriterStatus {
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

    /// Builds entries for one message; returns AI-title seed on first user-text.
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

    /// Builds snapshot + summary closing entries, or empty vec for no-op finish.
    pub(super) fn finish_entries(
        &mut self,
        snapshots: Vec<FileSnapshot>,
        now: OffsetDateTime,
    ) -> Vec<Entry> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        // message_count rather than writer status — writer may still be Pending in a
        // batched Finish.
        if self.message_count == 0 {
            return Vec::new();
        }
        if self.initial_message_count > 0 && self.message_count == self.initial_message_count {
            return Vec::new();
        }
        let mut entries: Vec<Entry> = snapshots
            .into_iter()
            .map(|snapshot| Entry::FileSnapshot { snapshot })
            .collect();
        entries.push(Entry::Summary {
            message_count: self.message_count,
            updated_at: now,
        });
        entries
    }

    /// Writes entries in one flush; transitions writer on failure for next-batch retry.
    pub(super) fn flush_entries(&mut self, entries: &[Entry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut writer = self.take_or_open_writer()?;
        let result = (|| -> Result<()> {
            for entry in entries {
                writer.append_no_flush(entry)?;
            }
            writer.flush()
        })();
        // `BufWriter`'s buffer is undefined after a partial write — flag
        // Broken so the next batch reopens instead of poisoning the flush.
        self.writer_status = match result {
            Ok(()) => WriterStatus::Active(writer),
            Err(_) => WriterStatus::Broken,
        };
        result
    }

    /// Returns a writer, transitioning from Pending or Broken as needed.
    fn take_or_open_writer(&mut self) -> Result<SessionWriter> {
        // Temporarily swap with Broken to take ownership.
        match std::mem::replace(&mut self.writer_status, WriterStatus::Broken) {
            WriterStatus::Active(w) => Ok(w),
            WriterStatus::Pending { header } => match self.store.create(&header) {
                Ok(w) => Ok(w),
                Err(e) => {
                    self.writer_status = WriterStatus::Pending { header };
                    Err(e)
                }
            },
            WriterStatus::Broken => self.store.open_append(&self.session_id),
        }
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
    format_current_dir(std::env::current_dir())
}

fn format_current_dir(result: std::io::Result<std::path::PathBuf>) -> String {
    match result {
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

/// Truncates a title to `max_len` characters, appending [`ELLIPSIS`]
/// when truncated.
///
/// `max_len` must be at least `ELLIPSIS_WIDTH + 1` (room for the marker
/// plus at least one character of the title). Only internal callers
/// drive this with [`MAX_TITLE_LEN`] = 80, so the precondition is a
/// sanity check, not user input handling.
fn truncate_title(s: &str, max_len: usize) -> String {
    debug_assert!(
        max_len > ELLIPSIS_WIDTH,
        "truncate_title: max_len must exceed ELLIPSIS_WIDTH",
    );
    let trimmed = s.lines().next().unwrap_or(s).trim();
    if trimmed.chars().count() <= max_len {
        trimmed.to_owned()
    } else {
        let boundary = trimmed
            .char_indices()
            .nth(max_len - ELLIPSIS_WIDTH)
            .map_or(trimmed.len(), |(i, _)| i);
        format!("{}{ELLIPSIS}", &trimmed[..boundary])
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::test_store;
    use super::*;
    use crate::file_tracker::FileTracker;

    // ── queue_message_entries ──

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

    // ── finish_entries ──

    #[test]
    fn finish_entries_after_record_produces_summary_with_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();
        let (entries, _) = state.queue_message_entries(&Message::user("hello"), now);
        state.flush_entries(&entries).unwrap();

        let entries = state.finish_entries(Vec::new(), now);
        let Some(Entry::Summary { message_count, .. }) = entries.last() else {
            panic!("expected trailing Summary, got {entries:?}");
        };
        assert_eq!(*message_count, 1);
    }

    #[test]
    fn finish_entries_pending_writer_is_empty_and_marks_finished() {
        // Nothing recorded → nothing to summarize, but `finished` still
        // latches so a later record cmd no-ops cleanly.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");

        let entries = state.finish_entries(Vec::new(), OffsetDateTime::now_utc());

        assert!(entries.is_empty());
        assert!(state.finished);
    }

    #[test]
    fn finish_entries_emits_one_file_snapshot_per_tracked_file() {
        // Three snapshots in → three FileSnapshot entries out, ahead
        // of the trailing Summary. The caller drains the tracker
        // before sending the cmd; the actor only writes them to disk.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let tracker = FileTracker::default();
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();
        let (msgs, _) = state.queue_message_entries(&Message::user("hi"), now);
        state.flush_entries(&msgs).unwrap();

        for name in ["/tmp/a", "/tmp/b", "/tmp/c"] {
            tracker.record_modify(
                std::path::Path::new(name),
                name.as_bytes(),
                std::time::UNIX_EPOCH,
                3,
            );
        }

        let entries = state.finish_entries(tracker.snapshot_all(), now);
        let snapshot_count = entries
            .iter()
            .filter(|e| matches!(e, Entry::FileSnapshot { .. }))
            .count();
        assert_eq!(snapshot_count, 3);
        assert!(
            matches!(entries.last(), Some(Entry::Summary { .. })),
            "summary must come after every snapshot",
        );
    }

    #[test]
    fn finish_entries_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();
        let (entries, _) = state.queue_message_entries(&Message::user("hi"), now);
        state.flush_entries(&entries).unwrap();
        let _first = state.finish_entries(Vec::new(), now);

        let second = state.finish_entries(Vec::new(), now);

        assert!(second.is_empty(), "second call short-circuits");
        assert!(state.finished);
    }

    #[test]
    fn finish_entries_skips_summary_on_empty_resume() {
        // A summary per noisy resume would accumulate one line per cycle.
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

        let entries = resumed.finish_entries(Vec::new(), now);

        assert!(entries.is_empty(), "no new messages → no summary");
    }

    // ── flush_entries ──

    // `/dev/full` is the cheapest way to drive a real flush failure on
    // an Active writer: open succeeds, every write returns ENOSPC. Linux
    // exposes it; macOS and Windows do not, so the test gates on Linux
    // rather than smuggling in a custom failing-writer trait.
    #[cfg(target_os = "linux")]
    #[test]
    fn flush_entries_active_writer_flush_failure_transitions_to_broken() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();
        let writer = super::super::store::open_append_at(std::path::Path::new("/dev/full"))
            .expect("/dev/full must be openable on Linux");
        state.writer_status = WriterStatus::Active(writer);

        let (entries, _) = state.queue_message_entries(&Message::user("hi"), now);
        let result = state.flush_entries(&entries);

        assert!(result.is_err(), "flush to /dev/full must surface ENOSPC");
        assert!(
            matches!(state.writer_status, WriterStatus::Broken),
            "flush failure on Active writer must transition to Broken",
        );
    }

    #[test]
    fn flush_entries_broken_writer_reopens_file_and_appends() {
        // Reopen must hit the existing file, not create a fresh one.
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
        // The next batch must retry create rather than open_append a
        // file that was never created.
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

    // ── format_current_dir ──

    #[test]
    fn format_current_dir_ok_renders_path() {
        let path = std::path::PathBuf::from("/tmp/x");
        assert_eq!(format_current_dir(Ok(path)), "/tmp/x");
    }

    #[test]
    fn format_current_dir_err_falls_back_to_unknown() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        assert_eq!(format_current_dir(Err(err)), "<unknown>");
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
    fn extract_user_text_is_none_for_tool_result_only() {
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
