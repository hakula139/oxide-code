//! In-memory session state owned by the actor. It uses no tokio, so lifecycle transitions test
//! without a runtime.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use time::OffsetDateTime;
use uuid::Uuid;

use super::display::fit_title;
use super::entry::{CURRENT_VERSION, CompactBoundary, Entry, Header};
use super::handle::Checkpoint;
use super::store::{SessionStore, SessionWriter, artifact_dir, encode_lines};
use crate::file_tracker::FileSnapshot;
use crate::message::{ContentBlock, Message, Role};

/// Retry-queue size in MiB past which a failed flush stops persistence for the rest of the session.
const MAX_UNWRITTEN_MIB: usize = 8;
pub(super) const MAX_UNWRITTEN_BYTES: usize = MAX_UNWRITTEN_MIB << 20;

fn persistence_stopped() -> String {
    format!(
        "session persistence stopped \
         after more than {MAX_UNWRITTEN_MIB} MiB of entries failed to write"
    )
}

// ── SessionState ──

/// Session lifecycle that [`super::actor::run`] drives, with all I/O going through the
/// [`SessionWriter`] in [`WriterStatus`].
pub(super) struct SessionState {
    pub(super) session_id: Arc<str>,
    /// The [`artifact_dir`] beside the session file, the one a fresh session will create.
    pub(super) artifact_dir: Arc<Path>,
    store: SessionStore,
    writer_status: WriterStatus,
    /// Unwritten entries, retried ahead of the next batch so no message precedes its parent.
    unwritten: Vec<Entry>,
    last_message_uuid: Option<Uuid>,
    /// Loaded count on resume (`0` for fresh). `finish_entries` skips the summary when unchanged.
    initial_message_count: u32,
    message_count: u32,
    /// Whether the session has seen its first user text, after which no message emits another
    /// title entry.
    first_user_prompt_seen: bool,
    finished: bool,
    persistence_stopped: bool,
}

/// Writer lifecycle. `Pending` defers file creation until the first non-empty flush.
enum WriterStatus {
    Pending {
        header: Header,
        deferred_title: Option<String>,
    },
    Active(SessionWriter),
}

impl SessionState {
    pub(super) fn fresh(store: SessionStore, model: &str, git_branch: Option<String>) -> Self {
        let created_at = OffsetDateTime::now_utc();
        let header = new_header(&store, model, git_branch, created_at);
        let session_file = store.session_file(&header.session_id, created_at);
        Self {
            artifact_dir: artifact_dir(&session_file, &header.session_id).into(),
            session_id: Arc::from(header.session_id.as_str()),
            store,
            writer_status: WriterStatus::Pending {
                header,
                deferred_title: None,
            },
            unwritten: Vec::new(),
            last_message_uuid: None,
            initial_message_count: 0,
            message_count: 0,
            first_user_prompt_seen: false,
            finished: false,
            persistence_stopped: false,
        }
    }

    /// A resumed session, writing through `writer` at once. `first_user_prompt_seen` says whether
    /// disk already holds a title or user message.
    pub(super) fn resumed(
        store: SessionStore,
        session_id: String,
        writer: SessionWriter,
        last_message_uuid: Option<Uuid>,
        initial_message_count: u32,
        first_user_prompt_seen: bool,
    ) -> Self {
        Self {
            artifact_dir: artifact_dir(writer.path(), &session_id).into(),
            session_id: Arc::from(session_id),
            store,
            writer_status: WriterStatus::Active(writer),
            unwritten: Vec::new(),
            last_message_uuid,
            initial_message_count,
            message_count: initial_message_count,
            first_user_prompt_seen,
            finished: false,
            persistence_stopped: false,
        }
    }

    /// Parks `title` for the flush that creates the file, or hands it back once the file exists.
    pub(super) fn try_defer_title(&mut self, title: String) -> Option<String> {
        let WriterStatus::Pending { deferred_title, .. } = &mut self.writer_status else {
            return Some(title);
        };
        *deferred_title = Some(title);
        None
    }

    /// Builds entries for one message and returns the AI-title seed on first user text.
    /// The first-prompt title is `title`, or else that text.
    /// `manual_title_set` skips both the first-prompt title and the AI-title seed.
    pub(super) fn queue_message_entries(
        &mut self,
        message: &Message,
        title: Option<&str>,
        now: OffsetDateTime,
        manual_title_set: bool,
    ) -> (Vec<Entry>, Option<String>) {
        let mut entries: Vec<Entry> = Vec::with_capacity(2);
        let mut ai_title_seed: Option<String> = None;

        if !self.first_user_prompt_seen
            && let Some(text) = extract_user_text(message)
        {
            // Latch before queueing anything. A flush failure must not replay this branch.
            self.first_user_prompt_seen = true;
            if !manual_title_set {
                ai_title_seed = Some(text.to_owned());
                entries.push(Entry::Title {
                    title: fit_title(title.unwrap_or(text)),
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

    /// The branch point the next message chains from.
    pub(super) fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            tip: self.last_message_uuid,
            message_count: self.message_count,
            first_user_prompt_seen: self.first_user_prompt_seen,
        }
    }

    /// Moves the chain anchor back to `checkpoint`, returning the entry that records the move. A
    /// rewind to before the first user prompt lets the next one title the session again.
    pub(super) fn rewind_entry(&mut self, checkpoint: Checkpoint, now: OffsetDateTime) -> Entry {
        self.last_message_uuid = checkpoint.tip;
        self.message_count = checkpoint.message_count;
        self.first_user_prompt_seen = checkpoint.first_user_prompt_seen;
        Entry::Rewind {
            tip: checkpoint.tip,
            timestamp: now,
        }
    }

    /// Builds the compaction boundary, the refreshed context entry, and the synthetic post-compact
    /// root message.
    pub(super) fn compact_entries(
        &self,
        boundary: CompactBoundary,
        synthetic_message: Message,
        now: OffsetDateTime,
    ) -> (Vec<Entry>, Uuid) {
        let CompactBoundary {
            summary,
            instructions,
            tasks,
            context,
        } = boundary;
        let synthetic_uuid = Uuid::new_v4();
        let entries = vec![
            Entry::Compact {
                summary,
                pre_message_count: self.message_count,
                instructions,
                tasks,
                timestamp: now,
            },
            Entry::Context {
                context: Box::new(context),
                timestamp: now,
            },
            Entry::Message {
                uuid: synthetic_uuid,
                parent_uuid: None,
                message: synthetic_message,
                timestamp: now,
            },
        ];
        (entries, synthetic_uuid)
    }

    /// Anchors future messages to the post-compact root once its entries are on disk.
    pub(super) fn commit_compact(&mut self, synthetic_uuid: Uuid) {
        self.last_message_uuid = Some(synthetic_uuid);
        self.message_count = 1;
        // The compacted tail is a fresh chain, so finish must write its own closing Summary.
        self.initial_message_count = 0;
        // The synthetic root is user-role but must not seed a duplicate first-prompt title.
        self.first_user_prompt_seen = true;
    }

    /// Unqueues a failed compact so a retry cannot persist a compaction the agent abandoned.
    pub(super) fn abandon_compact(&mut self) {
        self.truncate_from_last(|entry| matches!(entry, Entry::Compact { .. }));
    }

    /// Unqueues a failed archive so a retry cannot hide the session the user stayed in.
    pub(super) fn abandon_archive(&mut self) {
        self.truncate_from_last(|entry| matches!(entry, Entry::Archive { .. }));
    }

    /// Drops the queue from the last entry matching `starts_batch`.
    fn truncate_from_last(&mut self, starts_batch: impl Fn(&Entry) -> bool) {
        if let Some(start) = self.unwritten.iter().rposition(starts_batch) {
            self.unwritten.truncate(start);
        }
    }

    pub(super) fn message_count(&self) -> u32 {
        self.message_count
    }

    pub(super) fn persistence_stopped(&self) -> bool {
        self.persistence_stopped
    }

    /// Builds the snapshot and summary entries that close the session, once, and none when it
    /// recorded no new messages.
    pub(super) fn finish_entries(
        &mut self,
        snapshots: Vec<FileSnapshot>,
        now: OffsetDateTime,
    ) -> Vec<Entry> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        // Writer may still be Pending in a batched Finish. Key off message_count instead.
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

    /// Writes the retry queue, then `entries`, in one append and keeps them all queued on failure.
    /// The flush that creates the file writes the header and any deferred title first. A failure
    /// with more than [`MAX_UNWRITTEN_BYTES`] to write drops the queue and stops persistence.
    pub(super) fn flush_entries(&mut self, entries: Vec<Entry>) -> Result<()> {
        if self.persistence_stopped {
            bail!("{}", persistence_stopped());
        }
        self.unwritten.extend(entries);
        if self.unwritten.is_empty() {
            return Ok(());
        }

        let deferred_title = match &self.writer_status {
            WriterStatus::Pending {
                deferred_title: Some(title),
                ..
            } => Some(Entry::Title {
                title: title.clone(),
                updated_at: OffsetDateTime::now_utc(),
            }),
            _ => None,
        };
        let lines = encode_lines(deferred_title.iter().chain(&self.unwritten))?;
        let result = match &mut self.writer_status {
            WriterStatus::Active(writer) => writer.append_lines(&lines),
            WriterStatus::Pending { header, .. } => self
                .store
                .create(header, &lines)
                .map(|writer| self.writer_status = WriterStatus::Active(writer)),
        };

        if let Err(e) = result {
            if lines.len() > MAX_UNWRITTEN_BYTES {
                self.persistence_stopped = true;
                self.unwritten = Vec::new();
                return Err(e.context(persistence_stopped()));
            }
            return Err(e);
        }
        self.unwritten.clear();
        Ok(())
    }
}

// ── Helpers ──

fn new_header(
    store: &SessionStore,
    model: &str,
    git_branch: Option<String>,
    created_at: OffsetDateTime,
) -> Header {
    let project = store.project();
    Header {
        session_id: Uuid::new_v4().to_string(),
        cwd: project.cwd.clone(),
        project_root: Some(project.root.clone()),
        model: model.to_owned(),
        created_at,
        version: CURRENT_VERSION,
        git_branch,
    }
}

/// Extracts the first non-empty text content from a user message.
pub(super) fn extract_user_text(message: &Message) -> Option<&str> {
    if message.role != Role::User {
        return None;
    }
    message
        .content
        .iter()
        .filter_map(ContentBlock::as_text)
        .find(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_tracker::{FileTracker, LastView};
    use crate::session::handle::testing::boundary;
    use crate::session::store::load_session_data_from_path;
    use crate::session::store::local_testing::test_session_file;
    use crate::session::store::testing::{isolated_store, test_project_dir, test_store};
    use crate::tool::tasks::testing::plan;

    mod try_defer_title {
        use super::*;

        #[test]
        fn pending_accepts_and_overwrites_previous_value() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);

            assert!(state.try_defer_title("first".to_owned()).is_none());
            assert!(state.try_defer_title("second".to_owned()).is_none());

            let WriterStatus::Pending {
                deferred_title: Some(slot),
                ..
            } = &state.writer_status
            else {
                panic!("expected Pending with last-wins deferred title");
            };
            assert_eq!(slot, "second", "later defer overwrites the earlier one");
        }

        #[test]
        fn active_rejects_with_title() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();
            let (entries, _) =
                state.queue_message_entries(&Message::user("first"), None, now, false);
            state.flush_entries(entries).unwrap();
            assert!(matches!(state.writer_status, WriterStatus::Active(_)));

            let rejected = state.try_defer_title("Manual title".to_owned());
            assert_eq!(rejected.as_deref(), Some("Manual title"));
        }
    }

    mod queue_message_entries {
        use super::*;

        #[test]
        fn seeds_ai_title_on_first_user_text() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();

            let (entries, seed) =
                state.queue_message_entries(&Message::user("Fix the auth bug"), None, now, false);

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
        fn titles_the_session_after_a_given_title_and_seeds_the_ai_title_with_the_text() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();

            let (entries, seed) = state.queue_message_entries(
                &Message::user("Create AGENTS.md for this project."),
                Some("/init AGENTS.md"),
                now,
                false,
            );

            assert!(matches!(
                &entries[0],
                Entry::Title { title, .. } if title == "/init AGENTS.md",
            ));
            assert_eq!(seed.as_deref(), Some("Create AGENTS.md for this project."));
        }

        #[test]
        fn subsequent_user_message_skips_title_and_chains_uuid() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();
            _ = state.queue_message_entries(&Message::user("first"), None, now, false);
            let first_tip = state.last_message_uuid.unwrap();

            let (entries, seed) =
                state.queue_message_entries(&Message::user("second"), None, now, false);

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
        fn tool_result_only_user_does_not_seed_title() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();
            let msg = Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("t", "out", false)],
            };

            let (entries, seed) = state.queue_message_entries(&msg, None, now, false);

            assert_eq!(entries.len(), 1, "message only");
            assert!(matches!(&entries[0], Entry::Message { .. }));
            assert!(seed.is_none());
            assert!(
                !state.first_user_prompt_seen,
                "tool-result-only does not consume the first-prompt slot",
            );
        }

        #[test]
        fn assistant_message_skips_title() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();

            let (entries, seed) =
                state.queue_message_entries(&Message::assistant("hi"), None, now, false);

            assert_eq!(entries.len(), 1);
            assert!(seed.is_none());
            assert!(!state.first_user_prompt_seen);
        }
    }

    mod rewind_entry {
        use super::*;

        #[test]
        fn chains_the_next_message_to_the_checkpoint_tip() {
            let dir = tempfile::tempdir().unwrap();
            let mut state = SessionState::fresh(test_store(dir.path()), "m", None);
            let now = OffsetDateTime::now_utc();
            _ = state.queue_message_entries(&Message::user("first"), None, now, false);
            let checkpoint = state.checkpoint();
            _ = state.queue_message_entries(&Message::user("refused"), None, now, false);

            let entry = state.rewind_entry(checkpoint, now);
            let (entries, _) =
                state.queue_message_entries(&Message::user("next"), None, now, false);

            let first = checkpoint.tip.expect("the first message is the tip");
            assert!(matches!(entry, Entry::Rewind { tip: Some(tip), .. } if tip == first));
            assert!(matches!(
                &entries[..],
                [Entry::Message { parent_uuid: Some(parent), .. }] if *parent == first,
            ));
            assert_eq!(state.message_count, 2);
        }

        #[test]
        fn to_before_the_first_prompt_lets_the_next_one_title_the_session() {
            let dir = tempfile::tempdir().unwrap();
            let mut state = SessionState::fresh(test_store(dir.path()), "m", None);
            let now = OffsetDateTime::now_utc();
            let checkpoint = state.checkpoint();
            _ = state.queue_message_entries(&Message::user("refused"), None, now, false);

            _ = state.rewind_entry(checkpoint, now);
            let (entries, seed) =
                state.queue_message_entries(&Message::user("next"), None, now, false);

            assert!(matches!(&entries[0], Entry::Title { title, .. } if title == "next"));
            assert_eq!(seed.as_deref(), Some("next"));
        }
    }

    mod compact_entries {
        use super::*;
        use crate::prompt::SessionContext;

        #[test]
        fn writes_boundary_and_synthetic_message_without_committing() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();
            let (rec, _) = state.queue_message_entries(&Message::user("first"), None, now, false);
            state.flush_entries(rec).unwrap();
            let (rec, _) =
                state.queue_message_entries(&Message::assistant("reply"), None, now, false);
            state.flush_entries(rec).unwrap();
            assert_eq!(state.message_count(), 2);

            let (entries, synthetic_uuid) = state.compact_entries(
                CompactBoundary {
                    summary: "summary body".to_owned(),
                    instructions: Some("focus on auth".to_owned()),
                    tasks: plan(),
                    context: SessionContext::default(),
                },
                Message::user("post-compact"),
                now,
            );

            assert_eq!(entries.len(), 3);
            assert!(
                matches!(
                    &entries[0],
                    Entry::Compact {
                        summary,
                        pre_message_count: 2,
                        instructions: Some(i),
                        tasks,
                        ..
                    } if summary == "summary body" && i == "focus on auth" && *tasks == plan()
                ),
                "boundary entry must carry summary, pre-count, instructions, and tasks \
                 verbatim: {:?}",
                entries[0],
            );
            assert!(
                matches!(
                    &entries[1],
                    Entry::Context { context, timestamp }
                        if **context == SessionContext::default() && *timestamp == now
                ),
                "the context binds to the synthetic message through its timestamp: {:?}",
                entries[1],
            );
            assert!(
                matches!(
                    &entries[2],
                    Entry::Message {
                        uuid,
                        parent_uuid: None,
                        ..
                    } if *uuid == synthetic_uuid
                ),
                "synthetic post-compact message must reset the parent chain: {:?}",
                entries[2],
            );
            assert_eq!(
                state.message_count(),
                2,
                "pre-flush count must remain until commit"
            );
            assert_ne!(state.last_message_uuid, Some(synthetic_uuid));
        }

        #[test]
        fn omits_absent_instructions() {
            let (_dir, store) = isolated_store();
            let state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();

            let (entries, _) = state.compact_entries(boundary("s"), Message::user("synth"), now);

            assert!(matches!(
                &entries[0],
                Entry::Compact {
                    instructions: None,
                    ..
                }
            ));
        }
    }

    mod commit_compact {
        use super::*;

        #[test]
        fn resets_chain_and_message_count() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();
            let (rec, _) = state.queue_message_entries(&Message::user("first"), None, now, false);
            state.flush_entries(rec).unwrap();
            let (rec, _) =
                state.queue_message_entries(&Message::assistant("reply"), None, now, false);
            state.flush_entries(rec).unwrap();

            let (_, synthetic_uuid) =
                state.compact_entries(boundary("s"), Message::user("synth"), now);
            state.commit_compact(synthetic_uuid);

            assert_eq!(state.last_message_uuid, Some(synthetic_uuid));
            assert_eq!(state.message_count(), 1);
        }

        #[test]
        fn marks_first_user_prompt_seen_so_post_compact_record_skips_title() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();

            let (_, synthetic_uuid) =
                state.compact_entries(boundary("s"), Message::user("synth"), now);
            state.commit_compact(synthetic_uuid);

            assert!(state.first_user_prompt_seen);
            let (_, seed) = state.queue_message_entries(&Message::user("after"), None, now, false);
            assert!(seed.is_none());
        }

        #[test]
        fn clears_resume_anchor_so_finish_writes_summary() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            state.initial_message_count = 5;
            state.message_count = 5;
            let now = OffsetDateTime::now_utc();

            let (_, synthetic_uuid) =
                state.compact_entries(boundary("s"), Message::user("synth"), now);
            state.commit_compact(synthetic_uuid);

            assert_eq!(state.initial_message_count, 0);
            assert_eq!(state.message_count, 1);
        }
    }

    mod finish_entries {
        use super::*;

        #[test]
        fn after_record_produces_summary_with_count() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();
            let (entries, _) =
                state.queue_message_entries(&Message::user("hello"), None, now, false);
            state.flush_entries(entries).unwrap();

            let entries = state.finish_entries(Vec::new(), now);
            let Some(Entry::Summary { message_count, .. }) = entries.last() else {
                panic!("expected trailing Summary, got {entries:?}");
            };
            assert_eq!(*message_count, 1);
        }

        #[test]
        fn pending_writer_is_empty_and_marks_finished() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);

            let entries = state.finish_entries(Vec::new(), OffsetDateTime::now_utc());

            assert!(entries.is_empty());
            assert!(state.finished);
        }

        #[test]
        fn emits_one_file_snapshot_per_tracked_file() {
            let (_dir, store) = isolated_store();
            let tracker = FileTracker::default();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();
            let (msgs, _) = state.queue_message_entries(&Message::user("hi"), None, now, false);
            state.flush_entries(msgs).unwrap();

            for name in ["/tmp/a", "/tmp/b", "/tmp/c"] {
                _ = tracker.record_read(
                    std::path::Path::new(name),
                    name.as_bytes(),
                    LastView::Full,
                    "toolu_test",
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
        fn is_idempotent() {
            let (_dir, store) = isolated_store();
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();
            let (entries, _) = state.queue_message_entries(&Message::user("hi"), None, now, false);
            state.flush_entries(entries).unwrap();
            let _first = state.finish_entries(Vec::new(), now);

            let second = state.finish_entries(Vec::new(), now);

            assert!(second.is_empty(), "second call short-circuits");
            assert!(state.finished);
        }

        #[test]
        fn skips_summary_on_empty_resume() {
            let (_dir, store) = isolated_store();
            let mut original = SessionState::fresh(store.clone(), "m", None);
            let now = OffsetDateTime::now_utc();
            let (entries, _) =
                original.queue_message_entries(&Message::user("hi"), None, now, false);
            original.flush_entries(entries).unwrap();
            let parent = original.last_message_uuid;
            let session_id = original.session_id.to_string();
            drop(original);

            let writer = store.open_append(&session_id).unwrap();
            let mut resumed = SessionState::resumed(store, session_id, writer, parent, 1, true);

            let entries = resumed.finish_entries(Vec::new(), now);

            assert!(entries.is_empty(), "no new messages → no summary");
        }
    }

    mod flush_entries {
        use super::*;

        #[test]
        fn failed_write_retries_queued_entries_at_the_writer_path() {
            let source = tempfile::tempdir().unwrap();
            let elsewhere = tempfile::tempdir().unwrap();
            let now = OffsetDateTime::now_utc();
            let mut original = SessionState::fresh(test_store(source.path()), "m", None);
            let (entries, _) =
                original.queue_message_entries(&Message::user("first"), None, now, false);
            original.flush_entries(entries).unwrap();
            let session_id = original.session_id.to_string();
            let copied = elsewhere.path().join("copied.jsonl");
            std::fs::copy(test_session_file(source.path(), &session_id), &copied).unwrap();

            let empty_store = tempfile::tempdir().unwrap();
            let mut state = SessionState::resumed(
                test_store(empty_store.path()),
                session_id,
                SessionWriter::failing_once(&copied),
                original.last_message_uuid,
                1,
                true,
            );
            let (entries, _) =
                state.queue_message_entries(&Message::user("second"), None, now, false);
            let err = state.flush_entries(entries).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("failed to write {}", copied.display())
            );
            let (entries, _) =
                state.queue_message_entries(&Message::assistant("third"), None, now, false);
            state.flush_entries(entries).unwrap();

            let data = load_session_data_from_path(&copied).unwrap();
            assert_eq!(
                data.messages,
                [
                    Message::user("first"),
                    Message::user("second"),
                    Message::assistant("third"),
                ]
            );
            assert_eq!(data.last_uuid, state.last_message_uuid);
        }

        #[test]
        fn pending_create_failure_keeps_entries_and_deferred_title() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut state = SessionState::fresh(store, "m", None);
            let now = OffsetDateTime::now_utc();
            let session_id = state.session_id.to_string();
            assert!(
                state
                    .try_defer_title("Survives rollback".to_owned())
                    .is_none(),
                "fresh state must accept defer",
            );

            let project_dir = test_project_dir(dir.path());
            std::fs::remove_dir_all(&project_dir).unwrap();
            let (entries, _) =
                state.queue_message_entries(&Message::user("first"), None, now, false);
            let err = state.flush_entries(entries).unwrap_err();

            let WriterStatus::Pending {
                header,
                deferred_title: Some(restored),
            } = &state.writer_status
            else {
                panic!("create failure must leave Pending with deferred title intact");
            };
            let path = state.store.session_file(&session_id, header.created_at);
            assert_eq!(
                err.to_string(),
                format!("failed to create {}", path.display())
            );
            assert_eq!(restored, "Survives rollback");

            std::fs::create_dir_all(&project_dir).unwrap();
            let (entries, _) =
                state.queue_message_entries(&Message::assistant("second"), None, now, false);
            state.flush_entries(entries).unwrap();

            let data =
                load_session_data_from_path(&test_session_file(dir.path(), &session_id)).unwrap();
            assert_eq!(
                data.messages,
                [Message::user("first"), Message::assistant("second")]
            );
            assert_eq!(
                data.title.map(|t| t.title).as_deref(),
                Some("Survives rollback"),
            );
        }

        #[test]
        fn failure_over_the_cap_stops_persistence_for_good() {
            let dir = tempfile::tempdir().unwrap();
            let now = OffsetDateTime::now_utc();
            let mut original = SessionState::fresh(test_store(dir.path()), "m", None);
            let (entries, _) =
                original.queue_message_entries(&Message::user("first"), None, now, false);
            original.flush_entries(entries).unwrap();
            let session_id = original.session_id.to_string();
            let path = test_session_file(dir.path(), &session_id);
            let before = std::fs::read(&path).unwrap();

            let mut state = SessionState::resumed(
                test_store(dir.path()),
                session_id,
                SessionWriter::failing_once(&path),
                original.last_message_uuid,
                1,
                true,
            );
            let huge = Message::user("x".repeat(MAX_UNWRITTEN_BYTES));
            let (entries, _) = state.queue_message_entries(&huge, None, now, false);
            let err = state.flush_entries(entries).unwrap_err();
            assert!(
                format!("{err:#}").contains(&persistence_stopped()),
                "got: {err:#}"
            );
            let (entries, _) =
                state.queue_message_entries(&Message::assistant("after"), None, now, false);
            let err = state.flush_entries(entries).unwrap_err();

            assert_eq!(err.to_string(), persistence_stopped());
            assert!(state.persistence_stopped());
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "a stopped session writes nothing"
            );
        }
    }

    mod extract_user_text {
        use super::*;

        #[test]
        fn selects_first_nonempty_user_text() {
            let tool_result = ContentBlock::tool_result("t1", "output", false);
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
    }
}
