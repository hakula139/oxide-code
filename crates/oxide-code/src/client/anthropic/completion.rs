//! Non-streaming `/v1/messages` one-shot path. Returns flattened assistant text.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::debug;

use super::betas::{api_model_id, compute_betas, supports_structured_outputs};
use super::sse::format_api_error;
use super::wire::{CreateMessageRequest, OutputConfig, OutputFormat};
use super::{CLAUDE_CLI_VERSION, Client, billing, build_metadata, build_system_blocks};
use crate::config::Auth;
use crate::message::{ContentBlock, Message};

// ── Client::complete ──

impl Client {
    /// Non-streaming completion. Returns the concatenated assistant text; tool-use and thinking
    /// blocks are dropped. `output_format` is silently ignored on models without the
    /// structured-outputs capability so callers can pass it unconditionally.
    ///
    /// Distinct from [`Client::stream_message`]: no agentic beta stack, no `context_management`
    /// directive, and the billing block ships **only** on OAuth — API-key one-shots omit it
    /// because 3P gateways do not gate non-streaming traffic on the attestation.
    pub(crate) async fn complete(
        &self,
        model: &str,
        system: &str,
        user: &str,
        max_tokens: u32,
        output_format: Option<&OutputFormat>,
    ) -> Result<String> {
        let effective_format = output_format.filter(|_| supports_structured_outputs(model));
        let body = build_completion_body(
            model,
            system,
            user,
            max_tokens,
            &self.config.auth,
            &self.device_id,
            &self.session_id,
            effective_format,
        )?;

        let url = format!("{}/v1/messages?beta=true", self.config.base_url);
        debug!(model, body_len = body.len(), "sending completion request");

        let betas =
            compute_betas(model, &self.config.auth, false, effective_format.is_some()).join(",");
        let response = self
            .http
            .post(&url)
            .header("anthropic-beta", betas)
            .header("x-claude-code-session-id", &self.session_id)
            .body(body)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let body = response.text().await.unwrap_or_default();
            bail!(
                "{}",
                format_api_error(status, retry_after.as_deref(), &body)
            );
        }

        let CompletionResponse { content } = response
            .json()
            .await
            .context("failed to parse completion response")?;
        Ok(join_text_blocks(content))
    }
}

// ── Body Builder ──

/// System block order: billing (OAuth only) → identity prefix → caller system.
#[expect(
    clippy::too_many_arguments,
    reason = "8 distinct wire fields; a wrapper struct would just rename them"
)]
fn build_completion_body(
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    auth: &Auth,
    device_id: &str,
    session_id: &str,
    output_format: Option<&OutputFormat>,
) -> Result<String> {
    let messages = [Message::user(user)];

    let billing_header = matches!(auth, Auth::OAuth(_)).then(|| {
        let fingerprint = billing::compute_fingerprint(user, CLAUDE_CLI_VERSION);
        billing::build_billing_header(CLAUDE_CLI_VERSION, &fingerprint)
    });

    let system_blocks = build_system_blocks(billing_header.as_deref(), [(system, None)]);

    let mut body = serde_json::to_string(&CreateMessageRequest {
        model: api_model_id(model),
        max_tokens,
        stream: false,
        metadata: build_metadata(device_id, session_id),
        system: system_blocks,
        tools: None,
        thinking: None,
        output_config: OutputConfig::new(output_format, None),
        context_management: None,
        messages: &messages,
    })
    .context("failed to serialize request")?;

    if billing_header.is_some() {
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

    use super::super::betas::{
        CLAUDE_CODE_BETA_HEADER, OAUTH_BETA_HEADER, STRUCTURED_OUTPUTS_BETA_HEADER,
    };
    use super::super::testing::{Captured, api_key, captured, oauth, test_config};
    use super::*;
    use crate::client::anthropic::SYSTEM_PROMPT_PREFIX;
    use crate::client::anthropic::testing::completion_body;

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
            test_config(server.uri(), api_key(), "claude-haiku-4-5"),
            Some("sid-complete".to_owned()),
        )
        .unwrap();
        client
            .complete("claude-haiku-4-5", "", "u", 40, None)
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
            test_config(server.uri(), api_key(), "claude-haiku-4-5"),
            Some("sid".to_owned()),
        )
        .unwrap();
        let text = client
            .complete("claude-haiku-4-5", "sys", "user input", 40, None)
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
            test_config(server.uri(), api_key(), "claude-haiku-4-5"),
            Some("sid".to_owned()),
        )
        .unwrap();
        let err = client
            .complete("claude-haiku-4-5", "", "u", 40, None)
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
                    .insert_header("retry-after", "42")
                    .set_body_string(r#"{"error":{"type":"rate_limit_error"}}"#),
            )
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-haiku-4-5"),
            Some("sid".to_owned()),
        )
        .unwrap();
        let err = client
            .complete("claude-haiku-4-5", "", "u", 40, None)
            .await
            .expect_err("expected 429 error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("retry after 42"),
            "retry-after threaded: {msg}"
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
            test_config(server.uri(), api_key(), "claude-haiku-4-5"),
            Some("sid".to_owned()),
        )
        .unwrap();
        let err = client
            .complete("claude-haiku-4-5", "", "u", 40, None)
            .await
            .expect_err("expected parse error");
        assert!(
            format!("{err:#}").contains("failed to parse completion response"),
            "parse context: {err:#}",
        );
    }

    #[tokio::test]
    async fn complete_structured_output_gated_by_model_capability() {
        // Supported → body + beta tag ship; unsupported → both silently dropped.
        let fmt = OutputFormat::json_schema(serde_json::json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"],
            "additionalProperties": false,
        }));
        for (model, expect_structured) in [("claude-haiku-4-5", true), ("claude-haiku-4", false)] {
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

            let client = Client::new(
                test_config(server.uri(), api_key(), model),
                Some("sid".to_owned()),
            )
            .unwrap();
            _ = client
                .complete(model, "sys", "prompt", 40, Some(&fmt))
                .await
                .unwrap();

            let (body, beta) = sink.lock().unwrap().clone().expect("request captured");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                beta.contains(STRUCTURED_OUTPUTS_BETA_HEADER),
                expect_structured,
                "beta tag on {model}: {beta}",
            );
            assert_eq!(
                v.get("output_config").is_some(),
                expect_structured,
                "output_config on {model}: {body}",
            );
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
                Auth::OAuth("tok".to_owned()),
                "claude-haiku-4-5",
            ),
            Some("sid".to_owned()),
        )
        .unwrap();
        _ = client
            .complete("claude-haiku-4-5", "", "hi", 40, None)
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
        // `context_management.edits` is agentic-only; must stay off `complete` even on
        // capability-bearing models (Haiku 4.5 here).
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
            test_config(server.uri(), api_key(), "claude-haiku-4-5"),
            Some("sid".to_owned()),
        )
        .unwrap();
        _ = client
            .complete("claude-haiku-4-5", "sys", "hi", 40, None)
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
            "claude-haiku-4-5",
            "sys",
            "hi",
            40,
            &api_key(),
            "did",
            "sid",
            None,
        )
        .unwrap();
        let v = parse_body(&body);
        assert_eq!(v["model"], "claude-haiku-4-5");
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
    fn build_completion_body_system_blocks_match_auth_mode() {
        // API key → 2 blocks (identity + system); OAuth → 3 blocks (billing + identity + system).
        let api_body = build_completion_body(
            "claude-haiku-4-5",
            "sys-prompt",
            "hi",
            40,
            &api_key(),
            "did",
            "sid",
            None,
        )
        .unwrap();
        let api = parse_body(&api_body);
        let api_system = api["system"].as_array().unwrap();
        assert_eq!(api_system.len(), 2);
        assert_eq!(api_system[0]["text"], SYSTEM_PROMPT_PREFIX);
        assert_eq!(api_system[1]["text"], "sys-prompt");
        assert!(!api_body.contains("x-anthropic-billing-header:"));

        let oauth_body = build_completion_body(
            "claude-haiku-4-5",
            "sys-prompt",
            "Fix login",
            40,
            &oauth(),
            "did",
            "sid",
            None,
        )
        .unwrap();
        let oa = parse_body(&oauth_body);
        let oa_system = oa["system"].as_array().unwrap();
        assert_eq!(oa_system.len(), 3);
        let first = oa_system[0]["text"].as_str().unwrap();
        assert!(first.starts_with("x-anthropic-billing-header:"));
        assert!(first.contains(&format!("cc_version={CLAUDE_CLI_VERSION}")));
        assert_eq!(oa_system[1]["text"], SYSTEM_PROMPT_PREFIX);
        assert_eq!(oa_system[2]["text"], "sys-prompt");
        assert!(!oauth_body.contains("cch=00000"));
    }

    #[test]
    fn build_completion_body_empty_system_keeps_identity_prefix_alone() {
        // Non-Haiku OAuth requires the identity prefix in block 0 even with no caller system.
        let body = build_completion_body(
            "claude-haiku-4-5",
            "",
            "hi",
            40,
            &api_key(),
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
    fn build_completion_body_with_output_format_emits_output_config() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"],
            "additionalProperties": false,
        });
        let fmt = OutputFormat::json_schema(schema.clone());
        let body = build_completion_body(
            "claude-haiku-4-5",
            "sys",
            "hi",
            40,
            &api_key(),
            "did",
            "sid",
            Some(&fmt),
        )
        .unwrap();
        let v = parse_body(&body);
        assert_eq!(v["output_config"]["format"]["type"], "json_schema");
        assert_eq!(v["output_config"]["format"]["schema"], schema);
    }

    #[test]
    fn build_completion_body_routes_device_and_session_ids_into_metadata() {
        let body = build_completion_body(
            "claude-haiku-4-5",
            "",
            "hi",
            40,
            &api_key(),
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
            "model": "claude-haiku-4-5",
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
