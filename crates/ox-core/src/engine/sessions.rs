//! The engine's session swaps, saved-session archive operations, `/rename`, and what the
//! engine shows as it starts on a session.

use std::path::PathBuf;

use anyhow::Result;

use super::transcript::LiveTranscript;
use super::usage::estimate_cost_usd;
use super::{Engine, turn};
use crate::protocol::notice::session_write_failed;
use crate::protocol::{Event, EventSink, ResumeTarget};
use crate::session::display::id_prefix;
use crate::session::entry::ArchiveState;
use crate::session::handle::{self, LoadedTranscript, RollIntoOutcome};
use crate::session::resolver::resolve_saved_prefix;
use crate::session::saved::{self, BARE_ARCHIVE_HINT, saved_error_message};
use crate::util::git::current_branch;
use crate::util::text::plural;

impl Engine {
    /// Finalizes the live session and continues in a fresh, empty one.
    pub(super) async fn clear(&mut self, sink: &dyn EventSink) {
        self.roll_session(sink, ArchiveState::Active).await;
    }

    /// Archives the live session, then rolls like [`Self::clear`].
    pub(super) async fn archive(&mut self, sink: &dyn EventSink) {
        if self.transcript.messages.is_empty() {
            sink.emit(
                Event::TurnFailed(
                    "Nothing to archive: this session has no messages yet".to_owned(),
                ),
                "archive-empty",
            );
            return;
        }
        if let Some(failure) = self.transcript.session.archive().await.failure {
            sink.emit(
                Event::TurnFailed(format!("Archive failed: {failure}")),
                "archive-failed",
            );
            return;
        }
        self.roll_session(sink, ArchiveState::Archived).await;
    }

    pub(super) async fn set_archive(
        &self,
        sink: &dyn EventSink,
        path: PathBuf,
        session_id: String,
        state: ArchiveState,
    ) {
        let id = session_id.clone();
        let live_id = self.transcript.session.session_id().to_owned();
        let result = tokio::task::spawn_blocking(move || {
            saved::set_archived(&path, &session_id, state, &live_id)
                .map_err(|error| {
                    saved_error_message(
                        error,
                        (state == ArchiveState::Archived).then_some(BARE_ARCHIVE_HINT),
                    )
                })
                .map(|()| state)
        })
        .await
        .unwrap_or_else(|error| Err(format!("Session archive task failed: {error}")));
        sink.emit(Event::ArchiveUpdated { id, result }, "archive-updated");
    }

    async fn roll_session(&mut self, sink: &dyn EventSink, old_session: ArchiveState) {
        self.transcript.file_tracker.drain_writes().await;
        let git_branch = current_branch(self.cwd()).await;
        let outcome = handle::roll(
            &mut self.transcript.session,
            &self.store,
            self.transcript.file_tracker.snapshot_all(),
            self.client.model().id(),
            git_branch,
        )
        .await;
        self.adopt_session(sink, LoadedTranscript::default()).await;
        sink.emit(
            Event::SessionRolled {
                id: outcome.new_id,
                old_session,
            },
            "session-rolled",
        );
        report_finalize_failure(sink, outcome.finalize_failure);
        self.emit_session_start(sink);
        sink.emit(Event::SessionReady, "session-ready");
    }

    /// Finalizes the live session and continues `target`. On failure the live session stays.
    pub(super) async fn resume(&mut self, sink: &dyn EventSink, target: ResumeTarget) {
        let outcome = match self.roll_into(target).await {
            Ok(outcome) => outcome,
            Err(e) => {
                sink.emit(
                    Event::TurnFailed(format!(
                        "Resume failed (still on session {}): {e:#}",
                        id_prefix(self.transcript.session.session_id()),
                    )),
                    "resume-failed",
                );
                return;
            }
        };
        self.adopt_session(sink, outcome.transcript).await;
        sink.emit(
            Event::SessionResumed {
                id: self.transcript.session.session_id().to_owned(),
                title: outcome.title,
                prior: outcome.prior,
                history: self.take_history(),
            },
            "session-resumed",
        );
        report_finalize_failure(sink, outcome.finalize_failure);
        self.emit_session_start(sink);
        sink.emit(Event::SessionReady, "session-ready");
    }

    /// Finalizes the live session and swaps to the one `target` names. A prefix must match one
    /// saved session other than the live one.
    async fn roll_into(&mut self, target: ResumeTarget) -> Result<RollIntoOutcome> {
        self.transcript.file_tracker.drain_writes().await;
        let path = match target {
            ResumeTarget::Path(path) => path,
            ResumeTarget::Prefix(prefix) => {
                resolve_saved_prefix(&self.store, &prefix, self.transcript.session.session_id())?
                    .path
            }
        };
        let snapshots = self.transcript.file_tracker.snapshot_all();
        handle::roll_into(&mut self.transcript.session, &self.store, snapshots, &path).await
    }

    async fn adopt_session(&mut self, sink: &dyn EventSink, transcript: LoadedTranscript) {
        self.shells.stop_all().await;
        let (transcript, usage, warnings) = LiveTranscript::load(
            self.transcript.session.clone(),
            transcript,
            self.client.model(),
            self.workspace.cwd(),
        )
        .await;
        self.client
            .set_session_id(transcript.session.session_id().to_owned());
        self.transcript = transcript;
        self.instruction_warnings = warnings;
        self.policy.clear_session_allow();
        self.reset_context_usage(sink);
        self.total_estimated_cost_usd = estimate_cost_usd(&usage);
        self.title_usage = None;
    }

    pub(super) async fn rename(&self, sink: &dyn EventSink, title: String) {
        let outcome = self
            .transcript
            .session
            .set_manual_title(title.clone())
            .await;
        if let Some(failure) = outcome.failure {
            sink.emit(
                Event::RenameFailed(session_write_failed(&failure)),
                "rename-failed",
            );
            return;
        }
        sink.emit(
            Event::SessionTitleUpdated {
                id: self.transcript.session.session_id().to_owned(),
                title,
            },
            "session-title-updated",
        );
    }

    /// Shows what a session carries as the engine starts on it: its usage and task list, the
    /// warnings its prompt raised, the tracked files that drifted, and a transcript that ends on an
    /// interrupted response. Follows the event that swaps the session, which clears the chat.
    pub(super) fn emit_session_start(&mut self, sink: &dyn EventSink) {
        self.emit_usage_update(sink);
        self.emit_restored_tasks(sink);
        for warning in &self.instruction_warnings {
            sink.emit(Event::Notice(warning.clone()), "instruction-warning");
        }
        report_drift(sink, &std::mem::take(&mut self.transcript.drifted));
        turn::notify_if_interrupted(&self.transcript.messages, sink);
    }

    /// Shows the task list a loaded session restored. An empty one needs no event, since the
    /// frontend empties its list when the session changes.
    fn emit_restored_tasks(&self, sink: &dyn EventSink) {
        if !self.transcript.tasks.is_empty() {
            sink.emit(
                Event::TasksUpdated(self.transcript.tasks.clone()),
                "tasks-restored",
            );
        }
    }
}

/// Reports that the session a roll or resume left behind failed to finalize.
fn report_finalize_failure(sink: &dyn EventSink, failure: Option<String>) {
    if let Some(failure) = failure {
        sink.emit(
            Event::Error(format!("Previous session write failed: {failure}")),
            "previous-session-finalize-failed",
        );
    }
}

/// Warns that the tracked files in `drifted` changed on disk since the resumed session read them.
fn report_drift(sink: &dyn EventSink, drifted: &[PathBuf]) {
    if drifted.is_empty() {
        return;
    }
    sink.emit(
        Event::Notice(format_drift_warning(drifted)),
        "resume-drift-warning",
    );
}

pub(super) fn format_drift_warning(drifted: &[PathBuf]) -> String {
    const PREVIEW_CAP: usize = 3;
    let count = drifted.len();
    let preview: Vec<String> = drifted
        .iter()
        .take(PREVIEW_CAP)
        .map(|p| p.display().to_string())
        .collect();
    let suffix = if count > preview.len() {
        format!(", and {} more", count - preview.len())
    } else {
        String::new()
    };
    format!(
        "{count} tracked {file} changed on disk since the resumed session read {it}, \
         so Edit and Write need a fresh Read first: {}{suffix}",
        preview.join(", "),
        file = plural(count, "file", "files"),
        it = plural(count, "it", "them"),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use super::*;
    use crate::client::anthropic::testing::{sse_body, sse_response};
    use crate::config::compaction::CompactionConfig;
    use crate::engine::compaction::{AutoCompactFailures, auto_compaction_due, request_tokens};
    use crate::engine::local_testing::{
        EchoTool, dead_test_session, finished_session, greeting_title, inert_ops, mount_title,
        next_matching, system_prompt, texts, tracked_paths, write_unreadable_instructions,
    };
    use crate::engine::testing::{
        MODEL, SESSION_CONTEXT, engine, mount_reply, mount_tool_call_once, request_bodies,
        with_pinned_home,
    };
    use crate::file_tracker::testing::{assert_writes_draining, blocked_write, seed_full_read};
    use crate::file_tracker::{FileTracker, GateError, GatePurpose};
    use crate::message::Message;
    use crate::model::TokenUsage;
    use crate::permission::rule::Rule;
    use crate::permission::{Mode, PermissionClass};
    use crate::protocol::testing::CapturingSink;
    use crate::protocol::{Interaction, Op};
    use crate::session::handle::testing::record_write_call;
    use crate::session::store::local_testing::test_session_file;
    use crate::session::store::testing::{test_project_dir, test_store};
    use crate::session::store::{SessionStore, delete_at};
    use crate::tool::ToolRegistry;
    use crate::tool::bash::background::ShellState;
    use crate::tool::bash::background::local_testing::start_shell;
    use crate::tool::bash::background::testing::shell;
    use crate::tool::tasks::TasksTool;
    use crate::tool::tasks::testing::plan;
    use crate::util::git::local_testing::init_fixture_repo;
    use crate::util::git::testing::with_isolated_git_env_async;

    /// The branch in the live session's header, after one recorded message writes the file.
    async fn header_branch(engine: &Engine, dir: &Path) -> Option<String> {
        engine
            .transcript
            .session
            .record_message(Message::user("first"))
            .await;
        let jsonl = std::fs::read_to_string(test_session_file(
            dir,
            engine.transcript.session.session_id(),
        ))
        .unwrap();
        let header: serde_json::Value =
            serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        header["git_branch"].as_str().map(ToOwned::to_owned)
    }

    /// Runs a turn whose echo call `id` spills its output, and returns the session it ran in.
    async fn spill_echo_output(server: &MockServer, engine: &mut Engine, id: &str) -> String {
        let input = serde_json::json!({ "text": "x".repeat(300 << 10) });
        server.reset().await;
        mount_tool_call_once(server, id, "echo", &input.to_string()).await;
        mount_reply(server, "done").await;
        engine
            .submit_prompt("echo it", None, &CapturingSink::default(), &mut inert_ops())
            .await;
        engine.session_id().to_owned()
    }

    /// The names of the spill files in `artifact_dir`, sorted.
    fn spill_names(artifact_dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(artifact_dir.join("tool-results"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn clear_and_resume_drain_old_writes_before_saving_and_adopting() {
        with_pinned_home(async {
            for (name, resume) in [("clear", false), ("resume", true)] {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                let old_id = engine.session_id().to_owned();
                let target_id = finished_session(&store).await;
                let target = store.find_session_path(&target_id).unwrap();
                let path = dir.path().join("notes.txt");
                record_write_call(
                    &engine.transcript.session,
                    &engine.transcript.file_tracker,
                    "toolu_write",
                    &path,
                    true,
                )
                .await;
                let tracker = Arc::clone(&engine.transcript.file_tracker);
                let previous = tracker.snapshot_all();
                let release = blocked_write(&tracker, &path, b"committed", "toolu_write").await;
                let sink = CapturingSink::default();

                {
                    let mut operation: std::pin::Pin<Box<dyn Future<Output = ()>>> = if resume {
                        Box::pin(engine.resume(&sink, ResumeTarget::Path(target)))
                    } else {
                        Box::pin(engine.clear(&sink))
                    };
                    assert!(futures::poll!(operation.as_mut()).is_pending(), "{name}");
                    assert_writes_draining(&tracker);
                    assert_eq!(std::fs::read(&path).unwrap(), b"alpha", "{name}");
                    assert_eq!(tracker.snapshot_all(), previous, "{name}");
                    assert_eq!(sink.events(), [], "{name}");
                    release.send(()).unwrap();
                    operation.await;
                }

                assert_eq!(std::fs::read(&path).unwrap(), b"committed", "{name}");
                assert_eq!(
                    store.load_session_data(&old_id).unwrap().file_snapshots,
                    tracker.snapshot_all(),
                    "{name}",
                );
                assert_eq!(
                    tracker.verify_current_content(&path, b"committed", GatePurpose::Write),
                    Ok(()),
                    "{name}",
                );
                assert_eq!(
                    engine.transcript.file_tracker.verify_current_content(
                        &path,
                        b"committed",
                        GatePurpose::Write
                    ),
                    Err(GateError::NeverRead {
                        path,
                        purpose: GatePurpose::Write
                    }),
                    "{name}",
                );
                assert!(
                    sink.events()
                        .iter()
                        .any(|event| matches!(event, Event::SessionReady)),
                    "{name}",
                );
                assert_eq!(engine.finalize().await, None, "{name}");
            }
        })
        .await;
    }

    #[tokio::test]
    async fn clear_and_resume_rebuild_session_prompt_and_drop_session_rules() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let sink = CapturingSink::default();
            let target_id = finished_session(&store).await;
            engine.transcript.messages = vec![Message::user("before clear")];
            let rule = Rule::parse(PermissionClass::Exec, "cargo test *").unwrap();
            engine.policy.allow_for_session(rule.clone());

            engine.clear(&sink).await;
            assert_eq!(engine.transcript.messages, []);
            assert_eq!(engine.policy.session_allow(), []);
            assert_ne!(engine.transcript.prompt.user_context, SESSION_CONTEXT);
            assert!(
                engine
                    .transcript
                    .prompt
                    .user_context
                    .contains("# Current date")
            );
            assert_eq!(
                engine.client.session_id(),
                engine.transcript.session.session_id()
            );

            SESSION_CONTEXT.clone_into(&mut engine.transcript.prompt.user_context);
            engine.total_estimated_cost_usd = 1.23;
            engine.policy.allow_for_session(rule);
            let target_path = store.find_session_path(&target_id).unwrap();
            engine.resume(&sink, ResumeTarget::Path(target_path)).await;
            assert_eq!(engine.policy.session_allow(), []);
            assert_eq!(engine.transcript.session.session_id(), target_id);
            assert_eq!(engine.client.session_id(), target_id);
            assert_eq!(engine.transcript.messages.len(), 2);
            assert_ne!(engine.transcript.prompt.user_context, SESSION_CONTEXT);
            assert!(
                engine.total_estimated_cost_usd.abs() < f64::EPSILON,
                "a session that recorded no usage resumes at zero"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn clear_and_archive_record_the_branch_on_the_new_session() {
        with_pinned_home(with_isolated_git_env_async(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            init_fixture_repo(dir.path());
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::default();

            engine.clear(&sink).await;
            let cleared = header_branch(&engine, dir.path()).await;
            engine.transcript.messages = vec![Message::user("archive me")];
            engine.archive(&sink).await;
            let archived = header_branch(&engine, dir.path()).await;

            assert_eq!(cleared.as_deref(), Some("fixture-branch"));
            assert_eq!(archived.as_deref(), Some("fixture-branch"));
        }))
        .await;
    }

    #[tokio::test]
    async fn clear_and_resume_end_a_paused_auto_compaction() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let target_id = finished_session(&store).await;
            let target_path = store.find_session_path(&target_id).unwrap();
            let clear_sink = CapturingSink::default();
            let resume_sink = CapturingSink::default();

            engine.auto_compaction_failures = AutoCompactFailures::new(3);
            engine.clear(&clear_sink).await;
            let after_clear = engine.auto_compaction_failures;
            engine.auto_compaction_failures = AutoCompactFailures::new(3);
            engine
                .resume(&resume_sink, ResumeTarget::Path(target_path))
                .await;

            assert_eq!(after_clear, AutoCompactFailures::default());
            assert_eq!(
                engine.auto_compaction_failures,
                AutoCompactFailures::default()
            );
            let cleared = clear_sink.events();
            assert!(
                matches!(
                    cleared.as_slice(),
                    [
                        Event::AutoCompactionResumed,
                        Event::SessionRolled {
                            old_session: ArchiveState::Active,
                            ..
                        },
                        Event::SessionReady,
                    ]
                ),
                "{cleared:?}",
            );
            let resumed = resume_sink.events();
            assert!(
                matches!(
                    resumed.as_slice(),
                    [
                        Event::AutoCompactionResumed,
                        Event::SessionResumed { id, .. },
                        Event::SessionReady,
                    ] if *id == target_id
                ),
                "{resumed:?}",
            );
        })
        .await;
    }

    #[tokio::test]
    async fn clear_and_resume_spill_tool_output_into_the_live_session() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path())
                .await
                .with_tools(ToolRegistry::new(vec![Box::new(EchoTool)]));
            engine.set_mode(&CapturingSink::default(), Mode::Yolo);

            let first = spill_echo_output(&server, &mut engine, "call-1").await;
            engine.clear(&CapturingSink::default()).await;
            let cleared = spill_echo_output(&server, &mut engine, "call-2").await;
            let first_path = store.find_session_path(&first).unwrap();
            engine
                .resume(&CapturingSink::default(), ResumeTarget::Path(first_path))
                .await;
            let resumed = spill_echo_output(&server, &mut engine, "call-3").await;

            assert_ne!(cleared, first);
            assert_eq!(resumed, first);
            let beside_file = |id: &str| store.find_session_path(id).unwrap().with_file_name(id);
            assert_eq!(
                spill_names(&beside_file(&first)),
                ["call-1.txt", "call-3.txt"]
            );
            assert_eq!(spill_names(&beside_file(&cleared)), ["call-2.txt"]);
        })
        .await;
    }

    #[tokio::test]
    async fn clear_and_resume_end_the_background_shells() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let target_id = finished_session(&store).await;
            let target_path = store.find_session_path(&target_id).unwrap();

            start_shell(&engine.shells, "sleep 30");
            engine.clear(&CapturingSink::default()).await;
            let after_clear = engine.shells.list();
            start_shell(&engine.shells, "sleep 30");
            engine
                .resume(&CapturingSink::default(), ResumeTarget::Path(target_path))
                .await;

            assert_eq!(after_clear, []);
            assert_eq!(engine.shells.list(), []);
        })
        .await;
    }

    mod clear {
        use super::*;

        #[tokio::test]
        async fn saves_the_reads_with_the_session_that_made_them() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                let old_id = engine.session_id().to_owned();
                let before = dir.path().join("before.txt");
                let after = dir.path().join("after.txt");
                let late = dir.path().join("late.txt");
                for path in [&before, &after, &late] {
                    std::fs::write(path, "contents").unwrap();
                }
                let sink = CapturingSink::default();
                let mut ops = inert_ops();
                engine
                    .submit_prompt("before clear", None, &sink, &mut ops)
                    .await;
                seed_full_read(&engine.transcript.file_tracker, &before);
                let old_tracker = Arc::clone(&engine.transcript.file_tracker);

                engine.clear(&sink).await;
                assert_eq!(tracked_paths(&engine), [] as [PathBuf; 0]);
                seed_full_read(&old_tracker, &late);
                let new_id = engine.session_id().to_owned();
                engine
                    .submit_prompt("after clear", None, &sink, &mut ops)
                    .await;
                seed_full_read(&engine.transcript.file_tracker, &after);
                assert_eq!(engine.finalize().await, None);

                let saved_paths = |id: &str| -> Vec<PathBuf> {
                    let snapshots = store.load_session_data(id).unwrap().file_snapshots;
                    snapshots
                        .into_iter()
                        .map(|snapshot| snapshot.path)
                        .collect()
                };
                assert_eq!(saved_paths(&old_id), [before]);
                assert_eq!(saved_paths(&new_id), [after]);
            })
            .await;
        }

        #[tokio::test]
        async fn keeps_a_pending_title_cost_out_of_the_new_session() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_title(&server, greeting_title()).await;
                let reply_without_usage = sse_body(&[
                    serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "model": MODEL}}),
                    serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "reply"}}),
                    serde_json::json!({"type": "content_block_stop", "index": 0}),
                    serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
                    serde_json::json!({"type": "message_stop"}),
                ]);
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(sse_response(reply_without_usage))
                    .mount(&server)
                    .await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.generates_titles = true;
                let (sink, mut events) = mpsc::unbounded_channel();
                engine.submit_prompt("hello", None, &sink, &mut inert_ops()).await;
                next_matching(&mut events, |event| {
                    matches!(event, Event::SessionTitleUpdated { .. }).then_some(())
                })
                .await;
                assert!(
                    engine.total_estimated_cost_usd.abs() < f64::EPSILON,
                    "a turn without usage emits nothing, so the title cost is still pending"
                );

                engine.clear(&sink).await;

                assert!(
                    engine.total_estimated_cost_usd.abs() < f64::EPSILON,
                    "the new session starts at zero: {}",
                    engine.total_estimated_cost_usd,
                );
            })
            .await;
        }

        #[tokio::test]
        async fn reports_the_finalize_failure_and_instruction_warnings_before_it_ends() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.transcript.session = dead_test_session();
                let warning = write_unreadable_instructions(dir.path());
                let sink = CapturingSink::default();

                engine.clear(&sink).await;

                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::SessionRolled {
                                old_session: ArchiveState::Active,
                                ..
                            },
                            Event::Error(failure),
                            Event::Notice(notice),
                            Event::SessionReady,
                        ] if failure == "Previous session write failed: \
                                         session writer task has stopped. \
                                         Conversation history may be incomplete."
                            && *notice == warning
                    ),
                    "{events:?}",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn drops_the_task_list_and_resume_restores_it_after_the_session() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let input = serde_json::json!({ "tasks": plan() }).to_string();
                mount_tool_call_once(&server, "call-1", "tasks", &input).await;
                mount_reply(&server, "done").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path())
                    .await
                    .with_tools(ToolRegistry::new(vec![Box::new(TasksTool)]));
                let planned_id = engine.session_id().to_owned();
                let mut ops = inert_ops();
                engine
                    .submit_prompt("plan it", None, &CapturingSink::default(), &mut ops)
                    .await;

                engine.clear(&CapturingSink::default()).await;
                assert_eq!(engine.transcript.tasks, []);

                let sink = CapturingSink::default();
                let planned_path = store.find_session_path(&planned_id).unwrap();
                engine.resume(&sink, ResumeTarget::Path(planned_path)).await;
                assert_eq!(engine.transcript.tasks, plan());
                let events = sink.events();
                let resumed = events
                    .iter()
                    .position(|event| matches!(event, Event::SessionResumed { .. }))
                    .expect("the resume succeeds");
                let restored = events
                    .iter()
                    .position(|event| *event == Event::TasksUpdated(plan()))
                    .expect("the resume shows the restored list");
                assert!(resumed < restored, "{events:?}");
            })
            .await;
        }
    }

    mod archive {
        use super::*;

        #[tokio::test]
        async fn keeps_the_session_without_messages_or_after_each_failed_write() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                let live_id = engine.transcript.session.session_id().to_owned();
                let sink = CapturingSink::default();

                engine.archive(&sink).await;
                assert_eq!(engine.transcript.session.session_id(), live_id);
                engine.transcript.messages = vec![Message::user("unsaved")];
                engine.transcript.session = dead_test_session();
                engine.archive(&sink).await;
                engine.archive(&sink).await;
                assert_eq!(engine.transcript.session.session_id(), "dead-test-session");

                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::TurnFailed(empty),
                            Event::TurnFailed(first),
                            Event::TurnFailed(again),
                        ] if empty == "Nothing to archive: this session has no messages yet"
                            && first == "Archive failed: session writer task has stopped. \
                                         Conversation history may be incomplete."
                            && again == "Archive failed: session writer task has stopped"
                    ),
                    "{events:?}",
                );
            })
            .await;
        }
    }

    mod set_archive {
        use super::*;

        #[tokio::test]
        async fn reports_a_failed_write_without_switching_the_live_session() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let live_id = engine.transcript.session.session_id().to_owned();
                let path = dir.path().join("missing.jsonl");
                let sink = CapturingSink::default();
                let expected = saved_error_message(
                    saved::set_archived(&path, "saved", ArchiveState::Archived, &live_id)
                        .unwrap_err(),
                    Some(BARE_ARCHIVE_HINT),
                );

                engine
                    .set_archive(&sink, path, "saved".to_owned(), ArchiveState::Archived)
                    .await;

                assert_eq!(
                    sink.events(),
                    [Event::ArchiveUpdated {
                        id: "saved".to_owned(),
                        result: Err(expected)
                    }]
                );
                assert_eq!(engine.transcript.session.session_id(), live_id);
            })
            .await;
        }
    }

    mod resume {
        use super::*;

        #[tokio::test]
        async fn estimates_the_resumed_context_so_its_first_prompt_can_auto_compact() {
            with_pinned_home(async {
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let saved = handle::start(&store, MODEL, None);
                let saved_id = saved.session_id().to_owned();
                saved
                    .record_message(Message::user("x".repeat(240_000)))
                    .await;
                saved.record_message(Message::assistant("done")).await;
                saved.finalize(Vec::new()).await;
                let mut engine = engine("http://127.0.0.1:9".to_owned(), &store, dir.path()).await;
                engine.compaction = CompactionConfig::with_threshold(50_000);
                let path = store.find_session_path(&saved_id).unwrap();

                engine
                    .resume(&CapturingSink::default(), ResumeTarget::Path(path))
                    .await;

                assert_eq!(
                    engine.context_estimate,
                    request_tokens(&engine.transcript.prompt, &[], &engine.transcript.messages)
                );
                assert!(auto_compaction_due(
                    engine.auto_compaction(),
                    engine.context_estimate,
                    &engine.transcript.messages
                ));
            })
            .await;
        }

        #[tokio::test]
        async fn swaps_in_the_file_state_that_still_matches_disk_and_reports_the_rest() {
            with_pinned_home(async {
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let saved = handle::start(&store, MODEL, None);
                let saved_id = saved.session_id().to_owned();
                let saved_tracker = FileTracker::default();
                let kept = dir.path().join("kept.txt");
                let drifted = dir.path().join("drifted.txt");
                saved.record_message(Message::user("write them")).await;
                record_write_call(&saved, &saved_tracker, "toolu_kept", &kept, true).await;
                record_write_call(&saved, &saved_tracker, "toolu_drifted", &drifted, true).await;
                std::fs::write(&drifted, "changed").unwrap();
                saved.finalize(saved_tracker.snapshot_all()).await;
                let mut engine = engine("http://127.0.0.1:9".to_owned(), &store, dir.path()).await;
                let live_id = engine.session_id().to_owned();
                engine
                    .transcript
                    .session
                    .record_message(Message::user("live"))
                    .await;
                let live = dir.path().join("live.txt");
                std::fs::write(&live, "live").unwrap();
                seed_full_read(&engine.transcript.file_tracker, &live);
                let sink = CapturingSink::default();
                let path = store.find_session_path(&saved_id).unwrap();

                engine.resume(&sink, ResumeTarget::Path(path)).await;

                assert_eq!(tracked_paths(&engine), [kept]);
                let notice = Event::Notice(format_drift_warning(&[drifted]));
                assert!(sink.events().contains(&notice), "{:?}", sink.events());
                let live_files = store.load_session_data(&live_id).unwrap().file_snapshots;
                let live_paths: Vec<_> = live_files.into_iter().map(|file| file.path).collect();
                assert_eq!(live_paths, [live]);
            })
            .await;
        }

        #[tokio::test]
        async fn replays_the_recorded_prompt() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                let recorded_id = engine.transcript.session.session_id().to_owned();
                let sink = CapturingSink::default();
                let mut ops = inert_ops();
                engine.submit_prompt("first", None, &sink, &mut ops).await;
                engine.clear(&sink).await;

                let recorded_path = store.find_session_path(&recorded_id).unwrap();
                engine
                    .resume(&sink, ResumeTarget::Path(recorded_path))
                    .await;
                assert_eq!(engine.transcript.prompt.user_context, SESSION_CONTEXT);
                assert!(!engine.transcript.prompt_unrecorded);
                engine.submit_prompt("second", None, &sink, &mut ops).await;

                let turns = request_bodies(&server).await;
                assert_eq!(system_prompt(&turns[1]), system_prompt(&turns[0]));
                assert_eq!(turns[1]["messages"][0], turns[0]["messages"][0]);
                assert_eq!(turns[1]["messages"][1]["content"][0]["text"], "first");
            })
            .await;
        }

        #[tokio::test]
        async fn shows_and_replays_an_interruption() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "42").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                let interrupted_id = engine.session_id().to_owned();
                let sink = CapturingSink::default();
                let (ops_tx, mut ops) = mpsc::channel(1);
                ops_tx.send(Op::Interrupt).await.unwrap();
                engine
                    .submit_prompt("write a story", None, &sink, &mut ops)
                    .await;
                engine.clear(&sink).await;
                let path = store.find_session_path(&interrupted_id).unwrap();

                engine.resume(&sink, ResumeTarget::Path(path)).await;
                engine
                    .submit_prompt("what is 7 times 6?", None, &sink, &mut ops)
                    .await;

                let history = sink.events().into_iter().find_map(|event| match event {
                    Event::SessionResumed { history, .. } => Some(history),
                    _ => None,
                });
                assert_eq!(
                    history,
                    Some(vec![
                        Interaction::UserText("write a story".to_owned()),
                        Interaction::Interrupted,
                    ])
                );
                let request = &request_bodies(&server).await[0];
                assert_eq!(request["messages"][1], serde_json::json!({
                    "role": "user", "content": [{"type": "text", "text": "write a story"}],
                }));
                assert_eq!(request["messages"][2], serde_json::json!({
                    "role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}],
                }));
                assert_eq!(texts(request)[3..], ["what is 7 times 6?"]);
            })
            .await;
        }

        #[tokio::test]
        async fn from_another_project_spills_beside_the_file_that_delete_removes() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let other =
                    SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
                let other_id = finished_session(&other).await;
                let other_path = store.find_session_path(&other_id).unwrap();
                let mut engine = engine(server.uri(), &store, dir.path())
                    .await
                    .with_tools(ToolRegistry::new(vec![Box::new(EchoTool)]));
                engine.set_mode(&CapturingSink::default(), Mode::Yolo);
                engine
                    .resume(
                        &CapturingSink::default(),
                        ResumeTarget::Path(other_path.clone()),
                    )
                    .await;

                spill_echo_output(&server, &mut engine, "call-1").await;
                let beside_file = other_path.with_file_name(&other_id);
                assert_eq!(spill_names(&beside_file), ["call-1.txt"]);
                engine.clear(&CapturingSink::default()).await;
                delete_at(&other_path).unwrap();

                assert!(!beside_file.exists(), "delete removes the spills");
                assert!(!test_project_dir(dir.path()).join(&other_id).exists());
            })
            .await;
        }

        #[tokio::test]
        async fn reports_the_old_session_finalize_failure_before_it_ends() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let target_id = finished_session(&store).await;
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                engine.transcript.session = dead_test_session();
                let sink = CapturingSink::default();

                let target_path = store.find_session_path(&target_id).unwrap();
                engine.resume(&sink, ResumeTarget::Path(target_path)).await;

                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::SessionResumed { id, .. },
                            Event::Error(msg),
                            Event::SessionReady,
                        ] if *id == target_id
                                && msg == "Previous session write failed: \
                                           session writer task has stopped. \
                                           Conversation history may be incomplete."
                    ),
                    "{events:?}"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn failure_preserves_current_session_state() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                let original_id = engine.transcript.session.session_id().to_owned();
                let context_estimate = 100_000;
                let displayed_usage = TokenUsage::new(90_000, 9);
                engine.transcript.messages = vec![Message::user("still here")];
                engine.auto_compaction_failures = AutoCompactFailures::new(2);
                engine.context_estimate = context_estimate;
                engine.displayed_usage = Some(displayed_usage);
                engine.total_estimated_cost_usd = 1.23;
                let rule = Rule::parse(PermissionClass::Exec, "cargo test *").unwrap();
                engine.policy.allow_for_session(rule.clone());
                start_shell(&engine.shells, "sleep 30");
                let read = dir.path().join("read.txt");
                std::fs::write(&read, "read").unwrap();
                seed_full_read(&engine.transcript.file_tracker, &read);
                let sink = CapturingSink::default();

                engine
                    .resume(&sink, ResumeTarget::Path("missing-target.jsonl".into()))
                    .await;

                assert_eq!(engine.transcript.session.session_id(), original_id);
                assert_eq!(engine.transcript.messages.len(), 1);
                assert_eq!(engine.auto_compaction_failures, AutoCompactFailures::new(2));
                assert_eq!(engine.context_estimate, context_estimate);
                assert_eq!(engine.displayed_usage, Some(displayed_usage));
                assert!((engine.total_estimated_cost_usd - 1.23).abs() < f64::EPSILON);
                assert_eq!(engine.policy.session_allow(), [rule]);
                assert_eq!(tracked_paths(&engine), [read]);
                assert_eq!(
                    engine.shells.list(),
                    [shell("shell-1", "sleep 30", ShellState::Running)]
                );
                assert_eq!(
                    sink.events(),
                    [Event::TurnFailed(format!(
                        "Resume failed (still on session {}): \
                         failed to open missing-target.jsonl: {}",
                        id_prefix(&original_id),
                        std::fs::metadata("missing-target.jsonl").unwrap_err(),
                    ))]
                );
                engine.shells.stop_all().await;
            })
            .await;
        }
    }

    mod rename {
        use super::*;

        #[tokio::test]
        async fn announces_only_a_persisted_title() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let live_id = engine.transcript.session.session_id().to_owned();
                let sink = CapturingSink::default();

                engine.rename(&sink, "Fix login".to_owned()).await;
                assert!(engine.transcript.session.manual_title_set());
                engine.transcript.session = dead_test_session();
                engine.rename(&sink, "Lost".to_owned()).await;

                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::SessionTitleUpdated { id, title },
                            Event::RenameFailed(write),
                        ] if *id == live_id && title == "Fix login"
                            && write == "Session write failed: session writer task has stopped. \
                                         Conversation history may be incomplete."
                    ),
                    "{events:?}",
                );
            })
            .await;
        }
    }

    mod format_drift_warning {
        use super::*;

        #[test]
        fn names_a_single_file() {
            let warning = format_drift_warning(&[PathBuf::from("a.rs")]);

            assert_eq!(
                warning,
                "1 tracked file changed on disk since the resumed session read it, \
                 so Edit and Write need a fresh Read first: a.rs",
            );
        }

        #[test]
        fn previews_three_paths_and_counts_the_rest() {
            let drifted: Vec<PathBuf> = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"]
                .into_iter()
                .map(PathBuf::from)
                .collect();

            let warning = format_drift_warning(&drifted);

            assert_eq!(
                warning,
                "5 tracked files changed on disk since the resumed session read them, \
                 so Edit and Write need a fresh Read first: a.rs, b.rs, c.rs, and 2 more",
            );
        }
    }
}
