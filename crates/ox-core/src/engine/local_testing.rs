//! Test fixtures that this crate's engine test modules share. They drive turns through an engine
//! handle, build dispatch contexts and dead sessions, and seed transcripts large enough to compact.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::mpsc;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::abort::TurnAbort;
use super::dispatch::DispatchContext;
use super::testing::MODEL;
use super::{Engine, EngineHandle, TurnEnd};
use crate::client::anthropic::local_testing::completion_body;
use crate::file_tracker::testing::tracker;
use crate::message::{ContentBlock, Message, Replay, Role};
use crate::permission::rule::Rule;
use crate::permission::{Mode, PermissionClass, Policy, Workspace};
use crate::protocol::{Event, Op};
use crate::session::handle::{self, SessionHandle};
use crate::session::store::SessionStore;
use crate::tool::{Prepared, Tool, ToolCtx, ToolOutput, ToolSpec};

// ── Engine ──

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

pub(super) const TITLE_MODEL: &str = "claude-haiku-5-5";

/// Next event the engine emits that `pick` accepts, skipping the rest.
pub(super) async fn next_matching<T>(
    events: &mut mpsc::UnboundedReceiver<Event>,
    mut pick: impl FnMut(Event) -> Option<T>,
) -> T {
    loop {
        let event = events.recv().await.expect("the engine stopped early");
        if let Some(found) = pick(event) {
            return found;
        }
    }
}

/// Events up to and including the terminal event of the prompt in flight.
pub(super) async fn turn_events(events: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
    let prompt = Op::prompt("");
    let mut seen = Vec::new();
    loop {
        let event = events.recv().await.expect("the engine stopped early");
        let ended = TurnEnd::of(&prompt, &event).is_some();
        seen.push(event);
        if ended {
            return seen;
        }
    }
}

/// Answers each title request with `response`. Mount it before the reply that answers turns.
pub(super) async fn mount_title(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(
            serde_json::json!({ "model": TITLE_MODEL }),
        ))
        .respond_with(response)
        .mount(server)
        .await;
}

/// A title reply naming the session `Greeting`, billed at [`COMPLETION_USAGE`].
///
/// [`COMPLETION_USAGE`]: crate::client::anthropic::local_testing::COMPLETION_USAGE
pub(super) fn greeting_title() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(completion_body(r#"{"title":"Greeting"}"#))
}

pub(super) async fn submit(handle: &mut EngineHandle, text: &str) -> Vec<Event> {
    handle.ops.send(Op::prompt(text)).await.unwrap();
    turn_events(&mut handle.events).await
}

/// The first text block of each message in `request`, empty for a message that opens with
/// another block.
pub(super) fn texts(request: &serde_json::Value) -> Vec<&str> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"][0]["text"].as_str().unwrap_or_default())
        .collect()
}

/// The URL `/login` announced, once the engine emits it.
pub(super) async fn login_url(events: &mut mpsc::UnboundedReceiver<Event>) -> String {
    next_matching(events, |event| match event {
        Event::LoginStarted { url } => Some(url),
        _ => None,
    })
    .await
}

/// Writes a `CLAUDE.md` that is not UTF-8 into `dir`, returning the warning its load raises.
pub(super) fn write_unreadable_instructions(dir: &Path) -> String {
    let path = dir.join("CLAUDE.md");
    std::fs::write(&path, b"\xff\xfe").unwrap();
    let cause = std::fs::read_to_string(&path).unwrap_err();
    format!("Skipped instruction file {}: {cause}", path.display())
}

/// Paths the engine's file tracker holds, sorted.
pub(super) fn tracked_paths(engine: &Engine) -> Vec<PathBuf> {
    let mut paths: Vec<_> = engine
        .file_tracker
        .snapshot_all()
        .into_iter()
        .map(|snapshot| snapshot.path)
        .collect();
    paths.sort();
    paths
}

/// Files of the sessions `store` lists.
pub(super) fn session_paths(store: &SessionStore) -> Vec<PathBuf> {
    store
        .list()
        .unwrap()
        .into_iter()
        .map(|info| info.path)
        .collect()
}

/// Records one exchange in a new session in `store` and finishes it, returning its ID.
pub(super) async fn finished_session(store: &SessionStore) -> String {
    let session = handle::start(store, MODEL, None);
    let id = session.session_id().to_owned();
    session
        .record_message(Message::user("earlier prompt"))
        .await;
    session
        .record_message(Message::assistant("earlier reply"))
        .await;
    session.finish(Vec::new()).await;
    id
}

// ── Turn ──

impl TurnAbort {
    /// The error a `Failed` abort holds. Panics on any other abort.
    pub(super) fn into_failure(self) -> anyhow::Error {
        match self {
            Self::Failed(e) => e,
            other => panic!("expected a failure: {other:?}"),
        }
    }
}

pub(super) fn thinking_block(thinking: &str) -> ContentBlock {
    ContentBlock::Thinking {
        thinking: thinking.to_owned(),
        replay: Replay::with_model(MODEL, "sig"),
    }
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
        tracker: tracker(),
    }
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
