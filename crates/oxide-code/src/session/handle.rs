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
use tokio::task::JoinHandle;

use super::actor::{self, SessionCmd};
use super::sanitize::sanitize_resumed_messages;
use super::state::{SessionState, extract_user_text};
use super::store::{
    SessionData, SessionStore, load_session_data_from_path, open_append_at,
    read_session_id_from_path,
};
use crate::file_tracker::{FileSnapshot, FileTracker};
use crate::message::Message;
use crate::tool::ToolMetadata;

#[cfg(test)]
pub(crate) mod testing;

/// Headroom for burst tool-round writes + concurrent title-generator.
const CHANNEL_CAPACITY: usize = 1024;

// ── SessionHandle ──

/// Cheap to clone — clones share one actor task. All methods are async
/// and return after the batch flush this cmd lands in.
#[derive(Clone)]
pub(crate) struct SessionHandle {
    cmd_tx: mpsc::Sender<SessionCmd>,
    session_id: Arc<str>,
    shared: Arc<SharedState>,
    /// First `shutdown` takes the join; subsequent calls no-op.
    actor_join: Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
}

/// Sticky-once failure flags shared between actor and handle clones.
#[derive(Default)]
pub(super) struct SharedState {
    /// First batch-flush failure has surfaced through a caller's ack.
    flush_failure_surfaced: AtomicBool,
    /// First actor-gone surface has fired; subsequent send/recv errors
    /// stay silent.
    actor_gone_surfaced: AtomicBool,
    /// Most recent flush error message. `actor_gone_failure` reads this
    /// so the user sees the underlying I/O cause when the actor died on
    /// a real disk error, not just the generic "session actor is gone".
    last_flush_failure: std::sync::Mutex<Option<String>>,
}

impl SharedState {
    pub(super) fn record_flush_failure(&self, msg: &str) {
        match self.last_flush_failure.lock() {
            Ok(mut slot) => *slot = Some(msg.to_owned()),
            Err(e) => {
                tracing::warn!("last_flush_failure mutex poisoned, recovering");
                *e.into_inner() = Some(msg.to_owned());
            }
        }
    }

    /// `true` on the first call after [`Self::record_flush_failure`];
    /// sticky `false` afterwards. Surfaces a flush error to the user
    /// once and stays silent on every subsequent failure so a
    /// disk-full mid-conversation doesn't drown the UI.
    pub(super) fn surface_first_flush_failure(&self) -> bool {
        !self.flush_failure_surfaced.swap(true, Ordering::AcqRel)
    }

    /// Sticky-once: `true` on first call, `false` after.
    pub(super) fn surface_first_actor_gone(&self) -> bool {
        !self.actor_gone_surfaced.swap(true, Ordering::AcqRel)
    }

    /// Most recent flush error, threaded into actor-gone messages.
    pub(super) fn last_flush_failure(&self) -> Option<String> {
        match self.last_flush_failure.lock() {
            Ok(guard) => guard.clone(),
            Err(e) => {
                tracing::warn!("last_flush_failure mutex poisoned, recovering");
                e.into_inner().clone()
            }
        }
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
        rx.await.unwrap_or_else(|_| RecordOutcome {
            ai_title_seed: None,
            failure: self.actor_gone_failure(),
        })
    }

    /// Record sidecar metadata for every tool result from one agent
    /// turn in one shot — one cmd, one ack, one flush — realising the
    /// plan's "a turn's worth of cmds collapse into one flush" intent
    /// for the hot path. Items whose `metadata == ToolMetadata::default()`
    /// add no display fields and are skipped at absorb.
    pub(crate) async fn record_tool_metadata_batch(
        &self,
        items: Vec<(String, ToolMetadata)>,
    ) -> Outcome {
        let (ack, rx) = oneshot::channel();
        self.dispatch_outcome(SessionCmd::ToolMetadata { items, ack }, rx)
            .await
    }

    /// Append an AI-generated session title. Tail-scan picks the
    /// latest title (max `updated_at`), so this supersedes the
    /// first-prompt title on listings and resumes.
    pub(crate) async fn append_ai_title(&self, title: String) -> Outcome {
        let (ack, rx) = oneshot::channel();
        self.dispatch_outcome(SessionCmd::AppendAiTitle { title, ack }, rx)
            .await
    }

    /// Write the session summary and finalize. Idempotent; no-op on
    /// fresh sessions that never recorded anything.
    ///
    /// `snapshots` is drained from the shared
    /// [`FileTracker`][crate::file_tracker::FileTracker] just before
    /// the call — one `Entry::FileSnapshot` lands per snapshot, ahead
    /// of the `Entry::Summary`. An empty `Vec` finalizes with no
    /// tracker entries.
    pub(crate) async fn finish(&self, snapshots: Vec<FileSnapshot>) -> Outcome {
        let (ack, rx) = oneshot::channel();
        self.dispatch_outcome(SessionCmd::Finish { snapshots, ack }, rx)
            .await
    }

    /// Finalize and tear down: writes the summary, surfaces a flush
    /// error (if any), and shuts the actor down. Returns the failure
    /// message so callers can route it as fits their context (warn-log
    /// after the TUI has torn down, [`AgentSink::session_write_error`]
    /// while the sink is still live).
    pub(crate) async fn finalize(self, snapshots: Vec<FileSnapshot>) -> Option<String> {
        let outcome = self.finish(snapshots).await;
        self.shutdown().await;
        outcome.failure
    }

    /// Sends [`SessionCmd::Shutdown`] so the actor breaks its loop
    /// after the current batch flushes, then awaits the join handle.
    /// Cmd-driven exit (rather than waiting for every clone's
    /// [`mpsc::Sender`] to drop) keeps process-exit fast even when an
    /// orphaned clone — most importantly the detached title-generator
    /// task — is mid-HTTP and far from dropping its handle.
    /// Subsequent calls (on other clones) drain the join slot and
    /// return immediately.
    pub(crate) async fn shutdown(self) {
        let Self {
            cmd_tx, actor_join, ..
        } = self;
        let (ack, rx) = oneshot::channel();
        // If send fails the actor is already gone — proceed straight
        // to awaiting the join handle (likely already finished).
        if cmd_tx.send(SessionCmd::Shutdown { ack }).await.is_ok() {
            _ = rx.await;
        }
        drop(cmd_tx);
        let join = match actor_join.lock() {
            Ok(mut g) => g.take(),
            Err(e) => {
                tracing::warn!("actor_join mutex poisoned, recovering");
                e.into_inner().take()
            }
        };
        if let Some(j) = join {
            _ = j.await;
        }
    }

    /// Sends a cmd and awaits the `Outcome` ack; falls back to actor-gone on failure.
    async fn dispatch_outcome(&self, cmd: SessionCmd, rx: oneshot::Receiver<Outcome>) -> Outcome {
        if self.cmd_tx.send(cmd).await.is_err() {
            return Outcome {
                failure: self.actor_gone_failure(),
            };
        }
        rx.await.unwrap_or_else(|_| Outcome {
            failure: self.actor_gone_failure(),
        })
    }

    /// Surfaces actor-gone failure once, including the underlying I/O cause if known.
    fn actor_gone_failure(&self) -> Option<String> {
        if !self.shared.surface_first_actor_gone() {
            return None;
        }
        tracing::warn!("session actor task is gone — recording dropped");
        let cause = self.shared.last_flush_failure();
        let msg = match cause {
            Some(io_err) => format!(
                "Session writer task has stopped after I/O error: {io_err}. Conversation \
                 history may be incomplete; further write errors will be silent."
            ),
            None => "Session writer task has stopped. Conversation history may be incomplete; \
                     further write errors will be silent."
                .to_owned(),
        };
        Some(msg)
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
    /// Persisted tracker snapshots. The caller passes these to
    /// [`FileTracker::restore_verified`][crate::file_tracker::FileTracker::restore_verified]
    /// before the agent loop runs so the gate clears for previously-
    /// observed files that still match disk.
    pub(crate) file_snapshots: Vec<FileSnapshot>,
}

// ── Constructors ──

/// Start a fresh session and spawn its actor. The file materializes
/// lazily on the first record cmd.
pub(crate) fn start(store: &SessionStore, model: &str) -> SessionHandle {
    spawn_actor(SessionState::fresh(store.clone(), model))
}

/// Outcome of [`roll`]: the new session id plus any flush failure
/// from finalizing the old session. Caller decides where to surface
/// the failure (sink for live UI, warn-log post-teardown).
pub(crate) struct RollOutcome {
    pub(crate) new_id: String,
    pub(crate) finalize_failure: Option<String>,
}

/// Rolls `session` to a fresh handle and finalizes the old one. Two
/// orderings matter: snapshot **before** clear (so the snapshots
/// survive into the old JSONL via `finalize`), and replace **before**
/// finalize (so the new session is in place before the old handle is
/// consumed).
///
/// Caller must update any state derived from the session id (HTTP
/// client header, in-memory message log) — this helper does not.
pub(crate) async fn roll(
    session: &mut SessionHandle,
    store: &SessionStore,
    file_tracker: &FileTracker,
    model: &str,
) -> RollOutcome {
    let snapshots = file_tracker.snapshot_all();
    file_tracker.clear();
    let new_session = start(store, model);
    let new_id = new_session.session_id().to_owned();
    let old_session = std::mem::replace(session, new_session);
    let finalize_failure = old_session.finalize(snapshots).await;
    RollOutcome {
        new_id,
        finalize_failure,
    }
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
    // Bail rather than chain the next record onto a `last_message_uuid`
    // that sanitize just dropped.
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
        file_snapshots: data.file_snapshots,
    })
}

fn spawn_actor(state: SessionState) -> SessionHandle {
    let session_id = Arc::clone(&state.session_id);
    let shared = Arc::new(SharedState::default());
    let (cmd_tx, cmd_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let actor_shared = Arc::clone(&shared);
    let join = tokio::spawn(actor::run(state, cmd_rx, actor_shared));
    SessionHandle {
        cmd_tx,
        session_id,
        shared,
        actor_join: Arc::new(std::sync::Mutex::new(Some(join))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::{test_session_file, test_store};
    use super::*;
    use crate::file_tracker::FileTracker;
    use crate::file_tracker::testing::record_tracked_file;
    use crate::message::{ContentBlock, Role};

    // ── record_message ──

    #[tokio::test]
    async fn record_message_writes_title_before_first_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();

        handle.record_message(Message::user("First prompt")).await;
        handle.finish(Vec::new()).await;
        drop(handle);

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert!(lines[0].contains(r#""type":"header""#));
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
        let handle = testing::dead("dead");

        let first = handle.record_message(Message::user("a")).await;
        let second = handle.record_message(Message::user("b")).await;

        assert!(first.failure.is_some());
        assert!(second.failure.is_none());
    }

    #[tokio::test]
    async fn record_message_actor_gone_carries_underlying_flush_error() {
        // The user should see the I/O cause, not the generic
        // dropped-task fallback, when one is on record.
        let handle = testing::dead("dead");
        handle
            .shared
            .record_flush_failure("permission denied: /readonly/path");

        let first = handle.record_message(Message::user("a")).await;

        let msg = first.failure.expect("actor-gone surfaces on first call");
        assert!(
            msg.contains("after I/O error: permission denied"),
            "underlying flush error should be carried: {msg}",
        );
    }

    #[tokio::test]
    async fn record_message_actor_gone_surfaces_after_prior_flush_failure_was_surfaced() {
        // The two sticky-once flags must be independent: a previously
        // surfaced flush failure must not silence the actor-gone signal.
        let handle = testing::dead("dead");
        handle.shared.record_flush_failure("disk full");
        assert!(handle.shared.surface_first_flush_failure());

        let first = handle.record_message(Message::user("a")).await;

        assert!(first.failure.is_some());
    }

    #[tokio::test]
    async fn record_message_resumed_session_does_not_seed_ai_title() {
        // Resumed sessions inherit the original first-prompt title.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = start(&store, "m");
        let session_id = original.session_id().to_owned();
        original.record_message(Message::user("hello")).await;
        original.finish(Vec::new()).await;
        drop(original);

        let resumed = resume(&store, &session_id).unwrap().handle;
        let outcome = resumed.record_message(Message::user("more text")).await;
        assert!(outcome.ai_title_seed.is_none());
    }

    // ── record_tool_metadata_batch ──

    #[tokio::test]
    async fn record_tool_metadata_batch_writes_all_non_default_in_one_cmd() {
        // Default-metadata items are skipped at absorb; non-defaults
        // each produce one line.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();
        handle.record_message(Message::user("trigger")).await;

        let outcome = handle
            .record_tool_metadata_batch(vec![
                (
                    "t1".to_owned(),
                    ToolMetadata {
                        title: Some("Edited a.rs".to_owned()),
                        ..ToolMetadata::default()
                    },
                ),
                ("t2".to_owned(), ToolMetadata::default()),
                (
                    "t3".to_owned(),
                    ToolMetadata {
                        replacements: Some(2),
                        ..ToolMetadata::default()
                    },
                ),
            ])
            .await;
        handle.finish(Vec::new()).await;
        drop(handle);

        assert!(outcome.failure.is_none());
        let data = store.load_session_data(&sid).unwrap();
        assert_eq!(
            data.tool_result_metadata.len(),
            2,
            "default-only sidecars are skipped; t1 and t3 land",
        );
        assert_eq!(
            data.tool_result_metadata
                .get("t1")
                .unwrap()
                .title
                .as_deref(),
            Some("Edited a.rs"),
        );
        assert_eq!(
            data.tool_result_metadata.get("t3").unwrap().replacements,
            Some(2),
        );
    }

    #[tokio::test]
    async fn record_tool_metadata_batch_all_default_acks_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();
        handle.record_message(Message::user("trigger")).await;

        let outcome = handle
            .record_tool_metadata_batch(vec![
                ("t1".to_owned(), ToolMetadata::default()),
                ("t2".to_owned(), ToolMetadata::default()),
            ])
            .await;
        handle.finish(Vec::new()).await;
        drop(handle);

        assert!(outcome.failure.is_none());
        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        assert!(
            !content.contains(r#""type":"tool_result_metadata""#),
            "all-default batch writes nothing: {content}",
        );
    }

    #[tokio::test]
    async fn record_tool_metadata_batch_actor_gone_surfaces_failure_once_then_silences() {
        let handle = testing::dead("dead");
        let item = (
            "t1".to_owned(),
            ToolMetadata {
                title: Some("f.rs".to_owned()),
                ..ToolMetadata::default()
            },
        );

        let first = handle.record_tool_metadata_batch(vec![item.clone()]).await;
        let second = handle.record_tool_metadata_batch(vec![item]).await;

        assert!(first.failure.is_some(), "first call must surface failure");
        assert!(second.failure.is_none(), "subsequent calls must be silent");
    }

    #[tokio::test]
    async fn record_tool_metadata_batch_actor_drops_ack_surfaces_actor_gone_failure() {
        // Distinguishes the rx-await fallback in dispatch_outcome from
        // the cmd_tx.send-failed early return: send succeeds and the
        // actor receives the cmd, but drops the ack without responding
        // (simulating a panic between recv and ack). The succeed=3
        // header lets the same handle exercise the success arm on
        // ToolMetadata / AppendAiTitle / Finish variants of the
        // helper's or-pattern, then the drop arm on the 4th cmd.
        let handle = testing::acks_then_drops("acks-then-drops", 3);

        let title = handle.append_ai_title("ok".to_owned()).await;
        let finish = handle.finish(Vec::new()).await;
        let batch_ok = handle
            .record_tool_metadata_batch(vec![(
                "t1".to_owned(),
                ToolMetadata {
                    title: Some("f.rs".to_owned()),
                    ..ToolMetadata::default()
                },
            )])
            .await;
        let batch_dropped = handle.record_tool_metadata_batch(vec![]).await;

        assert!(title.failure.is_none(), "AppendAiTitle is acked healthily");
        assert!(finish.failure.is_none(), "Finish is acked healthily");
        assert!(
            batch_ok.failure.is_none(),
            "ToolMetadata is acked healthily"
        );
        assert!(
            batch_dropped.failure.is_some(),
            "dropped ack surfaces actor-gone",
        );

        // Drain the actor task so its loop-exit is exercised — without
        // shutdown, the test runtime tears the task down mid-recv.
        handle.shutdown().await;
    }

    // ── append_ai_title ──

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
        handle.finish(Vec::new()).await;
        drop(handle);

        let sessions = store.list().unwrap();
        let session = sessions.iter().find(|s| s.session_id == sid).unwrap();
        assert_eq!(
            session.title.as_ref().unwrap().title,
            "Fix auth flow for mobile",
        );
    }

    #[tokio::test]
    async fn append_ai_title_actor_gone_surfaces_failure_once_then_silences() {
        let handle = testing::dead("dead");

        let first = handle.append_ai_title("Fix auth".to_owned()).await;
        let second = handle.append_ai_title("Fix auth".to_owned()).await;

        assert!(first.failure.is_some(), "first call must surface failure");
        assert!(second.failure.is_none(), "subsequent calls must be silent");
    }

    // ── finish ──

    #[tokio::test]
    async fn finish_writes_summary_with_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();

        handle
            .record_message(Message::user("Fix the auth bug"))
            .await;
        handle.finish(Vec::new()).await;
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
        handle.finish(Vec::new()).await;
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
        original.finish(Vec::new()).await;
        drop(original);

        let resumed = resume(&store, &session_id).unwrap().handle;
        resumed.finish(Vec::new()).await;
        drop(resumed);

        let content = std::fs::read_to_string(test_session_file(dir.path(), &session_id)).unwrap();
        let summary_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"summary""#))
            .count();
        assert_eq!(summary_count, 1);
    }

    #[tokio::test]
    async fn finish_actor_gone_surfaces_failure_once_then_silences() {
        let handle = testing::dead("dead");

        let first = handle.finish(Vec::new()).await;
        let second = handle.finish(Vec::new()).await;

        assert!(first.failure.is_some(), "first call must surface failure");
        assert!(second.failure.is_none(), "subsequent calls must be silent");
    }

    // ── finalize ──

    #[tokio::test]
    async fn finalize_writes_summary_then_returns_none_on_success() {
        // Pins the success contract: summary lands on disk and the
        // returned failure is `None`.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();
        handle.record_message(Message::user("hi")).await;

        let failure = handle.finalize(Vec::new()).await;

        assert!(failure.is_none(), "healthy finalize reports no failure");
        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        assert!(
            content.contains(r#""type":"summary""#),
            "summary lands on disk: {content}",
        );
    }

    #[tokio::test]
    async fn finalize_returns_failure_when_actor_dead() {
        // Dead-actor path must surface so callers can route the
        // failure (warn-log on exit, sink on `/clear` roll).
        let handle = testing::dead("dead");
        let failure = handle.finalize(Vec::new()).await;
        assert!(failure.is_some(), "dead handle finalize surfaces failure");
    }

    // ── shutdown ──

    #[tokio::test]
    async fn shutdown_after_record_returns_after_actor_exits() {
        // shutdown's contract: the recorded message must be on disk by
        // the time the await returns.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();

        handle.record_message(Message::user("hello")).await;
        handle.shutdown().await;

        let path = test_session_file(dir.path(), &sid);
        let content = std::fs::read_to_string(path).unwrap();
        assert!(
            content.contains(r#""type":"message""#),
            "actor must have flushed before shutdown returned: {content}",
        );
    }

    #[tokio::test]
    async fn shutdown_is_safe_to_call_after_dead_handle() {
        // Smoke check: empty join slot must not panic.
        let handle = testing::dead("dead");
        handle.shutdown().await;
    }

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
        // The await means the batch flushed; the file is on disk.
        assert!(
            test_session_file(dir.path(), handle.session_id()).exists(),
            "first record_message should materialize the session file",
        );
    }

    // ── roll ──

    #[tokio::test]
    async fn roll_swaps_session_in_place_and_finalizes_old_with_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let tracker = FileTracker::default();
        let mut session = start(&store, "test-model");
        let original_id = session.session_id().to_owned();
        session.record_message(Message::user("original")).await;

        let outcome = roll(&mut session, &store, &tracker, "test-model").await;

        assert_ne!(outcome.new_id, original_id, "id must roll");
        assert_eq!(
            session.session_id(),
            outcome.new_id,
            "new session swapped in place",
        );
        assert!(
            outcome.finalize_failure.is_none(),
            "healthy roll reports no failure",
        );

        let old_jsonl = std::fs::read_to_string(test_session_file(dir.path(), &original_id))
            .expect("old session file lands on disk");
        assert!(
            old_jsonl.contains(r#""type":"summary""#),
            "old JSONL gets a summary entry: {old_jsonl}",
        );
        assert!(
            old_jsonl.contains(&original_id),
            "old JSONL header carries the original id: {old_jsonl}",
        );
    }

    #[tokio::test]
    async fn roll_persists_tracker_snapshots_into_old_session_then_clears_tracker() {
        // Snapshot-before-clear: snapshots from the live tracker must
        // land in the OLD JSONL via finalize, and the tracker must be
        // empty afterwards so the new session starts cold.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let tracker = FileTracker::default();
        let seed = dir.path().join("seed.txt");
        std::fs::write(&seed, b"hello").unwrap();
        crate::file_tracker::testing::seed_full_read(&tracker, &seed);
        assert!(!tracker.snapshot_all().is_empty(), "tracker seeded");

        let mut session = start(&store, "test-model");
        let original_id = session.session_id().to_owned();
        session.record_message(Message::user("seed")).await;

        _ = roll(&mut session, &store, &tracker, "test-model").await;

        assert!(
            tracker.snapshot_all().is_empty(),
            "tracker cleared after roll",
        );
        let old_jsonl = std::fs::read_to_string(test_session_file(dir.path(), &original_id))
            .expect("old session file lands on disk");
        assert!(
            old_jsonl.contains("seed.txt"),
            "snapshot survived into old JSONL: {old_jsonl}",
        );
    }

    // ── file-snapshot persistence ──

    #[tokio::test]
    async fn finish_persists_one_file_snapshot_per_tracked_file() {
        // Three tracked files in → three FileSnapshot lines on disk
        // with the right paths. Counting strings would pass even if
        // every snapshot pointed at the same file; parse and assert
        // the path set instead.
        let dir = tempfile::tempdir().unwrap();
        let files_dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let tracker = FileTracker::default();
        let handle = start(&store, "m");
        let sid = handle.session_id().to_owned();

        handle.record_message(Message::user("trigger")).await;
        let expected_paths: std::collections::HashSet<std::path::PathBuf> =
            ["a.rs", "b.rs", "c.rs"]
                .into_iter()
                .map(|name| {
                    let path = files_dir.path().join(name);
                    record_tracked_file(&tracker, &path, name.as_bytes());
                    path
                })
                .collect();

        handle.finish(tracker.snapshot_all()).await;
        drop(handle);

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        let snapshot_paths: std::collections::HashSet<std::path::PathBuf> = content
            .lines()
            .filter(|l| l.contains(r#""type":"file_snapshot""#))
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                std::path::PathBuf::from(v["path"].as_str().unwrap())
            })
            .collect();
        assert_eq!(
            snapshot_paths, expected_paths,
            "every tracked file lands as its own FileSnapshot",
        );
    }

    #[tokio::test]
    async fn resume_restores_unchanged_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let files_dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let tracker = FileTracker::default();
        let handle = start(&store, "m");
        let session_id = handle.session_id().to_owned();
        handle.record_message(Message::user("hi")).await;
        let path_a = files_dir.path().join("a.rs");
        record_tracked_file(&tracker, &path_a, b"alpha");
        handle.finish(tracker.snapshot_all()).await;
        drop(handle);

        let resumed_tracker = FileTracker::default();
        let resumed = resume(&store, &session_id).unwrap();
        resumed_tracker.restore_verified(resumed.file_snapshots);

        let meta = std::fs::metadata(&path_a).unwrap();
        let check = resumed_tracker.check_stat(
            &path_a,
            meta.modified().unwrap(),
            meta.len(),
            crate::file_tracker::GatePurpose::Edit,
        );
        assert_eq!(check, Ok(crate::file_tracker::StatCheck::Pass));
    }

    #[tokio::test]
    async fn resume_drops_drifted_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let files_dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let tracker = FileTracker::default();
        let handle = start(&store, "m");
        let session_id = handle.session_id().to_owned();
        handle.record_message(Message::user("hi")).await;
        let path_a = files_dir.path().join("a.rs");
        record_tracked_file(&tracker, &path_a, b"alpha");
        handle.finish(tracker.snapshot_all()).await;
        drop(handle);

        // Bump mtime / size before resume so the snapshot stat
        // mismatches and the entry drops.
        std::fs::write(&path_a, b"externally edited bytes").unwrap();

        let resumed_tracker = FileTracker::default();
        let resumed = resume(&store, &session_id).unwrap();
        resumed_tracker.restore_verified(resumed.file_snapshots);

        let meta = std::fs::metadata(&path_a).unwrap();
        let check = resumed_tracker.check_stat(
            &path_a,
            meta.modified().unwrap(),
            meta.len(),
            crate::file_tracker::GatePurpose::Edit,
        );
        assert_eq!(
            check,
            Err(crate::file_tracker::GateError::NeverRead {
                path: path_a.clone(),
                purpose: crate::file_tracker::GatePurpose::Edit,
            }),
            "drifted file must hit the must-read-first gate",
        );
    }

    #[tokio::test]
    async fn resume_drops_snapshots_for_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let files_dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let tracker = FileTracker::default();
        let handle = start(&store, "m");
        let session_id = handle.session_id().to_owned();
        handle.record_message(Message::user("hi")).await;
        let path_a = files_dir.path().join("a.rs");
        record_tracked_file(&tracker, &path_a, b"alpha");
        handle.finish(tracker.snapshot_all()).await;
        drop(handle);

        std::fs::remove_file(&path_a).unwrap();

        let resumed_tracker = FileTracker::default();
        let resumed = resume(&store, &session_id).unwrap();
        // restore_verified must drop silently; the tracker should
        // end up empty for this path rather than panic.
        resumed_tracker.restore_verified(resumed.file_snapshots);
        let check = resumed_tracker.check_stat(
            &path_a,
            std::time::UNIX_EPOCH,
            0,
            crate::file_tracker::GatePurpose::Edit,
        );
        assert_eq!(
            check,
            Err(crate::file_tracker::GateError::NeverRead {
                path: path_a.clone(),
                purpose: crate::file_tracker::GatePurpose::Edit,
            }),
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
        original.finish(Vec::new()).await;
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
        // Skip finish — `shutdown` drains the queued record cmd anyway.
        original.shutdown().await;

        let ResumedSession { messages, .. } = resume(&store, &session_id).unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn resume_empty_session_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = start(&store, "m");
        let session_id = original.session_id().to_owned();
        original.finish(Vec::new()).await;
        drop(original);

        assert!(resume(&store, &session_id).is_err());
    }

    #[tokio::test]
    async fn resume_all_messages_sanitized_returns_error() {
        // Sanitize drops the lone unresolved tool_use, leaving an
        // empty chain — resume must bail.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let original = start(&store, "m");
        let session_id = original.session_id().to_owned();
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
        original.shutdown().await;

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
        original.finish(Vec::new()).await;
        drop(original);

        let resumed = resume(&store, &session_id).unwrap().handle;
        resumed.record_message(Message::user("follow up")).await;
        resumed.finish(Vec::new()).await;
        drop(resumed);

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
}
