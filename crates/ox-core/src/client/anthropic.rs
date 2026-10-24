//! Anthropic Messages API client, which implements the [`Wire`] the agent loop streams through.
//! [`Client::complete`] handles side queries such as title generation.
//!
//! The wire shape mirrors the official `claude-code` CLI: pinned User-Agent and Stainless SDK
//! header set, billing attestation in `system[0]`, and per-request beta headers gated on model /
//! auth-mode capabilities. Some relay gateways admit only requests that carry the billing
//! attestation or `metadata.user_id`.

mod accumulator;
mod betas;
mod billing;
mod completion;
pub mod identity;
#[cfg(test)]
pub(crate) mod local_testing;
mod sse;
#[cfg(any(test, feature = "testing"))]
pub(crate) mod testing;
mod transport;
pub(crate) mod wire;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::task::TaskTracker;
use tracing::debug;
use uuid::Uuid;

use self::accumulator::ReplySpec;
use self::betas::compute_betas;
use self::sse::spawn_stream;
use self::transport::{MessagesRequest, messages_url};
use self::wire::{
    CacheControl, ContextManagement, CreateMessageRequest, OutputConfig, RequestMessages,
    RequestMetadata, SystemBlock, ThinkingConfig,
};
use super::{ResponseEvent, Wire};
use crate::config::auth::{self, Auth, AuthSource, LoginUnavailable};
use crate::config::gateway::GatewayCompat;
use crate::config::oauth::ClaudeCodeKeychain;
use crate::config::{Config, is_first_party_base_url};
use crate::message::{ContentBlock, Message, Role};
use crate::model::{Effort, ModelSpec, PromptCacheTtl};
use crate::prompt::SystemPrompt;
use crate::tool::ToolDefinition;
use crate::util::tls::Trust;

// ── Constants ──

const API_VERSION: &str = "2023-06-01";

/// Claude Code release the user agent, billing header, and agentic beta sequence all mirror. Some
/// relay gateways enforce a `cc_version` floor. Bump it only with a fresh beta capture.
const CLAUDE_CLI_VERSION: &str = "2.1.296";
const STAINLESS_PACKAGE_VERSION: &str = "0.81.0";
const STAINLESS_RUNTIME_VERSION: &str = "v24.3.0";
const STAINLESS_TIMEOUT_SECS: &str = "600";

/// Required as its own text block because non-Haiku OAuth requests 429 without it.
const SYSTEM_PROMPT_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

// ── Client ──

/// Per-session Anthropic client. Clones share the connection pool and stored-login token.
#[derive(Clone)]
pub(crate) struct Client {
    http: reqwest::Client,
    /// The `/v1/messages` endpoint under the configured base URL.
    messages_url: String,
    /// Whether the client talks to the Anthropic API, which alone gets `scope: "global"` and `cch`,
    /// as in Claude Code.
    is_first_party: bool,
    gateway: GatewayCompat,
    /// `None` while signed out.
    auth: Option<Auth>,
    trust: Arc<dyn Trust>,
    /// Stored-login reloads that outlive their requests, which the session waits for at exit.
    reloads: TaskTracker,
    model: ModelSpec,
    /// `None` for models outside the catalog.
    effort: Option<Effort>,
    /// User-set output cap. `None` defers to [`ModelSpec::resolve_max_tokens`].
    max_tokens: Option<u32>,
    prompt_cache_ttl: PromptCacheTtl,
    /// Whether adaptive thinking opts into summarized text.
    show_thinking: bool,
    session_id: String,
    device_id: String,
}

impl Client {
    /// Builds a client for the endpoint, credential, and request settings in `config`, using
    /// `trust` to verify servers.
    pub(crate) fn new(
        config: &Config,
        trust: impl Trust + 'static,
        session_id: Option<String>,
        device_id: String,
    ) -> Result<Self> {
        let Config {
            base_url,
            gateway,
            auth,
            model,
            effort,
            max_tokens,
            prompt_cache_ttl,
            show_thinking,
            ..
        } = config;
        Ok(Self {
            http: build_http(gateway, &trust)?,
            messages_url: messages_url(base_url),
            is_first_party: is_first_party_base_url(base_url),
            gateway: gateway.clone(),
            auth: auth.clone(),
            trust: Arc::new(trust),
            reloads: TaskTracker::new(),
            model: model.clone(),
            effort: *effort,
            max_tokens: *max_tokens,
            prompt_cache_ttl: *prompt_cache_ttl,
            show_thinking: *show_thinking,
            session_id: session_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            device_id,
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

    /// The trust roots used by requests, sign-ins, and stored-login refreshes.
    pub(crate) fn trust(&self) -> &dyn Trust {
        self.trust.as_ref()
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

    /// Swaps the active model and re-resolves effort. Thinking, betas, context management, and the
    /// default `max_tokens` derive from the model per request.
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

    /// Loads the first stored login again on a reload task. Dropping the returned future stops only
    /// the wait, since a refresh must still write the rotated tokens back.
    pub(crate) fn retry_stored_login(
        &self,
        keychain: ClaudeCodeKeychain,
    ) -> impl Future<Output = Result<Auth, LoginUnavailable>> + use<> {
        let trust = Arc::clone(&self.trust);
        let reload = self
            .reloads
            .spawn(async move { auth::retry_stored_login(keychain, trust.as_ref()).await });
        async move {
            reload
                .await
                .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
        }
    }

    /// Waits for the stored-login reloads in flight from this client or a clone of it, including
    /// one for a credential since replaced, so exiting does not cancel their write-back.
    pub(crate) async fn finish_reloads(&self) {
        self.reloads.close();
        self.reloads.wait().await;
    }

    /// Streams one conversation request. The receiver closes when the stream ends, the receiver is
    /// dropped, or an unrecoverable error is forwarded as a final `Err`.
    ///
    /// The stable `system` text always carries a `cache_control` breakpoint, shared across
    /// sessions. `breakpoint` marks the last content block of the final message. `user_context`
    /// rides as a synthetic leading user message to keep dynamic content such as CLAUDE.md out of
    /// the `system` parameter.
    fn stream(
        &self,
        messages: &[Message],
        system: &SystemPrompt,
        user_context: Option<&str>,
        tools: &[ToolDefinition],
        breakpoint: Option<CacheControl>,
    ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
        let prefix_cache = CacheControl::static_prefix(self.is_first_party, self.prompt_cache_ttl);
        let thinking = ThinkingConfig::for_model(&self.model, self.show_thinking);
        let request = self.prepare(RequestSpec {
            kind: RequestKind::Agentic,
            model: &self.model,
            thinking: thinking.as_ref(),
            max_tokens: self.max_tokens(),
            system: &[
                (&system.stable, Some(prefix_cache)),
                (&system.dynamic, None),
            ],
            tools: (!tools.is_empty()).then_some(tools),
            output_config: OutputConfig::new(None, self.effort),
            messages: RequestMessages {
                user_context,
                messages,
                breakpoint,
            },
        })?;

        Ok(spawn_stream(
            self.http.clone(),
            request,
            ReplySpec {
                model: self.model.id().to_owned(),
                thinking_text: thinking
                    .as_ref()
                    .map_or(self.show_thinking, ThinkingConfig::asks_for_text),
            },
        ))
    }
}

impl Wire for Client {
    /// Marks the transcript tail with a `cache_control` breakpoint, so the next request in the
    /// conversation reads the whole transcript from cache.
    fn stream_message(
        &self,
        messages: &[Message],
        system: &SystemPrompt,
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
        let breakpoint = CacheControl::conversation(self.prompt_cache_ttl);
        self.stream(messages, system, user_context, tools, Some(breakpoint))
    }

    fn stream_one_shot(
        &self,
        messages: &[Message],
        system: &SystemPrompt,
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
        self.stream(messages, system, user_context, tools, None)
    }
}

// ── HTTP Client ──

/// Builds the HTTP client with shared request headers and TLS trust. Each request adds its own
/// credential.
fn build_http(gateway: &GatewayCompat, trust: &dyn Trust) -> Result<reqwest::Client> {
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

    // No whole-request timeout because responses can run for minutes. The 60 s read timeout catches
    // slowloris dribble. Anthropic sends keepalives every ~15 s on healthy streams. reqwest strips
    // only `Authorization` and `Cookie` on a cross-host redirect, so following one would carry
    // `x-api-key` and the custom headers to another host.
    let builder = reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_mins(1));
    trust
        .apply(builder)?
        .build()
        .context("failed to build HTTP client")
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

// ── Request Building ──

/// What a request serves, which decides its betas and the body fields beyond the caller's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    /// A conversation turn, streamed with `context_management` and the agentic betas.
    Agentic,
    /// A side request such as a session title, sent without streaming or agentic betas.
    SideQuery,
}

/// What a caller of [`Client::prepare`] sets in a request body.
struct RequestSpec<'a> {
    kind: RequestKind,
    model: &'a ModelSpec,
    thinking: Option<&'a ThinkingConfig>,
    max_tokens: u32,
    /// The text after the identity block, each with its `cache_control`. Empty text is left out.
    system: &'a [(&'a str, Option<CacheControl>)],
    tools: Option<&'a [ToolDefinition]>,
    output_config: Option<OutputConfig<'a>>,
    messages: RequestMessages<'a>,
}

impl Client {
    /// Builds an attributed request. The kind selects streaming, context management, and betas.
    fn prepare(&self, spec: RequestSpec<'_>) -> Result<MessagesRequest> {
        let RequestSpec {
            kind,
            model,
            thinking,
            max_tokens,
            system,
            tools,
            output_config,
            messages,
        } = spec;
        let auth = self.auth()?;
        let agentic = kind == RequestKind::Agentic;

        // The fingerprint skips the user context, which Claude Code marks as a meta message.
        let attribution = self
            .gateway
            .attribution
            .then(|| attribution_text(first_user_text(messages.messages), self.is_first_party));
        let mut body = serde_json::to_string(&CreateMessageRequest {
            model: model.id(),
            max_tokens,
            stream: agentic,
            metadata: build_metadata(&self.device_id, &self.session_id),
            system: build_system_blocks(attribution.as_deref(), system),
            tools,
            thinking,
            output_config,
            // Accompanies the context-management beta, which `compute_betas` gates the same way.
            context_management: (agentic && model.info().is_some())
                .then(ContextManagement::clear_thinking_keep_all),
            messages,
        })
        .context("failed to serialize request")?;
        if attribution.is_some() && self.is_first_party {
            body = billing::inject_cch(&body)?;
        }

        debug!(
            ?kind,
            model = model.id(),
            body_len = body.len(),
            "sending API request"
        );
        let betas = compute_betas(model, thinking, auth.kind(), kind).join(",");
        Ok(self.messages_request(betas, body, auth))
    }
}

/// The `system[0]` attribution text. First-party requests carry a `cch` placeholder that
/// [`billing::inject_cch`] fills once the body is serialized. Like Claude Code since 2.1.181, other
/// base URLs get a stable block without it, so gateways that cache on the body still hit.
fn attribution_text(first_user_text: &str, is_first_party: bool) -> String {
    let fingerprint = billing::compute_fingerprint(first_user_text, CLAUDE_CLI_VERSION);
    billing::build_billing_header(CLAUDE_CLI_VERSION, &fingerprint, is_first_party)
}

fn first_user_text(messages: &[Message]) -> &str {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .into_iter()
        .flat_map(|m| &m.content)
        .find_map(ContentBlock::as_text)
        .unwrap_or("")
}

/// Some relay gateways admit a request without the billing block only when `user_id` holds a JSON
/// object with `session_id`.
fn build_metadata(device_id: &str, session_id: &str) -> RequestMetadata {
    #[derive(Serialize)]
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
    .expect("UserId has only string fields, which always serialize");
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
            text,
            cache_control: None,
        });
    }
    blocks.push(SystemBlock {
        text: SYSTEM_PROMPT_PREFIX,
        cache_control: None,
    });
    for &(text, cache_control) in extras {
        if !text.is_empty() {
            blocks.push(SystemBlock {
                text,
                cache_control,
            });
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use indoc::writedoc;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::local_testing::{
        client_for, completion_body, oauth, request_count, serve, test_client,
    };
    use super::testing::{DEVICE_ID, api_key, sse_body, sse_response, text_stream_body};
    use super::*;
    use crate::client::{Response, StopReason};
    use crate::config::oauth::testing::{HeldRefresh, ok_refresh_body, ox_login_in, write_creds};
    use crate::config::testing::test_config;
    use crate::model::TokenUsage;
    use crate::util::tls::PlatformTrust;
    use crate::util::tls::testing::{NoRoots, TEST_CA_PATH};

    mod client {
        use super::*;

        const OFFLINE_URL: &str = "https://example.invalid";

        const TEST_MODEL: &str = "claude-sonnet-5-5";

        async fn collect_events(
            mut rx: mpsc::Receiver<Result<ResponseEvent>>,
        ) -> Result<Vec<ResponseEvent>> {
            let mut out = Vec::new();
            while let Some(event) = rx.recv().await {
                out.push(event?);
            }
            Ok(out)
        }

        /// Streams one `hi` turn through `client` and collects its events.
        async fn stream_hi(client: &Client) -> Result<Vec<ResponseEvent>> {
            let rx = client.stream_message(
                &[Message::user("hi")],
                &SystemPrompt::default(),
                None,
                &[],
            )?;
            collect_events(rx).await
        }

        /// Streams `messages` through `client` against a mock server and returns the request it
        /// received.
        async fn capture_stream(
            mut client: Client,
            messages: &[Message],
            system: &SystemPrompt,
            user_context: Option<&str>,
        ) -> Request {
            let server = serve(vec![sse_response(text_stream_body("", "Hi"))]).await;
            client.messages_url = messages_url(&server.uri());
            collect_events(
                client
                    .stream_message(messages, system, user_context, &[])
                    .unwrap(),
            )
            .await
            .unwrap();

            let mut received = server.received_requests().await.expect("recorded requests");
            assert_eq!(received.len(), 1, "exactly one streamed call");
            received.remove(0)
        }

        /// [`capture_stream`] for one `hi` turn without a system prompt or user context.
        async fn capture_stream_request(client: Client) -> Request {
            capture_stream(
                client,
                &[Message::user("hi")],
                &SystemPrompt::default(),
                None,
            )
            .await
        }

        fn request_header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
            request.headers.get(name).map(|v| v.to_str().unwrap())
        }

        fn client_with(model: &str, effort: Option<Effort>) -> Client {
            let mut cfg = test_config(OFFLINE_URL, api_key(), model);
            cfg.effort = effort;
            client_for(&cfg)
        }

        /// Body and `anthropic-beta` header of one streamed turn sent through `client`.
        async fn capture_stream_turn(client: Client) -> (serde_json::Value, String) {
            let request = capture_stream_request(client).await;
            let betas = request_header(&request, "anthropic-beta")
                .unwrap_or_default()
                .to_owned();
            (serde_json::from_slice(&request.body).unwrap(), betas)
        }

        mod new {
            use super::*;

            #[test]
            fn exposes_the_model() {
                let client = test_client(OFFLINE_URL, api_key(), TEST_MODEL);

                assert_eq!(client.model().id(), TEST_MODEL);
            }

            #[test]
            fn exposes_the_prompt_cache_ttl() {
                let mut config = test_config(OFFLINE_URL, api_key(), TEST_MODEL);
                config.prompt_cache_ttl = PromptCacheTtl::FiveMin;

                let client = client_for(&config);

                assert_eq!(client.prompt_cache_ttl(), PromptCacheTtl::FiveMin);
            }

            #[test]
            fn without_a_session_id_generates_uuid_v4() {
                let config = test_config(OFFLINE_URL, api_key(), TEST_MODEL);

                let client = Client::new(&config, NoRoots, None, DEVICE_ID.to_owned()).unwrap();

                let sid = client.session_id();
                let parsed = Uuid::parse_str(sid)
                    .unwrap_or_else(|_| panic!("auto-generated session_id is not a UUID: {sid:?}"));
                assert_eq!(parsed.get_version_num(), 4);
            }

            #[test]
            fn preserves_explicit_session_id() {
                let sid = "11111111-2222-4333-8444-555555555555".to_owned();
                let config = test_config(OFFLINE_URL, api_key(), TEST_MODEL);

                let client =
                    Client::new(&config, NoRoots, Some(sid.clone()), DEVICE_ID.to_owned()).unwrap();

                assert_eq!(client.session_id(), sid);
            }

            #[tokio::test]
            async fn joins_the_messages_path_under_a_base_url_with_a_trailing_slash() {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/anthropic/v1/messages"))
                    .and(query_param("beta", "true"))
                    .respond_with(sse_response(text_stream_body("", "Hi")))
                    .expect(1)
                    .mount(&server)
                    .await;
                let base_url = format!("{}/anthropic/", server.uri());
                let client = test_client(base_url, api_key(), TEST_MODEL);

                stream_hi(&client).await.unwrap();
            }

            #[test]
            fn with_extra_ca_certs_accepts_valid_bundle() {
                // `util::tls` covers the handshake through a bundle, so this checks only that a
                // valid one builds a client with the platform's real trust services.
                let cfg = test_config(OFFLINE_URL, api_key(), TEST_MODEL);
                let trust = PlatformTrust(Some(TEST_CA_PATH.into()));

                Client::new(&cfg, trust, None, DEVICE_ID.to_owned())
                    .expect("valid CA bundle must build a client");
            }

            #[test]
            fn surfaces_extra_ca_certs_error_with_path() {
                let cfg = test_config(OFFLINE_URL, api_key(), TEST_MODEL);
                let trust = PlatformTrust(Some("/no/such/bundle.pem".into()));

                let err = Client::new(&cfg, trust, None, DEVICE_ID.to_owned())
                    .err()
                    .expect("missing CA must error");

                assert_eq!(
                    err.to_string(),
                    "failed to read extra CA bundle at /no/such/bundle.pem"
                );
            }
        }

        mod max_tokens {
            use super::*;

            #[tokio::test]
            async fn sends_the_user_cap_over_the_model_default() {
                let mut cfg = test_config(OFFLINE_URL, api_key(), "claude-opus-5-5");
                cfg.max_tokens = Some(4_096);
                let client = client_for(&cfg);

                let (body, _) = capture_stream_turn(client.clone()).await;

                assert_eq!(client.max_tokens(), 4_096);
                assert_eq!(body["max_tokens"], 4_096);
            }
        }

        mod set_session_id {
            use super::*;

            #[tokio::test]
            async fn propagates_to_header_and_metadata_user_id() {
                let mut client = test_client(OFFLINE_URL, api_key(), TEST_MODEL);

                client.set_session_id("sid-rolled".to_owned());
                let request = capture_stream_request(client).await;

                assert_eq!(
                    request_header(&request, "x-claude-code-session-id"),
                    Some("sid-rolled"),
                );
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(
                    body["metadata"]["user_id"],
                    build_metadata(DEVICE_ID, "sid-rolled").user_id,
                );
            }
        }

        mod set_model {
            use super::*;

            #[test]
            fn resolves_effort_and_persists_full_state() {
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
            async fn rederives_every_model_gated_request_field() {
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
        }

        mod set_effort {
            use super::*;

            #[test]
            fn resolves_pick_against_active_model() {
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
        }

        mod set_auth {
            use super::*;

            #[tokio::test]
            async fn sends_the_new_credential_and_signing_out_stops_requests() {
                let mut client = test_client(OFFLINE_URL, api_key(), TEST_MODEL);

                client.set_auth(Some(Auth::new(AuthSource::OxLogin, "ox-token")));
                let request = capture_stream_request(client.clone()).await;
                client.set_auth(None);
                let signed_out = client
                    .stream_message(&[Message::user("hi")], &SystemPrompt::default(), None, &[])
                    .expect_err("a signed-out client must not send");

                assert_eq!(
                    request_header(&request, "authorization"),
                    Some("Bearer ox-token")
                );
                assert_eq!(request_header(&request, "x-api-key"), None);
                assert_eq!(client.auth_source(), None);
                assert_eq!(signed_out.to_string(), "not signed in");
            }
        }

        mod finish_reloads {
            use super::*;

            #[tokio::test]
            async fn waits_for_a_refresh_that_a_replaced_login_left_in_flight() {
                for (token, expires_at) in [("expired", 0), ("rejected", 9_999_999_999_999)] {
                    let server = serve(vec![ResponseTemplate::new(401)]).await;
                    let refresh = HeldRefresh::mount(&server).await;
                    let dir = tempfile::tempdir().unwrap();
                    let creds = dir.path().join("credentials.json");
                    write_creds(&creds, token, Some("old-refresh"), expires_at);
                    let replaced = ox_login_in(&creds, &server.uri(), token, expires_at);
                    let mut client = test_client(server.uri(), replaced.clone(), TEST_MODEL);

                    refresh
                        .abort_mid_refresh({
                            let http = client.http.clone();
                            let request =
                                client.messages_request(String::new(), "{}".to_owned(), &replaced);
                            async move { transport::post_messages(&http, &request, 0).await }
                        })
                        .await;
                    client.set_auth(Some(Auth::new(AuthSource::OxLogin, "ox-token")));
                    refresh.release();
                    client.finish_reloads().await;

                    assert_eq!(replaced.secret(), "fresh-access", "{token}");
                    server.verify().await;
                }
            }
        }

        mod stream_message {
            use super::*;

            fn system_prompt(stable: &str, dynamic: &str) -> SystemPrompt {
                SystemPrompt {
                    stable: stable.to_owned(),
                    dynamic: dynamic.to_owned(),
                }
            }

            fn raw_sse_body(frames: &[(&str, &str)]) -> String {
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

            async fn capture_stream_body(config: Config) -> serde_json::Value {
                capture_stream_turn(client_for(&config)).await.0
            }

            #[tokio::test]
            async fn streams_usage_and_text_then_completes_the_reply() {
                let server = serve(vec![sse_response(text_stream_body("", "Hi"))]).await;
                let client = test_client(server.uri(), api_key(), TEST_MODEL);

                let events = stream_hi(&client).await.unwrap();

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
            async fn preserves_multibyte_codepoints_in_deltas() {
                // Pins the byte-level SSE buffer: lossy UTF-8 would mangle a 4-byte emoji split
                // across TCP chunk boundaries.
                let body = raw_sse_body(&[
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
                let server = serve(vec![sse_response(body)]).await;
                let client = test_client(server.uri(), api_key(), TEST_MODEL);

                let events = stream_hi(&client).await.unwrap();

                assert_eq!(
                    events,
                    [
                        ResponseEvent::Text("🦀rust".to_owned()),
                        ResponseEvent::Completed(Response {
                            content: vec![ContentBlock::Text {
                                text: "🦀rust".to_owned(),
                            }],
                            ..Response::default()
                        }),
                    ],
                );
            }

            #[tokio::test]
            async fn sends_one_credential_header_and_betas_per_auth_kind() {
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

                // Auth tokens keep API-key betas. OAuth adds its beta after the claude-code tag.
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
            async fn custom_headers_ride_along_and_override_built_ins() {
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
            async fn attribution_off_drops_billing_block() {
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
            async fn with_user_context_prepends_it_and_fingerprints_the_prompt() {
                let prompt = "Say 'hello' and nothing else.";
                let context = "<system-reminder>CLAUDE.md content here</system-reminder>";
                let client = test_client(OFFLINE_URL, api_key(), TEST_MODEL);

                let request = capture_stream(
                    client,
                    &[Message::user(prompt)],
                    &SystemPrompt::default(),
                    Some(context),
                )
                .await;

                let v: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(
                    v["system"][0]["text"],
                    format!(
                        "x-anthropic-billing-header: \
                         cc_version={CLAUDE_CLI_VERSION}.{}; cc_entrypoint=cli;",
                        billing::compute_fingerprint(prompt, CLAUDE_CLI_VERSION),
                    ),
                );
                let messages = v["messages"].as_array().expect("messages array");
                assert_eq!(messages.len(), 2, "user_context prepends: {v}");
                assert_eq!(
                    messages[0],
                    serde_json::json!({
                        "role": "user",
                        "content": [{"type": "text", "text": context}],
                    }),
                );
                assert_eq!(messages[1]["content"][0]["text"], prompt);
            }

            #[tokio::test]
            async fn third_party_base_url_drops_global_scope_keeps_its_beta() {
                // As in Claude Code, the beta ships on every base URL, while the body-side
                // `scope: "global"` goes only to the Anthropic API.
                let client = test_client(OFFLINE_URL, api_key(), TEST_MODEL);

                let request = capture_stream(
                    client,
                    &[Message::user("hi")],
                    &system_prompt("static", ""),
                    None,
                )
                .await;

                let betas = request_header(&request, "anthropic-beta").unwrap();
                assert_eq!(
                    betas.split(',').collect::<Vec<_>>(),
                    [
                        "claude-code-20250219",
                        "interleaved-thinking-2025-05-14",
                        "thinking-token-count-2026-05-13",
                        "context-management-2025-06-27",
                        "prompt-caching-scope-2026-01-05",
                        "mid-conversation-system-2026-04-07",
                        "per-turn-control-2026-07-01",
                        "advisor-tool-2026-03-01",
                        "effort-2025-11-24",
                        "thinking-binding-controls-2026-08-01",
                        "thinking-display-updates-2026-08-18",
                    ],
                );
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(
                    body["system"],
                    serde_json::json!([
                        {"type": "text", "text": attribution_text("hi", false)},
                        {"type": "text", "text": SYSTEM_PROMPT_PREFIX},
                        {
                            "type": "text",
                            "text": "static",
                            "cache_control": {"type": "ephemeral", "ttl": "1h"},
                        },
                    ]),
                );
            }

            #[tokio::test]
            async fn breakpoints_cover_static_prefix_and_conversation_tail() {
                let mut client = test_client(OFFLINE_URL, api_key(), TEST_MODEL);
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

                let request = capture_stream(
                    client,
                    &messages,
                    &system_prompt("static", "dynamic"),
                    Some("<system-reminder>context</system-reminder>"),
                )
                .await;

                let body = String::from_utf8(request.body).unwrap();
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(
                    v["system"][2]["cache_control"],
                    serde_json::json!({"type": "ephemeral", "scope": "global", "ttl": "1h"}),
                );
                assert_eq!(
                    v["system"][3],
                    serde_json::json!({"type": "text", "text": "dynamic"}),
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
            async fn malformed_frame_fails_the_stream() {
                let body = raw_sse_body(&[
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
                let server = serve(vec![sse_response(body)]).await;
                let client = test_client(server.uri(), api_key(), TEST_MODEL);

                let error = stream_hi(&client)
                    .await
                    .expect_err("corrupt data fails the stream");

                assert_eq!(
                    format!("{error:#}"),
                    "Anthropic API stream failed: failed to parse SSE data: {not valid json: \
                     key must be a string at line 1 column 2",
                );
            }

            #[tokio::test]
            async fn in_band_error_fails_the_stream_with_its_payload() {
                let body = raw_sse_body(&[(
                    "error",
                    r#"{"type":"error","error":{"type":"invalid_request_error","message":"Bad block"}}"#,
                )]);
                let server = serve(vec![sse_response(body)]).await;
                let client = test_client(server.uri(), api_key(), TEST_MODEL);

                let err = stream_hi(&client)
                    .await
                    .expect_err("an in-band error fails the stream");

                assert_eq!(
                    format!("{err:#}"),
                    "Anthropic API error (invalid_request_error): Bad block"
                );
                assert_eq!(
                    request_count(&server).await,
                    1,
                    "a non-transient error is not retried",
                );
            }

            #[tokio::test]
            async fn redirect_is_not_followed() {
                let elsewhere = serve(vec![ResponseTemplate::new(200)]).await;
                let server =
                    serve(vec![ResponseTemplate::new(307).insert_header(
                        "location",
                        format!("{}/v1/messages", elsewhere.uri()),
                    )])
                    .await;
                let client = test_client(server.uri(), api_key(), TEST_MODEL);

                let err = stream_hi(&client).await.expect_err("redirect must fail");

                assert_eq!(
                    format!("{err:#}"),
                    "Anthropic API endpoint redirected (HTTP 307 Temporary Redirect) to 127.0.0.1. \
                     Redirects are not followed, so point the base URL at the final endpoint.",
                );
                assert_eq!(request_count(&elsewhere).await, 0);
            }

            #[tokio::test]
            async fn http_error_propagates_status_and_body() {
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
                    let server = serve(vec![
                        ResponseTemplate::new(status)
                            .insert_header("x-should-retry", "false")
                            .set_body_string(body),
                    ])
                    .await;
                    let client = test_client(server.uri(), api_key(), TEST_MODEL);

                    let err = stream_hi(&client).await.expect_err("expected HTTP error");

                    assert_eq!(format!("{err:#}"), expected);
                }
            }

            #[tokio::test]
            async fn on_401_names_the_oauth_token_variable_without_resending() {
                let server = serve(vec![
                    ResponseTemplate::new(401).set_body_string("token revoked"),
                ])
                .await;
                let client = test_client(
                    server.uri(),
                    Auth::new(AuthSource::OAuthTokenEnv, "oat-env"),
                    TEST_MODEL,
                );

                let err = stream_hi(&client).await.expect_err("expected 401");

                assert_eq!(
                    format!("{err:#}"),
                    "Anthropic API rejected credentials (HTTP 401): token revoked. \
                     CLAUDE_CODE_OAUTH_TOKEN may be expired or revoked. \
                     Generate a new one with `claude setup-token`.",
                );
                assert_eq!(request_count(&server).await, 1);
            }

            #[tokio::test]
            async fn on_429_threads_retry_after_header_into_error() {
                // A hint past the retry cap fails fast with the hint in the message.
                let server = serve(vec![
                    ResponseTemplate::new(429)
                        .insert_header("retry-after", "120")
                        .set_body_string("Too many requests"),
                ])
                .await;
                let client = test_client(server.uri(), api_key(), TEST_MODEL);

                let err = stream_hi(&client).await.expect_err("expected 429");

                assert_eq!(
                    format!("{err:#}"),
                    "Anthropic API rate limited (HTTP 429): Too many requests. Retry after 120 s.",
                );
                assert_eq!(request_count(&server).await, 1);
            }

            #[tokio::test]
            async fn rejects_a_credential_no_header_can_carry() {
                // `HeaderValue::from_str` rejects control chars, which both auth arms must report.
                for auth in [
                    Auth::new(AuthSource::ApiKeyEnv, "bad\nkey"),
                    Auth::new(AuthSource::OAuthTokenEnv, "bad\rtoken"),
                ] {
                    let source = auth.source();
                    let client = test_client(OFFLINE_URL, auth, TEST_MODEL);

                    let err = stream_hi(&client).await.unwrap_err();

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

            #[tokio::test]
            async fn refreshes_an_expired_stored_login_once_for_every_clone() {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/token"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(ok_refresh_body(
                        "fresh-access",
                        "fresh-refresh",
                        3600,
                    )))
                    .expect(1)
                    .mount(&server)
                    .await;
                for (streamed, reply) in [
                    (true, sse_response(text_stream_body("", "Hi"))),
                    (
                        false,
                        ResponseTemplate::new(200).set_body_string(completion_body("Title")),
                    ),
                ] {
                    Mock::given(method("POST"))
                        .and(path("/v1/messages"))
                        .and(header("authorization", "Bearer fresh-access"))
                        .and(body_partial_json(serde_json::json!({ "stream": streamed })))
                        .respond_with(reply)
                        .expect(1)
                        .mount(&server)
                        .await;
                }
                let dir = tempfile::tempdir().unwrap();
                let file = dir.path().join("credentials.json");
                write_creds(&file, "expired", Some("old-refresh"), 0);
                let token_url = format!("{}/token", server.uri());
                let client = test_client(
                    server.uri(),
                    ox_login_in(&file, &token_url, "expired", 0),
                    TEST_MODEL,
                );

                stream_hi(&client).await.unwrap();
                let stored: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
                std::fs::remove_file(&file).unwrap();
                let haiku = ModelSpec::resolve("claude-haiku-5-5");
                let (title, _) = client
                    .clone()
                    .complete(&haiku, "", "u", 40, None, None)
                    .await
                    .unwrap();

                assert_eq!(stored["claudeAiOauth"]["accessToken"], "fresh-access");
                assert_eq!(stored["claudeAiOauth"]["refreshToken"], "fresh-refresh");
                assert_eq!(
                    title, "Title",
                    "the clone reuses the refreshed token unread"
                );
                server.verify().await;
            }

            #[tokio::test]
            async fn emits_output_config_effort() {
                let mut cfg = test_config(OFFLINE_URL, api_key(), "claude-opus-5-5");
                cfg.effort = Some(Effort::Xhigh);

                let body = capture_stream_body(cfg).await;

                assert_eq!(body["output_config"]["effort"], "xhigh");
            }

            #[tokio::test]
            async fn omits_output_config_without_an_effort() {
                let cfg = test_config(OFFLINE_URL, api_key(), "custom-model");
                assert!(cfg.effort.is_none(), "precondition: effort unset");

                let body = capture_stream_body(cfg).await;

                assert!(
                    body.get("output_config").is_none(),
                    "output_config absent: {body}",
                );
            }

            #[tokio::test]
            async fn context_management_body_present_on_every_known_model() {
                // The body directive must accompany the beta `compute_betas` sends for these.
                for info in crate::model::MODELS {
                    let cfg = test_config(OFFLINE_URL, api_key(), info.id);
                    let body = capture_stream_body(cfg).await;
                    let edits = body["context_management"]["edits"]
                        .as_array()
                        .unwrap_or_else(|| {
                            panic!("context_management.edits missing for {}", info.id)
                        });
                    assert_eq!(edits.len(), 1, "{}", info.id);
                    assert_eq!(edits[0]["type"], "clear_thinking_20251015", "{}", info.id);
                    assert_eq!(edits[0]["keep"], "all", "{}", info.id);
                }
            }

            #[tokio::test]
            async fn hidden_thinking_requests_updates_where_supported_and_omits_it_elsewhere() {
                for (id, display, sends_beta) in [
                    ("claude-opus-5-5", "updates", true),
                    ("claude-haiku-5-5", "omitted", false),
                ] {
                    let client = test_client(OFFLINE_URL, api_key(), id);
                    let (body, betas) = capture_stream_turn(client).await;
                    assert_eq!(body["thinking"]["display"], display, "{id}");
                    assert_eq!(
                        betas.contains("thinking-display-updates-2026-08-18"),
                        sends_beta,
                        "{id}: {betas}",
                    );
                }
            }

            #[tokio::test]
            async fn forwards_thinking_text_only_when_the_request_asked_for_it() {
                let body = sse_body(&[
                    serde_json::json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {"type": "thinking", "thinking": "", "signature": "sig"},
                    }),
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {"type": "thinking_delta", "thinking": "Plan."},
                    }),
                    serde_json::json!({"type": "message_stop"}),
                ]);
                for (id, show_thinking, forwards) in [
                    ("claude-haiku-5-5", false, false),
                    ("claude-haiku-5-5", true, true),
                    ("claude-opus-5-5", false, true),
                    ("custom-model", false, false),
                    ("custom-model", true, true),
                ] {
                    let server = serve(vec![sse_response(body.clone())]).await;
                    let mut cfg = test_config(server.uri(), api_key(), id);
                    cfg.show_thinking = show_thinking;

                    let events = stream_hi(&client_for(&cfg)).await.unwrap();

                    assert_eq!(
                        events.contains(&ResponseEvent::Thinking("Plan.".to_owned())),
                        forwards,
                        "{id} show_thinking={show_thinking}"
                    );
                }
            }

            #[tokio::test]
            async fn known_model_drops_stale_thinking_blocks_under_binding_beta() {
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
        }

        mod prepare {
            use super::*;

            #[test]
            fn hashes_the_billing_block_only_on_first_party_requests() {
                let messages = [Message::user("hi")];
                for kind in [RequestKind::Agentic, RequestKind::SideQuery] {
                    for is_first_party in [true, false] {
                        let mut client = test_client(OFFLINE_URL, api_key(), TEST_MODEL);
                        client.is_first_party = is_first_party;

                        let request = client
                            .prepare(RequestSpec {
                                kind,
                                model: client.model(),
                                thinking: None,
                                max_tokens: 40,
                                system: &[],
                                tools: None,
                                output_config: None,
                                messages: RequestMessages {
                                    user_context: None,
                                    messages: &messages,
                                    breakpoint: None,
                                },
                            })
                            .unwrap();

                        let body: serde_json::Value =
                            serde_json::from_slice(&request.body).unwrap();
                        let block = body["system"][0]["text"].as_str().unwrap();
                        let (prefix, cch) = block.split_once(" cch=").unwrap_or((block, ""));
                        assert!(prefix.ends_with("; cc_entrypoint=cli;"), "{block}");
                        let hash = cch.strip_suffix(';').unwrap_or(cch);
                        let hashed = hash.len() == 5
                            && hash != "00000"
                            && u32::from_str_radix(hash, 16).is_ok();
                        assert_eq!(hashed, is_first_party, "{kind:?} {block}");
                        assert_eq!(cch.is_empty(), !is_first_party, "{kind:?} {block}");
                    }
                }
            }
        }
    }

    mod normalize_platform {
        use super::*;

        #[test]
        fn maps_known_and_falls_back_to_unknown() {
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
    }

    mod normalize_arch {
        use super::*;

        #[test]
        fn maps_known_and_falls_back_to_unknown() {
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
    }

    mod first_user_text {
        use super::*;

        #[test]
        fn extracts_from_first_user_message() {
            let messages = vec![Message::user("hello world"), Message::assistant("hi")];
            assert_eq!(first_user_text(&messages), "hello world");
        }

        #[test]
        fn is_empty_when_absent() {
            let tool_only = Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "id".to_owned(),
                    content: "result".to_owned(),
                    is_error: false,
                }],
            };

            assert_eq!(first_user_text(&[]), "");
            assert_eq!(first_user_text(&[Message::assistant("hi")]), "");
            assert_eq!(first_user_text(&[tool_only]), "");
        }
    }

    mod build_metadata {
        use super::*;

        #[test]
        fn wraps_ids_in_stringified_json_with_canonical_field_order() {
            // Claude Code's field order is `device_id, account_uuid, session_id`, which
            // `serde_json::json!` would alphabetize.
            let meta = build_metadata("dev-1", "abc-123");
            assert_eq!(
                meta.user_id,
                r#"{"device_id":"dev-1","account_uuid":"","session_id":"abc-123"}"#,
            );
        }
    }

    mod build_system_blocks {
        use super::*;

        #[test]
        fn orders_billing_then_identity_then_extras() {
            let cache = CacheControl::static_prefix(true, PromptCacheTtl::OneHour);

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
        fn drops_empty_extras_and_omits_billing_when_absent() {
            let blocks = build_system_blocks(None, &[("", None), ("only-content", None)]);

            assert_eq!(
                serde_json::to_value(blocks).unwrap(),
                serde_json::json!([
                    {"type": "text", "text": SYSTEM_PROMPT_PREFIX},
                    {"type": "text", "text": "only-content"},
                ]),
            );
        }
    }
}
