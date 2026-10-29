//! Conversation compaction: the engine's `/compact` and auto-compaction, and the summarization
//! request, which forks the live conversation to read its prompt cache.

use std::collections::HashSet;
use std::iter;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow, bail};
use indoc::{formatdoc, indoc};
use tokio::sync::mpsc;
use tracing::warn;

use super::abort::{AbortResult, Inbox, TurnAbort, await_unless_aborted};
use super::turn::{collect_response, conversation_request, record_interruption};
use super::{Engine, TurnEnd, report_abort};
use crate::auth::Auth;
use crate::client::{PromptTooLong, Request, Response, ResponseEvent, StopReason, Wire};
use crate::config::compaction::AutoCompactionConfig;
use crate::message::{ContentBlock, Message, Role, TextOrigin};
use crate::model::TokenUsage;
use crate::prompt::{PromptParts, UserContext};
use crate::protocol::{CompactTrigger, Event, EventSink, Op};
use crate::session::handle::{Checkpoint, CompactBoundary};
use crate::session::post_compact::synthesize_post_compact_message;
use crate::session::store::CompactRecord;
use crate::tool::ToolDefinition;
use crate::util::text::join_sentences;

const MAX_AUTO_COMPACT_FAILURES: u8 = 3;

/// Rough transcript size below which compaction is refused, since the summary would be about as
/// long as the transcript.
const MIN_TOKENS_FOR_COMPACT: u32 = 1_000;

/// Retries for a summary request that overflowed the context window.
const MAX_OVERFLOW_RETRIES: u32 = 3;

/// Output room each overflow retry frees for the summary, beyond the overflow the API reports.
const SUMMARY_OUTPUT_RESERVE_TOKENS: u32 = 20_000;

/// Opens a retry transcript whose oldest rounds were dropped, since the API requires a user first.
const RETRY_TRUNCATION_MARKER: &str = "[Earlier conversation truncated to fit the summary.]";

/// Ends a tool-use turn a retry transcript leaves open, so its stripped thinking belongs to a
/// finished turn, which may omit it.
const RETRY_TURN_CLOSE_MARKER: &str = "[Stopped here to summarize the conversation.]";

/// User-message rubric. The request still carries the conversation's tools, so its last two
/// sentences keep the model from calling one.
const SUMMARIZATION_USER_RUBRIC: &str = indoc! {"
    Summarize the conversation above so another instance of yourself can pick up where this one left off. Capture, in this order:

    1. The engineer's overall intent and any constraints they stated.
    2. Key technical decisions made and why.
    3. Files, functions, and code paths touched (full paths when known).
    4. Current state: what is done, what is in progress, what is blocked, and which checks passed, failed, or have not run.
    5. The next concrete step, if one is obvious.

    Be concise. Terse bullets beat paragraphs. Preserve exact identifiers, file paths, error strings, and command lines verbatim. Output only the summary, without addressing the engineer or asking questions. Do not call any tools while writing this summary. Respond with text only."
};

// ── Auto-Compaction Failures ──

/// Automatic compaction failures in a row. Auto-compaction pauses at [`MAX_AUTO_COMPACT_FAILURES`]
/// until a reset.
#[derive(Clone, Copy, Default)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(super) struct AutoCompactFailures(u8);

impl AutoCompactFailures {
    #[cfg(test)]
    pub(super) const fn new(count: u8) -> Self {
        Self(count)
    }

    fn is_paused(self) -> bool {
        self.0 >= MAX_AUTO_COMPACT_FAILURES
    }

    /// Counts a failed attempt at `point`, reporting [`Event::AutoCompactionPaused`] if it pauses
    /// auto-compaction, and returns the abort that fails the unsent prompt or the turn. `cause` is
    /// why the summary failed, and `None` means the summary succeeded but its boundary was not
    /// saved.
    fn fail(
        &mut self,
        sink: &dyn EventSink,
        cause: Option<&anyhow::Error>,
        point: AutoPoint,
    ) -> TurnAbort {
        self.0 += 1;
        let paused = self.is_paused();
        if paused {
            sink.emit(
                Event::AutoCompactionPaused { failures: self.0 },
                "auto-compaction-paused",
            );
        }
        let next = match (point, paused) {
            (AutoPoint::MidTurn, false) => "The turn stopped before its next request. \
                 Run `/compact`, then send a message to continue."
                .to_owned(),
            (AutoPoint::MidTurn, true) => format!(
                "The turn stopped before its next request, and auto-compaction is paused \
                 after {MAX_AUTO_COMPACT_FAILURES} failures in a row. \
                 Run `/compact`, then send a message to continue."
            ),
            (AutoPoint::BeforePrompt, false) => {
                "The prompt was not sent, so send it again to retry.".to_owned()
            }
            (AutoPoint::BeforePrompt, true) => format!(
                "The prompt was not sent, and auto-compaction is paused \
                 after {MAX_AUTO_COMPACT_FAILURES} failures in a row. \
                 Send the prompt again to run it without compaction, \
                 or run `/compact` to retry compaction first."
            ),
        };
        let message = match cause {
            Some(e) => join_sentences(&format!("Auto-compaction failed: {e:#}"), &next),
            None => format!("Auto-compaction was not saved. {next}"),
        };
        TurnAbort::Failed(anyhow!(message))
    }

    /// Clears the count and reports [`Event::AutoCompactionResumed`] if auto-compaction was paused.
    pub(super) fn reset(&mut self, sink: &dyn EventSink) {
        if std::mem::take(self).is_paused() {
            sink.emit(Event::AutoCompactionResumed, "auto-compaction-resumed");
        }
    }
}

// ── Compaction ──

/// Where an automatic compaction runs.
#[derive(Debug, Clone, Copy)]
enum AutoPoint {
    /// Before a submitted prompt.
    BeforePrompt,
    /// At a round boundary inside a turn.
    MidTurn,
}

impl From<AutoPoint> for CompactTrigger {
    fn from(point: AutoPoint) -> Self {
        match point {
            AutoPoint::BeforePrompt => Self::Auto,
            AutoPoint::MidTurn => Self::MidTurn,
        }
    }
}

/// Why a compaction left the transcript unchanged.
enum CompactFailure {
    /// The user cancelled the summary, or the engine shut down.
    Aborted(TurnAbort),
    /// The summary request failed.
    Summary(anyhow::Error),
    /// The summary succeeded, but the session writer was gone, so its boundary was neither saved
    /// nor applied.
    WriterGone,
}

impl Engine {
    /// The auto-compaction config in force, disabled while [`AutoCompactFailures`] has paused it.
    pub(super) fn auto_compaction(&self) -> AutoCompactionConfig {
        if self.usage.auto_compaction_failures.is_paused() {
            AutoCompactionConfig::disabled()
        } else {
            self.compaction.auto
        }
    }

    /// Compacts the transcript ahead of a prompt once [`auto_compaction_due`] holds for the stored
    /// context estimate. A failure counts toward the pause and fails the unsent prompt, which the
    /// caller hands back.
    pub(super) async fn auto_compact(
        &mut self,
        auth: &Auth,
        sink: &dyn EventSink,
        ops: &mut mpsc::Receiver<Op>,
        inbox: &mut Inbox,
    ) -> AbortResult<()> {
        if !auto_compaction_due(
            self.auto_compaction(),
            self.usage.context_estimate(),
            self.transcript.messages(),
        ) {
            return Ok(());
        }

        self.run_auto_compaction(auth, sink, ops, inbox, AutoPoint::BeforePrompt)
            .await
            .map(drop)
    }

    /// Compacts the transcript at a round boundary of a turn whose context crossed the threshold,
    /// so the turn goes on from the summary, and returns the summary's checkpoint. A cancel ends
    /// the turn on [`Message::interruption`] as at any other round boundary, and a failure counts
    /// toward the pause and fails the turn.
    pub(super) async fn compact_mid_turn(
        &mut self,
        auth: &Auth,
        sink: &dyn EventSink,
        ops: &mut mpsc::Receiver<Op>,
        inbox: &mut Inbox,
    ) -> AbortResult<Checkpoint> {
        let compacted = self
            .run_auto_compaction(auth, sink, ops, inbox, AutoPoint::MidTurn)
            .await;
        if let Err(TurnAbort::Cancelled | TurnAbort::Shutdown) = compacted {
            record_interruption(&mut self.transcript, sink).await;
        }
        compacted
    }

    /// Runs an automatic compaction at `point`, counting a failure toward the pause.
    async fn run_auto_compaction(
        &mut self,
        auth: &Auth,
        sink: &dyn EventSink,
        ops: &mut mpsc::Receiver<Op>,
        inbox: &mut Inbox,
        point: AutoPoint,
    ) -> AbortResult<Checkpoint> {
        sink.emit(Event::AutoCompactionStarted, "auto-compaction-started");
        let cause = match self
            .run_compaction(auth, sink, ops, inbox, None, point.into())
            .await
        {
            Ok(checkpoint) => return Ok(checkpoint),
            Err(CompactFailure::Aborted(abort)) => return Err(abort),
            Err(CompactFailure::Summary(e)) => {
                warn!(?point, error = %format_args!("{e:#}"), "auto-compaction failed");
                Some(e)
            }
            Err(CompactFailure::WriterGone) => None,
        };
        Err(self
            .usage
            .auto_compaction_failures
            .fail(sink, cause.as_ref(), point))
    }

    /// Replaces the transcript with a summary steered by `instructions`. Failures leave it intact.
    pub(super) async fn compact(
        &mut self,
        sink: &dyn EventSink,
        ops: &mut mpsc::Receiver<Op>,
        instructions: Option<String>,
    ) -> TurnEnd {
        let mut inbox = Inbox::default();
        let auth = match self.ensure_signed_in(sink, ops, &mut inbox).await {
            Ok(auth) => auth,
            Err(abort) => {
                self.adopt_mode(&mut inbox);
                return report_abort(sink, abort, "compact-failed");
            }
        };
        let compacted = self
            .run_compaction(
                &auth,
                sink,
                ops,
                &mut inbox,
                instructions,
                CompactTrigger::Manual,
            )
            .await;
        self.adopt_mode(&mut inbox);
        let abort = match compacted {
            Ok(_) => {
                sink.emit(Event::SessionReady, "session-ready");
                return TurnEnd::Completed;
            }
            Err(CompactFailure::Aborted(abort)) => abort,
            Err(CompactFailure::Summary(e)) => TurnAbort::Failed(e.context("Compaction failed")),
            Err(CompactFailure::WriterGone) => TurnAbort::Failed(anyhow!(
                "Compaction was not saved, so the transcript is unchanged"
            )),
        };
        report_abort(sink, abort, "compact-failed")
    }

    /// Summarizes the transcript and swaps the summary in as it saves the boundary, starting the
    /// new transcript from a fresh user context, and returns the summary's checkpoint. Bills each
    /// summary request and shows the usage once any request reported it or the transcript changed.
    async fn run_compaction(
        &mut self,
        auth: &Auth,
        sink: &dyn EventSink,
        ops: &mut mpsc::Receiver<Op>,
        inbox: &mut Inbox,
        instructions: Option<String>,
        trigger: CompactTrigger,
    ) -> Result<Checkpoint, CompactFailure> {
        let tools = self.tools.definitions();
        let mut spent = Vec::new();
        let session_id = self.session_id();
        let summary = await_unless_aborted(
            request_summary(
                &self.client,
                conversation_request(&session_id, auth, &self.settings, &self.transcript, &tools),
                instructions.as_deref(),
                sink,
                &mut spent,
            ),
            ops,
            inbox,
            sink,
        )
        .await;
        let compacted = match summary {
            Ok(Ok(summary)) => {
                let (fresh, warnings) = UserContext::load(self.cwd(), self.settings.model()).await;
                let boundary = CompactBoundary {
                    record: CompactRecord {
                        summary,
                        instructions,
                        tasks: self.transcript.tasks.clone(),
                    },
                    context: self.transcript.prompt().refreshed(fresh),
                };
                let compacted = self.replace_with_summary(sink, boundary, trigger).await;
                if compacted.is_some() {
                    self.show_instruction_warnings(sink, warnings);
                }
                compacted.ok_or(CompactFailure::WriterGone)
            }
            Ok(Err(e)) => Err(CompactFailure::Summary(e)),
            Err(abort) => Err(CompactFailure::Aborted(abort)),
        };
        // Bills after the boundary, so a write error it surfaces follows the compaction event that
        // clears the TUI's chat.
        self.bill(sink, &spent).await;
        if compacted.is_ok() {
            self.reset_context_usage(sink);
        }
        if compacted.is_ok() || !spent.is_empty() {
            self.emit_usage_update(sink);
        }
        compacted
    }

    /// Saves the compaction boundary and swaps the transcript to the summary message, which lists
    /// the open background shells and, after a mid-turn `trigger`, asks the model to resume the
    /// turn. It then clears the file tracker, reports the compaction under `trigger`, and returns
    /// the summary's checkpoint. A failed write stops persistence, while the summary still replaces
    /// the transcript in memory and the failure is reported after the compaction event. Only a
    /// session writer that is gone discards the summary and leaves the transcript unchanged.
    async fn replace_with_summary(
        &mut self,
        sink: &dyn EventSink,
        boundary: CompactBoundary,
        trigger: CompactTrigger,
    ) -> Option<Checkpoint> {
        let synthetic = synthesize_post_compact_message(
            &boundary.record.summary,
            &boundary.record.tasks,
            &self.shells.list(),
            trigger,
        );
        let summary = boundary.record.summary.clone();
        let instructions = boundary.record.instructions.clone();
        let outcome = self.transcript.compact(boundary, synthetic).await;
        let Some(checkpoint) = outcome.checkpoint else {
            sink.emit(Event::SummaryDiscarded, "summary-discarded");
            sink.session_write(&outcome.report);
            return None;
        };

        sink.emit(
            Event::SessionCompacted {
                summary,
                pre_count: outcome.pre_count,
                instructions,
                trigger,
            },
            "session-compacted",
        );
        // Reports the write after the compaction event, which clears the TUI's chat.
        sink.session_write(&outcome.report);
        Some(checkpoint)
    }
}

/// Whether auto-compaction under `config` replaces `transcript`, whose next request holds about
/// `context_tokens`, never when that size is unknown. The transcript must also reach the `/compact`
/// size floor, below which the system prompt, instructions, and tools fill most of the context.
pub(super) fn auto_compaction_due(
    config: AutoCompactionConfig,
    context_tokens: u32,
    transcript: &[Message],
) -> bool {
    config.should_trigger(context_tokens) && transcript_tokens(transcript) >= MIN_TOKENS_FOR_COMPACT
}

// ── Summary Request ──

/// Drives the summarization request that forks `conversation`, the request the conversation's next
/// round would send, and returns the trimmed summary. `spent` receives the usage of every request,
/// even when the summary fails or the caller drops the future.
///
/// The summary streams to `sink` as [`Event::SummaryToken`]s. A retry, a failure, or a dropped
/// future ends them with [`Event::SummaryDiscarded`]. On success the caller must end them with
/// [`Event::SessionCompacted`] or [`Event::SummaryDiscarded`].
///
/// Errors when the transcript is too short to compact, a request fails, or the reply is empty or
/// stops before the summary finishes.
async fn request_summary(
    client: &dyn Wire,
    conversation: Request<'_>,
    instructions: Option<&str>,
    sink: &dyn EventSink,
    spent: &mut Vec<TokenUsage>,
) -> Result<String> {
    let transcript = conversation.messages;
    if transcript.is_empty() {
        bail!("the session has no messages yet");
    }
    let tokens = transcript_tokens(transcript);
    if tokens < MIN_TOKENS_FOR_COMPACT {
        bail!(
            "the conversation has about {tokens} tokens, \
             below the {MIN_TOKENS_FOR_COMPACT}-token minimum"
        );
    }

    let rubric = Message::user(build_user_message(instructions));
    let forked: Vec<Message> = transcript.iter().chain([&rubric]).cloned().collect();
    let stream = SummaryStream::new(sink);
    // The fork keeps the tail breakpoint, which reads the conversation's cached prefix.
    let mut rx = client.stream(Request {
        messages: &forked,
        ..conversation
    })?;
    let mut kept = transcript.to_vec();
    let mut retries = 0;
    let response = loop {
        let result = collect_summary(rx, &stream, spent).await;
        let smaller = overflow_tokens(&result)
            .filter(|_| retries < MAX_OVERFLOW_RETRIES)
            .and_then(|overflow| {
                drop_oldest_rounds(
                    &kept,
                    overflow.saturating_add(SUMMARY_OUTPUT_RESERVE_TOKENS),
                )
            });
        let Some(smaller) = smaller else {
            break result?;
        };
        retries += 1;
        kept = smaller;
        stream.discard();
        let retry = retry_request(&kept, &rubric);
        rx = client.stream(Request {
            messages: &retry,
            cache_tail: false,
            ..conversation
        })?;
    };
    match response.stop_reason {
        None | Some(StopReason::EndTurn | StopReason::StopSequence) => {}
        Some(StopReason::MaxTokens) => {
            bail!("the summary hit the `max_tokens` output limit before it finished")
        }
        Some(StopReason::ModelContextWindowExceeded) => {
            bail!("the transcript left no room in the context window for the summary")
        }
        Some(StopReason::Refusal) => bail!("the model declined to summarize the conversation"),
        Some(StopReason::ToolUse) => bail!("the model called a tool before finishing the summary"),
        Some(reason @ (StopReason::PauseTurn | StopReason::Unknown)) => {
            bail!("the summary stopped early ({})", reason.as_str())
        }
    }

    let summary: String = response
        .content
        .iter()
        .filter_map(ContentBlock::as_text)
        .collect();
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        bail!("the model returned an empty summary");
    }
    stream.keep();
    Ok(trimmed.to_owned())
}

/// Collects one summarization reply, adding its usage to `spent` even when it fails.
async fn collect_summary(
    rx: mpsc::Receiver<Result<ResponseEvent>>,
    stream: &SummaryStream<'_>,
    spent: &mut Vec<TokenUsage>,
) -> Result<Response> {
    let mut usage = RequestUsage {
        total: spent,
        request: None,
    };
    collect_response(rx, stream, &mut usage.request).await
}

/// Forwards the summary's text, retry notices, and usage limits to the frontend and drops the rest
/// of the reply, which belongs to no visible turn. Dropping it discards the streamed text, so a
/// cancelled compaction clears it.
struct SummaryStream<'a> {
    sink: &'a dyn EventSink,
    streamed: AtomicBool,
}

impl<'a> SummaryStream<'a> {
    fn new(sink: &'a dyn EventSink) -> Self {
        Self {
            sink,
            streamed: AtomicBool::new(false),
        }
    }

    /// Tells the frontend to drop the summary text streamed so far, if any.
    fn discard(&self) {
        if self.streamed.swap(false, Ordering::Relaxed) {
            self.sink.emit(Event::SummaryDiscarded, "summary-discarded");
        }
    }

    /// Leaves the streamed summary for the compact boundary to commit or discard.
    fn keep(mut self) {
        *self.streamed.get_mut() = false;
    }
}

impl Drop for SummaryStream<'_> {
    fn drop(&mut self) {
        self.discard();
    }
}

impl EventSink for SummaryStream<'_> {
    fn send(&self, event: Event) -> Result<()> {
        match event {
            Event::StreamToken(text) => {
                self.streamed.store(true, Ordering::Relaxed);
                self.sink.send(Event::SummaryToken(text))
            }
            Event::Retrying(_) | Event::RateLimitsUpdated(_) => self.sink.send(event),
            _ => Ok(()),
        }
    }
}

/// Adds one request's usage to the running total on drop, so failed and cancelled requests count.
struct RequestUsage<'a> {
    total: &'a mut Vec<TokenUsage>,
    request: Option<TokenUsage>,
}

impl Drop for RequestUsage<'_> {
    fn drop(&mut self) {
        self.total.extend(self.request);
    }
}

// ── Overflow Retries ──

/// Tokens a summary request ran over the context window by, or `Some(0)` when the API did not say.
/// `None` when the request fit.
fn overflow_tokens(result: &Result<Response>) -> Option<u32> {
    match result {
        Ok(response) => {
            (response.stop_reason == Some(StopReason::ModelContextWindowExceeded)).then_some(0)
        }
        Err(error) => error
            .downcast_ref::<PromptTooLong>()
            .map(|too_long| too_long.overflow.unwrap_or(0)),
    }
}

/// Copies `transcript` without thinking blocks or an earlier retry's truncation marker, then drops
/// its oldest API rounds until about `target` tokens are freed, keeping at least the newest round
/// and never parting a server tool result from its call. `None` when only one round remains.
fn drop_oldest_rounds(transcript: &[Message], target: u32) -> Option<Vec<Message>> {
    let transcript = match transcript {
        [first, rest @ ..] if is_truncation_marker(first) => rest,
        _ => transcript,
    };
    let mut messages: Vec<Message> = transcript.iter().filter_map(without_thinking).collect();
    let mut freed = 0;
    let mut cut = None;
    for (i, message) in messages.iter().enumerate() {
        if i > 0 && message.role == Role::Assistant && !answers_an_earlier_call(message) {
            cut = Some(i);
            if freed >= target {
                break;
            }
        }
        freed = freed.saturating_add(rough_tokens(message));
    }
    let kept = messages.split_off(cut?);
    Some(iter::once(truncation_marker()).chain(kept).collect())
}

/// Returns whether `message` holds a server tool result whose call an earlier reply made. A
/// `pause_turn`, or a client call beside the server call, defers its result to the next reply.
fn answers_an_earlier_call(message: &Message) -> bool {
    let calls: HashSet<&str> = message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ServerToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    message.content.iter().any(|b| {
        matches!(b, ContentBlock::ServerToolResult { tool_use_id, .. }
            if !calls.contains(tool_use_id.as_str()))
    })
}

fn truncation_marker() -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::generated_text(
            RETRY_TRUNCATION_MARKER,
            TextOrigin::CompactionTruncation,
        )],
    }
}

fn is_truncation_marker(message: &Message) -> bool {
    message.role == Role::User
        && matches!(
            &message.content[..],
            [ContentBlock::Text {
                origin: Some(TextOrigin::CompactionTruncation),
                ..
            }]
        )
}

/// Drops thinking blocks, which a new turn may omit, and the message if nothing else remains.
fn without_thinking(message: &Message) -> Option<Message> {
    let content: Vec<ContentBlock> = message
        .content
        .iter()
        .filter(|b| {
            !matches!(
                b,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        })
        .cloned()
        .collect();
    (!content.is_empty()).then_some(Message {
        role: message.role,
        content,
    })
}

/// Estimates the tokens of a whole request from its parts, for a transcript no request has reported
/// usage on.
pub(super) fn request_tokens(
    prompt: &PromptParts,
    tools: &[&ToolDefinition],
    transcript: &[Message],
) -> u32 {
    let system = &prompt.system;
    let text = system.stable.len() + system.dynamic.len() + prompt.user_context.len();
    let tools = serde_json::to_string(tools).map_or(0, |json| json.len());
    byte_tokens(text + tools).saturating_add(transcript_tokens(transcript))
}

/// Estimates the tokens `transcript` adds to a request, at four bytes of JSON each.
pub(super) fn transcript_tokens(transcript: &[Message]) -> u32 {
    transcript
        .iter()
        .map(rough_tokens)
        .fold(0, u32::saturating_add)
}

/// Estimates a message's tokens.
fn rough_tokens(message: &Message) -> u32 {
    byte_tokens(serde_json::to_string(message).map_or(0, |json| json.len()))
}

fn byte_tokens(bytes: usize) -> u32 {
    u32::try_from(bytes / 4).unwrap_or(u32::MAX)
}

/// Appends the rubric to a retry transcript, closing a tool-use turn it leaves open first. The API
/// merges consecutive user messages, so a tool result anywhere in the final user run leaves the
/// turn open, even when an interruption marker follows it.
fn retry_request(kept: &[Message], rubric: &Message) -> Vec<Message> {
    let open_turn = kept
        .iter()
        .rev()
        .take_while(|message| message.role == Role::User)
        .flat_map(|message| &message.content)
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
    kept.iter()
        .cloned()
        .chain(open_turn.then(|| Message::assistant(RETRY_TURN_CLOSE_MARKER)))
        .chain(iter::once(rubric.clone()))
        .collect()
}

// ── Rubric ──

/// Composes the rubric plus optional user-supplied focus instructions.
fn build_user_message(instructions: Option<&str>) -> String {
    match instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => formatdoc! {"
            {SUMMARIZATION_USER_RUBRIC}

            Additional instructions from the engineer:

            {extra}"
        },
        None => SUMMARIZATION_USER_RUBRIC.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::pin::pin;
    use std::sync::{LazyLock, Mutex};

    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request as HttpRequest, ResponseTemplate};

    use super::*;
    use crate::client::anthropic::Client;
    use crate::client::anthropic::local_testing::test_client;
    use crate::client::anthropic::testing::{api_key, sse_body, sse_response, text_stream_body};
    use crate::client::testing::five_hour_limit;
    use crate::config::compaction::CompactionConfig;
    use crate::engine::local_testing::{dead_test_session, inert_ops, tool_heavy_transcript};
    use crate::engine::login::SignIn;
    use crate::engine::testing::{
        MODEL, SESSION_CONTEXT, engine, mount_reply, request_bodies, with_pinned_home,
    };
    use crate::engine::transcript::LiveTranscript;
    use crate::file_tracker::testing::{
        assert_writes_draining, blocked_write, poll_until_draining, seed_full_read,
    };
    use crate::message::Replay;
    use crate::model::ModelSettings;
    use crate::permission::Mode;
    use crate::prompt::SessionContext;
    use crate::protocol::testing::CapturingSink;
    use crate::protocol::{ResumeTarget, UsageSnapshot};
    use crate::session::store::local_testing::fail_writes;
    use crate::session::store::testing::test_store;
    use crate::tool::bash::background::ShellState;
    use crate::tool::bash::background::local_testing::start_shell;
    use crate::tool::bash::background::testing::shell;
    use crate::tool::tasks::Task;
    use crate::tool::tasks::testing::plan;

    fn to_json(messages: &[Message]) -> Vec<serde_json::Value> {
        messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect()
    }

    mod auto_compact_failures {
        use super::*;

        mod fail {
            use super::*;

            #[test]
            fn keeps_the_cause_verbatim_before_the_next_step() {
                for (cause, expected) in [
                    (
                        Some(anyhow!("upstream timed out...")),
                        "Auto-compaction failed: upstream timed out... \
                         The prompt was not sent, so send it again to retry.",
                    ),
                    (
                        Some(anyhow!("upstream timed out")),
                        "Auto-compaction failed: upstream timed out. \
                         The prompt was not sent, so send it again to retry.",
                    ),
                    (
                        None,
                        "Auto-compaction was not saved. \
                         The prompt was not sent, so send it again to retry.",
                    ),
                ] {
                    let mut failures = AutoCompactFailures::default();

                    let e = failures
                        .fail(
                            &CapturingSink::default(),
                            cause.as_ref(),
                            AutoPoint::BeforePrompt,
                        )
                        .into_failure();

                    assert_eq!(e.to_string(), expected);
                    assert_eq!(failures, AutoCompactFailures::new(1));
                }
            }

            #[test]
            fn third_failure_pauses_auto_compaction_and_says_so() {
                let sink = CapturingSink::default();
                let mut failures = AutoCompactFailures::new(MAX_AUTO_COMPACT_FAILURES - 1);

                let e = failures
                    .fail(&sink, None, AutoPoint::BeforePrompt)
                    .into_failure();

                assert_eq!(
                    e.to_string(),
                    "Auto-compaction was not saved. \
                     The prompt was not sent, \
                     and auto-compaction is paused after 3 failures in a row. \
                     Send the prompt again to run it without compaction, \
                     or run `/compact` to retry compaction first.",
                );
                assert!(failures.is_paused());
                assert_eq!(sink.events(), [Event::AutoCompactionPaused { failures: 3 }]);
            }

            #[test]
            fn mid_turn_failure_points_to_compact_before_continuing() {
                for (count, expected) in [
                    (
                        0,
                        "Auto-compaction failed: upstream timed out. \
                         The turn stopped before its next request. \
                         Run `/compact`, then send a message to continue.",
                    ),
                    (
                        MAX_AUTO_COMPACT_FAILURES - 1,
                        "Auto-compaction failed: upstream timed out. \
                         The turn stopped before its next request, \
                         and auto-compaction is paused after 3 failures in a row. \
                         Run `/compact`, then send a message to continue.",
                    ),
                ] {
                    let mut failures = AutoCompactFailures::new(count);

                    let e = failures
                        .fail(
                            &CapturingSink::default(),
                            Some(&anyhow!("upstream timed out")),
                            AutoPoint::MidTurn,
                        )
                        .into_failure();

                    assert_eq!(e.to_string(), expected);
                }
            }
        }

        mod reset {
            use super::*;

            #[test]
            fn reports_a_resume_only_from_a_pause() {
                let sink = CapturingSink::default();
                let mut counting = AutoCompactFailures::new(MAX_AUTO_COMPACT_FAILURES - 1);
                let mut paused = AutoCompactFailures::new(MAX_AUTO_COMPACT_FAILURES);

                counting.reset(&sink);
                let after_counting = sink.events();
                paused.reset(&sink);

                assert_eq!(counting, AutoCompactFailures::default());
                assert_eq!(paused, AutoCompactFailures::default());
                assert_eq!(after_counting, []);
                assert_eq!(sink.events(), [Event::AutoCompactionResumed]);
            }
        }
    }

    mod auto_compact {
        use super::*;

        /// Engine on a fresh session in `dir` that auto-compacts at 50,000 tokens, whose last turn
        /// left `context_estimate` on `transcript`.
        async fn auto_engine(
            server: &MockServer,
            dir: &Path,
            context_estimate: u32,
            transcript: Vec<Message>,
        ) -> Engine {
            let mut engine = engine(server.uri(), &test_store(dir), dir).await;
            engine.compaction = CompactionConfig::with_threshold(50_000);
            engine.usage.set_context_estimate(context_estimate);
            engine.transcript.set_messages(transcript);
            engine
        }

        #[tokio::test]
        async fn skips_below_the_threshold_or_for_a_short_transcript() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let short = vec![Message::user("hi"), Message::assistant("hello")];
                let over = 50_000;
                let under = 20_000;
                for (context_estimate, transcript) in
                    [(under, tool_heavy_transcript()), (over, short)]
                {
                    let dir = tempfile::tempdir().unwrap();
                    let mut engine =
                        auto_engine(&server, dir.path(), context_estimate, transcript.clone())
                            .await;
                    let sink = CapturingSink::default();

                    let compacted = engine
                        .auto_compact(&api_key(), &sink, &mut inert_ops(), &mut Inbox::default())
                        .await;

                    assert!(compacted.is_ok(), "{compacted:?}");
                    assert_eq!(engine.transcript.messages(), transcript);
                    assert_eq!(sink.events(), []);
                }
                assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
            })
            .await;
        }

        #[tokio::test]
        async fn skips_once_failures_pause_it() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "   ").await;
                let dir = tempfile::tempdir().unwrap();
                let usage = 50_000;
                let mut engine =
                    auto_engine(&server, dir.path(), usage, tool_heavy_transcript()).await;
                engine.usage.auto_compaction_failures =
                    AutoCompactFailures::new(MAX_AUTO_COMPACT_FAILURES - 1);
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                let third = engine
                    .auto_compact(&api_key(), &sink, &mut ops, &mut Inbox::default())
                    .await;
                let fourth = engine
                    .auto_compact(&api_key(), &sink, &mut ops, &mut Inbox::default())
                    .await;

                assert!(matches!(third, Err(TurnAbort::Failed(_))), "{third:?}");
                assert!(
                    fourth.is_ok(),
                    "the next prompt goes out without compaction: {fourth:?}"
                );
                assert_eq!(request_bodies(&server).await.len(), 1);
                assert_eq!(engine.transcript.messages(), tool_heavy_transcript());
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::AutoCompactionStarted,
                            Event::SummaryToken(_),
                            Event::SummaryDiscarded,
                            Event::UsageUpdated(_),
                            Event::AutoCompactionPaused { failures: 3 },
                        ]
                    ),
                    "{events:?}",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn fails_the_prompt_when_the_writer_is_gone() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "auto summary").await;
                let dir = tempfile::tempdir().unwrap();
                let usage = 50_000;
                let mut engine =
                    auto_engine(&server, dir.path(), usage, tool_heavy_transcript()).await;
                engine.transcript.set_session(dead_test_session());
                let sink = CapturingSink::default();

                let e = engine
                    .auto_compact(&api_key(), &sink, &mut inert_ops(), &mut Inbox::default())
                    .await
                    .unwrap_err()
                    .into_failure();

                assert_eq!(
                    e.to_string(),
                    "Auto-compaction was not saved. \
                     The prompt was not sent, so send it again to retry.",
                );
                assert_eq!(
                    engine.usage.auto_compaction_failures,
                    AutoCompactFailures::new(1)
                );
                assert_eq!(engine.transcript.messages(), tool_heavy_transcript());
                assert_eq!(
                    engine.usage.context_estimate(),
                    usage,
                    "the transcript the usage measured stays"
                );
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::AutoCompactionStarted,
                            Event::SummaryToken(summary),
                            Event::SummaryDiscarded,
                            Event::Error(write),
                            Event::UsageUpdated(_),
                        ] if summary == "auto summary"
                            && write == "Session write failed: \
                                         session writer task has stopped. \
                                         Conversation history may be incomplete."
                    ),
                    "the summary is discarded before the write error, then billed: {events:?}",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn keeps_the_prompts_queued_during_the_summary_for_the_turn() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "auto summary").await;
                let dir = tempfile::tempdir().unwrap();
                let usage = 50_000;
                let mut engine =
                    auto_engine(&server, dir.path(), usage, tool_heavy_transcript()).await;
                engine.transcript.tasks = plan();
                let sink = CapturingSink::default();
                let (ops_tx, mut ops) = mpsc::channel(1);
                ops_tx
                    .try_send(Op::Queue("queued after summary".to_owned()))
                    .unwrap();
                let mut inbox = Inbox::default();

                let compacted = engine
                    .auto_compact(&api_key(), &sink, &mut ops, &mut inbox)
                    .await;

                assert!(compacted.is_ok(), "{compacted:?}");
                assert_eq!(inbox.prompts, ["queued after summary"]);
                assert_eq!(
                    engine.transcript.messages(),
                    [synthesize_post_compact_message(
                        "auto summary",
                        &plan(),
                        &[],
                        CompactTrigger::Auto
                    )]
                );
                assert!(
                    sink.events().iter().any(|event| matches!(
                        event,
                        Event::SessionCompacted {
                            trigger: CompactTrigger::Auto,
                            ..
                        }
                    )),
                    "{:?}",
                    sink.events(),
                );
            })
            .await;
        }
    }

    mod compact {
        use super::*;

        /// Serves one summary that streams `text` and then stalls with the connection open. Returns
        /// the base URL.
        async fn serve_stalled_summary(text: &str) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let body = sse_body(&[
                serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "model": MODEL, "usage": {"input_tokens": 1_000, "output_tokens": 0}}}),
                serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            ]);
            let declared = body.len() + 1;
            let response = formatdoc! {"
                HTTP/1.1 200 OK\r
                content-type: text/event-stream\r
                content-length: {declared}\r
                \r
                {body}"
            };
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                _ = socket.read(&mut [0; 1024]).await.unwrap();
                socket.write_all(response.as_bytes()).await.unwrap();
                std::future::pending::<()>().await;
            });
            base_url
        }

        #[tokio::test]
        async fn replaces_the_transcript_bills_the_summary_and_resumes_auto_compaction() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "manual summary").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                engine.transcript.set_messages(tool_heavy_transcript());
                engine.usage.set_displayed_usage(TokenUsage::new(90_000, 9));
                engine.usage.set_total_estimated_cost_usd(1.23);
                engine.usage.auto_compaction_failures = AutoCompactFailures::new(3);
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                let end = engine
                    .compact(&sink, &mut ops, Some("focus".to_owned()))
                    .await;

                assert_eq!(end, TurnEnd::Completed);
                assert_eq!(
                    engine.transcript.messages(),
                    [synthesize_post_compact_message(
                        "manual summary",
                        &[],
                        &[],
                        CompactTrigger::Manual
                    )]
                );
                assert_eq!(engine.usage.displayed_usage(), None);
                assert!(
                    (engine.usage.total_estimated_cost_usd() - (1.23 + 0.006)).abs() < 1e-12,
                    "the session cost keeps earlier turns and adds the summary: {}",
                    engine.usage.total_estimated_cost_usd(),
                );
                assert_ne!(
                    engine.transcript.prompt().user_context,
                    SESSION_CONTEXT,
                    "the compacted transcript starts from a fresh user context"
                );
                assert!(
                    !engine.transcript.prompt_unrecorded(),
                    "the boundary records the fresh prompt"
                );
                let data = store.load_session_data(&engine.session_id()).unwrap();
                assert_eq!(
                    data.context,
                    Some(engine.transcript.prompt().session_context())
                );
                assert_eq!(
                    engine.usage.auto_compaction_failures,
                    AutoCompactFailures::default()
                );
                let events = sink.events();
                let [
                    Event::SummaryToken(streamed),
                    Event::SessionCompacted {
                        trigger: CompactTrigger::Manual,
                        instructions: Some(focus),
                        ..
                    },
                    Event::AutoCompactionResumed,
                    Event::UsageUpdated(usage),
                    Event::SessionReady,
                ] = events.as_slice()
                else {
                    panic!(
                        "expected the summary, compaction, resume, cost, then the end: {events:?}"
                    );
                };
                assert_eq!(streamed, "manual summary");
                assert_eq!(focus, "focus");
                assert_eq!(usage.context_tokens, None);
                assert!((usage.estimated_cost_usd.unwrap() - (1.23 + 0.006)).abs() < 1e-12);
            })
            .await;
        }

        #[tokio::test]
        async fn carries_the_task_list_into_the_summary_and_through_a_resume() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "manual summary").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                let session_id = engine.session_id();
                engine.transcript.set_messages(tool_heavy_transcript());
                engine.transcript.tasks = plan();
                let mut ops = inert_ops();

                let end = engine
                    .compact(&CapturingSink::default(), &mut ops, None)
                    .await;

                assert_eq!(end, TurnEnd::Completed);
                assert_eq!(
                    engine.transcript.messages(),
                    [synthesize_post_compact_message(
                        "manual summary",
                        &plan(),
                        &[],
                        CompactTrigger::Manual
                    )]
                );
                assert_eq!(engine.transcript.tasks, plan());

                engine.clear(&CapturingSink::default()).await;
                engine
                    .resume(
                        &CapturingSink::default(),
                        ResumeTarget::Id(session_id.clone()),
                    )
                    .await;

                assert_eq!(engine.session_id(), session_id);
                assert_eq!(engine.transcript.tasks, plan());
            })
            .await;
        }

        #[tokio::test]
        async fn lists_the_open_background_shells_in_the_summary_message() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "manual summary").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                engine.transcript.set_messages(tool_heavy_transcript());
                start_shell(&engine.shells, "sleep 30").await;
                let mut ops = inert_ops();

                let end = engine
                    .compact(&CapturingSink::default(), &mut ops, None)
                    .await;

                assert_eq!(end, TurnEnd::Completed);
                assert_eq!(
                    engine.transcript.messages(),
                    [synthesize_post_compact_message(
                        "manual summary",
                        &[],
                        &[shell("shell-1", "sleep 30", ShellState::Running)],
                        CompactTrigger::Manual
                    )]
                );
                engine.shells.stop_all().await;
            })
            .await;
        }

        #[tokio::test]
        async fn failure_or_cancel_keeps_the_transcript() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "summary").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.usage.set_total_estimated_cost_usd(1.23);
                let sink = CapturingSink::default();
                let (ops_tx, mut ops) = mpsc::channel(1);

                engine
                    .transcript
                    .set_messages(vec![Message::user("too short")]);
                let end = engine.compact(&sink, &mut ops, None).await;
                assert_eq!(end, TurnEnd::Failed);
                assert_eq!(engine.transcript.messages(), [Message::user("too short")]);

                engine.transcript.set_messages(tool_heavy_transcript());
                ops_tx.send(Op::Interrupt).await.unwrap();
                let end = engine.compact(&sink, &mut ops, None).await;
                assert_eq!(end, TurnEnd::Cancelled);
                assert_eq!(engine.transcript.messages(), tool_heavy_transcript());

                engine.transcript.set_session(dead_test_session());
                let end = engine.compact(&sink, &mut ops, None).await;
                assert_eq!(
                    end,
                    TurnEnd::Failed,
                    "a compaction whose writer is gone fails"
                );
                assert_eq!(engine.transcript.messages(), tool_heavy_transcript());

                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::TurnFailed(short),
                            Event::Cancelled,
                            Event::SummaryToken(_),
                            Event::SummaryDiscarded,
                            Event::Error(boundary_write),
                            Event::UsageUpdated(UsageSnapshot {
                                context_tokens: None,
                                estimated_cost_usd: Some(cost),
                                ..
                            }),
                            Event::TurnFailed(unsaved),
                        ] if short == "Compaction failed: the conversation has about 15 tokens, \
                                       below the 1000-token minimum"
                            && boundary_write == "Session write failed: \
                                                  session writer task has stopped. \
                                                  Conversation history may be incomplete."
                            && (cost - (1.23 + 0.006)).abs() < 1e-12
                            && unsaved == "Compaction was not saved, so the transcript is unchanged"
                    ),
                    "only the failure that sent a request shows its cost: {events:?}",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn shows_the_session_cost_after_a_summary_that_reported_no_usage() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let body = sse_body(&[
                    json!({"type": "message_start", "message": {"id": "msg_1", "model": MODEL}}),
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "summary"}}),
                    json!({"type": "content_block_stop", "index": 0}),
                    json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
                    json!({"type": "message_stop"}),
                ]);
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(sse_response(body))
                    .mount(&server)
                    .await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.transcript.set_messages(tool_heavy_transcript());
                engine.usage.set_displayed_usage(TokenUsage::new(90_000, 9));
                engine.usage.set_total_estimated_cost_usd(1.23);
                let sink = CapturingSink::default();

                let end = engine.compact(&sink, &mut inert_ops(), None).await;

                assert_eq!(end, TurnEnd::Completed);
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::SummaryToken(_),
                            Event::SessionCompacted { .. },
                            Event::UsageUpdated(UsageSnapshot {
                                context_tokens: None,
                                estimated_cost_usd: Some(cost),
                                ..
                            }),
                            Event::SessionReady,
                        ] if (cost - 1.23).abs() < 1e-12
                    ),
                    "the cleared context shows beside the unchanged cost: {events:?}",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn fails_while_signed_out_without_sending_a_request() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "summary").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.sign_in = SignIn::SignedOut { retry: false };
                engine.transcript.set_messages(tool_heavy_transcript());
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                let end = engine.compact(&sink, &mut ops, None).await;

                assert_eq!(end, TurnEnd::Failed);
                assert_eq!(engine.transcript.messages(), tool_heavy_transcript());
                assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [Event::TurnFailed(msg)] if msg == "Not signed in. Run `/login` to sign in."
                    ),
                    "{events:?}"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn keeps_a_mode_switch_sent_before_a_cancel() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.transcript.set_messages(tool_heavy_transcript());
                let sink = CapturingSink::default();
                let (ops_tx, mut ops) = mpsc::channel(2);
                ops_tx.try_send(Op::SetMode(Mode::Plan)).unwrap();
                ops_tx.try_send(Op::Interrupt).unwrap();

                let end = engine.compact(&sink, &mut ops, None).await;

                assert_eq!(end, TurnEnd::Cancelled);
                assert_eq!(engine.policy.mode(), Mode::Plan);
            })
            .await;
        }

        #[tokio::test]
        async fn cancelled_mid_summary_discards_it_before_the_cancel() {
            with_pinned_home(async {
                let base_url = serve_stalled_summary("partial").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(base_url, &test_store(dir.path()), dir.path()).await;
                engine.transcript.set_messages(tool_heavy_transcript());
                let sink = CapturingSink::default();
                let (ops_tx, mut ops) = mpsc::channel(1);
                let interrupt_once_streaming = async {
                    while sink.events().is_empty() {
                        tokio::task::yield_now().await;
                    }
                    ops_tx.send(Op::Interrupt).await.unwrap();
                };

                let (end, ()) = tokio::join!(
                    engine.compact(&sink, &mut ops, None),
                    interrupt_once_streaming,
                );

                assert_eq!(end, TurnEnd::Cancelled);
                assert_eq!(engine.transcript.messages(), tool_heavy_transcript());
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::SummaryToken(partial),
                            Event::SummaryDiscarded,
                            Event::UsageUpdated(_),
                            Event::Cancelled,
                        ] if partial == "partial"
                    ),
                    "{events:?}",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn failure_bills_and_shows_the_summary_cost() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(sse_response(
                        text_stream_body("", "partial").replace("end_turn", "max_tokens"),
                    ))
                    .mount(&server)
                    .await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.transcript.set_messages(tool_heavy_transcript());
                engine.usage.set_displayed_usage(TokenUsage::new(50_000, 1));
                engine.usage.set_total_estimated_cost_usd(1.23);
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                let end = engine.compact(&sink, &mut ops, None).await;

                assert_eq!(end, TurnEnd::Failed);
                assert_eq!(engine.transcript.messages(), tool_heavy_transcript());
                assert_eq!(
                    engine.usage.displayed_usage(),
                    Some(TokenUsage::new(50_000, 1))
                );
                let expected = 1.23 + 0.006;
                assert!(
                    (engine.usage.total_estimated_cost_usd() - expected).abs() < 1e-12,
                    "a rejected summary is still billed: {}",
                    engine.usage.total_estimated_cost_usd(),
                );
                let events = sink.events();
                let [
                    Event::SummaryToken(partial),
                    Event::SummaryDiscarded,
                    Event::UsageUpdated(usage),
                    Event::TurnFailed(msg),
                ] = events.as_slice()
                else {
                    panic!(
                        "expected the discarded summary and its cost before the failure: {events:?}"
                    );
                };
                assert_eq!(partial, "partial");
                assert_eq!(usage.context_tokens, Some(50_000));
                assert!((usage.estimated_cost_usd.unwrap() - expected).abs() < 1e-12);
                assert_eq!(
                    msg,
                    "Compaction failed: \
                     the summary hit the `max_tokens` output limit before it finished",
                );
            })
            .await;
        }
    }

    mod replace_with_summary {
        use super::*;

        fn boundary(tasks: Vec<Task>) -> CompactBoundary {
            CompactBoundary {
                record: CompactRecord {
                    summary: "fixed login bug".to_owned(),
                    instructions: None,
                    tasks,
                },
                context: SessionContext::default(),
            }
        }

        #[tokio::test]
        async fn clears_the_tracker_and_carries_the_task_list_over() {
            with_pinned_home(async {
                for trigger in [CompactTrigger::Manual, CompactTrigger::Auto] {
                    let server = MockServer::start().await;
                    let dir = tempfile::tempdir().unwrap();
                    let store = test_store(dir.path());
                    let mut engine = engine(server.uri(), &store, dir.path()).await;
                    let session_id = engine.session_id();
                    engine.transcript.set_messages(tool_heavy_transcript());
                    let read = dir.path().join("read.txt");
                    std::fs::write(&read, "read").unwrap();
                    seed_full_read(&engine.transcript.file_tracker, &read);
                    let sink = CapturingSink::default();
                    let tracker = std::sync::Arc::clone(&engine.transcript.file_tracker);
                    let previous = tracker.snapshot_all();
                    let release = blocked_write(&tracker, &read, b"committed", "toolu_write").await;

                    let compacted = {
                        let mut compact = std::pin::pin!(engine.replace_with_summary(
                            &sink,
                            boundary(plan()),
                            trigger
                        ));
                        poll_until_draining(&tracker, compact.as_mut()).await;
                        assert_writes_draining(&tracker);
                        assert_eq!(std::fs::read(&read).unwrap(), b"read", "{trigger:?}");
                        assert_eq!(tracker.snapshot_all(), previous, "{trigger:?}");
                        assert_eq!(sink.events(), [], "{trigger:?}");
                        release.send(()).unwrap();
                        compact.await
                    };

                    assert!(compacted.is_some(), "{trigger:?}");
                    assert_eq!(std::fs::read(&read).unwrap(), b"committed", "{trigger:?}");
                    assert_eq!(
                        engine.transcript.file_tracker.snapshot_all(),
                        [],
                        "{trigger:?}"
                    );
                    let summary =
                        synthesize_post_compact_message("fixed login bug", &plan(), &[], trigger);
                    assert_eq!(engine.transcript.messages(), [summary], "{trigger:?}");
                    assert!(
                        matches!(
                            sink.events().as_slice(),
                            [Event::SessionCompacted {
                                summary,
                                trigger: actual_trigger,
                                ..
                            }] if summary == "fixed login bug" && *actual_trigger == trigger
                        ),
                        "{trigger:?}: {:?}",
                        sink.events(),
                    );
                    engine.transcript.session().clone().shutdown().await;
                    let saved = store.load_session_data(&session_id).unwrap();
                    assert_eq!(
                        saved.compact.map(|compact| compact.record.tasks),
                        Some(plan()),
                        "{trigger:?}"
                    );
                    assert_eq!(saved.messages, engine.transcript.messages(), "{trigger:?}");
                }
            })
            .await;
        }

        #[tokio::test]
        async fn keeps_the_transcript_and_tracker_when_the_writer_is_gone() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.transcript.set_session(dead_test_session());
                engine.transcript.set_messages(tool_heavy_transcript());
                let read = dir.path().join("read.txt");
                std::fs::write(&read, "read").unwrap();
                seed_full_read(&engine.transcript.file_tracker, &read);
                let tracked = engine.transcript.file_tracker.snapshot_all();
                let sink = CapturingSink::default();

                let compacted = engine
                    .replace_with_summary(&sink, boundary(Vec::new()), CompactTrigger::Auto)
                    .await;

                assert!(compacted.is_none());
                assert_eq!(engine.transcript.messages(), tool_heavy_transcript());
                assert_eq!(engine.transcript.file_tracker.snapshot_all(), tracked);
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [Event::SummaryDiscarded, Event::Error(message)]
                            if message == "Session write failed: \
                                           session writer task has stopped. \
                                           Conversation history may be incomplete."
                    ),
                    "a gone writer discards the summary before its error: {events:?}",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn reports_a_failed_write_after_the_compaction_event() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                let saved = engine
                    .transcript
                    .session()
                    .record_message(Message::user("saved"))
                    .await;
                assert_eq!(saved.report.failure, None);
                fail_writes(&store);
                engine.transcript.set_messages(tool_heavy_transcript());
                let sink = CapturingSink::default();

                let compacted = engine
                    .replace_with_summary(&sink, boundary(Vec::new()), CompactTrigger::Auto)
                    .await;

                assert!(compacted.is_some());
                assert_eq!(
                    engine.transcript.messages(),
                    [synthesize_post_compact_message(
                        "fixed login bug",
                        &[],
                        &[],
                        CompactTrigger::Auto
                    )]
                );
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [Event::SessionCompacted { .. }, Event::Error(failure)]
                            if failure.starts_with("Session write failed: ")
                                && failure.contains("disk full")
                    ),
                    "the failure follows the event that clears the chat: {events:?}",
                );
                engine.transcript.session().clone().shutdown().await;
            })
            .await;
        }

        #[tokio::test]
        async fn swaps_in_memory_once_its_write_stops_persistence() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                fail_writes(&store);
                let unsaved = engine
                    .transcript
                    .session()
                    .record_message(Message::user("unsaved"))
                    .await;
                assert!(unsaved.report.failure.is_some());
                engine.transcript.set_messages(tool_heavy_transcript());
                let summary = "s".repeat(2 << 20);
                let mut large = boundary(Vec::new());
                large.record.summary.clone_from(&summary);
                let sink = CapturingSink::default();

                let compacted = engine
                    .replace_with_summary(&sink, large, CompactTrigger::Auto)
                    .await;

                assert!(compacted.is_some());
                assert_eq!(
                    engine.transcript.messages(),
                    [synthesize_post_compact_message(
                        &summary,
                        &[],
                        &[],
                        CompactTrigger::Auto
                    )]
                );
                let events = sink.events();
                assert!(
                    matches!(events.as_slice(), [Event::SessionCompacted { .. }]),
                    "the stop already surfaced, so the compaction reports no error: {events:?}",
                );
                engine.transcript.session().clone().shutdown().await;
            })
            .await;
        }
    }

    mod request_summary {
        use super::*;

        fn streamed_summary_body(text: &str) -> String {
            summary_stream("", text, "end_turn", true)
        }

        /// SSE body for one summary reply. `finished: false` cuts the stream before `message_stop`.
        fn summary_stream(
            start_text: &str,
            delta_text: &str,
            stop_reason: &str,
            finished: bool,
        ) -> String {
            let mut frames = vec![
                json!({"type": "message_start", "message": {"id": "m", "model": "claude-haiku-5-5", "usage": {"input_tokens": 100, "output_tokens": 1}}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": start_text}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": delta_text}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 7}}),
            ];
            if finished {
                frames.push(json!({"type": "message_stop"}));
            }
            sse_body(&frames)
        }

        async fn mount_summary(server: &MockServer, body: String) {
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(sse_response(body))
                .mount(server)
                .await;
        }

        /// The session every summary request in these tests is attributed to.
        const SESSION_ID: &str = "parent-session";

        /// The credential every summary request in these tests sends.
        static API_KEY: LazyLock<Auth> = LazyLock::new(api_key);

        /// The settings every summary request in these tests sends under.
        static SETTINGS: LazyLock<ModelSettings> =
            LazyLock::new(|| ModelSettings::test("claude-haiku-5-5"));

        /// The conversation a summary forks, holding `messages` under its own system prompt and
        /// user context.
        fn parent(messages: Vec<Message>) -> LiveTranscript {
            let mut parent = LiveTranscript::with_messages(&dead_test_session(), messages);
            let prompt = parent.prompt_mut();
            prompt.system.stable = "You are the parent conversation.".to_owned();
            "Project instructions.".clone_into(&mut prompt.user_context);
            parent
        }

        async fn summarize(client: &Client, transcript: &[Message]) -> Result<String> {
            summarize_streaming(client, transcript).await.0
        }

        /// Summarizes `transcript` like [`summarize`], also returning the events it streamed.
        async fn summarize_streaming(
            client: &Client,
            transcript: &[Message],
        ) -> (Result<String>, Vec<Event>) {
            let sink = CapturingSink::default();
            let summary = request_summary(
                client,
                conversation_request(
                    SESSION_ID,
                    &API_KEY,
                    &SETTINGS,
                    &parent(transcript.to_vec()),
                    &[],
                ),
                None,
                &sink,
                &mut Vec::new(),
            )
            .await;
            (summary, sink.events())
        }

        fn token(text: &str) -> Event {
            Event::SummaryToken(text.to_owned())
        }

        fn strip_cache_control(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    map.remove("cache_control");
                    map.values_mut().for_each(strip_cache_control);
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(strip_cache_control),
                _ => {}
            }
        }

        /// Transcript with a large tool result in its oldest round and thinking in its kept round.
        fn overflowing_transcript() -> Vec<Message> {
            vec![
                Message::user("fix the bug"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::text("reading"),
                        ContentBlock::ToolUse {
                            id: "t1".to_owned(),
                            name: "read".to_owned(),
                            input: json!({"path": "/tmp/a"}),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "t1".to_owned(),
                        content: "a".repeat(100_000),
                        is_error: false,
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Thinking {
                            thinking: "the bug is in auth".to_owned(),
                            replay: Replay::with_model(MODEL, "sig"),
                        },
                        ContentBlock::text("found it"),
                    ],
                },
                Message::user("any progress?"),
                Message::assistant("fixed"),
            ]
        }

        async fn mount_overflow_then(server: &MockServer, retry_body: String) {
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(sse_response(summary_stream(
                    "",
                    "partial",
                    "model_context_window_exceeded",
                    true,
                )))
                .up_to_n_times(1)
                .mount(server)
                .await;
            mount_summary(server, retry_body).await;
        }

        /// Transcript of `rounds` exchanges with user messages of about 10,000 rough tokens each.
        fn heavy_rounds(rounds: usize) -> Vec<Message> {
            (0..rounds)
                .flat_map(|i| {
                    [
                        Message::user("a".repeat(40_000)),
                        Message::assistant(format!("reply {i}")),
                    ]
                })
                .collect()
        }

        fn prompt_too_long(message: &str) -> Mock {
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                    "type": "error",
                    "error": {"type": "invalid_request_error", "message": message},
                })))
        }

        /// Messages a request sent after its user-context message, without cache breakpoints.
        fn sent_messages(request: &HttpRequest) -> Vec<serde_json::Value> {
            let mut body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            strip_cache_control(&mut body);
            body["messages"].as_array().unwrap()[1..].to_vec()
        }

        /// Replies with a stream the test feeds, which stays open while the test holds its sender.
        struct HeldStreamClient(Mutex<Option<mpsc::Receiver<Result<ResponseEvent>>>>);

        impl Wire for HeldStreamClient {
            fn stream(
                &self,
                _request: Request<'_>,
            ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
                Ok(self.0.lock().unwrap().take().expect("one request"))
            }
        }

        #[tokio::test]
        async fn trims_the_whitespace_around_the_summary() {
            let server = MockServer::start().await;
            mount_summary(&server, streamed_summary_body("  fixed login bug  \n")).await;
            let client = test_client(server.uri());

            let summary = summarize(&client, &tool_heavy_transcript()).await.unwrap();

            assert_eq!(summary, "fixed login bug");
        }

        #[tokio::test]
        async fn keeps_the_text_that_opens_the_reply() {
            let server = MockServer::start().await;
            mount_summary(
                &server,
                summary_stream("  fixed", " login bug  \n", "end_turn", true),
            )
            .await;
            let client = test_client(server.uri());

            let (summary, events) = summarize_streaming(&client, &tool_heavy_transcript()).await;

            assert_eq!(summary.unwrap(), "fixed login bug");
            assert_eq!(
                events,
                [token("  fixed"), token(" login bug  \n")],
                "the kept summary streams untrimmed and is not discarded",
            );
        }

        #[tokio::test]
        async fn summary_leaves_out_the_reply_thinking() {
            let server = MockServer::start().await;
            mount_summary(
                &server,
                sse_body(&[
                    json!({"type": "message_start", "message": {"id": "m", "model": "claude-haiku-5-5"}}),
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "draft notes"}}),
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
                    json!({"type": "content_block_stop", "index": 0}),
                    json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
                    json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "fixed login bug"}}),
                    json!({"type": "content_block_stop", "index": 1}),
                    json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
                    json!({"type": "message_stop"}),
                ]),
            )
            .await;
            let client = test_client(server.uri());

            let (summary, events) = summarize_streaming(&client, &tool_heavy_transcript()).await;

            assert_eq!(summary.unwrap(), "fixed login bug");
            assert_eq!(events, [token("fixed login bug")]);
        }

        #[tokio::test]
        async fn request_forks_the_conversation_prefix() {
            let server = MockServer::start().await;
            mount_summary(&server, streamed_summary_body("done")).await;
            let client = test_client(server.uri());
            let conversation = parent(tool_heavy_transcript());
            let tools = [&ToolDefinition {
                name: "read".to_owned(),
                description: "Read a file.".to_owned(),
                input_schema: json!({"type": "object"}),
            }];
            let next_round =
                conversation_request(SESSION_ID, &API_KEY, &SETTINGS, &conversation, &tools);

            let mut rx = client.stream(next_round).unwrap();
            while rx.recv().await.is_some() {}
            request_summary(
                &client,
                next_round,
                Some("focus on auth"),
                &CapturingSink::default(),
                &mut Vec::new(),
            )
            .await
            .unwrap();

            let received = server.received_requests().await.expect("recorded requests");
            let session_ids: Vec<_> = received
                .iter()
                .map(|request| {
                    request.headers["x-claude-code-session-id"]
                        .to_str()
                        .unwrap()
                })
                .collect();
            assert_eq!(session_ids, [SESSION_ID; 2]);
            let mut bodies = request_bodies(&server).await;
            let last_block = |body: &serde_json::Value| {
                let messages = body["messages"].as_array().unwrap();
                let content = messages.last().unwrap()["content"].as_array().unwrap();
                content.last().unwrap()["cache_control"].clone()
            };
            assert_eq!(
                last_block(&bodies[1]),
                json!({"type": "ephemeral", "ttl": "1h"}),
                "the fork marks its tail to read the conversation's cached prefix",
            );
            bodies.iter_mut().for_each(strip_cache_control);
            let [parent, fork] = bodies.as_slice() else {
                panic!("expected two requests, got {}", bodies.len());
            };
            assert_eq!(fork["metadata"], parent["metadata"]);
            // Skips the billing block, whose `cch` hashes the body and so differs per request.
            let system =
                |body: &serde_json::Value| body["system"].as_array().unwrap()[1..].to_vec();
            assert_eq!(system(fork), system(parent));
            assert_eq!(fork["tools"], parent["tools"]);
            let rubric = json!({
                "role": "user",
                "content": [{"type": "text", "text": build_user_message(Some("focus on auth"))}],
            });
            let expected: Vec<_> = parent["messages"]
                .as_array()
                .unwrap()
                .iter()
                .cloned()
                .chain([rubric])
                .collect();
            assert_eq!(fork["messages"].as_array().unwrap(), &expected);
        }

        #[tokio::test]
        async fn retries_an_overflow_without_the_oldest_rounds_or_thinking() {
            let server = MockServer::start().await;
            mount_overflow_then(&server, streamed_summary_body("trimmed summary")).await;
            let client = test_client(server.uri());
            let sink = CapturingSink::default();
            let mut spent = Vec::new();
            let conversation = parent(overflowing_transcript());

            let summary = request_summary(
                &client,
                conversation_request(SESSION_ID, &API_KEY, &SETTINGS, &conversation, &[]),
                None,
                &sink,
                &mut spent,
            )
            .await
            .unwrap();

            assert_eq!(summary, "trimmed summary");
            assert_eq!(
                sink.events(),
                [
                    token("partial"),
                    Event::SummaryDiscarded,
                    token("trimmed summary")
                ],
                "the retry starts the summary over",
            );
            let received = server.received_requests().await.expect("recorded requests");
            assert_eq!(
                received[1].headers["x-claude-code-session-id"], SESSION_ID,
                "the retry stays attributed to the conversation's session",
            );
            let [_, retry] = request_bodies(&server).await.try_into().unwrap();
            assert!(
                !retry["messages"].to_string().contains("cache_control"),
                "no later request extends the retry, so its transcript carries no breakpoint",
            );
            assert_eq!(
                retry["messages"],
                json!([
                    {"role": "user", "content": [{"type": "text", "text": "Project instructions."}]},
                    {"role": "user", "content": [{"type": "text", "text": RETRY_TRUNCATION_MARKER}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "found it"}]},
                    {"role": "user", "content": [{"type": "text", "text": "any progress?"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "fixed"}]},
                    {"role": "user", "content": [{"type": "text", "text": build_user_message(None)}]},
                ]),
            );
            assert_eq!(
                spent,
                [TokenUsage::new(100, 7); 2],
                "both requests are billed"
            );
        }

        #[tokio::test]
        async fn retries_a_prompt_too_long_rejection_with_room_for_the_summary() {
            let server = MockServer::start().await;
            prompt_too_long("prompt is too long: 205000 tokens > 200000 maximum")
                .up_to_n_times(1)
                .mount(&server)
                .await;
            mount_summary(&server, streamed_summary_body("trimmed summary")).await;
            let client = test_client(server.uri());
            let transcript = heavy_rounds(4);

            let (summary, events) = summarize_streaming(&client, &transcript).await;

            assert_eq!(summary.unwrap(), "trimmed summary");
            assert_eq!(
                events,
                [token("trimmed summary")],
                "a rejection streams nothing to discard",
            );
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2);
            // Freeing the 5,000-token overflow and summary reserve takes the first five messages.
            let expected: Vec<Message> = iter::once(Message::user(RETRY_TRUNCATION_MARKER))
                .chain(transcript[5..].iter().cloned())
                .chain([Message::user(build_user_message(None))])
                .collect();
            assert_eq!(sent_messages(&requests[1]), to_json(&expected));
        }

        #[tokio::test]
        async fn retry_closes_a_tool_turn_the_kept_rounds_leave_open() {
            let tool_call = ContentBlock::ToolUse {
                id: "t1".to_owned(),
                name: "read".to_owned(),
                input: json!({"path": "/tmp/a"}),
            };
            let tool_result = Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_owned(),
                    content: "file body".to_owned(),
                    is_error: false,
                }],
            };
            for interrupted in [false, true] {
                let server = MockServer::start().await;
                mount_overflow_then(&server, streamed_summary_body("trimmed summary")).await;
                let client = test_client(server.uri());
                let mut transcript = vec![
                    Message::user("a".repeat(100_000)),
                    Message::assistant("reply 0"),
                    Message::user("check the file"),
                    Message {
                        role: Role::Assistant,
                        content: vec![
                            ContentBlock::Thinking {
                                thinking: "read it first".to_owned(),
                                replay: Replay::with_model(MODEL, "sig"),
                            },
                            ContentBlock::text("reading"),
                            tool_call.clone(),
                        ],
                    },
                    tool_result.clone(),
                ];
                transcript.extend(interrupted.then(Message::interruption));

                summarize(&client, &transcript).await.unwrap();

                let requests = server.received_requests().await.unwrap();
                let mut expected = vec![
                    Message::user(RETRY_TRUNCATION_MARKER),
                    Message::assistant("reply 0"),
                    Message::user("check the file"),
                    Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::text("reading"), tool_call.clone()],
                    },
                    tool_result.clone(),
                ];
                expected.extend(
                    interrupted.then(|| Message::user(crate::message::INTERRUPTION_MARKER)),
                );
                expected.extend([
                    Message::assistant(RETRY_TURN_CLOSE_MARKER),
                    Message::user(build_user_message(None)),
                ]);
                assert_eq!(
                    sent_messages(&requests[1]),
                    to_json(&expected),
                    "interrupted: {interrupted}",
                );
            }
        }

        #[tokio::test]
        async fn fails_when_the_newest_round_alone_still_overflows() {
            let server = MockServer::start().await;
            mount_overflow_then(
                &server,
                summary_stream("", "partial", "model_context_window_exceeded", true),
            )
            .await;
            let client = test_client(server.uri());
            let sink = CapturingSink::default();
            let mut spent = Vec::new();

            let err = request_summary(
                &client,
                conversation_request(
                    SESSION_ID,
                    &API_KEY,
                    &SETTINGS,
                    &parent(overflowing_transcript()),
                    &[],
                ),
                None,
                &sink,
                &mut spent,
            )
            .await
            .expect_err("an overflow with no round left to drop fails the compaction");

            assert_eq!(
                format!("{err:#}"),
                "the transcript left no room in the context window for the summary"
            );
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 3);
            assert_eq!(
                sent_messages(&requests[2]),
                to_json(&[
                    Message::user(RETRY_TRUNCATION_MARKER),
                    Message::assistant("fixed"),
                    Message::user(build_user_message(None)),
                ]),
            );
            assert_eq!(
                spent,
                [TokenUsage::new(100, 7); 3],
                "every request is billed"
            );
            assert_eq!(
                sink.events(),
                vec![vec![token("partial"), Event::SummaryDiscarded]; 3].concat(),
                "each retry and the failure discard what streamed",
            );
        }

        #[tokio::test]
        async fn gives_up_after_three_overflow_retries() {
            let server = MockServer::start().await;
            prompt_too_long("prompt is too long: 200001 tokens > 200000 maximum")
                .mount(&server)
                .await;
            let client = test_client(server.uri());

            let err = summarize(&client, &heavy_rounds(8))
                .await
                .expect_err("every retry overflows");

            assert_eq!(
                format!("{err:#}"),
                "Anthropic API error (HTTP 400 Bad Request): \
                 prompt is too long: 200001 tokens > 200000 maximum"
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 4);
        }

        #[tokio::test]
        async fn refuses_an_empty_or_short_session_without_a_request() {
            let server = MockServer::start().await;
            let client = test_client(server.uri());
            let short = vec![
                Message::user("fix the bug"),
                Message::assistant("looking now"),
                Message::user("any progress?"),
                Message::assistant("found it"),
            ];

            for (transcript, expected) in [
                (Vec::new(), "the session has no messages yet"),
                (
                    short,
                    "the conversation has about 65 tokens, below the 1000-token minimum",
                ),
            ] {
                let err = summarize(&client, &transcript)
                    .await
                    .expect_err("the session is refused");

                assert_eq!(format!("{err:#}"), expected);
            }
            assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
        }

        #[tokio::test]
        async fn rejects_an_empty_summary() {
            let server = MockServer::start().await;
            mount_summary(&server, streamed_summary_body("   \n  ")).await;
            let client = test_client(server.uri());

            let (summary, events) = summarize_streaming(&client, &tool_heavy_transcript()).await;

            let err = summary.expect_err("empty summary must error");
            assert_eq!(format!("{err:#}"), "the model returned an empty summary");
            assert_eq!(events, [token("   \n  "), Event::SummaryDiscarded]);
        }

        #[tokio::test]
        async fn rejects_an_unfinished_summary_and_still_records_usage() {
            let cases = [
                (
                    "max_tokens",
                    true,
                    "the summary hit the `max_tokens` output limit before it finished",
                ),
                (
                    "refusal",
                    true,
                    "the model declined to summarize the conversation",
                ),
                (
                    "tool_use",
                    true,
                    "the model called a tool before finishing the summary",
                ),
                ("pause_turn", true, "the summary stopped early (pause_turn)"),
                (
                    "end_turn",
                    false,
                    "Anthropic API stream ended before message_stop",
                ),
            ];
            for (stop_reason, finished, expected) in cases {
                let server = MockServer::start().await;
                mount_summary(
                    &server,
                    summary_stream("", "partial", stop_reason, finished),
                )
                .await;
                let client = test_client(server.uri());
                let sink = CapturingSink::default();
                let mut spent = Vec::new();

                let err = request_summary(
                    &client,
                    conversation_request(
                        SESSION_ID,
                        &API_KEY,
                        &SETTINGS,
                        &parent(tool_heavy_transcript()),
                        &[],
                    ),
                    None,
                    &sink,
                    &mut spent,
                )
                .await
                .expect_err("an unfinished summary must not replace the transcript");

                assert_eq!(format!("{err:#}"), expected, "{stop_reason}");
                assert_eq!(spent, [TokenUsage::new(100, 7)], "{stop_reason}");
                assert_eq!(
                    sink.events(),
                    [token("partial"), Event::SummaryDiscarded],
                    "{stop_reason}",
                );
            }
        }

        #[tokio::test]
        async fn dropped_mid_summary_discards_what_streamed() {
            let (tx, rx) = mpsc::channel(1);
            tx.send(Ok(ResponseEvent::Text("partial".to_owned())))
                .await
                .unwrap();
            let client = HeldStreamClient(Mutex::new(Some(rx)));
            let conversation = parent(tool_heavy_transcript());
            let sink = CapturingSink::default();
            let mut spent = Vec::new();

            {
                let mut compaction = pin!(request_summary(
                    &client,
                    conversation_request(SESSION_ID, &API_KEY, &SETTINGS, &conversation, &[]),
                    None,
                    &sink,
                    &mut spent,
                ));
                assert!(futures::poll!(compaction.as_mut()).is_pending());
                assert_eq!(sink.events(), [token("partial")]);
            }

            assert_eq!(sink.events(), [token("partial"), Event::SummaryDiscarded]);
        }

        #[tokio::test]
        async fn passes_retry_notices_and_usage_limits_through() {
            let notice = "HTTP 529 overloaded_error. Retrying in 1 s (retry 1 of 5).";
            let limits = five_hour_limit();
            let (tx, rx) = mpsc::channel(4);
            for event in [
                ResponseEvent::Retrying(notice.to_owned()),
                ResponseEvent::RateLimits(limits),
                ResponseEvent::Text("Recap".to_owned()),
                ResponseEvent::Completed(Response {
                    content: vec![ContentBlock::text("Recap")],
                    ..Response::default()
                }),
            ] {
                tx.send(Ok(event)).await.unwrap();
            }
            let client = HeldStreamClient(Mutex::new(Some(rx)));
            let sink = CapturingSink::default();

            let summary = request_summary(
                &client,
                conversation_request(
                    SESSION_ID,
                    &API_KEY,
                    &SETTINGS,
                    &parent(tool_heavy_transcript()),
                    &[],
                ),
                None,
                &sink,
                &mut Vec::new(),
            )
            .await
            .unwrap();

            assert_eq!(summary, "Recap");
            assert_eq!(
                sink.events(),
                [
                    Event::Retrying(notice.to_owned()),
                    Event::RateLimitsUpdated(limits),
                    token("Recap"),
                ],
            );
        }

        #[tokio::test]
        async fn surfaces_stream_error_event() {
            let server = MockServer::start().await;
            mount_summary(
                &server,
                sse_body(&[
                    json!({"type": "message_start", "message": {"id": "m", "model": "claude-haiku-5-5"}}),
                    json!({"type": "error", "error": {"type": "invalid_request_error", "message": "summary request rejected"}}),
                ]),
            )
            .await;

            let client = test_client(server.uri());
            let err = summarize(&client, &tool_heavy_transcript())
                .await
                .expect_err("in-band error must propagate");
            assert_eq!(
                format!("{err:#}"),
                "Anthropic API error (invalid_request_error): summary request rejected"
            );
        }

        #[tokio::test]
        async fn propagates_http_error() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(400).set_body_string(
                    r#"{"type":"error","error":{"type":"invalid_request_error","message":"summary request rejected"}}"#,
                ))
                .mount(&server)
                .await;

            let client = test_client(server.uri());
            let err = summarize(&client, &tool_heavy_transcript())
                .await
                .expect_err("expected error");
            assert_eq!(
                format!("{err:#}"),
                "Anthropic API error (HTTP 400 Bad Request): summary request rejected"
            );
        }
    }

    mod drop_oldest_rounds {
        use super::*;

        fn server_call(id: &str) -> ContentBlock {
            ContentBlock::ServerToolUse {
                id: id.to_owned(),
                name: "web_search".to_owned(),
                input: json!({}),
            }
        }

        fn server_result(id: &str) -> ContentBlock {
            ContentBlock::ServerToolResult {
                tool_use_id: id.to_owned(),
                kind: "web_search_tool_result".to_owned(),
                fields: serde_json::Map::new(),
            }
        }

        fn reply(content: Vec<ContentBlock>) -> Message {
            Message {
                role: Role::Assistant,
                content,
            }
        }

        #[test]
        fn keeps_the_newest_round_when_the_gap_exceeds_the_transcript() {
            let kept = drop_oldest_rounds(&tool_heavy_transcript(), u32::MAX).unwrap();

            assert_eq!(
                kept,
                [
                    truncation_marker(),
                    Message::assistant("alpha-end, beta-end"),
                ],
            );
        }

        #[test]
        fn replaces_an_earlier_truncation_marker() {
            let transcript = [
                truncation_marker(),
                Message::assistant("older"),
                Message::user("next"),
                Message::assistant("newer"),
            ];

            let kept = drop_oldest_rounds(&transcript, 0).unwrap();

            assert_eq!(
                to_json(&kept),
                to_json(&[truncation_marker(), Message::assistant("newer")]),
            );
        }

        #[test]
        fn keeps_a_deferred_server_call_with_its_result() {
            let transcript = [
                Message::user("search the web and list the files"),
                reply(vec![
                    server_call("s1"),
                    ContentBlock::ToolUse {
                        id: "t1".to_owned(),
                        name: "bash".to_owned(),
                        input: json!({"command": "ls"}),
                    },
                ]),
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "t1".to_owned(),
                        content: "f.rs".to_owned(),
                        is_error: false,
                    }],
                },
                reply(vec![server_result("s1"), ContentBlock::text("Found it.")]),
            ];

            let kept = drop_oldest_rounds(&transcript, u32::MAX).unwrap();

            assert_eq!(kept[0], truncation_marker());
            assert_eq!(kept[1..], transcript[1..]);
        }

        #[test]
        fn keeps_a_paused_server_call_with_its_continuation() {
            let transcript = [
                Message::user("research X"),
                reply(vec![ContentBlock::text("Searching."), server_call("s1")]),
                reply(vec![server_result("s1"), ContentBlock::text("Found X.")]),
            ];

            let kept = drop_oldest_rounds(&transcript, u32::MAX).unwrap();

            assert_eq!(kept[0], truncation_marker());
            assert_eq!(kept[1..], transcript[1..]);
        }

        #[test]
        fn needs_a_reply_to_keep() {
            let thinking_only = Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Thinking {
                    thinking: "hmm".to_owned(),
                    replay: Replay::with_model(MODEL, "sig"),
                }],
            };

            assert!(drop_oldest_rounds(&[Message::user("one"), thinking_only], 0).is_none());
        }
    }

    mod is_truncation_marker {
        use super::*;

        #[test]
        fn identifies_generated_markers_without_claiming_literal_user_text() {
            assert!(is_truncation_marker(&truncation_marker()));
            assert!(!is_truncation_marker(&Message::user(
                RETRY_TRUNCATION_MARKER
            )));
            assert!(!is_truncation_marker(&Message::assistant(
                RETRY_TRUNCATION_MARKER
            )));
        }
    }

    mod request_tokens {
        use super::*;

        #[test]
        fn counts_the_prompt_the_tools_and_the_transcript_at_four_bytes_a_token() {
            let mut prompt = PromptParts::default();
            prompt.system.stable = "s".repeat(4_000);
            prompt.system.dynamic = "d".repeat(400);
            prompt.user_context = "u".repeat(4_000);
            let tools = [&ToolDefinition {
                name: "read".to_owned(),
                description: "r".repeat(3_948),
                input_schema: json!({}),
            }];
            let transcript = [Message::user("hi")];

            let tokens = request_tokens(&prompt, &tools, &transcript);

            assert_eq!(tokens, 2_100 + 1_000 + transcript_tokens(&transcript));
        }
    }

    mod build_user_message {
        use super::*;

        #[test]
        fn without_instructions_is_just_the_rubric() {
            for instructions in [None, Some("   \n\t  ")] {
                let message = build_user_message(instructions);

                assert_eq!(message, SUMMARIZATION_USER_RUBRIC, "{instructions:?}");
            }
        }

        #[test]
        fn with_instructions_appends_them_under_an_anchor() {
            let message = build_user_message(Some("  focus on the build error\n"));

            assert_eq!(
                message,
                formatdoc! {"
                    {SUMMARIZATION_USER_RUBRIC}

                    Additional instructions from the engineer:

                    focus on the build error"
                },
            );
        }
    }

    mod summarization_user_rubric {
        use super::*;

        #[test]
        fn asks_which_checks_ran() {
            assert!(
                SUMMARIZATION_USER_RUBRIC.contains("which checks passed, failed, or have not run"),
                "{SUMMARIZATION_USER_RUBRIC}"
            );
        }

        #[test]
        fn ends_by_forbidding_tool_calls() {
            assert!(
                SUMMARIZATION_USER_RUBRIC.ends_with(
                    "Do not call any tools while writing this summary. Respond with text only."
                ),
                "{SUMMARIZATION_USER_RUBRIC}"
            );
        }
    }
}
