//! Engine events and background task results: chat and status updates, the turn lifecycle they
//! drive, session swaps, config changes, sign-in results, git probe updates, copy reports, and
//! background slash-command output.

use std::time::Instant;

use indoc::formatdoc;
use tokio::task::{JoinError, JoinSet};
use tracing::{debug, warn};

use ox_core::command;
use ox_core::config::Effort;
use ox_core::config::auth::AuthSource;
use ox_core::config::compaction::AutoCompactionConfig;
use ox_core::model::ModelSpec;
use ox_core::permission::{Basis, Mode, SessionGrant};
use ox_core::protocol::{CompactTrigger, Event, Interaction, Op, notice};
use ox_core::session::display::archive_notice;
use ox_core::session::entry::ArchiveState;
use ox_core::tool::view::ApprovalPreview;
use ox_core::util::text::plural;

use super::queue::FailedTurn;
use super::{App, CopyNotice};
use crate::clipboard::{CopyReport, Delivery};
use crate::components::status::Status;
use crate::git_probes::GitUpdate;
use crate::modal::approval::ApprovalModal;
use crate::slash::ChatOutput;
use crate::{NOTICE_DURATION, now};

impl App {
    pub(super) fn handle_engine_event(&mut self, event: Event) {
        match event {
            Event::StreamToken(token) => {
                self.chat.append_stream_token(&token);
                self.set_active_status(Status::Streaming);
            }
            Event::ThinkingToken(token) => {
                self.chat.append_thinking_token(&token);
                self.set_active_status(Status::Streaming);
            }
            Event::ProgressUpdate(text) => self.chat.push_progress_update(text),
            Event::Retrying(notice) => self.chat.push_notice(notice),
            Event::ToolCallStart(call) => {
                self.set_active_status(Status::ToolRunning {
                    name: call.name.clone(),
                });
                self.chat.push_tool_call(call);
            }
            Event::ToolCallEnd(result) => self.chat.push_tool_result(result),
            Event::PromptDrained(text) => self.apply_prompt_drained(text),
            Event::TurnComplete => self.finish_turn(),
            Event::TurnRolledBack => self.queue.mark_rolled_back(),
            Event::PromptUnsent => self.queue.mark_unsent(),
            Event::TurnFailed(msg) => self.apply_turn_failed(&msg),
            Event::UsageUpdated(usage) => self.status_bar.set_usage(Some(usage)),
            Event::RateLimitsUpdated(limits) => self.status_bar.set_rate_limits(Some(limits)),
            Event::Cancelled => {
                self.chat.push_interrupted_marker();
                self.enter_idle();
            }
            Event::AutoCompactionStarted => {
                self.queue.mark_compacting();
                self.set_active_status(Status::Compacting);
            }
            Event::AutoCompactionPaused { failures } => {
                self.session_info.auto_compaction_paused_after = Some(failures);
            }
            Event::AutoCompactionResumed => self.session_info.auto_compaction_paused_after = None,
            Event::SummaryToken(token) => self.chat.append_summary_token(&token),
            Event::SummaryDiscarded => self.chat.discard_summary(),
            Event::SessionTitleUpdated { id, title } => {
                if id == self.session_info.session_id {
                    self.status_bar.set_title(Some(title));
                }
            }
            Event::SessionRolled { id, archived } => self.apply_session_rolled(id, archived),
            Event::SessionResumed {
                id,
                title,
                unarchived,
                history,
            } => self.apply_session_resumed(id, title, unarchived, history),
            Event::SessionCompacted {
                summary,
                pre_count,
                instructions,
                trigger,
            } => {
                self.apply_session_compacted(&summary, pre_count, instructions.as_deref(), trigger);
            }
            Event::SessionReady => self.apply_session_ready(),
            Event::ConfigChanged {
                model,
                effort,
                requested,
                auto_compaction,
                notice,
            } => self.apply_config_changed(
                model,
                effort,
                requested.effort(),
                auto_compaction,
                notice,
            ),
            Event::ModeChanged(mode) => self.apply_mode_changed(mode),
            Event::LoginStarted { url } => self.open_sign_in_url(&url),
            Event::SignedIn { auth } => self.apply_signed_in(auth),
            Event::LoginRestored { auth } => self.apply_login_restored(auth),
            Event::SignedOut {
                removed,
                auth,
                fallback_error,
            } => self.apply_signed_out(removed, auth, fallback_error),
            Event::Notice(msg) => self.chat.push_system_message(msg),
            Event::Error(msg) | Event::RenameFailed(msg) => self.chat.push_error(&msg),
            Event::ApprovalRequested {
                id,
                preview,
                basis,
                grant,
            } => self.push_approval_modal(id, preview, &basis, grant),
            Event::SessionRulesUpdated(rules) => self.session_info.session_allow = rules,
            Event::TasksUpdated(tasks) => self.tasks.set(tasks),
            Event::BackgroundShellsUpdated(shells) => self.session_info.background_shells = shells,
        }
        self.dirty = true;
    }

    fn apply_prompt_drained(&mut self, text: String) {
        self.queue.confirm_drained(&text);
        self.chat.push_user_message(text);
    }

    /// Opens the approve-or-deny overlay for a gated tool call. Pushing onto the stack lets it nest
    /// over any open picker, and the modal's cancel hook resolves the blocked turn on dismissal.
    pub(super) fn push_approval_modal(
        &mut self,
        id: String,
        preview: ApprovalPreview,
        basis: &Basis,
        grant: Option<Box<SessionGrant>>,
    ) {
        self.modals.push(Box::new(ApprovalModal::new(
            id,
            preview,
            basis.reason(),
            grant,
        )));
    }

    fn finish_turn(&mut self) {
        self.chat.commit_streaming();
        // A cancel that lost the race to the turn's end, or a failed resume or archive, still
        // leaves the next step to the user.
        if matches!(
            &self.status,
            Status::Cancelling | Status::Resuming | Status::Archiving
        ) {
            self.enter_idle();
        } else {
            self.finalize_idle();
        }
    }

    /// Ends a failed operation.
    /// A turn whose automatic compaction failed goes idle with the queue held.
    /// A turn that never reached the model, or that the engine rolled back, does the same
    /// and hands its prompts back, one into the empty input for another try.
    fn apply_turn_failed(&mut self, msg: &str) {
        self.chat.push_error(msg);
        match self.queue.fail_turn(self.input.is_empty()) {
            FailedTurn::Ran => self.finish_turn(),
            FailedTurn::Held => {
                self.chat.commit_streaming();
                self.enter_idle();
            }
            FailedTurn::HandedBack(prompt) => {
                if let Some(prompt) = prompt {
                    self.input.set_text(&prompt);
                }
                self.enter_idle();
            }
        }
    }

    /// `/clear` or `/archive`: rebinds the session and empties chat for the fresh one.
    fn apply_session_rolled(&mut self, id: String, archived: bool) {
        let old_id = self.rebind_session(id, None);
        if archived {
            self.chat
                .push_system_message(archive_notice(&old_id, ArchiveState::Archived));
        }
        self.discard_queue();
    }

    /// Mid-session resume: rebinds the session and rebuilds chat from the target transcript.
    fn apply_session_resumed(
        &mut self,
        id: String,
        title: Option<String>,
        unarchived: bool,
        history: Vec<Interaction>,
    ) {
        self.rebind_session(id, title);
        self.chat.load_history(history);
        if unarchived {
            self.chat.push_system_message(archive_notice(
                &self.session_info.session_id,
                ArchiveState::Active,
            ));
        }
        if let Some(notice) = self.session_info.config.model.unsupported_notice() {
            self.chat.push_system_message(notice);
        }
        self.discard_queue();
        self.clear_modals();
    }

    /// Drops the prompts queued for the previous session, saying how many.
    fn discard_queue(&mut self) {
        let dropped = self.queue.discard_left_behind();
        if dropped > 0 {
            self.chat.push_system_message(format!(
                "Discarded {dropped} queued {} from the previous session",
                plural(dropped, "prompt", "prompts"),
            ));
        }
    }

    /// Rebinds to session `id` titled `title`, clearing the previous session's chat, rules,
    /// background shells, tasks, slash-command tasks, usage, and in-flight prompt. Returns the
    /// previous session's ID.
    fn rebind_session(&mut self, id: String, title: Option<String>) -> String {
        let old_id = std::mem::replace(&mut self.session_info.session_id, id);
        self.session_info.session_allow.clear();
        self.session_info.background_shells.clear();
        self.tasks.clear();
        self.slash_tasks = JoinSet::new();
        self.status_bar.set_title(title);
        self.status_bar.set_usage(None);
        self.chat.clear_history();
        self.queue.clear_active();
        old_id
    }

    /// Repaints chat with one boundary block after a manual or automatic compaction.
    /// Queued prompts survive because the session identity stays the same.
    /// A mid-turn compaction also leaves open modals alone, since the user may have opened one at
    /// any point of a long turn.
    fn apply_session_compacted(
        &mut self,
        summary: &str,
        pre_count: u32,
        instructions: Option<&str>,
        trigger: CompactTrigger,
    ) {
        self.chat.clear_history();
        self.status_bar.set_usage(None);
        self.chat
            .push_compacted_block(pre_count, instructions, summary.to_owned());
        self.queue.mark_compacted();
        match trigger {
            CompactTrigger::Manual => self.clear_modals(),
            CompactTrigger::Auto => {
                if let Some(prompt) = self.queue.active() {
                    self.chat
                        .push_user_message(command::unescape_literal(prompt).to_owned());
                }
                self.clear_modals();
            }
            CompactTrigger::MidTurn => self.queue.mark_summarized(),
        }
    }

    /// Ends a `/compact`, `/clear`, `/resume`, or `/archive` once the new transcript has reported
    /// its state. A cancel that lost the race to the swap leaves the queue to the user.
    fn apply_session_ready(&mut self) {
        match &self.status {
            Status::Compacting | Status::Clearing | Status::Resuming | Status::Archiving => {
                self.finalize_idle();
            }
            Status::Cancelling => self.enter_idle(),
            _ => {}
        }
    }

    fn apply_config_changed(
        &mut self,
        model: ModelSpec,
        effort: Option<Effort>,
        requested_effort: Option<Effort>,
        auto_compaction: AutoCompactionConfig,
        notice: String,
    ) {
        if model.id() != self.session_info.config.model.id() {
            self.status_bar
                .set_model(model.short_display_name().to_owned());
        }
        self.status_bar.set_effort(effort);
        self.session_info
            .config
            .apply_swap(model, effort, requested_effort, auto_compaction);
        self.chat.push_system_message(notice);
    }

    fn apply_mode_changed(&mut self, mode: Mode) {
        self.session_info.config.apply_mode(mode);
        self.chat.push_system_message(notice::mode_changed(mode));
    }

    /// Opens `url` in the browser and prints it in case the browser does not open. The sign-in
    /// returns to a loopback listener, so only a browser on this machine can complete it.
    fn open_sign_in_url(&mut self, url: &str) {
        let message = match (self.open_browser)(url) {
            Ok(()) => formatdoc! {"
                Opened the browser to sign in. If it did not open, visit this URL in a browser on this machine:
                {url}"
            },
            Err(e) => {
                debug!(error = %e, "failed to open the browser");
                formatdoc! {"
                    Could not open the browser. Sign in at this URL in a browser on this machine:
                    {url}"
                }
            }
        };
        self.chat.push_system_message(message);
    }

    fn apply_signed_in(&mut self, auth: AuthSource) {
        self.session_info.config.auth = Some(auth);
        self.status_bar.set_rate_limits(None);
        let message = match auth {
            AuthSource::OxLogin | AuthSource::ClaudeCodeLogin => {
                format!("Signed in. Requests now use {auth}.")
            }
            AuthSource::ApiKeyFile => format!(
                "Signed in. {auth} takes precedence, so requests keep using it. \
                 Remove `client.api_key` and restart ox to use the new login."
            ),
            AuthSource::AuthTokenEnv | AuthSource::ApiKeyEnv | AuthSource::OAuthTokenEnv => {
                format!(
                    "Signed in. {auth} takes precedence, so requests keep using it. \
                 Unset it and restart ox to use the new login."
                )
            }
        };
        self.chat.push_system_message(message);
        self.finish_turn();
    }

    fn apply_login_restored(&mut self, auth: AuthSource) {
        self.session_info.config.auth = Some(auth);
        self.status_bar.set_rate_limits(None);
        self.chat
            .push_system_message(format!("Signed in. Requests now use {auth}."));
    }

    fn apply_signed_out(
        &mut self,
        removed: bool,
        auth: Option<AuthSource>,
        fallback_error: Option<String>,
    ) {
        self.session_info.config.auth = auth;
        self.status_bar.set_rate_limits(None);
        let outcome = if removed {
            "Signed out of oxide-code."
        } else {
            "No oxide-code login is stored."
        };
        let active = match (auth, fallback_error) {
            (Some(source), _) => format!("Requests use {source}."),
            (None, Some(e)) => format!("No credential is active: {e}"),
            (None, None) => "No credential is active. Run `/login` to sign in.".to_owned(),
        };
        self.chat.push_system_message(format!("{outcome} {active}"));
        self.finish_turn();
    }

    /// Goes idle, then submits the next queued prompt.
    fn finalize_idle(&mut self) {
        self.enter_idle();
        self.drain_pending_prompt();
    }

    /// Resets to idle, clears orphan calls, and re-enables input. Queued prompts stay queued, and
    /// the ended turn no longer holds any of them.
    fn enter_idle(&mut self) {
        self.queue.end_turn();
        self.chat.clear_pending_calls();
        self.set_status(Status::Idle);
    }

    /// Starts a turn with the oldest queued prompt.
    pub(super) fn drain_pending_prompt(&mut self) {
        if let Some(prompt) = self.queue.start_next() {
            self.chat.push_user_message(prompt.clone());
            self.start_turn(Op::Prompt(prompt));
        }
    }

    /// Sets busy status unless a cancel is pending. Leaving `Compacting` releases the prompts held
    /// during an automatic compaction.
    fn set_active_status(&mut self, status: Status) {
        match &self.status {
            Status::Cancelling => {}
            Status::Compacting => {
                self.set_status(status);
                self.forward_queued_prompts();
            }
            _ => self.set_status(status),
        }
    }

    pub(super) fn apply_git_update(&mut self, update: GitUpdate) {
        let changed = match update {
            GitUpdate::Branch(branch) => self.status_bar.set_git_branch(branch),
            GitUpdate::PullRequest(pull_request) => self.status_bar.set_pull_request(pull_request),
        };
        self.dirty |= changed;
    }

    /// Notes how a copy went, queueing its OSC 52 write for the next frame.
    pub(super) fn apply_copy_report(&mut self, report: CopyReport) {
        let (text, failed) = report.notice();
        if let Ok(Delivery::Osc52 { sequence, .. }) = report.outcome {
            self.pending_clipboard = Some(sequence);
        }
        self.copy_notice = Some(CopyNotice {
            text,
            failed,
            until: now() + NOTICE_DURATION,
        });
        self.dirty = true;
    }

    /// Clears the copy notice once it has been shown long enough. Returns whether it cleared.
    pub(super) fn expire_copy_notice(&mut self, now: Instant) -> bool {
        self.copy_notice
            .take_if(|notice| now >= notice.until)
            .is_some()
    }

    /// Prints what a background slash command resolved to. A panicked task only logs, since the
    /// panic hook already reports the panic.
    pub(super) fn apply_slash_task(
        &mut self,
        finished: Result<Result<ChatOutput, String>, JoinError>,
    ) {
        match finished {
            Ok(Ok(ChatOutput::SystemMessage(text))) => self.chat.push_system_message(text),
            Ok(Ok(ChatOutput::GitDiff(diff))) => self.chat.push_git_diff(diff),
            Ok(Err(msg)) => self.chat.push_error(&msg),
            Err(e) => warn!("background slash command failed: {e}"),
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use indoc::indoc;
    use tokio::sync::mpsc::error::TryRecvError;

    use ox_core::config::compaction::testing::WINDOW_1M_THRESHOLD;
    use ox_core::config::provenance::Source;
    use ox_core::model::ResolvedModelId;
    use ox_core::permission::PermissionClass;
    use ox_core::permission::rule::Rule;
    use ox_core::permission::testing::exec_grant;
    use ox_core::protocol::testing::text_result;
    use ox_core::protocol::{ConfigPick, ResumeTarget, ToolCall, ToolResult, UsageSnapshot};
    use ox_core::tool::bash::background::ShellState;
    use ox_core::tool::bash::background::testing::{shell, shells_in_each_state};
    use ox_core::tool::tasks::TaskStatus;
    use ox_core::tool::tasks::testing::{plan, task};
    use ox_core::tool::view::{ApprovalBody, ToolResultView};
    use ox_core::util::git::PullRequest;

    use super::*;
    use crate::action::Action;
    use crate::app::testing::{five_hour_limit, rendered_text, test_app};
    use crate::clipboard::CopyError;
    use crate::glyphs::INTERRUPTED_MARKER;
    use crate::modal::ModalAction;
    use crate::modal::testing::ScriptedModal;
    use crate::slash::WorkingTreeDiff;
    use crate::theme::builtin;

    fn bash_call_start(id: &str, command: &str) -> Event {
        Event::ToolCallStart(ToolCall {
            id: id.to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": command}),
            icon: "$".to_owned(),
            label: command.to_owned(),
        })
    }

    fn model_pick(model: &ModelSpec, effort: Option<Effort>) -> ConfigPick {
        ConfigPick::Model {
            id: ResolvedModelId::new(model.id()),
            effort,
        }
    }

    mod handle_engine_event {
        use super::*;

        fn text_call_end(id: &str, content: &str, is_error: bool, title: Option<&str>) -> Event {
            Event::ToolCallEnd(text_result(id, content, is_error, title))
        }

        fn usage_snapshot() -> UsageSnapshot {
            UsageSnapshot {
                context_tokens: Some(124_000),
                context_window: Some(1_000_000),
                estimated_cost_usd: Some(0.4321),
            }
        }

        #[test]
        fn stream_token_switches_to_streaming() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(Event::StreamToken("partial".to_owned()));
            assert_eq!(app.status, Status::Streaming);
        }

        #[test]
        fn thinking_token_routes_to_chat_and_marks_streaming() {
            // Thinking tokens land in the chat view as their own block and must flip the bar to
            // Streaming, so the user sees the engine working before any visible text arrives.
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(Event::ThinkingToken("planning...".to_owned()));
            assert_eq!(app.status, Status::Streaming);
        }

        #[test]
        fn progress_update_renders_note_in_chat() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(Event::ProgressUpdate("Running the suite.".to_owned()));
            assert!(rendered_text(&mut app, 80, 24).contains("Running the suite."));
        }

        #[tokio::test]
        async fn retrying_notice_and_error_events_keep_the_turn_running() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");
            app.dispatch_action(Action::Submit("queued".to_owned()));
            rx.try_recv().expect("queued submit forwarded");

            app.handle_engine_event(Event::Retrying(
                "HTTP 529 overloaded_error. Retrying in 1 s (retry 1 of 5).".to_owned(),
            ));
            assert_eq!(
                app.chat.last_error_text(),
                None,
                "a retry is not an error block"
            );
            let notice = "Skipped instruction file /repo/CLAUDE.md: Permission denied";
            app.handle_engine_event(Event::Notice(notice.to_owned()));
            assert_eq!(app.chat.last_system_text(), Some(notice));
            app.handle_engine_event(Event::Error("Session write failed: disk full".to_owned()));

            assert_eq!(
                app.chat.last_error_text(),
                Some("Session write failed: disk full")
            );
            assert_eq!(app.status, Status::Streaming);
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "nothing resubmits mid-turn"
            );
            assert_eq!(*app.queue.pending(), ["queued"],);
        }

        #[test]
        fn tool_call_start_switches_to_tool_running() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(bash_call_start("t1", "ls"));
            assert_eq!(
                app.status,
                Status::ToolRunning {
                    name: "bash".to_owned()
                },
            );
            assert_eq!(
                app.chat.entry_count(),
                1,
                "tool call renders one chat entry",
            );
            assert!(rendered_text(&mut app, 60, 8).contains("$ ls"));
        }

        #[test]
        fn tool_call_end_renders_the_view_under_the_tool_title() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(bash_call_start("t1", "ls"));
            let before = app.chat.entry_count();
            app.handle_engine_event(Event::ToolCallEnd(ToolResult {
                id: "t1".to_owned(),
                content: "raw output".to_owned(),
                is_error: false,
                title: Some("Listed files".to_owned()),
                view: ToolResultView::Text {
                    content: "viewed output".to_owned(),
                },
            }));
            assert_eq!(app.chat.entry_count(), before + 1);
            let text = rendered_text(&mut app, 60, 8);
            assert!(text.contains("Listed files"), "{text}");
            assert!(text.contains("viewed output"), "{text}");
            assert!(!text.contains("raw output"), "{text}");
        }

        #[test]
        fn tool_call_end_without_title_falls_back_to_call_label() {
            // A tool that fails before `.with_title(...)`, as on a timeout or invalid input, sets
            // no result header. Its result still renders so the error body shows, headed by the
            // call label stashed at `ToolCallStart`.
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(bash_call_start("t1", "distinctive_label_xyz"));
            let before = app.chat.entry_count();
            app.handle_engine_event(text_call_end(
                "t1",
                "spawn failed: permission denied",
                true,
                None,
            ));
            assert_eq!(
                app.chat.entry_count(),
                before + 1,
                "result must render even when the tool did not set a title",
            );
            // The result header must be the stashed call label (the bash command). It appears twice
            // in the rendered view, once on the tool call line and once in the result header, which
            // shows that both rows carry the same label.
            let text = rendered_text(&mut app, 60, 8);
            let occurrences = text.matches("distinctive_label_xyz").count();
            assert_eq!(
                occurrences, 2,
                "expected call label on both the call and result rows, got {occurrences}:\n{text}",
            );
            assert!(
                !text.contains("(result)"),
                "generic fallback must not leak through when the pending call label is known, \
                 got:\n{text}",
            );
        }

        #[test]
        fn tool_call_end_without_start_uses_generic_fallback_header() {
            // An End without a matching Start (an engine bug or a dropped event) must still render
            // so the user sees the output. No pending entry → header falls back to `(result)`.
            let (mut app, _rx, _event_tx) = test_app(None);
            let before = app.chat.entry_count();
            app.handle_engine_event(text_call_end("orphan", "stray output", false, None));
            assert_eq!(app.chat.entry_count(), before + 1);
            // Viewport must fit header + body or auto-scroll hides the header.
            let text = rendered_text(&mut app, 60, 8);
            assert!(
                text.contains("(result)"),
                "orphan End with no pending call should use the generic fallback, got:\n{text}",
            );
        }

        #[tokio::test]
        async fn prompt_drained_pops_queue_head_and_pushes_user_message() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");
            app.dispatch_action(Action::Submit("queued-a".to_owned()));
            rx.try_recv().expect("queued-a submit forwarded");
            app.dispatch_action(Action::Submit("queued-b".to_owned()));
            rx.try_recv().expect("queued-b submit forwarded");

            let chat_before = app.chat.entry_count();
            app.handle_engine_event(Event::PromptDrained("queued-a".to_owned()));

            assert_eq!(*app.queue.pending(), ["queued-b"],);
            assert_eq!(
                app.chat.entry_count(),
                chat_before + 1,
                "drained prompt must push exactly one new user-message block",
            );
        }

        #[test]
        fn prompt_drained_outside_the_forwarded_queue_keeps_the_queue_and_shows_the_text() {
            // The engine only drains prompts it was handed, but a desync must still show the text.
            let (mut app, _rx, _event_tx) = test_app(None);
            app.queue.push("held".to_owned());
            let chat_before = app.chat.entry_count();

            app.handle_engine_event(Event::PromptDrained("held".to_owned()));

            assert_eq!(*app.queue.pending(), ["held"],);
            assert_eq!(app.chat.entry_count(), chat_before + 1);
        }

        #[tokio::test]
        async fn prompt_held_during_auto_compaction_forwards_ahead_of_later_submits() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("first".to_owned()));
            rx.try_recv().expect("first submit forwarded");
            app.handle_engine_event(Event::AutoCompactionStarted);
            app.dispatch_action(Action::Submit("held".to_owned()));
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "held while compacting"
            );

            app.handle_engine_event(Event::StreamToken("x".to_owned()));
            app.dispatch_action(Action::Submit("later".to_owned()));
            app.handle_engine_event(Event::PromptDrained("held".to_owned()));

            let forwarded: Vec<Op> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
            assert_eq!(
                forwarded,
                [Op::Queue("held".to_owned()), Op::Queue("later".to_owned())],
            );
            assert_eq!(*app.queue.pending(), ["later"],);
        }

        #[test]
        fn turn_complete_becomes_idle() {
            let (mut app, _rx, _event_tx) = test_app(None);
            // Drive into streaming first so TurnComplete has state to tear down.
            app.dispatch_action(Action::Submit("hi".to_owned()));
            assert_eq!(app.status, Status::Streaming);

            app.handle_engine_event(Event::TurnComplete);
            assert_eq!(app.status, Status::Idle);
        }

        #[tokio::test]
        async fn turn_complete_starts_a_turn_with_the_queue_head_and_hands_it_the_rest() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");
            app.queue.push("a".to_owned());
            app.queue.push("b".to_owned());

            app.handle_engine_event(Event::TurnComplete);

            let forwarded: Vec<Op> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
            assert_eq!(
                forwarded,
                [Op::Prompt("a".to_owned()), Op::Queue("b".to_owned())],
            );
            assert_eq!(app.status, Status::Streaming);
            assert_eq!(*app.queue.pending(), ["b"],);
        }

        #[tokio::test]
        async fn turn_complete_after_a_cancel_request_leaves_the_queue_to_the_user() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");
            app.dispatch_action(Action::Submit("queued".to_owned()));
            rx.try_recv().expect("queued submit forwarded");
            app.handle_esc();
            assert_eq!(rx.try_recv().ok(), Some(Op::Interrupt));

            app.handle_engine_event(Event::TurnComplete);

            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "nothing resubmits after a cancel"
            );
            assert_eq!(app.status, Status::Idle);
            assert_eq!(*app.queue.pending(), ["queued"],);
        }

        #[tokio::test]
        async fn turn_rolled_back_hands_the_refused_prompt_and_the_drained_ones_back() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("//refused".to_owned()));
            rx.try_recv().expect("prompt forwarded");
            app.dispatch_action(Action::Submit("drained".to_owned()));
            rx.try_recv().expect("queued prompt forwarded");
            app.handle_engine_event(Event::PromptDrained("drained".to_owned()));
            app.dispatch_action(Action::Submit("queued".to_owned()));
            rx.try_recv().expect("queued prompt forwarded");

            app.handle_engine_event(Event::TurnRolledBack);
            app.handle_engine_event(Event::TurnFailed(
                "The model declined this request.".to_owned(),
            ));

            assert_eq!(app.status, Status::Idle);
            assert_eq!(app.input.lines(), ["//refused".to_owned()]);
            assert_eq!(*app.queue.pending(), ["drained", "queued"],);
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "nothing reaches the engine until the user sends"
            );
        }

        #[tokio::test]
        async fn prompt_unsent_returns_to_the_input() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("//etc/hosts".to_owned()));
            rx.try_recv().expect("prompt forwarded");
            app.handle_engine_event(Event::AutoCompactionStarted);

            app.handle_engine_event(Event::PromptUnsent);
            app.handle_engine_event(Event::TurnFailed(
                "Auto-compaction failed: summarizer down. The prompt was not sent.".to_owned(),
            ));

            assert_eq!(
                app.chat.last_error_text(),
                Some("Auto-compaction failed: summarizer down. The prompt was not sent.")
            );
            assert_eq!(app.status, Status::Idle);
            assert_eq!(
                app.input.lines(),
                ["//etc/hosts".to_owned()],
                "Enter sends the prompt again as typed",
            );
            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "nothing reaches the engine until the user sends"
            );
        }

        #[tokio::test]
        async fn prompt_unsent_returns_to_the_held_queue_head() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("prompt forwarded");
            app.handle_engine_event(Event::AutoCompactionStarted);
            app.dispatch_action(Action::Submit("queued".to_owned()));

            app.handle_engine_event(Event::PromptUnsent);
            app.handle_engine_event(Event::TurnFailed(
                "Auto-compaction failed: summarizer down. The prompt was not sent.".to_owned(),
            ));

            assert_eq!(app.status, Status::Idle);
            assert!(app.input.is_empty());
            assert_eq!(
                *app.queue.pending(),
                ["active", "queued"],
                "the unsent prompt still runs first",
            );
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "nothing retries compaction until the user sends again"
            );
        }

        #[tokio::test]
        async fn prompt_unsent_holds_the_queue_after_an_untyped_prompt() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_op(Op::Prompt(
                "Create AGENTS.md for this repository.".to_owned(),
            ));
            rx.try_recv().expect("prompt forwarded");
            app.handle_engine_event(Event::AutoCompactionStarted);
            app.queue.push("queued".to_owned());

            app.handle_engine_event(Event::PromptUnsent);
            app.handle_engine_event(Event::TurnFailed(
                "Auto-compaction failed: summarizer down. The prompt was not sent.".to_owned(),
            ));

            assert!(app.input.is_empty(), "a command's prompt was never typed");
            assert_eq!(*app.queue.pending(), ["queued"],);
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[test]
        fn turn_failed_shows_the_error_and_finishes_the_turn() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("boom".to_owned()));
            app.handle_engine_event(Event::TurnFailed("API blew up".to_owned()));

            assert_eq!(app.chat.last_error_text(), Some("API blew up"));
            assert_eq!(app.status, Status::Idle);
        }

        #[tokio::test]
        async fn turn_failed_ends_an_archive_and_holds_the_prompts_queued_during_it() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_op(Op::Archive);
            assert_eq!(rx.recv().await, Some(Op::Archive));
            app.dispatch_action(Action::Submit("typed meanwhile".to_owned()));
            let during = rx.try_recv();

            app.handle_engine_event(Event::TurnFailed("Archive failed: disk full".to_owned()));

            assert_eq!(during, Err(TryRecvError::Empty));
            assert_eq!(
                app.chat.last_error_text(),
                Some("Archive failed: disk full")
            );
            assert_eq!(app.status, Status::Idle);
            assert_eq!(*app.queue.pending(), ["typed meanwhile"]);
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        }

        #[tokio::test]
        async fn turn_failed_shows_the_error_and_starts_the_queue_head_once() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");
            app.queue.push("first".to_owned());
            app.queue.push("second".to_owned());
            let chat_before = app.chat.entry_count();

            app.handle_engine_event(Event::TurnFailed("API failed".to_owned()));

            assert_eq!(
                app.chat.entry_count(),
                chat_before + 2,
                "error block, then the next prompt"
            );
            let forwarded: Vec<Op> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
            assert_eq!(
                forwarded,
                [
                    Op::Prompt("first".to_owned()),
                    Op::Queue("second".to_owned()),
                ],
            );
            assert_eq!(*app.queue.pending(), ["second"],);
        }

        #[tokio::test]
        async fn turn_failed_after_auto_compaction_succeeded_starts_the_queue_head() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("prompt forwarded");
            app.handle_engine_event(Event::AutoCompactionStarted);
            app.handle_engine_event(Event::SessionCompacted {
                summary: "auto summary".to_owned(),
                pre_count: 4,
                instructions: None,
                trigger: CompactTrigger::Auto,
            });
            app.queue.push("queued".to_owned());

            app.handle_engine_event(Event::TurnFailed("API failed".to_owned()));

            assert_eq!(rx.try_recv().unwrap(), Op::Prompt("queued".to_owned()));
            assert!(app.input.is_empty(), "the sent prompt is not handed back");
        }

        #[tokio::test]
        async fn turn_failed_during_a_mid_turn_compaction_holds_the_queue() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("prompt forwarded");
            app.handle_engine_event(Event::StreamToken("Reading.".to_owned()));
            app.handle_engine_event(Event::AutoCompactionStarted);
            app.dispatch_action(Action::Submit("queued".to_owned()));

            app.handle_engine_event(Event::TurnFailed(
                "Auto-compaction failed: summarizer down. \
                 The turn stopped before its next request."
                    .to_owned(),
            ));

            assert_eq!(app.status, Status::Idle);
            assert!(app.input.is_empty(), "the sent prompt is not handed back");
            assert_eq!(*app.queue.pending(), ["queued"]);
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "nothing retries compaction until the user sends again"
            );
        }

        #[tokio::test]
        async fn failed_resume_goes_idle_and_keeps_the_prompts_a_cancel_held() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("first".to_owned()));
            rx.try_recv().expect("first submit forwarded");
            app.handle_engine_event(Event::StreamToken("x".to_owned()));
            app.dispatch_action(Action::Submit("second".to_owned()));
            rx.try_recv().expect("second submit forwarded");
            app.handle_esc();
            assert_eq!(rx.try_recv().ok(), Some(Op::Interrupt));
            app.handle_engine_event(Event::Cancelled);
            app.dispatch_action(Action::Submit("/resume zzzz".to_owned()));
            assert_eq!(
                rx.try_recv().ok(),
                Some(Op::Resume {
                    target: ResumeTarget::Prefix("zzzz".to_owned()),
                }),
            );
            app.dispatch_action(Action::Submit("third".to_owned()));

            app.handle_engine_event(Event::TurnFailed(
                "Resume failed (still on session x): no session matching `zzzz`".to_owned(),
            ));

            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "nothing runs after a failed resume"
            );
            assert_eq!(app.status, Status::Idle);
            assert_eq!(*app.queue.pending(), ["second", "third"],);
        }

        #[tokio::test]
        async fn pending_queue_resubmits_prompts_each_ended_turn_dropped() {
            // A turn that ends before its next round boundary drops the queued prompts it held, so
            // each one must come back as a later turn.
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");
            for prompt in ["a", "b", "c"] {
                app.dispatch_action(Action::Submit(prompt.to_owned()));
            }
            let mut forwarded = || std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
            assert_eq!(
                forwarded(),
                [
                    Op::Queue("a".to_owned()),
                    Op::Queue("b".to_owned()),
                    Op::Queue("c".to_owned()),
                ],
            );

            app.handle_engine_event(Event::TurnFailed(
                "agent stopped after 3 tool rounds".to_owned(),
            ));
            assert_eq!(
                forwarded(),
                [
                    Op::Prompt("a".to_owned()),
                    Op::Queue("b".to_owned()),
                    Op::Queue("c".to_owned()),
                ],
            );

            app.handle_engine_event(Event::TurnComplete);
            assert_eq!(
                forwarded(),
                [Op::Prompt("b".to_owned()), Op::Queue("c".to_owned())],
            );

            app.handle_engine_event(Event::TurnComplete);
            assert_eq!(forwarded(), [Op::Prompt("c".to_owned())]);
            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
        }

        #[test]
        fn usage_updated_sets_status_bar_usage() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let usage = usage_snapshot();

            app.handle_engine_event(Event::UsageUpdated(usage));

            assert_eq!(app.status_bar.usage(), Some(usage));
            assert!(app.dirty);
        }

        #[test]
        fn cancelled_commits_partial_stream_with_marker_and_becomes_idle() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("hi".to_owned()));
            app.handle_engine_event(Event::StreamToken("partial answer".to_owned()));
            let entries_before = app.chat.entry_count();
            assert_eq!(app.status, Status::Streaming);

            app.handle_engine_event(Event::Cancelled);

            assert_eq!(app.status, Status::Idle);
            // commit_streaming pushes the partial text as an AssistantText block, and
            // push_interrupted_marker pushes the marker, adding two entries to whatever was there.
            assert_eq!(
                app.chat.entry_count(),
                entries_before + 2,
                "partial assistant text + interrupted marker",
            );
            let text = rendered_text(&mut app, 60, 12);
            assert!(text.contains("partial answer"), "stream tail kept: {text}");
            assert!(text.contains(INTERRUPTED_MARKER), "marker present: {text}");
        }

        #[test]
        fn cancelling_status_is_sticky_against_late_buffered_events() {
            // After the user dispatches Interrupt, the event channel may still hold StreamToken /
            // ToolCallStart events the engine emitted before its select arm dropped the turn
            // future. Those buffered events must not flip the bar back to Streaming / ToolRunning,
            // or the cancel acknowledgment flickers off until `Cancelled` finally arrives.
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("hi".to_owned()));
            app.dispatch_action(Action::Op(Op::Interrupt));
            assert_eq!(app.status, Status::Cancelling);

            app.handle_engine_event(Event::StreamToken("late".to_owned()));
            assert_eq!(app.status, Status::Cancelling);
            app.handle_engine_event(bash_call_start("t-late", "ls"));
            assert_eq!(app.status, Status::Cancelling);
        }

        #[test]
        fn cancelled_with_no_stream_still_pushes_marker() {
            // A cancel during a tool call (or before any stream tokens) makes commit_streaming a
            // no-op, but the marker still lands so the user sees where the cancel hit.
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("hi".to_owned()));
            let entries_before = app.chat.entry_count();

            app.handle_engine_event(Event::Cancelled);

            assert_eq!(app.chat.entry_count(), entries_before + 1);
            let text = rendered_text(&mut app, 60, 8);
            assert!(text.contains(INTERRUPTED_MARKER), "marker present: {text}");
        }

        #[tokio::test]
        async fn cancelled_goes_idle_and_esc_recalls_the_queued_prompt() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("first".to_owned()));
            rx.try_recv().expect("first submit forwarded");
            app.handle_engine_event(Event::StreamToken("x".to_owned()));
            app.dispatch_action(Action::Submit("second".to_owned()));
            rx.try_recv().expect("second submit forwarded");
            app.handle_esc();
            assert_eq!(rx.try_recv().ok(), Some(Op::Interrupt));

            app.handle_engine_event(Event::Cancelled);

            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "nothing resubmits after a cancel"
            );
            assert_eq!(app.status, Status::Idle);
            app.handle_esc();
            assert_eq!(app.input.lines(), ["second".to_owned()]);
            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
        }

        #[test]
        fn auto_compaction_started_sets_compacting_status() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active question".to_owned()));

            app.handle_engine_event(Event::AutoCompactionStarted);

            assert_eq!(app.status, Status::Compacting);
        }

        #[test]
        fn auto_compaction_paused_until_resumed_marks_the_status_pause() {
            let (mut app, _rx, _event_tx) = test_app(None);

            app.handle_engine_event(Event::AutoCompactionPaused { failures: 3 });
            let paused = app.session_info.auto_compaction_paused_after;
            app.handle_engine_event(Event::AutoCompactionResumed);

            assert_eq!(paused, Some(3));
            assert_eq!(app.session_info.auto_compaction_paused_after, None);
        }

        #[test]
        fn session_title_updated_overwrites_existing_title_for_current_session() {
            let (mut app, _rx, _event_tx) = test_app(Some("First prompt"));
            app.handle_engine_event(Event::SessionTitleUpdated {
                id: app.session_info.session_id.clone(),
                title: "AI-generated".to_owned(),
            });
            assert_eq!(app.status_bar.title(), Some("AI-generated"));
            assert!(app.dirty);
        }

        #[test]
        fn session_title_updated_drops_event_for_stale_session_id() {
            // A title task spawned before `/clear` can finish after the roll, so its event must not
            // paint the old session's title onto the current one.
            let (mut app, _rx, _event_tx) = test_app(Some("First prompt"));
            app.handle_engine_event(Event::SessionTitleUpdated {
                id: "different-session".to_owned(),
                title: "Stale title".to_owned(),
            });
            assert_eq!(
                app.status_bar.title(),
                Some("First prompt"),
                "current-session title must survive a stale event",
            );
        }

        #[test]
        fn session_rolled_clears_chat_rebinds_id_and_drops_stale_title() {
            let (mut app, _rx, _event_tx) = test_app(Some("Old session title"));
            app.chat.push_user_message("old prompt".to_owned());
            let original_id = app.session_info.session_id.clone();
            app.status_bar.set_usage(Some(usage_snapshot()));
            app.session_info.session_allow =
                vec![Rule::parse(PermissionClass::Exec, "ls").unwrap()];
            app.session_info.background_shells = shells_in_each_state();
            app.tasks.set(plan());

            app.handle_engine_event(Event::SessionRolled {
                id: "rolled-session".to_owned(),
                archived: false,
            });

            assert_eq!(
                app.session_info.session_id, "rolled-session",
                "session ID must rebind to the rolled session",
            );
            assert_ne!(app.session_info.session_id, original_id);
            assert!(
                app.status_bar.title().is_none(),
                "stale AI title must be cleared on roll",
            );
            assert!(
                app.status_bar.usage().is_none(),
                "stale usage must be cleared on roll",
            );
            assert_eq!(app.session_info.session_allow, []);
            assert_eq!(app.session_info.background_shells, []);
            assert_eq!(app.tasks.tasks(), []);
            assert!(
                app.chat.is_empty(),
                "clear must drain the chat so the welcome can repaint",
            );
            assert!(app.dirty);
        }

        #[tokio::test]
        async fn session_rolled_drops_the_old_session_slash_tasks() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.slash_tasks
                .spawn(async { Ok(ChatOutput::SystemMessage("old session's output".to_owned())) });

            app.handle_engine_event(Event::SessionRolled {
                id: "rolled-session".to_owned(),
                archived: false,
            });

            assert!(app.slash_tasks.join_next().await.is_none());
        }

        #[test]
        fn session_rolled_after_archive_names_the_archived_session() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.chat.push_user_message("old prompt".to_owned());
            app.session_info.session_id = "aaaa1111-archived".to_owned();

            app.handle_engine_event(Event::SessionRolled {
                id: "rolled-session".to_owned(),
                archived: true,
            });

            assert_eq!(app.session_info.session_id, "rolled-session");
            assert_eq!(app.chat.entry_count(), 1);
            let text = rendered_text(&mut app, 100, 12);
            assert!(text.contains("Archived session aaaa1111."), "{text}");
            assert!(!text.contains("old prompt"), "{text}");
        }

        #[test]
        fn session_rolled_discards_the_queue_with_a_notice() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.queue.push("queued before clear".to_owned());

            app.handle_engine_event(Event::SessionRolled {
                id: "rolled-session".to_owned(),
                archived: false,
            });

            assert_eq!(*app.queue.pending(), [] as [&str; 0]);
            assert_eq!(
                app.chat.last_system_text(),
                Some("Discarded 1 queued prompt from the previous session"),
            );
        }

        #[test]
        fn session_resumed_replays_transcript_and_clears_pending() {
            let (mut app, _rx, _event_tx) = test_app(Some("Old"));
            app.session_info.config.model = ModelSpec::resolve("claude-opus-5-5");
            app.chat.push_user_message("live prompt".to_owned());
            app.status_bar.set_usage(Some(usage_snapshot()));
            app.queue.push("queued-a".to_owned());
            app.queue.push("queued-b".to_owned());
            app.handle_engine_event(bash_call_start("pending-1", "ls"));
            app.session_info.session_allow =
                vec![Rule::parse(PermissionClass::Exec, "ls").unwrap()];
            app.session_info.background_shells = shells_in_each_state();
            app.tasks.set(plan());
            let original_id = app.session_info.session_id.clone();

            app.handle_engine_event(Event::SessionResumed {
                id: "resumed-session".to_owned(),
                title: Some("Resumed title".to_owned()),
                unarchived: false,
                history: vec![
                    Interaction::UserText("resumed user".to_owned()),
                    Interaction::AssistantText("resumed assistant".to_owned()),
                ],
            });

            assert_eq!(app.session_info.session_id, "resumed-session");
            assert_ne!(app.session_info.session_id, original_id);
            assert_eq!(app.status_bar.title(), Some("Resumed title"));
            assert!(
                app.status_bar.usage().is_none(),
                "stale usage must be cleared on resume",
            );
            assert_eq!(app.session_info.session_allow, []);
            assert_eq!(app.session_info.background_shells, []);
            assert_eq!(app.tasks.tasks(), []);
            assert_eq!(
                app.chat.entry_count(),
                3,
                "chat must reflect the resumed transcript + the queued-prompt-discarded notice",
            );
            assert_eq!(
                app.chat.last_system_text(),
                Some("Discarded 2 queued prompts from the previous session"),
            );
            assert_eq!(
                app.chat.pending_call_count(),
                0,
                "pending tool calls must drop on resume",
            );
            assert_eq!(
                *app.queue.pending(),
                [] as [&str; 0],
                "queued prompts must drop on resume"
            );
            assert!(app.dirty);
        }

        #[test]
        fn session_resumed_with_no_queued_prompts_is_silent() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.session_info.config.model = ModelSpec::resolve("claude-opus-5-5");
            app.handle_engine_event(Event::SessionResumed {
                id: "resumed".to_owned(),
                title: None,
                unarchived: false,
                history: vec![Interaction::UserText("only msg".to_owned())],
            });
            assert_eq!(
                app.chat.entry_count(),
                1,
                "no queued prompts → no discarded-prompts notice",
            );
        }

        #[test]
        fn session_resumed_after_unarchiving_appends_a_notice() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.session_info.config.model = ModelSpec::resolve("claude-opus-5-5");
            app.handle_engine_event(Event::SessionResumed {
                id: "bbbb2222-resumed".to_owned(),
                title: None,
                unarchived: true,
                history: vec![Interaction::UserText("only msg".to_owned())],
            });

            assert_eq!(app.chat.entry_count(), 2);
            let text = rendered_text(&mut app, 80, 12);
            assert!(text.contains("Unarchived session bbbb2222"), "{text}");
        }

        #[test]
        fn session_resumed_with_compact_replays_boundary_block() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.session_info.config.model = ModelSpec::resolve("claude-opus-5-5");
            app.handle_engine_event(Event::SessionResumed {
                id: "resumed".to_owned(),
                title: None,
                unarchived: false,
                history: vec![
                    Interaction::Compacted {
                        summary: "Compact summary".to_owned(),
                        pre_count: 4,
                        instructions: None,
                        trigger: None,
                    },
                    Interaction::AssistantText("post-compact reply".to_owned()),
                ],
            });

            assert_eq!(app.chat.entry_count(), 2);
            let text = rendered_text(&mut app, 80, 12);
            assert!(text.contains("Compacted 4 messages"));
            assert!(text.contains("Compact summary"));
            assert!(text.contains("post-compact reply"));
        }

        #[test]
        fn session_resumed_with_no_title_clears_stale_chrome() {
            let (mut app, _rx, _event_tx) = test_app(Some("Stale"));
            app.session_info.config.model = ModelSpec::resolve("claude-opus-5-5");
            app.handle_engine_event(Event::SessionResumed {
                id: "resumed".to_owned(),
                title: None,
                unarchived: false,
                history: Vec::new(),
            });
            assert!(
                app.status_bar.title().is_none(),
                "Some(None) title must clear the chrome",
            );
            assert!(app.chat.is_empty());
        }

        #[test]
        fn session_resumed_with_an_unsupported_model_repeats_its_notice() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(Event::SessionResumed {
                id: "resumed".to_owned(),
                title: None,
                unarchived: false,
                history: vec![Interaction::UserText("only msg".to_owned())],
            });

            assert_eq!(app.chat.entry_count(), 2);
            let text = rendered_text(&mut app, 120, 16);
            let prompt_at = text.find("only msg").unwrap();
            let notice_at = text.find("`test-model` is not a supported model").unwrap();
            assert!(prompt_at < notice_at, "{text}");
        }

        #[test]
        fn session_compacted_replays_summary_and_clears_pending_calls() {
            let (mut app, _rx, _event_tx) = test_app(Some("Pre-compact"));
            app.chat.push_user_message("pre-compact prompt".to_owned());
            app.status_bar.set_usage(Some(usage_snapshot()));
            app.handle_engine_event(bash_call_start("pending-1", "ls"));
            app.tasks.set(plan());

            app.handle_engine_event(Event::SessionCompacted {
                summary: "## Recap\n\nDid the thing.".to_owned(),
                pre_count: 4,
                instructions: Some("focus on auth".to_owned()),
                trigger: CompactTrigger::Manual,
            });

            assert_eq!(
                app.chat.entry_count(),
                1,
                "chat must collapse to the single CompactedBlock",
            );
            assert_eq!(
                app.chat.pending_call_count(),
                0,
                "pending tool calls must drop on compact",
            );
            assert!(
                app.status_bar.usage().is_none(),
                "stale usage must be cleared on compact",
            );
            assert_eq!(
                app.tasks.tasks(),
                plan(),
                "the task list outlives compaction"
            );
            assert!(app.dirty);
        }

        #[test]
        fn session_compacted_without_instructions_renders_clean_block() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(Event::SessionCompacted {
                summary: "summary only".to_owned(),
                pre_count: 2,
                instructions: None,
                trigger: CompactTrigger::Manual,
            });
            assert_eq!(app.chat.entry_count(), 1, "exactly one boundary block");
        }

        #[test]
        fn session_compacted_automatic_keeps_busy_state_and_queued_prompts() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.set_status(Status::Compacting);
            app.queue.push("queued while busy".to_owned());

            app.handle_engine_event(Event::SessionCompacted {
                summary: "auto summary".to_owned(),
                pre_count: 4,
                instructions: None,
                trigger: CompactTrigger::Auto,
            });

            assert_eq!(app.chat.entry_count(), 1);
            assert_eq!(app.status, Status::Compacting);
            assert_eq!(*app.queue.pending(), ["queued while busy"]);
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "automatic compact must not drain early"
            );
        }

        #[tokio::test]
        async fn session_compacted_automatic_replays_active_prompt_after_summary() {
            let (mut app, mut rx, _event_tx) = test_app(None);

            app.dispatch_action(Action::Submit("//etc/hosts".to_owned()));
            let forwarded = rx.try_recv().expect("prompt reaches the engine");
            assert_eq!(forwarded, Op::Prompt("/etc/hosts".to_owned()));
            app.handle_engine_event(Event::AutoCompactionStarted);

            app.handle_engine_event(Event::SessionCompacted {
                summary: "auto summary".to_owned(),
                pre_count: 4,
                instructions: None,
                trigger: CompactTrigger::Auto,
            });

            assert_eq!(app.chat.entry_count(), 2);
            assert_eq!(app.status, Status::Compacting);
            let text = rendered_text(&mut app, 80, 10);
            assert!(text.contains("auto summary"));
            assert!(
                text.contains("/etc/hosts") && !text.contains("//etc"),
                "the replay shows the literal text: {text}"
            );
        }

        #[tokio::test]
        async fn session_compacted_mid_turn_leaves_only_later_prompts_to_a_rollback() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("prompt forwarded");
            app.dispatch_action(Action::Submit("before".to_owned()));
            rx.try_recv().expect("queued prompt forwarded");
            app.handle_engine_event(Event::PromptDrained("before".to_owned()));
            app.handle_engine_event(Event::AutoCompactionStarted);
            app.handle_engine_event(Event::SessionCompacted {
                summary: "mid-turn summary".to_owned(),
                pre_count: 6,
                instructions: None,
                trigger: CompactTrigger::MidTurn,
            });
            app.handle_engine_event(Event::StreamToken("Resuming.".to_owned()));
            app.dispatch_action(Action::Submit("after".to_owned()));
            rx.try_recv().expect("queued prompt forwarded");
            app.handle_engine_event(Event::PromptDrained("after".to_owned()));

            app.handle_engine_event(Event::TurnRolledBack);
            app.handle_engine_event(Event::TurnFailed(
                "The model declined this request.".to_owned(),
            ));

            assert!(app.input.is_empty(), "the summary holds the active prompt");
            assert_eq!(*app.queue.pending(), ["after"]);
            let text = rendered_text(&mut app, 80, 40);
            assert!(
                text.contains("Compacted 6 messages") && !text.contains("active"),
                "the summary stands in for the prompt: {text}"
            );
        }

        #[test]
        fn session_compacted_mid_turn_keeps_the_open_modal_and_its_theme_preview() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let latte = builtin::find("latte").expect("a built-in theme");
            app.push_modal(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
            app.dispatch_action(Action::PreviewTheme(latte));
            let previewed = app.theme.text;

            app.handle_engine_event(Event::SessionCompacted {
                summary: "mid-turn summary".to_owned(),
                pre_count: 6,
                instructions: None,
                trigger: CompactTrigger::MidTurn,
            });

            assert!(app.modals.is_active());
            assert!(app.preview_theme_snapshot.is_some());
            assert_eq!(app.theme.text, previewed);
        }

        #[tokio::test]
        async fn session_ready_after_a_compaction_starts_the_queue_head_below_its_notices() {
            // Compaction keeps the session, so a queued prompt becomes the first post-compact turn.
            // `/resume` swaps sessions and drops it.
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.queue.push("queued before compact".to_owned());
            app.dispatch_op(Op::Compact { instructions: None });
            assert_eq!(rx.try_recv().ok(), Some(Op::Compact { instructions: None }));

            app.handle_engine_event(Event::SessionCompacted {
                summary: "s".to_owned(),
                pre_count: 2,
                instructions: None,
                trigger: CompactTrigger::Manual,
            });
            app.handle_engine_event(Event::Notice(
                "Skipped instruction file CLAUDE.md".to_owned(),
            ));
            let before_ready = rx.try_recv();
            app.handle_engine_event(Event::SessionReady);

            assert_eq!(before_ready, Err(TryRecvError::Empty));
            assert_eq!(
                rx.try_recv().ok(),
                Some(Op::Prompt("queued before compact".to_owned())),
            );
            assert_eq!(app.status, Status::Streaming);
            let text = rendered_text(&mut app, 80, 12);
            let notice_at = text.find("Skipped instruction file CLAUDE.md").unwrap();
            let prompt_at = text.find("queued before compact").unwrap();
            assert!(notice_at < prompt_at, "{text}");
        }

        #[tokio::test]
        async fn session_ready_after_a_cancelled_compaction_keeps_the_queue_waiting() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.queue.push("queued before compact".to_owned());
            app.dispatch_op(Op::Compact { instructions: None });
            app.dispatch_op(Op::Interrupt);
            assert_eq!(rx.try_recv().ok(), Some(Op::Compact { instructions: None }));
            assert_eq!(rx.try_recv().ok(), Some(Op::Interrupt));

            app.handle_engine_event(Event::SessionCompacted {
                summary: "s".to_owned(),
                pre_count: 2,
                instructions: None,
                trigger: CompactTrigger::Manual,
            });
            app.handle_engine_event(Event::SessionReady);

            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
            assert_eq!(*app.queue.pending(), ["queued before compact"]);
            assert_eq!(app.status, Status::Idle);
        }

        #[tokio::test]
        async fn session_ready_ends_a_resume_after_the_swap() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            let resume = Op::Resume {
                target: ResumeTarget::Prefix("1a2b".to_owned()),
            };
            app.dispatch_op(resume.clone());
            assert_eq!(rx.try_recv().ok(), Some(resume));

            app.handle_engine_event(Event::SessionResumed {
                id: "resumed-session".to_owned(),
                title: None,
                unarchived: false,
                history: Vec::new(),
            });
            let at_swap = app.status.clone();
            app.handle_engine_event(Event::SessionReady);

            assert_eq!(at_swap, Status::Resuming);
            assert_eq!(app.status, Status::Idle);
        }

        #[tokio::test]
        async fn session_ready_ends_an_archive_and_starts_the_prompt_queued_during_it() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_op(Op::Archive);
            assert_eq!(rx.recv().await, Some(Op::Archive));
            app.dispatch_action(Action::Submit("next".to_owned()));
            let during = rx.try_recv();

            app.handle_engine_event(Event::SessionRolled {
                id: "rolled-session".to_owned(),
                archived: true,
            });
            let at_roll = app.status.clone();
            app.handle_engine_event(Event::SessionReady);

            assert_eq!(during, Err(TryRecvError::Empty));
            assert_eq!(at_roll, Status::Archiving);
            assert_eq!(rx.recv().await, Some(Op::Prompt("next".to_owned())));
            assert_eq!(app.status, Status::Streaming);
        }

        #[test]
        fn config_changed_model_swap_refreshes_status_and_chat() {
            // Three surfaces refresh in one shot: status-bar label, `session_info` (backs `/status`
            // / `/config`), and a chat confirmation block.
            let (mut app, _rx, _event_tx) = test_app(None);
            let sonnet = ModelSpec::resolve("claude-sonnet-5-5");
            app.handle_engine_event(Event::ConfigChanged {
                model: sonnet.clone(),
                effort: Some(Effort::High),
                requested: model_pick(&sonnet, None),
                auto_compaction: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(WINDOW_1M_THRESHOLD),
                },
                notice: "Switched to Claude Sonnet 5.5 (claude-sonnet-5-5) · effort high"
                    .to_owned(),
            });

            assert_eq!(app.session_info.config.model.id(), "claude-sonnet-5-5");
            assert_eq!(app.session_info.config.effort, Some(Effort::High),);
            assert_eq!(app.status_bar.model(), "Sonnet 5.5");
            assert_eq!(
                app.session_info.config.compaction.auto.threshold_tokens,
                Some(WINDOW_1M_THRESHOLD),
            );
            assert_eq!(
                app.chat.last_system_text(),
                Some("Switched to Claude Sonnet 5.5 (claude-sonnet-5-5) · effort high"),
            );
            assert!(app.dirty);
        }

        #[test]
        fn config_changed_effort_only_keeps_status_bar_model_label() {
            // An effort-only swap leaves the cached model label alone, so only the snapshot effort
            // and chat confirmation update.
            let (mut app, _rx, _event_tx) = test_app(None);
            let original_model = app.status_bar.model().to_owned();
            app.handle_engine_event(Event::ConfigChanged {
                model: app.session_info.config.model.clone(),
                effort: Some(Effort::Xhigh),
                requested: ConfigPick::Effort(Effort::Xhigh),
                auto_compaction: app.session_info.config.compaction.auto,
                notice: "Effort set to xhigh".to_owned(),
            });

            assert_eq!(app.session_info.config.effort, Some(Effort::Xhigh),);
            assert_eq!(app.status_bar.model(), original_model);
            assert_eq!(app.chat.last_system_text(), Some("Effort set to xhigh"));
            assert!(app.dirty);
        }

        #[test]
        fn mode_changed_records_the_session_mode_and_shows_it() {
            let (mut app, _rx, _event_tx) = test_app(None);

            app.handle_engine_event(Event::ModeChanged(Mode::Plan));

            assert_eq!(app.session_info.config.permission_mode, Mode::Plan);
            assert_eq!(
                app.session_info.config.provenance.permission_mode,
                Source::Session
            );
            assert_eq!(app.chat.last_system_text(), Some("Switched to plan mode"));
            let text = rendered_text(&mut app, 120, 12);
            assert!(text.contains("Mode: plan"), "{text}");
        }

        #[test]
        fn login_started_opens_the_browser_and_prints_the_url() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let started = || Event::LoginStarted {
                url: "https://example.com/authorize".to_owned(),
            };

            app.open_browser = |url| {
                assert_eq!(url, "https://example.com/authorize");
                Ok(())
            };
            app.handle_engine_event(started());
            assert_eq!(
                app.chat.last_system_text(),
                Some(indoc! {"
                    Opened the browser to sign in. If it did not open, visit this URL in a browser on this machine:
                    https://example.com/authorize"
                }),
            );

            app.open_browser = |_| Err(io::Error::other("no browser"));
            app.handle_engine_event(started());
            assert_eq!(
                app.chat.last_system_text(),
                Some(indoc! {"
                    Could not open the browser. Sign in at this URL in a browser on this machine:
                    https://example.com/authorize"
                }),
            );
        }

        #[test]
        fn signed_in_names_the_credential_requests_use() {
            for (auth, expected) in [
                (
                    AuthSource::OxLogin,
                    "Signed in. Requests now use OAuth (oxide-code login).",
                ),
                (
                    AuthSource::ApiKeyEnv,
                    "Signed in. \
                     API key (ANTHROPIC_API_KEY) takes precedence, so requests keep using it. \
                     Unset it and restart ox to use the new login.",
                ),
                (
                    AuthSource::ApiKeyFile,
                    "Signed in. \
                     API key (client.api_key) takes precedence, so requests keep using it. \
                     Remove `client.api_key` and restart ox to use the new login.",
                ),
            ] {
                let (mut app, _rx, _event_tx) = test_app(None);
                app.session_info.config.auth = None;
                app.set_status(Status::SigningIn);

                app.handle_engine_event(Event::SignedIn { auth });

                assert_eq!(app.session_info.config.auth, Some(auth));
                assert_eq!(app.chat.last_system_text(), Some(expected));
                assert_eq!(app.status, Status::Idle);
            }
        }

        #[tokio::test]
        async fn signed_in_runs_a_prompt_held_while_signing_in() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_op(Op::Login);
            rx.try_recv().expect("login forwarded");
            app.dispatch_action(Action::Submit("held".to_owned()));

            app.handle_engine_event(Event::SignedIn {
                auth: AuthSource::OxLogin,
            });

            assert_eq!(rx.try_recv().ok(), Some(Op::Prompt("held".to_owned())));
            assert_eq!(app.status, Status::Streaming);
        }

        #[tokio::test]
        async fn signed_in_after_a_lost_cancel_keeps_the_held_prompt_queued() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_op(Op::Login);
            rx.try_recv().expect("login forwarded");
            app.dispatch_action(Action::Submit("held".to_owned()));
            app.set_status(Status::Cancelling);

            app.handle_engine_event(Event::SignedIn {
                auth: AuthSource::OxLogin,
            });

            assert_eq!(app.status, Status::Idle);
            assert_eq!(*app.queue.pending(), ["held"]);
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "the held prompt must not run"
            );
        }

        #[test]
        fn login_restored_names_the_credential_and_keeps_the_turn_running() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.session_info.config.auth = None;
            app.set_status(Status::Streaming);

            app.handle_engine_event(Event::LoginRestored {
                auth: AuthSource::ClaudeCodeLogin,
            });

            assert_eq!(
                app.session_info.config.auth,
                Some(AuthSource::ClaudeCodeLogin)
            );
            assert_eq!(
                app.chat.last_system_text(),
                Some("Signed in. Requests now use OAuth (Claude Code login).")
            );
            assert_eq!(app.status, Status::Streaming);
        }

        #[test]
        fn signed_out_reports_the_removal_and_the_credential_requests_use() {
            for (removed, auth, fallback_error, expected) in [
                (
                    true,
                    Some(AuthSource::ClaudeCodeLogin),
                    None,
                    "Signed out of oxide-code. Requests use OAuth (Claude Code login).",
                ),
                (
                    true,
                    None,
                    None,
                    "Signed out of oxide-code. No credential is active. Run `/login` to sign in.",
                ),
                (
                    true,
                    None,
                    Some("Claude Code login has expired. Run `claude` to sign in again."),
                    "Signed out of oxide-code. \
                     No credential is active: Claude Code login has expired. \
                     Run `claude` to sign in again.",
                ),
                (
                    false,
                    Some(AuthSource::ApiKeyEnv),
                    None,
                    "No oxide-code login is stored. Requests use API key (ANTHROPIC_API_KEY).",
                ),
            ] {
                let (mut app, _rx, _event_tx) = test_app(None);
                app.session_info.config.auth = Some(AuthSource::OxLogin);

                app.handle_engine_event(Event::SignedOut {
                    removed,
                    auth,
                    fallback_error: fallback_error.map(ToOwned::to_owned),
                });

                assert_eq!(app.session_info.config.auth, auth);
                assert_eq!(app.chat.last_system_text(), Some(expected));
            }
        }

        #[tokio::test]
        async fn signed_out_ends_a_logout_and_starts_the_prompt_queued_during_it() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_op(Op::Logout);
            assert_eq!(rx.recv().await, Some(Op::Logout));
            app.dispatch_action(Action::Submit("next".to_owned()));
            let during = (app.status.clone(), rx.try_recv());

            app.handle_engine_event(Event::SignedOut {
                removed: true,
                auth: Some(AuthSource::ApiKeyEnv),
                fallback_error: None,
            });

            assert_eq!(during, (Status::SigningOut, Err(TryRecvError::Empty)));
            assert_eq!(rx.recv().await, Some(Op::Prompt("next".to_owned())));
            assert_eq!(app.status, Status::Streaming);
        }

        #[test]
        fn credential_changes_drop_the_previous_accounts_usage_limits() {
            for event in [
                Event::SignedIn {
                    auth: AuthSource::OxLogin,
                },
                Event::LoginRestored {
                    auth: AuthSource::ClaudeCodeLogin,
                },
                Event::SignedOut {
                    removed: true,
                    auth: Some(AuthSource::ApiKeyEnv),
                    fallback_error: None,
                },
            ] {
                let (mut app, _rx, _event_tx) = test_app(None);
                app.handle_engine_event(Event::RateLimitsUpdated(five_hour_limit()));
                let label = format!("{event:?}");

                app.handle_engine_event(event);

                assert_eq!(app.status_bar.rate_limits(), None, "{label}");
            }
        }

        #[test]
        fn rename_failed_shows_the_error_and_keeps_the_title() {
            let (mut app, _rx, _event_tx) = test_app(Some("First prompt"));

            app.handle_engine_event(Event::RenameFailed(
                "Session write failed: disk full".to_owned(),
            ));

            assert_eq!(
                app.chat.last_error_text(),
                Some("Session write failed: disk full")
            );
            assert_eq!(app.status_bar.title(), Some("First prompt"));
            assert!(app.status.is_idle());
        }

        #[test]
        fn approval_with_a_grant_offers_allow_for_session() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(Event::ApprovalRequested {
                id: "call-1".to_owned(),
                preview: ApprovalPreview {
                    title: "Bash".to_owned(),
                    body: ApprovalBody::Command("cargo test".to_owned()),
                },
                basis: Basis::Mode(Mode::Manual),
                grant: Some(Box::new(exec_grant("cargo test"))),
            });

            let text = rendered_text(&mut app, 100, 24);
            assert!(text.contains("[A] allow for session"), "{text}");
        }

        #[test]
        fn session_rules_updated_replaces_the_rules_status_lists() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let rules = [
                (PermissionClass::Exec, "cargo test *"),
                (PermissionClass::Edit, "./out/**"),
            ]
            .map(|(class, pattern)| Rule::parse(class, pattern).unwrap());

            app.handle_engine_event(Event::SessionRulesUpdated(vec![rules[0].clone()]));
            app.handle_engine_event(Event::SessionRulesUpdated(rules.to_vec()));

            assert_eq!(app.session_info.session_allow, rules);
            assert!(app.dirty);
        }

        #[test]
        fn tasks_updated_replaces_the_panel_list() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(Event::TasksUpdated(vec![task(
                "Sketch the design",
                TaskStatus::Pending,
            )]));

            app.handle_engine_event(Event::TasksUpdated(plan()));

            assert_eq!(app.tasks.tasks(), plan());
            assert!(app.dirty);
        }

        #[test]
        fn background_shells_updated_replaces_the_status_list() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(Event::BackgroundShellsUpdated(vec![shell(
                "shell-1",
                "cargo watch",
                ShellState::Running,
            )]));
            app.dirty = false;

            app.handle_engine_event(Event::BackgroundShellsUpdated(shells_in_each_state()));

            assert_eq!(app.session_info.background_shells, shells_in_each_state());
            assert!(app.dirty);
        }
    }

    mod finish_turn {
        use super::*;

        #[test]
        fn evicts_orphaned_pending_calls() {
            // A `ToolCallStart` without a matching `ToolCallEnd` by the turn's end is an orphan
            // (crashed tool, agent-loop bug, mid-turn abort). The pending entry must be discarded
            // so long sessions don't accumulate stale IDs across turns.
            let (mut app, _rx, _event_tx) = test_app(None);
            app.handle_engine_event(bash_call_start("orphan", "ls"));
            assert_eq!(app.chat.pending_call_count(), 1);
            app.handle_engine_event(Event::TurnComplete);
            assert_eq!(
                app.chat.pending_call_count(),
                0,
                "turn end must evict calls whose result never arrived",
            );
        }
    }

    mod apply_git_update {
        use super::*;

        #[test]
        fn marks_dirty_only_when_the_status_bar_changes() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dirty = false;

            app.apply_git_update(GitUpdate::Branch(Some("main".to_owned())));
            assert!(!app.dirty, "unchanged branch skips the repaint");

            app.apply_git_update(GitUpdate::PullRequest(Some(PullRequest {
                number: 7,
                url: "https://example.com/pull/7".to_owned(),
                checks: None,
                review: None,
            })));
            assert!(app.dirty, "new pull request repaints");
        }
    }

    mod apply_copy_report {
        use super::*;

        #[test]
        fn queues_only_osc52_and_notes_failures() {
            let (mut app, _rx, _event_tx) = test_app(None);

            app.apply_copy_report(CopyReport {
                chars: 4,
                outcome: Ok(Delivery::Copied),
            });
            let copied = app.copy_notice.take().unwrap();
            app.apply_copy_report(CopyReport {
                chars: 4,
                outcome: Err(CopyError::TimedOut),
            });
            let failed = app.copy_notice.take().unwrap();

            assert_eq!(app.pending_clipboard, None);
            assert_eq!(
                (copied.text.as_str(), copied.failed),
                ("Copied 4 characters", false)
            );
            assert_eq!(
                (failed.text.as_str(), failed.failed),
                ("Not copied: the clipboard did not answer within 2 s", true),
            );
        }
    }

    mod apply_slash_task {
        use super::*;

        async fn finish(
            output: Result<ChatOutput, String>,
        ) -> Result<Result<ChatOutput, String>, JoinError> {
            tokio::spawn(async { output }).await
        }

        #[tokio::test]
        async fn prints_the_message_diff_or_error_the_task_resolved_to() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let diff = WorkingTreeDiff {
                untracked: vec!["new.txt".to_owned()],
                ..WorkingTreeDiff::default()
            };

            app.apply_slash_task(finish(Ok(ChatOutput::SystemMessage("clean".to_owned()))).await);
            assert_eq!(app.chat.last_system_text(), Some("clean"));

            app.dirty = false;
            app.apply_slash_task(finish(Ok(ChatOutput::GitDiff(diff))).await);
            assert!(app.dirty);
            assert!(rendered_text(&mut app, 60, 24).contains("  new.txt"));

            app.apply_slash_task(finish(Err("/diff: boom".to_owned())).await);
            assert_eq!(app.chat.last_error_text(), Some("/diff: boom"));
        }

        #[tokio::test]
        async fn a_panicked_task_prints_nothing() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let mut tasks = JoinSet::<Result<ChatOutput, String>>::new();
            tasks.spawn(async { panic!("task bug") });
            let finished = tasks.join_next().await.expect("one task spawned");

            app.apply_slash_task(finished);

            assert_eq!(app.chat.entry_count(), 0);
        }
    }

    mod expire_copy_notice {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn clears_the_notice_once_its_time_is_up() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.apply_copy_report(CopyReport {
                chars: 4,
                outcome: Ok(Delivery::Copied),
            });

            let just_before = NOTICE_DURATION
                .checked_sub(Duration::from_millis(1))
                .unwrap();
            tokio::time::advance(just_before).await;
            assert!(!app.expire_copy_notice(now()));
            assert!(app.copy_notice.is_some());

            tokio::time::advance(Duration::from_millis(1)).await;
            assert!(app.expire_copy_notice(now()));
            assert!(app.copy_notice.is_none());
            assert!(!app.expire_copy_notice(now()), "nothing left to clear");
        }
    }
}
