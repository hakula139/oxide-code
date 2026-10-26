//! Test fixtures for the Anthropic client that only this crate's tests use.

use std::sync::Arc;

use bytes::Bytes;
use tokio_util::task::TaskTracker;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::Client;
use super::testing::{DEVICE_ID, api_key};
use super::transport::MessagesRequest;
use crate::config::Config;
use crate::config::auth::{Auth, AuthSource};
use crate::config::testing::test_config;
use crate::model::TokenUsage;
use crate::util::tls::Trust as _;
use crate::util::tls::testing::NoRoots;

/// [`client_for`] on [`test_config`].
pub(crate) fn test_client(base_url: impl Into<String>, auth: Auth, model: &str) -> Client {
    client_for(&test_config(base_url, auth, model))
}

/// Client on `config` with a fixed session id, so wire headers carry a deterministic
/// `x-claude-code-session-id`.
pub(crate) fn client_for(config: &Config) -> Client {
    Client::new(
        config,
        NoRoots,
        Some("sid".to_owned()),
        DEVICE_ID.to_owned(),
    )
    .unwrap()
}

/// An HTTP client for plain-HTTP mock servers, which reads no system trust store.
pub(super) fn plain_http() -> reqwest::Client {
    NoRoots
        .apply(reqwest::Client::builder())
        .unwrap()
        .build()
        .unwrap()
}

/// A `/v1/messages` call to `url` with an empty body and an API key.
pub(super) fn messages_request(url: impl Into<String>) -> MessagesRequest {
    MessagesRequest {
        url: url
            .into()
            .parse()
            .expect("a test messages URL must be a URL"),
        betas: String::new(),
        session_id: "sid".to_owned(),
        body: Bytes::from_static(b"{}"),
        auth: api_key(),
        trust: Arc::new(NoRoots),
        reloads: TaskTracker::new(),
    }
}

/// A mock server answering `/v1/messages` with `responses` in order, one request each, then
/// repeating the last one.
pub(crate) async fn serve(responses: Vec<ResponseTemplate>) -> MockServer {
    let server = MockServer::start().await;
    let last = responses.len() - 1;
    for (i, response) in responses.into_iter().enumerate() {
        let mock = Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(response);
        let mock = if i == last {
            mock
        } else {
            mock.up_to_n_times(1)
        };
        mock.mount(&server).await;
    }
    server
}

pub(super) async fn request_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("the server records requests")
        .len()
}

/// The `x-stainless-retry-count` of each request `server` received, in order.
pub(super) async fn retry_counts(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("the server records requests")
        .iter()
        .map(|request| {
            request.headers["x-stainless-retry-count"]
                .to_str()
                .expect("the retry count is ASCII")
                .to_owned()
        })
        .collect()
}

/// Usage that [`completion_body`] reports.
pub(crate) const COMPLETION_USAGE: TokenUsage = TokenUsage::new(5, 3);

/// Non-streaming response body with the given text, billed at [`COMPLETION_USAGE`]. The model is
/// fixed because `CompletionResponse` deserializes only `content` and `usage`.
pub(crate) fn completion_body(text: &str) -> String {
    serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-5-5",
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": text}],
        "usage": COMPLETION_USAGE,
    })
    .to_string()
}

pub(super) fn auth_token() -> Auth {
    Auth::new(AuthSource::AuthTokenEnv, "g")
}

pub(super) fn oauth() -> Auth {
    Auth::new(AuthSource::ClaudeCodeLogin, "t")
}
