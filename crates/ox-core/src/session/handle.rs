//! Public session API. Every write method sends a [`super::actor::SessionCmd`] and awaits the
//! actor's ack without holding a lock across `await`.

#[cfg(test)]
pub(crate) mod testing;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, warn};
use uuid::Uuid;

use super::actor::{self, SessionCmd};
use super::entry::{CompactBoundary, CompactInfo, UsageRecord};
use super::sanitize::{retain_visible_file_snapshots, sanitize_resumed_messages};
use super::saved::SavedError;
use super::state::{SessionState, extract_user_text};
use super::store::{SessionData, SessionStore, load_session_data_from_path, open_append_at};
use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::prompt::SessionContext;
use crate::tool::ToolMetadata;
use crate::tool::tasks::{self, Task};
use crate::util::sync::lock_or_recover;

/// Headroom for burst tool-round writes and the concurrent title generator.
const CHANNEL_CAPACITY: usize = 1024;

// ── SessionHandle ──

/// Cheap to clone because clones share one actor task. Every write method returns after the
/// batch flush that its cmd lands in.
#[derive(Clone)]
pub struct SessionHandle {
    cmd_tx: mpsc::Sender<SessionCmd>,
    session_id: Arc<str>,
    artifact_dir: Arc<Path>,
    shared: Arc<SharedState>,
    /// Taken by the first `shutdown` among the clones.
    actor_join: Arc<Mutex<Option<JoinHandle<()>>>>,
    title_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

/// State the actor and every handle clone share. Each `surface_first_*` method returns `true` on
/// its first call and `false` after.
#[derive(Default)]
pub(super) struct SharedState {
    flush_failure_surfaced: AtomicBool,
    persistence_stop_surfaced: AtomicBool,
    actor_gone_surfaced: AtomicBool,
    /// Whether a `/rename` titled the session.
    /// [`SessionHandle::set_manual_title`] sets it before it sends the title,
    /// so no AI or first-prompt title follows.
    /// The title generator checks it before appending, and the actor checks again on arrival,
    /// since a `/rename` can land in between.
    manual_title_set: AtomicBool,
    /// Most recent flush error, threaded into actor-gone messages so the user sees the I/O cause.
    last_flush_failure: Mutex<Option<String>>,
}

impl SharedState {
    pub(super) fn record_flush_failure(&self, msg: &str) {
        *lock_or_recover(&self.last_flush_failure, "last_flush_failure") = Some(msg.to_owned());
    }

    pub(super) fn surface_first_flush_failure(&self) -> bool {
        surface_once(&self.flush_failure_surfaced)
    }

    pub(super) fn surface_first_persistence_stop(&self) -> bool {
        surface_once(&self.persistence_stop_surfaced)
    }

    fn surface_first_actor_gone(&self) -> bool {
        surface_once(&self.actor_gone_surfaced)
    }

    pub(super) fn mark_manual_title_set(&self) {
        self.manual_title_set.store(true, Ordering::Release);
    }

    pub(super) fn manual_title_set(&self) -> bool {
        self.manual_title_set.load(Ordering::Acquire)
    }

    fn last_flush_failure(&self) -> Option<String> {
        lock_or_recover(&self.last_flush_failure, "last_flush_failure").clone()
    }
}

fn surface_once(surfaced: &AtomicBool) -> bool {
    !surfaced.swap(true, Ordering::AcqRel)
}

/// A point in the active branch that [`SessionHandle::rewind`] can move the tip back to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Checkpoint {
    /// The message the next one chains from, `None` before the first.
    pub(super) tip: Option<Uuid>,
    pub(super) message_count: u32,
    /// Whether a user prompt before this point titled the session.
    pub(super) first_user_prompt_seen: bool,
}

/// Ack from [`SessionHandle::record_message`].
#[derive(Debug)]
pub(crate) struct RecordOutcome {
    /// The branch point the message chained from, `None` when the actor is gone.
    pub(crate) checkpoint: Option<Checkpoint>,
    /// `Some` only on the first user-text message of a fresh session.
    pub(crate) ai_title_seed: Option<String>,
    /// A write failure, reported only the first time each kind of failure occurs.
    pub(crate) failure: Option<String>,
}

/// Ack from the other write cmds, with the same failure surface as [`RecordOutcome`], except
/// [`SessionHandle::archive`]'s, which reports every failure.
#[derive(Debug)]
pub(crate) struct Outcome {
    pub(crate) failure: Option<String>,
}

/// Ack from [`SessionHandle::compact`]. `pre_count` is the message count when the request reached
/// the actor.
#[derive(Debug)]
pub(crate) struct CompactOutcome {
    pub(crate) pre_count: u32,
    /// The synthetic root as a rewind point, `Some` once the boundary took effect, either on disk
    /// or in memory alone after persistence stopped.
    pub(crate) checkpoint: Option<Checkpoint>,
    /// Every failure that leaves the boundary uncommitted, or else a write failure reported only
    /// the first time each kind occurs.
    pub(crate) failure: Option<String>,
}

impl SessionHandle {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The [`artifact_dir`](super::store::artifact_dir) beside the session's file, wherever the
    /// file lives.
    pub(crate) fn artifact_dir(&self) -> &Arc<Path> {
        &self.artifact_dir
    }

    /// Ties the AI title request to this session, so `finalize` aborts it.
    pub(super) fn set_title_task(&self, task: JoinHandle<()>) {
        *lock_or_recover(&self.title_task, "title_task") = Some(task);
    }

    /// Records a conversation message.
    pub(crate) async fn record_message(&self, msg: Message) -> RecordOutcome {
        self.record_message_with_context(msg, None).await
    }

    /// [`Self::record_message`], with the prompt state `msg` is the first to carry.
    pub(crate) async fn record_message_with_context(
        &self,
        msg: Message,
        context: Option<SessionContext>,
    ) -> RecordOutcome {
        let (ack, rx) = oneshot::channel();
        self.request(SessionCmd::Record { msg, context, ack }, rx)
            .await
            .unwrap_or_else(|| RecordOutcome {
                checkpoint: None,
                ai_title_seed: None,
                failure: self.actor_gone_failure(),
            })
    }

    /// Moves the session's tip back to `checkpoint`, abandoning the messages recorded after it.
    /// `checkpoint` must come from the last compaction or a later record, because the loader cannot
    /// rewind across a boundary.
    /// A rewind to before the first user prompt stops the title request that prompt started,
    /// so the next prompt titles the session.
    pub(crate) async fn rewind(&self, checkpoint: Checkpoint) -> Outcome {
        if !checkpoint.first_user_prompt_seen {
            self.stop_title_task().await;
        }
        let (ack, rx) = oneshot::channel();
        self.dispatch_outcome(SessionCmd::Rewind { checkpoint, ack }, rx)
            .await
    }

    /// Records sidecar metadata for a tool round in one cmd. Default-metadata items are skipped.
    pub(crate) async fn record_tool_metadata_batch(
        &self,
        items: Vec<(String, ToolMetadata)>,
    ) -> Outcome {
        let (ack, rx) = oneshot::channel();
        self.dispatch_outcome(SessionCmd::ToolMetadata { items, ack }, rx)
            .await
    }

    /// Records the token usage of billed requests, one entry each.
    pub(crate) async fn record_usage(&self, records: Vec<UsageRecord>) -> Outcome {
        let (ack, rx) = oneshot::channel();
        self.dispatch_outcome(SessionCmd::Usage { records, ack }, rx)
            .await
    }

    /// Appends an AI-generated session title (supersedes the first-prompt title).
    pub(super) async fn append_ai_title(&self, title: String) -> Outcome {
        let (ack, rx) = oneshot::channel();
        self.dispatch_outcome(SessionCmd::AppendAiTitle { title, ack }, rx)
            .await
    }

    /// Persists a user-supplied title, suppressing AI titles for the rest of the session.
    pub(crate) async fn set_manual_title(&self, title: String) -> Outcome {
        self.stop_title_task().await;
        self.shared.mark_manual_title_set();
        let (ack, rx) = oneshot::channel();
        self.dispatch_outcome(SessionCmd::SetManualTitle { title, ack }, rx)
            .await
    }

    pub(crate) fn manual_title_set(&self) -> bool {
        self.shared.manual_title_set()
    }

    /// Writes the compaction boundary and the synthetic post-compact message, then resets the chain
    /// anchor.
    /// The caller swaps in the summary only once [`CompactOutcome::checkpoint`]
    /// says the boundary took effect.
    pub(crate) async fn compact(
        &self,
        boundary: CompactBoundary,
        synthetic_message: Message,
    ) -> CompactOutcome {
        let (ack, rx) = oneshot::channel();
        let cmd = SessionCmd::Compact {
            boundary,
            synthetic_message,
            ack,
        };
        self.request(cmd, rx)
            .await
            .unwrap_or_else(|| CompactOutcome {
                pre_count: 0,
                checkpoint: None,
                failure: Some(self.actor_stopped_failure()),
            })
    }

    /// Marks the session archived, reporting every failure since the caller rolls away only once
    /// the entry is on disk. Call it only after a message is recorded, since the entry would
    /// otherwise create a file holding nothing else.
    pub(crate) async fn archive(&self) -> Outcome {
        let (ack, rx) = oneshot::channel();
        self.request(SessionCmd::Archive { ack }, rx)
            .await
            .unwrap_or_else(|| Outcome {
                failure: Some(self.actor_stopped_failure()),
            })
    }

    /// Writes `snapshots` and the summary, once and only if the session recorded new messages.
    pub(crate) async fn finish(&self, snapshots: Vec<FileSnapshot>) -> Outcome {
        let (ack, rx) = oneshot::channel();
        self.dispatch_outcome(SessionCmd::Finish { snapshots, ack }, rx)
            .await
    }

    /// Stops the title request, writes the summary, and shuts the actor down, returning the
    /// summary's write failure.
    pub(crate) async fn finalize(self, snapshots: Vec<FileSnapshot>) -> Option<String> {
        self.stop_title_task().await;
        let outcome = self.finish(snapshots).await;
        self.shutdown().await;
        outcome.failure
    }

    /// Aborts the title request and waits for it to end, so it never writes to or reports on a
    /// renamed or finalized session.
    async fn stop_title_task(&self) {
        let task = lock_or_recover(&self.title_task, "title_task").take();
        if let Some(task) = task {
            task.abort();
            if let Err(e) = task.await
                && e.is_panic()
            {
                error!("title generation panicked: {e}");
            }
        }
    }

    /// Sends [`SessionCmd::Shutdown`] so the actor breaks after the current batch, then awaits
    /// the join handle. Cmd-driven exit keeps a clone held elsewhere from pinning the actor.
    pub(crate) async fn shutdown(self) {
        let Self {
            cmd_tx, actor_join, ..
        } = self;
        let (ack, rx) = oneshot::channel();
        if cmd_tx.send(SessionCmd::Shutdown { ack }).await.is_ok() {
            _ = rx.await;
        }
        drop(cmd_tx);
        let join = lock_or_recover(&actor_join, "actor_join").take();
        if let Some(j) = join
            && let Err(e) = j.await
        {
            if e.is_panic() {
                error!("session actor panicked: {e}");
            } else {
                warn!("session actor task cancelled: {e}");
            }
        }
    }

    /// Sends a cmd and awaits the `Outcome` ack, falling back to actor-gone on failure.
    async fn dispatch_outcome(&self, cmd: SessionCmd, rx: oneshot::Receiver<Outcome>) -> Outcome {
        self.request(cmd, rx).await.unwrap_or_else(|| Outcome {
            failure: self.actor_gone_failure(),
        })
    }

    async fn request<T>(&self, cmd: SessionCmd, rx: oneshot::Receiver<T>) -> Option<T> {
        self.cmd_tx.send(cmd).await.ok()?;
        rx.await.ok()
    }

    /// The actor-gone failure for a write the caller must know failed, in full the first time.
    fn actor_stopped_failure(&self) -> String {
        self.actor_gone_failure()
            .unwrap_or_else(|| "session writer task has stopped".to_owned())
    }

    /// Surfaces actor-gone failure once, including the underlying I/O cause if known.
    fn actor_gone_failure(&self) -> Option<String> {
        if !self.shared.surface_first_actor_gone() {
            return None;
        }
        warn!("session actor task is gone, so the recording was dropped");
        let cause = self.shared.last_flush_failure();
        let msg = match cause {
            Some(io_err) => format!(
                "session writer task has stopped after I/O error: {io_err}. \
                 Conversation history may be incomplete."
            ),
            None => "session writer task has stopped. Conversation history may be incomplete."
                .to_owned(),
        };
        Some(msg)
    }
}

// ── Resumed Sessions ──

/// A session's transcript as loaded from disk, with the state saved beside its messages.
#[derive(Debug, Default)]
pub struct LoadedTranscript {
    pub(crate) messages: Vec<Message>,
    /// Recorded prompt state the resumed conversation replays.
    pub(crate) context: Option<SessionContext>,
    pub(crate) compact: Option<CompactInfo>,
    pub(crate) tool_metadata: HashMap<String, ToolMetadata>,
    /// Every billed request the session recorded, which seeds its cost estimate.
    pub(crate) usage: Vec<UsageRecord>,
    /// The task list the transcript leaves, from its last `tasks` call or else its compaction.
    pub(crate) tasks: Vec<Task>,
    /// The file tracker state the session saved for reads the transcript still shows, which the
    /// engine restores where it still matches disk.
    pub(crate) file_snapshots: Vec<FileSnapshot>,
}

/// Live handle plus display-only extras the TUI uses to reconstruct what the user saw live.
pub struct ResumedSession {
    pub handle: SessionHandle,
    pub transcript: LoadedTranscript,
    pub title: Option<String>,
    /// Whether the session was archived, so resuming it appended an entry unarchiving it.
    pub unarchived: bool,
}

// ── Constructors ──

/// Starts a fresh session on `git_branch` and spawns its actor. The first record creates the file.
pub(crate) fn start(
    store: &SessionStore,
    model: &str,
    git_branch: Option<String>,
) -> SessionHandle {
    spawn_actor(SessionState::fresh(store.clone(), model, git_branch))
}

/// The new session ID and the old session's finalize failure.
#[derive(Debug)]
pub(crate) struct RollOutcome {
    pub(crate) new_id: String,
    pub(crate) finalize_failure: Option<String>,
}

/// Rolls `session` to a fresh handle on `git_branch` and finalizes the old one with `snapshots`,
/// its file tracker state.
pub(crate) async fn roll(
    session: &mut SessionHandle,
    store: &SessionStore,
    snapshots: Vec<FileSnapshot>,
    model: &str,
    git_branch: Option<String>,
) -> RollOutcome {
    let new_session = start(store, model, git_branch);
    let new_id = new_session.session_id().to_owned();
    let old_session = std::mem::replace(session, new_session);
    let finalize_failure = old_session.finalize(snapshots).await;
    RollOutcome {
        new_id,
        finalize_failure,
    }
}

/// The resumed transcript and the old session's finalize failure.
#[derive(Debug)]
pub(crate) struct RollIntoOutcome {
    pub(crate) transcript: LoadedTranscript,
    pub(crate) title: Option<String>,
    pub(crate) unarchived: bool,
    pub(crate) finalize_failure: Option<String>,
}

/// Loads and sanitizes the session at `target`, swaps it in, then finalizes the old one with
/// `snapshots`, its file tracker state. Fails without swapping when `target` is the live session,
/// whose append writer is in flight, or cannot be loaded.
pub(crate) async fn roll_into(
    session: &mut SessionHandle,
    store: &SessionStore,
    snapshots: Vec<FileSnapshot>,
    target: &Path,
) -> Result<RollIntoOutcome> {
    let data = load_session_data_from_path(target)?;
    if data.session_id == session.session_id() {
        return Err(SavedError::live(session.session_id()).into());
    }
    let ResumedSession {
        handle: target_handle,
        transcript,
        title,
        unarchived,
    } = resume_loaded(store, target, data)?;

    let old_session = std::mem::replace(session, target_handle);
    let finalize_failure = old_session.finalize(snapshots).await;
    Ok(RollIntoOutcome {
        transcript,
        title,
        unarchived,
        finalize_failure,
    })
}

/// Resumes the session file at `path`: loads, sanitizes, opens it for append, unarchives it, and
/// spawns the actor.
pub(super) fn resume_from_path(store: &SessionStore, path: &Path) -> Result<ResumedSession> {
    resume_loaded(store, path, load_session_data_from_path(path)?)
}

/// [`resume_from_path`] for the `data` already loaded from `path`.
fn resume_loaded(
    store: &SessionStore,
    path: &Path,
    mut data: SessionData,
) -> Result<ResumedSession> {
    let session_id = data.session_id;
    let mut writer = open_append_at(path)?;
    let pre_sanitize_len = data.messages.len();
    sanitize_resumed_messages(&mut data.messages);
    // Bail only when sanitize wipes a non-empty list, since chaining onto a dropped
    // `last_message_uuid` would corrupt the chain. A title-only file is fine because the next
    // message starts a fresh chain.
    if pre_sanitize_len > 0 && data.messages.is_empty() {
        bail!("session {session_id} has no messages left after repair");
    }
    retain_visible_file_snapshots(&mut data.file_snapshots, &data.messages);
    if data.archived {
        writer.set_archived(false)?;
    }

    let first_user_prompt_seen =
        data.messages.iter().any(|m| extract_user_text(m).is_some()) || data.title.is_some();
    let tasks = tasks::latest(&data.messages)
        .or_else(|| data.compact.as_ref().map(|compact| compact.tasks.clone()))
        .unwrap_or_default();
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
        transcript: LoadedTranscript {
            messages: data.messages,
            context: data.context,
            compact: data.compact,
            tool_metadata: data.tool_result_metadata,
            usage: data.usage,
            tasks,
            file_snapshots: data.file_snapshots,
        },
        title,
        unarchived: data.archived,
    })
}

fn spawn_actor(state: SessionState) -> SessionHandle {
    let session_id = Arc::clone(&state.session_id);
    let artifact_dir = Arc::clone(&state.artifact_dir);
    let shared = Arc::new(SharedState::default());
    let (cmd_tx, cmd_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let actor_shared = Arc::clone(&shared);
    let join = tokio::spawn(actor::run(state, cmd_rx, actor_shared));
    SessionHandle {
        cmd_tx,
        session_id,
        artifact_dir,
        shared,
        actor_join: Arc::new(Mutex::new(Some(join))),
        title_task: Arc::default(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    use time::macros::datetime;

    use super::*;
    use crate::file_tracker::{
        FileState, FileTracker, GateError, GatePurpose, LastView,
        testing::{record_tracked_file, seed_full_read},
    };
    use crate::message::{ContentBlock, Replay, Role};
    use crate::prompt::PromptParts;
    use crate::session::entry::{Entry, SessionInfo};
    use crate::session::handle::testing::record_write_call;
    use crate::session::store::listing::ProjectScope;
    use crate::session::store::local_testing::{
        saved_entries, saved_snapshot_paths, saved_titles, test_session_file,
    };
    use crate::session::store::testing::{
        TEST_PROJECT, isolated_store, seed_test_session, test_project_dir, test_store,
    };
    use crate::tool::tasks::testing::plan;
    use crate::tool::view::{DiffChunk, EditDiff, NumberedLine};

    /// The failure the first write after the actor stops reports.
    const STOPPED: &str =
        "session writer task has stopped. Conversation history may be incomplete.";

    /// The failure every later write reports once [`STOPPED`] was surfaced.
    const STOPPED_AGAIN: &str = "session writer task has stopped";

    /// Resumes the session whose file name carries `session_id`.
    fn resume_id(store: &SessionStore, session_id: &str) -> Result<ResumedSession> {
        resume_from_path(store, &store.find_session_path(session_id)?)
    }

    mod record_message {
        use super::*;

        #[tokio::test]
        async fn seeds_ai_title_only_for_first_user_text() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);

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
        async fn actor_gone_surfaces_failure_once_then_silences() {
            let handle = testing::dead("dead");

            let first = handle.record_message(Message::user("a")).await;
            let second = handle.record_message(Message::user("b")).await;

            assert_eq!(first.failure.as_deref(), Some(STOPPED),);
            assert_eq!(second.failure, None);
        }

        #[tokio::test]
        async fn actor_gone_carries_underlying_flush_error() {
            let handle = testing::dead("dead");
            handle
                .shared
                .record_flush_failure("permission denied: /readonly/path");

            let first = handle.record_message(Message::user("a")).await;

            assert_eq!(
                first.failure.as_deref(),
                Some(
                    "session writer task has stopped after I/O error: \
                     permission denied: /readonly/path. Conversation history may be incomplete."
                ),
            );
        }

        #[tokio::test]
        async fn actor_gone_surfaces_after_prior_flush_failure_was_surfaced() {
            let handle = testing::dead("dead");
            handle.shared.record_flush_failure("disk full");
            assert!(handle.shared.surface_first_flush_failure());

            let first = handle.record_message(Message::user("a")).await;

            assert_eq!(
                first.failure.as_deref(),
                Some(
                    "session writer task has stopped after I/O error: disk full. \
                     Conversation history may be incomplete."
                ),
            );
        }

        #[tokio::test]
        async fn resumed_session_does_not_seed_ai_title() {
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let session_id = original.session_id().to_owned();
            original.record_message(Message::user("hello")).await;
            original.finish(Vec::new()).await;
            drop(original);

            let resumed = resume_id(&store, &session_id).unwrap().handle;
            let outcome = resumed.record_message(Message::user("more text")).await;
            assert!(outcome.ai_title_seed.is_none());
        }
    }

    mod rewind {
        use super::*;

        #[tokio::test]
        async fn to_before_the_first_prompt_stops_the_title_request_it_started() {
            let dir = tempfile::tempdir().unwrap();
            let handle = start(&test_store(dir.path()), "m", None);
            let refused = handle.record_message(Message::user("refused")).await;
            let (started_tx, started) = oneshot::channel();
            let (alive, mut ended) = oneshot::channel::<()>();
            handle.set_title_task(tokio::spawn(async move {
                let _alive = alive;
                started_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            }));
            started.await.unwrap();

            let outcome = handle
                .rewind(refused.checkpoint.expect("the actor is running"))
                .await;

            assert_eq!(outcome.failure, None);
            assert_eq!(ended.try_recv(), Err(oneshot::error::TryRecvError::Closed));
        }

        #[tokio::test]
        async fn to_after_the_first_prompt_keeps_the_title_request() {
            let dir = tempfile::tempdir().unwrap();
            let handle = start(&test_store(dir.path()), "m", None);
            handle.record_message(Message::user("first")).await;
            let refused = handle.record_message(Message::user("refused")).await;
            let (started_tx, started) = oneshot::channel();
            let (alive, mut ended) = oneshot::channel::<()>();
            handle.set_title_task(tokio::spawn(async move {
                let _alive = alive;
                started_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            }));
            started.await.unwrap();

            handle
                .rewind(refused.checkpoint.expect("the actor is running"))
                .await;

            assert_eq!(ended.try_recv(), Err(oneshot::error::TryRecvError::Empty));
            handle.finalize(Vec::new()).await;
        }
    }

    mod record_tool_metadata_batch {
        use super::*;

        #[tokio::test]
        async fn writes_all_non_default_in_one_cmd() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
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
                            truncated_total: Some(2),
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
                "default-only sidecars are skipped, so only t1 and t3 land",
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
                data.tool_result_metadata.get("t3").unwrap().truncated_total,
                Some(2),
            );
        }

        #[tokio::test]
        async fn actor_drops_ack_surfaces_actor_gone_failure() {
            // Five healthy acks cover each cmd `dispatch_outcome` sends. The actor drops the sixth.
            let handle = testing::acks_then_drops("acks-then-drops", 5);

            let title = handle.append_ai_title("ok".to_owned()).await;
            let manual = handle.set_manual_title("manual".to_owned()).await;
            let finish = handle.finish(Vec::new()).await;
            let usage = handle.record_usage(Vec::new()).await;
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
            assert!(
                manual.failure.is_none(),
                "SetManualTitle is acked healthily"
            );
            assert!(finish.failure.is_none(), "Finish is acked healthily");
            assert!(usage.failure.is_none(), "Usage is acked healthily");
            assert!(
                batch_ok.failure.is_none(),
                "ToolMetadata is acked healthily"
            );
            assert_eq!(
                batch_dropped.failure.as_deref(),
                Some(STOPPED),
                "dropped ack surfaces actor-gone",
            );

            // Shutting down runs the stand-in actor's loop exit.
            handle.shutdown().await;
        }
    }

    mod append_ai_title {
        use super::*;

        #[tokio::test]
        async fn writes_title_entry_and_supersedes_first_prompt_on_list() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
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
    }

    mod set_manual_title {
        use super::*;

        async fn title_notification_race(accept_before_rename: bool) {
            use wiremock::matchers::{method, path as wm_path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            use crate::client::anthropic::local_testing::{completion_body, test_client};
            use crate::client::anthropic::testing::api_key;
            use crate::protocol::{Event, EventSink};
            use crate::session::title_generator;

            #[derive(Clone)]
            struct TitleSink(mpsc::UnboundedSender<Event>);

            impl EventSink for TitleSink {
                fn send(&self, event: Event) -> Result<()> {
                    Ok(self.0.send(event)?)
                }
            }

            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(completion_body(r#"{"title":"AI title"}"#)),
                )
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let mut session = start(&test_store(dir.path()), "claude-haiku-5-5", None);
            session.record_message(Message::user("first prompt")).await;
            let session_id = session.session_id().to_owned();
            let actor = session.cmd_tx.clone();
            let (cmd_tx, mut commands) = mpsc::channel(8);
            session.cmd_tx = cmd_tx;
            let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
            title_generator::spawn(
                test_client(server.uri(), api_key(), "claude-haiku-5-5"),
                session.clone(),
                TitleSink(tx),
                "prompt".to_owned(),
            );
            let usage = commands.recv().await.unwrap();
            assert!(matches!(usage, SessionCmd::Usage { .. }));
            actor.send(usage).await.unwrap();
            let SessionCmd::AppendAiTitle { title, ack } = commands.recv().await.unwrap() else {
                panic!("title generator must request AI append");
            };
            if accept_before_rename {
                let (accepted, accepted_rx) = tokio::sync::oneshot::channel();
                actor
                    .send(SessionCmd::AppendAiTitle {
                        title: title.clone(),
                        ack: accepted,
                    })
                    .await
                    .unwrap();
                assert!(accepted_rx.await.unwrap().failure.is_none());
            }
            let renamed = session.clone();
            let rename =
                tokio::spawn(
                    async move { renamed.set_manual_title("Manual title".to_owned()).await },
                );
            let manual = commands.recv().await.unwrap();
            assert!(matches!(manual, SessionCmd::SetManualTitle { .. }));

            actor.send(manual).await.unwrap();
            assert!(rename.await.unwrap().failure.is_none());
            if accept_before_rename {
                _ = ack.send(Outcome { failure: None });
            } else {
                actor
                    .send(SessionCmd::AppendAiTitle { title, ack })
                    .await
                    .unwrap();
            }

            assert_eq!(
                test_store(dir.path())
                    .load_session_data(&session_id)
                    .unwrap()
                    .title
                    .unwrap()
                    .title,
                "Manual title"
            );
            assert_eq!(
                events.recv().await,
                None,
                "a completed manual rename must suppress the delayed AI event"
            );
        }

        #[tokio::test]
        async fn writes_user_provided_title_and_latches_shared_flag() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let sid = handle.session_id().to_owned();

            assert!(
                !handle.manual_title_set(),
                "fresh handle must start unlatched"
            );

            handle.record_message(Message::user("seed")).await;
            let outcome = handle.set_manual_title("User wins".to_owned()).await;

            assert!(outcome.failure.is_none(), "healthy actor acks cleanly");
            assert!(
                handle.manual_title_set(),
                "the shared flag must latch immediately on dispatch",
            );

            handle.finish(Vec::new()).await;
            drop(handle);

            let sessions = store.list().unwrap();
            let session = sessions.iter().find(|s| s.session_id == sid).unwrap();
            assert_eq!(session.title.as_ref().unwrap().title, "User wins");
        }

        #[tokio::test]
        async fn suppresses_ai_notification_when_actor_skips_append() {
            title_notification_race(false).await;
        }

        #[tokio::test]
        async fn suppresses_ai_notification_after_accepted_append() {
            title_notification_race(true).await;
        }

        #[tokio::test]
        async fn aborting_the_title_usage_write_still_delivers_its_cost() {
            use wiremock::matchers::{method, path as wm_path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            use crate::client::anthropic::local_testing::{completion_body, test_client};
            use crate::client::anthropic::testing::api_key;
            use crate::protocol::testing::CapturingSink;
            use crate::session::title_generator;

            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(completion_body(r#"{"title":"AI title"}"#)),
                )
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let mut session = start(&test_store(dir.path()), "claude-haiku-5-5", None);
            session.record_message(Message::user("first prompt")).await;
            let session_id = session.session_id().to_owned();
            let actor = session.cmd_tx.clone();
            let (cmd_tx, mut commands) = mpsc::channel(8);
            session.cmd_tx = cmd_tx;
            let usage = title_generator::spawn(
                test_client(server.uri(), api_key(), "claude-haiku-5-5"),
                session.clone(),
                CapturingSink::default(),
                "prompt".to_owned(),
            );
            let SessionCmd::Usage {
                records,
                ack: _unanswered,
            } = commands.recv().await.unwrap()
            else {
                panic!("title generator must record its usage first");
            };
            let (written, written_rx) = oneshot::channel();
            actor
                .send(SessionCmd::Usage {
                    records,
                    ack: written,
                })
                .await
                .unwrap();
            assert!(written_rx.await.unwrap().failure.is_none());

            let renamed = session.clone();
            let rename =
                tokio::spawn(
                    async move { renamed.set_manual_title("Manual title".to_owned()).await },
                );
            actor.send(commands.recv().await.unwrap()).await.unwrap();
            assert!(rename.await.unwrap().failure.is_none());

            let data = test_store(dir.path())
                .load_session_data(&session_id)
                .unwrap();
            assert_eq!(
                data.usage,
                [usage
                    .await
                    .expect("a cost the file keeps must also reach the engine")]
            );
        }

        #[tokio::test]
        async fn actor_gone_surfaces_failure_but_still_latches_flag() {
            let handle = testing::dead("dead");

            let outcome = handle.set_manual_title("User wins".to_owned()).await;

            assert!(outcome.failure.is_some(), "actor-gone must surface failure");
            assert!(
                handle.manual_title_set(),
                "flag latches regardless of dispatch"
            );
        }
    }

    mod compact {
        use super::*;

        #[tokio::test]
        async fn writes_boundary_and_acks_with_pre_count() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let handle = start(&store, "m", None);
            let sid = handle.session_id().to_owned();

            handle.record_message(Message::user("a")).await;
            handle.record_message(Message::assistant("b")).await;

            let outcome = handle
                .compact(
                    CompactBoundary {
                        instructions: Some("focus".to_owned()),
                        ..testing::boundary("synth summary")
                    },
                    Message::user("post"),
                )
                .await;

            assert_eq!(outcome.pre_count, 2, "pre_count snapshots before reset");
            assert!(outcome.checkpoint.is_some());
            assert!(outcome.failure.is_none(), "healthy actor acks cleanly");
            handle.finish(Vec::new()).await;
            drop(handle);

            assert!(
                saved_entries(dir.path(), &sid)
                    .iter()
                    .any(|entry| matches!(entry, Entry::Compact { .. })),
                "the committed boundary is on disk",
            );
        }

        #[tokio::test]
        async fn reports_every_failure_to_reach_the_actor() {
            async fn compact(handle: &SessionHandle) -> CompactOutcome {
                handle
                    .compact(testing::boundary("s"), Message::user("synth"))
                    .await
            }

            let dead = testing::dead("dead");
            for expected in [STOPPED, STOPPED_AGAIN] {
                let outcome = compact(&dead).await;
                assert_eq!(outcome.pre_count, 0, "no actor means no pre-count signal");
                assert!(outcome.checkpoint.is_none());
                assert_eq!(outcome.failure.as_deref(), Some(expected));
            }

            let stalled = testing::acks_then_drops("stalled", 1);
            assert!(compact(&stalled).await.checkpoint.is_some());
            let dropped = compact(&stalled).await;
            assert!(dropped.checkpoint.is_none());
            assert_eq!(dropped.failure.as_deref(), Some(STOPPED));
            stalled.shutdown().await;
        }
    }

    mod archive {
        use super::*;

        #[tokio::test]
        async fn holds_until_the_next_resume_unarchives_the_session() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let original = start(&store, "m", None);
            let session_id = original.session_id().to_owned();
            original.record_message(Message::user("hello")).await;

            let outcome = original.archive().await;
            assert!(outcome.failure.is_none());
            original.shutdown().await;
            assert!(store.load_session_data(&session_id).unwrap().archived);

            let resumed = resume_id(&store, &session_id).unwrap();
            assert!(resumed.unarchived);
            assert_eq!(resumed.transcript.messages, [Message::user("hello")]);
            resumed.handle.shutdown().await;
            assert!(!store.load_session_data(&session_id).unwrap().archived);

            let again = resume_id(&store, &session_id).unwrap();
            assert!(!again.unarchived);
            again.handle.shutdown().await;
            let archive_states: Vec<_> = saved_entries(dir.path(), &session_id)
                .iter()
                .filter_map(|entry| match entry {
                    Entry::Archive { archived, .. } => Some(*archived),
                    _ => None,
                })
                .collect();
            assert_eq!(
                archive_states,
                [true, false],
                "resuming an active session appends no archive entry",
            );
        }

        #[tokio::test]
        async fn reports_every_failure_to_reach_the_actor() {
            let dead = testing::dead("dead");
            assert_eq!(dead.archive().await.failure.as_deref(), Some(STOPPED));
            assert_eq!(
                dead.archive().await.failure.as_deref(),
                Some(STOPPED_AGAIN),
                "a second failure must not read as success once actor-gone was surfaced",
            );

            let stalled = testing::acks_then_drops("stalled", 1);
            assert_eq!(stalled.archive().await.failure, None);
            assert_eq!(stalled.archive().await.failure.as_deref(), Some(STOPPED));
            stalled.shutdown().await;
        }
    }

    mod finish {
        use super::*;

        #[tokio::test]
        async fn writes_summary_with_count() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
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
        async fn empty_session_leaves_no_file() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let handle = start(&store, "m", None);
            handle.finish(Vec::new()).await;
            drop(handle);

            let project_dir = test_project_dir(dir.path());
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
        async fn skips_summary_on_empty_resume() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let original = start(&store, "m", None);
            let session_id = original.session_id().to_owned();
            original.record_message(Message::user("hello")).await;
            original.finish(Vec::new()).await;
            drop(original);

            let resumed = resume_id(&store, &session_id).unwrap().handle;
            resumed.finish(Vec::new()).await;
            drop(resumed);

            let entries = saved_entries(dir.path(), &session_id);
            let summary_count = entries
                .iter()
                .filter(|entry| matches!(entry, Entry::Summary { .. }))
                .count();
            assert_eq!(summary_count, 1);
        }

        #[tokio::test]
        async fn actor_gone_surfaces_failure_once_then_silences() {
            let handle = testing::dead("dead");

            let first = handle.finish(Vec::new()).await;
            let second = handle.finish(Vec::new()).await;

            assert_eq!(first.failure.as_deref(), Some(STOPPED),);
            assert_eq!(second.failure, None);
        }

        #[tokio::test]
        async fn persists_one_file_snapshot_per_tracked_file() {
            let dir = tempfile::tempdir().unwrap();
            let files_dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let tracker = FileTracker::default();
            let handle = start(&store, "m", None);
            let sid = handle.session_id().to_owned();

            handle.record_message(Message::user("trigger")).await;
            let expected_paths: std::collections::HashSet<std::path::PathBuf> =
                ["a.rs", "b.rs", "c.rs"]
                    .into_iter()
                    .map(|name| {
                        let path = files_dir.path().join(name);
                        record_tracked_file(&tracker, &path, name.as_bytes(), "toolu_1");
                        path
                    })
                    .collect();

            handle.finish(tracker.snapshot_all()).await;
            drop(handle);

            let snapshot_paths: std::collections::HashSet<std::path::PathBuf> =
                saved_entries(dir.path(), &sid)
                    .into_iter()
                    .filter_map(|entry| match entry {
                        Entry::FileSnapshot { snapshot } => Some(snapshot.path),
                        _ => None,
                    })
                    .collect();
            assert_eq!(
                snapshot_paths, expected_paths,
                "every tracked file lands as its own FileSnapshot",
            );
        }
    }

    mod finalize {
        use super::*;

        #[tokio::test]
        async fn writes_summary_then_succeeds() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let handle = start(&store, "m", None);
            let sid = handle.session_id().to_owned();
            handle.record_message(Message::user("hi")).await;

            let failure = handle.finalize(Vec::new()).await;

            assert!(failure.is_none(), "healthy finalize reports no failure");
            let entries = saved_entries(dir.path(), &sid);
            assert!(
                matches!(
                    entries.last(),
                    Some(Entry::Summary {
                        message_count: 1,
                        ..
                    })
                ),
                "summary lands on disk: {entries:?}",
            );
        }

        #[tokio::test]
        async fn fails_when_actor_dead() {
            let handle = testing::dead("dead");

            let failure = handle.finalize(Vec::new()).await;

            assert_eq!(failure.as_deref(), Some(STOPPED));
        }

        #[tokio::test]
        async fn ends_the_title_task_before_it_returns() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let (started_tx, started) = oneshot::channel();
            let (alive, mut ended) = oneshot::channel::<()>();
            handle.set_title_task(tokio::spawn(async move {
                let _alive = alive;
                started_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            }));
            started.await.unwrap();

            let failure = handle.finalize(Vec::new()).await;

            assert_eq!(failure, None);
            assert_eq!(ended.try_recv(), Err(oneshot::error::TryRecvError::Closed));
        }

        #[tokio::test]
        async fn writes_the_summary_after_the_title_task_panics() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let handle = start(&store, "m", None);
            let sid = handle.session_id().to_owned();
            handle.record_message(Message::user("hi")).await;
            let (started_tx, started) = oneshot::channel();
            handle.set_title_task(tokio::spawn(async move {
                started_tx.send(()).unwrap();
                panic!("deliberate title task panic for testing");
            }));
            started.await.unwrap();

            let failure = handle.finalize(Vec::new()).await;

            assert_eq!(failure, None);
            let entries = saved_entries(dir.path(), &sid);
            assert!(
                matches!(
                    entries.last(),
                    Some(Entry::Summary {
                        message_count: 1,
                        ..
                    })
                ),
                "summary lands on disk: {entries:?}",
            );
        }
    }

    mod shutdown {
        use super::*;

        #[tokio::test]
        async fn after_record_completes_after_actor_exits() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let handle = start(&store, "m", None);
            let sid = handle.session_id().to_owned();

            handle.record_message(Message::user("hello")).await;
            handle.shutdown().await;

            let entries = saved_entries(dir.path(), &sid);
            assert!(
                entries
                    .iter()
                    .any(|entry| matches!(entry, Entry::Message { .. })),
                "actor must have flushed before shutdown returned: {entries:?}",
            );
        }

        #[tokio::test]
        async fn dead_actor_completes_without_panicking() {
            let handle = testing::dead("dead");
            handle.shutdown().await;
        }

        #[tokio::test]
        async fn actor_panic_completes_without_panicking() {
            let handle = testing::panicked("panicked");

            handle.shutdown().await;
        }

        #[tokio::test]
        async fn cancelled_actor_completes_without_panicking() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            if let Some(j) = handle.actor_join.lock().unwrap().as_ref() {
                j.abort();
            }
            handle.shutdown().await;
        }
    }

    mod start {
        use super::*;

        #[tokio::test]
        async fn does_not_materialize_file_until_first_record() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let handle = start(&store, "test-model", None);

            let project_dir = test_project_dir(dir.path());
            assert!(
                std::fs::read_dir(&project_dir).unwrap().next().is_none(),
                "fresh session must not create a file before the first record",
            );

            handle.record_message(Message::user("first")).await;
            assert!(
                test_session_file(dir.path(), handle.session_id()).exists(),
                "first record_message should materialize the session file",
            );
        }

        #[tokio::test]
        async fn keeps_artifacts_beside_the_file_its_first_record_creates() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());

            let handle = start(&store, "test-model", None);

            let id = handle.session_id().to_owned();
            let expected = test_project_dir(dir.path()).join(&id);
            assert_eq!(handle.artifact_dir().to_path_buf(), expected);
            handle.record_message(Message::user("first")).await;
            assert_eq!(
                test_session_file(dir.path(), &id).with_file_name(&id),
                expected
            );
        }
    }

    mod roll {
        use super::*;

        #[tokio::test]
        async fn swaps_session_in_place_and_finalizes_old_with_summary() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut session = start(&store, "test-model", None);
            let original_id = session.session_id().to_owned();
            session.record_message(Message::user("original")).await;

            let outcome = roll(
                &mut session,
                &store,
                Vec::new(),
                "test-model",
                Some("feat/x".to_owned()),
            )
            .await;

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

            let old = saved_entries(dir.path(), &original_id);
            assert!(
                matches!(&old[..], [Entry::Header(header), .., Entry::Summary { .. }]
                    if header.session_id == original_id),
                "old JSONL keeps its header and gets a summary entry: {old:?}",
            );

            session.record_message(Message::user("rolled")).await;
            session.finish(Vec::new()).await;
            let new = saved_entries(dir.path(), &outcome.new_id);
            assert!(
                matches!(new.first(), Some(Entry::Header(header))
                    if header.git_branch.as_deref() == Some("feat/x")),
                "new JSONL header records the branch: {new:?}",
            );
        }

        #[tokio::test]
        async fn persists_the_tracker_snapshots_into_the_old_session() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let tracker = FileTracker::default();
            let seed = dir.path().join("seed.txt");
            std::fs::write(&seed, b"hello").unwrap();
            seed_full_read(&tracker, &seed);
            let mut session = start(&store, "test-model", None);
            let original_id = session.session_id().to_owned();
            session.record_message(Message::user("seed")).await;

            let outcome = roll(
                &mut session,
                &store,
                tracker.snapshot_all(),
                "test-model",
                None,
            )
            .await;

            assert_eq!(outcome.finalize_failure, None);
            assert_eq!(
                saved_snapshot_paths(&saved_entries(dir.path(), &original_id)),
                [seed.as_path()],
                "snapshot survived into the old JSONL",
            );
        }
    }

    mod roll_into {
        use super::*;

        #[tokio::test]
        async fn swaps_to_target_and_finalizes_old() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());

            let target = start(&store, "test-model", None);
            let target_id = target.session_id().to_owned();
            target.record_message(Message::user("target prompt")).await;
            target
                .record_message(Message::assistant("target reply"))
                .await;
            target.finish(Vec::new()).await;
            drop(target);

            let mut session = start(&store, "test-model", None);
            let original_id = session.session_id().to_owned();
            session.record_message(Message::user("live")).await;

            let target = test_session_file(dir.path(), &target_id);
            let outcome = roll_into(&mut session, &store, Vec::new(), &target)
                .await
                .expect("roll_into must succeed for healthy target");

            assert_eq!(
                session.session_id(),
                target_id,
                "handle now points at target"
            );
            assert_eq!(
                outcome.transcript.messages,
                [
                    Message::user("target prompt"),
                    Message::assistant("target reply")
                ],
                "resumed payload carries the target's transcript",
            );
            assert!(
                outcome.finalize_failure.is_none(),
                "healthy roll_into reports no finalize failure",
            );

            let entries = saved_entries(dir.path(), &original_id);
            assert!(
                matches!(
                    entries.last(),
                    Some(Entry::Summary {
                        message_count: 1,
                        ..
                    })
                ),
                "old JSONL gets a summary: {entries:?}",
            );
        }

        #[tokio::test]
        async fn target_load_failure_keeps_live_session_intact() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut session = start(&store, "test-model", None);
            let original_id = session.session_id().to_owned();
            session.record_message(Message::user("live")).await;

            let missing = dir.path().join("missing-target.jsonl");
            let err = roll_into(&mut session, &store, Vec::new(), &missing)
                .await
                .unwrap_err();

            assert_eq!(
                err.to_string(),
                format!("failed to open {}", missing.display())
            );
            assert_eq!(
                session.session_id(),
                original_id,
                "live session id unchanged after failure",
            );
        }

        #[tokio::test]
        async fn rejects_resuming_into_live_session_id() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut session = start(&store, "test-model", None);
            let live_id = session.session_id().to_owned();
            session.record_message(Message::user("live")).await;

            let live_path = test_session_file(dir.path(), &live_id);
            let err = roll_into(&mut session, &store, Vec::new(), &live_path)
                .await
                .expect_err("self-resume must be rejected");
            assert_eq!(
                err.to_string(),
                format!("session {} is the live session", &live_id[..8])
            );
            assert_eq!(session.session_id(), live_id, "live session unchanged");
        }

        #[tokio::test]
        async fn finalizes_the_old_session_with_its_snapshots_and_returns_the_target_s_snapshots() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let tracker = FileTracker::default();

            let target = start(&store, "test-model", None);
            let target_id = target.session_id().to_owned();
            target.record_message(Message::user("target")).await;
            let target_seed = dir.path().join("target.txt");
            let target_tracker = FileTracker::default();
            record_write_call(&target, &target_tracker, "toolu_target", &target_seed, true).await;
            let target_snapshots = target_tracker.snapshot_all();
            target.finish(target_snapshots).await;
            drop(target);

            let live_seed = dir.path().join("live.txt");
            std::fs::write(&live_seed, b"live body").unwrap();
            seed_full_read(&tracker, &live_seed);
            let mut session = start(&store, "test-model", None);
            let original_id = session.session_id().to_owned();
            session.record_message(Message::user("live")).await;

            let target = test_session_file(dir.path(), &target_id);
            let outcome = roll_into(&mut session, &store, tracker.snapshot_all(), &target)
                .await
                .expect("roll_into must succeed");

            assert_eq!(
                saved_snapshot_paths(&saved_entries(dir.path(), &original_id)),
                [live_seed.as_path()],
                "live tracker snapshot drains into the old JSONL",
            );
            let loaded: Vec<_> = outcome
                .transcript
                .file_snapshots
                .into_iter()
                .map(|s| s.path)
                .collect();
            assert_eq!(loaded, [target_seed]);
        }
    }

    mod resume_from_path {
        use super::*;

        #[tokio::test]
        async fn loads_messages_and_keeps_session_id() {
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let session_id = original.session_id().to_owned();
            original.record_message(Message::user("hello")).await;
            original.record_message(Message::assistant("hi")).await;
            original.finish(Vec::new()).await;
            drop(original);

            let ResumedSession {
                handle: resumed,
                transcript,
                ..
            } = resume_id(&store, &session_id).unwrap();
            assert_eq!(resumed.session_id(), session_id);
            assert_eq!(transcript.messages.len(), 2);
        }

        #[tokio::test]
        async fn keeps_artifacts_beside_the_file_wherever_it_lives() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let original = start(&store, "m", None);
            let session_id = original.session_id().to_owned();
            original.record_message(Message::user("hello")).await;
            original.shutdown().await;
            let outside = dir.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            let copied = outside.join("copied.jsonl");
            std::fs::copy(test_session_file(dir.path(), &session_id), &copied).unwrap();

            let ResumedSession { handle, .. } = resume_from_path(&store, &copied).unwrap();

            assert_eq!(
                handle.artifact_dir().to_path_buf(),
                outside.join(&session_id)
            );
        }

        #[tokio::test]
        async fn works_on_unfinished_session() {
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let session_id = original.session_id().to_owned();
            original.record_message(Message::user("hello")).await;
            original.shutdown().await;

            let ResumedSession { transcript, .. } = resume_id(&store, &session_id).unwrap();
            assert_eq!(transcript.messages.len(), 1);
        }

        #[tokio::test]
        async fn title_only_session_keeps_title_on_next_message() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let session_id = "title-only-fixture";
            seed_test_session(
                &store,
                session_id,
                Some("User-named"),
                None,
                time::macros::datetime!(2026-01-01 00:00:00 UTC),
            );

            let resumed = resume_id(&store, session_id).expect("title-only session must resume");
            assert!(
                resumed.transcript.messages.is_empty(),
                "no messages on disk yet"
            );
            assert_eq!(resumed.title.as_deref(), Some("User-named"));

            resumed.handle.record_message(Message::user("hi")).await;
            resumed.handle.finish(Vec::new()).await;
            resumed.handle.shutdown().await;

            let entries = saved_entries(dir.path(), session_id);
            assert_eq!(
                saved_titles(&entries),
                ["User-named"],
                "next message must not add a first-prompt title alongside the existing one",
            );
            let message_count = entries
                .iter()
                .filter(|entry| matches!(entry, Entry::Message { .. }))
                .count();
            assert_eq!(message_count, 1, "the resumed message must reach disk");
        }

        #[tokio::test]
        async fn empty_session_errors() {
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let session_id = original.session_id().to_owned();
            original.finish(Vec::new()).await;
            drop(original);

            let err = resume_id(&store, &session_id)
                .err()
                .expect("a session with no messages must not resume");

            assert_eq!(err.to_string(), format!("session not found: {session_id}"));
        }

        #[tokio::test]
        async fn all_messages_sanitized_errors() {
            // Sanitize drops the lone unresolved tool_use, so the empty chain makes resume bail.
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let session_id = original.session_id().to_owned();
            original
                .record_message(Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::tool_use(
                        "unresolved",
                        "bash",
                        serde_json::Value::Null,
                    )],
                })
                .await;
            drop(original);

            let err = resume_id(&store, &session_id)
                .err()
                .expect("all messages sanitized must be an error");

            assert_eq!(
                format!("{err:#}"),
                format!("session {session_id} has no messages left after repair"),
            );
        }

        #[tokio::test]
        async fn drops_assistant_message_with_only_unresolved_tool_use() {
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let session_id = original.session_id().to_owned();
            original.record_message(Message::user("do X")).await;
            original
                .record_message(Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::tool_use(
                        "unresolved",
                        "bash",
                        serde_json::Value::Null,
                    )],
                })
                .await;
            original.shutdown().await;

            let ResumedSession { transcript, .. } = resume_id(&store, &session_id).unwrap();

            assert_eq!(
                transcript.messages,
                [Message::user("do X")],
                "assistant-only-tool-use should be dropped",
            );
        }

        #[tokio::test]
        async fn preserves_parent_chain_on_next_record() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let original = start(&store, "m", None);
            let session_id = original.session_id().to_owned();
            original.record_message(Message::user("hello")).await;
            original.record_message(Message::assistant("hi")).await;
            original.finish(Vec::new()).await;
            drop(original);

            let resumed = resume_id(&store, &session_id).unwrap().handle;
            resumed.record_message(Message::user("follow up")).await;
            resumed.finish(Vec::new()).await;
            drop(resumed);

            let entries = saved_entries(dir.path(), &session_id);

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
            assert!(msg_uuids[0].1.is_none());
            assert_eq!(msg_uuids[1].1, Some(msg_uuids[0].0));
            assert_eq!(
                msg_uuids[2].1,
                Some(msg_uuids[1].0),
                "post-resume message chains to pre-resume tail",
            );
        }

        #[tokio::test]
        async fn restores_unchanged_snapshots_whose_tool_result_survives() {
            let dir = tempfile::tempdir().unwrap();
            let files_dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let tracker = FileTracker::default();
            let handle = start(&store, "m", None);
            let session_id = handle.session_id().to_owned();
            handle.record_message(Message::user("hi")).await;
            let path_a = files_dir.path().join("a.rs");
            let path_b = files_dir.path().join("b.rs");
            record_write_call(&handle, &tracker, "toolu_a", &path_a, true).await;
            record_write_call(&handle, &tracker, "toolu_b", &path_b, false).await;
            handle.finish(tracker.snapshot_all()).await;
            drop(handle);

            let resumed = resume_id(&store, &session_id).unwrap();
            let (resumed_tracker, _) = FileTracker::restored(resumed.transcript.file_snapshots);

            let restored =
                resumed_tracker.verify_current_content(&path_a, b"alpha", GatePurpose::Edit);
            assert_eq!(restored, Ok(()));
            let unanswered =
                resumed_tracker.verify_current_content(&path_b, b"alpha", GatePurpose::Edit);
            assert_eq!(
                unanswered,
                Err(GateError::NeverRead {
                    path: path_b.clone(),
                    purpose: GatePurpose::Edit,
                }),
                "resume drops the unanswered call, so its write must be read again",
            );
        }

        #[tokio::test]
        async fn drops_drifted_snapshots() {
            let dir = tempfile::tempdir().unwrap();
            let files_dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let tracker = FileTracker::default();
            let handle = start(&store, "m", None);
            let session_id = handle.session_id().to_owned();
            handle.record_message(Message::user("hi")).await;
            let path_a = files_dir.path().join("a.rs");
            record_write_call(&handle, &tracker, "toolu_a", &path_a, true).await;
            handle.finish(tracker.snapshot_all()).await;
            drop(handle);

            std::fs::write(&path_a, b"externally edited bytes").unwrap();

            let resumed = resume_id(&store, &session_id).unwrap();
            let (resumed_tracker, _) = FileTracker::restored(resumed.transcript.file_snapshots);

            let result = resumed_tracker.verify_current_content(
                &path_a,
                b"externally edited bytes",
                GatePurpose::Edit,
            );
            assert_eq!(
                result,
                Err(GateError::NeverRead {
                    path: path_a.clone(),
                    purpose: GatePurpose::Edit,
                }),
                "drifted file must hit the must-read-first gate",
            );
        }

        #[tokio::test]
        async fn drops_snapshots_for_missing_files() {
            let dir = tempfile::tempdir().unwrap();
            let files_dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let tracker = FileTracker::default();
            let handle = start(&store, "m", None);
            let session_id = handle.session_id().to_owned();
            handle.record_message(Message::user("hi")).await;
            let path_a = files_dir.path().join("a.rs");
            record_write_call(&handle, &tracker, "toolu_a", &path_a, true).await;
            handle.finish(tracker.snapshot_all()).await;
            drop(handle);

            std::fs::remove_file(&path_a).unwrap();

            let resumed = resume_id(&store, &session_id).unwrap();
            let (resumed_tracker, _) = FileTracker::restored(resumed.transcript.file_snapshots);
            let result = resumed_tracker.verify_current_content(&path_a, b"", GatePurpose::Edit);
            assert_eq!(
                result,
                Err(GateError::NeverRead {
                    path: path_a.clone(),
                    purpose: GatePurpose::Edit,
                }),
            );
        }
    }

    mod session_format {
        use super::*;

        /// Writes a session holding every entry type and content block, with messages, contexts,
        /// and sidecars after its compaction, since a resume drops those from before it. Returns
        /// the file masked, which a release copies as its fixture.
        async fn write_every_entry(dir: &Path) -> String {
            let session = spawn_actor(SessionState::fresh(
                test_store(dir),
                "m",
                Some("main".to_owned()),
            ));
            let session_id = session.session_id().to_owned();
            let mut prompt = PromptParts::default();
            "2026-01-01".clone_into(&mut prompt.seen.date);

            session
                .record_message_with_context(
                    Message::user("Edit f.rs"),
                    Some(prompt.session_context()),
                )
                .await;
            session
                .record_message(Message::assistant("Which change?"))
                .await;
            session.record_usage(vec![usage("5m", 1)]).await;
            session.append_ai_title("Edit f.rs".to_owned()).await;
            session.set_manual_title("Fix f.rs".to_owned()).await;
            session.record_usage(vec![usage("1h", 2)]).await;
            session
                .compact(
                    CompactBoundary {
                        summary: "Asked to edit f.rs.".to_owned(),
                        instructions: Some("focus on f.rs".to_owned()),
                        tasks: plan(),
                        context: prompt.session_context(),
                    },
                    Message::user("Summary: asked to edit f.rs."),
                )
                .await;

            "2026-01-02".clone_into(&mut prompt.seen.date);
            session
                .record_message_with_context(
                    Message::user("Rename main in f.rs"),
                    Some(prompt.session_context()),
                )
                .await;
            record_tool_turns(&session).await;
            session.record_message(Message::assistant("Done.")).await;
            let refused = session.record_message(Message::user("Delete f.rs")).await;
            session
                .rewind(refused.checkpoint.expect("the actor is running"))
                .await;
            session
                .record_usage(vec![usage("1h", 3), usage("1h", 4)])
                .await;
            session.archive().await;
            session
                .finish(vec![
                    FileSnapshot {
                        path: PathBuf::from("/repo/f.rs"),
                        state: FileState {
                            content_hash: 42,
                            last_view: LastView::Full,
                            recorded_at: datetime!(2026-01-01 00:00:01 UTC),
                            tool_use_id: "t1".to_owned(),
                        },
                    },
                    FileSnapshot {
                        path: PathBuf::from("/repo/g.rs"),
                        state: FileState {
                            content_hash: 43,
                            last_view: LastView::Partial {
                                ranges: vec![(1, 20)],
                                total_lines: 40,
                            },
                            recorded_at: datetime!(2026-01-01 00:00:02 UTC),
                            tool_use_id: "t2".to_owned(),
                        },
                    },
                ])
                .await;
            session.shutdown().await;

            mask_volatile(&std::fs::read_to_string(test_session_file(dir, &session_id)).unwrap())
        }

        /// A billed request whose token counts all differ, so a swapped field changes the snapshot.
        fn usage(cache_ttl: &str, scale: u32) -> UsageRecord {
            serde_json::from_value(serde_json::json!({
                "model": "claude-sonnet-5-5",
                "cache_ttl": cache_ttl,
                "input_tokens": scale,
                "cache_creation_input_tokens": scale * 10,
                "cache_read_input_tokens": scale * 100,
                "output_tokens": scale * 1000,
            }))
            .unwrap()
        }

        async fn record_tool_turns(session: &SessionHandle) {
            session.record_message(assistant_with_every_block()).await;
            session
                .record_message(Message {
                    role: Role::User,
                    content: vec![ContentBlock::tool_result("t1", "Edited f.rs.", false)],
                })
                .await;
            session
                .record_tool_metadata_batch(vec![("t1".to_owned(), edit_metadata())])
                .await;
            session
                .record_message(Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::tool_use(
                            "t2",
                            "read",
                            serde_json::json!({ "file_path": "g.rs", "limit": 20 }),
                        ),
                        ContentBlock::tool_use(
                            "t3",
                            "glob",
                            serde_json::json!({ "pattern": "*.rs" }),
                        ),
                        ContentBlock::tool_use(
                            "t4",
                            "bash",
                            serde_json::json!({ "command": "cargo test" }),
                        ),
                        ContentBlock::tool_use(
                            "t5",
                            "tasks",
                            serde_json::json!({ "tasks": [
                                { "content": "Rename main", "status": "completed" },
                                { "content": "Fix the failing test", "status": "in_progress" },
                            ] }),
                        ),
                    ],
                })
                .await;
            session
                .record_message(Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::tool_result("t2", "fn main() {}", false),
                        ContentBlock::tool_result("t3", "g.rs", false),
                        ContentBlock::tool_result("t4", "test result: FAILED", true),
                        ContentBlock::tool_result(
                            "t5",
                            "Updated the task list: 1 of 2 completed.",
                            false,
                        ),
                    ],
                })
                .await;
            session
                .record_tool_metadata_batch(vec![
                    (
                        "t2".to_owned(),
                        ToolMetadata {
                            title: Some("Read g.rs".to_owned()),
                            ..ToolMetadata::default()
                        },
                    ),
                    (
                        "t3".to_owned(),
                        ToolMetadata {
                            title: Some("Found 100 files".to_owned()),
                            truncated_total: Some(250),
                            ..ToolMetadata::default()
                        },
                    ),
                ])
                .await;
        }

        fn edit_metadata() -> ToolMetadata {
            ToolMetadata {
                title: Some("Edited f.rs".to_owned()),
                diff: Some(EditDiff {
                    chunk: DiffChunk {
                        old: vec![NumberedLine {
                            number: 1,
                            text: "fn main() {}".to_owned(),
                        }],
                        new: vec![NumberedLine {
                            number: 1,
                            text: "fn run() {}".to_owned(),
                        }],
                    },
                    locations: vec![1],
                }),
                ..ToolMetadata::default()
            }
        }

        fn assistant_with_every_block() -> Message {
            let opus = |payload| Replay::with_model("claude-opus-5-5", payload);
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "Rename main.".to_owned(),
                        replay: opus("signature"),
                    },
                    ContentBlock::RedactedThinking {
                        replay: opus("redacted"),
                    },
                    ContentBlock::ServerToolUse {
                        id: "s1".to_owned(),
                        name: "advisor".to_owned(),
                        input: serde_json::json!({}),
                    },
                    ContentBlock::ServerToolResult {
                        tool_use_id: "s1".to_owned(),
                        kind: "advisor_tool_result".to_owned(),
                        fields: serde_json::Map::from_iter([(
                            "content".to_owned(),
                            serde_json::json!({
                                "type": "advisor_redacted_result",
                                "encrypted_content": "opaque",
                            }),
                        )]),
                    },
                    ContentBlock::Text {
                        text: "Renaming main.".to_owned(),
                    },
                    ContentBlock::tool_use(
                        "t1",
                        "edit",
                        serde_json::json!({ "file_path": "f.rs" }),
                    ),
                ],
            }
        }

        /// Replaces the per-run UUIDs, timestamps, and cwd with placeholders. UUIDs are numbered in
        /// order of appearance and timestamps in time order, so a fixture keeps which lines share a
        /// value and which came later.
        fn mask_volatile(content: &str) -> String {
            let uuid_re = regex::Regex::new(
                r#""[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}""#,
            )
            .unwrap();
            let ts_re =
                regex::Regex::new(r#""\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z""#).unwrap();
            let cwd_re = regex::Regex::new(r#""cwd":"[^"]*""#).unwrap();
            let root_re = regex::Regex::new(r#""project_root":"[^"]*""#).unwrap();
            let parse =
                |quoted: &str| OffsetDateTime::parse(quoted.trim_matches('"'), &Rfc3339).unwrap();

            let mut uuids: Vec<&str> = Vec::new();
            for found in uuid_re.find_iter(content) {
                if !uuids.contains(&found.as_str()) {
                    uuids.push(found.as_str());
                }
            }
            let mut timestamps: Vec<_> = ts_re
                .find_iter(content)
                .map(|found| parse(found.as_str()))
                .collect();
            timestamps.sort_unstable();
            timestamps.dedup();

            let masked = uuid_re.replace_all(content, |caps: &regex::Captures<'_>| {
                let n = uuids.iter().position(|uuid| *uuid == &caps[0]).unwrap() + 1;
                format!(r#""<UUID-{n}>""#)
            });
            let masked = ts_re.replace_all(&masked, |caps: &regex::Captures<'_>| {
                let n = timestamps.binary_search(&parse(&caps[0])).unwrap() + 1;
                format!(r#""<TS-{n}>""#)
            });
            let masked = cwd_re.replace_all(&masked, r#""cwd":"<CWD>""#);
            root_re
                .replace_all(&masked, r#""project_root":"<ROOT>""#)
                .into_owned()
        }

        /// Lists and resumes the unmasked `fixture`, checks that a new message continues the
        /// resumed chain, and renders what the listing and the resume recovered.
        async fn resume_and_continue(fixture: &str) -> String {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let path = test_project_dir(dir.path()).join("0-fixture.jsonl");
            std::fs::write(&path, unmask(fixture)).unwrap();

            let [listed]: [SessionInfo; 1] = store
                .list_matching("", ProjectScope::Current)
                .unwrap()
                .try_into()
                .unwrap();
            let resumed = resume_from_path(&store, &path).unwrap();
            let transcript = &resumed.transcript;
            let recovered = serde_json::json!({
                "listed": {
                    "title": listed.title.map(|title| title.title),
                    "git_branch": listed.git_branch,
                    "message_count": listed.exit.map(|exit| exit.message_count),
                    "archived": listed.archived,
                },
                "title": resumed.title,
                "unarchived": resumed.unarchived,
                "compact": transcript.compact.as_ref().map(|compact| serde_json::json!({
                    "summary": compact.summary,
                    "pre_message_count": compact.pre_message_count,
                    "instructions": compact.instructions,
                    "tasks": compact.tasks,
                })),
                "context": transcript.context,
                "messages": transcript.messages,
                "tool_metadata": transcript.tool_metadata,
                "usage": transcript.usage,
                "tasks": transcript.tasks,
                "file_snapshots": resumed.transcript.file_snapshots,
            });

            let before = load_session_data_from_path(&path).unwrap().messages;
            resumed
                .handle
                .record_message(Message::user("after resume"))
                .await;
            resumed.handle.shutdown().await;
            let after = load_session_data_from_path(&path).unwrap().messages;
            assert_eq!(
                after,
                [before, vec![Message::user("after resume")]].concat(),
                "the next message continues the resumed chain",
            );

            serde_json::to_string_pretty(&recovered).unwrap()
        }

        /// Fills each placeholder [`mask_volatile`] writes with one fixed value, giving a higher
        /// `<TS-n>` a later time.
        fn unmask(fixture: &str) -> String {
            let placeholder = regex::Regex::new(r"<(UUID|TS|CWD|ROOT)(?:-(\d+))?>").unwrap();
            placeholder
                .replace_all(fixture, |caps: &regex::Captures<'_>| {
                    let n: u64 = caps.get(2).map_or(0, |n| n.as_str().parse().unwrap());
                    match &caps[1] {
                        "UUID" => format!("00000000-0000-4000-8000-{n:012x}"),
                        "TS" => format!("2026-01-01T00:{:02}:{:02}Z", n / 60, n % 60),
                        "ROOT" => format!("/work/{TEST_PROJECT}"),
                        _ => "/repo".to_owned(),
                    }
                })
                .into_owned()
        }

        #[tokio::test]
        async fn writes_every_entry_type_as_byte_compatible_jsonl() {
            let dir = tempfile::tempdir().unwrap();
            insta::assert_snapshot!(write_every_entry(dir.path()).await);
        }

        #[test]
        fn resumes_from_every_release_fixture() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            // Fixtures start at v0.1.0, so none exist before that release.
            insta::with_settings!({ allow_empty_glob => true }, {
                insta::glob!("fixtures/*.jsonl", |fixture| {
                    let fixture = std::fs::read_to_string(fixture).unwrap();
                    let recovered = runtime.block_on(resume_and_continue(&fixture));
                    insta::assert_snapshot!(recovered);
                });
            });
        }

        #[tokio::test]
        async fn resumes_from_the_jsonl_this_build_writes() {
            let dir = tempfile::tempdir().unwrap();
            let fixture = write_every_entry(dir.path()).await;
            let recovered = resume_and_continue(&fixture).await;
            insta::assert_snapshot!(recovered);
        }
    }
}
