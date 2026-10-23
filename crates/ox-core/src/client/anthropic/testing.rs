//! Shared test fixtures for the Anthropic client and its consumers.

use std::fmt::Write as _;

use indoc::writedoc;
use wiremock::ResponseTemplate;

use crate::config::auth::{Auth, AuthSource};

pub(crate) const DEVICE_ID: &str = "did";

/// Streamed `end_turn` reply whose text opens as `start_text` and ends with one `delta_text`
/// delta, billed at 1,000 input and 100 output tokens.
pub(crate) fn text_stream_body(start_text: &str, delta_text: &str) -> String {
    sse_body(&[
        serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-sonnet-5-5", "usage": {"input_tokens": 1_000, "output_tokens": 0}}}),
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": start_text}}),
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": delta_text}}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 100}}),
        serde_json::json!({"type": "message_stop"}),
    ])
}

/// Streamed `tool_use` reply calling `name` with `input_json`, billed like [`text_stream_body`].
pub(crate) fn tool_use_stream_body(id: &str, name: &str, input_json: &str) -> String {
    sse_body(&[
        serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-sonnet-5-5", "usage": {"input_tokens": 1_000, "output_tokens": 0}}}),
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": name}}),
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": input_json}}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 100}}),
        serde_json::json!({"type": "message_stop"}),
    ])
}

/// Successful response streaming `body` as server-sent events.
pub(crate) fn sse_response(body: impl Into<Vec<u8>>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.into(), "text/event-stream")
}

/// Server-sent events body carrying each frame under an event named for its `type`.
pub(crate) fn sse_body(frames: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for frame in frames {
        let event = frame["type"].as_str().expect("each frame names its type");
        _ = writedoc!(
            body,
            "
            event: {event}
            data: {frame}

            ",
        );
    }
    body
}

pub(crate) fn api_key() -> Auth {
    Auth::new(AuthSource::ApiKeyEnv, "k")
}
