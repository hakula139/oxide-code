//! Conversation compaction: the auto-compaction trigger and the summarization request.
//!
//! The summarization request forks the live conversation. It sends the conversation's own system
//! prompt, user context, tools, and transcript, then appends the rubric as the last user message,
//! so it reads the conversation's prompt cache and passes earlier thinking blocks back unchanged. A
//! summary that overflows the context window, as an HTTP 400 `prompt is too long` or a
//! `model_context_window_exceeded` stop, retries up to three times without the oldest rounds or
//! any thinking block, which misses the cache. The summary's text streams to the frontend as it
//! generates. `compact_session` leaves the session untouched, and its callers swap in the summary
//! through `compact_boundary` once it succeeds.

use std::collections::HashSet;
use std::iter;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow, bail};
use indoc::{formatdoc, indoc};
use tokio::sync::mpsc;
use tracing::warn;

use super::abort::{AbortResult, Inbox, TurnAbort, await_unless_aborted};
use super::compact_boundary::{self, Boundary};
use super::turn::collect_response;
use crate::client::{PromptTooLong, Response, ResponseEvent, StopReason, TokenUsage, Wire};
use crate::config::compaction::AutoCompactionConfig;
use crate::file_tracker::FileTracker;
use crate::message::{ContentBlock, Message, Role};
use crate::prompt::PromptParts;
use crate::protocol::{Event, EventSink, Op};
use crate::session::handle::SessionHandle;
use crate::tool::ToolDefinition;
use crate::tool::bash::background::BackgroundShells;
use crate::tool::tasks::Task;

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

/// User-message rubric. Five short asks keep the summary compact without named sections. The
/// request still carries the conversation's tools, so the last sentence keeps the model from
/// calling one.
const SUMMARIZATION_USER_RUBRIC: &str = indoc! {"
    Summarize the conversation above so another instance of yourself can pick up where this one left
    off. Capture, in this order:

    1. The engineer's overall intent and any constraints they stated.
    2. Key technical decisions made and why.
    3. Files, functions, and code paths touched (full paths when known).
    4. Current state: what is done, what is in progress, what is blocked.
    5. The next concrete step, if one is obvious.

    Be concise. Terse bullets beat paragraphs. Preserve exact identifiers, file paths, error strings,
    and command lines verbatim. Output only the summary, without addressing the engineer or asking
    questions. Do not call any tools while writing this summary. Respond with text only.
"};

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

    /// Counts a failure and reports [`Event::AutoCompactionPaused`] if it pauses auto-compaction.
    fn record(&mut self, sink: &dyn EventSink) {
        self.0 += 1;
        if self.is_paused() {
            sink.emit(
                Event::AutoCompactionPaused { failures: self.0 },
                "auto-compaction-paused",
            );
        }
    }

    /// Clears the count and reports [`Event::AutoCompactionResumed`] if auto-compaction was paused.
    pub(super) fn reset(&mut self, sink: &dyn EventSink) {
        if std::mem::take(self).is_paused() {
            sink.emit(Event::AutoCompactionResumed, "auto-compaction-resumed");
        }
    }
}

pub(super) struct AutoCompact<'a> {
    pub(super) config: AutoCompactionConfig,
    pub(super) failures: &'a mut AutoCompactFailures,
    pub(super) file_tracker: &'a FileTracker,
    /// The conversation's request prefix, which the summarizer reuses to read its prompt cache.
    pub(super) prompt: &'a PromptParts,
    pub(super) tools: &'a [ToolDefinition],
    /// The task list the summary message carries over.
    pub(super) tasks: &'a [Task],
    /// The background shells whose open ones the summary message lists.
    pub(super) shells: &'a BackgroundShells,
    /// Each summary request's usage, recorded even when the summary fails or the user cancels it.
    pub(super) spent: &'a mut Vec<TokenUsage>,
}

/// Compacts `messages` when `usage` crosses the threshold, returning whether it did. Skips without
/// auto state or usage, below the threshold, while [`AutoCompactFailures`] has paused
/// auto-compaction, or when the transcript is too short for a summary to shrink it. A failed
/// summary or boundary write counts as a failure and fails the prompt before it is sent.
#[expect(
    clippy::too_many_arguments,
    reason = "auto-compaction needs the same live turn state as manual compaction"
)]
pub(super) async fn auto_compact_if_needed(
    client: &dyn Wire,
    session: &SessionHandle,
    messages: &mut Vec<Message>,
    sink: &dyn EventSink,
    ops: &mut mpsc::Receiver<Op>,
    inbox: &mut Inbox,
    auto: Option<&mut AutoCompact<'_>>,
    usage: Option<TokenUsage>,
) -> AbortResult<bool> {
    let Some(auto) = auto else {
        return Ok(false);
    };
    let Some(usage) = usage else {
        return Ok(false);
    };
    if auto.failures.is_paused()
        || !auto.config.should_trigger(usage.total_tokens())
        || transcript_tokens(messages) < MIN_TOKENS_FOR_COMPACT
    {
        return Ok(false);
    }

    sink.emit(Event::AutoCompactionStarted, "auto-compaction-started");
    let summary = match await_unless_aborted(
        compact_session(
            client,
            auto.prompt,
            auto.tools,
            messages,
            None,
            sink,
            auto.spent,
        ),
        ops,
        inbox,
        sink,
    )
    .await?
    {
        Ok(summary) => summary,
        Err(e) => {
            warn!("auto-compaction failed: {e:#}");
            return Err(record_auto_compact_failure(
                auto,
                sink,
                &format!("Auto-compaction failed: {e:#}"),
            ));
        }
    };
    let compacted = compact_boundary::replace_session_with_summary(
        session,
        auto.file_tracker,
        messages,
        sink,
        Boundary {
            summary,
            instructions: None,
            tasks: auto.tasks.to_vec(),
            shells: auto.shells.list(),
            automatic: true,
        },
    )
    .await;
    if !compacted {
        return Err(record_auto_compact_failure(
            auto,
            sink,
            "Auto-compaction was not saved",
        ));
    }
    Ok(true)
}

/// Counts a failed attempt and returns the abort that fails the unsent prompt. `headline` can end
/// on a cause's own sentence, so its period is dropped before the sentences on what to do next.
fn record_auto_compact_failure(
    auto: &mut AutoCompact<'_>,
    sink: &dyn EventSink,
    headline: &str,
) -> TurnAbort {
    auto.failures.record(sink);
    let next = if auto.failures.is_paused() {
        format!(
            "The prompt was not sent, and auto-compaction is paused after \
             {MAX_AUTO_COMPACT_FAILURES} failures in a row. Send the prompt again to run it \
             without compaction, or run /compact to retry compaction first."
        )
    } else {
        "The prompt was not sent, so send it again to retry.".to_owned()
    };
    TurnAbort::Failed(anyhow!("{}. {next}", headline.trim_end_matches('.')))
}

/// Drives the summarization request and returns the trimmed summary. `prompt` and `tools` must be
/// the ones the conversation's own requests send. `spent` receives the usage of every request, even
/// when the summary fails or the caller drops the future.
///
/// The summary streams to `sink` as [`Event::SummaryToken`]s. A retry, a failure, or a dropped
/// future ends them with [`Event::SummaryDiscarded`]. On success the streamed text stays up, and
/// the caller must end it with [`Event::SessionCompacted`] or [`Event::SummaryDiscarded`].
///
/// Errors when the transcript is empty or too short to compact, when a request fails, or when the
/// reply is empty or stopped before the summary finished, including when the last retry still
/// overflows.
pub(super) async fn compact_session(
    client: &dyn Wire,
    prompt: &PromptParts,
    tools: &[ToolDefinition],
    transcript: &[Message],
    instructions: Option<&str>,
    sink: &dyn EventSink,
    spent: &mut Vec<TokenUsage>,
) -> Result<String> {
    if transcript.is_empty() {
        bail!("the session has no messages yet");
    }
    let tokens = transcript_tokens(transcript);
    if tokens < MIN_TOKENS_FOR_COMPACT {
        bail!(
            "the conversation has about {tokens} tokens, below the \
             {MIN_TOKENS_FOR_COMPACT}-token minimum"
        );
    }

    let sections: Vec<&str> = prompt.system_sections.iter().map(String::as_str).collect();
    let user_context = Some(prompt.user_context.as_str());
    let rubric = Message::user(build_user_message(instructions));
    let request: Vec<Message> = transcript.iter().chain([&rubric]).cloned().collect();
    let stream = SummaryStream::new(sink);
    let mut rx = client.stream_message(&request, &sections, user_context, tools)?;
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
        let request = retry_request(&kept, &rubric);
        rx = client.stream_one_shot(&request, &sections, user_context, tools)?;
    };
    match response.stop_reason {
        None | Some(StopReason::EndTurn | StopReason::StopSequence) => {}
        Some(StopReason::MaxTokens) => {
            bail!("the summary hit the max_tokens output limit before it finished")
        }
        Some(StopReason::ModelContextWindowExceeded) => {
            bail!("the transcript left no room in the context window for the summary")
        }
        Some(StopReason::Refusal) => bail!("the model declined to summarize the conversation"),
        Some(StopReason::ToolUse) => bail!("the model called a tool before finishing the summary"),
        Some(reason @ (StopReason::PauseTurn | StopReason::Unknown)) => {
            bail!("the summary stopped early ({reason:?})")
        }
    }

    let summary: String = response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
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
/// of the reply, which belongs to no visible turn. Dropping it discards the streamed text, so a cancelled
/// compaction clears it.
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

/// Adds one request's usage to the running total on drop, so a failed or cancelled request still
/// counts.
struct RequestUsage<'a> {
    total: &'a mut Vec<TokenUsage>,
    request: Option<TokenUsage>,
}

impl Drop for RequestUsage<'_> {
    fn drop(&mut self) {
        self.total.extend(self.request);
    }
}

/// Tokens a summary request ran over the context window by, or `Some(0)` when the API did not
/// say. `None` when the request fit.
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
    Some(
        iter::once(Message::user(RETRY_TRUNCATION_MARKER))
            .chain(kept)
            .collect(),
    )
}

/// Whether `message` holds a server tool result whose call an earlier reply made. A `pause_turn`,
/// or a client call beside the server call, defers its result to the next reply.
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

fn is_truncation_marker(message: &Message) -> bool {
    let marker = RETRY_TRUNCATION_MARKER;
    message.role == Role::User
        && matches!(&message.content[..], [ContentBlock::Text { text }] if text == marker)
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

fn transcript_tokens(transcript: &[Message]) -> u32 {
    transcript
        .iter()
        .map(rough_tokens)
        .fold(0, u32::saturating_add)
}

/// Estimates a message's tokens at four bytes of JSON each.
fn rough_tokens(message: &Message) -> u32 {
    let bytes = serde_json::to_string(message).map_or(0, |json| json.len());
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

/// Composes the rubric plus optional user-supplied focus instructions.
fn build_user_message(instructions: Option<&str>) -> String {
    let rubric = SUMMARIZATION_USER_RUBRIC.trim();
    match instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => formatdoc! {"
            {rubric}

            Additional instructions from the engineer:

            {extra}
        "},
        None => rubric.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tokio::sync::Notify;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::client::anthropic::Client;
    use crate::client::anthropic::testing::{api_key, sse_body, test_client};
    use crate::client::{RateLimitWindow, RateLimits};
    use crate::engine::testing::{
        FakeClient, MODEL, dead_test_session, empty_prompt, inert_ops, test_session, text_turn,
        tool_heavy_transcript, with_stop,
    };
    use crate::message::Replay;
    use crate::protocol::CapturingSink;
    use crate::session::post_compact::synthesize_post_compact_message;
    use crate::tool::tasks::testing::plan;

    // ── AutoCompactFailures::reset ──

    #[test]
    fn reset_reports_a_resume_only_from_a_pause() {
        let sink = CapturingSink::new();
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

    // ── auto_compact_if_needed ──

    struct CountingFailingClient {
        calls: AtomicUsize,
    }

    impl CountingFailingClient {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Wire for CountingFailingClient {
        fn stream_message(
            &self,
            _messages: &[Message],
            _system_sections: &[&str],
            _user_context: Option<&str>,
            _tools: &[ToolDefinition],
        ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("the summarizer is unavailable. Retry in a minute."))
        }
    }

    struct DelayedSummaryClient {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl Wire for DelayedSummaryClient {
        fn stream_message(
            &self,
            _messages: &[Message],
            _system_sections: &[&str],
            _user_context: Option<&str>,
            _tools: &[ToolDefinition],
        ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
            let (tx, rx) = mpsc::channel(8);
            let started = self.started.clone();
            let release = self.release.clone();
            tokio::spawn(async move {
                started.notify_one();
                release.notified().await;
                for event in text_turn("auto summary") {
                    tx.send(Ok(event)).await.expect("test receiver alive");
                }
            });
            Ok(rx)
        }
    }

    #[tokio::test]
    async fn auto_compact_if_needed_skips_without_auto_state_usage_or_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(Vec::new());
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = tool_heavy_transcript();
        let mut inbox = Inbox::default();
        let mut failures = AutoCompactFailures::default();

        let absent = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_ops(),
            &mut inbox,
            None,
            Some(TokenUsage {
                input_tokens: 20,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 1,
            }),
        )
        .await
        .unwrap();
        assert!(!absent);

        let missing_usage = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_ops(),
            &mut inbox,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                tasks: &[],
                shells: &BackgroundShells::default(),
                spent: &mut Vec::new(),
            }),
            None,
        )
        .await
        .unwrap();
        assert!(!missing_usage);

        let below_threshold = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_ops(),
            &mut inbox,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(100),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                tasks: &[],
                shells: &BackgroundShells::default(),
                spent: &mut Vec::new(),
            }),
            Some(TokenUsage {
                input_tokens: 20,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 1,
            }),
        )
        .await
        .unwrap();
        assert!(!below_threshold);
        assert_eq!(messages, tool_heavy_transcript());
        assert_eq!(failures, AutoCompactFailures::default());
    }

    #[tokio::test]
    async fn auto_compact_if_needed_skips_a_transcript_too_short_to_shrink() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(Vec::new());
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let short = vec![Message::user("hi"), Message::assistant("hello")];
        let mut messages = short.clone();
        let mut failures = AutoCompactFailures::default();

        let compacted = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_ops(),
            &mut Inbox::default(),
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                tasks: &[],
                shells: &BackgroundShells::default(),
                spent: &mut Vec::new(),
            }),
            Some(TokenUsage {
                input_tokens: 20,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 1,
            }),
        )
        .await;

        assert!(
            matches!(compacted, Ok(false)),
            "a prefix-heavy context goes out without compaction: {compacted:?}"
        );
        assert_eq!(messages, short);
        assert_eq!(failures, AutoCompactFailures::default());
        assert_eq!(sink.events(), []);
        assert_eq!(client.requests(), [] as [Vec<Message>; 0]);
    }

    #[tokio::test]
    async fn auto_compact_if_needed_fails_the_prompt_when_the_summary_fails() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = tool_heavy_transcript();
        let mut inbox = Inbox::default();
        let mut failures = AutoCompactFailures::default();

        let compacted = auto_compact_if_needed(
            &CountingFailingClient::new(),
            &session,
            &mut messages,
            &sink,
            &mut inert_ops(),
            &mut inbox,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                tasks: &[],
                shells: &BackgroundShells::default(),
                spent: &mut Vec::new(),
            }),
            Some(TokenUsage {
                input_tokens: 20,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 1,
            }),
        )
        .await;

        let Err(TurnAbort::Failed(error)) = compacted else {
            panic!("a failed summary fails the prompt: {compacted:?}");
        };
        assert_eq!(
            format!("{error:#}"),
            "Auto-compaction failed: the summarizer is unavailable. Retry in a minute. The prompt \
             was not sent, so send it again to retry.",
        );
        assert_eq!(failures, AutoCompactFailures::new(1));
        assert_eq!(messages, tool_heavy_transcript());
        assert_eq!(sink.events(), [Event::AutoCompactionStarted]);
    }

    #[tokio::test]
    async fn auto_compact_if_needed_fails_the_prompt_when_the_boundary_is_not_saved() {
        let session = dead_test_session();
        let client = FakeClient::new(vec![text_turn("auto summary")]);
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = tool_heavy_transcript();
        let mut inbox = Inbox::default();
        let mut failures = AutoCompactFailures::default();

        let compacted = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_ops(),
            &mut inbox,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                tasks: &[],
                shells: &BackgroundShells::default(),
                spent: &mut Vec::new(),
            }),
            Some(TokenUsage {
                input_tokens: 20,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 1,
            }),
        )
        .await;

        let Err(TurnAbort::Failed(error)) = compacted else {
            panic!("an unsaved boundary fails the prompt: {compacted:?}");
        };
        assert_eq!(
            format!("{error:#}"),
            "Auto-compaction was not saved. The prompt was not sent, so send it again to retry.",
        );
        assert_eq!(failures, AutoCompactFailures::new(1));
        assert_eq!(messages, tool_heavy_transcript());
        let events = sink.events();
        assert!(
            matches!(
                events.as_slice(),
                [
                    Event::AutoCompactionStarted,
                    Event::SummaryToken(summary),
                    Event::SummaryDiscarded,
                    Event::Error(message),
                ] if summary == "auto summary" && message.starts_with("Session write failed")
            ),
            "the unsaved summary is discarded before the write failure: {events:?}",
        );
    }

    #[tokio::test]
    async fn auto_compact_if_needed_third_failure_turns_auto_compaction_off() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = CountingFailingClient::new();
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = tool_heavy_transcript();
        let mut inbox = Inbox::default();
        let mut failures = AutoCompactFailures::new(MAX_AUTO_COMPACT_FAILURES - 1);
        let usage = Some(TokenUsage {
            input_tokens: 50_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: 1,
        });
        let mut attempt = async || {
            auto_compact_if_needed(
                &client,
                &session,
                &mut messages,
                &sink,
                &mut inert_ops(),
                &mut inbox,
                Some(&mut AutoCompact {
                    config: AutoCompactionConfig {
                        enabled: true,
                        threshold_tokens: Some(50_000),
                    },
                    failures: &mut failures,
                    file_tracker: &tracker,
                    prompt: &empty_prompt(),
                    tools: &[],
                    tasks: &[],
                    shells: &BackgroundShells::default(),
                    spent: &mut Vec::new(),
                }),
                usage,
            )
            .await
        };

        let third = attempt().await;
        let fourth = attempt().await;

        let Err(TurnAbort::Failed(error)) = third else {
            panic!("the third failure still fails its prompt: {third:?}");
        };
        assert_eq!(
            format!("{error:#}"),
            "Auto-compaction failed: the summarizer is unavailable. Retry in a minute. The prompt \
             was not sent, and auto-compaction is paused after 3 failures in a row. Send the \
             prompt again to run it without compaction, or run /compact to retry compaction \
             first.",
        );
        assert!(
            matches!(fourth, Ok(false)),
            "the next prompt goes out without compaction: {fourth:?}"
        );
        assert_eq!(
            failures,
            AutoCompactFailures::new(MAX_AUTO_COMPACT_FAILURES)
        );
        assert_eq!(client.calls(), 1);
        assert_eq!(
            sink.events(),
            [
                Event::AutoCompactionStarted,
                Event::AutoCompactionPaused { failures: 3 },
            ],
        );
    }

    #[tokio::test]
    async fn auto_compact_if_needed_queues_submit_and_carries_the_task_list_over() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let client = DelayedSummaryClient {
            started: started.clone(),
            release: release.clone(),
        };
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = tool_heavy_transcript();
        let mut inbox = Inbox::default();
        let mut failures = AutoCompactFailures::default();
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        let mut auto = AutoCompact {
            config: AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(10),
            },
            failures: &mut failures,
            file_tracker: &tracker,
            prompt: &empty_prompt(),
            tools: &[],
            tasks: &plan(),
            shells: &BackgroundShells::default(),
            spent: &mut Vec::new(),
        };

        let compact = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut rx,
            &mut inbox,
            Some(&mut auto),
            Some(TokenUsage {
                input_tokens: 20,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 1,
            }),
        );
        let queue_prompt = async {
            started.notified().await;
            tx.send(Op::Queue("queued after summary".to_owned()))
                .await
                .unwrap();
            tokio::task::yield_now().await;
            release.notify_one();
        };
        let (compacted, ()) = tokio::join!(compact, queue_prompt);
        let compacted = compacted.unwrap();

        assert!(compacted);
        assert_eq!(inbox.prompts, vec!["queued after summary"]);
        assert_eq!(*auto.failures, AutoCompactFailures::default());
        assert_eq!(
            sink.events()
                .iter()
                .filter(|event| matches!(event, Event::PromptDrained(_)))
                .count(),
            0
        );
        assert!(sink.events().iter().any(|event| matches!(
            event,
            Event::SessionCompacted {
                automatic: true,
                ..
            }
        )));
        assert_eq!(
            messages,
            [synthesize_post_compact_message(
                "auto summary",
                &plan(),
                &[]
            )]
        );
    }

    #[tokio::test]
    async fn auto_compact_if_needed_retries_an_overflowing_summary_without_the_oldest_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            with_stop(
                text_turn("partial"),
                StopReason::ModelContextWindowExceeded,
                None,
            ),
            text_turn("auto summary"),
        ]);
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = tool_heavy_transcript();
        let mut failures = AutoCompactFailures::default();

        let compacted = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_ops(),
            &mut Inbox::default(),
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                tasks: &[],
                shells: &BackgroundShells::default(),
                spent: &mut Vec::new(),
            }),
            Some(TokenUsage {
                input_tokens: 20,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 1,
            }),
        )
        .await
        .unwrap();

        assert!(compacted);
        let rubric = Message::user(SUMMARIZATION_USER_RUBRIC.trim());
        assert_eq!(
            client.requests(),
            [
                [tool_heavy_transcript(), vec![rubric.clone()]].concat(),
                vec![
                    Message::user(RETRY_TRUNCATION_MARKER),
                    Message::assistant("alpha-end, beta-end"),
                    rubric,
                ],
            ],
            "the retry keeps only the newest round",
        );
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text.contains("auto summary"))
        );
        let events = sink.events();
        assert!(
            matches!(
                events.as_slice(),
                [
                    Event::AutoCompactionStarted,
                    Event::SummaryToken(overflowed),
                    Event::SummaryDiscarded,
                    Event::SummaryToken(retried),
                    Event::SessionCompacted { automatic: true, .. },
                ] if overflowed == "partial" && retried == "auto summary"
            ),
            "the overflowed summary is discarded before the retry streams: {events:?}",
        );
    }

    // ── compact_session ──

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
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(server)
            .await;
    }

    fn parent_prompt() -> PromptParts {
        let mut prompt = PromptParts::default();
        prompt.system_sections = vec!["You are the parent conversation.".to_owned()];
        "Project instructions.".clone_into(&mut prompt.user_context);
        prompt
    }

    async fn summarize(client: &Client, transcript: &[Message]) -> Result<String> {
        summarize_streaming(client, transcript).await.0
    }

    /// Summarizes `transcript` like [`summarize`], also returning the events the summary streamed.
    async fn summarize_streaming(
        client: &Client,
        transcript: &[Message],
    ) -> (Result<String>, Vec<Event>) {
        let sink = CapturingSink::new();
        let summary = compact_session(
            client,
            &parent_prompt(),
            &[],
            transcript,
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

    #[tokio::test]
    async fn compact_session_returns_trimmed_summary_on_happy_path() {
        let server = MockServer::start().await;
        mount_summary(&server, streamed_summary_body("  fixed login bug  \n")).await;

        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let summary = summarize(&client, &tool_heavy_transcript()).await.unwrap();
        assert_eq!(summary, "fixed login bug");
    }

    #[tokio::test]
    async fn compact_session_collects_initial_text_from_content_block_start() {
        let server = MockServer::start().await;
        mount_summary(
            &server,
            summary_stream("  fixed", " login bug  \n", "end_turn", true),
        )
        .await;

        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let (summary, events) = summarize_streaming(&client, &tool_heavy_transcript()).await;
        assert_eq!(summary.unwrap(), "fixed login bug");
        assert_eq!(
            events,
            [token("  fixed"), token(" login bug  \n")],
            "the kept summary streams untrimmed and is not discarded",
        );
    }

    #[tokio::test]
    async fn compact_session_summary_leaves_out_the_reply_thinking() {
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

        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let (summary, events) = summarize_streaming(&client, &tool_heavy_transcript()).await;
        assert_eq!(summary.unwrap(), "fixed login bug");
        assert_eq!(events, [token("fixed login bug")]);
    }

    #[tokio::test]
    async fn compact_session_request_forks_the_conversation_prefix() {
        let server = MockServer::start().await;
        let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let recorded = Arc::clone(&bodies);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(move |req: &Request| {
                recorded
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&req.body).unwrap());
                ResponseTemplate::new(200)
                    .set_body_string(streamed_summary_body("done"))
                    .insert_header("content-type", "text/event-stream")
            })
            .mount(&server)
            .await;
        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let prompt = parent_prompt();
        let tools = [ToolDefinition {
            name: "read".to_owned(),
            description: "Read a file.".to_owned(),
            input_schema: json!({"type": "object"}),
        }];
        let transcript = tool_heavy_transcript();

        let sections: Vec<&str> = prompt.system_sections.iter().map(String::as_str).collect();
        let mut rx = client
            .stream_message(&transcript, &sections, Some(&prompt.user_context), &tools)
            .unwrap();
        while rx.recv().await.is_some() {}
        compact_session(
            &client,
            &prompt,
            &tools,
            &transcript,
            Some("focus on auth"),
            &CapturingSink::new(),
            &mut Vec::new(),
        )
        .await
        .unwrap();

        let mut bodies = bodies.lock().unwrap().clone();
        bodies.iter_mut().for_each(strip_cache_control);
        let [parent, fork] = bodies.as_slice() else {
            panic!("expected two requests, got {}", bodies.len());
        };
        // Skips the billing block, whose `cch` hashes the whole body and so differs per request.
        let system = |body: &serde_json::Value| body["system"].as_array().unwrap()[1..].to_vec();
        assert_eq!(system(fork), system(parent));
        assert_eq!(fork["tools"], parent["tools"]);
        let parent_messages = parent["messages"].as_array().unwrap();
        let fork_messages = fork["messages"].as_array().unwrap();
        assert_eq!(fork_messages[..parent_messages.len()], parent_messages[..]);
        assert_eq!(fork_messages.len(), parent_messages.len() + 1);
        let rubric = fork_messages.last().unwrap();
        assert_eq!(rubric["role"], "user");
        let rubric = rubric.to_string();
        assert!(rubric.contains("Do not call any tools"), "{rubric}");
        assert!(rubric.contains("focus on auth"), "{rubric}");
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

    /// Transcript whose oldest round holds a large tool result and whose kept round has thinking.
    fn overflowing_transcript() -> Vec<Message> {
        vec![
            Message::user("fix the bug"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "reading".to_owned(),
                    },
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
                    ContentBlock::Text {
                        text: "found it".to_owned(),
                    },
                ],
            },
            Message::user("any progress?"),
            Message::assistant("fixed"),
        ]
    }

    async fn mount_overflow_then(server: &MockServer, retry_body: String) {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(summary_stream(
                        "",
                        "partial",
                        "model_context_window_exceeded",
                        true,
                    ))
                    .insert_header("content-type", "text/event-stream"),
            )
            .up_to_n_times(1)
            .mount(server)
            .await;
        mount_summary(server, retry_body).await;
    }

    #[tokio::test]
    async fn compact_session_retries_an_overflow_without_the_oldest_rounds_or_thinking() {
        let server = MockServer::start().await;
        mount_overflow_then(&server, streamed_summary_body("trimmed summary")).await;
        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let sink = CapturingSink::new();
        let mut spent = Vec::new();

        let summary = compact_session(
            &client,
            &parent_prompt(),
            &[],
            &overflowing_transcript(),
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
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let mut retry: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert!(
            !retry["messages"].to_string().contains("cache_control"),
            "no later request extends the retry, so its transcript carries no breakpoint",
        );
        strip_cache_control(&mut retry);
        let messages = retry["messages"].as_array().unwrap();
        assert_eq!(
            messages[..messages.len() - 1],
            [
                json!({"role": "user", "content": [{"type": "text", "text": "Project instructions."}]}),
                json!({"role": "user", "content": [{"type": "text", "text": RETRY_TRUNCATION_MARKER}]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "found it"}]}),
                json!({"role": "user", "content": [{"type": "text", "text": "any progress?"}]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "fixed"}]}),
            ],
        );
        assert!(
            messages
                .last()
                .unwrap()
                .to_string()
                .contains("Summarize the conversation"),
            "the rubric stays last",
        );
        assert_eq!(
            spent,
            [TokenUsage::new(100, 7); 2],
            "both requests are billed"
        );
    }

    /// Transcript of `rounds` exchanges whose user messages each hold about 10,000 rough tokens.
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
    fn sent_messages(request: &Request) -> Vec<serde_json::Value> {
        let mut body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        strip_cache_control(&mut body);
        body["messages"].as_array().unwrap()[1..].to_vec()
    }

    fn to_json(messages: &[Message]) -> Vec<serde_json::Value> {
        messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn compact_session_retries_a_prompt_too_long_rejection_with_room_for_the_summary() {
        let server = MockServer::start().await;
        prompt_too_long("prompt is too long: 205000 tokens > 200000 maximum")
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_summary(&server, streamed_summary_body("trimmed summary")).await;
        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
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
        // Freeing the 5,000-token overflow plus the summary reserve takes the first five messages.
        let expected: Vec<Message> = iter::once(Message::user(RETRY_TRUNCATION_MARKER))
            .chain(transcript[5..].iter().cloned())
            .chain([Message::user(build_user_message(None))])
            .collect();
        assert_eq!(sent_messages(&requests[1]), to_json(&expected));
    }

    #[tokio::test]
    async fn compact_session_retry_closes_a_tool_turn_the_kept_rounds_leave_open() {
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
            let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
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
                        ContentBlock::Text {
                            text: "reading".to_owned(),
                        },
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
                    content: vec![
                        ContentBlock::Text {
                            text: "reading".to_owned(),
                        },
                        tool_call.clone(),
                    ],
                },
                tool_result.clone(),
            ];
            expected.extend(interrupted.then(Message::interruption));
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
    async fn compact_session_fails_when_the_newest_round_alone_still_overflows() {
        let server = MockServer::start().await;
        mount_overflow_then(
            &server,
            summary_stream("", "partial", "model_context_window_exceeded", true),
        )
        .await;
        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let sink = CapturingSink::new();
        let mut spent = Vec::new();

        let err = compact_session(
            &client,
            &parent_prompt(),
            &[],
            &overflowing_transcript(),
            None,
            &sink,
            &mut spent,
        )
        .await
        .expect_err("an overflow with no round left to drop fails the compaction");

        assert!(
            format!("{err:#}").contains("no room in the context window"),
            "{err:#}"
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
    async fn compact_session_gives_up_after_three_overflow_retries() {
        let server = MockServer::start().await;
        prompt_too_long("prompt is too long: 200001 tokens > 200000 maximum")
            .mount(&server)
            .await;
        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");

        let err = summarize(&client, &heavy_rounds(8))
            .await
            .expect_err("every retry overflows");

        assert!(format!("{err:#}").contains("prompt is too long"), "{err:#}");
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn compact_session_refuses_an_empty_or_short_session_without_a_request() {
        let server = MockServer::start().await;
        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
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
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn compact_session_empty_summary_errors() {
        let server = MockServer::start().await;
        mount_summary(&server, streamed_summary_body("   \n  ")).await;

        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let (summary, events) = summarize_streaming(&client, &tool_heavy_transcript()).await;
        let err = summary.expect_err("empty summary must error");
        assert_eq!(format!("{err:#}"), "the model returned an empty summary");
        assert_eq!(events, [token("   \n  "), Event::SummaryDiscarded]);
    }

    #[tokio::test]
    async fn compact_session_rejects_an_unfinished_summary_and_still_records_usage() {
        let cases = [
            ("max_tokens", true, "max_tokens output limit"),
            ("refusal", true, "declined to summarize"),
            ("tool_use", true, "called a tool"),
            ("pause_turn", true, "stopped early (PauseTurn)"),
            ("end_turn", false, "ended before message_stop"),
        ];
        for (stop_reason, finished, expected) in cases {
            let server = MockServer::start().await;
            mount_summary(
                &server,
                summary_stream("", "partial", stop_reason, finished),
            )
            .await;
            let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
            let sink = CapturingSink::new();
            let mut spent = Vec::new();

            let err = compact_session(
                &client,
                &parent_prompt(),
                &[],
                &tool_heavy_transcript(),
                None,
                &sink,
                &mut spent,
            )
            .await
            .expect_err("an unfinished summary must not replace the transcript");

            assert!(
                format!("{err:#}").contains(expected),
                "{stop_reason}: {err:#}"
            );
            assert_eq!(spent, [TokenUsage::new(100, 7)], "{stop_reason}");
            assert_eq!(
                sink.events(),
                [token("partial"), Event::SummaryDiscarded],
                "{stop_reason}",
            );
        }
    }

    #[tokio::test]
    async fn compact_session_dropped_mid_summary_discards_what_streamed() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(Ok(ResponseEvent::Text("partial".to_owned())))
            .await
            .unwrap();
        let client = HeldStreamClient(Mutex::new(Some(rx)));
        let (prompt, transcript) = (parent_prompt(), tool_heavy_transcript());
        let sink = CapturingSink::new();
        let mut spent = Vec::new();

        {
            let mut compaction = pin!(compact_session(
                &client,
                &prompt,
                &[],
                &transcript,
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
    async fn compact_session_passes_retry_notices_and_usage_limits_through() {
        let notice = "HTTP 529 overloaded_error. Retrying in 1 s (retry 1 of 5).";
        let limits = RateLimits {
            five_hour: Some(RateLimitWindow {
                utilization: 0.42,
                resets_at: 1_760_000_000,
            }),
            seven_day: None,
        };
        let (tx, rx) = mpsc::channel(4);
        for event in [
            ResponseEvent::Retrying(notice.to_owned()),
            ResponseEvent::RateLimits(limits),
            ResponseEvent::Text("Recap".to_owned()),
            ResponseEvent::Completed(Response {
                content: vec![ContentBlock::Text {
                    text: "Recap".to_owned(),
                }],
                ..Response::default()
            }),
        ] {
            tx.send(Ok(event)).await.unwrap();
        }
        let client = HeldStreamClient(Mutex::new(Some(rx)));
        let sink = CapturingSink::new();

        let summary = compact_session(
            &client,
            &parent_prompt(),
            &[],
            &tool_heavy_transcript(),
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

    /// Replies with a stream the test feeds, which stays open while the test holds its sender.
    struct HeldStreamClient(Mutex<Option<mpsc::Receiver<Result<ResponseEvent>>>>);

    impl Wire for HeldStreamClient {
        fn stream_message(
            &self,
            _messages: &[Message],
            _system_sections: &[&str],
            _user_context: Option<&str>,
            _tools: &[ToolDefinition],
        ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
            Ok(self.0.lock().unwrap().take().expect("one request"))
        }
    }

    #[tokio::test]
    async fn compact_session_surfaces_stream_error_event() {
        let server = MockServer::start().await;
        mount_summary(
            &server,
            indoc! {r#"
                event: ping
                data: {"type":"message_start","message":{"id":"m","model":"claude-haiku-5-5"}}

                event: ping
                data: {"type":"error","error":{"type":"overloaded_error","message":"servers overloaded"}}

            "#}
            .to_owned(),
        )
        .await;

        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let err = summarize(&client, &tool_heavy_transcript())
            .await
            .expect_err("in-band error must propagate");
        assert!(
            format!("{err:#}").contains("servers overloaded"),
            "underlying API error message must thread through: {err:#}",
        );
    }

    #[tokio::test]
    async fn compact_session_propagates_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(500).set_body_string(r#"{"error":{"type":"server_error"}}"#),
            )
            .mount(&server)
            .await;

        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let err = summarize(&client, &tool_heavy_transcript())
            .await
            .expect_err("expected error");
        assert!(format!("{err:#}").contains("500") || format!("{err:#}").contains("server_error"));
    }

    // ── drop_oldest_rounds ──

    #[test]
    fn drop_oldest_rounds_keeps_the_newest_round_when_the_gap_exceeds_the_transcript() {
        let kept = drop_oldest_rounds(&tool_heavy_transcript(), u32::MAX).unwrap();

        assert_eq!(
            kept,
            [
                Message::user(RETRY_TRUNCATION_MARKER),
                Message::assistant("alpha-end, beta-end"),
            ],
        );
    }

    #[test]
    fn drop_oldest_rounds_replaces_an_earlier_truncation_marker() {
        let transcript = [
            Message::user(RETRY_TRUNCATION_MARKER),
            Message::assistant("older"),
            Message::user("next"),
            Message::assistant("newer"),
        ];

        let kept = drop_oldest_rounds(&transcript, 0).unwrap();

        assert_eq!(
            to_json(&kept),
            to_json(&[
                Message::user(RETRY_TRUNCATION_MARKER),
                Message::assistant("newer")
            ]),
        );
    }

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
    fn drop_oldest_rounds_keeps_a_deferred_server_call_with_its_result() {
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
            reply(vec![
                server_result("s1"),
                ContentBlock::Text {
                    text: "Found it.".to_owned(),
                },
            ]),
        ];

        let kept = drop_oldest_rounds(&transcript, u32::MAX).unwrap();

        assert_eq!(kept[0], Message::user(RETRY_TRUNCATION_MARKER));
        assert_eq!(kept[1..], transcript[1..]);
    }

    #[test]
    fn drop_oldest_rounds_keeps_a_paused_server_call_with_its_continuation() {
        let transcript = [
            Message::user("research X"),
            reply(vec![
                ContentBlock::Text {
                    text: "Searching.".to_owned(),
                },
                server_call("s1"),
            ]),
            reply(vec![
                server_result("s1"),
                ContentBlock::Text {
                    text: "Found X.".to_owned(),
                },
            ]),
        ];

        let kept = drop_oldest_rounds(&transcript, u32::MAX).unwrap();

        assert_eq!(kept[0], Message::user(RETRY_TRUNCATION_MARKER));
        assert_eq!(kept[1..], transcript[1..]);
    }

    #[test]
    fn drop_oldest_rounds_needs_a_reply_to_keep() {
        let thinking_only = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "hmm".to_owned(),
                replay: Replay::with_model(MODEL, "sig"),
            }],
        };

        assert!(drop_oldest_rounds(&[Message::user("one"), thinking_only], 0).is_none());
    }

    // ── build_user_message ──

    #[test]
    fn build_user_message_without_instructions_is_just_the_rubric() {
        let s = build_user_message(None);
        assert!(s.contains("Summarize the conversation"));
        assert!(!s.contains("Additional instructions"));
    }

    #[test]
    fn build_user_message_with_instructions_appends_them_under_an_anchor() {
        let s = build_user_message(Some("focus on the build error"));
        assert!(s.contains("Additional instructions from the engineer"));
        assert!(s.contains("focus on the build error"));
    }

    #[test]
    fn build_user_message_treats_whitespace_only_instructions_as_absent() {
        let s = build_user_message(Some("   \n\t  "));
        assert!(!s.contains("Additional instructions"));
    }
}
