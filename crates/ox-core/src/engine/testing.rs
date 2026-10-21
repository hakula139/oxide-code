//! Test fixtures, shared with other crates' tests, that build [`Engine`]s and mock API replies.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use super::compaction::AutoCompactFailures;
use super::{Engine, EngineHandle, EnginePanic, EngineTask};
use crate::client::anthropic::Client;
use crate::client::anthropic::testing::{
    DEVICE_ID, api_key, sse_response, test_config, text_stream_body, tool_use_stream_body,
};
use crate::config::oauth::ClaudeCodeKeychain;
use crate::file_tracker::FileTracker;
use crate::model::ModelSpec;
use crate::permission::Workspace;
use crate::prompt;
use crate::session::handle;
use crate::session::store::SessionStore;
use crate::tool::ToolRegistry;
use crate::util::path::testing::HOME_VAR;
use crate::util::tls::testing::NoRoots;

// ── Engine ──

pub(super) const MODEL: &str = "claude-opus-5-5";
pub(super) const SESSION_CONTEXT: &str = "<system-reminder>session context</system-reminder>";

/// Non-interactive engine on a fresh session in `store`, working in `cwd`. Its prompt carries
/// [`SESSION_CONTEXT`] in place of the rendered user context, so tests can tell it apart from a
/// rebuilt one. Pass an empty directory, and run under [`with_pinned_home`], so the context never
/// reads the developer's instruction files or git state.
pub async fn engine(base_url: String, store: &SessionStore, cwd: &Path) -> Engine {
    let (mut prompt, instruction_warnings) =
        prompt::build_prompt(&ModelSpec::resolve(MODEL), cwd).await;
    SESSION_CONTEXT.clone_into(&mut prompt.user_context);
    let config = test_config(base_url, api_key(), MODEL);
    let session = handle::start(store, MODEL, None);
    Engine {
        client: Client::new(
            &config,
            NoRoots,
            Some(session.session_id().to_owned()),
            DEVICE_ID.to_owned(),
        )
        .expect("the test client builds"),
        oauth_token_url: "http://127.0.0.1:9/token".to_owned(),
        claude_code_keychain: ClaudeCodeKeychain::NONE,
        retry_login: false,
        tools: Arc::new(ToolRegistry::new(Vec::new())),
        store: store.clone(),
        session,
        messages: Vec::new(),
        compact: None,
        tool_metadata: HashMap::new(),
        tasks: Vec::new(),
        prompt,
        prompt_unrecorded: true,
        startup_notices: Vec::new(),
        instruction_warnings,
        file_tracker: Arc::new(FileTracker::default()),
        shells: Arc::default(),
        policy: config.permission,
        max_tool_rounds: config.max_tool_rounds,
        compaction: config.compaction,
        workspace: Arc::new(Workspace::new(cwd)),
        interactive: false,
        auto_compaction_failures: AutoCompactFailures::default(),
        last_usage: None,
        displayed_usage: None,
        total_estimated_cost_usd: 0.0,
        title_usage: None,
    }
}

impl Engine {
    /// This engine offering `tools` in place of its own.
    #[must_use]
    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Arc::new(tools);
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
        let vars = [
            (HOME_VAR, Some(home.path())),
            ("XDG_CONFIG_HOME", None),
            ("CLAUDE_CONFIG_DIR", None),
        ];
        temp_env::async_with_vars(vars, test).await;
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
