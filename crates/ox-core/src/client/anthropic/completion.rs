//! Non-streaming `/v1/messages` side-query path.
//! Returns flattened assistant text and the response's usage.

use anyhow::{Context as _, Result};
use serde::Deserialize;

use super::transport::{post_messages, with_retry};
use super::wire::{ContentBlockInfo, OutputConfig, OutputFormat, RequestMessages};
use super::{Client, RequestKind, RequestSpec};
use crate::config::auth::Auth;
use crate::message::Message;
use crate::model::{Effort, ModelSpec, TokenUsage};

// ── Side Query ──

/// One side request: a single user message under its own system prompt and output settings.
pub(crate) struct SideQuery<'a> {
    pub(crate) model: ModelSpec,
    pub(crate) system: &'a str,
    pub(crate) user: &'a str,
    pub(crate) max_tokens: u32,
    pub(crate) effort: Option<Effort>,
    /// Dropped when the config turns structured outputs off, so callers can set it unconditionally.
    pub(crate) output_format: Option<&'a OutputFormat>,
}

// ── Client::complete ──

impl Client {
    /// Sends `query` for the live session `session_id` with `auth` without streaming and returns the
    /// concatenated assistant text, dropping tool-use and thinking blocks, with the usage the
    /// response reported. The request carries no agentic betas or `context_management` directive.
    pub(crate) async fn complete(
        &self,
        session_id: &str,
        auth: &Auth,
        query: SideQuery<'_>,
    ) -> Result<(String, Option<TokenUsage>)> {
        let SideQuery {
            model,
            system,
            user,
            max_tokens,
            effort,
            output_format,
        } = query;
        let messages = [Message::user(user)];
        let request = &self.prepare(RequestSpec {
            kind: RequestKind::SideQuery,
            session_id,
            auth,
            model: &model,
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
        COMPLETION_USAGE, SESSION_ID, auth_token, client_for, completion_body, oauth,
        request_count, serve, test_client,
    };
    use crate::client::anthropic::testing::{DEVICE_ID, api_key};
    use crate::client::anthropic::{SYSTEM_PROMPT_PREFIX, attribution_text, build_metadata};
    use crate::config::testing::test_config;

    mod client {
        use super::*;

        mod complete {
            use super::*;

            const HAIKU: &str = "claude-haiku-5-5";

            /// A Haiku side query for `user` under `system`, capped at 40 tokens.
            fn query<'a>(system: &'a str, user: &'a str) -> SideQuery<'a> {
                SideQuery {
                    model: ModelSpec::resolve(HAIKU),
                    system,
                    user,
                    max_tokens: 40,
                    effort: None,
                    output_format: None,
                }
            }

            async fn complete_err(client: &Client, auth: &Auth) -> String {
                let err = client
                    .complete(SESSION_ID, auth, query("", "u"))
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
                let client = test_client(server.uri());

                let (text, usage) = client
                    .complete(SESSION_ID, &api_key(), query("sys", "user input"))
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
                    .complete(SESSION_ID, &api_key(), query("sys", "Fix login"))
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
                        "metadata": {"user_id": build_metadata(DEVICE_ID, SESSION_ID).user_id},
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
            async fn sends_the_caller_session_id_header() {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .and(header("x-claude-code-session-id", "sid-live"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(completion_body("ok")))
                    .expect(1)
                    .mount(&server)
                    .await;
                let client = test_client(server.uri());

                client
                    .complete("sid-live", &api_key(), query("", "u"))
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
                    let mut config = test_config(server.uri(), api_key(), HAIKU);
                    config.gateway.structured_outputs = structured_outputs;
                    let client = client_for(&config);

                    let query = SideQuery {
                        effort: Some(Effort::Low),
                        output_format: Some(&fmt),
                        ..query("sys", "prompt")
                    };

                    client.complete(SESSION_ID, &oauth(), query).await.unwrap();

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
                    let mut config = test_config(server.uri(), api_key(), HAIKU);
                    config.gateway.attribution = attribution;

                    client_for(&config)
                        .complete(SESSION_ID, &auth, query("sys", "Fix login"))
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
                let client = test_client(server.uri());

                let (text, _) = client
                    .complete(SESSION_ID, &api_key(), query("sys", "user input"))
                    .await
                    .unwrap();

                assert_eq!(text, "Fix login bug");
                assert_eq!(request_count(&server).await, 2);
            }

            #[tokio::test]
            async fn http_error_propagates_status_and_body() {
                let server = serve(vec![ResponseTemplate::new(400).set_body_string(
                    r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be positive"}}"#,
                )])
                .await;
                let client = test_client(server.uri());

                let err = complete_err(&client, &api_key()).await;

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
                let client = test_client(server.uri());

                let err = complete_err(&client, &api_key()).await;

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
                let client = test_client(server.uri());

                let err = complete_err(&client, &auth_token()).await;

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
                let client = test_client(server.uri());

                let err = client
                    .complete(SESSION_ID, &api_key(), query("", "u"))
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
                input: serde_json::json!({}),
            }];

            assert_eq!(join_text_blocks(blocks), "");
        }
    }
}
