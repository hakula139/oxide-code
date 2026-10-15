//! Test-only [`Conversation`] fixtures. Child of `driver` so it can build the private fields.

use std::sync::Arc;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::Conversation;
use crate::client::anthropic::testing::{api_key, test_client, text_stream_body};
use crate::file_tracker::FileTracker;
use crate::prompt;
use crate::session::handle;
use crate::session::store::SessionStore;
use crate::tool::ToolRegistry;

pub(super) const MODEL: &str = "claude-opus-5-5";
pub(super) const SESSION_CONTEXT: &str = "<system-reminder>session context</system-reminder>";

/// Non-interactive conversation on a fresh session in `store`. Its prompt carries
/// [`SESSION_CONTEXT`] in place of the rendered user context, so tests can tell it apart from a
/// rebuilt one.
pub(super) async fn conversation(base_url: String, store: &SessionStore) -> Conversation {
    let cwd = std::env::current_dir().unwrap();
    let mut prompt = prompt::build_prompt(MODEL, &cwd).await;
    SESSION_CONTEXT.clone_into(&mut prompt.user_context);
    Conversation {
        client: test_client(base_url, api_key(), MODEL),
        tools: Arc::new(ToolRegistry::new(Vec::new())),
        session: handle::start(store, MODEL),
        messages: Vec::new(),
        prompt,
        prompt_unrecorded: true,
        file_tracker: Arc::new(FileTracker::default()),
        cwd,
        interactive: false,
        auto_compaction_failures: 0,
        last_usage: None,
        displayed_usage: None,
        total_estimated_cost_usd: 0.0,
    }
}

/// Answers every Messages request with a streamed reply of `text`.
pub(super) async fn mount_reply(server: &MockServer, text: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(text_stream_body("", text))
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(server)
        .await;
}

/// Request bodies the server received, in arrival order.
pub(super) async fn request_bodies(server: &MockServer) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

/// The system blocks of `request` without the billing attestation, whose body hash varies with
/// every request.
pub(super) fn system_prompt(request: &serde_json::Value) -> Vec<&serde_json::Value> {
    request["system"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|block| {
            !block["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("x-anthropic-billing-header:"))
        })
        .collect()
}
