//! Test-only [`Engine`] fixtures. Child of `engine` so it can build the private fields.

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use super::{Engine, EngineHandle, EngineTask};
use crate::client::anthropic::testing::{
    api_key, sse_response, test_client, text_stream_body, tool_use_stream_body,
};
use crate::file_tracker::FileTracker;
use crate::model::ModelSpec;
use crate::prompt;
use crate::session::handle;
use crate::session::store::SessionStore;
use crate::tool::ToolRegistry;
use crate::util::path::testing::HOME_VAR;

pub(super) const MODEL: &str = "claude-opus-5-5";
pub(super) const SESSION_CONTEXT: &str = "<system-reminder>session context</system-reminder>";

/// Non-interactive engine on a fresh session in `store`, working in `cwd`. Its prompt carries
/// [`SESSION_CONTEXT`] in place of the rendered user context, so tests can tell it apart from a
/// rebuilt one. Pass an empty directory, and run under [`with_pinned_home`], so the context never
/// reads the developer's instruction files or git state.
pub(crate) async fn engine(base_url: String, store: &SessionStore, cwd: &Path) -> Engine {
    let mut prompt = prompt::build_prompt(&ModelSpec::resolve(MODEL.to_owned()), cwd).await;
    SESSION_CONTEXT.clone_into(&mut prompt.user_context);
    let client = test_client(base_url, api_key(), MODEL);
    Engine {
        policy: client.permission().clone(),
        client,
        tools: Arc::new(ToolRegistry::new(Vec::new())),
        store: store.clone(),
        session: handle::start(store, MODEL),
        messages: Vec::new(),
        compact: None,
        tool_metadata: HashMap::new(),
        prompt,
        prompt_unrecorded: true,
        file_tracker: Arc::new(FileTracker::default()),
        cwd: cwd.to_owned(),
        interactive: false,
        auto_compaction_failures: 0,
        last_usage: None,
        displayed_usage: None,
        total_estimated_cost_usd: 0.0,
    }
}

/// Handle to an engine that has already stopped, as after a panic.
pub(crate) fn stopped_engine() -> EngineHandle {
    let (ops, _) = mpsc::channel(1);
    let (_, events) = mpsc::unbounded_channel();
    EngineHandle {
        ops,
        events,
        task: EngineTask(tokio::spawn(async { None })),
    }
}

/// Runs `test` with the home directory pinned to an empty one, so context loads find no user
/// instructions even while parallel tests swap it. Boxed, since a whole test body is too large a
/// future to hold on the stack.
pub(crate) fn with_pinned_home<'a>(
    test: impl Future<Output = ()> + 'a,
) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
    Box::pin(async {
        let home = tempfile::tempdir().unwrap();
        temp_env::async_with_vars([(HOME_VAR, Some(home.path()))], test).await;
    })
}

/// Answers every Messages request with a streamed reply of `text`.
pub(crate) async fn mount_reply(server: &MockServer, text: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(text_stream_body("", text)))
        .mount(server)
        .await;
}

/// Answers the next Messages request with a call to the tool `name`. Mount it before the reply that
/// should follow.
pub(super) async fn mount_tool_call_once(server: &MockServer, name: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(tool_use_stream_body("call-1", name, "{}")))
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// Answers the next Messages request only after a minute, so it stays in flight until the test
/// cancels or quits it. Mount it before the reply that should follow.
pub(super) async fn mount_stalled_reply_once(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            sse_response(text_stream_body("", "too late")).set_delay(Duration::from_secs(60)),
        )
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// Waits until `server` has received a request, so the engine is mid-request.
pub(super) async fn until_requested(server: &MockServer) {
    while server.received_requests().await.unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
}

/// Request bodies the server received, in arrival order.
pub(crate) async fn request_bodies(server: &MockServer) -> Vec<serde_json::Value> {
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
