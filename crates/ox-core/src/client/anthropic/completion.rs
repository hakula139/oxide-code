//! Non-streaming `/v1/messages` side-query path. Returns flattened assistant text and the response's
//! usage.

use anyhow::{Context as _, Result};
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
            thinking: None,
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
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::anthropic::local_testing::{
        COMPLETION_USAGE, auth_token, client_for, completion_body, oauth, request_count, serve,
        test_client,
    };
    use crate::client::anthropic::testing::{DEVICE_ID, api_key, test_config};
    use crate::client::anthropic::{SYSTEM_PROMPT_PREFIX, attribution_text, build_metadata};

    mod client {
        use super::*;

        mod complete {
            use super::*;

            const HAIKU: &str = "claude-haiku-5-5";

            fn haiku() -> ModelSpec {
                ModelSpec::resolve(HAIKU)
            }

            async fn complete_err(client: &Client) -> String {
                let err = client
                    .complete(&haiku(), "", "u", 40, None, None)
                    .await
                    .expect_err("the request must fail");
                format!("{err:#}")
            }

            #[tokio::test]
            async fn happy_path_produces_assistant_text_and_usage() {
                let server = serve(vec![
                    ResponseTemplate::new(200).set_body_string(completion_body("Fix login bug")),
                ])
                .await;
                let client = test_client(server.uri(), api_key(), HAIKU);

                let (text, usage) = client
                    .complete(&haiku(), "sys", "user input", 40, None, None)
                    .await
                    .unwrap();

                assert_eq!(text, "Fix login bug");
                assert_eq!(usage, Some(COMPLETION_USAGE));
            }

            #[tokio::test]
            async fn sends_the_caller_system_and_prompt_as_a_side_query() {
                let server = serve(vec![
                    ResponseTemplate::new(200).set_body_string(completion_body("ok")),
                ])
                .await;
                let mut config = test_config(server.uri(), api_key(), HAIKU);
                config.gateway.attribution = false;
                let client = client_for(&config);

                client
                    .complete(&haiku(), "sys", "Fix login", 40, None, None)
                    .await
                    .unwrap();

                let received = server.received_requests().await.expect("recorded requests");
                let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
                assert_eq!(
                    body,
                    serde_json::json!({
                        "model": HAIKU,
                        "max_tokens": 40,
                        "stream": false,
                        "metadata": {"user_id": build_metadata(DEVICE_ID, "sid").user_id},
                        "system": [
                            {"type": "text", "text": SYSTEM_PROMPT_PREFIX},
                            {"type": "text", "text": "sys"},
                        ],
                        "messages": [
                            {"role": "user", "content": [{"type": "text", "text": "Fix login"}]},
                        ],
                    }),
                );
            }

            #[tokio::test]
            async fn sends_the_current_session_id_header() {
                // `/clear` rolls the id in place, so `complete` must send the current one.
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .and(header("x-claude-code-session-id", "sid-rolled"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(completion_body("ok")))
                    .expect(1)
                    .mount(&server)
                    .await;
                let mut client = test_client(server.uri(), api_key(), HAIKU);
                client.set_session_id("sid-rolled".to_owned());

                client
                    .complete(&haiku(), "", "u", 40, None, None)
                    .await
                    .unwrap();
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
                    let server = serve(vec![
                        ResponseTemplate::new(200).set_body_string(completion_body("ok")),
                    ])
                    .await;
                    let mut config = test_config(server.uri(), oauth(), HAIKU);
                    config.gateway.structured_outputs = structured_outputs;
                    let client = client_for(&config);

                    client
                        .complete(&haiku(), "sys", "prompt", 40, Some(Effort::Low), Some(&fmt))
                        .await
                        .unwrap();

                    let received = server.received_requests().await.expect("recorded requests");
                    let body: serde_json::Value =
                        serde_json::from_slice(&received[0].body).unwrap();
                    let expected_format = if structured_outputs {
                        serde_json::json!({"type": "json_schema", "schema": schema})
                    } else {
                        serde_json::Value::Null
                    };
                    assert_eq!(body["output_config"]["effort"], "low", "{body}");
                    assert_eq!(body["output_config"]["format"], expected_format, "{body}");
                    assert_eq!(
                        received[0].headers.get("anthropic-beta").unwrap(),
                        "oauth-2025-04-20",
                    );
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
                    let server = serve(vec![
                        ResponseTemplate::new(200).set_body_string(completion_body("ok")),
                    ])
                    .await;
                    let kind = auth.kind();
                    let mut config = test_config(server.uri(), auth, HAIKU);
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

            #[tokio::test]
            async fn retries_a_transient_status_before_the_reply() {
                let server = serve(vec![
                    ResponseTemplate::new(529).insert_header("retry-after", "0"),
                    ResponseTemplate::new(200).set_body_string(completion_body("Fix login bug")),
                ])
                .await;
                let client = test_client(server.uri(), api_key(), HAIKU);

                let (text, _) = client
                    .complete(&haiku(), "sys", "user input", 40, None, None)
                    .await
                    .unwrap();

                assert_eq!(text, "Fix login bug");
                assert_eq!(request_count(&server).await, 2);
            }

            #[tokio::test]
            async fn while_signed_out_fails_without_a_request() {
                let mut config = test_config("https://example.invalid", api_key(), HAIKU);
                config.auth = None;
                let client = client_for(&config);

                let err = complete_err(&client).await;

                assert_eq!(err, "not signed in");
            }

            #[tokio::test]
            async fn http_error_propagates_status_and_body() {
                let server = serve(vec![ResponseTemplate::new(400).set_body_string(
                    r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be positive"}}"#,
                )])
                .await;
                let client = test_client(server.uri(), api_key(), HAIKU);

                let err = complete_err(&client).await;

                assert_eq!(
                    err,
                    "Anthropic API error (HTTP 400 Bad Request): max_tokens: must be positive"
                );
            }

            #[tokio::test]
            async fn on_429_surfaces_retry_after_header_in_error() {
                let server = serve(vec![
                    ResponseTemplate::new(429)
                        .insert_header("retry-after", "120")
                        .set_body_string(
                            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Slow down"}}"#,
                        ),
                ])
                .await;
                let client = test_client(server.uri(), api_key(), HAIKU);

                let err = complete_err(&client).await;

                assert_eq!(
                    err,
                    "Anthropic API rate limited (HTTP 429): Slow down. Retry after 120 s."
                );
            }

            #[tokio::test]
            async fn on_401_names_the_gateway_token_variable() {
                let server = serve(vec![
                    ResponseTemplate::new(401).set_body_string("unauthorized"),
                ])
                .await;
                let client = test_client(server.uri(), auth_token(), HAIKU);

                let err = complete_err(&client).await;

                assert_eq!(
                    err,
                    "Anthropic API rejected credentials (HTTP 401): unauthorized. \
                     Check ANTHROPIC_AUTH_TOKEN."
                );
            }

            #[tokio::test]
            async fn malformed_response_body_errors_with_parse_context() {
                // A 200 with non-JSON must surface a parse error so the title path falls back.
                let server = serve(vec![
                    ResponseTemplate::new(200).set_body_string("<not json>"),
                ])
                .await;
                let client = test_client(server.uri(), api_key(), HAIKU);

                let err = client
                    .complete(&haiku(), "", "u", 40, None, None)
                    .await
                    .expect_err("the request must fail");

                assert_eq!(err.to_string(), "failed to parse completion response");
            }
        }
    }

    mod join_text_blocks {
        use super::*;

        #[test]
        fn concatenates_text_and_drops_non_text_blocks() {
            // Round-trips through `CompletionResponse` to pin the live wire shape.
            let body = serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "claude-haiku-5-5",
                "stop_reason": "end_turn",
                "content": [
                    {"type": "thinking", "thinking": "pondering", "signature": "sig"},
                    {"type": "text", "text": "Fix auth bug"},
                    {"type": "tool_use", "id": "t1", "name": "noop", "input": {}},
                    {"type": "text", "text": " and friends"},
                ],
                "usage": {"input_tokens": 10, "output_tokens": 5},
            });

            let parsed: CompletionResponse = serde_json::from_value(body).unwrap();

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
