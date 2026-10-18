//! Shared test fixtures for the Anthropic client and its consumers.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use indoc::writedoc;
use reqwest::header::HeaderMap;
use wiremock::ResponseTemplate;

use super::Client;
use super::transport::MessagesRequest;
use crate::config::auth::{Auth, AuthSource};
use crate::config::compaction::CompactionConfig;
use crate::config::gateway::GatewayCompat;
use crate::config::{Config, DEFAULT_THEME, PromptCacheTtl, StatusLineSegment, ThemeConfig};
use crate::model::ModelSpec;
use crate::util::tls::Trust;

pub(crate) const DEVICE_ID: &str = "did";

/// Minimal [`Config`] for unit / wiremock tests.
pub(crate) fn test_config(base_url: impl Into<String>, auth: Auth, model: &str) -> Config {
    Config {
        auth: Some(auth),
        signed_out_notice: None,
        base_url: base_url.into(),
        extra_ca_certs: None,
        gateway: GatewayCompat {
            custom_headers: HeaderMap::new(),
            attribution: true,
            structured_outputs: true,
        },
        model: ModelSpec::resolve(model.to_owned()),
        effort: None,
        max_tokens: Some(128),
        max_tool_rounds: None,
        prompt_cache_ttl: PromptCacheTtl::OneHour,
        compaction: CompactionConfig::disabled(),
        permission: crate::permission::Policy::default(),
        workspace: crate::permission::Workspace::new(std::path::Path::new("/repo")),
        show_thinking: false,
        show_welcome: true,
        status_line: StatusLineSegment::DEFAULT.to_vec(),
        theme: ThemeConfig {
            base: DEFAULT_THEME.to_owned(),
            overrides: toml::Table::new(),
        },
        provenance: crate::config::provenance::Provenance::default(),
    }
}

/// [`client_for`] on [`test_config`].
pub(crate) fn test_client(base_url: impl Into<String>, auth: Auth, model: &str) -> Client {
    client_for(&test_config(base_url, auth, model))
}

/// Client on `config` with a fixed session id, so wire headers carry a deterministic
/// `x-claude-code-session-id`.
pub(crate) fn client_for(config: &Config) -> Client {
    Client::new(
        config,
        Trust::NoRoots,
        Some("sid".to_owned()),
        DEVICE_ID.to_owned(),
    )
    .unwrap()
}

/// An HTTP client for plain-HTTP mock servers, which reads no system trust store.
pub(super) fn plain_http() -> reqwest::Client {
    Trust::NoRoots
        .apply(reqwest::Client::builder())
        .unwrap()
        .build()
        .unwrap()
}

/// A `/v1/messages` call to `url` with an empty body and an API key.
pub(super) fn messages_request(url: impl Into<String>) -> MessagesRequest {
    let auth = Auth::new(AuthSource::ApiKeyEnv, "k");
    MessagesRequest {
        url: url.into(),
        betas: String::new(),
        session_id: "sid".to_owned(),
        body: Bytes::from_static(b"{}"),
        auth: auth.source(),
        credential: auth.header().unwrap(),
    }
}

/// Non-streaming response body with the given text. The model is fixed because `CompletionResponse`
/// deserializes only `content`.
pub(crate) fn completion_body(text: &str) -> String {
    serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-5-5",
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": 5, "output_tokens": 3}
    })
    .to_string()
}

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
pub(crate) fn sse_response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_string(body)
        .insert_header("content-type", "text/event-stream")
}

/// Server-sent events body carrying each frame under an event named for its `type`.
pub(crate) fn sse_body(frames: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for frame in frames {
        let event = frame["type"].as_str().unwrap();
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

pub(crate) fn auth_token() -> Auth {
    Auth::new(AuthSource::AuthTokenEnv, "g")
}

pub(crate) fn oauth() -> Auth {
    Auth::new(AuthSource::ClaudeCodeLogin, "t")
}

/// Slot for data captured by a wiremock responder. Capturing it in an `Fn` and inspecting it
/// across awaits takes the full `Arc<Mutex<Option<T>>>`.
pub(crate) type Captured<T> = Arc<Mutex<Option<T>>>;

pub(crate) fn captured<T>() -> Captured<T> {
    Arc::new(Mutex::new(None))
}
