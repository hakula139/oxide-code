//! Non-streaming `/v1/messages` one-shot path. Returns flattened assistant text and the response's
//! usage.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::transport::{post_messages, with_retry};
use super::wire::{ContentBlockInfo, OutputConfig, OutputFormat, RequestMessages};
use super::{Client, RequestKind, RequestSpec};
use crate::config::Effort;
use crate::message::Message;
use crate::model::{ModelSpec, TokenUsage};

// ── Client::complete ──

impl Client {
    /// Sends a non-streaming request and returns the concatenated assistant text, dropping tool-use
    /// and thinking blocks, with the usage the response reported. The request carries no agentic
    /// betas or `context_management` directive. `output_format` is dropped when the config turns
    /// structured outputs off, so callers can pass it unconditionally.
    pub(crate) async fn complete(
        &self,
        model: &ModelSpec,
        system: &str,
        user: &str,
        max_tokens: u32,
        effort: Option<Effort>,
        output_format: Option<&OutputFormat>,
    ) -> Result<(String, Option<TokenUsage>)> {
        let messages = [Message::user(user)];
        let request = &self.prepare(RequestSpec {
            kind: RequestKind::SideQuery,
            model,
            max_tokens,
            system: &[(system, None)],
            tools: None,
            output_config: OutputConfig::new(
                output_format.filter(|_| self.gateway.structured_outputs),
                effort,
            ),
            messages: RequestMessages {
                user_context: None,
                messages: &messages,
                breakpoint: None,
            },
        })?;
        let CompletionResponse { content, usage } = with_retry(
            |retry| async move {
                let response = post_messages(&self.http, request, retry).await?;
                Ok(response
                    .json()
                    .await
                    .context("failed to parse completion response")?)
            },
            |_| {},
        )
        .await?;
        Ok((join_text_blocks(content), usage))
    }
}

// ── Response Handling ──

fn join_text_blocks(content: Vec<ContentBlockInfo>) -> String {
    content
        .into_iter()
        .filter_map(|b| match b {
            ContentBlockInfo::Text { text } => Some(text),
            _ => None,
        })
        .collect()
}

#[derive(Deserialize)]
struct CompletionResponse {
    content: Vec<ContentBlockInfo>,
    usage: Option<TokenUsage>,
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::super::betas::OAUTH_BETA_HEADER;
    use super::super::local_testing::{
        Captured, auth_token, captured, client_for, oauth, test_client,
    };
    use super::super::testing::{DEVICE_ID, api_key, test_config};
    use super::*;
    use crate::client::anthropic::local_testing::{COMPLETION_USAGE, completion_body};
    use crate::client::anthropic::{SYSTEM_PROMPT_PREFIX, attribution_text};
    use crate::util::tls::testing::NoRoots;

    mod client {
        use super::*;

        mod complete {
            use super::*;

            fn haiku() -> ModelSpec {
                ModelSpec::resolve("claude-haiku-5-5".to_owned())
            }

            #[tokio::test]
            async fn sends_x_claude_code_session_id_header() {
                // `/clear` rolls the id in place, so `complete` must send the current one.
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .and(header("x-claude-code-session-id", "sid-complete"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(completion_body("ok")))
                    .mount(&server)
                    .await;

                let client = Client::new(
                    &test_config(server.uri(), api_key(), "claude-haiku-5-5"),
                    NoRoots,
                    Some("sid-complete".to_owned()),
                    DEVICE_ID.to_owned(),
                )
                .unwrap();
                client
                    .complete(&haiku(), "", "u", 40, None, None)
                    .await
                    .unwrap();
            }

            #[tokio::test]
            async fn happy_path_produces_assistant_text_and_usage() {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_string(completion_body("Fix login bug")),
                    )
                    .mount(&server)
                    .await;

                let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
                let (text, usage) = client
                    .complete(&haiku(), "sys", "user input", 40, None, None)
                    .await
                    .unwrap();
                assert_eq!(text, "Fix login bug");
                assert_eq!(usage, Some(COMPLETION_USAGE));
            }

            #[tokio::test]
            async fn retries_a_transient_status_before_the_reply() {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(ResponseTemplate::new(529).insert_header("retry-after", "0"))
                    .up_to_n_times(1)
                    .mount(&server)
                    .await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_string(completion_body("Fix login bug")),
                    )
                    .mount(&server)
                    .await;

                let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
                let (text, _) = client
                    .complete(&haiku(), "sys", "user input", 40, None, None)
                    .await
                    .unwrap();

                assert_eq!(text, "Fix login bug");
                assert_eq!(server.received_requests().await.unwrap().len(), 2);
            }

            #[tokio::test]
            async fn while_signed_out_fails_without_a_request() {
                let mut config =
                    test_config("https://example.invalid", api_key(), "claude-haiku-5-5");
                config.auth = None;
                let client = Client::new(&config, NoRoots, None, DEVICE_ID.to_owned()).unwrap();

                let err = client
                    .complete(&haiku(), "", "u", 40, None, None)
                    .await
                    .expect_err("a signed-out client must not send");

                assert_eq!(err.to_string(), "not signed in");
            }

            #[tokio::test]
            async fn http_error_propagates_status_and_body() {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(
                        ResponseTemplate::new(400)
                            .set_body_string(r#"{"error":{"type":"invalid_request"}}"#),
                    )
                    .mount(&server)
                    .await;

                let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
                let err = client
                    .complete(&haiku(), "", "u", 40, None, None)
                    .await
                    .expect_err("expected error");
                let msg = format!("{err:#}");
                assert!(msg.contains("400"), "status surfaced: {msg}");
                assert!(msg.contains("invalid_request"), "body surfaced: {msg}");
            }

            #[tokio::test]
            async fn on_429_surfaces_retry_after_header_in_error() {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(
                        ResponseTemplate::new(429)
                            .insert_header("retry-after", "120")
                            .set_body_string(r#"{"error":{"type":"rate_limit_error"}}"#),
                    )
                    .mount(&server)
                    .await;

                let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
                let err = client
                    .complete(&haiku(), "", "u", 40, None, None)
                    .await
                    .expect_err("expected 429 error");
                let msg = format!("{err:#}");
                assert!(
                    msg.contains("Retry after 120 s."),
                    "retry-after threaded: {msg}"
                );
            }

            #[tokio::test]
            async fn on_401_names_the_gateway_token_variable() {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
                    .mount(&server)
                    .await;

                let client = test_client(server.uri(), auth_token(), "claude-haiku-5-5");
                let err = client
                    .complete(&haiku(), "", "u", 40, None, None)
                    .await
                    .expect_err("expected 401");
                assert!(
                    format!("{err:#}").contains("Check ANTHROPIC_AUTH_TOKEN."),
                    "{err:#}"
                );
            }

            #[tokio::test]
            async fn malformed_response_body_errors_with_parse_context() {
                // A 200 with non-JSON must surface a parse error so the title path falls back.
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(ResponseTemplate::new(200).set_body_string("<not json>"))
                    .mount(&server)
                    .await;

                let client = test_client(server.uri(), api_key(), "claude-haiku-5-5");
                let err = client
                    .complete(&haiku(), "", "u", 40, None, None)
                    .await
                    .expect_err("expected parse error");
                assert!(
                    format!("{err:#}").contains("failed to parse completion response"),
                    "parse context: {err:#}",
                );
            }

            #[tokio::test]
            async fn ships_effort_and_format_without_beta_tags_unless_structured_outputs_off() {
                // Both fields are GA, so neither may drag a beta tag along.
                let schema = serde_json::json!({
                    "type": "object",
                    "properties": {"title": {"type": "string"}},
                    "required": ["title"],
                    "additionalProperties": false,
                });
                let fmt = OutputFormat::json_schema(schema.clone());
                for structured_outputs in [true, false] {
                    let server = MockServer::start().await;
                    let sink: Captured<(String, String)> = captured();
                    let sink_clone = std::sync::Arc::clone(&sink);
                    Mock::given(method("POST"))
                        .and(path("/v1/messages"))
                        .respond_with(move |req: &Request| {
                            let body = String::from_utf8_lossy(&req.body).into_owned();
                            let beta = req
                                .headers
                                .get("anthropic-beta")
                                .map(|v| v.to_str().unwrap().to_owned())
                                .unwrap_or_default();
                            *sink_clone.lock().unwrap() = Some((body, beta));
                            ResponseTemplate::new(200).set_body_string(completion_body("ok"))
                        })
                        .mount(&server)
                        .await;

                    let mut config = test_config(server.uri(), oauth(), "claude-haiku-5-5");
                    config.gateway.structured_outputs = structured_outputs;
                    let client = client_for(&config);
                    _ = client
                        .complete(&haiku(), "sys", "prompt", 40, Some(Effort::Low), Some(&fmt))
                        .await
                        .unwrap();

                    let (body, beta) = sink.lock().unwrap().clone().expect("request captured");
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(v["output_config"]["effort"], "low", "{body}");
                    let expected_format = if structured_outputs {
                        serde_json::json!({"type": "json_schema", "schema": schema})
                    } else {
                        serde_json::Value::Null
                    };
                    assert_eq!(v["output_config"]["format"], expected_format, "{body}");
                    assert_eq!(beta, OAUTH_BETA_HEADER, "only the auth tag ships: {beta}");
                }
            }

            #[tokio::test]
            async fn carries_the_billing_block_under_every_credential_unless_turned_off() {
                for (auth, attribution) in [
                    (api_key(), true),
                    (auth_token(), true),
                    (oauth(), true),
                    (api_key(), false),
                ] {
                    let server = MockServer::start().await;
                    Mock::given(method("POST"))
                        .and(path("/v1/messages"))
                        .respond_with(
                            ResponseTemplate::new(200).set_body_string(completion_body("ok")),
                        )
                        .mount(&server)
                        .await;
                    let kind = auth.kind();
                    let mut config = test_config(server.uri(), auth, "claude-haiku-5-5");
                    config.gateway.attribution = attribution;

                    client_for(&config)
                        .complete(&haiku(), "sys", "Fix login", 40, None, None)
                        .await
                        .unwrap();

                    let received = server.received_requests().await.expect("recorded requests");
                    let body: serde_json::Value =
                        serde_json::from_slice(&received[0].body).unwrap();
                    let expected = if attribution {
                        attribution_text("Fix login", false)
                    } else {
                        SYSTEM_PROMPT_PREFIX.to_owned()
                    };
                    assert_eq!(
                        body["system"][0]["text"], expected,
                        "{kind:?} {attribution}"
                    );
                }
            }
        }
    }

    mod join_text_blocks {
        use super::*;

        #[test]
        fn concatenates_text_and_drops_non_text_blocks() {
            // Round-trips through `CompletionResponse` to pin the live wire shape.
            let body = r#"{
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-haiku-5-5",
            "stop_reason": "end_turn",
            "content": [
                {"type":"thinking","thinking":"pondering","signature":"sig"},
                {"type":"text","text":"Fix auth bug"},
                {"type":"tool_use","id":"t1","name":"noop","input":{}},
                {"type":"text","text":" and friends"}
            ],
            "usage": {"input_tokens":10,"output_tokens":5}
        }"#;
            let parsed: CompletionResponse = serde_json::from_str(body).unwrap();
            assert_eq!(join_text_blocks(parsed.content), "Fix auth bug and friends");
        }

        #[test]
        fn gives_empty_text_for_a_tool_only_response() {
            // Empty signals "parse failure, keep fallback" to the title caller.
            let blocks = vec![ContentBlockInfo::ToolUse {
                id: "t1".to_owned(),
                name: "noop".to_owned(),
            }];
            assert_eq!(join_text_blocks(blocks), "");
        }
    }
}
