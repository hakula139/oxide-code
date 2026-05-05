//! Shared test fixtures for the Anthropic client and its consumers.

use std::sync::{Arc, Mutex};

use super::Client;
use crate::config::{Auth, Config, PromptCacheTtl};
use crate::tui::theme::Theme;

/// Minimal [`Config`] for unit / wiremock tests.
pub(crate) fn test_config(base_url: impl Into<String>, auth: Auth, model: &str) -> Config {
    Config {
        auth,
        base_url: base_url.into(),
        model: model.to_owned(),
        effort: None,
        max_tokens: 128,
        prompt_cache_ttl: PromptCacheTtl::OneHour,
        thinking: None,
        show_thinking: false,
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
        "model": "claude-haiku-4-5",
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": 5, "output_tokens": 3}
    })
    .to_string()
}

pub(crate) fn api_key() -> Auth {
    Auth::ApiKey("k".to_owned())
}

pub(crate) fn oauth() -> Auth {
    Auth::OAuth("t".to_owned())
}

/// Slot for data captured by a wiremock responder. `Fn` capture + cross-await inspection require
/// the full `Arc<Mutex<Option<T>>>` triple.
pub(crate) type Captured<T> = Arc<Mutex<Option<T>>>;

pub(crate) fn captured<T>() -> Captured<T> {
    Arc::new(Mutex::new(None))
}
