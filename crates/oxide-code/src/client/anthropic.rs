//! Anthropic Messages API streaming client.
//!
//! [`Client::stream_message`] drives the main agent loop: assembles
//! the request (identity prefix, billing attestation for OAuth,
//! static / dynamic system-block split for cache reuse), POSTs
//! `/v1/messages` with SSE streaming, and forwards parsed
//! [`StreamEvent`]s on an mpsc channel. [`Client::complete`] (in
//! [`completion`]) covers non-streaming one-shots.
//!
//! Per-request `anthropic-beta` headers are computed from the model's
//! [`crate::model::Capabilities`] via [`betas::compute_betas`], so
//! gateways that reject unsupported betas don't 400 on spurious
//! feature flags.

mod betas;
mod billing;
mod completion;
mod sse;
pub(crate) mod wire;

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use tokio::sync::mpsc;
use tracing::debug;
use uuid::Uuid;

use crate::config::{Auth, Config};
use crate::message::{ContentBlock, Message, Role};
use crate::prompt::SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
use crate::tool::ToolDefinition;

use betas::{compute_betas, static_prefix_cache_control};
use sse::stream_sse;
use wire::{
    CacheControl, ContextManagement, CreateMessageRequest, OutputConfig, RequestMetadata,
    StreamEvent, SystemBlock,
};

const API_VERSION: &str = "2023-06-01";

/// Matches the installed Claude Code version. The rest of this PR is
/// pinned against 2.1.119 packet captures; keep the wire
/// `User-Agent` / `cc_version` claim aligned.
const CLAUDE_CLI_VERSION: &str = "2.1.119";

/// OAuth-required identity prefix. The Anthropic API returns 429 for non-Haiku
/// models with OAuth tokens unless the system prompt starts with this exact
/// string in its own text block.
const SYSTEM_PROMPT_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

#[derive(Clone)]
pub(crate) struct Client {
    http: reqwest::Client,
    config: Config,
    session_id: String,
    /// Whether `config.base_url` points at the first-party Anthropic
    /// API. Computed once at construction so per-request paths don't
    /// re-parse the URL — the value gates the `prompt-caching-scope`
    /// beta and `cache_control.scope: "global"`, which 3P gateways
    /// reject.
    is_first_party: bool,
}

impl Client {
    pub(crate) fn new(config: Config, session_id: Option<String>) -> Result<Self> {
        let session_id = session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let is_first_party = betas::is_first_party_base_url(&config.base_url);
        let mut headers = HeaderMap::new();

        match &config.auth {
            Auth::ApiKey(key) => {
                let mut value = HeaderValue::from_str(key)?;
                value.set_sensitive(true);
                headers.insert("x-api-key", value);
            }
            Auth::OAuth(token) => {
                let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
                value.set_sensitive(true);
                headers.insert(AUTHORIZATION, value);
            }
        }

        // `anthropic-beta` is set per-request in `stream_message` /
        // `complete` because the accepted set varies by model and call
        // type — see [`betas::compute_betas`].
        headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
        headers.insert(
            "anthropic-dangerous-direct-browser-access",
            HeaderValue::from_static("true"),
        );

        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!("claude-cli/{CLAUDE_CLI_VERSION} (external, cli)"))?,
        );

        // Client identification, mirroring Claude Code's Stainless SDK —
        // third-party gateways may check for their presence.
        headers.insert("x-app", HeaderValue::from_static("cli"));
        headers.insert(
            "x-claude-code-session-id",
            HeaderValue::from_str(&session_id)?,
        );
        headers.insert("x-stainless-lang", HeaderValue::from_static("js"));
        headers.insert(
            "x-stainless-os",
            HeaderValue::from_static(normalize_platform(std::env::consts::OS)),
        );
        headers.insert(
            "x-stainless-arch",
            HeaderValue::from_static(normalize_arch(std::env::consts::ARCH)),
        );

        // No whole-request timeout — assistant responses can legitimately
        // run for minutes. The 60 s read timeout catches slowloris dribble;
        // Anthropic sends keepalives every ~15 s on healthy streams.
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_mins(1))
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            http,
            config,
            session_id,
            is_first_party,
        })
    }

    /// Returns the model name for use in the system prompt.
    pub(crate) fn model(&self) -> &str {
        &self.config.model
    }

    /// Client-side session id carried in `x-claude-code-session-id` and
    /// billing metadata. Caller-supplied or auto-generated UUID v4.
    #[cfg(test)]
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Stream a message response from the Anthropic API.
    ///
    /// `system_sections` ship as individual `system` text blocks so
    /// `cache_control` can apply to the static prefix only. `user_context`
    /// is prepended as a synthetic user message (keeping dynamic content
    /// like CLAUDE.md out of the cacheable `system` parameter).
    ///
    /// System-block ordering is delegated to [`build_system_blocks`].
    /// The static section is the only block carrying `cache_control` —
    /// `scope=global` on 1P, default org-scoped on 3P.
    ///
    /// Returns an mpsc receiver of [`StreamEvent`]s.
    pub(crate) fn stream_message(
        &self,
        messages: &[Message],
        system_sections: &[&str],
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        let effective_messages: Vec<Message> = match user_context {
            Some(ctx) => std::iter::once(Message::user(ctx))
                .chain(messages.iter().cloned())
                .collect(),
            None => messages.to_vec(),
        };

        let billing_header = matches!(self.config.auth, Auth::OAuth(_)).then(|| {
            let fingerprint = billing::compute_fingerprint(
                first_user_text(&effective_messages),
                CLAUDE_CLI_VERSION,
            );
            billing::build_billing_header(CLAUDE_CLI_VERSION, &fingerprint)
        });

        let (static_sections, dynamic_sections) = split_at_boundary(system_sections);
        let static_joined = static_sections.join("\n\n");
        let dynamic_joined = dynamic_sections.join("\n\n");

        let static_cache_control =
            static_prefix_cache_control(self.is_first_party, self.config.prompt_cache_ttl);
        let system_blocks = build_system_blocks(
            billing_header.as_deref(),
            [
                (static_joined.as_str(), Some(static_cache_control)),
                (dynamic_joined.as_str(), None),
            ],
        );

        let caps = crate::model::capabilities_for(&self.config.model);

        let url = format!("{}/v1/messages?beta=true", self.config.base_url);
        let mut body = serde_json::to_string(&CreateMessageRequest {
            // `[1m]` is a client-side tag; strip before the wire.
            model: betas::api_model_id(&self.config.model),
            max_tokens: self.config.max_tokens,
            stream: true,
            metadata: build_metadata(&self.session_id),
            system: system_blocks,
            tools: (!tools.is_empty()).then_some(tools),
            thinking: self.config.thinking.as_ref(),
            output_config: OutputConfig::new(None, self.config.effort),
            // Gated on the same capability flag as the
            // `context-management-2025-06-27` beta header so body and
            // header stay in sync — claude-code 2.1.119 ships them
            // together on every 4.6+ agentic request.
            context_management: caps
                .context_management
                .then(ContextManagement::clear_thinking_keep_all),
            messages: &effective_messages,
        })
        .context("failed to serialize request")?;

        if billing_header.is_some() {
            body = billing::inject_cch(&body)?;
        }

        debug!(body_len = body.len(), "sending API request");

        let (tx, rx) = mpsc::channel(64);
        let http = self.http.clone();
        let betas = compute_betas(
            &self.config.model,
            &self.config.auth,
            true,
            false,
            self.is_first_party,
        )
        .join(",");

        tokio::spawn(async move {
            let result = stream_sse(&http, &url, betas, body, &tx).await;
            if let Err(e) = result {
                _ = tx.send(Err(e)).await;
            }
        });

        Ok(rx)
    }
}

/// Builds the `metadata.user_id` field as a stringified JSON object.
fn build_metadata(session_id: &str) -> RequestMetadata {
    let user_id = serde_json::json!({ "session_id": session_id }).to_string();
    RequestMetadata { user_id }
}

/// Assembles the `system` block sequence shared by streaming and
/// one-shot paths. Order is load-bearing: billing's `cch=00000`
/// placeholder must serialize first so [`billing::inject_cch`]'s
/// single-occurrence replacement is unambiguous, and the identity
/// prefix must occupy its own block on non-Haiku OAuth.
///
/// Empty `extras` entries are dropped so callers can hand in optional
/// sections without `if !text.is_empty()` guards at every site.
fn build_system_blocks<'a, const N: usize>(
    billing_header: Option<&'a str>,
    extras: [(&'a str, Option<CacheControl>); N],
) -> Vec<SystemBlock<'a>> {
    let mut blocks = Vec::with_capacity(2 + N);
    if let Some(text) = billing_header {
        blocks.push(SystemBlock {
            r#type: "text",
            text,
            cache_control: None,
        });
    }
    blocks.push(SystemBlock {
        r#type: "text",
        text: SYSTEM_PROMPT_PREFIX,
        cache_control: None,
    });
    for (text, cache_control) in extras {
        if !text.is_empty() {
            blocks.push(SystemBlock {
                r#type: "text",
                text,
                cache_control,
            });
        }
    }
    blocks
}

/// Maps `std::env::consts::OS` to the Stainless SDK's `normalizePlatform` names.
fn normalize_platform(os: &str) -> &'static str {
    match os {
        "macos" => "MacOS",
        "linux" => "Linux",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        "openbsd" => "OpenBSD",
        "ios" => "iOS",
        "android" => "Android",
        _ => "Unknown",
    }
}

/// Maps `std::env::consts::ARCH` to the Stainless SDK's `normalizeArch` names.
fn normalize_arch(arch: &str) -> &'static str {
    match arch {
        "x86" => "x32",
        "x86_64" => "x64",
        "arm" => "arm",
        "aarch64" => "arm64",
        _ => "unknown",
    }
}

/// Splits system sections at the boundary marker into static and dynamic parts.
///
/// Returns `(static_sections, dynamic_sections)`. The boundary marker itself
/// is excluded from both. Sections before the boundary are static (globally
/// cacheable); sections after are dynamic (per-session).
fn split_at_boundary<'a>(sections: &[&'a str]) -> (Vec<&'a str>, Vec<&'a str>) {
    let boundary_pos = sections
        .iter()
        .position(|&s| s == SYSTEM_PROMPT_DYNAMIC_BOUNDARY);

    if let Some(pos) = boundary_pos {
        let static_part = sections[..pos]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect();
        let dynamic_part = sections[pos + 1..]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect();
        (static_part, dynamic_part)
    } else {
        // No boundary — treat everything as static.
        let all = sections.iter().filter(|s| !s.is_empty()).copied().collect();
        (all, Vec::new())
    }
}

/// Extracts the text of the first user message for fingerprint computation.
fn first_user_text(messages: &[Message]) -> &str {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .into_iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

// ── Test Fixtures ──
//
// Shared across `client::anthropic` tests, `session::title_generator`
// tests, and the `agent::tests` wiremock integration — all three drive
// a real `Client` against a mock server and need the same defaults
// (ApiKey auth, session id, 128 max_tokens, no thinking config).

/// Minimal [`Config`] suitable for unit and wiremock tests. Defaults
/// match every existing call site: `max_tokens = 128`, `thinking = None`,
/// `show_thinking = false`.
#[cfg(test)]
pub(crate) fn test_config(base_url: impl Into<String>, auth: Auth, model: &str) -> Config {
    use crate::config::PromptCacheTtl;

    Config {
        auth,
        base_url: base_url.into(),
        model: model.to_owned(),
        effort: None,
        max_tokens: 128,
        prompt_cache_ttl: PromptCacheTtl::OneHour,
        thinking: None,
        show_thinking: false,
    }
}

/// [`Client`] on top of [`test_config`], with a fixed session id so the
/// wire headers carry a deterministic `x-claude-code-session-id`.
#[cfg(test)]
pub(crate) fn test_client(base_url: impl Into<String>, auth: Auth, model: &str) -> Client {
    Client::new(test_config(base_url, auth, model), Some("sid".to_owned())).unwrap()
}

/// Non-streaming Messages-API response body with the given text content.
/// Model is hardcoded; assertions in tests inspect request-side model
/// selection, never response-side.
#[cfg(test)]
pub(crate) fn completion_body(text: &str) -> String {
    serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": 5, "output_tokens": 3}
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use wiremock::matchers::{header, header_regex, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::wire::{ContentBlockInfo, Delta};
    use super::*;
    use crate::config::{Effort, ThinkingConfig};

    // ── Fixtures ──

    const OFFLINE_URL: &str = "https://example.invalid";
    const TEST_MODEL: &str = "claude-sonnet-4-6";

    fn api_key() -> Auth {
        Auth::ApiKey("k".to_owned())
    }

    fn oauth() -> Auth {
        Auth::OAuth("t".to_owned())
    }

    /// Builds an SSE response body from `(event, data)` pairs. Each
    /// frame is emitted as `event: <name>\ndata: <json>\n\n`, encoding
    /// the frame-separator invariant in one place so call sites don't
    /// hand-roll it (and can't silently omit the `\n\n`).
    fn sse_body(frames: &[(&str, &str)]) -> String {
        use std::fmt::Write;
        let mut body = String::new();
        for (event, data) in frames {
            writeln!(body, "event: {event}").unwrap();
            writeln!(body, "data: {data}").unwrap();
            body.push('\n');
        }
        body
    }

    async fn collect_events(
        mut rx: mpsc::Receiver<Result<StreamEvent>>,
    ) -> Result<Vec<StreamEvent>> {
        let mut out = Vec::new();
        while let Some(event) = rx.recv().await {
            out.push(event?);
        }
        Ok(out)
    }

    /// Well-formed SSE body for a short text response.
    fn text_stream_body() -> String {
        sse_body(&[
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-6","usage":{"input_tokens":5,"output_tokens":0}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ])
    }

    /// Slot for the last request body captured by a wiremock responder.
    type Captured<T> = Arc<Mutex<Option<T>>>;

    fn captured<T>() -> Captured<T> {
        Arc::new(Mutex::new(None))
    }

    // ── Client::new / Client::model ──

    #[test]
    fn new_with_api_key_exposes_model() {
        let client = Client::new(
            test_config(OFFLINE_URL, Auth::ApiKey("sk-test".to_owned()), TEST_MODEL),
            None,
        )
        .unwrap();
        assert_eq!(client.model(), "claude-sonnet-4-6");
    }

    #[test]
    fn new_with_oauth_token_exposes_model() {
        let client = Client::new(
            test_config(
                OFFLINE_URL,
                Auth::OAuth("oauth-token".to_owned()),
                TEST_MODEL,
            ),
            None,
        )
        .unwrap();
        assert_eq!(client.model(), "claude-sonnet-4-6");
    }

    #[test]
    fn new_none_session_id_generates_uuid_v4() {
        let client = Client::new(
            test_config(OFFLINE_URL, Auth::ApiKey("k".to_owned()), TEST_MODEL),
            None,
        )
        .unwrap();
        let sid = client.session_id();
        let parsed = Uuid::parse_str(sid)
            .unwrap_or_else(|_| panic!("auto-generated session_id is not a UUID: {sid:?}"));
        assert_eq!(parsed.get_version_num(), 4);
    }

    #[test]
    fn new_preserves_explicit_session_id() {
        let sid = "11111111-2222-4333-8444-555555555555".to_owned();
        let client = Client::new(
            test_config(OFFLINE_URL, Auth::ApiKey("k".to_owned()), TEST_MODEL),
            Some(sid.clone()),
        )
        .unwrap();
        assert_eq!(client.session_id(), sid);
    }

    #[test]
    fn new_rejects_auth_values_containing_invalid_header_bytes() {
        // `HeaderValue::from_str` rejects control chars (\n, \r); both
        // auth arms must propagate the error instead of panicking.
        for auth in [
            Auth::ApiKey("bad\nkey".to_owned()),
            Auth::OAuth("bad\rtoken".to_owned()),
        ] {
            // `Client` has no Debug derive, so .unwrap_err() doesn't
            // compile — use .err().unwrap() on the Option instead.
            let err = Client::new(
                test_config(OFFLINE_URL, auth, TEST_MODEL),
                Some("sid".to_owned()),
            )
            .err()
            .unwrap();
            assert!(
                format!("{err:#}").to_ascii_lowercase().contains("header"),
                "error should mention header: {err:#}",
            );
        }
    }

    // ── Client::stream_message ──

    #[tokio::test]
    async fn stream_message_happy_text_emits_start_delta_stop_in_order() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(query_param("beta", "true"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body()),
            )
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-sonnet-4-6"),
            Some("sid".to_owned()),
        )
        .unwrap();
        let rx = client
            .stream_message(&[Message::user("hello")], &[], None, &[])
            .unwrap();
        let events = collect_events(rx).await.unwrap();

        assert!(
            matches!(&events[0], StreamEvent::MessageStart { message } if message.id == "msg_1"),
        );
        assert!(matches!(
            &events[1],
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockInfo::Text { .. }
            },
        ));
        let StreamEvent::ContentBlockDelta {
            index: 0,
            delta: Delta::TextDelta { text },
        } = &events[2]
        else {
            panic!("expected text delta, got {:?}", events[2]);
        };
        assert_eq!(text, "Hi");
        assert!(matches!(events[5], StreamEvent::MessageStop));
    }

    #[tokio::test]
    async fn stream_message_preserves_multibyte_codepoints_in_deltas() {
        // Pins the byte-level SSE buffer: a chunk decoded as lossy UTF-8
        // would mangle a 4-byte emoji split across TCP chunk boundaries.
        let server = MockServer::start().await;
        let body = sse_body(&[
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"🦀rust"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-sonnet-4-6"),
            Some("sid".to_owned()),
        )
        .unwrap();
        let events = collect_events(
            client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap(),
        )
        .await
        .unwrap();
        let got = events.iter().find_map(|e| match e {
            StreamEvent::ContentBlockDelta {
                delta: Delta::TextDelta { text },
                ..
            } => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(got, Some("🦀rust"));
    }

    #[tokio::test]
    async fn stream_message_malformed_frame_is_skipped_without_poisoning_stream() {
        // The valid delta after a malformed frame must still deliver —
        // one bad frame cannot poison the whole turn.
        let server = MockServer::start().await;
        let body = sse_body(&[
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            ("content_block_delta", "{not valid json"),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-sonnet-4-6"),
            Some("sid".to_owned()),
        )
        .unwrap();
        let events = collect_events(
            client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap(),
        )
        .await
        .unwrap();
        let delta = events.iter().find_map(|e| match e {
            StreamEvent::ContentBlockDelta {
                delta: Delta::TextDelta { text },
                ..
            } => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(delta, Some("Hi"));
    }

    #[tokio::test]
    async fn stream_message_mid_stream_error_event_is_delivered_with_api_payload() {
        // `StreamEvent::Error` flows as `Ok(Error { .. })` on the channel;
        // the caller (`agent.rs`) converts it to a bail!.
        let server = MockServer::start().await;
        let body = sse_body(&[(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Servers overloaded"}}"#,
        )]);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-sonnet-4-6"),
            Some("sid".to_owned()),
        )
        .unwrap();
        let events = collect_events(
            client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap(),
        )
        .await
        .unwrap();
        let err = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Error { error } => Some(error),
                _ => None,
            })
            .expect("error event must be delivered");
        assert_eq!(err.error_type, "overloaded_error");
        assert_eq!(err.message, "Servers overloaded");
    }

    #[tokio::test]
    async fn stream_message_http_error_propagates_status_and_body() {
        for (status, body) in [
            (429_u16, r#"{"error":{"type":"rate_limit_error"}}"#),
            (529, "overloaded"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;

            let client = Client::new(
                test_config(server.uri(), api_key(), "claude-sonnet-4-6"),
                Some("sid".to_owned()),
            )
            .unwrap();
            let rx = client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap();
            let err = collect_events(rx).await.expect_err("expected HTTP error");
            let msg = format!("{err:#}");
            assert!(
                msg.contains(&status.to_string()),
                "status {status} in error: {msg}",
            );
            assert!(msg.contains(body), "body surfaced in error: {msg}");
        }
    }

    #[tokio::test]
    async fn stream_message_receiver_dropped_mid_stream_does_not_deadlock() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body())
                    .set_delay(Duration::from_millis(50)),
            )
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-sonnet-4-6"),
            Some("sid".to_owned()),
        )
        .unwrap();
        let mut rx = client
            .stream_message(&[Message::user("hi")], &[], None, &[])
            .unwrap();
        _ = rx.recv().await;
        drop(rx);
        // Lets the background task observe the closed channel and exit;
        // any panic would surface in test output.
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    #[tokio::test]
    async fn stream_message_api_key_sends_x_api_key_and_session_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-test"))
            .and(header("x-claude-code-session-id", "sid-abc"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body()),
            )
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(
                server.uri(),
                Auth::ApiKey("sk-test".to_owned()),
                "claude-sonnet-4-6",
            ),
            Some("sid-abc".to_owned()),
        )
        .unwrap();
        // A missing header on either matcher would 404 the mock and
        // surface as an HTTP error; success proves both are present.
        collect_events(
            client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stream_message_oauth_sends_bearer_plus_oauth_and_gateway_beta_tags() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("authorization", "Bearer tok"))
            .and(header_regex("anthropic-beta", r"oauth-2025-04-20"))
            .and(header_regex("anthropic-beta", r"claude-code-20250219"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body()),
            )
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(
                server.uri(),
                Auth::OAuth("tok".to_owned()),
                "claude-sonnet-4-6",
            ),
            Some("sid".to_owned()),
        )
        .unwrap();
        collect_events(
            client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stream_message_billing_block_is_oauth_only_with_cch_populated() {
        // OAuth must inject the billing header and replace the
        // `cch=00000` placeholder; API-key auth must do neither.
        for (auth, expect_billing) in [(api_key(), false), (oauth(), true)] {
            let server = MockServer::start().await;
            let body_sink: Captured<String> = captured();
            let sink_clone = std::sync::Arc::clone(&body_sink);
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(move |req: &Request| {
                    *sink_clone.lock().unwrap() =
                        Some(String::from_utf8_lossy(&req.body).into_owned());
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(text_stream_body())
                })
                .mount(&server)
                .await;

            let client = Client::new(
                test_config(server.uri(), auth, "claude-sonnet-4-6"),
                Some("sid".to_owned()),
            )
            .unwrap();
            collect_events(
                client
                    .stream_message(&[Message::user("hi")], &[], None, &[])
                    .unwrap(),
            )
            .await
            .unwrap();

            let body = body_sink.lock().unwrap().clone().expect("body captured");
            let has_billing = body.contains("x-anthropic-billing-header:");
            assert_eq!(
                has_billing, expect_billing,
                "billing block presence: {body}"
            );
            if expect_billing {
                assert!(!body.contains("cch=00000"), "cch populated: {body}");
            }
        }
    }

    #[tokio::test]
    async fn stream_message_prepends_user_context_as_synthetic_user_message() {
        let server = MockServer::start().await;
        let body_sink: Captured<String> = captured();
        let sink_clone = std::sync::Arc::clone(&body_sink);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(move |req: &Request| {
                *sink_clone.lock().unwrap() = Some(String::from_utf8_lossy(&req.body).into_owned());
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body())
            })
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-sonnet-4-6"),
            Some("sid".to_owned()),
        )
        .unwrap();
        collect_events(
            client
                .stream_message(
                    &[Message::user("user-question")],
                    &[],
                    Some("<system-reminder>CLAUDE.md content here</system-reminder>"),
                    &[],
                )
                .unwrap(),
        )
        .await
        .unwrap();

        let body = body_sink.lock().unwrap().clone().expect("body captured");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let messages = v["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2, "user_context prepends: {body}");
        assert_eq!(messages[0]["role"], "user");
        assert!(
            messages[0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("system-reminder"),
        );
        assert_eq!(messages[1]["content"][0]["text"], "user-question");
    }

    #[tokio::test]
    async fn stream_message_third_party_base_url_drops_global_scope_and_its_beta() {
        // Mock server URIs are third-party by definition. Pin both
        // halves of the 3P request shape: the static-prefix
        // cache_control must be `{"type":"ephemeral"}` only, and the
        // `prompt-caching-scope` beta must be absent. Regressing
        // either half is exactly how PR #22's gateway 400 fired.
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
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body())
            })
            .mount(&server)
            .await;

        let client = Client::new(
            test_config(server.uri(), api_key(), "claude-sonnet-4-6"),
            Some("sid".to_owned()),
        )
        .unwrap();
        collect_events(
            client
                .stream_message(&[Message::user("hi")], &["static-a", "static-b"], None, &[])
                .unwrap(),
        )
        .await
        .unwrap();

        let (body, beta) = sink.lock().unwrap().clone().expect("request captured");
        assert!(
            !beta.contains("prompt-caching-scope-2026-01-05"),
            "prompt-caching-scope beta absent on 3P: {beta}",
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let system = v["system"].as_array().expect("system array");
        // [0] identity prefix (no cache_control), [1] static joined.
        assert_eq!(system[0]["text"], SYSTEM_PROMPT_PREFIX);
        assert!(
            system[0].get("cache_control").is_none(),
            "identity prefix carries no cache_control: {body}",
        );
        let cc = &system[1]["cache_control"];
        assert_eq!(cc["type"], "ephemeral");
        assert!(
            cc.get("scope").is_none(),
            "scope field omitted entirely on 3P (not null): {body}",
        );
        // TTL rides through on 3P — only `scope` is gated on 1P.
        assert_eq!(cc["ttl"], "1h", "default 1h ttl survives on 3P: {body}");
    }

    // ── Client::stream_message / agentic body fields ──

    /// Captures the serialized body of a single streaming request.
    /// Most agentic-body tests only care about what oxide-code sends,
    /// not the response — this collapses the ceremony to two lines
    /// per test.
    async fn capture_stream_body(config: Config) -> serde_json::Value {
        let server = MockServer::start().await;
        let sink: Captured<String> = captured();
        let sink_clone = std::sync::Arc::clone(&sink);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(move |req: &Request| {
                *sink_clone.lock().unwrap() = Some(String::from_utf8_lossy(&req.body).into_owned());
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body())
            })
            .mount(&server)
            .await;

        let mut cfg = config;
        cfg.base_url = server.uri();
        let client = Client::new(cfg, Some("sid".to_owned())).unwrap();
        collect_events(
            client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap(),
        )
        .await
        .unwrap();
        let body = sink.lock().unwrap().clone().expect("request captured");
        serde_json::from_str(&body).unwrap()
    }

    #[tokio::test]
    async fn stream_message_opus_4_7_emits_output_config_effort_xhigh() {
        let mut cfg = test_config("https://placeholder.invalid", api_key(), "claude-opus-4-7");
        cfg.effort = Some(Effort::Xhigh);
        let body = capture_stream_body(cfg).await;
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    #[tokio::test]
    async fn stream_message_omits_output_config_when_effort_is_none() {
        // Non-effort-capable model → `Config.effort == None` → the
        // whole `output_config` block is absent (not `{}`).
        let cfg = test_config(
            "https://placeholder.invalid",
            api_key(),
            "claude-sonnet-4-5",
        );
        assert!(cfg.effort.is_none(), "precondition: effort unset");
        let body = capture_stream_body(cfg).await;
        assert!(
            body.get("output_config").is_none(),
            "output_config absent: {body}",
        );
    }

    #[tokio::test]
    async fn stream_message_context_management_body_present_on_4_6_plus() {
        // Every model whose `context_management` capability flag is
        // set must also ship the body directive alongside the beta
        // header.
        for model in [
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
        ] {
            let cfg = test_config("https://placeholder.invalid", api_key(), model);
            let body = capture_stream_body(cfg).await;
            let edits = body["context_management"]["edits"]
                .as_array()
                .unwrap_or_else(|| panic!("context_management.edits missing for {model}: {body}"));
            assert_eq!(edits.len(), 1, "{model}");
            assert_eq!(edits[0]["type"], "clear_thinking_20251015", "{model}");
            assert_eq!(edits[0]["keep"], "all", "{model}");
        }
    }

    #[tokio::test]
    async fn stream_message_context_management_absent_on_unknown_model() {
        // Unknown model ids (no `MODELS` row matches) fall back to
        // the all-false `Capabilities::default()` — no beta, no body
        // directive. Keeps "beta sent ⇒ body populated" an invariant.
        let cfg = test_config("https://placeholder.invalid", api_key(), "claude-opus-5-0");
        let body = capture_stream_body(cfg).await;
        assert!(
            body.get("context_management").is_none(),
            "context_management absent on unknown models: {body}",
        );
    }

    #[tokio::test]
    async fn stream_message_show_thinking_emits_display_summarized() {
        let mut cfg = test_config("https://placeholder.invalid", api_key(), "claude-opus-4-7");
        cfg.thinking = Some(ThinkingConfig::Adaptive {
            display: Some(crate::config::ThinkingDisplay::Summarized),
        });
        let body = capture_stream_body(cfg).await;
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
    }

    #[tokio::test]
    async fn stream_message_show_thinking_false_omits_display_field() {
        // `Adaptive { display: None }` must serialize without a
        // `display` key — `skip_serializing_if` on the wire.
        let mut cfg = test_config("https://placeholder.invalid", api_key(), "claude-opus-4-7");
        cfg.thinking = Some(ThinkingConfig::Adaptive { display: None });
        let body = capture_stream_body(cfg).await;
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(
            body["thinking"].get("display").is_none(),
            "display field absent: {body}",
        );
    }

    // ── build_system_blocks ──

    #[test]
    fn build_system_blocks_orders_billing_then_identity_then_extras() {
        let billing = "x-anthropic-billing-header: cc_version=2.1.119; cch=00000;";
        let cache = CacheControl {
            r#type: "ephemeral",
            scope: Some("global"),
            ttl: Some("1h"),
        };
        let blocks = build_system_blocks(
            Some(billing),
            [("static body", Some(cache)), ("dynamic body", None)],
        );

        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].text, billing);
        assert!(blocks[0].cache_control.is_none(), "billing has no cc");
        assert_eq!(blocks[1].text, SYSTEM_PROMPT_PREFIX);
        assert!(blocks[1].cache_control.is_none(), "identity has no cc");
        assert_eq!(blocks[2].text, "static body");
        assert_eq!(
            blocks[2].cache_control.as_ref().and_then(|c| c.scope),
            Some("global"),
        );
        assert_eq!(blocks[3].text, "dynamic body");
        assert!(blocks[3].cache_control.is_none(), "dynamic has no cc");
    }

    #[test]
    fn build_system_blocks_drops_empty_extras_and_omits_billing_when_absent() {
        let blocks = build_system_blocks(None, [("", None), ("only-content", None)]);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, SYSTEM_PROMPT_PREFIX);
        assert_eq!(blocks[1].text, "only-content");
    }

    // ── build_metadata ──

    #[test]
    fn build_metadata_wraps_session_id_in_stringified_json() {
        // `metadata.user_id` is a stringified JSON object on the wire
        // (not a nested object) — round-trip check keeps the
        // double-encoding explicit.
        let meta = build_metadata("abc-123");
        let parsed: serde_json::Value = serde_json::from_str(&meta.user_id).unwrap();
        assert_eq!(parsed["session_id"], "abc-123");
    }

    // ── normalize_platform ──

    #[test]
    fn normalize_platform_maps_known_and_falls_back_to_unknown() {
        for (input, expected) in [
            ("macos", "MacOS"),
            ("linux", "Linux"),
            ("windows", "Windows"),
            ("freebsd", "FreeBSD"),
            ("openbsd", "OpenBSD"),
            ("ios", "iOS"),
            ("android", "Android"),
            ("haiku", "Unknown"),
        ] {
            assert_eq!(normalize_platform(input), expected, "input={input}");
        }
    }

    // ── normalize_arch ──

    #[test]
    fn normalize_arch_maps_known_and_falls_back_to_unknown() {
        for (input, expected) in [
            ("x86", "x32"),
            ("x86_64", "x64"),
            ("arm", "arm"),
            ("aarch64", "arm64"),
            ("riscv64gc", "unknown"),
        ] {
            assert_eq!(normalize_arch(input), expected, "input={input}");
        }
    }

    // ── split_at_boundary ──

    #[test]
    fn split_at_boundary_separates_static_and_dynamic() {
        let sections = &["intro", "tasks", SYSTEM_PROMPT_DYNAMIC_BOUNDARY, "env"];
        let (statics, dynamic) = split_at_boundary(sections);
        assert_eq!(statics, vec!["intro", "tasks"]);
        assert_eq!(dynamic, vec!["env"]);
    }

    #[test]
    fn split_at_boundary_without_marker_treats_all_as_static() {
        let (statics, dynamic) = split_at_boundary(&["intro", "tasks", "env"]);
        assert_eq!(statics, vec!["intro", "tasks", "env"]);
        assert!(dynamic.is_empty());
    }

    #[test]
    fn split_at_boundary_filters_empty_sections() {
        let sections = &["intro", "", SYSTEM_PROMPT_DYNAMIC_BOUNDARY, "", "env"];
        let (statics, dynamic) = split_at_boundary(sections);
        assert_eq!(statics, vec!["intro"]);
        assert_eq!(dynamic, vec!["env"]);
    }

    #[test]
    fn split_at_boundary_at_extremes_yields_empty_side() {
        let (statics, dynamic) =
            split_at_boundary(&[SYSTEM_PROMPT_DYNAMIC_BOUNDARY, "env", "lang"]);
        assert!(statics.is_empty());
        assert_eq!(dynamic, vec!["env", "lang"]);

        let (statics, dynamic) =
            split_at_boundary(&["intro", "tasks", SYSTEM_PROMPT_DYNAMIC_BOUNDARY]);
        assert_eq!(statics, vec!["intro", "tasks"]);
        assert!(dynamic.is_empty());
    }

    // ── first_user_text ──

    #[test]
    fn first_user_text_extracts_from_first_user_message() {
        let messages = vec![Message::user("hello world"), Message::assistant("hi")];
        assert_eq!(first_user_text(&messages), "hello world");
    }

    #[test]
    fn first_user_text_returns_empty_when_absent() {
        assert_eq!(first_user_text(&[]), "");
        assert_eq!(first_user_text(&[Message::assistant("hi")]), "");
        let tool_only = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "id".to_owned(),
                content: "result".to_owned(),
                is_error: false,
            }],
        }];
        assert_eq!(first_user_text(&tool_only), "");
    }
}
