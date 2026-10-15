//! `/compact` summarization request and summary wrapping.
//!
//! The summarization request forks the live conversation. It sends the conversation's own system
//! prompt, user context, tools, and transcript, then appends the rubric as the last user message,
//! so it reads the conversation's prompt cache and passes earlier thinking blocks back unchanged. A
//! summary that overflows the context window, as an HTTP 400 `prompt is too long` or a
//! `model_context_window_exceeded` stop, retries up to three times without the oldest rounds or
//! any thinking block, which misses the cache. Session mutation happens in the driver after the
//! summary succeeds.

use std::iter;

use anyhow::{Result, bail};
use indoc::{formatdoc, indoc};
use tokio::sync::mpsc;

use crate::agent::{AgentClient, StreamOutcome, TokenUsage};
use crate::client::anthropic::transport::PromptTooLong;
use crate::client::anthropic::wire::{StopReason, StreamEvent};
use crate::message::{ContentBlock, Message, Role};
use crate::prompt::PromptParts;
use crate::protocol::{Event, EventSink};
use crate::tool::ToolDefinition;

/// Minimum messages required for compaction to be worthwhile. Below this, the summary is
/// usually longer than the transcript itself.
const MIN_MESSAGES_FOR_COMPACT: usize = 4;

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
const SUMMARIZATION_USER_RUBRIC: &str = indoc! {r"
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

/// Prefix for the synthetic post-compact user message.
const SUMMARY_PREFIX: &str = indoc! {r"
    This conversation has been compacted. The summary below covers the prior work. Continue from here
    without re-asking the engineer what to do.
"};

/// Drives the summarization request and returns the trimmed summary. `prompt` and `tools` must be
/// the ones the conversation's own requests send. `spent` receives the usage of every request,
/// even when the summary fails or the caller drops the future.
///
/// Errors when the transcript is too short to compact, when a request fails, or when the reply is
/// empty or stopped before the summary finished, including when the last retry still overflows.
pub(crate) async fn compact_session(
    client: &dyn AgentClient,
    prompt: &PromptParts,
    tools: &[ToolDefinition],
    transcript: &[Message],
    instructions: Option<&str>,
    spent: &mut Vec<TokenUsage>,
) -> Result<String> {
    if transcript.len() < MIN_MESSAGES_FOR_COMPACT {
        bail!(
            "session is too short to compact ({} messages, need at least {MIN_MESSAGES_FOR_COMPACT})",
            transcript.len(),
        );
    }
    let text_messages = transcript
        .iter()
        .filter(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { .. }))
        })
        .count();
    if text_messages < MIN_MESSAGES_FOR_COMPACT {
        bail!(
            "session has too little text to compact ({text_messages} text messages, need at least {MIN_MESSAGES_FOR_COMPACT})",
        );
    }

    let sections: Vec<&str> = prompt.system_sections.iter().map(String::as_str).collect();
    let user_context = Some(prompt.user_context.as_str());
    let rubric = Message::user(build_user_message(instructions));
    let request: Vec<Message> = transcript.iter().chain([&rubric]).cloned().collect();
    let mut rx = client.stream_message(&request, &sections, user_context, tools)?;
    let mut kept = transcript.to_vec();
    let mut retries = 0;
    let outcome = loop {
        let result = collect_summary(rx, spent).await;
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
        let request = retry_request(&kept, &rubric);
        rx = client.stream_one_shot(&request, &sections, user_context, tools)?;
    };
    match outcome.stop_reason {
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

    let summary: String = outcome
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        bail!("compaction returned empty summary");
    }
    Ok(trimmed.to_owned())
}

/// Collects one summarization reply, adding its usage to `spent` even when it fails.
async fn collect_summary(
    rx: mpsc::Receiver<Result<StreamEvent>>,
    spent: &mut Vec<TokenUsage>,
) -> Result<StreamOutcome> {
    let mut usage = RequestUsage {
        total: spent,
        request: None,
    };
    super::collect_response(rx, &QuietSink, &mut usage.request).await
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
fn overflow_tokens(result: &Result<StreamOutcome>) -> Option<u32> {
    match result {
        Ok(outcome) => {
            (outcome.stop_reason == Some(StopReason::ModelContextWindowExceeded)).then_some(0)
        }
        Err(error) => error
            .downcast_ref::<PromptTooLong>()
            .map(|too_long| too_long.overflow.unwrap_or(0)),
    }
}

/// Copies `transcript` without thinking blocks or an earlier retry's truncation marker, then drops
/// its oldest API rounds until about `target` tokens are freed, keeping at least the newest round.
/// `None` when only one round remains.
fn drop_oldest_rounds(transcript: &[Message], target: u32) -> Option<Vec<Message>> {
    let transcript = match transcript {
        [first, rest @ ..] if is_truncation_marker(first) => rest,
        _ => transcript,
    };
    let mut messages: Vec<Message> = transcript.iter().filter_map(without_thinking).collect();
    let mut freed = 0;
    let mut cut = None;
    for (i, message) in messages.iter().enumerate() {
        if i > 0 && message.role == Role::Assistant {
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

/// Estimates a message's tokens at four bytes of JSON each.
fn rough_tokens(message: &Message) -> u32 {
    let bytes = serde_json::to_string(message).map_or(0, |json| json.len());
    u32::try_from(bytes / 4).unwrap_or(u32::MAX)
}

/// Appends the rubric to a retry transcript, closing a tool-use turn it leaves open first.
fn retry_request(kept: &[Message], rubric: &Message) -> Vec<Message> {
    let open_turn = kept.last().is_some_and(|message| {
        message.role == Role::User
            && message
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    });
    kept.iter()
        .cloned()
        .chain(open_turn.then(|| Message::assistant(RETRY_TURN_CLOSE_MARKER)))
        .chain(iter::once(rubric.clone()))
        .collect()
}

/// Discards the summary's stream tokens, which belong to no visible turn.
struct QuietSink;

impl EventSink for QuietSink {
    fn send(&self, _event: Event) -> Result<()> {
        Ok(())
    }
}

/// Composes the rubric plus optional user-supplied focus instructions.
fn build_user_message(instructions: Option<&str>) -> String {
    match instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => formatdoc! {"
            {rubric}

            Additional instructions from the engineer:

            {extra}
        ", rubric = SUMMARIZATION_USER_RUBRIC.trim()},
        None => SUMMARIZATION_USER_RUBRIC.trim().to_owned(),
    }
}

/// Composes the synthetic post-compact root message for the next turn.
pub(crate) fn synthesize_post_compact_message(summary: &str) -> Message {
    Message::user(formatdoc! {"
        {prefix}

        {summary}
    ", prefix = SUMMARY_PREFIX.trim(), summary = summary.trim()})
}

/// Removes the synthetic summary prefix from a resumed post-compact root message.
pub(crate) fn strip_synthetic_post_compact_prefix(message: &mut Message) -> bool {
    if message.role != Role::User {
        return false;
    }
    let Some(ContentBlock::Text { text }) = message.content.first() else {
        return false;
    };
    if !text.trim_start().starts_with(SUMMARY_PREFIX.trim()) {
        return false;
    }

    message.content.remove(0);
    true
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::client::anthropic::Client;
    use crate::client::anthropic::testing::{api_key, test_client};
    use crate::message::Role;

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

    fn sse_body(frames: &[serde_json::Value]) -> String {
        use std::fmt::Write as _;

        let mut body = String::new();
        for frame in frames {
            _ = writeln!(body, "event: ping\ndata: {frame}\n");
        }
        body
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
        compact_session(
            client,
            &parent_prompt(),
            &[],
            transcript,
            None,
            &mut Vec::new(),
        )
        .await
    }

    fn fake_transcript() -> Vec<Message> {
        vec![
            Message::user("fix the bug"),
            Message::assistant("looking now"),
            Message::user("any progress?"),
            Message::assistant("found it"),
        ]
    }

    /// Transcript whose middle round carries thinking, a tool call, and its result.
    fn tool_round_transcript() -> Vec<Message> {
        vec![
            Message::user("fix the bug"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "check the file".to_owned(),
                        signature: "sig".to_owned(),
                    },
                    ContentBlock::Text {
                        text: "let me check".to_owned(),
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
                    content: "file body".to_owned(),
                    is_error: false,
                }],
            },
            Message::assistant("found it"),
            Message::user("any progress?"),
            Message::assistant("fixed"),
        ]
    }

    #[tokio::test]
    async fn compact_session_returns_trimmed_summary_on_happy_path() {
        let server = MockServer::start().await;
        mount_summary(&server, streamed_summary_body("  fixed login bug  \n")).await;

        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let summary = summarize(&client, &fake_transcript()).await.unwrap();
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
        let summary = summarize(&client, &fake_transcript()).await.unwrap();
        assert_eq!(summary, "fixed login bug");
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
        let summary = summarize(&client, &fake_transcript()).await.unwrap();
        assert_eq!(summary, "fixed login bug");
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
            name: "read",
            description: "Read a file.",
            input_schema: json!({"type": "object"}),
        }];
        let transcript = tool_round_transcript();

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
                        signature: "sig".to_owned(),
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
        let mut spent = Vec::new();

        let summary = compact_session(
            &client,
            &parent_prompt(),
            &[],
            &overflowing_transcript(),
            None,
            &mut spent,
        )
        .await
        .unwrap();

        assert_eq!(summary, "trimmed summary");
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

        let summary = summarize(&client, &transcript).await.unwrap();

        assert_eq!(summary, "trimmed summary");
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
        let server = MockServer::start().await;
        mount_overflow_then(&server, streamed_summary_body("trimmed summary")).await;
        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
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
        let transcript = vec![
            Message::user("a".repeat(100_000)),
            Message::assistant("reply 0"),
            Message::user("check the file"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "read it first".to_owned(),
                        signature: "sig".to_owned(),
                    },
                    ContentBlock::Text {
                        text: "reading".to_owned(),
                    },
                    tool_call.clone(),
                ],
            },
            tool_result.clone(),
        ];

        summarize(&client, &transcript).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let expected = [
            Message::user(RETRY_TRUNCATION_MARKER),
            Message::assistant("reply 0"),
            Message::user("check the file"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "reading".to_owned(),
                    },
                    tool_call,
                ],
            },
            tool_result,
            Message::assistant(RETRY_TURN_CLOSE_MARKER),
            Message::user(build_user_message(None)),
        ];
        assert_eq!(sent_messages(&requests[1]), to_json(&expected));
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
        let mut spent = Vec::new();

        let err = compact_session(
            &client,
            &parent_prompt(),
            &[],
            &overflowing_transcript(),
            None,
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
    async fn compact_session_too_short_transcript_errors_before_request() {
        let server = MockServer::start().await;
        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let err = summarize(&client, &[Message::user("hi")])
            .await
            .expect_err("must refuse short transcript");
        assert!(format!("{err:#}").contains("too short to compact"));
    }

    #[tokio::test]
    async fn compact_session_refuses_when_too_few_messages_carry_text() {
        let server = MockServer::start().await;
        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let mut transcript = tool_round_transcript();
        transcript.truncate(4);
        transcript[1]
            .content
            .retain(|b| !matches!(b, ContentBlock::Text { .. }));

        let err = summarize(&client, &transcript)
            .await
            .expect_err("transcript must still satisfy the useful-work threshold");
        assert!(format!("{err:#}").contains("too little text"));
    }

    #[tokio::test]
    async fn compact_session_empty_summary_errors() {
        let server = MockServer::start().await;
        mount_summary(&server, streamed_summary_body("   \n  ")).await;

        let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
        let err = summarize(&client, &fake_transcript())
            .await
            .expect_err("empty summary must error");
        assert!(format!("{err:#}").contains("empty summary"));
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
            let mut spent = Vec::new();

            let err = compact_session(
                &client,
                &parent_prompt(),
                &[],
                &fake_transcript(),
                None,
                &mut spent,
            )
            .await
            .expect_err("an unfinished summary must not replace the transcript");

            assert!(
                format!("{err:#}").contains(expected),
                "{stop_reason}: {err:#}"
            );
            assert_eq!(spent, [TokenUsage::new(100, 7)], "{stop_reason}");
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
        let err = summarize(&client, &fake_transcript())
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
        let err = summarize(&client, &fake_transcript())
            .await
            .expect_err("expected error");
        assert!(format!("{err:#}").contains("500") || format!("{err:#}").contains("server_error"));
    }

    // ── drop_oldest_rounds ──

    #[test]
    fn drop_oldest_rounds_keeps_the_newest_round_when_the_gap_exceeds_the_transcript() {
        let kept = drop_oldest_rounds(&fake_transcript(), u32::MAX).unwrap();

        let texts: Vec<_> = kept
            .iter()
            .map(|m| match &m.content[..] {
                [ContentBlock::Text { text }] => (m.role, text.as_str()),
                other => panic!("unexpected content {other:?}"),
            })
            .collect();
        assert_eq!(
            texts,
            [
                (Role::User, RETRY_TRUNCATION_MARKER),
                (Role::Assistant, "found it")
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

    #[test]
    fn drop_oldest_rounds_needs_a_reply_to_keep() {
        let thinking_only = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "hmm".to_owned(),
                signature: "sig".to_owned(),
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

    // ── synthesize_post_compact_message ──

    #[test]
    fn synthesize_post_compact_prepends_summary_prefix() {
        let m = synthesize_post_compact_message("did X, Y, Z");
        assert_eq!(m.role, Role::User);
        let ContentBlock::Text { text } = &m.content[0] else {
            panic!("expected text block");
        };
        assert!(text.contains("This conversation has been compacted"));
        assert!(text.contains("did X, Y, Z"));
    }

    // ── strip_synthetic_post_compact_prefix ──

    #[test]
    fn strip_synthetic_post_compact_prefix_removes_only_the_prefix_block() {
        let mut m = synthesize_post_compact_message("internal summary");
        m.content.push(ContentBlock::Text {
            text: "real prompt".to_owned(),
        });

        assert!(strip_synthetic_post_compact_prefix(&mut m));
        assert_eq!(m.content.len(), 1);
        assert!(matches!(&m.content[0], ContentBlock::Text { text } if text == "real prompt"));
    }

    #[test]
    fn strip_synthetic_post_compact_prefix_leaves_regular_user_text() {
        let mut m = Message::user("This conversation should stay visible");

        assert!(!strip_synthetic_post_compact_prefix(&mut m));
        assert_eq!(m.content.len(), 1);
    }

    #[test]
    fn strip_synthetic_post_compact_prefix_leaves_assistant_messages() {
        let mut m = Message::assistant(format!(
            "{}\n\nnot a synthetic user root",
            SUMMARY_PREFIX.trim(),
        ));

        assert!(!strip_synthetic_post_compact_prefix(&mut m));
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.content.len(), 1);
    }

    #[test]
    fn strip_synthetic_post_compact_prefix_leaves_non_text_first_block() {
        let mut m = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_owned(),
                content: SUMMARY_PREFIX.trim().to_owned(),
                is_error: false,
            }],
        };

        assert!(!strip_synthetic_post_compact_prefix(&mut m));
        assert!(matches!(m.content[0], ContentBlock::ToolResult { .. }));
    }
}
