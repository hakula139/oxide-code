//! Anthropic Messages API client, which implements the [`Wire`] the agent loop streams through.
//! [`Client::complete`] handles one-shots such as title generation.
//!
//! The wire shape mirrors the official `claude-code` CLI: pinned User-Agent and Stainless SDK
//! header set, billing attestation in `system[0]`, and per-request beta headers gated on model /
//! auth-mode capabilities. Some relay gateways admit only requests that carry the billing
//! attestation or `metadata.user_id`.

mod accumulator;
mod betas;
mod billing;
mod completion;
pub(crate) mod identity;
mod sse;
mod transport;
pub(crate) mod wire;

#[cfg(test)]
pub(crate) mod testing;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use tokio::sync::mpsc;
use tracing::debug;
use uuid::Uuid;

use crate::client::{ResponseEvent, Wire};
use crate::config::auth::{Auth, AuthSource};
use crate::config::gateway::GatewayCompat;
use crate::config::{Config, Effort, PromptCacheTtl, is_first_party_base_url};
use crate::message::{ContentBlock, Message, Role};
use crate::model::ModelSpec;
use crate::prompt::SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
use crate::tool::ToolDefinition;
use crate::util::tls::apply_extra_ca_certs;

use betas::{compute_betas, conversation_cache_control, static_prefix_cache_control};
use sse::forward_sse;
use wire::{
    CacheControl, ContextManagement, CreateMessageRequest, OutputConfig, RequestMessages,
    RequestMetadata, SystemBlock,
};

// ── Constants ──

const API_VERSION: &str = "2023-06-01";

/// Claude Code release the user agent, billing header, and agentic beta sequence all mirror.
/// Some relay gateways enforce a `cc_version` floor. Bump it only with a fresh beta capture.
const CLAUDE_CLI_VERSION: &str = "2.1.296";
const STAINLESS_PACKAGE_VERSION: &str = "0.81.0";
const STAINLESS_RUNTIME_VERSION: &str = "v24.3.0";
const STAINLESS_TIMEOUT_SECS: &str = "600";

/// Required as its own text block because non-Haiku OAuth requests 429 without it.
const SYSTEM_PROMPT_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

// ── Client ──

/// Shareable HTTP client for the Anthropic Messages API, holding the endpoint and what each
/// request sends. `Clone` is cheap because `reqwest::Client` shares its connection pool. Callers
/// keep one per session and swap `session_id` / `model` / `effort` / `auth` via `set_*`.
#[derive(Clone)]
pub(crate) struct Client {
    http: reqwest::Client,
    model: ModelSpec,
    /// `None` for models outside the catalog.
    effort: Option<Effort>,
    /// `None` while signed out.
    auth: Option<Auth>,
    base_url: String,
    extra_ca_certs: Option<PathBuf>,
    gateway: GatewayCompat,
    /// User-set output cap. `None` defers to [`ModelSpec::resolve_max_tokens`].
    max_tokens: Option<u32>,
    prompt_cache_ttl: PromptCacheTtl,
    /// Whether adaptive thinking opts into summarized text.
    show_thinking: bool,
    session_id: String,
    device_id: String,
    /// Whether the client talks to the Anthropic API, which alone gets `scope: "global"` and
    /// `cch`, as in Claude Code.
    is_first_party: bool,
}

impl Client {
    /// Builds a client for the endpoint, credential, and request settings in `config`.
    pub(crate) fn new(
        config: &Config,
        session_id: Option<String>,
        device_id: String,
    ) -> Result<Self> {
        let Config {
            model,
            effort,
            auth,
            base_url,
            extra_ca_certs,
            gateway,
            max_tokens,
            prompt_cache_ttl,
            show_thinking,
            ..
        } = config;
        Ok(Self {
            http: build_http(gateway, extra_ca_certs.as_deref())?,
            model: model.clone(),
            effort: *effort,
            auth: auth.clone(),
            base_url: base_url.clone(),
            extra_ca_certs: extra_ca_certs.clone(),
            gateway: gateway.clone(),
            max_tokens: *max_tokens,
            prompt_cache_ttl: *prompt_cache_ttl,
            show_thinking: *show_thinking,
            session_id: session_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            device_id,
            is_first_party: is_first_party_base_url(base_url),
        })
    }

    pub(crate) fn model(&self) -> &ModelSpec {
        &self.model
    }

    pub(crate) fn effort(&self) -> Option<Effort> {
        self.effort
    }

    /// The output cap that requests to the active model carry.
    pub(crate) fn max_tokens(&self) -> u32 {
        self.model.resolve_max_tokens(self.max_tokens)
    }

    pub(crate) fn prompt_cache_ttl(&self) -> PromptCacheTtl {
        self.prompt_cache_ttl
    }

    /// Where the credential came from, `None` while signed out.
    pub(crate) fn auth_source(&self) -> Option<AuthSource> {
        self.auth.as_ref().map(Auth::source)
    }

    /// PEM bundle appended to the platform trust roots, which stored-login refreshes also trust.
    pub(crate) fn extra_ca_certs(&self) -> Option<&Path> {
        self.extra_ca_certs.as_deref()
    }

    /// The credential used by requests, or an error while signed out.
    pub(crate) fn auth(&self) -> Result<&Auth> {
        self.auth.as_ref().context("not signed in")
    }

    #[cfg(test)]
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Replaces the id used for `x-claude-code-session-id` and `metadata.user_id`.
    pub(crate) fn set_session_id(&mut self, id: String) {
        debug_assert!(
            HeaderValue::from_str(&id).is_ok(),
            "session id must be a legal HTTP header value: {id:?}",
        );
        self.session_id = id;
    }

    /// Swaps the active model and re-resolves effort. Thinking, betas, context management, and
    /// the default `max_tokens` derive from the model per request.
    pub(crate) fn set_model(&mut self, id: String) {
        let model = ModelSpec::resolve(id);
        self.effort = model.resolve_effort(self.effort);
        self.model = model;
    }

    /// Swaps the active effort. Stays `None` on unknown models.
    pub(crate) fn set_effort(&mut self, pick: Effort) -> Option<Effort> {
        self.effort = self.model.resolve_effort(Some(pick));
        self.effort
    }

    /// Swaps the credential for later requests, `None` to sign out. Callers install a stored login
    /// only in place of another stored login or none, which `Config::load` allows only for a
    /// first-party base URL.
    pub(crate) fn set_auth(&mut self, auth: Option<Auth>) {
        self.auth = auth;
    }

    /// Streams one request. The receiver closes when the stream ends, the receiver is dropped, or
    /// an unrecoverable error is forwarded as a final `Err`.
    ///
    /// The static `system` prefix up to [`SYSTEM_PROMPT_DYNAMIC_BOUNDARY`] always carries a
    /// `cache_control` breakpoint, shared across sessions. `breakpoint` marks the last content
    /// block of the final message. `user_context` rides as a synthetic leading user message to
    /// keep dynamic content (CLAUDE.md and friends) out of the `system` parameter.
    fn stream(
        &self,
        messages: &[Message],
        system_sections: &[&str],
        user_context: Option<&str>,
        tools: &[ToolDefinition],
        breakpoint: Option<CacheControl>,
    ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
        let auth = self.auth()?;

        // The fingerprint skips the user context, which Claude Code marks as a meta message.
        let attribution = self
            .gateway
            .attribution
            .then(|| attribution_text(first_user_text(messages), self.is_first_party));

        let (static_sections, dynamic_sections) = split_at_boundary(system_sections);
        let static_joined = static_sections.join("\n\n");
        let dynamic_joined = dynamic_sections.join("\n\n");

        let static_cache_control =
            static_prefix_cache_control(self.is_first_party, self.prompt_cache_ttl);
        let system_blocks = build_system_blocks(
            attribution.as_deref(),
            &[
                (static_joined.as_str(), Some(static_cache_control)),
                (dynamic_joined.as_str(), None),
            ],
        );

        let model = &self.model;
        let thinking = model.thinking(self.show_thinking);
        let mut body = serde_json::to_string(&CreateMessageRequest {
            model: model.id(),
            max_tokens: self.max_tokens(),
            stream: true,
            metadata: build_metadata(&self.device_id, &self.session_id),
            system: system_blocks,
            tools: (!tools.is_empty()).then_some(tools),
            thinking: thinking.as_ref(),
            output_config: OutputConfig::new(None, self.effort),
            // Body must accompany the beta header, which `compute_betas` gates the same way.
            context_management: model
                .info()
                .map(|_| ContextManagement::clear_thinking_keep_all()),
            messages: RequestMessages {
                user_context,
                messages,
                breakpoint,
            },
        })
        .context("failed to serialize request")?;

        if attribution.as_deref().is_some_and(billing::is_hashed) {
            body = billing::inject_cch(&body)?;
        }

        debug!(body_len = body.len(), "sending API request");

        let (tx, rx) = mpsc::channel(64);
        let http = self.http.clone();
        let betas = compute_betas(model, thinking.as_ref(), auth.kind(), true).join(",");
        let request = self.messages_request(betas, body, auth)?;
        tokio::spawn(forward_sse(http, request, model.id().to_owned(), tx));

        Ok(rx)
    }
}

impl Wire for Client {
    /// Marks the transcript tail with a `cache_control` breakpoint, so the next request in the
    /// conversation reads the whole transcript from cache.
    fn stream_message(
        &self,
        messages: &[Message],
        system_sections: &[&str],
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
        let breakpoint = conversation_cache_control(self.prompt_cache_ttl);
        self.stream(
            messages,
            system_sections,
            user_context,
            tools,
            Some(breakpoint),
        )
    }

    fn stream_one_shot(
        &self,
        messages: &[Message],
        system_sections: &[&str],
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
        self.stream(messages, system_sections, user_context, tools, None)
    }
}

// ── HTTP Client ──

/// The HTTP client with every header requests share. Each request adds its own credential.
fn build_http(gateway: &GatewayCompat, extra_ca_certs: Option<&Path>) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
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
    headers.insert("x-app", HeaderValue::from_static("cli"));

    // Claude Code's Anthropic SDK sends these on every request.
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

    headers.extend(gateway.custom_headers.clone());

    // No whole-request timeout because responses can run for minutes. The 60 s read timeout
    // catches slowloris dribble. Anthropic sends keepalives every ~15 s on healthy streams.
    // reqwest strips only `Authorization` and `Cookie` on a cross-host redirect, so following
    // one would carry `x-api-key` and the custom headers to another host.
    let builder = reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_mins(1));
    let builder = apply_extra_ca_certs(builder, extra_ca_certs)?;
    builder.build().context("failed to build HTTP client")
}

// ── Request Building ──

/// The `system[0]` attribution text. First-party requests carry a `cch` placeholder that
/// [`billing::inject_cch`] fills once the body is serialized. Like Claude Code since 2.1.181,
/// other base URLs get a stable block without it, so gateways that cache on the body still hit.
fn attribution_text(first_user_text: &str, is_first_party: bool) -> String {
    let fingerprint = billing::compute_fingerprint(first_user_text, CLAUDE_CLI_VERSION);
    billing::build_billing_header(CLAUDE_CLI_VERSION, &fingerprint, is_first_party)
}

/// Some relay gateways admit a request without the billing block only when `user_id` holds a
/// JSON object with `session_id`.
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

/// The billing block leads, as in Claude Code, so [`billing::inject_cch`] finds its placeholder
/// first. Some relay gateways reject the block in any other slot.
fn build_system_blocks<'a>(
    attribution: Option<&'a str>,
    extras: &[(&'a str, Option<CacheControl>)],
) -> Vec<SystemBlock<'a>> {
    let mut blocks = Vec::with_capacity(2 + extras.len());
    if let Some(text) = attribution {
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
    for &(text, cache_control) in extras {
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

// ── Platform Normalization ──

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

// ── System Prompt Helpers ──

/// Splits at the boundary marker, dropping the marker itself.
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
        let all = sections.iter().filter(|s| !s.is_empty()).copied().collect();
        (all, Vec::new())
    }
}

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
    use std::fmt::Write as _;

    use indoc::writedoc;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::testing::{
        Captured, DEVICE_ID, api_key, captured, client_for, oauth, test_client, test_config,
        text_stream_body,
    };
    use super::*;
    use crate::client::{Response, StopReason, TokenUsage};
    use crate::config::auth::{Auth, AuthSource};
    use crate::config::{Effort, PromptCacheTtl};

    // ── Fixtures ──

    const OFFLINE_URL: &str = "https://example.invalid";
    const TEST_MODEL: &str = "claude-sonnet-5-5";

    fn sse_body(frames: &[(&str, &str)]) -> String {
        let mut body = String::new();
        for (event, data) in frames {
            _ = writedoc!(
                body,
                "
                event: {event}
                data: {data}

                ",
            );
        }
        body
    }

    async fn collect_events(
        mut rx: mpsc::Receiver<Result<ResponseEvent>>,
    ) -> Result<Vec<ResponseEvent>> {
        let mut out = Vec::new();
        while let Some(event) = rx.recv().await {
            out.push(event?);
        }
        Ok(out)
    }

    /// Streams one `hi` turn against a mock server and returns the request it received.
    async fn capture_stream_request(mut client: Client) -> Request {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body("", "Hi")),
            )
            .mount(&server)
            .await;

        client.base_url = server.uri();
        collect_events(
            client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap(),
        )
        .await
        .unwrap();

        let mut received = server.received_requests().await.expect("recorded requests");
        assert_eq!(received.len(), 1, "exactly one streamed call");
        received.remove(0)
    }

    fn request_header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
        request.headers.get(name).map(|v| v.to_str().unwrap())
    }

    // ── Client::new / Client::model ──

    #[test]
    fn new_with_api_key_exposes_model() {
        let client = Client::new(
            &test_config(
                OFFLINE_URL,
                Auth::new(AuthSource::ApiKeyEnv, "sk-test"),
                TEST_MODEL,
            ),
            None,
            DEVICE_ID.to_owned(),
        )
        .unwrap();
        assert_eq!(client.model().id(), TEST_MODEL);
    }

    #[test]
    fn new_with_oauth_token_exposes_model() {
        let client = Client::new(
            &test_config(
                OFFLINE_URL,
                Auth::new(AuthSource::ClaudeCodeLogin, "oauth-token"),
                TEST_MODEL,
            ),
            None,
            DEVICE_ID.to_owned(),
        )
        .unwrap();
        assert_eq!(client.model().id(), TEST_MODEL);
    }

    #[test]
    fn new_exposes_the_prompt_cache_ttl() {
        let mut config = test_config(
            OFFLINE_URL,
            Auth::new(AuthSource::ApiKeyEnv, "sk-test"),
            TEST_MODEL,
        );
        config.prompt_cache_ttl = PromptCacheTtl::FiveMin;
        let client = Client::new(&config, None, DEVICE_ID.to_owned()).unwrap();

        assert_eq!(client.prompt_cache_ttl(), PromptCacheTtl::FiveMin);
    }

    #[test]
    fn new_none_session_id_generates_uuid_v4() {
        let client = Client::new(
            &test_config(
                OFFLINE_URL,
                Auth::new(AuthSource::ApiKeyEnv, "k"),
                TEST_MODEL,
            ),
            None,
            DEVICE_ID.to_owned(),
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
            &test_config(
                OFFLINE_URL,
                Auth::new(AuthSource::ApiKeyEnv, "k"),
                TEST_MODEL,
            ),
            Some(sid.clone()),
            DEVICE_ID.to_owned(),
        )
        .unwrap();
        assert_eq!(client.session_id(), sid);
    }

    #[test]
    fn new_with_extra_ca_certs_accepts_valid_bundle() {
        // This pins Client wiring only. `util::tls` covers the handshake through a bundle.
        let pem = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut pem.as_file(), crate::util::tls::TEST_CA_PEM.as_bytes())
            .unwrap();
        let mut cfg = test_config(OFFLINE_URL, api_key(), TEST_MODEL);
        cfg.extra_ca_certs = Some(pem.path().to_path_buf());
        Client::new(&cfg, None, DEVICE_ID.to_owned()).expect("valid CA bundle must build a client");
    }

    #[test]
    fn new_surfaces_extra_ca_certs_error_with_path() {
        // An unreadable trust anchor must fail the client build and cite the configured path so
        // the user can debug it without inspecting TLS internals.
        let mut cfg = test_config(OFFLINE_URL, api_key(), TEST_MODEL);
        cfg.extra_ca_certs = Some(std::path::PathBuf::from("/no/such/bundle.pem"));
        let err = Client::new(&cfg, None, DEVICE_ID.to_owned())
            .err()
            .expect("missing CA must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to read extra CA bundle"), "{msg}");
        assert!(msg.contains("/no/such/bundle.pem"), "{msg}");
    }

    // ── Client::max_tokens ──

    #[tokio::test]
    async fn max_tokens_sends_the_user_cap_over_the_model_default() {
        let mut cfg = test_config(OFFLINE_URL, api_key(), "claude-opus-5-5");
        cfg.max_tokens = Some(4_096);
        let client = client_for(&cfg);

        let (body, _) = capture_stream_turn(client.clone()).await;

        assert_eq!(client.max_tokens(), 4_096);
        assert_eq!(body["max_tokens"], 4_096);
    }

    // ── Client::set_session_id ──

    #[tokio::test]
    async fn set_session_id_propagates_to_header_and_metadata_user_id() {
        // Pin both wire surfaces: the mock matches the rolled id in the header, and the assertion
        // below pins the embedded JSON in `metadata.user_id`.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-claude-code-session-id", "sid-rolled"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body("", "Hi")),
            )
            .mount(&server)
            .await;

        let mut client = Client::new(
            &test_config(
                server.uri(),
                Auth::new(AuthSource::ApiKeyEnv, "k"),
                TEST_MODEL,
            ),
            Some("sid-original".to_owned()),
            DEVICE_ID.to_owned(),
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
        client_for(&cfg)
    }

    #[test]
    fn set_model_resolves_effort_and_persists_full_state() {
        for (from_model, from_effort, swap_to, expect) in [
            (
                "claude-opus-5-5",
                Some(Effort::Xhigh),
                "claude-sonnet-5-5",
                Some(Effort::Xhigh),
            ),
            (
                "custom-model",
                None,
                "claude-sonnet-5-5",
                Some(Effort::High),
            ),
            ("claude-opus-5-5", Some(Effort::High), "custom-model", None),
        ] {
            let mut client = client_with(from_model, from_effort);
            client.set_model(swap_to.to_owned());
            assert_eq!(
                client.effort, expect,
                "{from_model} → {swap_to}: stored effort",
            );
            assert_eq!(client.model().id(), swap_to, "{swap_to}: stored id");
        }
    }

    #[tokio::test]
    async fn set_model_rederives_every_model_gated_request_field() {
        let mut cfg = test_config(OFFLINE_URL, api_key(), "custom-model");
        cfg.max_tokens = None;
        cfg.show_thinking = true;
        let mut client = client_for(&cfg);

        let (unknown, unknown_betas) = capture_stream_turn(client.clone()).await;
        for field in ["thinking", "output_config", "context_management"] {
            assert!(unknown.get(field).is_none(), "{field} absent: {unknown}");
        }
        assert_eq!(unknown["max_tokens"], 16_000);
        for beta in ["context-management", "thinking-binding-controls"] {
            assert!(!unknown_betas.contains(beta), "{beta}: {unknown_betas}");
        }

        client.set_model("claude-opus-5-5".to_owned());
        let (known, known_betas) = capture_stream_turn(client).await;
        assert_eq!(known["model"], "claude-opus-5-5");
        assert_eq!(known["max_tokens"], 64_000);
        assert_eq!(
            known["thinking"],
            serde_json::json!({
                "type": "adaptive",
                "display": "summarized",
                "block_binding": {"prefix_mismatch_behavior": "drop_block"},
            }),
        );
        assert_eq!(known["output_config"]["effort"], "medium");
        assert_eq!(
            known["context_management"]["edits"][0]["type"],
            "clear_thinking_20251015",
        );
        assert!(
            known_betas.contains("context-management-2025-06-27"),
            "{known_betas}"
        );
    }

    // ── Client::set_effort ──

    #[test]
    fn set_effort_resolves_pick_against_active_model() {
        for (model, initial, pick, expect) in [
            (
                "claude-sonnet-5-5",
                Some(Effort::High),
                Effort::Max,
                Some(Effort::Max),
            ),
            ("custom-model", None, Effort::High, None),
        ] {
            let mut client = client_with(model, initial);
            assert_eq!(client.set_effort(pick), expect, "{model} pick={pick:?}");
            assert_eq!(
                client.effort, expect,
                "{model} pick={pick:?}: stored effort",
            );
        }
    }

    // ── Client::set_auth ──

    #[tokio::test]
    async fn set_auth_sends_the_new_credential_and_signing_out_stops_requests() {
        let mut client = Client::new(
            &test_config(OFFLINE_URL, api_key(), TEST_MODEL),
            None,
            DEVICE_ID.to_owned(),
        )
        .unwrap();

        client.set_auth(Some(Auth::new(AuthSource::OxLogin, "ox-token")));
        let request = capture_stream_request(client.clone()).await;
        client.set_auth(None);
        let signed_out = client
            .stream_message(&[Message::user("hi")], &[], None, &[])
            .expect_err("a signed-out client must not send");

        assert_eq!(
            request_header(&request, "authorization"),
            Some("Bearer ox-token")
        );
        assert_eq!(request_header(&request, "x-api-key"), None);
        assert_eq!(client.auth_source(), None);
        assert_eq!(signed_out.to_string(), "not signed in");
    }

    // ── Client::stream_message ──

    #[tokio::test]
    async fn stream_message_streams_usage_and_text_then_completes_the_reply() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(query_param("beta", "true"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body("", "Hi")),
            )
            .mount(&server)
            .await;

        let client = test_client(server.uri(), api_key(), TEST_MODEL);
        let rx = client
            .stream_message(&[Message::user("hello")], &[], None, &[])
            .unwrap();
        let events = collect_events(rx).await.unwrap();

        assert_eq!(
            events,
            [
                ResponseEvent::Usage(TokenUsage::new(1_000, 0)),
                ResponseEvent::Text("Hi".to_owned()),
                ResponseEvent::Usage(TokenUsage::new(1_000, 100)),
                ResponseEvent::Completed(Response {
                    content: vec![ContentBlock::Text {
                        text: "Hi".to_owned(),
                    }],
                    stop_reason: Some(StopReason::EndTurn),
                    ..Response::default()
                }),
            ],
        );
    }

    #[tokio::test]
    async fn stream_message_preserves_multibyte_codepoints_in_deltas() {
        // Pins the byte-level SSE buffer: lossy UTF-8 would mangle a 4-byte emoji split across
        // TCP chunk boundaries.
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

        let client = test_client(server.uri(), api_key(), TEST_MODEL);
        let events = collect_events(
            client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap(),
        )
        .await
        .unwrap();
        let got = events.iter().find_map(|e| match e {
            ResponseEvent::Text(text) => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(got, Some("🦀rust"));
    }

    #[tokio::test]
    async fn stream_message_sends_one_credential_header_and_betas_per_auth_kind() {
        let mut betas = Vec::new();
        for (auth, api_key, bearer) in [
            (
                Auth::new(AuthSource::ApiKeyEnv, "sk-env"),
                Some("sk-env"),
                None,
            ),
            (
                Auth::new(AuthSource::AuthTokenEnv, "gw-token"),
                None,
                Some("Bearer gw-token"),
            ),
            (
                Auth::new(AuthSource::OAuthTokenEnv, "oat-env"),
                None,
                Some("Bearer oat-env"),
            ),
        ] {
            let source = auth.source();
            let client = test_client(OFFLINE_URL, auth, TEST_MODEL);
            let request = capture_stream_request(client).await;
            assert_eq!(request_header(&request, "x-api-key"), api_key, "{source:?}");
            assert_eq!(
                request_header(&request, "authorization"),
                bearer,
                "{source:?}"
            );
            assert_eq!(
                request_header(&request, "x-claude-code-session-id"),
                Some("sid")
            );
            betas.push(
                request_header(&request, "anthropic-beta")
                    .unwrap()
                    .to_owned(),
            );
        }

        // The auth token keeps the API-key betas. OAuth adds its beta after the claude-code tag.
        let [api_key, auth_token, oauth]: [String; 3] = betas.try_into().unwrap();
        let tail = api_key.strip_prefix("claude-code-20250219,").unwrap();
        assert!(!tail.contains("oauth"), "{api_key}");
        assert_eq!(auth_token, api_key);
        assert_eq!(
            oauth,
            format!("claude-code-20250219,oauth-2025-04-20,{tail}")
        );
    }

    #[tokio::test]
    async fn stream_message_custom_headers_ride_along_and_override_built_ins() {
        // An API key travels in `x-api-key`, so a gateway may take its own `Authorization`.
        let mut config = test_config(OFFLINE_URL, api_key(), TEST_MODEL);
        for (name, value) in [
            ("x-team", "platform"),
            ("user-agent", "custom/1.0"),
            ("authorization", "Bearer gateway-jwt"),
        ] {
            config
                .gateway
                .custom_headers
                .insert(name, HeaderValue::from_static(value));
        }
        let request = capture_stream_request(client_for(&config)).await;
        assert_eq!(request_header(&request, "x-team"), Some("platform"));
        assert_eq!(
            request
                .headers
                .get_all("user-agent")
                .iter()
                .collect::<Vec<_>>(),
            ["custom/1.0"],
        );
        assert_eq!(
            request_header(&request, "authorization"),
            Some("Bearer gateway-jwt")
        );
        assert_eq!(request_header(&request, "x-api-key"), Some("k"));
    }

    #[tokio::test]
    async fn stream_message_billing_block_hashes_only_first_party_requests() {
        for is_first_party in [true, false] {
            let mut client = test_client(OFFLINE_URL, api_key(), TEST_MODEL);
            client.is_first_party = is_first_party;
            let (body, _) = capture_stream_turn(client).await;
            let block = body["system"][0]["text"].as_str().unwrap();
            let (prefix, cch) = block.split_once(" cch=").unwrap_or((block, ""));
            assert!(prefix.ends_with("; cc_entrypoint=cli;"), "{block}");
            let hash = cch.strip_suffix(';').unwrap_or(cch);
            let hashed =
                hash.len() == 5 && hash != "00000" && u32::from_str_radix(hash, 16).is_ok();
            assert_eq!(hashed, is_first_party, "{block}");
            assert_eq!(cch.is_empty(), !is_first_party, "{block}");
        }
    }

    #[tokio::test]
    async fn stream_message_attribution_off_drops_billing_block() {
        let mut config = test_config(OFFLINE_URL, oauth(), TEST_MODEL);
        config.gateway.attribution = false;
        let body = capture_stream_body(config).await;
        assert_eq!(body["system"][0]["text"], SYSTEM_PROMPT_PREFIX);
        assert!(
            !body.to_string().contains("x-anthropic-billing-header"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn stream_message_with_user_context_prepends_it_and_fingerprints_the_prompt() {
        let prompt = "Say 'hello' and nothing else.";
        let context = "<system-reminder>CLAUDE.md content here</system-reminder>";
        let server = MockServer::start().await;
        let body_sink: Captured<String> = captured();
        let sink_clone = std::sync::Arc::clone(&body_sink);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(move |req: &Request| {
                *sink_clone.lock().unwrap() = Some(String::from_utf8_lossy(&req.body).into_owned());
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body("", "Hi"))
            })
            .mount(&server)
            .await;

        let client = test_client(server.uri(), api_key(), TEST_MODEL);
        collect_events(
            client
                .stream_message(&[Message::user(prompt)], &[], Some(context), &[])
                .unwrap(),
        )
        .await
        .unwrap();

        let body = body_sink.lock().unwrap().clone().expect("body captured");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["system"][0]["text"],
            format!(
                "x-anthropic-billing-header: \
                 cc_version={CLAUDE_CLI_VERSION}.{}; cc_entrypoint=cli;",
                billing::compute_fingerprint(prompt, CLAUDE_CLI_VERSION),
            ),
        );
        let messages = v["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2, "user_context prepends: {body}");
        assert_eq!(
            messages[0],
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": context}]}),
        );
        assert_eq!(messages[1]["content"][0]["text"], prompt);
    }

    #[tokio::test]
    async fn stream_message_third_party_base_url_drops_global_scope_keeps_its_beta() {
        // As in Claude Code, the beta ships on every base URL, while the body-side
        // `scope: "global"` goes only to the Anthropic API.
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
                    .set_body_string(text_stream_body("", "Hi"))
            })
            .mount(&server)
            .await;

        let client = test_client(server.uri(), api_key(), TEST_MODEL);
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
            "prompt-caching-scope beta ships on 3P: {beta}",
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let system = v["system"].as_array().expect("system array");
        // [0] billing, [1] identity prefix, [2] static joined.
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
        // TTL rides through on 3P. Only `scope` is gated on 1P.
        assert_eq!(cc["ttl"], "1h", "default 1h ttl survives on 3P: {body}");
    }

    #[tokio::test]
    async fn stream_message_breakpoints_cover_static_prefix_and_conversation_tail() {
        let server = MockServer::start().await;
        let sink: Captured<String> = captured();
        let sink_clone = std::sync::Arc::clone(&sink);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(move |req: &Request| {
                *sink_clone.lock().unwrap() = Some(String::from_utf8_lossy(&req.body).into_owned());
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(text_stream_body("", "Hi"))
            })
            .mount(&server)
            .await;

        let mut client = test_client(server.uri(), api_key(), "claude-sonnet-5-5");
        // The conversation breakpoint omits `scope` even where the system prefix is global.
        client.is_first_party = true;
        let messages = [
            Message::user("question"),
            Message::assistant("answer"),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".to_owned(),
                    content: "ok".to_owned(),
                    is_error: false,
                }],
            },
        ];
        collect_events(
            client
                .stream_message(
                    &messages,
                    &["static", SYSTEM_PROMPT_DYNAMIC_BOUNDARY, "dynamic"],
                    Some("<system-reminder>context</system-reminder>"),
                    &[],
                )
                .unwrap(),
        )
        .await
        .unwrap();

        let body = sink.lock().unwrap().clone().expect("request captured");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["system"][2]["cache_control"],
            serde_json::json!({"type": "ephemeral", "scope": "global", "ttl": "1h"}),
        );
        let sent = v["messages"].as_array().expect("messages array");
        assert_eq!(sent.len(), 4, "user context leads the transcript: {body}");
        assert_eq!(
            sent[3]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
        );
        assert_eq!(
            body.matches(r#""cache_control""#).count(),
            2,
            "only the static prefix and the final block carry breakpoints: {body}",
        );
    }

    #[tokio::test]
    async fn stream_message_malformed_frame_is_skipped_without_poisoning_stream() {
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

        let client = test_client(server.uri(), api_key(), TEST_MODEL);
        let events = collect_events(
            client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap(),
        )
        .await
        .unwrap();
        let delta = events.iter().find_map(|e| match e {
            ResponseEvent::Text(text) => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(delta, Some("Hi"));
    }

    #[tokio::test]
    async fn stream_message_in_band_error_fails_the_stream_with_its_payload() {
        let server = MockServer::start().await;
        let body = sse_body(&[(
            "error",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Bad block"}}"#,
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

        let client = test_client(server.uri(), api_key(), TEST_MODEL);
        let err = collect_events(
            client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap(),
        )
        .await
        .expect_err("an in-band error fails the stream");

        assert_eq!(
            format!("{err:#}"),
            "API error (invalid_request_error): Bad block"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a non-transient error is not retried",
        );
    }

    #[tokio::test]
    async fn stream_message_redirect_is_not_followed() {
        let elsewhere = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&elsewhere)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", format!("{}/v1/messages", elsewhere.uri())),
            )
            .mount(&server)
            .await;

        let client = test_client(server.uri(), api_key(), TEST_MODEL);
        let rx = client
            .stream_message(&[Message::user("hi")], &[], None, &[])
            .unwrap();
        let err = collect_events(rx).await.expect_err("redirect must fail");

        assert_eq!(
            format!("{err:#}"),
            "Anthropic API endpoint redirected (HTTP 307 Temporary Redirect) to 127.0.0.1. \
             Redirects are not followed, so point the base URL at the final endpoint.",
        );
        elsewhere.verify().await;
    }

    #[tokio::test]
    async fn stream_message_http_error_propagates_status_and_body() {
        let rate_limited = r#"{"error":{"type":"rate_limit_error"}}"#;
        for (status, body, expected) in [
            (
                429_u16,
                rate_limited,
                format!(
                    "Anthropic API rate limited (HTTP 429): {rate_limited}. \
                     Retry after a short delay."
                ),
            ),
            (
                529,
                "overloaded",
                "Anthropic API overloaded (HTTP 529): overloaded. Retry in a few seconds."
                    .to_owned(),
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("x-should-retry", "false")
                        .set_body_string(body),
                )
                .mount(&server)
                .await;

            let client = test_client(server.uri(), api_key(), TEST_MODEL);
            let rx = client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap();
            let err = collect_events(rx).await.expect_err("expected HTTP error");

            assert_eq!(format!("{err:#}"), expected);
        }
    }

    #[tokio::test]
    async fn stream_message_401_names_the_oauth_token_variable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("token revoked"))
            .mount(&server)
            .await;

        let client = test_client(
            server.uri(),
            Auth::new(AuthSource::OAuthTokenEnv, "oat-env"),
            TEST_MODEL,
        );
        let rx = client
            .stream_message(&[Message::user("hi")], &[], None, &[])
            .unwrap();
        let err = collect_events(rx).await.expect_err("expected 401");

        assert_eq!(
            format!("{err:#}"),
            "Anthropic API rejected credentials (HTTP 401): token revoked. \
             CLAUDE_CODE_OAUTH_TOKEN may be expired or revoked. \
             Generate a new one with `claude setup-token`.",
        );
    }

    #[tokio::test]
    async fn stream_message_429_threads_retry_after_header_into_error() {
        // A hint past the retry cap fails fast with the hint in the message.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "120")
                    .set_body_string("Too many requests"),
            )
            .mount(&server)
            .await;

        let client = test_client(server.uri(), api_key(), TEST_MODEL);
        let rx = client
            .stream_message(&[Message::user("hi")], &[], None, &[])
            .unwrap();
        let err = collect_events(rx).await.expect_err("expected 429");

        assert_eq!(
            format!("{err:#}"),
            "Anthropic API rate limited (HTTP 429): Too many requests. Retry after 120 s.",
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn stream_message_rejects_a_credential_no_header_can_carry() {
        // `HeaderValue::from_str` rejects control chars (\n, \r). Both auth arms must propagate.
        for auth in [
            Auth::new(AuthSource::ApiKeyEnv, "bad\nkey"),
            Auth::new(AuthSource::OAuthTokenEnv, "bad\rtoken"),
        ] {
            let source = auth.source();
            let client = test_client(OFFLINE_URL, auth, TEST_MODEL);

            let err = client
                .stream_message(&[Message::user("hi")], &[], None, &[])
                .unwrap_err();

            let msg = format!("{err:#}");
            assert!(
                msg.starts_with(&format!("{source} is not a valid HTTP header value")),
                "error names the credential source: {msg}",
            );
            assert!(
                !msg.contains("bad"),
                "error must not echo the secret: {msg}"
            );
        }
    }

    // ── Client::stream_message / agentic body fields ──

    /// Body and `anthropic-beta` header of one streamed turn sent through `client`.
    async fn capture_stream_turn(client: Client) -> (serde_json::Value, String) {
        let request = capture_stream_request(client).await;
        let betas = request_header(&request, "anthropic-beta")
            .unwrap_or_default()
            .to_owned();
        (serde_json::from_slice(&request.body).unwrap(), betas)
    }

    async fn capture_stream_body(config: Config) -> serde_json::Value {
        capture_stream_turn(client_for(&config)).await.0
    }

    #[tokio::test]
    async fn stream_message_emits_output_config_effort() {
        let mut cfg = test_config(OFFLINE_URL, api_key(), "claude-opus-5-5");
        cfg.effort = Some(Effort::Xhigh);
        let body = capture_stream_body(cfg).await;
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    #[tokio::test]
    async fn stream_message_omits_output_config_when_effort_is_none() {
        // Unknown models resolve to `Config.effort == None`, so the body omits `output_config`.
        let cfg = test_config(OFFLINE_URL, api_key(), "custom-model");
        assert!(cfg.effort.is_none(), "precondition: effort unset");
        let body = capture_stream_body(cfg).await;
        assert!(
            body.get("output_config").is_none(),
            "output_config absent: {body}",
        );
    }

    #[tokio::test]
    async fn stream_message_context_management_body_present_on_every_known_model() {
        // The body directive must accompany the beta header `compute_betas` sends for these.
        for info in crate::model::MODELS {
            let cfg = test_config(OFFLINE_URL, api_key(), info.id);
            let body = capture_stream_body(cfg).await;
            let edits = body["context_management"]["edits"]
                .as_array()
                .unwrap_or_else(|| panic!("context_management.edits missing for {}", info.id));
            assert_eq!(edits.len(), 1, "{}", info.id);
            assert_eq!(edits[0]["type"], "clear_thinking_20251015", "{}", info.id);
            assert_eq!(edits[0]["keep"], "all", "{}", info.id);
        }
    }

    #[tokio::test]
    async fn stream_message_hidden_thinking_requests_updates_where_supported() {
        for (id, display, sends_beta) in [
            ("claude-opus-5-5", Some("updates"), true),
            ("claude-haiku-5-5", None, false),
        ] {
            let client = test_client(OFFLINE_URL, api_key(), id);
            let (body, betas) = capture_stream_turn(client).await;
            assert_eq!(
                body["thinking"].get("display"),
                display.map(Into::into).as_ref(),
                "{id}"
            );
            assert_eq!(
                betas.contains("thinking-display-updates-2026-08-18"),
                sends_beta,
                "{id}: {betas}",
            );
        }
    }

    #[tokio::test]
    async fn stream_message_known_model_drops_stale_thinking_blocks_under_binding_beta() {
        for info in crate::model::MODELS {
            let client = test_client(OFFLINE_URL, api_key(), info.id);
            let (body, betas) = capture_stream_turn(client).await;
            assert_eq!(
                body["thinking"]["block_binding"],
                serde_json::json!({"prefix_mismatch_behavior": "drop_block"}),
                "{}",
                info.id,
            );
            assert!(
                betas
                    .split(',')
                    .any(|beta| beta == "thinking-binding-controls-2026-08-01"),
                "{}: {betas}",
                info.id,
            );
        }
    }

    // ── build_metadata ──

    #[test]
    fn build_metadata_wraps_ids_in_stringified_json_with_canonical_field_order() {
        // Claude Code's field order is `device_id, account_uuid, session_id`, which
        // `serde_json::json!` would alphabetize.
        let meta = build_metadata("dev-1", "abc-123");
        assert_eq!(
            meta.user_id,
            r#"{"device_id":"dev-1","account_uuid":"","session_id":"abc-123"}"#,
        );
    }

    // ── build_system_blocks ──

    #[test]
    fn build_system_blocks_orders_billing_then_identity_then_extras() {
        let cache = CacheControl {
            r#type: "ephemeral",
            scope: Some("global"),
            ttl: Some("1h"),
        };

        let blocks = build_system_blocks(
            Some("billing"),
            &[("static body", Some(cache)), ("dynamic body", None)],
        );

        assert_eq!(
            serde_json::to_value(blocks).unwrap(),
            serde_json::json!([
                {"type": "text", "text": "billing"},
                {"type": "text", "text": SYSTEM_PROMPT_PREFIX},
                {
                    "type": "text",
                    "text": "static body",
                    "cache_control": {"type": "ephemeral", "scope": "global", "ttl": "1h"},
                },
                {"type": "text", "text": "dynamic body"},
            ]),
        );
    }

    #[test]
    fn build_system_blocks_drops_empty_extras_and_omits_billing_when_absent() {
        let blocks = build_system_blocks(None, &[("", None), ("only-content", None)]);

        assert_eq!(
            serde_json::to_value(blocks).unwrap(),
            serde_json::json!([
                {"type": "text", "text": SYSTEM_PROMPT_PREFIX},
                {"type": "text", "text": "only-content"},
            ]),
        );
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
        assert_eq!(dynamic, [] as [&str; 0]);
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
        assert_eq!(statics, [] as [&str; 0]);
        assert_eq!(dynamic, vec!["env", "lang"]);

        let (statics, dynamic) =
            split_at_boundary(&["intro", "tasks", SYSTEM_PROMPT_DYNAMIC_BOUNDARY]);
        assert_eq!(statics, vec!["intro", "tasks"]);
        assert_eq!(dynamic, [] as [&str; 0]);
    }

    // ── first_user_text ──

    #[test]
    fn first_user_text_extracts_from_first_user_message() {
        let messages = vec![Message::user("hello world"), Message::assistant("hi")];
        assert_eq!(first_user_text(&messages), "hello world");
    }

    #[test]
    fn first_user_text_is_empty_when_absent() {
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
