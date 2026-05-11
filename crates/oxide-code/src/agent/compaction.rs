//! `/compact` driver: streams a one-shot summarization request through the live [`Client`] and
//! returns the trimmed summary text. The driver itself does not touch session state — that is
//! the caller's job (see `apply_compact` in the agent-loop dispatch).
//!
//! Wire shape: an empty tool list and a dedicated minimal system prompt so the model cannot
//! attempt a tool call mid-summary. The transcript is stripped to text-only content blocks
//! before sending — tool-use, tool-result, and thinking blocks are dropped. The rubric (and
//! optional user instructions) ride as a final user message after the stripped transcript.

use anyhow::{Result, bail};
use indoc::{formatdoc, indoc};

use crate::client::anthropic::Client;
use crate::client::anthropic::wire::{Delta, StreamEvent};
use crate::message::{ContentBlock, Message};

/// Minimum messages required for compaction to be worthwhile. Below this, the summary is
/// usually longer than the transcript itself.
const MIN_MESSAGES_FOR_COMPACT: usize = 4;

/// System prompt for the summarization request. Deliberately narrow — the surrounding
/// `SYSTEM_PROMPT_PREFIX` ("You are Claude Code...") is added by the client; this section
/// reframes the model's job for the compaction turn.
pub(crate) const SUMMARIZATION_SYSTEM: &str = indoc! {r"
    You are summarizing a conversation between a software engineer and an AI coding assistant.

    Output ONLY the summary text. Do not call any tools. Do not ask clarifying questions. Do not
    address the engineer directly. Write in plain prose; markdown bullets are fine where they
    aid readability.
"};

/// User-message rubric. Five short asks; the model converges on the right shape without the
/// numbered-section ceremony Claude Code uses.
pub(crate) const SUMMARIZATION_USER_RUBRIC: &str = indoc! {r"
    Summarize the conversation above so another instance of yourself can pick up where this one
    left off. Capture, in this order:

    1. The engineer's overall intent and any constraints they stated.
    2. Key technical decisions made and why.
    3. Files, functions, and code paths touched (full paths when known).
    4. Current state: what is done, what is in progress, what is blocked.
    5. The next concrete step, if one is obvious.

    Be concise — terse bullets beat paragraphs. Preserve exact identifiers, file paths, error
    strings, and command lines verbatim.
"};

/// Prepended to the synthetic post-compact user message materializing the summary into the
/// next turn. Phrasing tells the next-turn model to use the summary rather than re-asking what
/// to do — without this prefix the next turn often redundantly clarifies intent.
pub(crate) const SUMMARY_PREFIX: &str = indoc! {r"
    This conversation has been compacted. The summary below covers the prior work; continue
    from here without re-asking the engineer what to do.
"};

/// Drives the compaction request. Returns the trimmed summary text on success.
///
/// Errors when the transcript is too short to compact, when the API errors mid-stream, or when
/// the model returns an empty / whitespace-only response.
pub(crate) async fn compact_session(
    client: &Client,
    transcript: &[Message],
    instructions: Option<&str>,
) -> Result<String> {
    if transcript.len() < MIN_MESSAGES_FOR_COMPACT {
        bail!(
            "session is too short to compact ({} messages, need at least {MIN_MESSAGES_FOR_COMPACT})",
            transcript.len(),
        );
    }

    let mut messages = strip_to_conversation(transcript);
    if messages.len() < MIN_MESSAGES_FOR_COMPACT {
        bail!(
            "session has too little text to compact ({} text messages, need at least {MIN_MESSAGES_FOR_COMPACT})",
            messages.len(),
        );
    }
    messages.push(Message::user(build_user_message(instructions)));

    let mut rx = client.stream_message(&messages, &[SUMMARIZATION_SYSTEM], None, &[])?;

    let mut summary = String::new();
    while let Some(event) = rx.recv().await {
        match event? {
            StreamEvent::ContentBlockDelta {
                delta: Delta::TextDelta { text },
                ..
            } => summary.push_str(&text),
            StreamEvent::Error { error } => bail!("API error during compaction: {}", error.message),
            StreamEvent::MessageStop => break,
            // ContentBlockStart/Stop, MessageStart/Delta, Ping, thinking deltas, etc. — ignore.
            _ => {}
        }
    }

    let trimmed = summary.trim();
    if trimmed.is_empty() {
        bail!("compaction returned empty summary");
    }
    Ok(trimmed.to_owned())
}

/// Strips tool-use / tool-result / thinking blocks from the transcript. Keeps text-only blocks
/// and drops any message that ends up with no content.
fn strip_to_conversation(transcript: &[Message]) -> Vec<Message> {
    transcript
        .iter()
        .filter_map(|m| {
            let kept: Vec<ContentBlock> = m
                .content
                .iter()
                .filter(|b| matches!(b, ContentBlock::Text { .. }))
                .cloned()
                .collect();
            if kept.is_empty() {
                None
            } else {
                Some(Message {
                    role: m.role,
                    content: kept,
                })
            }
        })
        .collect()
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

/// Composes the synthetic post-compact user message. The boundary marker plus the summary
/// itself; lands in the JSONL as a normal `Entry::Message` and in the next turn's `messages`
/// array as the new chain head.
pub(crate) fn synthesize_post_compact_message(summary: &str) -> Message {
    Message::user(formatdoc! {"
        {prefix}

        {summary}
    ", prefix = SUMMARY_PREFIX.trim(), summary = summary.trim()})
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::client::anthropic::testing::{Captured, api_key, captured, test_client};
    use crate::message::Role;

    // ── strip_to_conversation ──

    #[test]
    fn strip_to_conversation_drops_tool_use_and_tool_result_blocks() {
        let messages = vec![
            Message::user("hi"),
            Message {
                role: Role::Assistant,
                content: vec![
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
            Message::assistant("done"),
        ];
        let stripped = strip_to_conversation(&messages);
        assert_eq!(stripped.len(), 3, "tool-result-only message dropped");
        assert!(
            stripped[1]
                .content
                .iter()
                .all(|b| matches!(b, ContentBlock::Text { .. }))
        );
        assert_eq!(stripped[1].content.len(), 1);
    }

    #[test]
    fn strip_to_conversation_drops_thinking_blocks() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "pondering".to_owned(),
                    signature: "sig".to_owned(),
                },
                ContentBlock::Text {
                    text: "answer".to_owned(),
                },
            ],
        }];
        let stripped = strip_to_conversation(&messages);
        assert_eq!(stripped.len(), 1);
        assert_eq!(stripped[0].content.len(), 1);
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

    // ── compact_session ──

    fn streamed_summary_body(text: &str) -> String {
        use std::fmt::Write as _;

        let frames = [
            json!({"type": "message_start", "message": {"id": "m", "model": "claude-haiku-4-5"}}).to_string(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}).to_string(),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}).to_string(),
            json!({"type": "content_block_stop", "index": 0}).to_string(),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}).to_string(),
            json!({"type": "message_stop"}).to_string(),
        ];
        let mut body = String::new();
        for frame in &frames {
            _ = writeln!(body, "event: ping\ndata: {frame}\n");
        }
        body
    }

    fn fake_transcript() -> Vec<Message> {
        vec![
            Message::user("fix the bug"),
            Message::assistant("looking now"),
            Message::user("any progress?"),
            Message::assistant("found it"),
        ]
    }

    #[tokio::test]
    async fn compact_session_too_short_transcript_errors_before_request() {
        let server = MockServer::start().await;
        let client = test_client(server.uri(), api_key(), "claude-haiku-4-5");
        let err = compact_session(&client, &[Message::user("hi")], None)
            .await
            .expect_err("must refuse short transcript");
        assert!(format!("{err:#}").contains("too short to compact"));
    }

    #[tokio::test]
    async fn compact_session_refuses_when_stripped_transcript_is_too_short() {
        let server = MockServer::start().await;
        let client = test_client(server.uri(), api_key(), "claude-haiku-4-5");
        let transcript = vec![
            Message::user("fix the bug"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"path": "/tmp/a"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_owned(),
                    content: "file body".to_owned(),
                    is_error: false,
                }],
            },
            Message::assistant("done"),
        ];

        let err = compact_session(&client, &transcript, None)
            .await
            .expect_err("stripped transcript must still satisfy the useful-work threshold");
        assert!(format!("{err:#}").contains("too little text"));
    }

    #[tokio::test]
    async fn compact_session_returns_trimmed_summary_on_happy_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(streamed_summary_body("  fixed login bug  \n"))
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let client = test_client(server.uri(), api_key(), "claude-haiku-4-5");
        let summary = compact_session(&client, &fake_transcript(), None)
            .await
            .unwrap();
        assert_eq!(summary, "fixed login bug");
    }

    #[tokio::test]
    async fn compact_session_empty_summary_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(streamed_summary_body("   \n  "))
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let client = test_client(server.uri(), api_key(), "claude-haiku-4-5");
        let err = compact_session(&client, &fake_transcript(), None)
            .await
            .expect_err("empty summary must error");
        assert!(format!("{err:#}").contains("empty summary"));
    }

    #[tokio::test]
    async fn compact_session_request_strips_non_text_blocks_and_sends_no_tools() {
        // Capture the outgoing request to verify wire shape.
        let server = MockServer::start().await;
        let sink: Captured<String> = captured();
        let sink_clone = std::sync::Arc::clone(&sink);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(move |req: &Request| {
                *sink_clone.lock().unwrap() = Some(String::from_utf8_lossy(&req.body).into_owned());
                ResponseTemplate::new(200)
                    .set_body_string(streamed_summary_body("done"))
                    .insert_header("content-type", "text/event-stream")
            })
            .mount(&server)
            .await;

        let client = test_client(server.uri(), api_key(), "claude-haiku-4-5");
        let mut transcript = fake_transcript();
        transcript[1] = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "let me check".to_owned(),
                },
                ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"path": "/tmp/a"}),
                },
            ],
        };
        compact_session(&client, &transcript, Some("focus on auth"))
            .await
            .unwrap();

        let body = sink.lock().unwrap().clone().expect("body captured");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v.get("tools").is_none(), "tools omitted: {body}");

        let body_text = body.as_str();
        assert!(
            !body_text.contains("\"tool_use\""),
            "tool_use stripped: {body}"
        );
        assert!(
            !body_text.contains("\"tool_result\""),
            "tool_result stripped: {body}"
        );
        assert!(
            body_text.contains("focus on auth"),
            "instructions threaded: {body}"
        );
    }

    #[tokio::test]
    async fn compact_session_surfaces_stream_error_event() {
        // Stream that opens cleanly then emits an in-band error frame (rate limit / overload) —
        // the bail path inside the receive loop, distinct from HTTP-level failures.
        let body = "event: ping\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"claude-haiku-4-5\"}}\n\nevent: ping\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"servers overloaded\"}}\n\n";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let client = test_client(server.uri(), api_key(), "claude-haiku-4-5");
        let err = compact_session(&client, &fake_transcript(), None)
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

        let client = test_client(server.uri(), api_key(), "claude-haiku-4-5");
        let err = compact_session(&client, &fake_transcript(), None)
            .await
            .expect_err("expected error");
        assert!(format!("{err:#}").contains("500") || format!("{err:#}").contains("server_error"));
    }
}
