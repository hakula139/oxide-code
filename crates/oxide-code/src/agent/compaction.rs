//! `/compact` summarization request builder and stream collector.
//!
//! Compaction sends text-only transcript messages, a dedicated summarization system prompt, and
//! no tool definitions. Session mutation happens in the agent loop after the summary succeeds.

use anyhow::{Result, bail};
use indoc::{formatdoc, indoc};

use crate::client::anthropic::Client;
use crate::client::anthropic::wire::{ContentBlockInfo, Delta, StreamEvent};
use crate::message::{ContentBlock, Message};

/// Minimum messages required for compaction to be worthwhile. Below this, the summary is
/// usually longer than the transcript itself.
const MIN_MESSAGES_FOR_COMPACT: usize = 4;

/// System prompt for the summarization request. The client still adds the regular Claude Code
/// prefix, so this only reframes the compaction turn.
const SUMMARIZATION_SYSTEM: &str = indoc! {r"
    You are summarizing a conversation between a software engineer and an AI coding assistant.

    Output ONLY the summary text. Do not call any tools. Do not ask clarifying questions. Do
    not address the engineer directly. Write in plain prose. Markdown bullets are fine where
    they aid readability.
"};

/// User-message rubric. Five short asks keep the summary compact without named sections.
const SUMMARIZATION_USER_RUBRIC: &str = indoc! {r"
    Summarize the conversation above so another instance of yourself can pick up where this one left
    off. Capture, in this order:

    1. The engineer's overall intent and any constraints they stated.
    2. Key technical decisions made and why.
    3. Files, functions, and code paths touched (full paths when known).
    4. Current state: what is done, what is in progress, what is blocked.
    5. The next concrete step, if one is obvious.

    Be concise. Terse bullets beat paragraphs. Preserve exact identifiers, file paths, error strings,
    and command lines verbatim.
"};

/// Prefix for the synthetic post-compact user message. It tells the next turn to continue from
/// the summary.
const SUMMARY_PREFIX: &str = indoc! {r"
    This conversation has been compacted. The summary below covers the prior work. Continue from here
    without re-asking the engineer what to do.
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
            StreamEvent::ContentBlockStart {
                content_block: ContentBlockInfo::Text { text },
                ..
            }
            | StreamEvent::ContentBlockDelta {
                delta: Delta::TextDelta { text },
                ..
            } => summary.push_str(&text),
            StreamEvent::Error { error } => bail!("API error during compaction: {}", error.message),
            StreamEvent::MessageStop => break,
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

/// Composes the synthetic post-compact root message for the next turn.
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

    // ── compact_session ──

    fn streamed_summary_body(text: &str) -> String {
        streamed_summary_body_parts("", text)
    }

    fn streamed_summary_body_parts(start_text: &str, delta_text: &str) -> String {
        use std::fmt::Write as _;

        let frames = [
            json!({"type": "message_start", "message": {"id": "m", "model": "claude-haiku-4-5"}}).to_string(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": start_text}}).to_string(),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": delta_text}}).to_string(),
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
    async fn compact_session_collects_initial_text_from_content_block_start() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(streamed_summary_body_parts("  fixed", " login bug  \n"))
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
        transcript.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: "file body".to_owned(),
                is_error: false,
            }],
        });
        compact_session(&client, &transcript, Some("focus on auth"))
            .await
            .unwrap();

        let body = sink.lock().unwrap().clone().expect("body captured");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v.get("tools").is_none(), "tools omitted: {body}");

        let body_text = body.as_str();
        assert!(
            !body_text.contains(r#""tool_use""#),
            "tool_use stripped: {body}"
        );
        assert!(
            !body_text.contains(r#""tool_result""#),
            "tool_result stripped: {body}"
        );
        assert!(
            body_text.contains("focus on auth"),
            "instructions threaded: {body}"
        );
    }

    #[tokio::test]
    async fn compact_session_surfaces_stream_error_event() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(indoc! {r#"
                        event: ping
                        data: {"type":"message_start","message":{"id":"m","model":"claude-haiku-4-5"}}

                        event: ping
                        data: {"type":"error","error":{"type":"overloaded_error","message":"servers overloaded"}}

                    "#})
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
}
