//! Engine and background events that update TUI state.

use std::time::Instant;

use indoc::formatdoc;
use tokio::task::{JoinError, JoinSet};
use tracing::{debug, warn};

use ox_core::config::auth::AuthSource;
use ox_core::config::compaction::AutoCompactionConfig;
use ox_core::model::{Effort, ModelSpec};
use ox_core::permission::{Basis, Mode, SessionGrant};
use ox_core::protocol::{CompactTrigger, Event, Interaction, SavedTarget, notice};
use ox_core::session::display::{archive_notice, id_prefix};
use ox_core::session::entry::{ArchiveState, SessionInfo};
use ox_core::session::saved::{ArchiveUpdate, SavedError};
use ox_core::tool::view::ApprovalPreview;
use ox_core::util::text::plural;

use super::queue::FailedTurn;
use super::{App, CopyNotice, SlashTask};
use crate::clipboard::{CopyReport, Delivery};
use crate::components::status::Status;
use crate::git_probes::GitUpdate;
use crate::modal::approval::ApprovalModal;
use crate::slash;
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
            Event::Cancelled => self.apply_cancelled(),
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
            Event::ArchiveUpdated {
                target,
                state,
                result,
            } => self.apply_archive_updated(&target, state, &result),
            Event::SavedFound(result) => self.apply_saved_found(result),
            Event::SavedDeleted { id, result } => self.apply_saved_deleted(&id, &result),
            Event::SessionRolled { id, old_session } => self.apply_session_rolled(id, old_session),
            Event::SessionResumed {
                id,
                title,
                prior,
                history,
            } => self.apply_session_resumed(id, title, prior, history),
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
            Event::LoginStarted { url, manual_url } => self.open_sign_in_url(&url, &manual_url),
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

    /// Ends a cancelled operation, handing back a prompt the engine never sent and otherwise
    /// marking where the reply stopped.
    fn apply_cancelled(&mut self) {
        if self.queue.unsent() {
            self.settle_failed_turn();
        } else {
            self.chat.push_interrupted_marker();
            self.enter_idle();
        }
    }

    fn apply_prompt_drained(&mut self, text: String) {
        if let Some(display) = self.queue.confirm_drained(text) {
            self.chat.push_user_message(display);
        }
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

    /// Leaves held prompts queued, since a retained picker can submit another idle-only operation.
    fn apply_archive_updated(
        &mut self,
        target: &SavedTarget,
        state: ArchiveState,
        result: &Result<ArchiveUpdate, SavedError>,
    ) {
        self.modals.archive_updated(target, state, result);
        match slash::archive_message(target, state, result) {
            Ok(note) => self.chat.push_system_message(note),
            Err(error) => self.chat.push_error(&error),
        }
        self.enter_idle();
    }

    /// Opens the `/delete` confirm modal on the session the engine resolved the prefix to.
    fn apply_saved_found(&mut self, result: Result<Box<SessionInfo>, SavedError>) {
        match result {
            Ok(info) => self.push_modal(slash::delete_confirm_modal(&info)),
            Err(error) => self.chat.push_error(&slash::delete_lookup_error(&error)),
        }
        self.enter_idle();
    }

    /// Hands a delete result to the confirm modal, which closes with its chat line on success and
    /// shows the error on failure. Reports the result in chat when no modal claims it, as after the
    /// user dismissed the confirm modal while the delete ran.
    fn apply_saved_deleted(&mut self, id: &str, result: &Result<(), SavedError>) {
        match self.modals.saved_deleted(id, result) {
            Some(actions) => {
                for action in actions {
                    self.apply_modal_action(action);
                }
            }
            None => match result {
                Ok(()) => self
                    .chat
                    .push_system_message(format!("Deleted session {}", id_prefix(id))),
                Err(error) => self.chat.push_error(&slash::saved_error_message(
                    error,
                    slash::CLEAR_THEN_DELETE_HINT,
                )),
            },
        }
        self.enter_idle();
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

    /// Shows `msg` as an error, then settles the turn.
    fn apply_turn_failed(&mut self, msg: &str) {
        self.chat.push_error(msg);
        self.settle_failed_turn();
    }

    /// Settles a failed or cancelled turn by what the engine marked: an unsent prompt hands itself
    /// back, and a rolled-back turn also hands back the prompts it drained.
    fn settle_failed_turn(&mut self) {
        match self.queue.fail_turn(self.input.is_empty()) {
            FailedTurn::Ran => self.finish_turn(),
            FailedTurn::Held => {
                self.chat.commit_streaming();
                self.enter_idle();
            }
            FailedTurn::HandedBack(prompt) => {
                if let Some(prompt) = prompt {
                    self.input.set_text(prompt.line());
                    if prompt.is_slash() {
                        let line = prompt.line();
                        self.chat
                            .push_system_message(format!("Press Enter to run `{line}` again"));
                    }
                }
                self.enter_idle();
            }
        }
    }

    /// `/clear` or `/archive`: rebinds the session and empties chat for the fresh one.
    fn apply_session_rolled(&mut self, id: String, old_session: ArchiveState) {
        let old_id = self.rebind_session(id, None);
        if old_session == ArchiveState::Archived {
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
        prior: ArchiveState,
        history: Vec<Interaction>,
    ) {
        self.rebind_session(id, title);
        self.chat.load_history(history);
        if prior == ArchiveState::Archived {
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

    /// Repaints chat with one boundary block after a manual or automatic compaction. Queued prompts
    /// survive because the session identity stays the same. A mid-turn compaction also leaves open
    /// modals alone, since the user may have opened one at any point of a long turn.
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
                if let Some(active) = self.queue.active() {
                    self.chat.push_user_message(active.display().to_owned());
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

    /// Opens `url` in the browser, which returns to a listener on this machine, and prints
    /// `manual_url` for a browser that does not open or runs elsewhere, whose page shows a code to
    /// paste back.
    fn open_sign_in_url(&mut self, url: &str, manual_url: &str) {
        let message = match (self.open_browser)(url) {
            Ok(()) => formatdoc! {"
                Opened the browser to sign in. If it did not open or cannot reach this machine, sign in at this URL and paste the code it shows here:
                {manual_url}"
            },
            Err(e) => {
                debug!(error = %e, "failed to open the browser");
                formatdoc! {"
                    Could not open the browser. Sign in at this URL and paste the code it shows here:
                    {manual_url}"
                }
            }
        };
        self.chat.push_system_message(message);
    }

    fn apply_signed_in(&mut self, auth: AuthSource) {
        self.session_info.config.auth = Some(auth);
        self.status_bar.set_rate_limits(None);
        let recovery = match auth {
            AuthSource::OxLogin | AuthSource::ClaudeCodeLogin => None,
            AuthSource::ClientBearer | AuthSource::ClientApiKey | AuthSource::ClientOAuth => {
                Some("Remove `client.auth` and restart ox to use the new login.")
            }
            AuthSource::AuthTokenEnv | AuthSource::ApiKeyEnv | AuthSource::OAuthTokenEnv => {
                Some("Unset it and restart ox to use the new login.")
            }
        };
        let message = match recovery {
            Some(recovery) => {
                format!("Signed in. {auth} takes precedence, so requests keep using it. {recovery}")
            }
            None => format!("Signed in. Requests now use {auth}."),
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
        if let Some(next) = self.queue.pop_next() {
            self.start_turn(next);
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
            GitUpdate::Branch(branch) => {
                let changed = self.status_bar.set_git_branch(branch);
                if changed {
                    self.status_bar.set_pull_request(None);
                    self.git_probes.invalidate_pull_request(now());
                }
                changed
            }
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

    /// Applies what a background slash command resolved to through the same path as a typed one. A
    /// panicked task only logs, since the panic hook already reports the panic.
    pub(super) fn apply_slash_task(&mut self, finished: Result<SlashTask, JoinError>) {
        match finished {
            Ok(task) => {
                if let Some(action) = self.apply_slash_outcome(task.outcome, &task.line) {
                    self.dispatch_action(action);
                }
            }
            Err(e) => warn!(error = %e, "background slash command failed"),
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;
    use std::time::Duration;

    use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
    use indoc::indoc;
    use tokio::sync::mpsc::error::TryRecvError;

    use ox_core::config::StatusLineSegment;
    use ox_core::config::compaction::testing::WINDOW_1M_THRESHOLD;
    use ox_core::config::provenance::Source;
    use ox_core::model::ResolvedModelId;
    use ox_core::permission::PermissionClass;
    use ox_core::permission::rule::Rule;
    use ox_core::permission::testing::exec_grant;
    use ox_core::protocol::testing::text_result;
    use ox_core::protocol::{ConfigPick, Op, ResumeTarget, ToolCall, ToolResult, UsageSnapshot};
    use ox_core::session::entry::TitleInfo;
    use ox_core::session::resolver::ResolveError;
    use ox_core::session::saved::SavedAction;
    use ox_core::session::store::testing::{
        isolated_store, listed_session, seed_test_session, stamped_id,
    };
    use ox_core::tool::bash::background::ShellState;
    use ox_core::tool::bash::background::testing::{shell, shells_in_each_state};
    use ox_core::tool::tasks::TaskStatus;
    use ox_core::tool::tasks::testing::{plan, task};
    use ox_core::tool::view::{ApprovalBody, ToolResultView};
    use ox_core::util::git::testing::pull_request;

    use super::*;
    use crate::action::Action;
    use crate::app::testing::{five_hour_limit, rendered_text, test_app};
    use crate::clipboard::CopyError;
    use crate::glyphs::INTERRUPTED_MARKER;
    use crate::modal::ModalAction;
    use crate::modal::testing::ScriptedModal;
    use crate::slash::{ResumePicker, SlashOutcome, WorkingTreeDiff};
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
            let text = rendered_text(&mut app, 60, 9);
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
            assert_eq!(forwarded, [Op::prompt("a"), Op::Queue("b".to_owned())],);
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
            assert_eq!(
                app.chat.last_system_text(),
                None,
                "a typed prompt needs no note"
            );
            assert_eq!(*app.queue.pending(), ["drained", "queued"],);
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "nothing reaches the engine until the user sends"
            );
        }

        #[tokio::test]
        async fn turn_rolled_back_notes_that_enter_reruns_a_slash_command() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::SlashPrompt {
                line: "/init AGENTS.md".to_owned(),
                prompt: "Create AGENTS.md for this repository.".to_owned(),
            });
            rx.try_recv().expect("prompt forwarded");

            app.handle_engine_event(Event::TurnRolledBack);
            app.handle_engine_event(Event::TurnFailed(
                "The model declined this request.".to_owned(),
            ));

            assert_eq!(app.status, Status::Idle);
            assert_eq!(app.input.lines(), ["/init AGENTS.md".to_owned()]);
            assert_eq!(
                app.chat.last_system_text(),
                Some("Press Enter to run `/init AGENTS.md` again")
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
        async fn prompt_unsent_reruns_a_slash_prompt_as_built() {
            let root = tempfile::tempdir().unwrap();
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.session_info.project_root = root.path().to_owned();
            app.dispatch_action(Action::Submit("/init AGENTS.md".to_owned()));
            let original = rx.try_recv().expect("prompt forwarded");
            app.handle_engine_event(Event::AutoCompactionStarted);
            app.dispatch_action(Action::Submit("queued".to_owned()));
            app.handle_engine_event(Event::PromptUnsent);
            app.handle_engine_event(Event::TurnFailed(
                "Auto-compaction failed: summarizer down. The prompt was not sent.".to_owned(),
            ));
            assert_eq!(*app.queue.pending(), ["/init AGENTS.md", "queued"]);

            app.dispatch_action(Action::Submit("go".to_owned()));

            assert_eq!(rx.try_recv(), Ok(original));
            let text = rendered_text(&mut app, 80, 20);
            assert_eq!(text.matches("/init AGENTS.md").count(), 2, "{text}");
        }

        #[tokio::test]
        async fn prompt_unsent_after_a_signed_out_failure_keeps_the_queue_queued() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");
            app.dispatch_action(Action::Submit("queued".to_owned()));
            rx.try_recv().expect("queued submit forwarded");

            app.handle_engine_event(Event::PromptUnsent);
            app.handle_engine_event(Event::TurnFailed(
                "Not signed in. Run `/login` to sign in.".to_owned(),
            ));

            assert_eq!(
                app.chat.last_error_text(),
                Some("Not signed in. Run `/login` to sign in.")
            );
            assert_eq!(app.status, Status::Idle);
            assert!(app.input.is_empty());
            assert_eq!(
                *app.queue.pending(),
                ["active", "queued"],
                "the unsent prompt runs first, and the queued prompt waits behind it",
            );
            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "the queued prompt must not start another signed-out turn"
            );
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
                [Op::prompt("first"), Op::Queue("second".to_owned())],
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

            assert_eq!(rx.try_recv().unwrap(), Op::prompt("queued"));
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
        async fn turn_failed_during_a_manual_compaction_holds_the_prompts_queued_during_it() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_op(Op::Compact { instructions: None });
            assert_eq!(rx.try_recv().ok(), Some(Op::Compact { instructions: None }));
            app.dispatch_action(Action::Submit("typed meanwhile".to_owned()));

            app.handle_engine_event(Event::TurnFailed(
                "Compaction failed: summarizer down".to_owned(),
            ));

            assert_eq!(
                rx.try_recv(),
                Err(TryRecvError::Empty),
                "nothing retries compaction until the user sends again"
            );
            assert_eq!(*app.queue.pending(), ["typed meanwhile"]);
            assert_eq!(app.status, Status::Idle);
            assert_eq!(
                app.chat.last_error_text(),
                Some("Compaction failed: summarizer down")
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
                    Op::prompt("a"),
                    Op::Queue("b".to_owned()),
                    Op::Queue("c".to_owned()),
                ],
            );

            app.handle_engine_event(Event::TurnComplete);

            assert_eq!(forwarded(), [Op::prompt("b"), Op::Queue("c".to_owned())],);

            app.handle_engine_event(Event::TurnComplete);

            assert_eq!(forwarded(), [Op::prompt("c")]);
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
            // Events buffered before an interrupt must not clear the cancellation status before
            // `Cancelled` arrives.
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
        async fn cancelled_with_an_unsent_prompt_hands_it_back_without_a_marker() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            rx.try_recv().expect("active submit forwarded");
            app.dispatch_action(Action::Submit("queued".to_owned()));
            rx.try_recv().expect("queued submit forwarded");
            app.handle_engine_event(Event::PromptUnsent);

            app.handle_engine_event(Event::Cancelled);

            assert_eq!(app.status, Status::Idle);
            assert_eq!(*app.queue.pending(), ["active", "queued"]);
            let text = rendered_text(&mut app, 60, 12);
            assert!(
                !text.contains(INTERRUPTED_MARKER),
                "nothing was sent, so no reply was interrupted: {text}"
            );
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
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
                old_session: ArchiveState::Active,
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
            let task = SlashTask {
                line: "/diff".to_owned(),
                outcome: Ok(SlashOutcome::SystemMessage(
                    "old session's output".to_owned(),
                )),
            };
            app.slash_tasks.spawn(async move { task });

            app.handle_engine_event(Event::SessionRolled {
                id: "rolled-session".to_owned(),
                old_session: ArchiveState::Active,
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
                old_session: ArchiveState::Archived,
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
                old_session: ArchiveState::Active,
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
                prior: ArchiveState::Active,
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
                prior: ArchiveState::Active,
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
                prior: ArchiveState::Archived,
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
                prior: ArchiveState::Active,
                history: vec![
                    Interaction::Compacted {
                        summary: "Compact summary".to_owned(),
                        pre_count: 4,
                        instructions: None,
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
                prior: ArchiveState::Active,
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
                prior: ArchiveState::Active,
                history: vec![Interaction::UserText("only msg".to_owned())],
            });

            assert_eq!(app.chat.entry_count(), 2);
            let text = rendered_text(&mut app, 120, 16);
            let prompt_at = text.find("only msg").unwrap();
            let notice_at = text.find("`test-model` is not a supported model").unwrap();
            assert!(prompt_at < notice_at, "{text}");
            assert!(
                text.contains("Auto-compaction requires an explicit token threshold."),
                "{text}",
            );
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
            let slash_prompt = Action::SlashPrompt {
                line: "/init".to_owned(),
                prompt: "Create AGENTS.md for this repository.".to_owned(),
            };
            for (action, shown, hidden) in [
                (
                    Action::Submit("//etc/hosts".to_owned()),
                    "/etc/hosts",
                    "//etc",
                ),
                (slash_prompt, "/init", "Create AGENTS.md"),
            ] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.dispatch_action(action);
                rx.try_recv().expect("prompt reaches the engine");
                app.handle_engine_event(Event::AutoCompactionStarted);

                app.handle_engine_event(Event::SessionCompacted {
                    summary: "auto summary".to_owned(),
                    pre_count: 4,
                    instructions: None,
                    trigger: CompactTrigger::Auto,
                });

                assert_eq!(app.chat.entry_count(), 2, "{shown}");
                assert_eq!(app.status, Status::Compacting, "{shown}");
                let text = rendered_text(&mut app, 80, 10);
                assert!(text.contains("auto summary"), "{text}");
                assert!(
                    text.contains(shown) && !text.contains(hidden),
                    "the replay shows {shown}: {text}"
                );
            }
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
                Some(Op::prompt("queued before compact")),
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
                prior: ArchiveState::Active,
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
                old_session: ArchiveState::Archived,
            });
            let at_roll = app.status.clone();
            app.handle_engine_event(Event::SessionReady);

            assert_eq!(during, Err(TryRecvError::Empty));
            assert_eq!(at_roll, Status::Archiving);
            assert_eq!(rx.recv().await, Some(Op::prompt("next")));
            assert_eq!(app.status, Status::Streaming);
        }

        #[test]
        fn config_changed_model_swap_refreshes_status_and_chat() {
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
            assert!(
                text.lines().next().unwrap().contains(" │ plan │ "),
                "{text}"
            );
        }

        #[test]
        fn login_started_opens_the_browser_and_prints_the_manual_url() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let started = || Event::LoginStarted {
                url: "https://example.com/authorize".to_owned(),
                manual_url: "https://example.com/authorize?manual".to_owned(),
            };
            app.open_browser = |url| {
                assert_eq!(url, "https://example.com/authorize");
                Ok(())
            };

            app.handle_engine_event(started());

            assert_eq!(
                app.chat.last_system_text(),
                Some(indoc! {"
                    Opened the browser to sign in. If it did not open or cannot reach this machine, sign in at this URL and paste the code it shows here:
                    https://example.com/authorize?manual"
                }),
            );

            app.open_browser = |_| Err(io::Error::other("no browser"));
            app.handle_engine_event(started());

            assert_eq!(
                app.chat.last_system_text(),
                Some(indoc! {"
                    Could not open the browser. Sign in at this URL and paste the code it shows here:
                    https://example.com/authorize?manual"
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
                    AuthSource::ClientBearer,
                    "Signed in. \
                     Auth token (client.auth) takes precedence, so requests keep using it. \
                     Remove `client.auth` and restart ox to use the new login.",
                ),
                (
                    AuthSource::ClientApiKey,
                    "Signed in. \
                     API key (client.auth) takes precedence, so requests keep using it. \
                     Remove `client.auth` and restart ox to use the new login.",
                ),
                (
                    AuthSource::ClientOAuth,
                    "Signed in. \
                     OAuth (client.auth) takes precedence, so requests keep using it. \
                     Remove `client.auth` and restart ox to use the new login.",
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
            app.queue.push("held".to_owned());

            app.handle_engine_event(Event::SignedIn {
                auth: AuthSource::OxLogin,
            });

            assert_eq!(rx.try_recv().ok(), Some(Op::prompt("held")));
            assert_eq!(app.status, Status::Streaming);
        }

        #[tokio::test]
        async fn signed_in_after_a_lost_cancel_keeps_the_held_prompt_queued() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            app.dispatch_op(Op::Login);
            rx.try_recv().expect("login forwarded");
            app.queue.push("held".to_owned());
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
            assert_eq!(rx.recv().await, Some(Op::prompt("next")));
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
            assert!(text.contains("A to allow for the session"), "{text}");
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

    mod apply_archive_updated {
        use super::*;

        #[test]
        fn successful_completion_keeps_held_input_until_the_next_picker_action() {
            let archive_key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
            for (next_key, next_status) in [
                (archive_key, Status::Archiving),
                (KeyEvent::from(KeyCode::Enter), Status::Resuming),
            ] {
                let (_dir, store) = isolated_store();
                for (id, second) in [("first", 0), ("second", 1)] {
                    seed_test_session(
                        &store,
                        id,
                        Some(id),
                        Some(1),
                        time::macros::datetime!(2026-04-18 09:00:00 UTC)
                            + time::Duration::seconds(second),
                    );
                }
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.queue.push("held prompt".to_owned());
                app.set_status(Status::Cancelling);
                app.handle_engine_event(Event::Cancelled);
                let picker = ResumePicker::loaded(store, app.session_info.session_id.clone());
                app.push_modal(Box::new(picker));

                app.handle_crossterm_event(&CrosstermEvent::Key(archive_key));

                let target = SavedTarget::Id("second".to_owned());
                assert_eq!(
                    rx.try_recv(),
                    Ok(Op::SetArchive {
                        target: target.clone(),
                        state: ArchiveState::Archived,
                    })
                );

                app.handle_engine_event(Event::ArchiveUpdated {
                    target,
                    state: ArchiveState::Archived,
                    result: Ok(ArchiveUpdate {
                        id: "second".to_owned(),
                        changed: true,
                    }),
                });

                assert_eq!(app.status, Status::Idle, "{next_key:?}");
                assert_eq!(*app.queue.pending(), ["held prompt"], "{next_key:?}");
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{next_key:?}");

                app.handle_crossterm_event(&CrosstermEvent::Key(next_key));

                let expected = if next_key == archive_key {
                    Op::SetArchive {
                        target: SavedTarget::Id("first".to_owned()),
                        state: ArchiveState::Archived,
                    }
                } else {
                    Op::Resume {
                        target: ResumeTarget::Id("first".to_owned()),
                    }
                };
                assert_eq!(rx.try_recv(), Ok(expected), "{next_key:?}");
                assert_eq!(app.status, next_status, "{next_key:?}");
                assert_eq!(*app.queue.pending(), ["held prompt"], "{next_key:?}");
                assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "{next_key:?}");
            }
        }

        #[test]
        fn completion_keeps_queued_input_for_the_user() {
            let update = || {
                Ok(ArchiveUpdate {
                    id: "saved".to_owned(),
                    changed: true,
                })
            };
            for (status, result) in [
                (Status::Archiving, update()),
                (Status::Cancelling, update()),
                (
                    Status::Archiving,
                    Err(SavedError::Failed {
                        action: SavedAction::Archive,
                        id: "saved".to_owned(),
                        cause: "disk full".to_owned(),
                    }),
                ),
            ] {
                let (mut app, mut rx, _event_tx) = test_app(None);
                app.queue.push("next prompt".to_owned());
                app.set_status(status);

                app.apply_archive_updated(
                    &SavedTarget::Prefix("saved".to_owned()),
                    ArchiveState::Archived,
                    &result,
                );

                assert_eq!(app.status, Status::Idle);
                assert_eq!(*app.queue.pending(), ["next prompt"]);
                assert_eq!(
                    rx.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                );
            }
        }
    }

    mod apply_saved_found {
        use super::*;

        #[test]
        fn opens_the_confirm_modal_or_reports_why_no_session_matched() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.set_status(Status::Searching);

            app.handle_engine_event(Event::SavedFound(Ok(Box::new(listed_session("saved")))));

            assert_eq!(app.status, Status::Idle);
            assert!(app.modals.is_active(), "the confirm modal opens");

            app.modals.clear();
            app.set_status(Status::Searching);
            app.handle_engine_event(Event::SavedFound(Err(SavedError::Resolve(
                ResolveError::Live {
                    prefix: "test-".to_owned(),
                },
            ))));

            assert_eq!(app.status, Status::Idle);
            assert!(!app.modals.is_active());
            assert_eq!(
                app.chat.last_error_text(),
                Some(
                    "/delete: Session prefix `test-` matches the live session. \
                     Run `/clear` to start a new one, then delete it."
                )
            );
        }
    }

    mod apply_saved_deleted {
        use super::*;

        #[test]
        fn a_delete_closes_the_confirm_modal_into_a_chat_note_and_a_failure_keeps_it() {
            for (result, closes) in [
                (Ok(()), true),
                (
                    Err(SavedError::Failed {
                        action: SavedAction::Delete,
                        id: "saved".to_owned(),
                        cause: "disk full".to_owned(),
                    }),
                    false,
                ),
            ] {
                let (mut app, _rx, _event_tx) = test_app(None);
                let info = SessionInfo {
                    title: Some(TitleInfo {
                        title: "Old work".to_owned(),
                        updated_at: time::macros::datetime!(2026-04-18 09:00:00 UTC),
                    }),
                    ..listed_session("saved")
                };
                app.handle_engine_event(Event::SavedFound(Ok(Box::new(info))));
                app.set_status(Status::Deleting);

                app.handle_engine_event(Event::SavedDeleted {
                    id: "saved".to_owned(),
                    result: result.clone(),
                });

                assert_eq!(app.status, Status::Idle, "{result:?}");
                assert_eq!(app.modals.is_active(), !closes, "{result:?}");
                let note = closes.then_some("Deleted session saved: Old work");
                assert_eq!(app.chat.last_system_text(), note, "{result:?}");
            }
        }

        #[test]
        fn reports_the_result_in_chat_once_the_confirm_modal_is_gone() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let id = stamped_id(0xab);
            app.set_status(Status::Deleting);

            app.handle_engine_event(Event::SavedDeleted {
                id: id.clone(),
                result: Ok(()),
            });

            assert_eq!(app.status, Status::Idle);
            assert_eq!(
                app.chat.last_system_text(),
                Some("Deleted session abababab")
            );

            app.set_status(Status::Deleting);
            app.handle_engine_event(Event::SavedDeleted {
                id,
                result: Err(SavedError::Live {
                    id: "abababab".to_owned(),
                }),
            });

            assert_eq!(app.status, Status::Idle);
            assert_eq!(
                app.chat.last_error_text(),
                Some(
                    "Session abababab is the live session. \
                     Run `/clear` to start a new one, then delete it."
                )
            );
        }
    }

    mod finish_turn {
        use super::*;

        #[test]
        fn evicts_orphaned_pending_calls() {
            // Turn boundaries discard calls whose results never arrived, preventing stale IDs from
            // accumulating.
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

        fn app_showing_pull_request_7() -> App {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.git_probes = crate::git_probes::GitProbes::new(
                PathBuf::from("/repo"),
                &[StatusLineSegment::PullRequest],
                now(),
            );
            app.status_bar.set_git_branch(Some("main".to_owned()));
            app.apply_git_update(GitUpdate::PullRequest(Some(pull_request(7))));
            app
        }

        #[test]
        fn marks_dirty_only_when_the_status_bar_changes() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dirty = false;

            app.apply_git_update(GitUpdate::Branch(Some("main".to_owned())));

            assert!(!app.dirty, "unchanged branch skips the repaint");

            app.apply_git_update(GitUpdate::PullRequest(Some(pull_request(7))));

            assert!(app.dirty, "new pull request repaints");
        }

        #[test]
        fn a_branch_change_clears_the_pull_request_and_probes_it_again() {
            let mut app = app_showing_pull_request_7();
            app.dirty = false;

            let before = now();
            app.apply_git_update(GitUpdate::Branch(Some("feature".to_owned())));
            let after = now();

            assert!(app.dirty, "the branch change repaints");
            assert_eq!(app.status_bar.git_branch(), Some("feature"));
            assert_eq!(app.status_bar.pull_request(), None);
            assert!(app.git_probes.pull_request_stale());
            let due = app
                .git_probes
                .pull_request_due()
                .expect("the roster reads the pull request");
            assert!(
                before <= due && due <= after,
                "an idle probe is due at once"
            );
        }

        #[test]
        fn an_unchanged_branch_keeps_the_pull_request() {
            let mut app = app_showing_pull_request_7();

            app.apply_git_update(GitUpdate::Branch(Some("main".to_owned())));

            assert_eq!(app.status_bar.pull_request().map(|pr| pr.number), Some(7));
            assert!(!app.git_probes.pull_request_stale());
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

        async fn finish(outcome: Result<SlashOutcome, String>) -> Result<SlashTask, JoinError> {
            tokio::spawn(async move {
                SlashTask {
                    line: "/diff".to_owned(),
                    outcome,
                }
            })
            .await
        }

        #[tokio::test]
        async fn applies_the_message_diff_or_error_the_task_resolved_to() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let diff = WorkingTreeDiff {
                untracked: vec!["new.txt".to_owned()],
                ..WorkingTreeDiff::default()
            };

            app.apply_slash_task(finish(Ok(SlashOutcome::SystemMessage("clean".to_owned()))).await);

            assert_eq!(app.chat.last_system_text(), Some("clean"));

            app.dirty = false;
            app.apply_slash_task(finish(Ok(SlashOutcome::GitDiff(diff))).await);

            assert!(app.dirty);
            assert!(rendered_text(&mut app, 60, 24).contains("  new.txt"));

            app.apply_slash_task(finish(Err("/diff: boom".to_owned())).await);

            assert_eq!(app.chat.last_error_text(), Some("/diff: boom"));
        }

        #[tokio::test]
        async fn a_panicked_task_prints_nothing() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let mut tasks = JoinSet::<SlashTask>::new();
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
            let expired_early = app.expire_copy_notice(now());

            assert!(!expired_early);
            assert!(app.copy_notice.is_some());

            tokio::time::advance(Duration::from_millis(1)).await;
            let expired = app.expire_copy_notice(now());

            assert!(expired);
            assert!(app.copy_notice.is_none());
            assert!(!app.expire_copy_notice(now()), "nothing left to clear");
        }
    }
}
