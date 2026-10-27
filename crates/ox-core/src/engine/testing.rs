//! Test fixtures, shared with other crates' tests, that build [`Engine`]s and mock API replies.

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use super::{Engine, EngineHandle, EnginePanic, EngineTask, SessionParts};
use crate::client::anthropic::testing::{
    DEVICE_ID, api_key, sse_response, text_stream_body, tool_use_stream_body,
};
use crate::config::oauth::ClaudeCodeKeychain;
use crate::config::testing::test_config;
use crate::permission::Workspace;
use crate::protocol::Event;
use crate::session::handle::{self, LoadedTranscript};
use crate::session::store::SessionStore;
use crate::tool::ToolRegistry;
use crate::util::path::testing::pinned_home;
use crate::util::tls::testing::NoRoots;

// ── Engine ──

pub(super) const MODEL: &str = "claude-opus-5-5";
pub(super) const SESSION_CONTEXT: &str = "<system-reminder>session context</system-reminder>";

/// Engine without titles on a fresh session in `store`, working in `cwd`. Its prompt carries
/// [`SESSION_CONTEXT`] in place of the rendered user context, so tests can tell it apart from a
/// rebuilt one. Pass an empty directory, and run under [`with_pinned_home`], so the context never
/// reads the developer's instruction files or git state.
pub async fn engine(base_url: String, store: &SessionStore, cwd: &Path) -> Engine {
    let mut config = test_config(base_url, api_key(), MODEL);
    config.workspace = Workspace::new(cwd);
    let session = SessionParts {
        store: store.clone(),
        handle: handle::start(store, MODEL, None),
        transcript: LoadedTranscript::default(),
    };
    let mut engine = Engine::new(&config, NoRoots, DEVICE_ID.to_owned(), session, false)
        .await
        .expect("the test engine builds");
    SESSION_CONTEXT.clone_into(&mut engine.prompt.user_context);
    "http://127.0.0.1:9/token".clone_into(&mut engine.oauth_token_url);
    engine.claude_code_keychain = ClaudeCodeKeychain::NONE;
    engine.with_tools(ToolRegistry::new(Vec::new()))
}

/// An [`EngineHandle`] that ends each prompt at once and reports `failure` from shutdown, standing
/// in for an engine whose session cannot be saved.
pub fn unsaved_session_handle(failure: &str) -> EngineHandle {
    let (ops, mut pending) = mpsc::channel(1);
    let (events, received) = mpsc::unbounded_channel();
    let failure = failure.to_owned();
    let task = tokio::spawn(async move {
        while pending.recv().await.is_some() {
            _ = events.send(Event::TurnComplete);
        }
        Some(failure)
    });
    EngineHandle {
        ops,
        events: received,
        task: EngineTask(task),
    }
}

impl Engine {
    /// This engine offering `tools` in place of its own.
    #[must_use]
    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Arc::new(tools);
        self
    }

    /// This engine for a run nobody attends, as
    /// [`Config::set_unattended`](crate::config::Config::set_unattended) sets up.
    #[must_use]
    pub fn unattended(mut self) -> Self {
        self.policy.set_unattended();
        self
    }
}

/// Handle to an engine whose task panicked with `engine bug`, closing both channels.
pub fn panicked_engine() -> EngineHandle {
    let (ops, _) = mpsc::channel(1);
    let (_, events) = mpsc::unbounded_channel();
    EngineHandle {
        ops,
        events,
        task: EngineTask(tokio::spawn(async { panic!("engine bug") })),
    }
}

/// The message of an [`EnginePanic`] whose `panic!` took a string literal.
pub fn panic_message(panic: EnginePanic) -> &'static str {
    let payload = std::panic::catch_unwind(AssertUnwindSafe(|| panic.resume())).unwrap_err();
    payload
        .downcast_ref::<&str>()
        .expect("the panic message is a literal")
}

/// Runs `test` with the home directory pinned to an empty one and `XDG_CONFIG_HOME` and
/// `CLAUDE_CONFIG_DIR` unset, so context loads find no user instructions or Claude Code login even
/// while parallel tests swap them. Boxed, since a whole test body is too large a future to hold on
/// the stack.
pub fn with_pinned_home<'a>(
    test: impl Future<Output = ()> + 'a,
) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
    Box::pin(async {
        let home = tempfile::tempdir().expect("the temp home is created");
        temp_env::async_with_vars(pinned_home(home.path()), test).await;
    })
}

/// Answers every Messages request with a streamed reply of `text`.
pub async fn mount_reply(server: &MockServer, text: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(text_stream_body("", text)))
        .mount(server)
        .await;
}

/// Answers the next Messages request with the tool call `id` to `name` with `input_json`. Mount it
/// before the reply that should follow.
pub async fn mount_tool_call_once(server: &MockServer, id: &str, name: &str, input_json: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(tool_use_stream_body(id, name, input_json)))
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// Answers the next Messages request only after a minute, so it stays in flight until the test
/// cancels or quits it. Mount it before the reply that should follow.
pub async fn mount_stalled_reply_once(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            sse_response(text_stream_body("", "too late")).set_delay(Duration::from_secs(60)),
        )
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// Waits until `server` has received `count` requests, so the engine is mid-request.
pub async fn until_requested(server: &MockServer, count: usize) {
    while server
        .received_requests()
        .await
        .expect("the mock server records requests")
        .len()
        < count
    {
        tokio::task::yield_now().await;
    }
}

/// Request bodies the server received, in arrival order.
pub async fn request_bodies(server: &MockServer) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .expect("the mock server records requests")
        .iter()
        .map(|request| serde_json::from_slice(&request.body).expect("each request body is JSON"))
        .collect()
}
