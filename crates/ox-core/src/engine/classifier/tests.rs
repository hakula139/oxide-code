use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::client::anthropic::local_testing::{COMPLETION_USAGE, client_for, completion_body};
use crate::client::anthropic::testing::{
    api_key, sse_body, sse_response, text_stream_body, tool_use_stream_body,
};
use crate::config::compaction::CompactionConfig;
use crate::config::permission_auto::AutoPermissionConfig;
use crate::config::testing::test_config;
use crate::engine::EngineHandle;
use crate::engine::local_testing::{tool_heavy_transcript, turn_events};
use crate::engine::testing::{MODEL, engine, request_bodies, until_requested, with_pinned_home};
use crate::message::{ContentBlock, Message, Role, TextOrigin};
use crate::model::ModelSpec;
use crate::permission::rule::Rule;
use crate::permission::{Mode, PermissionClass, Policy};
use crate::protocol::local_testing::CapturingSink;
use crate::protocol::{ApprovalDecision, Event, Op, OpEnd};
use crate::session::handle;
use crate::session::store::UsageRecord;
use crate::session::store::testing::test_store;
use crate::tool::tasks::TasksTool;
use crate::tool::{Prepared, Tool, ToolCtx, ToolDefinition, ToolOutput, ToolRegistry, ToolSpec};

const HAIKU: &str = "claude-haiku-5-5";

fn classifier_reply(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(completion_body(text))
}

fn refused_classifier_reply() -> ResponseTemplate {
    let mut body: serde_json::Value = serde_json::from_str(&completion_body(
        r#"{"decision":"allow","reason":"Requested"}"#,
    ))
    .unwrap();
    body["stop_reason"] = json!("refusal");
    ResponseTemplate::new(200).set_body_json(body)
}

async fn mount_classifier(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"model": HAIKU, "stream": false})))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn mount_tool_turn(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"model": MODEL, "stream": true})))
        .respond_with(sse_response(tool_use_stream_body("call-1", "probe", "{}")))
        .up_to_n_times(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"model": MODEL, "stream": true})))
        .respond_with(sse_response(text_stream_body("", "done")))
        .mount(server)
        .await;
}

struct ProbeTool(Arc<AtomicUsize>);

impl Tool for ProbeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            definition: ToolDefinition {
                name: "probe".to_owned(),
                description: "records a probe".to_owned(),
                input_schema: json!({"type": "object"}),
            },
            class: PermissionClass::Exec,
            parallel_safe: false,
        }
    }

    fn run<'a>(
        &'a self,
        _prepared: Prepared,
        _ctx: &'a ToolCtx,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            ToolOutput::success("probed")
        })
    }
}

mod classify {
    use super::*;

    #[tokio::test]
    async fn sends_configured_model_instructions_and_complete_action_with_or_without_schema() {
        for structured_outputs in [true, false] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(classifier_reply(
                    r#"{"decision":"allow","reason":"Requested inspection"}"#,
                ))
                .mount(&server)
                .await;
            let mut config = test_config(server.uri(), api_key(), MODEL);
            config.gateway.structured_outputs = structured_outputs;
            let client = client_for(&config);
            let auth = api_key();
            let settings = AutoPermissionConfig {
                model: ModelSpec::resolve("gateway-mini"),
                instructions: vec!["Ask before publishing an artifact.".to_owned()],
            };
            let context = ClassifierContext {
                client: &client,
                auth: &auth,
                config: &settings,
                cache_ttl: PromptCacheTtl::FiveMin,
                context_window: None,
                user_prompts: vec!["Inspect the current build.".to_owned()],
            };
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let session = handle::start(&store, MODEL, None);
            session.record_message(Message::user("inspect")).await;
            let payload = "complete-action-".repeat(4_000);
            let input = json!({"command": payload});
            let access = Access::None;
            let workspace = Workspace::new(Path::new("/repo"));
            let mut usage = UsageLedger::for_session(&[], 0);

            let assessment = context
                .classify(
                    Action {
                        name: "bash",
                        input: &input,
                        access: &access,
                    },
                    &workspace,
                    &session,
                    &mut usage,
                    &CapturingSink::default(),
                )
                .await
                .unwrap();

            assert!(matches!(assessment.decision, AssessmentDecision::Allow));
            assert_eq!(assessment.reason, "Requested inspection");
            let bodies = request_bodies(&server).await;
            let [request] = bodies.as_slice() else {
                panic!("one classifier request: {bodies:?}");
            };
            assert_eq!(request["model"], "gateway-mini");
            assert_eq!(request["stream"], false);
            assert_eq!(
                request["output_config"]["format"].is_object(),
                structured_outputs
            );
            let text = request.to_string();
            assert!(text.contains("Ask before publishing an artifact."));
            assert!(text.contains("Inspect the current build."));
            assert!(text.contains(&payload), "the pending action is complete");
        }
    }

    #[tokio::test]
    async fn records_successful_response_usage_before_rejecting_an_invalid_or_refused_verdict() {
        for (name, response) in [
            ("malformed", classifier_reply("not a verdict")),
            ("refused", refused_classifier_reply()),
        ] {
            let server = MockServer::start().await;
            mount_classifier(&server, response).await;
            let config = test_config(server.uri(), api_key(), MODEL);
            let client = client_for(&config);
            let auth = api_key();
            let settings = AutoPermissionConfig::default();
            let context = ClassifierContext {
                client: &client,
                auth: &auth,
                config: &settings,
                cache_ttl: PromptCacheTtl::FiveMin,
                context_window: None,
                user_prompts: vec!["inspect".to_owned()],
            };
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let session = handle::start(&store, MODEL, None);
            session.record_message(Message::user("inspect")).await;
            let mut usage = UsageLedger::for_session(&[], 0);
            let sink = CapturingSink::default();

            let result = context
                .classify(
                    Action {
                        name: "probe",
                        input: &json!({}),
                        access: &Access::None,
                    },
                    &Workspace::new(Path::new("/repo")),
                    &session,
                    &mut usage,
                    &sink,
                )
                .await;

            assert!(
                result.is_err(),
                "{name}: invalid or refused output must not approve"
            );
            let records = store
                .load_session_data(&session.session_id())
                .unwrap()
                .usage;
            assert_eq!(
                records,
                [UsageRecord {
                    model: HAIKU.to_owned(),
                    cache_ttl: PromptCacheTtl::FiveMin,
                    tokens: COMPLETION_USAGE,
                }],
                "{name}",
            );
            let expected = crate::engine::usage::estimate_cost_usd(&records);
            assert_eq!(usage.total_estimated_cost_usd(), expected, "{name}");
            assert!(sink.events().iter().any(|event| matches!(
                event,
                Event::UsageUpdated(snapshot) if snapshot.estimated_cost_usd == Some(expected)
            )));
        }
    }
}

mod permission_gate {
    use super::*;

    async fn resolve_turn(handle: &mut EngineHandle, decision: ApprovalDecision) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            let event = handle.events.recv().await.expect("the turn finishes");
            if let Event::ApprovalRequested { id, .. } = &event {
                handle
                    .ops
                    .submit(Op::Resolve {
                        id: id.clone(),
                        decision: decision.clone(),
                    })
                    .await
                    .unwrap();
            }
            let ended = matches!(event, Event::OpEnded { .. });
            events.push(event);
            if ended {
                return events;
            }
        }
    }

    async fn assert_gate_case(
        name: &str,
        response: ResponseTemplate,
        decision: ApprovalDecision,
        attended: bool,
        expected_runs: usize,
        expected_asks: usize,
    ) {
        let server = MockServer::start().await;
        mount_classifier(&server, response).await;
        mount_tool_turn(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let runs = Arc::new(AtomicUsize::new(0));
        let mut engine =
            engine(server.uri(), &store, dir.path())
                .await
                .with_tools(ToolRegistry::new(vec![Box::new(ProbeTool(Arc::clone(
                    &runs,
                )))]));
        engine.policy.set_mode(Mode::Auto);
        if !attended {
            engine = engine.unattended();
        }
        let mut handle = engine.spawn();

        handle
            .ops
            .submit(Op::prompt("Run the probe."))
            .await
            .unwrap();
        let events = resolve_turn(&mut handle, decision).await;

        assert!(
            matches!(
                events.last(),
                Some(Event::OpEnded {
                    end: OpEnd::Completed,
                    ..
                })
            ),
            "{name}: {events:?}"
        );
        assert_eq!(runs.load(Ordering::SeqCst), expected_runs, "{name}");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::ApprovalRequested { .. }))
                .count(),
            expected_asks,
            "{name}"
        );
        let requests = request_bodies(&server).await;
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["model"] == HAIKU)
                .count(),
            1,
            "{name}"
        );
        assert_eq!(handle.shutdown().await.unwrap(), None, "{name}");
    }

    #[tokio::test]
    async fn allows_or_requests_user_approval_and_keeps_failed_classification_unapproved() {
        with_pinned_home(async {
            for (name, response, decision, attended, expected_runs, expected_asks) in [
                (
                    "allow",
                    classifier_reply(r#"{"decision":"allow","reason":"Requested"}"#),
                    ApprovalDecision::Deny,
                    true,
                    1,
                    0,
                ),
                (
                    "ask approved",
                    classifier_reply(r#"{"decision":"ask","reason":"Needs approval"}"#),
                    ApprovalDecision::Approve,
                    true,
                    1,
                    1,
                ),
                (
                    "ask rejected",
                    classifier_reply(r#"{"decision":"ask","reason":"Needs approval"}"#),
                    ApprovalDecision::Deny,
                    true,
                    0,
                    1,
                ),
                (
                    "malformed",
                    classifier_reply("not JSON"),
                    ApprovalDecision::Approve,
                    true,
                    1,
                    1,
                ),
                (
                    "refusal with an allow-shaped response",
                    refused_classifier_reply(),
                    ApprovalDecision::Deny,
                    true,
                    0,
                    1,
                ),
                (
                    "empty reason",
                    classifier_reply(r#"{"decision":"allow","reason":" "}"#),
                    ApprovalDecision::Deny,
                    true,
                    0,
                    1,
                ),
                (
                    "request failed",
                    ResponseTemplate::new(400).set_body_string("invalid request"),
                    ApprovalDecision::Deny,
                    true,
                    0,
                    1,
                ),
                (
                    "unattended",
                    classifier_reply("not JSON"),
                    ApprovalDecision::Approve,
                    false,
                    0,
                    0,
                ),
            ] {
                assert_gate_case(
                    name,
                    response,
                    decision,
                    attended,
                    expected_runs,
                    expected_asks,
                )
                .await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn explicit_rules_and_session_grants_settle_without_a_classifier_request() {
        with_pinned_home(async {
            for (tier, expected_runs, expected_asks) in [
                ("deny", 0, 0),
                ("ask", 0, 1),
                ("allow", 1, 0),
                ("session", 1, 0),
            ] {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .and(body_partial_json(json!({"model": HAIKU})))
                    .respond_with(classifier_reply(
                        r#"{"decision":"allow","reason":"Unneeded"}"#,
                    ))
                    .expect(0)
                    .mount(&server)
                    .await;
                mount_tool_turn(&server).await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let runs = Arc::new(AtomicUsize::new(0));
                let mut engine =
                    engine(server.uri(), &store, dir.path())
                        .await
                        .with_tools(ToolRegistry::new(vec![Box::new(ProbeTool(Arc::clone(
                            &runs,
                        )))]));
                let rule = || Rule::parse(PermissionClass::Exec, "*").unwrap();
                let deny = if tier == "deny" {
                    vec![rule()]
                } else {
                    Vec::new()
                };
                let ask = if tier == "ask" {
                    vec![rule()]
                } else {
                    Vec::new()
                };
                let allow = if tier == "session" {
                    Vec::new()
                } else {
                    vec![rule()]
                };
                engine.policy = Policy::resolve(Mode::Auto, deny, ask, allow, &engine.workspace);
                if tier != "allow" {
                    engine.policy.allow_for_session(rule());
                }
                let mut handle = engine.spawn();

                handle
                    .ops
                    .submit(Op::prompt("Run the probe."))
                    .await
                    .unwrap();
                let events = resolve_turn(&mut handle, ApprovalDecision::Deny).await;

                assert!(
                    matches!(
                        events.last(),
                        Some(Event::OpEnded {
                            end: OpEnd::Completed,
                            ..
                        })
                    ),
                    "{tier}: {events:?}"
                );
                assert_eq!(runs.load(Ordering::SeqCst), expected_runs, "{tier}");
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, Event::ApprovalRequested { .. }))
                        .count(),
                    expected_asks,
                    "{tier}"
                );
                assert_eq!(handle.shutdown().await.unwrap(), None, "{tier}");
            }
        })
        .await;
    }

    #[tokio::test]
    async fn classifier_receives_user_prompts_without_assistant_tool_or_generated_text() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_classifier(
                &server,
                classifier_reply(r#"{"decision":"allow","reason":"Requested"}"#),
            )
            .await;
            mount_tool_turn(&server).await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let runs = Arc::new(AtomicUsize::new(0));
            let mut engine =
                engine(server.uri(), &store, dir.path())
                    .await
                    .with_tools(ToolRegistry::new(vec![Box::new(ProbeTool(Arc::clone(
                        &runs,
                    )))]));
            engine.policy.set_mode(Mode::Auto);
            engine.transcript.set_messages(vec![
                Message::user("Inspect the requested project."),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::text("assistant-approval-marker"),
                        ContentBlock::tool_use("earlier", "probe", json!({})),
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::tool_result("earlier", "tool-approval-marker", false),
                        ContentBlock::generated_text(
                            "generated-approval-marker",
                            TextOrigin::SystemReminder,
                        ),
                    ],
                },
            ]);
            let mut handle = engine.spawn();

            handle
                .ops
                .submit(Op::prompt("Run the requested probe."))
                .await
                .unwrap();
            let events = resolve_turn(&mut handle, ApprovalDecision::Deny).await;

            assert!(
                matches!(
                    events.last(),
                    Some(Event::OpEnded {
                        end: OpEnd::Completed,
                        ..
                    })
                ),
                "{events:?}"
            );
            assert_eq!(runs.load(Ordering::SeqCst), 1);
            let requests = request_bodies(&server).await;
            let classifier = requests
                .iter()
                .find(|request| request["model"] == HAIKU)
                .unwrap();
            let text = classifier.to_string();
            assert!(text.contains("Inspect the requested project."));
            assert!(text.contains("Run the requested probe."));
            for excluded in [
                "assistant-approval-marker",
                "tool-approval-marker",
                "generated-approval-marker",
            ] {
                assert!(
                    !text.contains(excluded),
                    "{excluded} reached classifier input"
                );
            }
            assert_eq!(handle.shutdown().await.unwrap(), None);
        })
        .await;
    }

    #[tokio::test]
    async fn keeps_genuine_user_prompts_and_queued_input_after_mid_turn_compaction() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_compacting_tasks_call(&server).await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .and(body_partial_json(json!({"model": MODEL, "stream": true})))
                .respond_with(sse_response(text_stream_body(
                    "",
                    "summary-approval-marker",
                )))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            mount_tool_turn(&server).await;
            mount_classifier(
                &server,
                classifier_reply(r#"{"decision":"allow","reason":"Requested"}"#),
            )
            .await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let runs = Arc::new(AtomicUsize::new(0));
            let mut engine =
                engine(server.uri(), &store, dir.path())
                    .await
                    .with_tools(ToolRegistry::new(vec![
                        Box::new(TasksTool),
                        Box::new(ProbeTool(Arc::clone(&runs))),
                    ]));
            engine.policy.set_mode(Mode::Auto);
            engine.compaction = CompactionConfig::with_threshold(50_000);
            let mut messages = tool_heavy_transcript();
            messages[0] = Message::user("Inspect the requested project.");
            messages[2].content.push(ContentBlock::generated_text(
                "generated-approval-marker",
                TextOrigin::SystemReminder,
            ));
            engine.transcript.set_messages(messages);
            let mut handle = engine.spawn();
            handle
                .ops
                .submit(Op::prompt("Run the requested probe."))
                .await
                .unwrap();
            until_requested(&server, 1).await;

            handle
                .ops
                .submit(Op::Queue("Also inspect the requested docs.".to_owned()))
                .await
                .unwrap();
            let events = resolve_turn(&mut handle, ApprovalDecision::Deny).await;

            assert!(
                matches!(
                    events.last(),
                    Some(Event::OpEnded {
                        end: OpEnd::Completed,
                        ..
                    })
                ),
                "{events:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, Event::SessionCompacted { .. }))
            );
            assert_eq!(runs.load(Ordering::SeqCst), 1);
            let requests = request_bodies(&server).await;
            let classifier = requests
                .iter()
                .find(|request| request["model"] == HAIKU)
                .unwrap();
            let action: serde_json::Value = serde_json::from_str(
                classifier["messages"][0]["content"][0]["text"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                action["user_prompts"],
                json!([
                    "Inspect the requested project.",
                    "Run the requested probe.",
                    "Also inspect the requested docs.",
                ])
            );
            assert_eq!(handle.shutdown().await.unwrap(), None);
        })
        .await;
    }

    async fn mount_compacting_tasks_call(server: &MockServer) {
        let frames = [
            json!({"type": "message_start", "message": {"id": "msg_1", "model": MODEL, "usage": {"input_tokens": 60_000, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "tasks-1", "name": "tasks"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": r#"{"tasks":[]}"#}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 100}}),
            json!({"type": "message_stop"}),
        ];
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(json!({"model": MODEL, "stream": true})))
            .respond_with(
                sse_response(sse_body(&frames)).set_delay(std::time::Duration::from_millis(200)),
            )
            .up_to_n_times(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn interrupt_during_classification_prevents_tool_execution() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_classifier(
                &server,
                classifier_reply(r#"{"decision":"allow","reason":"Requested"}"#)
                    .set_delay(std::time::Duration::from_secs(60)),
            )
            .await;
            mount_tool_turn(&server).await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let runs = Arc::new(AtomicUsize::new(0));
            let mut engine =
                engine(server.uri(), &store, dir.path())
                    .await
                    .with_tools(ToolRegistry::new(vec![Box::new(ProbeTool(Arc::clone(
                        &runs,
                    )))]));
            engine.policy.set_mode(Mode::Auto);
            let mut handle = engine.spawn();
            handle
                .ops
                .submit(Op::prompt("Run the probe."))
                .await
                .unwrap();
            until_requested(&server, 2).await;
            let requests = request_bodies(&server).await;
            assert_eq!(requests[1]["model"], HAIKU);

            handle.ops.submit(Op::Interrupt).await.unwrap();
            let events = turn_events(&mut handle.events).await;

            assert!(
                matches!(
                    events.last(),
                    Some(Event::OpEnded {
                        end: OpEnd::Cancelled,
                        ..
                    })
                ),
                "{events:?}"
            );
            assert_eq!(runs.load(Ordering::SeqCst), 0);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Event::ApprovalRequested { .. }))
                    .count(),
                0
            );
            assert_eq!(handle.shutdown().await.unwrap(), None);
        })
        .await;
    }
}
