//! Shared test fixtures for the Anthropic client and its consumers.

use std::sync::{Arc, Mutex};

use reqwest::header::HeaderMap;

use super::Client;
use crate::config::auth::{Auth, AuthSource};
use crate::config::gateway::GatewayCompat;
use crate::config::{CompactionConfig, Config, PromptCacheTtl, StatusLineSegment};
use crate::model::ModelSpec;
use crate::tui::theme::Theme;

/// Minimal [`Config`] for unit / wiremock tests.
pub(crate) fn test_config(base_url: impl Into<String>, auth: Auth, model: &str) -> Config {
    Config {
        auth,
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
        show_thinking: false,
        show_welcome: true,
        status_line: StatusLineSegment::DEFAULT.to_vec(),
        theme: Theme::default(),
        theme_name: "mocha".to_owned(),
    }
}

/// Fixed session id so wire headers carry a deterministic `x-claude-code-session-id`.
pub(crate) fn test_client(base_url: impl Into<String>, auth: Auth, model: &str) -> Client {
    Client::new(test_config(base_url, auth, model), Some("sid".to_owned())).unwrap()
}

/// Non-streaming response body with the given text. Model is hardcoded; tests assert request side.
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
    use std::fmt::Write as _;

    let frames = [
        serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-sonnet-5-5", "usage": {"input_tokens": 1_000, "output_tokens": 0}}}),
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": start_text}}),
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": delta_text}}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 100}}),
        serde_json::json!({"type": "message_stop"}),
    ];
    let mut body = String::new();
    for frame in &frames {
        _ = writeln!(
            body,
            "event: {}\ndata: {frame}\n",
            frame["type"].as_str().unwrap()
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

/// Slot for data captured by a wiremock responder. `Fn` capture + cross-await inspection require
/// the full `Arc<Mutex<Option<T>>>` triple.
pub(crate) type Captured<T> = Arc<Mutex<Option<T>>>;

pub(crate) fn captured<T>() -> Captured<T> {
    Arc::new(Mutex::new(None))
}
