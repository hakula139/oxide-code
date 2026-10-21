//! Test fixtures for this crate's engine, turn, dispatch, and compaction tests. They mock stalled
//! replies, script turns, and seed transcripts large enough to compact.

use std::collections::VecDeque;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::json;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use super::dispatch::DispatchContext;
use super::testing::MODEL;
use crate::client::anthropic::testing::{sse_response, text_stream_body};
use crate::client::{Response, ResponseEvent, StopDetails, StopReason, Wire};
use crate::file_tracker::FileTracker;
use crate::message::{ContentBlock, Message, Replay, Role};
use crate::permission::rule::Rule;
use crate::permission::{Mode, PermissionClass, Policy, Workspace};
use crate::prompt::{PromptParts, SystemPrompt};
use crate::protocol::Op;
use crate::session::handle::{self, SessionHandle};
use crate::session::store::testing::test_store;
use crate::tool::{Prepared, Tool, ToolCtx, ToolDefinition, ToolOutput, ToolSpec};

// ── Engine ──

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

/// In-process fake that scripts a [`ResponseEvent`] stream per turn and records every request. A
/// request past the last scripted turn panics.
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
        _system: &SystemPrompt,
        _user_context: Option<&str>,
        _tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let events = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .expect("every request has a scripted turn");
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
        replay: Replay::with_model(MODEL, "sig"),
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

/// Echoes its input.
pub(super) struct EchoTool;

impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".to_owned(),
            description: "echo the input".to_owned(),
            input_schema: json!({"type": "object"}),
            class: PermissionClass::Exec,
            parallel_safe: false,
        }
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(
            async move { ToolOutput::success(prepared.input.to_string()).with_title("echoed") },
        )
    }
}

/// Inert receiver whose `recv()` stays pending, its sender leaked so callers need not hold it.
pub(super) fn inert_ops() -> mpsc::Receiver<Op> {
    let (tx, rx) = mpsc::channel(1);
    std::mem::forget(tx);
    rx
}

pub(super) fn empty_prompt() -> &'static PromptParts {
    static PROMPT: LazyLock<PromptParts> = LazyLock::new(PromptParts::default);
    &PROMPT
}

/// Dispatch context in `cwd` over `exec` deny / allow patterns and the shipped defaults, leaking
/// the policy so the context can borrow it for `'static`. `echo` is an `exec`-class tool that
/// accesses nothing, so only `*` matches it.
pub(super) fn dispatch_with(
    mode: Mode,
    deny: &[&str],
    allow: &[&str],
    cwd: &Path,
) -> DispatchContext<'static> {
    let rules = |patterns: &[&str]| {
        patterns
            .iter()
            .map(|pattern| Rule::parse(PermissionClass::Exec, pattern).expect("pattern parses"))
            .collect()
    };
    let workspace = Arc::new(Workspace::new(cwd));
    let policy = Box::leak(Box::new(
        Policy::resolve(mode, rules(deny), Vec::new(), rules(allow), &[], &workspace)
            .expect("shipped defaults parse"),
    ));
    DispatchContext {
        policy,
        workspace,
        session_id: "test-session".to_owned(),
        artifact_dir: Arc::from(cwd.join("session")),
        tracker: Arc::new(FileTracker::default()),
    }
}

pub(super) fn test_session(dir: &Path) -> SessionHandle {
    let store = test_store(dir);
    handle::start(&store, "claude-sonnet-5-5", None)
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
