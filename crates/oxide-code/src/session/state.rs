//! In-memory session state owned by the actor. Pure data — no tokio; I/O hides behind
//! [`WriterStatus`] so lifecycle transitions test without a runtime.

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

/// Char cap for first-prompt-derived titles. Sized so a 120-col `--list` row still has room
/// after the fixed `ID / Last Active / Msgs` prefix.
const MAX_TITLE_LEN: usize = 80;

// ── SessionState ──

/// Pure-data lifecycle owned by [`super::actor::run`]. All I/O happens through
/// [`SessionWriter`] held inside [`WriterStatus`]; the rest is bookkeeping the actor mutates
/// between batches. Never shared across tasks — the actor is the sole owner.
pub(super) struct SessionState {
    pub(super) session_id: Arc<str>,
    store: SessionStore,
    writer_status: WriterStatus,
    last_message_uuid: Option<Uuid>,
    /// Loaded count on resume (`0` for fresh). `finish_entries` skips the summary when unchanged.
    initial_message_count: u32,
    message_count: u32,
    /// Latched on first user-text so we don't emit a duplicate title entry.
    first_user_prompt_seen: bool,
    finished: bool,
}

/// Writer lifecycle.
///
/// `Pending` defers `create + header write` until the first non-empty flush — a session that
/// exits without recording leaves nothing on disk. `deferred_title` holds the most recent
/// `/rename` (last-wins) and rides out alongside the header on first promotion, or vanishes
/// with the actor. `Broken` is set after a partial write so the next batch reopens via
/// `open_append` instead of trusting an undefined `BufWriter`.
enum WriterStatus {
    Pending {
        header: Entry,
        deferred_title: Option<String>,
    },
    Active(SessionWriter),
    Broken,
}

impl SessionState {
    pub(super) fn fresh(store: SessionStore, model: &str) -> Self {
        let (session_id, header) = new_header(model);
        Self {
            session_id: Arc::from(session_id),
            store,
            writer_status: WriterStatus::Pending {
                header,
                deferred_title: None,
            },
            last_message_uuid: None,
            initial_message_count: 0,
            message_count: 0,
            first_user_prompt_seen: false,
            finished: false,
        }
    }

    /// Resumed sessions land directly in `Active`. Caller pre-ORs `first_user_prompt_seen` with
    /// "title already on disk" so the next message doesn't push a duplicate first-prompt entry.
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

    /// Queues `title` to flush as a `UserProvided` entry on first `Pending` → `Active`
    /// promotion. Returns `Err(title)` when the writer is already `Active` / `Broken` so the
    /// caller can route into the live batch instead. A second deferral overwrites the first.
    pub(super) fn try_defer_title(&mut self, title: String) -> Result<(), String> {
        let WriterStatus::Pending { deferred_title, .. } = &mut self.writer_status else {
            return Err(title);
        };
        *deferred_title = Some(title);
        Ok(())
    }

    /// Builds entries for one message; returns AI-title seed on first user-text.
    /// `manual_title_set` skips both the `FirstPrompt` push and the AI-title seed.
    pub(super) fn queue_message_entries(
        &mut self,
        message: &Message,
        now: OffsetDateTime,
        manual_title_set: bool,
    ) -> (Vec<Entry>, Option<String>) {
        let mut entries: Vec<Entry> = Vec::with_capacity(2);
        let mut ai_title_seed: Option<String> = None;

        if !self.first_user_prompt_seen
            && let Some(text) = extract_user_text(message)
        {
            // Latch before the push so a later flush failure won't produce a duplicate.
            self.first_user_prompt_seen = true;
            // `/rename` already queued the UserProvided title; skip the FirstPrompt + AI seed.
            if !manual_title_set {
                ai_title_seed = Some(text.to_owned());
                entries.push(Entry::Title {
                    title: truncate_title(text, MAX_TITLE_LEN),
                    source: TitleSource::FirstPrompt,
                    updated_at: now,
                });
            }
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
        // Writer may still be Pending in a batched Finish; key off message_count instead.
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

    /// Writes entries in one flush; transitions to Broken on failure so next batch reopens.
    /// On first `Pending` → `Active` promotion, any deferred title flushes ahead of `entries`
    /// as a `UserProvided` entry stamped at flush time.
    pub(super) fn flush_entries(&mut self, entries: &[Entry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let (mut writer, deferred_title) = self.take_or_open_writer()?;
        let result = (|| -> Result<()> {
            if let Some(title) = &deferred_title {
                writer.append_no_flush(&Entry::Title {
                    title: title.clone(),
                    source: TitleSource::UserProvided,
                    updated_at: OffsetDateTime::now_utc(),
                })?;
            }
            for entry in entries {
                writer.append_no_flush(entry)?;
            }
            writer.flush()
        })();
        self.writer_status = match result {
            Ok(()) => WriterStatus::Active(writer),
            Err(_) => WriterStatus::Broken,
        };
        result
    }

    /// Returns the writer plus any title deferred while `Pending`. On `Pending` failure the
    /// header AND deferred title are restored so the next batch retries `create`.
    fn take_or_open_writer(&mut self) -> Result<(SessionWriter, Option<String>)> {
        match std::mem::replace(&mut self.writer_status, WriterStatus::Broken) {
            WriterStatus::Active(w) => Ok((w, None)),
            WriterStatus::Pending {
                header,
                deferred_title,
            } => match self.store.create(&header) {
                Ok(w) => Ok((w, deferred_title)),
                Err(e) => {
                    self.writer_status = WriterStatus::Pending {
                        header,
                        deferred_title,
                    };
                    Err(e)
                }
            },
            WriterStatus::Broken => Ok((self.store.open_append(&self.session_id)?, None)),
        }
    }
}

// ── Helpers ──

fn new_header(model: &str) -> (String, Entry) {
    let session_id = Uuid::new_v4().to_string();
    let cwd = current_dir_string();
    // Skipped under `cfg(test)` so byte-compatible JSONL snapshots and seeded fixtures don't
    // depend on the working tree's branch — every test site that needs a non-`None` branch
    // supplies its own fixture via direct `Entry::Header` construction.
    let git_branch = if cfg!(test) {
        None
    } else {
        current_git_branch(&cwd)
    };
    let header = Entry::Header {
        session_id: session_id.clone(),
        cwd,
        model: model.to_owned(),
        created_at: OffsetDateTime::now_utc(),
        version: CURRENT_VERSION,
        git_branch,
    };
    (session_id, header)
}

/// Best-effort branch name via `git rev-parse --abbrev-ref HEAD`. Returns `None` when not in a
/// repo, when git is missing, or when HEAD is detached (returned as the literal `HEAD` — surfaced
/// as `None` so the metadata column doesn't show a useless `· HEAD`).
fn current_git_branch(cwd: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_git_branch(output.status.success(), &output.stdout)
}

/// Pure parser for `git rev-parse --abbrev-ref HEAD` output. Split out from the shell-out so the
/// success / detached-HEAD / invalid-UTF-8 branches can be exercised without a fixture repo.
fn parse_git_branch(success: bool, stdout: &[u8]) -> Option<String> {
    if !success {
        return None;
    }
    let branch = std::str::from_utf8(stdout).ok()?.trim();
    if branch.is_empty() || branch == "HEAD" {
        return None;
    }
    Some(branch.to_owned())
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

/// Truncates a title to `max_len` characters, appending [`ELLIPSIS`] when truncated.
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

        let (entries, seed) =
            state.queue_message_entries(&Message::user("Fix the auth bug"), now, false);

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
        _ = state.queue_message_entries(&Message::user("first"), now, false);
        let first_tip = state.last_message_uuid.unwrap();

        let (entries, seed) = state.queue_message_entries(&Message::user("second"), now, false);

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

        let (entries, seed) = state.queue_message_entries(&msg, now, false);

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

        let (entries, seed) = state.queue_message_entries(&Message::assistant("hi"), now, false);

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
        let (entries, _) = state.queue_message_entries(&Message::user("hello"), now, false);
        state.flush_entries(&entries).unwrap();

        let entries = state.finish_entries(Vec::new(), now);
        let Some(Entry::Summary { message_count, .. }) = entries.last() else {
            panic!("expected trailing Summary, got {entries:?}");
        };
        assert_eq!(*message_count, 1);
    }

    #[test]
    fn finish_entries_pending_writer_is_empty_and_marks_finished() {
        // Nothing recorded → nothing to summarize, but `finished` still latches so a later
        // record cmd no-ops cleanly.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");

        let entries = state.finish_entries(Vec::new(), OffsetDateTime::now_utc());

        assert!(entries.is_empty());
        assert!(state.finished);
    }

    #[test]
    fn finish_entries_emits_one_file_snapshot_per_tracked_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let tracker = FileTracker::default();
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();
        let (msgs, _) = state.queue_message_entries(&Message::user("hi"), now, false);
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
        let (entries, _) = state.queue_message_entries(&Message::user("hi"), now, false);
        state.flush_entries(&entries).unwrap();
        let _first = state.finish_entries(Vec::new(), now);

        let second = state.finish_entries(Vec::new(), now);

        assert!(second.is_empty(), "second call short-circuits");
        assert!(state.finished);
    }

    #[test]
    fn finish_entries_skips_summary_on_empty_resume() {
        // Without this short-circuit, every noisy resume would append another summary line.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut original = SessionState::fresh(store.clone(), "m");
        let now = OffsetDateTime::now_utc();
        let (entries, _) = original.queue_message_entries(&Message::user("hi"), now, false);
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

    // `/dev/full` drives a real ENOSPC on every write — Linux-only, so the test gates rather
    // than smuggle in a failing-writer trait.
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

        let (entries, _) = state.queue_message_entries(&Message::user("hi"), now, false);
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

        let (entries, _) = state.queue_message_entries(&Message::user("first"), now, false);
        state.flush_entries(&entries).unwrap();
        assert!(matches!(state.writer_status, WriterStatus::Active(_)));
        state.writer_status = WriterStatus::Broken;

        let (entries, _) = state.queue_message_entries(&Message::user("second"), now, false);
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
    fn flush_entries_pending_create_failure_keeps_pending_and_preserves_deferred_title() {
        // The next batch must retry create rather than open_append a file that was never
        // created — and the deferred title must survive into the retry. A regression that
        // restored `Pending { deferred_title: None }` on rollback would silently drop the
        // user's `/rename`.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut state = SessionState::fresh(store, "m");
        let now = OffsetDateTime::now_utc();
        let session_id = state.session_id.to_string();
        state
            .try_defer_title("Survives rollback".to_owned())
            .expect("fresh state must accept defer");

        let project_dir = super::super::store::test_project_dir(dir.path());
        std::fs::remove_dir_all(&project_dir).unwrap();
        let (entries, _) = state.queue_message_entries(&Message::user("first"), now, false);
        let result = state.flush_entries(&entries);

        assert!(result.is_err(), "create must fail with project dir gone");
        let WriterStatus::Pending {
            deferred_title: Some(restored),
            ..
        } = &state.writer_status
        else {
            panic!("create failure must leave Pending with deferred title intact");
        };
        assert_eq!(
            restored, "Survives rollback",
            "deferred title must survive rollback verbatim",
        );

        std::fs::create_dir_all(&project_dir).unwrap();
        state.flush_entries(&entries).expect("retry succeeds");
        let path = super::super::store::test_session_file(dir.path(), &session_id);
        let content = std::fs::read_to_string(path).unwrap();
        assert!(
            content.contains("Survives rollback"),
            "deferred title reaches disk on retry: {content}",
        );
    }

    // ── current_git_branch ──

    #[test]
    fn current_git_branch_in_a_real_repo_returns_the_branch_name() {
        // Skipped silently if `git` isn't on PATH so CI without git doesn't fail — production
        // path correctly returns `None` in that case. An empty repo's rev-parse would return the
        // literal `HEAD`, so we make a commit first.
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let Ok(status) = std::process::Command::new("git")
            .args(["init", "-q", "-b", "fixture-branch"])
            .current_dir(cwd)
            .status()
        else {
            return;
        };
        if !status.success() {
            return;
        }
        for args in [
            ["config", "user.email", "test@example.com"].as_slice(),
            ["config", "user.name", "Test"].as_slice(),
            ["config", "commit.gpgsign", "false"].as_slice(),
            ["commit", "-q", "--allow-empty", "-m", "init"].as_slice(),
        ] {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap();
        }
        assert_eq!(
            current_git_branch(cwd),
            Some("fixture-branch".to_owned()),
            "branch should round-trip after the initial commit on the requested branch"
        );
    }

    #[test]
    fn current_git_branch_outside_a_repo_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(current_git_branch(dir.path().to_str().unwrap()), None);
    }

    // ── parse_git_branch ──

    #[test]
    fn parse_git_branch_keeps_branch_names_and_drops_everything_else() {
        // Trailing newline trimmed so the metadata column doesn't render `\n`.
        assert_eq!(
            parse_git_branch(true, b"feat/login\n"),
            Some("feat/login".to_owned())
        );
        // Non-zero exit (not-a-repo, missing git, ...) collapses to None.
        assert_eq!(parse_git_branch(false, b"main\n"), None);
        assert_eq!(parse_git_branch(true, &[0xff, 0xfe, b'\n']), None);
        assert_eq!(parse_git_branch(true, b""), None);
        assert_eq!(parse_git_branch(true, b"   \n"), None);
        // `HEAD` is rev-parse's detached-HEAD output — useless in the picker.
        assert_eq!(parse_git_branch(true, b"HEAD\n"), None);
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
    fn extract_user_text_returns_first_non_empty_text_from_user_role_only() {
        let tool_result = ContentBlock::ToolResult {
            tool_use_id: "t1".to_owned(),
            content: "output".to_owned(),
            is_error: false,
        };
        let user_with_only_blank = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "  ".to_owned(),
            }],
        };
        let user_with_only_tool_result = Message {
            role: Role::User,
            content: vec![tool_result.clone()],
        };
        let user_with_text_after_tool_result = Message {
            role: Role::User,
            content: vec![
                tool_result,
                ContentBlock::Text {
                    text: "follow-up".to_owned(),
                },
            ],
        };

        assert_eq!(extract_user_text(&Message::user("hello")), Some("hello"));
        assert_eq!(extract_user_text(&Message::assistant("hello")), None);
        assert_eq!(extract_user_text(&user_with_only_blank), None);
        assert_eq!(extract_user_text(&user_with_only_tool_result), None);
        assert_eq!(
            extract_user_text(&user_with_text_after_tool_result),
            Some("follow-up"),
        );
    }

    // ── truncate_title ──

    #[test]
    fn truncate_title_returns_at_most_max_len_chars_after_first_line_trim() {
        assert_eq!(truncate_title("hello world", 60), "hello world");
        assert_eq!(truncate_title("", 60), "");
        assert_eq!(truncate_title("first line\nsecond line", 60), "first line");
        assert_eq!(truncate_title("  padded  ", 60), "padded");

        let exact = "a".repeat(60);
        assert_eq!(truncate_title(&exact, 60), exact);

        let long = "a".repeat(100);
        assert_eq!(truncate_title(&long, 20), format!("{}...", "a".repeat(17)));
    }

    #[test]
    fn truncate_title_multibyte_respects_character_count() {
        // Exact char count: 57 é + "..." = 60, not "<= 60".
        let s = "\u{00e9}".repeat(61);
        let result = truncate_title(&s, 60);
        assert_eq!(result.chars().count(), 60);
        assert_eq!(result, format!("{}...", "\u{00e9}".repeat(57)));
    }
}
