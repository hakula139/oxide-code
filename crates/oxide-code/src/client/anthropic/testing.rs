//! Shared test fixtures for the Anthropic client and its consumers
//! (`agent::tests`, `session::title_generator::tests`).

use std::sync::{Arc, Mutex};

use super::Client;
use crate::config::{Auth, Config, PromptCacheTtl};
use crate::tui::theme::Theme;

/// Minimal [`Config`] suitable for unit and wiremock tests. Defaults
/// match every existing call site: `max_tokens = 128`, `thinking = None`,
/// `show_thinking = false`.
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
    }
}

/// [`Client`] on top of [`test_config`], with a fixed session id so the
/// wire headers carry a deterministic `x-claude-code-session-id`.
pub(crate) fn test_client(base_url: impl Into<String>, auth: Auth, model: &str) -> Client {
    Client::new(test_config(base_url, auth, model), Some("sid".to_owned())).unwrap()
}

/// Non-streaming Messages-API response body with the given text content.
/// Model is hardcoded; assertions in tests inspect request-side model
/// selection, never response-side.
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

/// Slot for data captured by a wiremock responder closure (request body,
/// header pair, etc.). The `Arc<Mutex<Option<T>>>` triple is needed
/// because wiremock responders are `Fn`, capture is mutable, and tests
/// need to inspect the value across the await boundary.
pub(crate) type Captured<T> = Arc<Mutex<Option<T>>>;

pub(crate) fn captured<T>() -> Captured<T> {
    Arc::new(Mutex::new(None))
}
