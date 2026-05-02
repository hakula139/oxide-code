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
mod identity;
mod sse;
pub(crate) mod wire;

#[cfg(test)]
pub(crate) mod testing;

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use tokio::sync::mpsc;
use tracing::debug;
use uuid::Uuid;

use crate::config::{Auth, Config, Effort};
use crate::message::{ContentBlock, Message, Role};
use crate::prompt::SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
use crate::prompt::environment::marketing_name;
use crate::tool::ToolDefinition;

use betas::{compute_betas, static_prefix_cache_control};
use sse::stream_sse;
use wire::{
    CacheControl, ContextManagement, CreateMessageRequest, OutputConfig, RequestMetadata,
    StreamEvent, SystemBlock,
};

const API_VERSION: &str = "2023-06-01";

/// Pinned to the latest packaged claude-code release; gateways
/// reject pre-allowlist versions.
const CLAUDE_CLI_VERSION: &str = "2.1.121";
/// `@anthropic-ai/sdk` version shipped by the pinned claude-code release.
const STAINLESS_PACKAGE_VERSION: &str = "0.81.0";
/// Node.js version the Stainless SDK reports as `process.version`.
const STAINLESS_RUNTIME_VERSION: &str = "v24.3.0";
/// Stainless per-request timeout matching the SDK default.
const STAINLESS_TIMEOUT_SECS: &str = "600";

/// OAuth-required identity prefix. The Anthropic API returns 429 for non-Haiku
/// models with OAuth tokens unless the system prompt starts with this exact
/// string in its own text block.
const SYSTEM_PROMPT_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Resolved values produced by [`Client::set_model`] — feeds
/// [`AgentEvent::ModelSwitched`](crate::agent::event::AgentEvent::ModelSwitched)
/// without a second [`crate::model`] lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelSwap {
    pub(crate) model_id: String,
    pub(crate) marketing: String,
    pub(crate) effort: Option<Effort>,
}

#[derive(Clone)]
pub(crate) struct Client {
    http: reqwest::Client,
    config: Config,
    session_id: String,
    /// Per-machine id sent in `metadata.user_id.device_id`, lazily
    /// minted under `$XDG_DATA_HOME/ox/user-id` at construction.
    device_id: String,
    /// Whether `config.base_url` points at the first-party Anthropic
    /// API. Cached at construction so per-request paths don't re-parse
    /// the URL — gates `cache_control.scope: "global"`, which 3P
    /// gateways reject downstream of tool definitions.
    is_first_party: bool,
}

impl Client {
    pub(crate) fn new(config: Config, session_id: Option<String>) -> Result<Self> {
        let session_id = session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let device_id = identity::load_or_create_device_id();
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

        // 3P gateways fingerprint absence of the full Stainless header set.
        // `x-claude-code-session-id` is per-request so `/clear` can roll
        // the id without rebuilding the HTTP client.
        headers.insert("x-app", HeaderValue::from_static("cli"));
        headers.insert("x-stainless-lang", HeaderValue::from_static("js"));
        headers.insert(
            "x-stainless-package-version",
            HeaderValue::from_static(STAINLESS_PACKAGE_VERSION),
        );
        headers.insert("x-stainless-runtime", HeaderValue::from_static("node"));
        headers.insert(
            "x-stainless-runtime-version",
            HeaderValue::from_static(STAINLESS_RUNTIME_VERSION),
        );
        headers.insert(
            "x-stainless-os",
            HeaderValue::from_static(normalize_platform(std::env::consts::OS)),
        );
        headers.insert(
            "x-stainless-arch",
            HeaderValue::from_static(normalize_arch(std::env::consts::ARCH)),
        );
        headers.insert(
            "x-stainless-timeout",
            HeaderValue::from_static(STAINLESS_TIMEOUT_SECS),
        );
        headers.insert("x-stainless-retry-count", HeaderValue::from_static("0"));

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
            device_id,
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

    /// Replaces the id used for `x-claude-code-session-id` and
    /// `metadata.user_id`. Debug-asserts header-value validity;
    /// callers feed UUID v4s from `SessionHandle`, so failure is a
    /// programmer error.
    pub(crate) fn set_session_id(&mut self, id: String) {
        debug_assert!(
            HeaderValue::from_str(&id).is_ok(),
            "session id must be a legal HTTP header value: {id:?}",
        );
        self.session_id = id;
    }

    /// Swaps the active model and re-clamps `config.effort` against
    /// the new caps. Returns the resolved [`ModelSwap`] so the agent
    /// loop's `/model` arm builds
    /// [`AgentEvent::ModelSwitched`](crate::agent::event::AgentEvent::ModelSwitched)
    /// without re-deriving. Per-request paths re-read `&self.config.model`
    /// every call, so betas / `output_config` / `context_management`
    /// pick up the swap on the next stream.
    ///
    /// Effort is lossy across swaps: an `xhigh`-clamped-to-`high`
    /// model doesn't restore `xhigh` on swap-back. Tracking the raw
    /// pick separately is deferred (see `model.md`).
    pub(crate) fn set_model(&mut self, model: String) -> ModelSwap {
        let caps = crate::model::capabilities_for(&model);
        let effort = match self.config.effort {
            Some(pick) => caps.clamp_effort(pick),
            None => caps.default_effort(),
        };
        self.config.effort = effort;
        let marketing = marketing_name(&model).map_or_else(|| model.clone(), ToOwned::to_owned);
        self.config.model.clone_from(&model);
        ModelSwap {
            model_id: model,
            marketing,
            effort,
        }
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

        // 3P gateways reject API-key traffic without the cch
        // attestation, so the billing block ships under both auth modes.
        let billing_header = {
            let fingerprint = billing::compute_fingerprint(
                first_user_text(&effective_messages),
                CLAUDE_CLI_VERSION,
            );
            billing::build_billing_header(CLAUDE_CLI_VERSION, &fingerprint)
        };

        let (static_sections, dynamic_sections) = split_at_boundary(system_sections);
        let static_joined = static_sections.join("\n\n");
        let dynamic_joined = dynamic_sections.join("\n\n");

        let static_cache_control =
            static_prefix_cache_control(self.is_first_party, self.config.prompt_cache_ttl);
        let system_blocks = build_system_blocks(
            Some(&billing_header),
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
            metadata: build_metadata(&self.device_id, &self.session_id),
            system: system_blocks,
            tools: (!tools.is_empty()).then_some(tools),
            thinking: self.config.thinking.as_ref(),
            output_config: OutputConfig::new(None, self.config.effort),
            // Body and header stay in sync — claude-code 2.1.119 ships
            // both on every 4.6+ agentic request.
            context_management: caps
                .context_management
                .then(ContextManagement::clear_thinking_keep_all),
            messages: &effective_messages,
        })
        .context("failed to serialize request")?;

        body = billing::inject_cch(&body)?;

        debug!(body_len = body.len(), "sending API request");

        let (tx, rx) = mpsc::channel(64);
        let http = self.http.clone();
        let betas = compute_betas(&self.config.model, &self.config.auth, true, false).join(",");
        let session_id = self.session_id.clone();

        tokio::spawn(async move {
            let result = stream_sse(&http, &url, betas, session_id, body, &tx).await;
            if let Err(e) = result {
                _ = tx.send(Err(e)).await;
            }
        });

        Ok(rx)
    }
}

/// Builds the `metadata.user_id` field as a stringified JSON object.
///
/// Field order matters: gateways reject anything but `device_id,
/// account_uuid, session_id`. A typed struct (not `serde_json::json!`)
/// preserves declaration order on the wire — `json!` would alphabetize.
fn build_metadata(device_id: &str, session_id: &str) -> RequestMetadata {
    #[derive(serde::Serialize)]
    struct UserId<'a> {
        device_id: &'a str,
        account_uuid: &'a str,
        session_id: &'a str,
    }

    let user_id = serde_json::to_string(&UserId {
        device_id,
        account_uuid: "",
        session_id,
    })
    .expect("UserId fields are owned `str`s with no serialization failure modes");
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

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, header_regex, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::billing::build_billing_header;
    use super::testing::{Captured, api_key, captured, oauth, test_config};
    use super::wire::{ContentBlockInfo, Delta};
    use super::*;
    use crate::config::{Effort, ThinkingConfig};

    // ── Fixtures ──

    const OFFLINE_URL: &str = "https://example.invalid";
    const TEST_MODEL: &str = "claude-sonnet-4-6";

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

    // ── Client::set_session_id ──

    #[tokio::test]
    async fn set_session_id_propagates_to_header_and_metadata_user_id() {
        // Pins both wire surfaces — the mock matches the rolled id in
        // the header (wrong value 404s) and the assertion below pins
        // the embedded JSON in `metadata.user_id`.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-claude-code-session-id", "sid-rolled"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body()),
            )
            .mount(&server)
            .await;

        let mut client = Client::new(
            test_config(server.uri(), Auth::ApiKey("k".to_owned()), TEST_MODEL),
            Some("sid-original".to_owned()),
        )
        .unwrap();
        client.set_session_id("sid-rolled".to_owned());
        collect_events(
            client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap(),
        )
        .await
        .unwrap();

        let received = server.received_requests().await.expect("recorded requests");
        assert_eq!(received.len(), 1, "exactly one streamed call");
        let body: serde_json::Value =
            serde_json::from_slice(&received[0].body).expect("request body is JSON");
        let user_id = body["metadata"]["user_id"]
            .as_str()
            .expect("metadata.user_id is a string");
        assert!(
            user_id.contains("sid-rolled"),
            "metadata.user_id carries the new session id: {user_id}",
        );
        assert!(
            !user_id.contains("sid-original"),
            "old session id must not leak into the body: {user_id}",
        );
    }

    // ── Client::set_model ──

    fn client_with(model: &str, effort: Option<Effort>) -> Client {
        let mut cfg = test_config(OFFLINE_URL, api_key(), model);
        cfg.effort = effort;
        Client::new(cfg, Some("sid".to_owned())).unwrap()
    }

    #[test]
    fn set_model_swaps_model_and_returns_marketing_and_effort() {
        // Pin the round-trip: ModelSwap mirrors the resolved values
        // and `config` reflects the swap for the next request.
        let mut client = client_with("claude-opus-4-7", Some(Effort::Xhigh));
        let swap = client.set_model("claude-sonnet-4-6".to_owned());

        assert_eq!(swap.model_id, "claude-sonnet-4-6");
        assert_eq!(swap.marketing, "Claude Sonnet 4.6");
        // Sonnet 4.6 caps at high; xhigh clamps down silently.
        assert_eq!(swap.effort, Some(Effort::High));
        assert_eq!(client.model(), "claude-sonnet-4-6");
        assert_eq!(client.config.effort, Some(Effort::High));
    }

    #[test]
    fn set_model_re_clamps_effort_when_new_model_does_not_accept_current_level() {
        // Sonnet 4.6 caps below `max`; clamp down rather than letting
        // the next request 400.
        let mut client = client_with("claude-opus-4-6", Some(Effort::Max));
        let swap = client.set_model("claude-sonnet-4-6".to_owned());
        assert_eq!(swap.effort, Some(Effort::High));
        assert_eq!(client.config.effort, Some(Effort::High));
    }

    #[test]
    fn set_model_drops_effort_when_new_model_does_not_accept_it() {
        // Haiku 4.5 doesn't accept `output_config.effort` — clamp to
        // None so the body omits the field entirely.
        let mut client = client_with("claude-opus-4-7", Some(Effort::Xhigh));
        let swap = client.set_model("claude-haiku-4-5".to_owned());
        assert_eq!(swap.effort, None);
        assert_eq!(client.config.effort, None);
    }

    #[test]
    fn set_model_picks_default_effort_when_previous_model_did_not_accept_effort() {
        // Haiku → Opus 4.7 must pick the new model's default rather
        // than leave effort=None and ship without the body field.
        let mut client = client_with("claude-haiku-4-5", None);
        let swap = client.set_model("claude-opus-4-7".to_owned());
        assert_eq!(swap.effort, Some(Effort::Xhigh));
        assert_eq!(client.config.effort, Some(Effort::Xhigh));
    }

    #[test]
    fn set_model_unknown_id_falls_back_to_raw_id_for_marketing() {
        // Unknown model → marketing mirrors the raw id; caps default
        // to all-false so effort drops to None.
        let mut client = client_with("claude-opus-4-7", Some(Effort::High));
        let swap = client.set_model("claude-opus-5-0".to_owned());
        assert_eq!(swap.model_id, "claude-opus-5-0");
        assert_eq!(swap.marketing, "claude-opus-5-0");
        assert_eq!(swap.effort, None);
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
    async fn stream_message_429_threads_retry_after_header_into_error() {
        // The streaming path's retry-after extraction lives in stream_sse,
        // not format_api_error — pin that the header is read off the
        // response *before* the body is consumed.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "60")
                    .set_body_string(r#"{"error":{"type":"rate_limit_error"}}"#),
            )
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
        let err = collect_events(rx).await.expect_err("expected 429");
        assert!(
            format!("{err:#}").contains("retry after 60"),
            "retry-after threaded through stream_sse: {err:#}",
        );
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
    async fn stream_message_billing_block_ships_under_both_auth_modes_with_cch_populated() {
        // 3P gateways reject API-key requests without the cch
        // attestation, so the billing block must ship under both
        // auth modes with the placeholder replaced by xxHash64.
        for auth in [api_key(), oauth()] {
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
            assert!(
                body.contains("x-anthropic-billing-header:"),
                "billing block must ship under both auth modes: {body}",
            );
            assert!(!body.contains("cch=00000"), "cch populated: {body}");
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
    async fn stream_message_third_party_base_url_drops_global_scope_keeps_its_beta() {
        // On 3P, the `prompt-caching-scope` beta still ships (gateway
        // fingerprints absence) but the body-side `scope: "global"`
        // is dropped (gateway rejects the scope downstream of tools).
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
            beta.contains("prompt-caching-scope-2026-01-05"),
            "prompt-caching-scope beta ships on 3P for fingerprint parity: {beta}",
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let system = v["system"].as_array().expect("system array");
        // [0] billing header, [1] identity prefix, [2] static joined.
        assert!(
            system[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("x-anthropic-billing-header:"),
            "billing header occupies system[0]: {body}",
        );
        assert_eq!(system[1]["text"], SYSTEM_PROMPT_PREFIX);
        let cc = &system[2]["cache_control"];
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

    // ── build_metadata ──

    #[test]
    fn build_metadata_wraps_ids_in_stringified_json_with_canonical_field_order() {
        // Field order must be `device_id, account_uuid, session_id` —
        // `serde_json::json!` would alphabetize and trip 3P validation.
        let meta = build_metadata("dev-1", "abc-123");
        assert_eq!(
            meta.user_id,
            r#"{"device_id":"dev-1","account_uuid":"","session_id":"abc-123"}"#,
        );
    }

    // ── build_system_blocks ──

    #[test]
    fn build_system_blocks_orders_billing_then_identity_then_extras() {
        let billing = build_billing_header(CLAUDE_CLI_VERSION, "abc");
        let cache = CacheControl {
            r#type: "ephemeral",
            scope: Some("global"),
            ttl: Some("1h"),
        };
        let blocks = build_system_blocks(
            Some(&billing),
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
