//! Public session API. Every write method applies its change to the session state and commits it
//! under the state's lock on a blocking thread, so no lock is held across `await`.

#[cfg(test)]
pub(crate) mod testing;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context as _, Result, bail};
use tokio::task::JoinHandle;
use tracing::{error, warn};
use uuid::Uuid;

use super::resolver::resolve_target;
use super::sanitize::{retain_visible_file_snapshots, sanitize_resumed_messages};
use super::saved::SavedError;
use super::state::SessionState;
use super::store::listing::{ArchiveState, ProjectScope};
use super::store::load::load;
use super::store::{CompactInfo, CompactRecord, SessionStore, UsageRecord};
use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::prompt::SessionContext;
use crate::protocol::ResumeTarget;
use crate::tool::ToolMetadata;
use crate::tool::tasks::{self, Task};
use crate::util::sync::lock_or_recover;

// ── SessionHandle ──

/// Cheap to clone because clones share one session state. Every write method returns after the
/// commit that saves its change.
#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<Inner>,
}

/// What every handle clone shares.
struct Inner {
    /// Locked only on blocking threads, since a commit holds it while the store is busy.
    state: Mutex<SessionState>,
    /// The session the handle writes to, which a fork replaces. Readable without the state's lock.
    live: Mutex<Live>,
    /// Whether a write already reported a commit failure, which the next ones then omit.
    failure_surfaced: AtomicBool,
    title_task: Mutex<Option<JoinHandle<()>>>,
}

/// The live session's ID and artifact directory.
struct Live {
    id: Arc<str>,
    artifact_dir: Arc<Path>,
}

impl Live {
    fn of(state: &SessionState) -> Self {
        Self {
            id: Arc::from(state.session_id()),
            artifact_dir: state.artifact_dir().into(),
        }
    }
}

/// A point in the active branch that [`SessionHandle::rewind`] can move the tip back to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Checkpoint {
    /// The message the next one chains from, `None` before the first.
    pub(super) tip: Option<Uuid>,
    pub(super) message_count: u32,
    /// Whether a prior prompt or compaction root used the first-prompt title opportunity.
    pub(super) first_user_prompt_seen: bool,
}

/// A finished summary and what its compaction boundary records beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactBoundary {
    pub(crate) record: CompactRecord,
    /// Prompt state recorded beside the summary so resume restores the same request prefix.
    pub(crate) context: SessionContext,
}

/// What a write's commit reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteReport {
    /// A write failure, reported only the first time one occurs, except by
    /// [`SessionHandle::archive`], which reports every failure.
    pub failure: Option<String>,
    /// The fork the commit made.
    pub fork: Option<Fork>,
}

/// Another process wrote to session `from` since this one last saw it, so this conversation
/// continues as session `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fork {
    pub from: String,
    pub to: String,
}

/// What [`SessionHandle::record_message`] returns.
#[derive(Debug)]
pub(crate) struct RecordOutcome {
    /// The branch point the message chained from.
    pub(crate) checkpoint: Checkpoint,
    /// `Some` when this ordinary user message may seed the session's automatic title.
    pub(crate) ai_title_seed: Option<String>,
    pub(crate) report: WriteReport,
}

/// What [`SessionHandle::compact`] returns. `pre_count` is the message count the compaction
/// replaced.
#[derive(Debug)]
pub(crate) struct CompactOutcome {
    pub(crate) pre_count: u32,
    /// The synthetic root as a rewind point. The boundary takes effect in memory whether or not its
    /// commit lands.
    pub(crate) checkpoint: Checkpoint,
    pub(crate) report: WriteReport,
}

/// What [`SessionHandle::append_ai_title`] returns. `applied` is whether the title is the
/// session's once its write commits, `false` when a manual title in memory or in the store held.
#[derive(Debug)]
pub(super) struct AiTitleOutcome {
    pub(super) applied: bool,
    pub(super) report: WriteReport,
}

impl SessionHandle {
    fn new(state: SessionState) -> Self {
        Self {
            inner: Arc::new(Inner {
                live: Mutex::new(Live::of(&state)),
                state: Mutex::new(state),
                failure_surfaced: AtomicBool::new(false),
                title_task: Mutex::default(),
            }),
        }
    }

    /// The live session's ID, which a fork replaces.
    pub fn session_id(&self) -> String {
        lock_or_recover(&self.inner.live, "live").id.to_string()
    }

    /// The directory the live session's artifacts go to.
    pub(crate) fn artifact_dir(&self) -> Arc<Path> {
        Arc::clone(&lock_or_recover(&self.inner.live, "live").artifact_dir)
    }

    /// Ties the AI title request to this session, so a rename, a rewind to before the first
    /// prompt, or `finalize` aborts it.
    pub(super) fn set_title_task(&self, task: JoinHandle<()>) {
        *lock_or_recover(&self.inner.title_task, "title_task") = Some(task);
    }

    /// Records a conversation message.
    pub(crate) async fn record_message(&self, msg: Message) -> RecordOutcome {
        self.record_prompt(msg, None, None).await
    }

    /// Records the user prompt `msg`, with the prompt state it is the first to carry. A fresh
    /// session takes `title` as its first-prompt title in place of `msg`'s text.
    pub(crate) async fn record_prompt(
        &self,
        msg: Message,
        context: Option<SessionContext>,
        title: Option<String>,
    ) -> RecordOutcome {
        let ((checkpoint, ai_title_seed), report) = self
            .write(move |state| {
                let checkpoint = state.checkpoint();
                let seed = state.queue_message(msg, context, BTreeMap::new(), title.as_deref());
                (checkpoint, seed)
            })
            .await;
        RecordOutcome {
            checkpoint,
            ai_title_seed,
            report,
        }
    }

    /// Moves the session's head back to `checkpoint`, abandoning the messages recorded after it.
    /// `checkpoint` must come from the last compaction or a later record, since the chain resume
    /// reads ends at the compaction root. A rewind to before the first user prompt stops the title
    /// request that prompt started, so the next prompt titles the session.
    pub(crate) async fn rewind(&self, checkpoint: Checkpoint) -> WriteReport {
        if !checkpoint.first_user_prompt_seen {
            self.stop_title_task().await;
        }
        self.write(move |state| state.rewind(checkpoint)).await.1
    }

    /// Records one tool round in one commit: the assistant message and the tool results answering
    /// it, which carry their metadata. Default metadata is skipped.
    pub(crate) async fn record_round(
        &self,
        assistant: Message,
        tool_results: Message,
        metadata: Vec<(String, ToolMetadata)>,
    ) -> WriteReport {
        let metadata = metadata
            .into_iter()
            .filter(|(_, metadata)| *metadata != ToolMetadata::default())
            .collect();
        self.write(move |state| {
            _ = state.queue_message(assistant, None, BTreeMap::new(), None);
            _ = state.queue_message(tool_results, None, metadata, None);
        })
        .await
        .1
    }

    /// Records the token usage of billed requests, one row each.
    pub(crate) async fn record_usage(&self, records: Vec<UsageRecord>) -> WriteReport {
        self.write(move |state| state.queue_usage(records)).await.1
    }

    /// Records an AI-generated session title, which supersedes the first-prompt title but never a
    /// manual one, including a manual one the commit reads from the store.
    pub(super) async fn append_ai_title(&self, title: String) -> AiTitleOutcome {
        let (applied, committed) = self
            .commit(
                move |state| state.set_ai_title(title),
                |applied, state| applied && !state.title_manual(),
            )
            .await;
        AiTitleOutcome {
            applied,
            report: self.report(committed),
        }
    }

    /// Persists a user-supplied title, suppressing AI titles for the rest of the session.
    pub(crate) async fn set_manual_title(&self, title: String) -> WriteReport {
        self.stop_title_task().await;
        self.write(move |state| state.set_title(title, true))
            .await
            .1
    }

    /// Writes the synthetic post-compact root carrying the compaction boundary, which resets the
    /// chain the next message extends. The compaction takes effect even when its write fails.
    pub(crate) async fn compact(
        &self,
        boundary: CompactBoundary,
        synthetic_message: Message,
    ) -> CompactOutcome {
        let ((pre_count, checkpoint), report) = self
            .write(move |state| {
                let pre_count = state.compact(boundary, synthetic_message);
                (pre_count, state.checkpoint())
            })
            .await;
        CompactOutcome {
            pre_count,
            checkpoint,
            report,
        }
    }

    /// Marks the session archived, reporting every failure since the caller rolls away only once
    /// the archive is saved. Call it only after a message is recorded, since a session without one
    /// has no row to archive.
    pub(crate) async fn archive(&self) -> WriteReport {
        let ((), committed) = self.commit(SessionState::queue_archive, |(), _| ()).await;
        let failure = committed.err();
        if failure.is_some() {
            self.mark_failure_surfaced();
        }
        WriteReport {
            failure,
            fork: None,
        }
    }

    /// Saves `snapshots`, the file tracker state that closes the session, once.
    pub(crate) async fn finish(&self, snapshots: Vec<FileSnapshot>) -> WriteReport {
        self.write(move |state| state.finish(snapshots)).await.1
    }

    /// Stops the title request and saves the file snapshots, returning the last write's report.
    pub(crate) async fn finalize(self, snapshots: Vec<FileSnapshot>) -> WriteReport {
        self.stop_title_task().await;
        self.finish(snapshots).await
    }

    /// Aborts the title request and waits for it to end, so it never reports on a renamed or
    /// finalized session. A write the request already started still commits, possibly after the
    /// session ends.
    async fn stop_title_task(&self) {
        let task = lock_or_recover(&self.inner.title_task, "title_task").take();
        if let Some(task) = task {
            task.abort();
            if let Err(e) = task.await
                && e.is_panic()
            {
                error!(error = %e, "title generation panicked");
            }
        }
    }

    /// Applies `apply` to the state and commits what it queued, returning its value and the
    /// commit's report.
    async fn write<T: Send + 'static>(
        &self,
        apply: impl FnOnce(&mut SessionState) -> T + Send + 'static,
    ) -> (T, WriteReport) {
        let (value, committed) = self.commit(apply, |value, _| value).await;
        (value, self.report(committed))
    }

    /// Applies `apply`, commits, and settles under the state's lock on a blocking thread, so a
    /// busy store never stalls the runtime, then returns what `settle` makes of `apply`'s value
    /// and the committed state, with the commit's result. A fork points every clone at the new
    /// session. A panic while the lock is held stops persistence, which the next write that saves
    /// anything reports. A cancelled caller leaves the write to run to the end and log any failure.
    async fn commit<T: Send + 'static, U: Send + 'static>(
        &self,
        apply: impl FnOnce(&mut SessionState) -> T + Send + 'static,
        settle: impl FnOnce(T, &SessionState) -> U + Send + 'static,
    ) -> (U, Result<Option<Fork>, String>) {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut state = lock_state(&inner.state);
            let value = apply(&mut state);
            let committed = state.flush().map_err(|e| format!("{e:#}"));
            match &committed {
                Ok(Some(_)) => *lock_or_recover(&inner.live, "live") = Live::of(&state),
                Ok(None) => {}
                Err(failure) => warn!(error = %failure, "session write failed"),
            }
            (settle(value, &state), committed)
        })
        .await
        .expect("session write task panicked")
    }

    /// The report of a commit that returned `committed`, which carries a failure only the first
    /// time one occurs.
    fn report(&self, committed: Result<Option<Fork>, String>) -> WriteReport {
        match committed {
            Ok(fork) => WriteReport {
                failure: None,
                fork,
            },
            Err(failure) => WriteReport {
                failure: self.mark_failure_surfaced().then_some(failure),
                fork: None,
            },
        }
    }

    /// Marks a commit failure as reported, returning whether it is the first one.
    fn mark_failure_surfaced(&self) -> bool {
        !self.inner.failure_surfaced.swap(true, Ordering::AcqRel)
    }
}

const WRITE_PANICKED: &str = "a session write panicked";

/// Locks `state`. A write that panicked while holding the lock may have left it half applied, so
/// the lock recovers with persistence stopped.
fn lock_state(state: &Mutex<SessionState>) -> MutexGuard<'_, SessionState> {
    state.lock().unwrap_or_else(|poisoned| {
        error!("{WRITE_PANICKED}, so persistence stops");
        state.clear_poison();
        let mut guard = poisoned.into_inner();
        guard.stop(WRITE_PANICKED.to_owned());
        guard
    })
}

// ── Resumed Sessions ──

/// A session's transcript as loaded from the store, with the state saved beside its messages.
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

/// The live handle on a resumed session, with its transcript, title, and prior archive state.
pub struct ResumedSession {
    pub handle: SessionHandle,
    pub transcript: LoadedTranscript,
    pub title: Option<String>,
    /// The session's state before this resume, which unarchives an archived session.
    pub prior: ArchiveState,
}

// ── Constructors ──

/// Starts a fresh session on `git_branch`. The first record creates its row.
pub(crate) fn start(
    store: &SessionStore,
    model: &str,
    git_branch: Option<String>,
) -> SessionHandle {
    SessionHandle::new(SessionState::fresh(store.clone(), model, git_branch))
}

/// The new session ID and the old session's last write report.
#[derive(Debug)]
pub(crate) struct RollOutcome {
    pub(crate) new_id: String,
    pub(crate) finalize: WriteReport,
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
    let new_id = new_session.session_id();
    let old_session = std::mem::replace(session, new_session);
    let finalize = old_session.finalize(snapshots).await;
    RollOutcome { new_id, finalize }
}

/// The resumed transcript, title, and prior archive state, plus the old session's last report.
#[derive(Debug)]
pub(crate) struct RollIntoOutcome {
    pub(crate) transcript: LoadedTranscript,
    pub(crate) title: Option<String>,
    pub(crate) prior: ArchiveState,
    pub(crate) finalize: WriteReport,
}

/// Resolves `target`, loads and sanitizes the session it names, swaps it in, then finalizes the old
/// one with `snapshots`, its file tracker state. Fails without swapping when `target` is the live
/// session or cannot be loaded. A fork the resumed session makes records `model`.
pub(crate) async fn roll_into(
    session: &mut SessionHandle,
    store: &SessionStore,
    snapshots: Vec<FileSnapshot>,
    target: ResumeTarget,
    model: &str,
) -> Result<RollIntoOutcome> {
    let store = store.clone();
    let live_id = session.session_id();
    let model = model.to_owned();
    let ResumedSession {
        handle: target_handle,
        transcript,
        title,
        prior,
    } = tokio::task::spawn_blocking(move || {
        let session_id = resolve_target(&store, target, Some(&live_id), ProjectScope::Current)?;
        if session_id == live_id {
            return Err(SavedError::live(&live_id).into());
        }
        resume(&store, &session_id, &model)
    })
    .await
    .context("session resume task failed")??;

    let old_session = std::mem::replace(session, target_handle);
    let finalize = old_session.finalize(snapshots).await;
    Ok(RollIntoOutcome {
        transcript,
        title,
        prior,
        finalize,
    })
}

/// Resumes saved session `session_id`: takes its liveness lock, loads and sanitizes it, unarchives
/// it. A fork it makes records `model`.
pub(super) fn resume(
    store: &SessionStore,
    session_id: &str,
    model: &str,
) -> Result<ResumedSession> {
    let lock = store.hold(session_id)?;
    let mut data = load(&mut store.connect()?, session_id)?;
    let pre_sanitize_len = data.messages.len();
    sanitize_resumed_messages(&mut data.messages);
    // Bail only when sanitize wipes a non-empty list, since the next message would chain onto
    // messages the transcript no longer shows.
    if pre_sanitize_len > 0 && data.messages.is_empty() {
        bail!("session {session_id} has no messages left after repair");
    }
    retain_visible_file_snapshots(&mut data.file_snapshots, &data.messages);
    if data.archive_state == ArchiveState::Archived {
        store.set_archived(session_id, ArchiveState::Active)?;
    }

    let tasks = tasks::latest(&data.messages)
        .or_else(|| {
            data.compact
                .as_ref()
                .map(|compact| compact.record.tasks.clone())
        })
        .unwrap_or_default();
    let message_count = u32::try_from(data.messages.len()).unwrap_or(u32::MAX);
    let state = SessionState::resumed(store.clone(), &data, message_count, model, lock);
    Ok(ResumedSession {
        handle: SessionHandle::new(state),
        transcript: LoadedTranscript {
            messages: data.messages,
            context: data.context,
            compact: data.compact,
            tool_metadata: data.tool_metadata,
            usage: data.usage,
            tasks,
            file_snapshots: data.file_snapshots,
        },
        title: data.title,
        prior: data.archive_state,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::macros::datetime;
    use tokio::sync::oneshot;

    use super::*;
    use crate::file_tracker::testing::{record_tracked_file, seed_full_read};
    use crate::file_tracker::{
        FileSource, FileState, FileTracker, GateError, GatePurpose, LastView,
    };
    use crate::message::{ContentBlock, INTERRUPTION_MARKER, Replay, Role, TextOrigin};
    use crate::prompt::PromptParts;
    use crate::session::handle::testing::record_write_call;
    use crate::session::state::STOPPED;
    use crate::session::store::local_testing::{
        fail_writes, load_dump, mask_export, sql_dump, unmask,
    };
    use crate::session::store::testing::{
        WriterBarrier, archive_test_session, isolated_store, saved_message_count,
        seed_test_session, test_store,
    };
    use crate::tool::StoredView;
    use crate::tool::tasks::testing::plan;
    use crate::tool::view::{DiffChunk, EditDiff, NumberedLine, ToolResultView};

    fn resume_id(store: &SessionStore, session_id: &str) -> Result<ResumedSession> {
        resume(store, session_id, "m")
    }

    /// The title session `session_id` lists.
    fn listed_title(store: &SessionStore, session_id: &str) -> Option<String> {
        store.find(session_id).unwrap().unwrap().title
    }

    fn saved_snapshot_paths(store: &SessionStore, session_id: &str) -> Vec<PathBuf> {
        store
            .load_session_data(session_id)
            .unwrap()
            .file_snapshots
            .into_iter()
            .map(|snapshot| snapshot.path)
            .collect()
    }

    /// Spawns a title task that runs until something stops it, and returns the receiver that
    /// closes when it ends.
    async fn pending_title_task(handle: &SessionHandle) -> oneshot::Receiver<()> {
        let (started_tx, started) = oneshot::channel();
        let (alive, ended) = oneshot::channel::<()>();
        handle.set_title_task(tokio::spawn(async move {
            let _alive = alive;
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        started.await.unwrap();
        ended
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
        async fn resumed_session_does_not_seed_ai_title() {
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let session_id = original.session_id();
            original.record_message(Message::user("hello")).await;

            let resumed = resume_id(&store, &session_id).unwrap().handle;
            let outcome = resumed.record_message(Message::user("more text")).await;

            assert!(outcome.ai_title_seed.is_none());
        }
    }

    mod rewind {
        use super::*;

        #[tokio::test]
        async fn to_before_the_first_prompt_stops_the_title_request_it_started() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let refused = handle.record_message(Message::user("refused")).await;
            let mut ended = pending_title_task(&handle).await;

            let report = handle.rewind(refused.checkpoint).await;

            assert_eq!(report, WriteReport::default());
            assert_eq!(ended.try_recv(), Err(oneshot::error::TryRecvError::Closed));
        }

        #[tokio::test]
        async fn to_after_the_first_prompt_keeps_the_title_request() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            handle.record_message(Message::user("first")).await;
            let refused = handle.record_message(Message::user("refused")).await;
            let mut ended = pending_title_task(&handle).await;

            handle.rewind(refused.checkpoint).await;

            assert_eq!(ended.try_recv(), Err(oneshot::error::TryRecvError::Empty));
            handle.finalize(Vec::new()).await;
        }
    }

    mod record_round {
        use super::*;

        #[tokio::test]
        async fn writes_all_non_default_metadata() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let sid = handle.session_id();
            handle.record_message(Message::user("trigger")).await;
            let edited = ToolMetadata {
                title: Some("Edited a.rs".to_owned()),
                result_view: None,
            };
            let viewed = ToolMetadata {
                title: None,
                result_view: Some(StoredView::Text),
            };

            let report = handle
                .record_round(
                    Message::assistant("round"),
                    Message::user("result"),
                    vec![
                        ("t1".to_owned(), edited.clone()),
                        ("t2".to_owned(), ToolMetadata::default()),
                        ("t3".to_owned(), viewed.clone()),
                    ],
                )
                .await;

            assert_eq!(report, WriteReport::default());
            let data = store.load_session_data(&sid).unwrap();
            assert_eq!(
                data.messages,
                [
                    Message::user("trigger"),
                    Message::assistant("round"),
                    Message::user("result"),
                ],
                "the round lands in order",
            );
            assert_eq!(
                data.tool_metadata,
                HashMap::from([("t1".to_owned(), edited), ("t3".to_owned(), viewed)]),
                "default-only metadata is skipped",
            );
        }
    }

    mod append_ai_title {
        use super::*;

        #[tokio::test]
        async fn supersedes_the_first_prompt_title() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let sid = handle.session_id();
            handle.record_message(Message::user("Fix login bug")).await;

            handle
                .append_ai_title("Fix auth flow for mobile".to_owned())
                .await;

            assert_eq!(
                listed_title(&store, &sid).as_deref(),
                Some("Fix auth flow for mobile")
            );
        }

        #[tokio::test]
        async fn keeps_the_manual_title_a_fork_copied() {
            let (_dir, store) = isolated_store();
            let stale = start(&store, "m", None);
            let source = stale.session_id();
            stale.record_message(Message::user("first prompt")).await;
            let other = resume_id(&store, &source).unwrap().handle;
            other.record_message(Message::user("elsewhere")).await;
            other.set_manual_title("Renamed".to_owned()).await;
            let forked = stale.record_message(Message::assistant("reply")).await;
            let fork = forked.report.fork.expect("the stale handle forks").to;

            let ai = stale.append_ai_title("AI title".to_owned()).await;

            assert!(!ai.applied);
            let data = store.load_session_data(&fork).unwrap();
            assert_eq!(data.title.as_deref(), Some("Renamed"));
            assert!(data.title_manual);
        }

        #[tokio::test]
        async fn keeps_the_manual_title_a_later_fork_copies() {
            let (_dir, store) = isolated_store();
            let stale = start(&store, "m", None);
            let source = stale.session_id();
            stale.record_message(Message::user("first prompt")).await;
            let other = resume_id(&store, &source).unwrap().handle;
            other.record_message(Message::user("elsewhere")).await;
            other.set_manual_title("Renamed".to_owned()).await;

            let ai = stale.append_ai_title("AI title".to_owned()).await;
            let forked = stale.record_message(Message::assistant("reply")).await;

            assert!(!ai.applied);
            let fork = forked.report.fork.expect("the stale handle forks").to;
            let data = store.load_session_data(&fork).unwrap();
            assert_eq!(data.title.as_deref(), Some("Renamed"));
            assert!(data.title_manual);
        }

        #[tokio::test]
        async fn keeps_the_manual_title_another_process_set() {
            let (_dir, store) = isolated_store();
            let stale = start(&store, "m", None);
            let sid = stale.session_id();
            stale.record_message(Message::user("first prompt")).await;
            let other = resume_id(&store, &sid).unwrap().handle;
            other.set_manual_title("Renamed".to_owned()).await;

            let ai = stale.append_ai_title("AI title".to_owned()).await;

            assert!(!ai.applied);
            let data = store.load_session_data(&sid).unwrap();
            assert_eq!(data.title.as_deref(), Some("Renamed"));
            assert!(data.title_manual);
        }

        #[tokio::test]
        async fn keeps_the_manual_title_a_resume_loaded() {
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let sid = original.session_id();
            original.record_message(Message::user("seed")).await;
            original.set_manual_title("Manual".to_owned()).await;
            let resumed = resume_id(&store, &sid).unwrap().handle;

            let ai = resumed.append_ai_title("AI title".to_owned()).await;

            assert!(!ai.applied);
            assert_eq!(listed_title(&store, &sid).as_deref(), Some("Manual"));
        }
    }

    mod set_manual_title {
        use super::*;

        #[tokio::test]
        async fn writes_user_provided_title() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let sid = handle.session_id();
            handle.record_message(Message::user("seed")).await;

            let report = handle.set_manual_title("User wins".to_owned()).await;

            assert_eq!(report, WriteReport::default());
            assert_eq!(listed_title(&store, &sid).as_deref(), Some("User wins"));
        }

        #[tokio::test]
        async fn stops_the_title_request() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            handle.record_message(Message::user("seed")).await;
            let mut ended = pending_title_task(&handle).await;

            let report = handle.set_manual_title("User wins".to_owned()).await;

            assert_eq!(report, WriteReport::default());
            assert_eq!(ended.try_recv(), Err(oneshot::error::TryRecvError::Closed));
        }

        #[tokio::test]
        async fn titles_a_fresh_session_with_its_first_message() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let sid = handle.session_id();

            handle.set_manual_title("Named early".to_owned()).await;
            let first = handle.record_message(Message::user("first")).await;

            assert_eq!(first.ai_title_seed, None);
            assert_eq!(listed_title(&store, &sid).as_deref(), Some("Named early"));
        }
    }

    mod compact {
        use super::*;

        #[tokio::test]
        async fn saves_the_boundary_and_acks_with_pre_count() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let sid = handle.session_id();
            handle.record_message(Message::user("a")).await;
            handle.record_message(Message::assistant("b")).await;
            let mut boundary = testing::boundary("synth summary");
            boundary.record.instructions = Some("focus".to_owned());

            let outcome = handle.compact(boundary, Message::user("post")).await;

            assert_eq!(outcome.pre_count, 2, "pre_count snapshots before reset");
            assert_eq!(outcome.report, WriteReport::default());
            let data = store.load_session_data(&sid).unwrap();
            assert_eq!(data.messages, [Message::user("post")]);
            let compact = data.compact.expect("the committed boundary is saved");
            assert_eq!(compact.record.instructions.as_deref(), Some("focus"));
            assert_eq!(compact.pre_message_count, 2);
        }

        #[tokio::test]
        async fn takes_effect_in_memory_when_its_commit_fails() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            handle.record_message(Message::user("a")).await;
            handle.record_message(Message::assistant("b")).await;
            fail_writes(&store);

            let outcome = handle
                .compact(testing::boundary("s"), Message::user("synth"))
                .await;
            let next = handle.record_message(Message::user("c")).await;

            assert_eq!(outcome.pre_count, 2);
            assert_eq!(
                outcome.report.failure.as_deref(),
                Some("disk full: Error code 1811: constraint failed")
            );
            assert_eq!(
                next.checkpoint.message_count, 1,
                "the next message chains from the synthetic root"
            );
        }
    }

    mod archive {
        use super::*;

        #[tokio::test]
        async fn holds_until_the_next_resume_unarchives_the_session() {
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let session_id = original.session_id();
            original.record_message(Message::user("hello")).await;

            let report = original.archive().await;
            let archived = store.load_session_data(&session_id).unwrap().archive_state;
            let resumed = resume_id(&store, &session_id).unwrap();
            let unarchived = store.load_session_data(&session_id).unwrap().archive_state;
            let again = resume_id(&store, &session_id).unwrap();

            assert_eq!(report, WriteReport::default());
            assert_eq!(archived, ArchiveState::Archived);
            assert_eq!(resumed.prior, ArchiveState::Archived);
            assert_eq!(resumed.transcript.messages, [Message::user("hello")]);
            assert_eq!(unarchived, ArchiveState::Active);
            assert_eq!(again.prior, ArchiveState::Active);
        }
    }

    mod finish {
        use super::*;

        #[tokio::test]
        async fn lists_the_first_prompt_title_and_the_message_count() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let sid = handle.session_id();
            handle
                .record_message(Message::user("Fix the auth bug"))
                .await;

            handle.finish(Vec::new()).await;

            let info = store.find(&sid).unwrap().unwrap();
            assert_eq!(info.title.as_deref(), Some("Fix the auth bug"));
            assert_eq!(info.message_count, 1);
        }

        #[tokio::test]
        async fn leaves_no_session_without_messages() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);

            handle.finish(Vec::new()).await;

            assert_eq!(store.list().unwrap(), []);
        }

        #[tokio::test]
        async fn saves_one_snapshot_per_tracked_file() {
            let (_dir, store) = isolated_store();
            let files_dir = tempfile::tempdir().unwrap();
            let tracker = FileTracker::default();
            let handle = start(&store, "m", None);
            let sid = handle.session_id();
            handle.record_message(Message::user("trigger")).await;
            let mut expected: Vec<PathBuf> = ["a.rs", "b.rs", "c.rs"]
                .into_iter()
                .map(|name| {
                    let path = files_dir.path().join(name);
                    record_tracked_file(&tracker, &path, name.as_bytes(), "toolu_1");
                    path
                })
                .collect();

            handle.finish(tracker.snapshot_all()).await;

            let mut saved = saved_snapshot_paths(&store, &sid);
            saved.sort();
            expected.sort();
            assert_eq!(saved, expected);
        }
    }

    mod finalize {
        use super::*;

        #[tokio::test]
        async fn saves_the_snapshots_then_releases_the_session() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let sid = handle.session_id();
            handle.record_message(Message::user("hi")).await;

            let report = handle.finalize(vec![snapshot("/repo/f.rs")]).await;

            assert_eq!(report, WriteReport::default());
            assert_eq!(
                saved_snapshot_paths(&store, &sid),
                [PathBuf::from("/repo/f.rs")]
            );
            store
                .delete(&sid)
                .expect("the finalized session's liveness lock is released");
        }

        #[tokio::test]
        async fn ends_the_title_task_before_it_returns() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let mut ended = pending_title_task(&handle).await;

            let report = handle.finalize(Vec::new()).await;

            assert_eq!(report, WriteReport::default());
            assert_eq!(ended.try_recv(), Err(oneshot::error::TryRecvError::Closed));
        }

        #[tokio::test]
        async fn saves_the_snapshots_after_the_title_task_panics() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let sid = handle.session_id();
            handle.record_message(Message::user("hi")).await;
            let (started_tx, started) = oneshot::channel();
            handle.set_title_task(tokio::spawn(async move {
                started_tx.send(()).unwrap();
                panic!("deliberate title task panic for testing");
            }));
            started.await.unwrap();

            let report = handle.finalize(vec![snapshot("/repo/f.rs")]).await;

            assert_eq!(report, WriteReport::default());
            assert_eq!(
                saved_snapshot_paths(&store, &sid),
                [PathBuf::from("/repo/f.rs")]
            );
        }
    }

    mod write {
        use super::*;

        #[tokio::test]
        async fn surfaces_a_failed_commit_once_but_to_every_archive() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            fail_writes(&store);

            let first = handle.record_message(Message::user("a")).await;
            let second = handle.record_message(Message::user("b")).await;
            let archived = handle.archive().await;

            let disk_full = "disk full: Error code 1811: constraint failed";
            assert_eq!(first.report.failure.as_deref(), Some(disk_full));
            assert_eq!(second.report, WriteReport::default());
            assert_eq!(archived.failure, Some(format!("{STOPPED}: {disk_full}")));
        }

        #[tokio::test]
        async fn panicking_stops_persistence_for_every_clone() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let sid = handle.session_id();
            handle.record_message(Message::user("saved")).await;
            let clone = handle.clone();

            let panicked = tokio::spawn(async move {
                clone
                    .write(|_| -> u8 { panic!("deliberate write panic") })
                    .await
            })
            .await;
            let first = handle.record_message(Message::user("dropped")).await;
            let second = handle.record_message(Message::user("also dropped")).await;

            assert!(panicked.unwrap_err().is_panic());
            assert_eq!(
                first.report.failure,
                Some(format!("{STOPPED}: {WRITE_PANICKED}"))
            );
            assert_eq!(second.report, WriteReport::default());
            assert_eq!(saved_message_count(&store, &sid), 1);
        }

        #[tokio::test]
        async fn panicking_after_a_failed_commit_keeps_its_cause() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            fail_writes(&store);
            let failed = handle.record_message(Message::user("lost")).await;
            let clone = handle.clone();

            let panicked = tokio::spawn(async move {
                clone
                    .write(|_| -> u8 { panic!("deliberate write panic") })
                    .await
            })
            .await;
            let archived = handle.archive().await;

            let disk_full = "disk full: Error code 1811: constraint failed";
            assert_eq!(failed.report.failure.as_deref(), Some(disk_full));
            assert!(panicked.unwrap_err().is_panic());
            assert_eq!(archived.failure, Some(format!("{STOPPED}: {disk_full}")));
        }
    }

    mod start {
        use super::*;

        #[tokio::test]
        async fn creates_the_session_with_its_first_record() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "test-model", None);
            let sid = handle.session_id();
            let before = store.find(&sid).unwrap();

            handle.record_message(Message::user("first")).await;

            assert_eq!(
                before, None,
                "a fresh session has no row before its first record"
            );
            assert!(store.find(&sid).unwrap().is_some());
        }

        #[tokio::test]
        async fn keeps_artifacts_in_the_store_under_the_session_id() {
            let (_dir, store) = isolated_store();

            let handle = start(&store, "test-model", None);

            assert_eq!(
                handle.artifact_dir().to_path_buf(),
                store.artifact_dir(&handle.session_id())
            );
        }
    }

    mod roll {
        use super::*;

        #[tokio::test]
        async fn swaps_in_a_session_on_the_branch_and_finalizes_the_old_one() {
            let (_dir, store) = isolated_store();
            let tracker = FileTracker::default();
            let seed = store.artifact_dir("seed").with_file_name("seed.txt");
            std::fs::create_dir_all(seed.parent().unwrap()).unwrap();
            std::fs::write(&seed, b"hello").unwrap();
            seed_full_read(&tracker, &seed);
            let mut session = start(&store, "test-model", None);
            let original_id = session.session_id();
            session.record_message(Message::user("original")).await;

            let outcome = roll(
                &mut session,
                &store,
                tracker.snapshot_all(),
                "test-model",
                Some("feat/x".to_owned()),
            )
            .await;
            session.record_message(Message::user("rolled")).await;

            assert_ne!(outcome.new_id, original_id, "id must roll");
            assert_eq!(outcome.finalize, WriteReport::default());
            assert_eq!(saved_snapshot_paths(&store, &original_id), [seed]);
            let new = store.find(&outcome.new_id).unwrap().unwrap();
            assert_eq!(new.git_branch.as_deref(), Some("feat/x"));
            assert_eq!(
                store.load_session_data(&outcome.new_id).unwrap().messages,
                [Message::user("rolled")]
            );
        }
    }

    mod roll_into {
        use super::*;

        #[tokio::test]
        async fn serves_runtime_work_while_an_archived_resume_waits_for_the_writer() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                "target",
                Some("Saved"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            archive_test_session(&store, "target");
            let barrier = WriterBarrier::hold(&store);
            let mut session = start(&store, "m", None);

            let resume = tokio::spawn(async move {
                roll_into(
                    &mut session,
                    &store,
                    Vec::new(),
                    ResumeTarget::Id("target".to_owned()),
                    "m",
                )
                .await
                .unwrap()
            });
            let independent = tokio::spawn(async { "runtime event" });

            assert_eq!(independent.await.unwrap(), "runtime event");
            assert!(!resume.is_finished());
            barrier.release();
            assert_eq!(resume.await.unwrap().prior, ArchiveState::Archived);
        }

        #[tokio::test]
        async fn swaps_to_the_target_and_finalizes_the_old_session_with_its_snapshots() {
            let (dir, store) = isolated_store();
            let target = start(&store, "test-model", None);
            let target_id = target.session_id();
            target.record_message(Message::user("target prompt")).await;
            let target_seed = dir.path().join("target.txt");
            let target_tracker = FileTracker::default();
            record_write_call(&target, &target_tracker, "toolu_target", &target_seed, true).await;
            target.finalize(target_tracker.snapshot_all()).await;
            let tracker = FileTracker::default();
            let live_seed = dir.path().join("live.txt");
            std::fs::write(&live_seed, b"live body").unwrap();
            seed_full_read(&tracker, &live_seed);
            let mut session = start(&store, "test-model", None);
            let original_id = session.session_id();
            session.record_message(Message::user("live")).await;

            let outcome = roll_into(
                &mut session,
                &store,
                tracker.snapshot_all(),
                ResumeTarget::Id(target_id.clone()),
                "test-model",
            )
            .await
            .expect("roll_into must succeed for a healthy target");

            assert_eq!(
                session.session_id(),
                target_id,
                "handle now points at target"
            );
            assert_eq!(
                outcome.transcript.messages[0],
                Message::user("target prompt")
            );
            assert_eq!(outcome.finalize, WriteReport::default());
            assert_eq!(saved_snapshot_paths(&store, &original_id), [live_seed]);
            let loaded: Vec<_> = outcome
                .transcript
                .file_snapshots
                .into_iter()
                .map(|s| s.path)
                .collect();
            assert_eq!(loaded, [target_seed]);
        }

        #[tokio::test]
        async fn keeps_the_live_session_when_the_target_fails_or_is_live() {
            let (dir, store) = isolated_store();
            let mut session = start(&store, "test-model", None);
            let live_id = session.session_id();
            session.record_message(Message::user("live")).await;
            let missing = dir.path().join("missing-target.jsonl");

            for (target, expected) in [
                (
                    ResumeTarget::Path(missing.clone()),
                    format!("failed to open {}", missing.display()),
                ),
                (
                    ResumeTarget::Id(live_id.clone()),
                    format!("session {} is the live session", &live_id[..8]),
                ),
                (
                    ResumeTarget::Id("missing".to_owned()),
                    "session not found: missing".to_owned(),
                ),
            ] {
                let err = roll_into(&mut session, &store, Vec::new(), target, "test-model")
                    .await
                    .unwrap_err();

                assert_eq!(err.to_string(), expected);
                assert_eq!(session.session_id(), live_id, "{expected}");
            }
        }
    }

    mod resume {
        use super::*;

        #[tokio::test]
        async fn text_origins_survive_save_and_resume() {
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let session_id = original.session_id();
            let literal = "<system-reminder>Explain this tag.</system-reminder>";
            let messages = [
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::text(literal),
                        ContentBlock::text(INTERRUPTION_MARKER),
                        ContentBlock::generated_text(literal, TextOrigin::SystemReminder),
                    ],
                },
                Message::interruption(),
            ];
            for message in &messages {
                let outcome = original.record_message(message.clone()).await;
                assert_eq!(outcome.report, WriteReport::default());
            }
            assert_eq!(original.finalize(Vec::new()).await, WriteReport::default());

            let resumed = resume_id(&store, &session_id).unwrap();

            assert_eq!(resumed.transcript.messages, messages);
        }

        #[tokio::test]
        async fn restores_first_prompt_title_eligibility_across_rewind_and_resume() {
            #[derive(Clone, Copy, PartialEq, Eq)]
            enum RenameAt {
                BeforePrompt,
                BeforeRewind,
                AfterRewind,
            }

            for (name, rename_at) in [
                ("automatic title", None),
                ("manual before prompt", Some(RenameAt::BeforePrompt)),
                ("manual before rewind", Some(RenameAt::BeforeRewind)),
                ("manual after rewind", Some(RenameAt::AfterRewind)),
            ] {
                let (_dir, store) = isolated_store();
                let original = start(&store, "m", None);
                let session_id = original.session_id();
                let rename = async |at| {
                    if rename_at == Some(at) {
                        let renamed = original.set_manual_title("Manual title".to_owned()).await;
                        assert_eq!(renamed, WriteReport::default(), "{name}");
                    }
                };
                rename(RenameAt::BeforePrompt).await;
                let refused = original.record_message(Message::user("refused")).await;
                rename(RenameAt::BeforeRewind).await;
                let rewound = original.rewind(refused.checkpoint).await;
                assert_eq!(rewound, WriteReport::default(), "{name}");
                rename(RenameAt::AfterRewind).await;
                assert_eq!(
                    original.finalize(Vec::new()).await,
                    WriteReport::default(),
                    "{name}"
                );

                let resumed = resume_id(&store, &session_id).unwrap();
                let accepted = resumed
                    .handle
                    .record_message(Message::user("accepted"))
                    .await;
                assert_eq!(accepted.report, WriteReport::default(), "{name}");
                assert_eq!(
                    resumed.handle.finalize(Vec::new()).await,
                    WriteReport::default(),
                    "{name}"
                );

                let expected_seed = rename_at.is_none().then_some("accepted");
                assert_eq!(accepted.ai_title_seed.as_deref(), expected_seed, "{name}");
                assert_eq!(
                    listed_title(&store, &session_id).as_deref(),
                    Some(expected_seed.unwrap_or("Manual title")),
                    "{name}",
                );
            }
        }

        #[tokio::test]
        async fn loads_the_messages_with_or_without_a_finish_and_keeps_the_session_id() {
            for finished in [true, false] {
                let (_dir, store) = isolated_store();
                let original = start(&store, "m", None);
                let session_id = original.session_id();
                original.record_message(Message::user("hello")).await;
                original.record_message(Message::assistant("hi")).await;
                if finished {
                    original.finish(Vec::new()).await;
                }

                let resumed = resume_id(&store, &session_id).unwrap();

                assert_eq!(resumed.handle.session_id(), session_id, "{finished}");
                assert_eq!(
                    resumed.transcript.messages,
                    [Message::user("hello"), Message::assistant("hi")],
                    "{finished}"
                );
                assert_eq!(
                    resumed.handle.artifact_dir().to_path_buf(),
                    store.artifact_dir(&session_id),
                    "{finished}"
                );
            }
        }

        #[tokio::test]
        async fn names_a_session_that_was_never_saved() {
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let session_id = original.session_id();
            original.finalize(Vec::new()).await;

            let err = resume_id(&store, &session_id)
                .err()
                .expect("a session with no messages must not resume");

            assert_eq!(err.to_string(), format!("session not found: {session_id}"));
        }

        #[tokio::test]
        async fn repairs_unresolved_tool_calls_and_fails_when_nothing_is_left() {
            let unresolved = || Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "unresolved",
                    "bash",
                    serde_json::Value::Null,
                )],
            };
            for (name, messages, expected) in [
                (
                    "drops the call",
                    vec![Message::user("do X"), unresolved()],
                    Ok(vec![Message::user("do X")]),
                ),
                ("nothing left", vec![unresolved()], Err(())),
            ] {
                let (_dir, store) = isolated_store();
                let original = start(&store, "m", None);
                let session_id = original.session_id();
                for message in messages {
                    original.record_message(message).await;
                }

                let resumed = resume_id(&store, &session_id);

                match (resumed, expected) {
                    (Ok(resumed), Ok(expected)) => {
                        assert_eq!(resumed.transcript.messages, expected, "{name}");
                    }
                    (Err(err), Err(())) => assert_eq!(
                        format!("{err:#}"),
                        format!("session {session_id} has no messages left after repair"),
                        "{name}",
                    ),
                    (resumed, _) => panic!("{name}: {:?}", resumed.map(|r| r.transcript)),
                }
            }
        }

        #[tokio::test]
        async fn chains_the_next_record_to_the_resumed_head() {
            let (_dir, store) = isolated_store();
            let original = start(&store, "m", None);
            let session_id = original.session_id();
            original.record_message(Message::user("hello")).await;
            original.record_message(Message::assistant("hi")).await;
            let head = store.load_session_data(&session_id).unwrap().head;

            let resumed = resume_id(&store, &session_id).unwrap().handle;
            resumed.record_message(Message::user("follow up")).await;

            let parent: Option<String> = store
                .connect()
                .unwrap()
                .query_row(
                    "SELECT parent_id FROM messages WHERE session_id = ?1 ORDER BY seq DESC",
                    [&session_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(parent, head.map(|id| id.to_string()));
            assert_eq!(
                store.load_session_data(&session_id).unwrap().messages.len(),
                3
            );
        }

        #[tokio::test]
        async fn restores_only_the_snapshots_whose_result_survives_and_that_match_disk() {
            let (_dir, store) = isolated_store();
            let files_dir = tempfile::tempdir().unwrap();
            let tracker = FileTracker::default();
            let handle = start(&store, "m", None);
            let session_id = handle.session_id();
            handle.record_message(Message::user("hi")).await;
            let [kept, unanswered, drifted, missing] =
                ["kept.rs", "unanswered.rs", "drifted.rs", "missing.rs"]
                    .map(|name| files_dir.path().join(name));
            record_write_call(&handle, &tracker, "toolu_kept", &kept, true).await;
            record_write_call(&handle, &tracker, "toolu_unanswered", &unanswered, false).await;
            record_write_call(&handle, &tracker, "toolu_drifted", &drifted, true).await;
            record_write_call(&handle, &tracker, "toolu_missing", &missing, true).await;
            handle.finalize(tracker.snapshot_all()).await;
            std::fs::write(&drifted, b"externally edited bytes").unwrap();
            std::fs::remove_file(&missing).unwrap();

            let resumed = resume_id(&store, &session_id).unwrap();
            let (resumed_tracker, _) =
                FileTracker::restored(resumed.transcript.file_snapshots).await;

            assert_eq!(
                resumed_tracker.verify_current_content(&kept, b"alpha", GatePurpose::Edit),
                Ok(())
            );
            for path in [unanswered, drifted, missing] {
                assert_eq!(
                    resumed_tracker.verify_current_content(&path, b"alpha", GatePurpose::Edit),
                    Err(GateError::NeverRead {
                        path: path.clone(),
                        purpose: GatePurpose::Edit,
                    }),
                    "{}",
                    path.display(),
                );
            }
        }
    }

    mod session_format {
        use std::collections::BTreeMap;

        use super::*;

        /// Writes a session holding every row kind, content block, and saved text origin, with
        /// messages, contexts, and metadata after its compaction, since a resume drops those from
        /// before it. Returns the store and the session's ID. A release copies the store's dump and
        /// the session's export as its fixtures.
        async fn write_every_row_kind(dir: &Path) -> (SessionStore, String) {
            let store = test_store(dir);
            let session = start(&store, "m", Some("main".to_owned()));
            let mut prompt = PromptParts::default();
            "2026-01-01".clone_into(&mut prompt.seen.date);

            session
                .record_prompt(
                    Message::user("Edit f.rs"),
                    Some(prompt.session_context()),
                    None,
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
                        record: CompactRecord {
                            summary: "Asked to edit f.rs.".to_owned(),
                            instructions: Some("focus on f.rs".to_owned()),
                            tasks: plan(),
                        },
                        context: prompt.session_context(),
                    },
                    Message {
                        role: Role::User,
                        content: vec![
                            ContentBlock::generated_text(
                                "Asked to edit f.rs.",
                                TextOrigin::CompactionSummary,
                            ),
                            ContentBlock::generated_text(
                                "<system-reminder>Edit f.rs is in progress.</system-reminder>",
                                TextOrigin::SystemReminder,
                            ),
                        ],
                    },
                )
                .await;

            "2026-01-02".clone_into(&mut prompt.seen.date);
            session
                .record_prompt(
                    Message::user("Rename main in f.rs"),
                    Some(prompt.session_context()),
                    None,
                )
                .await;
            record_tool_turns(&session).await;
            session.record_message(Message::assistant("Done.")).await;
            session
                .record_prompt(Message::user("Explain f.rs"), None, None)
                .await;
            session.record_message(Message::interruption()).await;
            let refused = session.record_message(Message::user("Delete f.rs")).await;
            session.rewind(refused.checkpoint).await;
            session
                .record_usage(vec![usage("1h", 3), usage("1h", 4)])
                .await;
            session.archive().await;
            session
                .finish(vec![
                    snapshot("/repo/f.rs"),
                    FileSnapshot {
                        path: PathBuf::from("/repo/g.rs"),
                        state: FileState {
                            content_hash: 43,
                            last_view: LastView::Partial {
                                ranges: vec![(1, 20)],
                                total_lines: 40,
                            },
                            source: FileSource::Read,
                            recorded_at: datetime!(2026-01-01 00:00:02 UTC),
                            tool_use_id: "t2".to_owned(),
                        },
                    },
                ])
                .await;
            let session_id = session.session_id();

            (store, session_id)
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
            session
                .record_round(
                    assistant_with_every_block(),
                    Message {
                        role: Role::User,
                        content: vec![ContentBlock::tool_result("t1", "Edited f.rs.", false)],
                    },
                    vec![("t1".to_owned(), edit_metadata())],
                )
                .await;
            session
                .record_round(
                    Message {
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
                    },
                    Message {
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
                    },
                    vec![
                        (
                            "t2".to_owned(),
                            ToolMetadata {
                                title: Some("Read g.rs".to_owned()),
                                result_view: None,
                            },
                        ),
                        (
                            "t3".to_owned(),
                            ToolMetadata {
                                title: Some("Found 100 files".to_owned()),
                                result_view: Some(StoredView::View(ToolResultView::GlobFiles {
                                    pattern: "*.rs".to_owned(),
                                    files: vec!["g.rs".to_owned()],
                                    total: 250,
                                })),
                            },
                        ),
                    ],
                )
                .await;
        }

        fn edit_metadata() -> ToolMetadata {
            ToolMetadata {
                title: Some("Edited f.rs".to_owned()),
                result_view: Some(StoredView::View(ToolResultView::Diff(EditDiff {
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
                }))),
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
                    ContentBlock::text("Renaming main."),
                    ContentBlock::tool_use(
                        "t1",
                        "edit",
                        serde_json::json!({ "file_path": "f.rs" }),
                    ),
                ],
            }
        }

        /// Lists and resumes the only session in `store`, checks that a new message continues the
        /// resumed chain, and renders what the listing and the resume recovered.
        async fn resume_and_continue(store: &SessionStore) -> String {
            let scope = crate::session::store::listing::ListScope {
                projects: ProjectScope::Current,
                archive: ArchiveState::Archived,
            };
            let [listed] =
                <[_; 1]>::try_from(store.list_paged(None, scope).unwrap().into_sessions())
                    .expect("the store holds one session");
            let session_id = listed.session_id.clone();
            let resumed = resume_id(store, &session_id).unwrap();
            let transcript = &resumed.transcript;
            let recovered = serde_json::json!({
                "listed": {
                    "title": listed.title,
                    "git_branch": listed.git_branch,
                    "message_count": listed.message_count,
                    "archive_state": listed.archive_state,
                },
                "title": resumed.title,
                "prior": resumed.prior,
                "compact": transcript.compact.as_ref().map(|compact| serde_json::json!({
                    "summary": compact.record.summary,
                    "pre_message_count": compact.pre_message_count,
                    "instructions": compact.record.instructions,
                    "tasks": compact.record.tasks,
                })),
                "context": transcript.context,
                "messages": transcript.messages,
                "tool_metadata": transcript.tool_metadata.iter().collect::<BTreeMap<_, _>>(),
                "usage": transcript.usage,
                "tasks": transcript.tasks,
                "file_snapshots": transcript.file_snapshots,
                "title_manual": store.load_session_data(&session_id).unwrap().title_manual,
            });

            let before = store.load_session_data(&session_id).unwrap().messages;
            resumed
                .handle
                .record_message(Message::user("after resume"))
                .await;
            let after = store.load_session_data(&session_id).unwrap().messages;
            assert_eq!(
                after,
                [before, vec![Message::user("after resume")]].concat(),
                "the next message continues the resumed chain",
            );

            serde_json::to_string_pretty(&recovered).unwrap()
        }

        fn export(store: &SessionStore, session_id: &str) -> String {
            let mut out = Vec::new();
            store.export(session_id, &mut out).unwrap();
            String::from_utf8(out).unwrap()
        }

        #[tokio::test]
        async fn writes_every_row_kind() {
            let dir = tempfile::tempdir().unwrap();

            let (store, _) = write_every_row_kind(dir.path()).await;

            insta::assert_snapshot!(sql_dump(&store));
        }

        #[tokio::test]
        async fn exports_every_row_kind() {
            let dir = tempfile::tempdir().unwrap();
            let (store, session_id) = write_every_row_kind(dir.path()).await;

            let exported = export(&store, &session_id);

            insta::assert_snapshot!(mask_export(&exported));
        }

        #[tokio::test]
        async fn imports_the_export_into_the_rows_it_came_from() {
            let written = tempfile::tempdir().unwrap();
            let (store, session_id) = write_every_row_kind(written.path()).await;
            let (_dir, imported) = isolated_store();

            let target = imported
                .import(export(&store, &session_id).as_bytes())
                .unwrap();

            assert_eq!(target, session_id);
            assert_eq!(sql_dump(&imported), sql_dump(&store));
        }

        #[tokio::test]
        async fn resumes_from_the_dump_this_build_writes() {
            let written = tempfile::tempdir().unwrap();
            let (store, _) = write_every_row_kind(written.path()).await;
            let dir = tempfile::tempdir().unwrap();

            let recovered =
                resume_and_continue(&load_dump(dir.path(), &unmask(&sql_dump(&store)))).await;

            insta::assert_snapshot!(recovered);
        }

        #[test]
        fn resumes_from_every_release_fixture() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            // Fixtures start at v0.1.0, so none exist before that release.
            insta::with_settings!({ allow_empty_glob => true }, {
                insta::glob!("fixtures/*.{sql,jsonl}", |fixture| {
                    let content = unmask(&std::fs::read_to_string(fixture).unwrap());
                    let dir = tempfile::tempdir().unwrap();
                    let store = if fixture.extension().is_some_and(|ext| ext == "sql") {
                        load_dump(dir.path(), &content)
                    } else {
                        let store = test_store(dir.path());
                        store.import(content.as_bytes()).unwrap();
                        store
                    };
                    let recovered = runtime.block_on(resume_and_continue(&store));
                    insta::assert_snapshot!(recovered);
                });
            });
        }
    }

    fn snapshot(path: &str) -> FileSnapshot {
        FileSnapshot {
            path: PathBuf::from(path),
            state: FileState {
                content_hash: 42,
                last_view: LastView::Full,
                source: FileSource::Read,
                recorded_at: datetime!(2026-01-01 00:00:01 UTC),
                tool_use_id: "t1".to_owned(),
            },
        }
    }
}
