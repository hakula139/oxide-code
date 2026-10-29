//! Conversation compaction: the engine's `/compact` and auto-compaction, which swap the transcript
//! for a summary of it.

mod summary;

use anyhow::{Result, anyhow};
use tracing::warn;

use self::summary::request_summary;
use super::abort::{AbortResult, Mailbox, TurnAbort};
use super::estimate::transcript_tokens;
use super::post_compact::synthesize_post_compact_message;
use super::turn::{conversation_request, record_interruption};
use super::{Engine, TurnEnd, report_abort};
use crate::auth::Auth;
use crate::config::compaction::AutoCompactionConfig;
use crate::message::Message;
use crate::prompt::UserContext;
use crate::protocol::{CompactTrigger, Event, EventSink};
use crate::session::handle::CompactBoundary;
use crate::session::store::CompactRecord;
use crate::util::text::join_sentences;

const MAX_AUTO_COMPACT_FAILURES: u8 = 3;

/// Rough transcript size below which compaction is refused, since the summary would be about as
/// long as the transcript.
const MIN_TOKENS_FOR_COMPACT: u32 = 1_000;

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
            sink.emit(Event::AutoCompactionPaused { failures: self.0 });
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
            sink.emit(Event::AutoCompactionResumed);
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
        mailbox: &mut Mailbox<'_>,
    ) -> AbortResult<()> {
        if !auto_compaction_due(
            self.auto_compaction(),
            self.usage.context_estimate(),
            self.transcript.messages(),
        ) {
            return Ok(());
        }

        self.run_auto_compaction(auth, sink, mailbox, AutoPoint::BeforePrompt)
            .await
    }

    /// Compacts the transcript at a round boundary of a turn whose context crossed the threshold,
    /// so the turn goes on from the summary. A cancel ends the turn on [`Message::interruption`] as
    /// at any other round boundary, and a failure counts toward the pause and fails the turn.
    pub(super) async fn compact_mid_turn(
        &mut self,
        auth: &Auth,
        sink: &dyn EventSink,
        mailbox: &mut Mailbox<'_>,
    ) -> AbortResult<()> {
        let compacted = self
            .run_auto_compaction(auth, sink, mailbox, AutoPoint::MidTurn)
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
        mailbox: &mut Mailbox<'_>,
        point: AutoPoint,
    ) -> AbortResult<()> {
        sink.emit(Event::AutoCompactionStarted);
        let cause = match self
            .run_compaction(auth, sink, mailbox, None, point.into())
            .await
        {
            Ok(()) => return Ok(()),
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
        mailbox: &mut Mailbox<'_>,
        instructions: Option<String>,
    ) -> TurnEnd {
        let auth = match self.ensure_signed_in(sink, mailbox).await {
            Ok(auth) => auth,
            Err(abort) => return report_abort(sink, abort),
        };
        let compacted = self
            .run_compaction(&auth, sink, mailbox, instructions, CompactTrigger::Manual)
            .await;
        let abort = match compacted {
            Ok(()) => {
                sink.emit(Event::SessionReady);
                return TurnEnd::Completed;
            }
            Err(CompactFailure::Aborted(abort)) => abort,
            Err(CompactFailure::Summary(e)) => TurnAbort::Failed(e.context("Compaction failed")),
            Err(CompactFailure::WriterGone) => TurnAbort::Failed(anyhow!(
                "Compaction was not saved, so the transcript is unchanged"
            )),
        };
        report_abort(sink, abort)
    }

    /// Summarizes the transcript and swaps the summary in as it saves the boundary, starting the
    /// new transcript from a fresh user context. Bills each summary request and shows the usage
    /// once any request reported it or the transcript changed.
    async fn run_compaction(
        &mut self,
        auth: &Auth,
        sink: &dyn EventSink,
        mailbox: &mut Mailbox<'_>,
        instructions: Option<String>,
        trigger: CompactTrigger,
    ) -> Result<(), CompactFailure> {
        let tools = self.tools.definitions();
        let mut spent = Vec::new();
        let session_id = self.session_id();
        let summary = mailbox
            .race(
                request_summary(
                    &self.client,
                    conversation_request(
                        &session_id,
                        auth,
                        &self.settings,
                        &self.transcript,
                        &tools,
                    ),
                    instructions.as_deref(),
                    sink,
                    &mut spent,
                ),
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
                if self.replace_with_summary(sink, boundary, trigger).await {
                    self.show_instruction_warnings(sink, warnings);
                    Ok(())
                } else {
                    Err(CompactFailure::WriterGone)
                }
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
    /// whether the summary replaced the transcript. A failed write stops persistence, while the
    /// summary still replaces the transcript in memory and the failure is reported after the
    /// compaction event. Only a session writer that is gone discards the summary and leaves the
    /// transcript unchanged.
    async fn replace_with_summary(
        &mut self,
        sink: &dyn EventSink,
        boundary: CompactBoundary,
        trigger: CompactTrigger,
    ) -> bool {
        let synthetic = synthesize_post_compact_message(
            &boundary.record.summary,
            &boundary.record.tasks,
            &self.shells.list(),
            trigger,
        );
        let summary = boundary.record.summary.clone();
        let instructions = boundary.record.instructions.clone();
        let outcome = self.transcript.compact(boundary, synthetic).await;
        if outcome.checkpoint.is_none() {
            sink.emit(Event::SummaryDiscarded);
            sink.session_write(&outcome.report);
            return false;
        }

        sink.emit(Event::SessionCompacted {
            summary,
            pre_count: outcome.pre_count,
            instructions,
            trigger,
        });
        // Reports the write after the compaction event, which clears the TUI's chat.
        sink.session_write(&outcome.report);
        true
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use indoc::formatdoc;
    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use super::*;
    use crate::client::anthropic::testing::{api_key, sse_body, sse_response, text_stream_body};
    use crate::config::compaction::CompactionConfig;
    use crate::engine::local_testing::{dead_test_session, inert_mailbox, tool_heavy_transcript};
    use crate::engine::login::SignIn;
    use crate::engine::testing::{
        MODEL, SESSION_CONTEXT, engine, mount_reply, request_bodies, with_pinned_home,
    };
    use crate::file_tracker::testing::{
        assert_writes_draining, blocked_write, poll_until_draining, seed_full_read,
    };
    use crate::model::TokenUsage;
    use crate::permission::Mode;
    use crate::prompt::SessionContext;
    use crate::protocol::testing::CapturingSink;
    use crate::protocol::{Op, ResumeTarget, UsageSnapshot};
    use crate::session::store::local_testing::fail_writes;
    use crate::session::store::testing::test_store;
    use crate::tool::bash::background::ShellState;
    use crate::tool::bash::background::local_testing::start_shell;
    use crate::tool::bash::background::testing::shell;
    use crate::tool::tasks::Task;
    use crate::tool::tasks::testing::plan;

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
                        .auto_compact(&api_key(), &sink, &mut inert_mailbox())
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
                let mut mailbox = inert_mailbox();

                let third = engine.auto_compact(&api_key(), &sink, &mut mailbox).await;
                let fourth = engine.auto_compact(&api_key(), &sink, &mut mailbox).await;

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
                    .auto_compact(&api_key(), &sink, &mut inert_mailbox())
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
                let mut mailbox = Mailbox::new(&mut ops);
                ops_tx
                    .try_send(Op::Queue("queued after summary".to_owned()))
                    .unwrap();

                let compacted = engine.auto_compact(&api_key(), &sink, &mut mailbox).await;

                assert!(compacted.is_ok(), "{compacted:?}");
                assert_eq!(
                    mailbox.drain_prompts().collect::<Vec<_>>(),
                    ["queued after summary"]
                );
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
                let mut mailbox = inert_mailbox();

                let end = engine
                    .compact(&sink, &mut mailbox, Some("focus".to_owned()))
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
                let mut mailbox = inert_mailbox();

                let end = engine
                    .compact(&CapturingSink::default(), &mut mailbox, None)
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
                let mut mailbox = inert_mailbox();

                let end = engine
                    .compact(&CapturingSink::default(), &mut mailbox, None)
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
                let mut mailbox = Mailbox::new(&mut ops);

                engine
                    .transcript
                    .set_messages(vec![Message::user("too short")]);
                let end = engine.compact(&sink, &mut mailbox, None).await;
                assert_eq!(end, TurnEnd::Failed);
                assert_eq!(engine.transcript.messages(), [Message::user("too short")]);

                engine.transcript.set_messages(tool_heavy_transcript());
                ops_tx.send(Op::Interrupt).await.unwrap();
                let end = engine.compact(&sink, &mut mailbox, None).await;
                assert_eq!(end, TurnEnd::Cancelled);
                assert_eq!(engine.transcript.messages(), tool_heavy_transcript());

                engine.transcript.set_session(dead_test_session());
                let end = engine.compact(&sink, &mut mailbox, None).await;
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

                let end = engine.compact(&sink, &mut inert_mailbox(), None).await;

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
                let mut mailbox = inert_mailbox();

                let end = engine.compact(&sink, &mut mailbox, None).await;

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
                let mut mailbox = Mailbox::new(&mut ops);
                ops_tx.try_send(Op::SetMode(Mode::Plan)).unwrap();
                ops_tx.try_send(Op::Interrupt).unwrap();

                let end = engine.compact(&sink, &mut mailbox, None).await;

                assert_eq!(end, TurnEnd::Cancelled);
                assert_eq!(mailbox.take_mode(), Some(Mode::Plan));
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
                let mut mailbox = Mailbox::new(&mut ops);
                let interrupt_once_streaming = async {
                    while sink.events().is_empty() {
                        tokio::task::yield_now().await;
                    }
                    ops_tx.send(Op::Interrupt).await.unwrap();
                };

                let (end, ()) = tokio::join!(
                    engine.compact(&sink, &mut mailbox, None),
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
                let mut mailbox = inert_mailbox();

                let end = engine.compact(&sink, &mut mailbox, None).await;

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

                    assert!(compacted, "{trigger:?}");
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

                assert!(!compacted);
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

                assert!(compacted);
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

                assert!(compacted);
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
}
