//! Non-streaming `/v1/messages` one-shot path. Returns flattened assistant text.

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::debug;

use super::betas::compute_betas;
use super::transport::{post_messages, with_retry};
use super::wire::{CreateMessageRequest, OutputConfig, OutputFormat, RequestMessages};
use super::{Client, attribution_text, billing, build_metadata, build_system_blocks};
use crate::config::Effort;
use crate::message::{ContentBlock, Message};
use crate::model::ModelSpec;

// ── Client::complete ──

impl Client {
    /// Non-streaming completion. Returns the concatenated assistant text and drops tool-use and
    /// thinking blocks. `output_format` is dropped when the config turns structured outputs off,
    /// so callers can pass it unconditionally.
    ///
    /// Distinct from [`Client::stream_message`]: no agentic beta stack, no `context_management`
    /// directive, and the billing block ships only on OAuth (see [`Client::attributes`]).
    pub(crate) async fn complete(
        &self,
        model: &ModelSpec,
        system: &str,
        user: &str,
        max_tokens: u32,
        effort: Option<Effort>,
        output_format: Option<&OutputFormat>,
    ) -> Result<String> {
        let attribution = self
            .attributes(false)
            .then(|| attribution_text(user, self.is_first_party));
        let body = build_completion_body(
            model.id(),
            system,
            user,
            max_tokens,
            effort,
            attribution.as_deref(),
            &self.device_id,
            &self.session_id,
            output_format.filter(|_| self.config.gateway.structured_outputs),
        )?;

        debug!(
            model = model.id(),
            body_len = body.len(),
            "sending completion request"
        );

        let betas = compute_betas(model, None, self.config.auth.kind(), false).join(",");
        let request = &self.messages_request(betas, body);
        let CompletionResponse { content } = with_retry(
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
        Ok(join_text_blocks(content))
    }
}

// ── Body Builder ──

/// System block order: `attribution` (when set) → identity prefix → caller system.
#[expect(
    clippy::too_many_arguments,
    reason = "9 distinct request inputs, and a wrapper struct would only rename them"
)]
fn build_completion_body(
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    effort: Option<Effort>,
    attribution: Option<&str>,
    device_id: &str,
    session_id: &str,
    output_format: Option<&OutputFormat>,
) -> Result<String> {
    let messages = [Message::user(user)];

    let system_blocks = build_system_blocks(attribution, [(system, None)]);

    let mut body = serde_json::to_string(&CreateMessageRequest {
        model,
        max_tokens,
        stream: false,
        metadata: build_metadata(device_id, session_id),
        system: system_blocks,
        tools: None,
        thinking: None,
        output_config: OutputConfig::new(output_format, effort),
        context_management: None,
        messages: RequestMessages {
            messages: &messages,
            breakpoint: None,
        },
    })
    .context("failed to serialize request")?;

    if attribution.is_some_and(billing::is_hashed) {
        body = billing::inject_cch(&body)?;
    }
    Ok(body)
}

// ── Response Handling ──

/// Concatenates text blocks; drops tool-use / thinking.
fn join_text_blocks(content: Vec<ContentBlock>) -> String {
    content
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect()
}

#[derive(Deserialize)]
struct CompletionResponse {
    content: Vec<ContentBlock>,
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::super::betas::{CLAUDE_CODE_BETA_HEADER, OAUTH_BETA_HEADER};
    use super::super::testing::{Captured, api_key, auth_token, captured, oauth, test_config};
    use super::*;
    use crate::client::anthropic::testing::completion_body;
    use crate::client::anthropic::{CLAUDE_CLI_VERSION, SYSTEM_PROMPT_PREFIX};
    use crate::config::auth::{Auth, AuthSource};

    fn haiku() -> ModelSpec {
        ModelSpec::resolve("claude-haiku-5-5".to_owned())
    }

    fn parse_body(body: &str) -> serde_json::Value {
        serde_json::from_str(body).expect("serialized body must be valid JSON")
    }

    // ── Client::complete ──

    #[tokio::test]
    async fn complete_sends_x_claude_code_session_id_header() {
        // `/clear` rolls the id without rebuilding the client; the non-streaming path must follow.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-claude-code-session-id", "sid-complete"))
            .respond_with(ResponseTemplate::new(200).set_body_string(completion_body("ok")))
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-haiku-5-5"),
            Some("sid-complete".to_owned()),
        )
        .unwrap();
        client
            .complete(&haiku(), "", "u", 40, None, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn complete_happy_path_produces_assistant_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(completion_body("Fix login bug")),
            )
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-haiku-5-5"),
            Some("sid".to_owned()),
        )
        .unwrap();
        let text = client
            .complete(&haiku(), "sys", "user input", 40, None, None)
            .await
            .unwrap();
        assert_eq!(text, "Fix login bug");
    }

    #[tokio::test]
    async fn complete_http_error_propagates_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"error":{"type":"invalid_request"}}"#),
            )
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-haiku-5-5"),
            Some("sid".to_owned()),
        )
        .unwrap();
        let err = client
            .complete(&haiku(), "", "u", 40, None, None)
            .await
            .expect_err("expected error");
        let msg = format!("{err:#}");
        assert!(msg.contains("400"), "status surfaced: {msg}");
        assert!(msg.contains("invalid_request"), "body surfaced: {msg}");
    }

    #[tokio::test]
    async fn complete_429_surfaces_retry_after_header_in_error() {
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

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-haiku-5-5"),
            Some("sid".to_owned()),
        )
        .unwrap();
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
    async fn complete_401_names_the_gateway_token_variable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), auth_token(), "claude-haiku-5-5"),
            Some("sid".to_owned()),
        )
        .unwrap();
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
    async fn complete_malformed_response_body_errors_with_parse_context() {
        // A 200 with non-JSON must surface a parse error so the title path falls back cleanly.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<not json>"))
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-haiku-5-5"),
            Some("sid".to_owned()),
        )
        .unwrap();
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
    async fn complete_ships_effort_and_format_without_beta_tags_unless_structured_outputs_off() {
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
            let client = Client::new(config, Some("sid".to_owned())).unwrap();
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
    async fn complete_oauth_haiku_carries_billing_block_but_not_gateway_tag() {
        // Non-agentic Haiku: drops the gateway tag, keeps the OAuth billing attestation.
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
                ResponseTemplate::new(200).set_body_string(completion_body("Fix"))
            })
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(
                server.uri(),
                Auth::new(AuthSource::ClaudeCodeLogin, "tok"),
                "claude-haiku-5-5",
            ),
            Some("sid".to_owned()),
        )
        .unwrap();
        _ = client
            .complete(&haiku(), "", "hi", 40, None, None)
            .await
            .unwrap();

        let (body, beta) = sink.lock().unwrap().clone().expect("request captured");
        assert!(beta.contains(OAUTH_BETA_HEADER), "OAuth tag: {beta}");
        assert!(
            !beta.contains(CLAUDE_CODE_BETA_HEADER),
            "no gateway tag on Haiku one-shot: {beta}",
        );
        assert!(
            body.contains("x-anthropic-billing-header:"),
            "billing block present: {body}",
        );
        assert!(!body.contains("cch=00000"), "cch populated: {body}");
    }

    #[tokio::test]
    async fn complete_does_not_emit_context_management_edits() {
        // `context_management.edits` is agentic-only, so `complete` omits it even on known models.
        let server = MockServer::start().await;
        let sink: Captured<String> = captured();
        let sink_clone = std::sync::Arc::clone(&sink);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(move |req: &Request| {
                *sink_clone.lock().unwrap() = Some(String::from_utf8_lossy(&req.body).into_owned());
                ResponseTemplate::new(200).set_body_string(completion_body("ok"))
            })
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-haiku-5-5"),
            Some("sid".to_owned()),
        )
        .unwrap();
        _ = client
            .complete(&haiku(), "sys", "hi", 40, None, None)
            .await
            .unwrap();

        let body = sink.lock().unwrap().clone().expect("body captured");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            v.get("context_management").is_none(),
            "context_management absent on one-shot path: {body}",
        );
    }

    // ── build_completion_body ──

    #[test]
    fn build_completion_body_omits_tools_thinking_and_output_config_by_default() {
        let body = build_completion_body(
            "claude-haiku-5-5",
            "sys",
            "hi",
            40,
            None,
            None,
            "did",
            "sid",
            None,
        )
        .unwrap();
        let v = parse_body(&body);
        assert_eq!(v["model"], "claude-haiku-5-5");
        assert_eq!(v["max_tokens"], 40);
        assert_eq!(v["stream"], false);
        assert!(v.get("tools").is_none(), "tools omitted: {v}");
        assert!(v.get("thinking").is_none(), "thinking omitted: {v}");
        assert!(
            v.get("output_config").is_none(),
            "output_config omitted: {v}"
        );
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn build_completion_body_attribution_prepends_attested_billing_block() {
        let plain_body = build_completion_body(
            "claude-haiku-5-5",
            "sys-prompt",
            "hi",
            40,
            None,
            None,
            "did",
            "sid",
            None,
        )
        .unwrap();
        let plain = parse_body(&plain_body);
        let plain_system = plain["system"].as_array().unwrap();
        assert_eq!(plain_system.len(), 2);
        assert_eq!(plain_system[0]["text"], SYSTEM_PROMPT_PREFIX);
        assert_eq!(plain_system[1]["text"], "sys-prompt");
        assert!(!plain_body.contains("x-anthropic-billing-header:"));

        let attributed_body = build_completion_body(
            "claude-haiku-5-5",
            "sys-prompt",
            "Fix login",
            40,
            None,
            Some(&attribution_text("Fix login", true)),
            "did",
            "sid",
            None,
        )
        .unwrap();
        let attributed = parse_body(&attributed_body);
        let attributed_system = attributed["system"].as_array().unwrap();
        assert_eq!(attributed_system.len(), 3);
        let first = attributed_system[0]["text"].as_str().unwrap();
        assert!(first.starts_with("x-anthropic-billing-header:"));
        assert!(first.contains(&format!("cc_version={CLAUDE_CLI_VERSION}")));
        assert_eq!(attributed_system[1]["text"], SYSTEM_PROMPT_PREFIX);
        assert_eq!(attributed_system[2]["text"], "sys-prompt");
        assert!(!attributed_body.contains("cch=00000"));
    }

    #[test]
    fn build_completion_body_empty_system_keeps_identity_prefix_alone() {
        // Non-Haiku OAuth requires the identity prefix in block 0 even with no caller system.
        let body = build_completion_body(
            "claude-haiku-5-5",
            "",
            "hi",
            40,
            None,
            None,
            "did",
            "sid",
            None,
        )
        .unwrap();
        let v = parse_body(&body);
        let system = v["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["text"], SYSTEM_PROMPT_PREFIX);
    }

    #[test]
    fn build_completion_body_with_effort_or_format_emits_output_config() {
        let fmt = OutputFormat::json_schema(serde_json::json!({"type": "object"}));
        for (effort, format, expected) in [
            (
                Some(Effort::Low),
                None,
                serde_json::json!({"effort": "low"}),
            ),
            (
                None,
                Some(&fmt),
                serde_json::json!({"format": {"type": "json_schema", "schema": {"type": "object"}}}),
            ),
        ] {
            let body = build_completion_body(
                "claude-haiku-5-5",
                "sys",
                "hi",
                40,
                effort,
                None,
                "did",
                "sid",
                format,
            )
            .unwrap();
            assert_eq!(parse_body(&body)["output_config"], expected);
        }
    }

    #[test]
    fn build_completion_body_routes_device_and_session_ids_into_metadata() {
        let body = build_completion_body(
            "claude-haiku-5-5",
            "",
            "hi",
            40,
            None,
            None,
            "did-456",
            "sid-789",
            None,
        )
        .unwrap();
        assert!(
            body.contains("sid-789"),
            "session_id threads into metadata.user_id: {body}",
        );
        assert!(
            body.contains("did-456"),
            "device_id threads into metadata.user_id: {body}",
        );
    }

    // ── join_text_blocks / CompletionResponse ──

    #[test]
    fn join_text_blocks_concatenates_text_and_drops_non_text_blocks() {
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
    fn join_text_blocks_is_empty_for_tool_only_response() {
        // Empty signals "parse failure, keep fallback" to the title caller.
        let blocks = vec![ContentBlock::ToolUse {
            id: "t1".to_owned(),
            name: "noop".to_owned(),
            input: serde_json::Value::Null,
        }];
        assert_eq!(join_text_blocks(blocks), "");
    }
}
