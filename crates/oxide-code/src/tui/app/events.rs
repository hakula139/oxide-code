//! Engine and git probe events: chat and status updates, the turn lifecycle they drive, session
//! swaps, config changes, and sign-in results.

use indoc::formatdoc;

use super::App;
use crate::config::auth::AuthSource;
use crate::config::compaction::AutoCompactionConfig;
use crate::config::{Effort, display_auto_compaction};
use crate::model::ModelSpec;
use crate::permission::SessionGrant;
use crate::protocol::{Event, Interaction, Op};
use crate::session::display::archive_notice;
use crate::slash;
use crate::tool::ApprovalPreview;
use crate::tui::components::status::Status;
use crate::tui::git_probes::GitUpdate;
use crate::tui::modal::approval::ApprovalModal;

impl App {
    pub(super) fn handle_engine_event(&mut self, event: Event) {
        match event {
            Event::StreamToken(token) => {
                self.chat.append_stream_token(&token);
                self.set_active_status(Status::Streaming);
                self.input.set_enabled(false);
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
            Event::UsageUpdated(usage) => {
                self.status_bar.set_usage(Some(usage));
            }
            Event::RateLimitsUpdated(limits) => self.status_bar.set_rate_limits(Some(limits)),
            Event::TurnComplete => {
                self.finish_turn();
            }
            Event::Cancelled => {
                self.chat.push_interrupted_marker();
                self.active_prompt = None;
                self.enter_idle();
            }
            Event::AutoCompactionStarted => {
                self.auto_compacting = true;
                self.set_active_status(Status::Compacting);
                self.input.set_enabled(false);
            }
            Event::AutoCompactionPaused { failures } => {
                self.session_info.auto_compaction_paused_after = Some(failures);
            }
            Event::AutoCompactionResumed => self.session_info.auto_compaction_paused_after = None,
            Event::SummaryToken(token) => self.chat.append_summary_token(&token),
            Event::SummaryDiscarded => self.chat.discard_summary(),
            Event::SessionTitleUpdated { session_id, title } => {
                if session_id == self.session_info.session_id {
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
                automatic,
            } => self.apply_session_compacted(
                &summary,
                pre_count,
                instructions.as_deref(),
                automatic,
            ),
            Event::ConfigChanged {
                model,
                effort,
                requested_effort,
                auto_compaction,
            } => self.apply_config_changed(model, effort, requested_effort, auto_compaction),
            Event::LoginStarted { url } => self.open_sign_in_url(&url),
            Event::SignedIn { auth } => self.apply_signed_in(auth),
            Event::SignedOut {
                removed,
                auth,
                fallback_error,
            } => self.apply_signed_out(removed, auth, fallback_error),
            Event::TurnFailed(msg) => {
                self.chat.push_error(&msg);
                if self.auto_compacting {
                    self.return_unsent_prompt();
                } else {
                    self.finish_turn();
                }
            }
            Event::Error(msg) | Event::RenameFailed(msg) => self.chat.push_error(&msg),
            Event::ApprovalRequested { id, preview, grant } => {
                self.push_approval_modal(id, preview, grant);
            }
            Event::SessionRulesUpdated(rules) => self.session_info.session_allow = rules,
            Event::TasksUpdated(tasks) => self.tasks.set(tasks),
        }
        self.dirty = true;
    }

    fn apply_prompt_drained(&mut self, text: String) {
        if self.forwarded_prompts > 0
            && self
                .pending_prompts
                .front()
                .is_some_and(|queued| slash::unescape_literal(queued) == text)
        {
            self.pending_prompts.pop_front();
            self.forwarded_prompts -= 1;
        } else {
            tracing::warn!("drained prompt {text:?} does not lead the forwarded queue");
        }
        self.chat.push_user_message(text);
    }

    /// Opens the approve-or-deny overlay for a gated tool call. Pushing onto the stack lets it nest
    /// over any open picker, and the modal's cancel hook resolves the blocked turn on dismissal.
    pub(super) fn push_approval_modal(
        &mut self,
        id: String,
        preview: ApprovalPreview,
        grant: Option<Box<SessionGrant>>,
    ) {
        self.modals
            .push(Box::new(ApprovalModal::new(id, preview, grant)));
    }

    fn finish_turn(&mut self) {
        self.chat.commit_streaming();
        self.active_prompt = None;
        // A cancel that lost the race to the turn's end, or a failed resume, still leaves the next
        // step to the user.
        if matches!(
            self.status_bar.status(),
            Status::Cancelling | Status::Resuming
        ) {
            self.enter_idle();
        } else {
            self.finalize_idle();
        }
    }

    /// Ends a prompt whose automatic compaction failed before it was sent. Goes idle without
    /// starting the queue, and hands the prompt back: into the empty input when nothing else is
    /// queued, so Enter sends it again, or else to the queue head, so it still runs first.
    fn return_unsent_prompt(&mut self) {
        if let Some(prompt) = self.active_prompt.take() {
            if self.pending_prompts.is_empty() && self.input.is_empty() {
                self.input.set_text(&prompt);
            } else {
                self.pending_prompts.push_front(prompt);
            }
        }
        self.enter_idle();
    }

    /// `/clear` or `/archive`: rebinds the session and empties chat for the fresh one.
    fn apply_session_rolled(&mut self, id: String, archived: bool) {
        let old_id = std::mem::replace(&mut self.session_info.session_id, id);
        self.session_info.session_allow.clear();
        self.tasks.clear();
        self.status_bar.set_title(None);
        self.status_bar.set_usage(None);
        self.chat.clear_history();
        self.active_prompt = None;
        if archived {
            self.chat.push_system_message(archive_notice(&old_id, true));
        }
    }

    /// Mid-session resume: rebinds the session and rebuilds chat from the target transcript.
    fn apply_session_resumed(
        &mut self,
        id: String,
        title: Option<String>,
        unarchived: bool,
        history: Vec<Interaction>,
    ) {
        self.session_info.session_id = id;
        self.session_info.session_allow.clear();
        self.tasks.clear();
        self.status_bar.set_title(title);
        self.status_bar.set_usage(None);
        self.chat.clear_history();
        self.chat.load_history(history);
        if unarchived {
            self.chat
                .push_system_message(archive_notice(&self.session_info.session_id, false));
        }
        if let Some(notice) = self.session_info.config.model.unsupported_notice() {
            self.chat.push_system_message(notice);
        }
        self.active_prompt = None;
        // Queued prompts belonged to the previous thread, so resume drops them.
        let dropped = self.pending_prompts.len();
        self.pending_prompts.clear();
        if dropped > 0 {
            self.chat.push_system_message(format!(
                "{dropped} queued prompt{plural} discarded. Typed for the previous session.",
                plural = if dropped == 1 { "" } else { "s" },
            ));
        }
        self.clear_modals();
        self.finalize_idle();
    }

    /// Repaints chat after `/compact` with one boundary block. Queued prompts survive because the
    /// session identity stays the same.
    fn apply_session_compacted(
        &mut self,
        summary: &str,
        pre_count: u32,
        instructions: Option<&str>,
        automatic: bool,
    ) {
        self.chat.clear_history();
        self.status_bar.set_usage(None);
        self.chat
            .push_compacted_block(pre_count, instructions, summary.to_owned());
        self.auto_compacting = false;
        if automatic && let Some(prompt) = &self.active_prompt {
            self.chat
                .push_user_message(slash::unescape_literal(prompt).to_owned());
        }
        self.clear_modals();
        if !automatic {
            self.active_prompt = None;
            self.finalize_idle();
        }
    }

    fn apply_config_changed(
        &mut self,
        model: ModelSpec,
        effort: Option<Effort>,
        requested_effort: Option<Effort>,
        auto_compaction: AutoCompactionConfig,
    ) {
        let model_changed = model.id() != self.session_info.config.model.id();
        let prev_effort = self.session_info.config.effort;
        let prev_auto_compaction = self.session_info.config.compaction.auto;
        let confirmation = format_config_change(
            &model,
            model_changed,
            prev_effort,
            effort,
            requested_effort,
            prev_auto_compaction,
            auto_compaction,
        );
        if model_changed {
            self.status_bar
                .set_model(model.short_display_name().to_owned());
        }
        self.status_bar.set_effort(effort);
        self.session_info
            .config
            .apply_swap(model, effort, requested_effort, auto_compaction);
        self.chat.push_system_message(confirmation);
    }

    /// Opens `url` in the browser and prints it in case the browser does not open. The sign-in
    /// returns to a loopback listener, so only a browser on this machine can complete it.
    fn open_sign_in_url(&mut self, url: &str) {
        let message = match (self.open_browser)(url) {
            Ok(()) => formatdoc! {"
            Opened the browser to sign in. If it did not open, visit this URL in a browser on \
                this machine:
                {url}"},
            Err(e) => {
                tracing::debug!("failed to open the browser: {e}");
                formatdoc! {"
                    Could not open the browser. Sign in at this URL in a browser on this machine:
                    {url}"}
            }
        };
        self.chat.push_system_message(message);
    }

    fn apply_signed_in(&mut self, auth: AuthSource) {
        self.session_info.config.auth = Some(auth);
        self.status_bar.set_rate_limits(None);
        let message = if auth == AuthSource::OxLogin {
            format!("Signed in. Requests now use {auth}.")
        } else {
            format!("Signed in. {auth} takes precedence, so requests keep using it.")
        };
        self.chat.push_system_message(message);
        self.finish_turn();
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
            (None, None) => "No credential is active. Run /login to sign in.".to_owned(),
        };
        self.chat.push_system_message(format!("{outcome} {active}"));
    }

    /// Goes idle, then submits the next queued prompt.
    fn finalize_idle(&mut self) {
        self.enter_idle();
        self.drain_pending_prompt();
    }

    /// Resets to idle, clears orphan calls, and re-enables input. Queued prompts stay queued, and
    /// the ended turn no longer holds any of them.
    fn enter_idle(&mut self) {
        self.auto_compacting = false;
        self.chat.clear_pending_calls();
        self.forwarded_prompts = 0;
        self.status_bar.set_status(Status::Idle);
        self.input.set_enabled(true);
    }

    /// Starts a turn with the oldest queued prompt.
    pub(super) fn drain_pending_prompt(&mut self) {
        if let Some(queued) = self.pending_prompts.pop_front() {
            let prompt = slash::unescape_literal(&queued).to_owned();
            self.chat.push_user_message(prompt.clone());
            self.active_prompt = Some(queued);
            self.start_turn(Op::Prompt(prompt));
        }
    }

    /// Sets busy status unless a user-acknowledgement status is showing. Leaving `Compacting`
    /// releases the prompts held during an automatic compaction.
    fn set_active_status(&mut self, status: Status) {
        match self.status_bar.status() {
            Status::Cancelling | Status::ExitArmed { .. } => {}
            Status::Compacting => {
                self.status_bar.set_status(status);
                self.forward_queued_prompts();
            }
            _ => self.status_bar.set_status(status),
        }
    }

    pub(super) fn apply_git_update(&mut self, update: GitUpdate) {
        let changed = match update {
            GitUpdate::Branch(branch) => self.status_bar.set_git_branch(branch),
            GitUpdate::PullRequest(pull_request) => self.status_bar.set_pull_request(pull_request),
        };
        if changed {
            self.dirty = true;
        }
    }
}

/// `Event::ConfigChanged` confirmation, surfacing silent effort shifts.
fn format_config_change(
    model: &ModelSpec,
    model_changed: bool,
    prev_effort: Option<Effort>,
    new_effort: Option<Effort>,
    requested_effort: Option<Effort>,
    prev_auto_compaction: AutoCompactionConfig,
    new_auto_compaction: AutoCompactionConfig,
) -> String {
    let message = if model_changed {
        let head = format!("Switched to {} ({})", model.display_name(), model.id());
        match (requested_effort, prev_effort, new_effort) {
            (Some(req), _, None) => format!("{head}. {}", req.unapplied_notice()),
            (None, None, None) => format!("{head}."),
            (None, Some(_), None) => format!("{head}. Effort cleared (model has no effort tier)."),
            (None, None, Some(eff)) => format!("{head} · effort {eff} (model default)."),
            (_, _, Some(eff)) => format!("{head} · effort {eff}."),
        }
    } else {
        match (requested_effort, new_effort) {
            (Some(req), None) => req.unapplied_notice(),
            (_, Some(eff)) => format!("Effort set to {eff}."),
            // Slash dispatch keeps this unreachable, but a clear fallback beats a panic.
            (None, None) => "Config unchanged.".to_owned(),
        }
    };
    if model_changed && prev_auto_compaction != new_auto_compaction {
        return append_sentence(
            message,
            &format!(
                "Auto compaction {}",
                display_auto_compaction(new_auto_compaction)
            ),
        );
    }
    message
}

fn append_sentence(mut message: String, sentence: &str) -> String {
    if message.ends_with('.') {
        message.pop();
    }
    format!("{message}. {sentence}.")
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::Path;

    use super::*;
    use crate::config::compaction::test_thresholds;
    use crate::permission::rule::Rule;
    use crate::permission::{Access, PermissionClass, Policy, Workspace};
    use crate::protocol::{ResumeTarget, ToolCall, ToolResult, UsageSnapshot};
    use crate::tool::tasks::TaskStatus;
    use crate::tool::tasks::testing::{plan, task};
    use crate::tool::{ApprovalBody, ToolResultView};
    use crate::tui::action::Action;
    use crate::tui::app::testing::{five_hour_limit, rendered_text, test_app};
    use crate::tui::glyphs::INTERRUPTED_MARKER;

    fn bash_call_start(id: &str, command: &str) -> Event {
        Event::ToolCallStart(ToolCall {
            id: id.to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": command}),
            icon: "$".to_owned(),
            label: command.to_owned(),
        })
    }

    fn text_call_end(id: &str, content: &str, is_error: bool, title: Option<&str>) -> Event {
        Event::ToolCallEnd(ToolResult {
            id: id.to_owned(),
            content: content.to_owned(),
            is_error,
            title: title.map(ToOwned::to_owned),
            view: ToolResultView::Text {
                content: content.to_owned(),
            },
        })
    }

    fn spec(id: &str) -> ModelSpec {
        ModelSpec::resolve(id.to_owned())
    }

    fn base_auto_compaction() -> AutoCompactionConfig {
        AutoCompactionConfig {
            enabled: true,
            threshold_tokens: Some(155_000),
        }
    }

    fn usage_snapshot() -> UsageSnapshot {
        UsageSnapshot {
            context_tokens: Some(124_000),
            context_window: Some(1_000_000),
            estimated_cost_usd: Some(0.4321),
        }
    }

    // ── handle_engine_event ──

    #[test]
    fn handle_session_title_updated_overwrites_existing_title_for_current_session() {
        let (mut app, _rx, _event_tx) = test_app(Some("First prompt"));
        app.handle_engine_event(Event::SessionTitleUpdated {
            session_id: app.session_info.session_id.clone(),
            title: "AI-generated".to_owned(),
        });
        assert_eq!(app.status_bar.title(), Some("AI-generated"));
        assert!(app.dirty);
    }

    #[test]
    fn handle_session_title_updated_drops_event_for_stale_session_id() {
        // Title task spawned before `/clear` finishes after the roll;
        // its event must not paint the old session's title onto the current one.
        let (mut app, _rx, _event_tx) = test_app(Some("First prompt"));
        app.handle_engine_event(Event::SessionTitleUpdated {
            session_id: "different-session".to_owned(),
            title: "Stale title".to_owned(),
        });
        assert_eq!(
            app.status_bar.title(),
            Some("First prompt"),
            "current-session title must survive a stale event",
        );
    }

    #[test]
    fn handle_rename_failed_shows_the_error_and_keeps_the_title() {
        let (mut app, _rx, _event_tx) = test_app(Some("First prompt"));

        app.handle_engine_event(Event::RenameFailed(
            "Session write failed: disk full".to_owned(),
        ));

        assert!(app.chat.last_is_error());
        assert_eq!(app.status_bar.title(), Some("First prompt"));
        assert!(app.input.is_enabled());
    }

    #[test]
    fn handle_config_changed_model_swap_refreshes_status_and_chat() {
        // Three surfaces refresh in one shot: status-bar label, `session_info` (backs `/status` /
        // `/config`), and a chat confirmation block.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::ConfigChanged {
            model: spec("claude-sonnet-5-5"),
            effort: Some(Effort::High),
            requested_effort: None,
            auto_compaction: AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(test_thresholds::WINDOW_1M),
            },
        });

        assert_eq!(app.session_info.config.model.id(), "claude-sonnet-5-5");
        assert_eq!(app.session_info.config.effort, Some(Effort::High),);
        assert_eq!(app.status_bar.model(), "Sonnet 5.5");
        assert_eq!(
            app.session_info.config.compaction.auto.threshold_tokens,
            Some(test_thresholds::WINDOW_1M),
        );
        let body = app.chat.last_system_text().expect("confirmation block");
        assert_eq!(
            body,
            format!(
                "Switched to Claude Sonnet 5.5 (claude-sonnet-5-5) · effort high. \
                 Auto compaction at {} tokens.",
                test_thresholds::WINDOW_1M,
            ),
        );
        assert!(app.dirty);
    }

    #[test]
    fn handle_config_changed_effort_only_keeps_status_bar_model_label() {
        // Effort-only swap leaves the cached model label alone; only
        // the snapshot effort and chat confirmation update.
        let (mut app, _rx, _event_tx) = test_app(None);
        let original_model = app.status_bar.model().to_owned();
        app.handle_engine_event(Event::ConfigChanged {
            model: app.session_info.config.model.clone(),
            effort: Some(Effort::Xhigh),
            requested_effort: Some(Effort::Xhigh),
            auto_compaction: app.session_info.config.compaction.auto,
        });
        assert_eq!(app.session_info.config.effort, Some(Effort::Xhigh),);
        assert_eq!(app.status_bar.model(), original_model);
        let body = app.chat.last_system_text().expect("confirmation block");
        assert_eq!(body, "Effort set to xhigh.");
        assert!(app.dirty);
    }

    #[test]
    fn handle_login_started_opens_the_browser_and_prints_the_url() {
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
            Some(indoc::indoc! {"
            Opened the browser to sign in. If it did not open, visit this URL in a browser on \
                this machine:
                https://example.com/authorize"}),
        );

        app.open_browser = |_| Err(io::Error::other("no browser"));
        app.handle_engine_event(started());
        assert_eq!(
            app.chat.last_system_text(),
            Some(indoc::indoc! {"
                Could not open the browser. Sign in at this URL in a browser on this machine:
                https://example.com/authorize"}),
        );
    }

    #[test]
    fn handle_signed_in_names_the_credential_requests_use() {
        for (auth, expected) in [
            (
                AuthSource::OxLogin,
                "Signed in. Requests now use OAuth (oxide-code login).",
            ),
            (
                AuthSource::ApiKeyEnv,
                "Signed in. API key (ANTHROPIC_API_KEY) takes precedence, so requests keep using \
                 it.",
            ),
        ] {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.session_info.config.auth = None;
            app.status_bar.set_status(Status::SigningIn);

            app.handle_engine_event(Event::SignedIn { auth });

            assert_eq!(app.session_info.config.auth, Some(auth));
            assert_eq!(app.chat.last_system_text(), Some(expected));
            assert_eq!(app.status_bar.status(), &Status::Idle);
            assert!(app.input.is_enabled());
        }
    }

    #[tokio::test]
    async fn handle_signed_in_runs_a_prompt_held_while_signing_in() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_op(Op::Login);
        rx.recv().await.expect("login forwarded");
        app.dispatch_action(Action::Submit("held".to_owned()));

        app.handle_engine_event(Event::SignedIn {
            auth: AuthSource::OxLogin,
        });

        assert_eq!(rx.recv().await, Some(Op::Prompt("held".to_owned())));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
    }

    #[tokio::test]
    async fn handle_signed_in_after_a_lost_cancel_keeps_the_held_prompt_queued() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_op(Op::Login);
        rx.recv().await.expect("login forwarded");
        app.dispatch_action(Action::Submit("held".to_owned()));
        app.status_bar.set_status(Status::Cancelling);

        app.handle_engine_event(Event::SignedIn {
            auth: AuthSource::OxLogin,
        });

        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert_eq!(app.pending_prompts, ["held"]);
        assert!(rx.try_recv().is_err(), "the held prompt must not run");
    }

    #[test]
    fn handle_signed_out_reports_the_removal_and_the_credential_requests_use() {
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
                "Signed out of oxide-code. No credential is active. Run /login to sign in.",
            ),
            (
                true,
                None,
                Some("Claude Code login has expired. Run `claude` to sign in again."),
                "Signed out of oxide-code. No credential is active: Claude Code login has expired. \
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

    #[test]
    fn handle_signed_in_or_out_drops_the_previous_accounts_usage_limits() {
        for event in [
            Event::SignedIn {
                auth: AuthSource::OxLogin,
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
    fn handle_usage_updated_sets_status_bar_usage() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let usage = usage_snapshot();

        app.handle_engine_event(Event::UsageUpdated(usage));

        assert_eq!(app.status_bar.usage(), Some(usage));
        assert!(app.dirty);
    }

    #[test]
    fn handle_engine_event_approval_with_a_grant_offers_allow_for_session() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let grant = Policy::default()
            .session_grant(
                PermissionClass::Bash,
                &Access::for_command("cargo test", &Workspace::new(Path::new("/repo"))),
            )
            .unwrap();
        app.handle_engine_event(Event::ApprovalRequested {
            id: "call-1".to_owned(),
            preview: ApprovalPreview {
                title: "Bash".to_owned(),
                body: ApprovalBody::Command("cargo test".to_owned()),
            },
            grant: Some(Box::new(grant)),
        });

        let text = rendered_text(&mut app, 100, 24);
        assert!(text.contains("[A] allow for session"), "{text}");
    }

    #[test]
    fn handle_session_rules_updated_replaces_the_rules_status_lists() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let rules = ["bash(cargo test:*)", "edit(out/**)"].map(|text| Rule::parse(text).unwrap());

        app.handle_engine_event(Event::SessionRulesUpdated(vec![rules[0].clone()]));
        app.handle_engine_event(Event::SessionRulesUpdated(rules.to_vec()));

        assert_eq!(app.session_info.session_allow, rules);
        assert!(app.dirty);
    }

    #[test]
    fn handle_tasks_updated_replaces_the_panel_list() {
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
    fn handle_session_rolled_clears_chat_rebinds_id_and_drops_stale_title() {
        let (mut app, _rx, _event_tx) = test_app(Some("Old session title"));
        app.chat.push_user_message("old prompt".to_owned());
        let original_id = app.session_info.session_id.clone();
        app.status_bar.set_usage(Some(usage_snapshot()));
        app.session_info.session_allow = vec![Rule::parse("bash(ls)").unwrap()];
        app.tasks.set(plan());

        app.handle_engine_event(Event::SessionRolled {
            id: "rolled-session".to_owned(),
            archived: false,
        });

        assert_eq!(
            app.session_info.session_id, "rolled-session",
            "session id must rebind to the rolled session",
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
        assert_eq!(app.tasks.tasks(), []);
        assert!(
            app.chat.is_empty(),
            "clear must drain the chat so the welcome can repaint",
        );
        assert!(app.dirty);
    }

    #[test]
    fn handle_session_rolled_after_archive_names_the_archived_session() {
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
    fn handle_session_resumed_replays_transcript_and_clears_pending() {
        let (mut app, _rx, _event_tx) = test_app(Some("Old"));
        app.session_info.config.model = spec("claude-opus-5-5");
        app.chat.push_user_message("live prompt".to_owned());
        app.status_bar.set_usage(Some(usage_snapshot()));
        app.pending_prompts.push_back("queued".to_owned());
        app.handle_engine_event(bash_call_start("pending-1", "ls"));
        app.session_info.session_allow = vec![Rule::parse("bash(ls)").unwrap()];
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
        assert_eq!(app.tasks.tasks(), []);
        assert_eq!(
            app.chat.entry_count(),
            3,
            "chat must reflect the resumed transcript + the queued-prompt-discarded notice",
        );
        assert_eq!(
            app.chat.pending_call_count(),
            0,
            "pending tool calls must drop on resume",
        );
        assert!(
            app.pending_prompts.is_empty(),
            "queued prompts must drop on resume",
        );
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled(), "resume returns to idle input");
        assert!(app.dirty);
    }

    #[test]
    fn handle_session_resumed_with_no_queued_prompts_is_silent() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.session_info.config.model = spec("claude-opus-5-5");
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
    fn handle_session_resumed_after_unarchiving_appends_a_notice() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.session_info.config.model = spec("claude-opus-5-5");
        app.handle_engine_event(Event::SessionResumed {
            id: "bbbb2222-resumed".to_owned(),
            title: None,
            unarchived: true,
            history: vec![Interaction::UserText("only msg".to_owned())],
        });

        assert_eq!(app.chat.entry_count(), 2);
        let text = rendered_text(&mut app, 80, 12);
        assert!(text.contains("Unarchived session bbbb2222."), "{text}");
    }

    #[test]
    fn handle_session_resumed_with_compact_replays_boundary_block() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.session_info.config.model = spec("claude-opus-5-5");
        app.handle_engine_event(Event::SessionResumed {
            id: "resumed".to_owned(),
            title: None,
            unarchived: false,
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
    fn handle_session_resumed_with_no_title_clears_stale_chrome() {
        let (mut app, _rx, _event_tx) = test_app(Some("Stale"));
        app.session_info.config.model = spec("claude-opus-5-5");
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
    fn handle_session_resumed_with_an_unsupported_model_repeats_its_notice() {
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
    fn handle_session_compacted_replays_summary_and_clears_pending_calls() {
        let (mut app, _rx, _event_tx) = test_app(Some("Pre-compact"));
        app.chat.push_user_message("pre-compact prompt".to_owned());
        app.status_bar.set_usage(Some(usage_snapshot()));
        app.handle_engine_event(bash_call_start("pending-1", "ls"));
        app.tasks.set(plan());

        app.handle_engine_event(Event::SessionCompacted {
            summary: "## Recap\n\nDid the thing.".to_owned(),
            pre_count: 4,
            instructions: Some("focus on auth".to_owned()),
            automatic: false,
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
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled(), "compact returns to idle input");
        assert!(app.dirty);
    }

    #[tokio::test]
    async fn handle_session_compacted_drains_queued_prompts_unlike_resume() {
        // Compact preserves user intent: a queued prompt becomes the first post-compact turn
        // rather than being dropped (the way `/resume` does, since `/resume` swaps identity).
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.pending_prompts
            .push_back("queued after compact".to_owned());

        app.handle_engine_event(Event::SessionCompacted {
            summary: "s".to_owned(),
            pre_count: 2,
            instructions: None,
            automatic: false,
        });

        let forwarded = rx.recv().await.expect("drained prompt reaches the engine");
        assert_eq!(
            forwarded,
            Op::Prompt("queued after compact".to_owned()),
            "queued prompt must drain as the next user turn",
        );
        assert!(app.pending_prompts.is_empty());
    }

    #[test]
    fn handle_session_compacted_without_instructions_renders_clean_block() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::SessionCompacted {
            summary: "summary only".to_owned(),
            pre_count: 2,
            instructions: None,
            automatic: false,
        });
        assert_eq!(app.chat.entry_count(), 1, "exactly one boundary block");
    }

    #[tokio::test]
    async fn handle_session_compacted_automatic_keeps_busy_state_and_queued_prompts() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.input.set_enabled(false);
        app.status_bar.set_status(Status::Compacting);
        app.pending_prompts
            .push_back("queued while busy".to_owned());

        app.handle_engine_event(Event::SessionCompacted {
            summary: "auto summary".to_owned(),
            pre_count: 4,
            instructions: None,
            automatic: true,
        });

        assert_eq!(app.chat.entry_count(), 1);
        assert_eq!(app.status_bar.status(), &Status::Compacting);
        assert!(!app.input.is_enabled());
        assert_eq!(app.pending_prompts.len(), 1);
        assert!(
            rx.try_recv().is_err(),
            "automatic compact must not drain early"
        );
    }

    #[test]
    fn handle_auto_compaction_started_sets_compacting_status() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active question".to_owned()));

        app.handle_engine_event(Event::AutoCompactionStarted);

        assert_eq!(app.status_bar.status(), &Status::Compacting);
        assert!(!app.input.is_enabled());
    }

    #[test]
    fn handle_auto_compaction_paused_until_resumed_marks_the_status_pause() {
        let (mut app, _rx, _event_tx) = test_app(None);

        app.handle_engine_event(Event::AutoCompactionPaused { failures: 3 });
        let paused = app.session_info.auto_compaction_paused_after;
        app.handle_engine_event(Event::AutoCompactionResumed);

        assert_eq!(paused, Some(3));
        assert_eq!(app.session_info.auto_compaction_paused_after, None);
    }

    #[tokio::test]
    async fn handle_session_compacted_automatic_replays_active_prompt_after_summary() {
        let (mut app, mut rx, _event_tx) = test_app(None);

        app.dispatch_action(Action::Submit("//etc/hosts".to_owned()));
        let forwarded = rx.recv().await.expect("prompt reaches the engine");
        assert_eq!(forwarded, Op::Prompt("/etc/hosts".to_owned()));
        app.handle_engine_event(Event::AutoCompactionStarted);

        app.handle_engine_event(Event::SessionCompacted {
            summary: "auto summary".to_owned(),
            pre_count: 4,
            instructions: None,
            automatic: true,
        });

        assert_eq!(app.chat.entry_count(), 2);
        assert_eq!(app.status_bar.status(), &Status::Compacting);
        assert!(!app.input.is_enabled());
        let text = rendered_text(&mut app, 80, 10);
        assert!(text.contains("auto summary"));
        assert!(
            text.contains("/etc/hosts") && !text.contains("//etc"),
            "the replay shows the literal text: {text}"
        );
    }

    #[test]
    fn handle_stream_token_switches_to_streaming_and_disables_input() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::StreamToken("partial".to_owned()));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.input.is_enabled());
    }

    #[test]
    fn handle_thinking_token_routes_to_chat_and_marks_streaming() {
        // Thinking tokens land in the chat view as a separate block
        // (not interleaved with assistant text) and must flip the bar
        // to Streaming so the user sees the engine is working even before any visible text arrives.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::ThinkingToken("planning...".to_owned()));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        // Unlike StreamToken, thinking does not disable input on its own, since the matching
        // prompt already did that.
    }

    #[test]
    fn handle_progress_update_renders_note_in_chat() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(Event::ProgressUpdate("Running the suite.".to_owned()));
        assert!(rendered_text(&mut app, 80, 24).contains("Running the suite."));
    }

    #[test]
    fn handle_tool_call_start_switches_to_tool_running() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_engine_event(bash_call_start("t1", "ls"));
        assert_eq!(
            app.status_bar.status(),
            &Status::ToolRunning {
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
    fn handle_cancelled_commits_partial_stream_with_marker_and_becomes_idle() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("hi".to_owned()));
        app.handle_engine_event(Event::StreamToken("partial answer".into()));
        let entries_before = app.chat.entry_count();
        assert_eq!(app.status_bar.status(), &Status::Streaming);

        app.handle_engine_event(Event::Cancelled);

        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled());
        // commit_streaming pushes the partial text as an AssistantText
        // block; push_interrupted_marker pushes the marker — two new
        // entries on top of whatever was there.
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
        // After the user dispatches Interrupt, the event channel may still
        // have queued StreamToken / ToolCallStart events the engine emitted
        // before its select arm dropped the turn future. Those buffered
        // events must not flip the bar back to Streaming / ToolRunning —
        // otherwise the cancel acknowledgement flickers off until `Cancelled` finally arrives.
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("hi".to_owned()));
        app.dispatch_action(Action::Op(Op::Interrupt));
        assert_eq!(app.status_bar.status(), &Status::Cancelling);

        app.handle_engine_event(Event::StreamToken("late".into()));
        assert_eq!(app.status_bar.status(), &Status::Cancelling);
        app.handle_engine_event(bash_call_start("t-late", "ls"));
        assert_eq!(app.status_bar.status(), &Status::Cancelling);
    }

    #[test]
    fn handle_tool_call_end_renders_the_view_under_the_tool_title() {
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
    fn handle_tool_call_end_without_title_falls_back_to_call_label() {
        // `title: None` means the tool didn't set a result header —
        // typically a failure path (timeout, invalid input) that
        // aborted before `.with_title(...)`. The result must still be
        // pushed (silent swallow would hide the error body) and the
        // header must render the tool-call label stashed at
        // `ToolCallStart`, not a blank string or the generic `(result)` fallback.
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
        // The result header must be the stashed call label (the bash
        // command). It appears twice in the rendered view — once for
        // the tool call line, once for the result header — which is
        // what we want to confirm: both the call row and the result row carry the same label.
        let text = rendered_text(&mut app, 60, 8);
        let occurrences = text.matches("distinctive_label_xyz").count();
        assert_eq!(
            occurrences, 2,
            "expected call label on both the call and result rows, got {occurrences}:\n{text}",
        );
        assert!(
            !text.contains("(result)"),
            "generic fallback must not leak through when the pending call label is known, got:\n{text}",
        );
    }

    #[test]
    fn handle_tool_call_end_without_start_uses_generic_fallback_header() {
        // An End without a matching Start (an engine bug or a dropped event) must still render so
        // the user sees the output. No pending entry → header falls back to `(result)`.
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
        rx.recv().await.expect("active submit forwarded");
        app.dispatch_action(Action::Submit("queued-a".to_owned()));
        rx.recv().await.expect("queued-a submit forwarded");
        app.dispatch_action(Action::Submit("queued-b".to_owned()));
        rx.recv().await.expect("queued-b submit forwarded");

        let chat_before = app.chat.entry_count();
        app.handle_engine_event(Event::PromptDrained("queued-a".to_owned()));

        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["queued-b".to_owned()],
        );
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
        app.pending_prompts.push_back("held".into());
        let chat_before = app.chat.entry_count();

        app.handle_engine_event(Event::PromptDrained("held".to_owned()));

        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["held".to_owned()],
        );
        assert_eq!(app.chat.entry_count(), chat_before + 1);
    }

    #[tokio::test]
    async fn prompt_held_during_auto_compaction_forwards_ahead_of_later_submits() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("first".to_owned()));
        rx.recv().await.expect("first submit forwarded");
        app.handle_engine_event(Event::AutoCompactionStarted);
        app.dispatch_action(Action::Submit("held".to_owned()));
        assert!(rx.try_recv().is_err(), "held while compacting");

        app.handle_engine_event(Event::StreamToken("x".to_owned()));
        app.dispatch_action(Action::Submit("later".to_owned()));
        app.handle_engine_event(Event::PromptDrained("held".to_owned()));

        let forwarded: Vec<Op> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            forwarded,
            [Op::Queue("held".to_owned()), Op::Queue("later".to_owned())],
        );
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["later".to_owned()],
        );
    }

    #[test]
    fn handle_turn_complete_becomes_idle_and_reenables_input() {
        let (mut app, _rx, _event_tx) = test_app(None);
        // Drive into streaming first so TurnComplete has state to tear down.
        app.dispatch_action(Action::Submit("hi".to_owned()));
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.input.is_enabled());

        app.handle_engine_event(Event::TurnComplete);
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled());
    }

    #[tokio::test]
    async fn turn_complete_starts_a_turn_with_the_queue_head_and_hands_it_the_rest() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.pending_prompts.push_back("a".into());
        app.pending_prompts.push_back("b".into());

        app.handle_engine_event(Event::TurnComplete);

        let forwarded: Vec<Op> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            forwarded,
            [Op::Prompt("a".to_owned()), Op::Queue("b".to_owned())],
        );
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["b".to_owned()],
        );
    }

    #[tokio::test]
    async fn turn_complete_after_a_cancel_request_leaves_the_queue_to_the_user() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.dispatch_action(Action::Submit("queued".to_owned()));
        rx.recv().await.expect("queued submit forwarded");
        app.handle_esc();
        assert_eq!(rx.recv().await, Some(Op::Interrupt));

        app.handle_engine_event(Event::TurnComplete);

        assert!(rx.try_recv().is_err(), "nothing resubmits after a cancel");
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["queued".to_owned()],
        );
    }

    #[test]
    fn handle_cancelled_with_no_stream_still_pushes_marker() {
        // Cancel during a tool call (or before any stream tokens) —
        // commit_streaming is a no-op, but the marker still lands so
        // the user sees where the cancel hit.
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
        rx.recv().await.expect("first submit forwarded");
        app.handle_engine_event(Event::StreamToken("x".to_owned()));
        app.dispatch_action(Action::Submit("second".to_owned()));
        rx.recv().await.expect("second submit forwarded");
        app.handle_esc();
        assert_eq!(rx.recv().await, Some(Op::Interrupt));

        app.handle_engine_event(Event::Cancelled);

        assert!(rx.try_recv().is_err(), "nothing resubmits after a cancel");
        assert_eq!(app.status_bar.status(), &Status::Idle);
        app.handle_esc();
        assert_eq!(app.input.lines(), ["second".to_owned()]);
        assert!(app.pending_prompts.is_empty());
    }

    #[tokio::test]
    async fn failed_resume_goes_idle_and_keeps_the_prompts_a_cancel_held() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("first".to_owned()));
        rx.recv().await.expect("first submit forwarded");
        app.handle_engine_event(Event::StreamToken("x".to_owned()));
        app.dispatch_action(Action::Submit("second".to_owned()));
        rx.recv().await.expect("second submit forwarded");
        app.handle_esc();
        assert_eq!(rx.recv().await, Some(Op::Interrupt));
        app.handle_engine_event(Event::Cancelled);
        app.dispatch_action(Action::Submit("/resume zzzz".to_owned()));
        assert_eq!(
            rx.recv().await,
            Some(Op::Resume {
                target: ResumeTarget::Prefix("zzzz".to_owned()),
            }),
        );
        app.dispatch_action(Action::Submit("third".to_owned()));

        app.handle_engine_event(Event::TurnFailed(
            "Resume failed (still on session x): no session matching `zzzz`".to_owned(),
        ));

        assert!(rx.try_recv().is_err(), "nothing runs after a failed resume");
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled());
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["second".to_owned(), "third".to_owned()],
        );
    }

    #[tokio::test]
    async fn handle_retrying_and_error_notices_keep_the_turn_running() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.dispatch_action(Action::Submit("queued".to_owned()));
        rx.recv().await.expect("queued submit forwarded");

        app.handle_engine_event(Event::Retrying(
            "HTTP 529 overloaded_error. Retrying in 1s (retry 1 of 5).".to_owned(),
        ));
        assert!(!app.chat.last_is_error(), "a retry is not an error block");
        app.handle_engine_event(Event::Error("Session write failed: disk full".to_owned()));

        assert!(app.chat.last_is_error(), "error entry appended");
        assert_eq!(app.status_bar.status(), &Status::Streaming);
        assert!(!app.input.is_enabled());
        assert!(rx.try_recv().is_err(), "nothing resubmits mid-turn");
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["queued".to_owned()],
        );
    }

    #[test]
    fn handle_turn_failed_shows_the_error_and_finishes_the_turn() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("boom".to_owned()));
        app.handle_engine_event(Event::TurnFailed("API blew up".to_owned()));

        assert!(app.chat.last_is_error(), "error entry appended");
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled());
    }

    #[tokio::test]
    async fn handle_turn_failed_shows_the_error_and_starts_the_queue_head_once() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        app.pending_prompts.push_back("first".into());
        app.pending_prompts.push_back("second".into());
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
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            vec!["second".to_owned()],
        );
    }

    #[tokio::test]
    async fn handle_turn_failed_during_auto_compaction_returns_the_unsent_prompt_to_the_input() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("//etc/hosts".to_owned()));
        rx.recv().await.expect("prompt forwarded");
        app.handle_engine_event(Event::AutoCompactionStarted);

        app.handle_engine_event(Event::TurnFailed(
            "Auto-compaction failed: summarizer down. The prompt was not sent.".to_owned(),
        ));

        assert!(app.chat.last_is_error());
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_enabled());
        assert_eq!(
            app.input.lines(),
            ["//etc/hosts".to_owned()],
            "Enter sends the prompt again as typed",
        );
        assert!(app.pending_prompts.is_empty());
        assert!(
            rx.try_recv().is_err(),
            "nothing reaches the engine until the user sends"
        );
    }

    #[tokio::test]
    async fn handle_turn_failed_during_auto_compaction_returns_the_prompt_to_the_held_queue_head() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("prompt forwarded");
        app.handle_engine_event(Event::AutoCompactionStarted);
        app.dispatch_action(Action::Submit("queued".to_owned()));

        app.handle_engine_event(Event::TurnFailed(
            "Auto-compaction failed: summarizer down. The prompt was not sent.".to_owned(),
        ));

        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.input.is_empty());
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["active".to_owned(), "queued".to_owned()],
            "the unsent prompt still runs first",
        );
        assert!(
            rx.try_recv().is_err(),
            "nothing retries compaction until the user sends again"
        );
    }

    #[tokio::test]
    async fn handle_turn_failed_during_auto_compaction_holds_the_queue_after_an_untyped_prompt() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_op(Op::Prompt(
            "Create AGENTS.md for this repository.".to_owned(),
        ));
        rx.recv().await.expect("prompt forwarded");
        app.handle_engine_event(Event::AutoCompactionStarted);
        app.pending_prompts.push_back("queued".to_owned());

        app.handle_engine_event(Event::TurnFailed(
            "Auto-compaction failed: summarizer down. The prompt was not sent.".to_owned(),
        ));

        assert!(app.input.is_empty(), "a command's prompt was never typed");
        assert_eq!(
            app.pending_prompts.iter().cloned().collect::<Vec<_>>(),
            ["queued".to_owned()],
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_turn_failed_after_auto_compaction_succeeded_starts_the_queue_head() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("prompt forwarded");
        app.handle_engine_event(Event::AutoCompactionStarted);
        app.handle_engine_event(Event::SessionCompacted {
            summary: "auto summary".to_owned(),
            pre_count: 4,
            instructions: None,
            automatic: true,
        });
        app.pending_prompts.push_back("queued".to_owned());

        app.handle_engine_event(Event::TurnFailed("API failed".to_owned()));

        assert_eq!(rx.try_recv().unwrap(), Op::Prompt("queued".to_owned()));
        assert!(app.input.is_empty(), "the sent prompt is not handed back");
    }

    #[tokio::test]
    async fn pending_queue_resubmits_prompts_each_ended_turn_dropped() {
        // A turn that ends before its next round boundary drops the queued prompts it held, so
        // each one must come back as a later turn.
        let (mut app, mut rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        rx.recv().await.expect("active submit forwarded");
        for prompt in ["a", "b", "c"] {
            app.dispatch_action(Action::Submit(prompt.to_owned()));
        }
        let mut forwarded = || std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(forwarded().len(), 3);

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
        assert!(app.pending_prompts.is_empty());
    }

    // ── finish_turn ──

    #[test]
    fn finish_turn_evicts_orphaned_pending_calls() {
        // `ToolCallStart` without a matching `ToolCallEnd` by turn-end
        // is an orphan (crashed tool, agent-loop bug, mid-turn abort).
        // The pending entry must be discarded so long sessions don't
        // accumulate stale ids across turns.
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

    // ── apply_git_update ──

    #[test]
    fn apply_git_update_marks_dirty_only_when_the_status_bar_changes() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dirty = false;

        app.apply_git_update(GitUpdate::Branch(Some("main".to_owned())));
        assert!(!app.dirty, "unchanged branch skips the repaint");

        app.apply_git_update(GitUpdate::PullRequest(Some(
            crate::util::git::PullRequest {
                number: 7,
                url: "https://github.com/o/r/pull/7".to_owned(),
                checks: None,
                review: None,
            },
        )));
        assert!(app.dirty, "new pull request repaints");
    }

    // ── format_config_change ──

    #[test]
    fn format_config_change_swap_both_none_omits_effort_clause() {
        // Pin: no `effort` substring at all, never a stray "none"
        // word. Mutation that prints `effort none.` would surface here.
        let s = format_config_change(
            &spec("custom-model"),
            true,
            None,
            None,
            None,
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(s, "Switched to custom-model (custom-model).");
    }

    #[test]
    fn format_config_change_swap_to_no_tier_model_surfaces_requested_effort() {
        // Model swap onto a no-tier model with an explicit effort ask. The user needs to know
        // the requested tier didn't apply, otherwise a silent drop reads as a config change
        // landing.
        let s = format_config_change(
            &spec("custom-model"),
            true,
            None,
            None,
            Some(Effort::High),
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(
            s,
            "Switched to custom-model (custom-model). \
             Effort unchanged: model has no effort tier (asked for high).",
        );
    }

    #[test]
    fn format_config_change_swap_clears_effort_when_new_model_drops_it() {
        // User had a tier; new model has none. Surface the change so
        // the user knows their effort just disappeared.
        let s = format_config_change(
            &spec("custom-model"),
            true,
            Some(Effort::Xhigh),
            None,
            None,
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(
            s,
            "Switched to custom-model (custom-model). Effort cleared (model has no effort tier)."
        );
    }

    #[test]
    fn format_config_change_swap_marks_default_when_previous_was_none() {
        // None → Some means the new model's default kicked in;
        // distinguishing this from "user's pick survived" prevents
        // the user from thinking they chose this tier.
        let s = format_config_change(
            &spec("claude-opus-5-5"),
            true,
            None,
            Some(Effort::Medium),
            None,
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(
            s,
            "Switched to Claude Opus 5.5 (claude-opus-5-5) · effort medium (model default)."
        );
    }

    #[test]
    fn format_config_change_swap_reports_carried_or_picked_effort_plainly() {
        // A carried-over tier and an explicit pick both read as the plain effort clause. Pin
        // exact format so a stray suffix (`(unchanged)`) would fail.
        for (prev, requested, new) in [
            (Some(Effort::High), None, Effort::High),
            (Some(Effort::Medium), Some(Effort::Max), Effort::Max),
        ] {
            let s = format_config_change(
                &spec("claude-opus-5-5"),
                true,
                prev,
                Some(new),
                requested,
                base_auto_compaction(),
                base_auto_compaction(),
            );
            assert_eq!(
                s,
                format!("Switched to Claude Opus 5.5 (claude-opus-5-5) · effort {new}."),
            );
        }
    }

    #[test]
    fn format_config_change_effort_explicit_pick_matches_resolution() {
        let s = format_config_change(
            &spec("claude-opus-5-5"),
            false,
            Some(Effort::High),
            Some(Effort::Xhigh),
            Some(Effort::Xhigh),
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(s, "Effort set to xhigh.");
    }

    #[test]
    fn format_config_change_effort_pick_on_no_tier_model_surfaces_loss() {
        // The slash command preflight stops this through /effort, but
        // client-driven flows could still emit it.
        let s = format_config_change(
            &spec("custom-model"),
            false,
            None,
            None,
            Some(Effort::High),
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(
            s,
            "Effort unchanged: model has no effort tier (asked for high)."
        );
    }

    #[test]
    fn format_config_change_without_model_or_effort_reports_no_change() {
        let s = format_config_change(
            &spec("custom-model"),
            false,
            None,
            None,
            None,
            base_auto_compaction(),
            base_auto_compaction(),
        );
        assert_eq!(s, "Config unchanged.");
    }

    #[test]
    fn format_config_change_model_swap_mentions_compaction_threshold_change() {
        let new_auto_compaction = AutoCompactionConfig {
            enabled: true,
            threshold_tokens: Some(test_thresholds::WINDOW_1M),
        };

        let s = format_config_change(
            &spec("claude-sonnet-5-5"),
            true,
            Some(Effort::High),
            Some(Effort::High),
            None,
            base_auto_compaction(),
            new_auto_compaction,
        );

        assert_eq!(
            s,
            format!(
                "Switched to Claude Sonnet 5.5 (claude-sonnet-5-5) · effort high. \
                 Auto compaction at {} tokens.",
                test_thresholds::WINDOW_1M,
            ),
        );
    }
}
