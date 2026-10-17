//! Test-only engine fixtures: [`Engine`] builders and mocked replies, the turn fixtures the turn,
//! dispatch, and compaction tests share, and the transcripts large enough to compact. Child of
//! `engine` so it can build the private fields.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::json;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use super::compaction::AutoCompactFailures;
use super::dispatch::GateContext;
use super::{Engine, EngineHandle, EngineTask};
use crate::client::anthropic::Client;
use crate::client::anthropic::testing::{
    api_key, sse_response, test_config, text_stream_body, tool_use_stream_body,
};
use crate::client::{Response, ResponseEvent, StopDetails, StopReason, Wire};
use crate::file_tracker::FileTracker;
use crate::message::{ContentBlock, Message, Replay, Role};
use crate::model::ModelSpec;
use crate::permission::{Mode, PermissionClass, Policy};
use crate::prompt::{self, PromptParts};
use crate::protocol::Op;
use crate::session::handle::{self, SessionHandle};
use crate::session::store::SessionStore;
use crate::session::store::testing::test_store;
use crate::tool::{Tool, ToolDefinition, ToolMetadata, ToolOutput, ToolRegistry};
use crate::util::path::testing::HOME_VAR;

// ── Engine ──

pub(super) const MODEL: &str = "claude-opus-5-5";
pub(super) const SESSION_CONTEXT: &str = "<system-reminder>session context</system-reminder>";

/// Non-interactive engine on a fresh session in `store`, working in `cwd`. Its prompt carries
/// [`SESSION_CONTEXT`] in place of the rendered user context, so tests can tell it apart from a
/// rebuilt one. Pass an empty directory, and run under [`with_pinned_home`], so the context never
/// reads the developer's instruction files or git state.
pub(crate) async fn engine(base_url: String, store: &SessionStore, cwd: &Path) -> Engine {
    let mut prompt = prompt::build_prompt(&ModelSpec::resolve(MODEL.to_owned()), cwd).await;
    SESSION_CONTEXT.clone_into(&mut prompt.user_context);
    let config = test_config(base_url, api_key(), MODEL);
    let session = handle::start(store, MODEL);
    Engine {
        client: Client::new(&config, Some(session.session_id().to_owned())).unwrap(),
        oauth_token_url: "http://127.0.0.1:9/token".to_owned(),
        tools: Arc::new(ToolRegistry::new(Vec::new())),
        store: store.clone(),
        session,
        messages: Vec::new(),
        compact: None,
        tool_metadata: HashMap::new(),
        prompt,
        prompt_unrecorded: true,
        file_tracker: Arc::new(FileTracker::default()),
        policy: config.permission,
        max_tool_rounds: config.max_tool_rounds,
        compaction: config.compaction,
        cwd: cwd.to_owned(),
        interactive: false,
        auto_compaction_failures: AutoCompactFailures::default(),
        last_usage: None,
        displayed_usage: None,
        total_estimated_cost_usd: 0.0,
    }
}

impl Engine {
    pub(crate) fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Arc::new(tools);
        self
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

/// Answers the next Messages request with the tool call `id` to `name` with `input_json`. Mount it
/// before the reply that should follow.
pub(crate) async fn mount_tool_call_once(
    server: &MockServer,
    id: &str,
    name: &str,
    input_json: &str,
) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(tool_use_stream_body(id, name, input_json)))
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

/// Waits until `server` has received `count` requests, so the engine is mid-request.
pub(super) async fn until_requested(server: &MockServer, count: usize) {
    while server.received_requests().await.unwrap().len() < count {
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

// ── Turn ──

/// In-process fake that scripts a [`ResponseEvent`] stream per turn and records each request's
/// transcript.
pub(super) struct FakeClient {
    turns: StdMutex<VecDeque<Vec<Result<ResponseEvent>>>>,
    requests: StdMutex<Vec<Vec<Message>>>,
}

impl FakeClient {
    pub(super) fn new(turns: Vec<Vec<ResponseEvent>>) -> Self {
        let turns = turns
            .into_iter()
            .map(|events| events.into_iter().map(Ok).collect())
            .collect();
        Self {
            turns: StdMutex::new(turns),
            requests: StdMutex::default(),
        }
    }

    /// Appends a turn whose stream fails with `error`.
    pub(super) fn then_fail(self, error: &str) -> Self {
        self.turns
            .lock()
            .unwrap()
            .push_back(vec![Err(anyhow!("{error}"))]);
        self
    }

    pub(super) fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }
}

impl Wire for FakeClient {
    fn stream_message(
        &self,
        messages: &[Message],
        _system_sections: &[&str],
        _user_context: Option<&str>,
        _tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        let (tx, rx) = mpsc::channel(events.len().max(1));
        for event in events {
            tx.try_send(event).expect("channel capacity");
        }
        Ok(rx)
    }
}

pub(super) fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_owned(),
    }
}

pub(super) fn thinking_block(thinking: &str) -> ContentBlock {
    ContentBlock::Thinking {
        thinking: thinking.to_owned(),
        replay: Replay::without_model("sig"),
    }
}

/// One reply completing with `content`, streaming its text and thinking first.
pub(super) fn reply_turn(content: Vec<ContentBlock>) -> Vec<ResponseEvent> {
    let mut events: Vec<ResponseEvent> = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(ResponseEvent::Text(text.clone())),
            ContentBlock::Thinking { thinking, .. } => {
                Some(ResponseEvent::Thinking(thinking.clone()))
            }
            _ => None,
        })
        .collect();
    events.push(ResponseEvent::Completed(Response {
        content,
        ..Response::default()
    }));
    events
}

pub(super) fn text_turn(text: &str) -> Vec<ResponseEvent> {
    reply_turn(vec![text_block(text)])
}

/// The turn's completed reply.
pub(super) fn reply_of(events: &mut [ResponseEvent]) -> &mut Response {
    match events.last_mut() {
        Some(ResponseEvent::Completed(response)) => response,
        last => panic!("the turn ends in its reply: {last:?}"),
    }
}

/// Ends the turn's reply on `stop_reason`.
pub(super) fn with_stop(
    mut events: Vec<ResponseEvent>,
    stop_reason: StopReason,
    stop_details: Option<StopDetails>,
) -> Vec<ResponseEvent> {
    let reply = reply_of(&mut events);
    reply.stop_reason = Some(stop_reason);
    reply.stop_details = stop_details;
    events
}

/// Echoes its input. Exercises the tool-dispatch path without subprocess machinery.
pub(super) struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "echo the input"
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Bash
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    fn run(
        &self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move {
            ToolOutput {
                content: input.to_string(),
                is_error: false,
                metadata: ToolMetadata {
                    title: Some("echoed".into()),
                    ..Default::default()
                },
            }
        })
    }
}

/// Inert receiver whose `recv()` stays pending. The sender is leaked so call sites don't
/// need to bind it for the test's lifetime.
pub(super) fn inert_ops() -> mpsc::Receiver<Op> {
    let (tx, rx) = crate::protocol::inert_op_channel();
    std::mem::forget(tx);
    rx
}

pub(super) fn empty_prompt() -> PromptParts {
    PromptParts::default()
}

/// Gate over allow / deny rule sets, leaking the policy so the gate can borrow it for `'static`.
/// `echo` is a `bash`-class tool with no gate target, so only a class-wide rule matches it.
pub(super) fn gate_with(mode: Mode, allow: &[&str], deny: &[&str]) -> GateContext<'static> {
    let owned = |rules: &[&str]| rules.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let policy = Box::leak(Box::new(
        Policy::resolve(mode, &owned(allow), &owned(deny), Path::new(".")).expect("rules parse"),
    ));
    GateContext {
        policy,
        cwd: Path::new("."),
    }
}

pub(super) fn test_session(dir: &Path) -> SessionHandle {
    let store = test_store(dir);
    handle::start(&store, "claude-sonnet-5-5")
}

/// Handle whose actor channel is closed. Every write returns actor-gone.
pub(super) fn dead_test_session() -> SessionHandle {
    handle::testing::dead("dead-test-session")
}

// ── Compaction ──

/// A log of 60 numbered lines ending on `{service}-end`, about 900 rough tokens.
pub(super) fn service_log(service: &str) -> String {
    (1..=60)
        .map(|n| format!("{n:>6}\t2026-10-16 INFO {service} served request {n} in 12 ms\n"))
        .chain([format!("{service}-end\n")])
        .collect()
}

/// A session sized past the compaction floor by its tool results: one prompt, a round that reads
/// two service logs, and one reply.
pub(super) fn tool_heavy_transcript() -> Vec<Message> {
    let read = |id: &str, file: &str| ContentBlock::ToolUse {
        id: id.to_owned(),
        name: "read".to_owned(),
        input: json!({"file_path": format!("/repo/{file}")}),
    };
    let log = |id: &str, service: &str| ContentBlock::ToolResult {
        tool_use_id: id.to_owned(),
        content: service_log(service),
        is_error: false,
    };
    vec![
        Message::user("Read log-a.txt and log-b.txt, then reply with the final line of each."),
        Message {
            role: Role::Assistant,
            content: vec![
                thinking_block("Read both logs."),
                read("t1", "log-a.txt"),
                read("t2", "log-b.txt"),
            ],
        },
        Message {
            role: Role::User,
            content: vec![log("t1", "alpha"), log("t2", "beta")],
        },
        Message::assistant("alpha-end, beta-end"),
    ]
}
