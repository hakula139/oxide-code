//! Anthropic Messages API streaming client.
//!
//! [`Client::stream_message`] drives the main agent loop: assembles
//! the request (identity prefix, billing attestation for OAuth,
//! static / dynamic system-block split for cache reuse), POSTs
//! `/v1/messages` with SSE streaming, and forwards parsed
//! [`StreamEvent`]s on an mpsc channel. [`Client::complete`] covers
//! non-streaming one-shots (title generation today, future
//! classifiers) with optional JSON-schema-constrained output.
//!
//! Per-request `anthropic-beta` headers are computed from the model's
//! [`crate::model::Capabilities`] via [`compute_betas`], so gateways
//! that reject unsupported betas (Haiku, subscriptions without 1M)
//! don't 400 on spurious feature flags.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::debug;
use uuid::Uuid;

use super::billing;
use crate::config::{Auth, Config, ThinkingConfig};
use crate::message::{ContentBlock, Message, Role};
use crate::prompt::SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
use crate::tool::ToolDefinition;

const API_VERSION: &str = "2023-06-01";
const CLAUDE_CODE_BETA_HEADER: &str = "claude-code-20250219";
const CONTEXT_1M_BETA_HEADER: &str = "context-1m-2025-08-07";
const CONTEXT_MANAGEMENT_BETA_HEADER: &str = "context-management-2025-06-27";
const EFFORT_BETA_HEADER: &str = "effort-2025-11-24";
const INTERLEAVED_THINKING_BETA_HEADER: &str = "interleaved-thinking-2025-05-14";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const PROMPT_CACHING_SCOPE_BETA_HEADER: &str = "prompt-caching-scope-2026-01-05";
const STRUCTURED_OUTPUTS_BETA_HEADER: &str = "structured-outputs-2025-12-15";

/// Matches the installed Claude Code version.
const CLAUDE_CLI_VERSION: &str = "2.1.101";

/// OAuth-required identity prefix. The Anthropic API returns 429 for non-Haiku
/// models with OAuth tokens unless the system prompt starts with this exact
/// string in its own text block.
const SYSTEM_PROMPT_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

// ── Request types ──

#[derive(Serialize)]
struct CreateMessageRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    metadata: RequestMetadata,
    /// Serialized before `messages` so the billing header's `cch=00000`
    /// placeholder appears first in the JSON, making [`billing::inject_cch`]'s
    /// single-occurrence replacement safe even when tool results contain the
    /// literal placeholder string.
    system: Vec<SystemBlock<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolDefinition]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<&'a ThinkingConfig>,
    /// JSON-schema-constrained output format for one-shot utility calls
    /// (title generation, future classifiers). Must travel alongside the
    /// `structured-outputs-2025-12-15` beta header; both are gated on
    /// `Capabilities::structured_outputs` so unsupported models silently
    /// drop back to free-form text rather than 400ing the gateway.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig<'a>>,
    messages: &'a [Message],
}

/// Wrapper matching the wire shape `output_config.format = {...}`.
#[derive(Serialize)]
struct OutputConfig<'a> {
    format: &'a OutputFormat,
}

/// JSON-schema-constrained completion format. Constructed via
/// [`OutputFormat::json_schema`]; callers typically build one per
/// request shape (e.g., `{"title": string}`) and pass it by reference
/// to [`Client::complete`].
#[derive(Debug, Serialize)]
pub(crate) struct OutputFormat {
    r#type: &'static str,
    schema: serde_json::Value,
}

impl OutputFormat {
    /// Builds a `json_schema` output format from a precomputed schema
    /// value. The schema must already match Anthropic's expectations
    /// (`type: "object"`, `additionalProperties: false`, explicit
    /// `required` array) — we don't validate here.
    pub(crate) fn json_schema(schema: serde_json::Value) -> Self {
        Self {
            r#type: "json_schema",
            schema,
        }
    }
}

/// Top-level `metadata` object on every outbound request.
///
/// `user_id` is a stringified JSON object containing `session_id` (and
/// optionally `device_id` / `account_uuid`). The API receives it as a
/// flat string, not a nested object.
#[derive(Serialize)]
struct RequestMetadata {
    user_id: String,
}

/// A text block in the system prompt array. The Anthropic API accepts `system`
/// as either a string or an array of these blocks. Using the array form lets
/// the identity prefix occupy its own block, which is required for OAuth
/// validation on non-Haiku models.
#[derive(Serialize)]
struct SystemBlock<'a> {
    r#type: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// Prompt caching control. The `scope` field determines the cache sharing
/// level: `"global"` for static content identical across sessions (1P only),
/// `None` for the default org-scoped ephemeral cache (universally accepted).
///
/// `scope: "global"` must be a true prefix of all preceding request content
/// — the server rejects a global-scoped block preceded by a non-global
/// block (including tool definitions, which render before `system`). See
/// [`is_first_party_base_url`] for where the gating decision is made.
#[derive(Serialize)]
struct CacheControl {
    r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'static str>,
}

// ── SSE response types ──

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields are populated by serde and used in downstream matching"
    )
)]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        message: MessageResponse,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockInfo,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiError,
    },
    /// Catch-all for unrecognized event types.
    /// Silently skipped during stream processing.
    #[serde(other)]
    Unknown,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields populated by serde, defined for full SSE protocol coverage"
    )
)]
#[derive(Debug, Clone, Deserialize)]
pub struct MessageResponse {
    pub id: String,
    pub model: String,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockInfo {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    ServerToolUse {
        id: String,
        name: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    /// Catch-all for unrecognized block types.
    /// Silently skipped during stream processing.
    #[serde(other)]
    Unknown,
}

#[expect(
    clippy::enum_variant_names,
    reason = "variant names mirror Anthropic API delta type values (text_delta, input_json_delta, etc.)"
)]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    /// Full signature value (overwrites, not appended).
    SignatureDelta {
        signature: String,
    },
    /// Catch-all for unrecognized delta types.
    /// Silently skipped during stream processing.
    #[serde(other)]
    Unknown,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields populated by serde, defined for full SSE protocol coverage"
    )
)]
#[derive(Debug, Clone, Deserialize)]
pub struct MessageDeltaBody {
    pub stop_reason: Option<String>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields populated by serde, defined for full SSE protocol coverage"
    )
)]
#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

// ── Client ──

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    config: Config,
    session_id: String,
}

impl Client {
    pub fn new(config: Config, session_id: Option<String>) -> Result<Self> {
        let session_id = session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
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
        // type — see [`compute_betas`].
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
        })
    }

    /// Returns the model name for use in the system prompt.
    pub fn model(&self) -> &str {
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
    /// Returns an mpsc receiver of [`StreamEvent`]s.
    pub fn stream_message(
        &self,
        messages: &[Message],
        system_sections: &[&str],
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        let messages_with_context: Vec<Message>;
        let effective_messages: &[Message] = if let Some(ctx) = user_context {
            messages_with_context = std::iter::once(Message::user(ctx))
                .chain(messages.iter().cloned())
                .collect();
            &messages_with_context
        } else {
            messages
        };

        let billing_header = matches!(self.config.auth, Auth::OAuth(_)).then(|| {
            let fingerprint = billing::compute_fingerprint(
                first_user_text(effective_messages),
                CLAUDE_CLI_VERSION,
            );
            billing::build_billing_header(CLAUDE_CLI_VERSION, &fingerprint)
        });

        // Global-scope prompt caching only fires on the official API —
        // third-party gateways reject `scope: "global"` on a system block
        // because tool definitions render first and taint the cache
        // prefix. On 3P we fall back to the default (org-scoped)
        // ephemeral cache, which every gateway accepts.
        let is_first_party = is_first_party_base_url(&self.config.base_url);

        // System-block order (boundary marker filtered):
        //
        // 1. Billing header (OAuth only; no cache_control).
        // 2. Identity prefix (no cache_control).
        // 3. Static sections joined (ephemeral cache; scope=global on 1P,
        //    default org-scoped on 3P).
        // 4. Dynamic sections joined (no cache_control).
        let (static_sections, dynamic_sections) = split_at_boundary(system_sections);
        let static_joined = static_sections.join("\n\n");
        let dynamic_joined = dynamic_sections.join("\n\n");

        let mut system_blocks = Vec::new();
        if let Some(ref header) = billing_header {
            system_blocks.push(SystemBlock {
                r#type: "text",
                text: header,
                cache_control: None,
            });
        }
        system_blocks.push(SystemBlock {
            r#type: "text",
            text: SYSTEM_PROMPT_PREFIX,
            cache_control: None,
        });
        if !static_joined.is_empty() {
            system_blocks.push(SystemBlock {
                r#type: "text",
                text: &static_joined,
                cache_control: Some(static_prefix_cache_control(is_first_party)),
            });
        }
        if !dynamic_joined.is_empty() {
            system_blocks.push(SystemBlock {
                r#type: "text",
                text: &dynamic_joined,
                cache_control: None,
            });
        }

        let url = format!("{}/v1/messages?beta=true", self.config.base_url);
        let mut body = serde_json::to_string(&CreateMessageRequest {
            // `[1m]` is a client-side tag; strip before the wire.
            model: api_model_id(&self.config.model),
            max_tokens: self.config.max_tokens,
            stream: true,
            metadata: self.build_metadata(),
            system: system_blocks,
            tools: (!tools.is_empty()).then_some(tools),
            thinking: self.config.thinking.as_ref(),
            output_config: None,
            messages: effective_messages,
        })
        .context("failed to serialize request")?;

        if billing_header.is_some() {
            body = billing::inject_cch(&body);
        }

        debug!(body_len = body.len(), "sending API request");

        let (tx, rx) = mpsc::channel(64);
        let http = self.http.clone();
        let betas = compute_betas(
            &self.config.model,
            &self.config.auth,
            true,
            false,
            is_first_party,
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

    /// Non-streaming completion, used for one-shot utility calls (AI
    /// title generation, future classifiers). Returns the concatenated
    /// text of the assistant's reply; non-text blocks are filtered out.
    ///
    /// `output_format` constrains the reply to a JSON schema via the
    /// `structured-outputs-2025-12-15` beta. On models whose
    /// [`Capabilities::structured_outputs`][crate::model::Capabilities::structured_outputs]
    /// is `false`, both the body field and the beta are silently
    /// dropped — the caller must tolerate free-form text in that case.
    pub async fn complete(
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
            &self.session_id,
            effective_format,
        )?;

        let url = format!("{}/v1/messages?beta=true", self.config.base_url);
        debug!(model, body_len = body.len(), "sending completion request");

        // Non-agentic one-shot — 1P gating only affects the
        // `prompt-caching-scope` beta, which `compute_betas` restricts
        // to the agentic branch anyway. Still passed for signature
        // symmetry with [`Self::stream_message`].
        let is_first_party = is_first_party_base_url(&self.config.base_url);
        let betas = compute_betas(
            model,
            &self.config.auth,
            false,
            effective_format.is_some(),
            is_first_party,
        )
        .join(",");
        let response = self
            .http
            .post(&url)
            .header("anthropic-beta", betas)
            .body(body)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("API error (HTTP {status}): {body}");
        }

        let CompletionResponse { content } = response
            .json()
            .await
            .context("failed to parse completion response")?;
        Ok(join_text_blocks(content))
    }

    /// Builds the `metadata.user_id` field as a stringified JSON object.
    fn build_metadata(&self) -> RequestMetadata {
        build_metadata(&self.session_id)
    }
}

/// Shared `metadata.user_id` builder. Kept at file scope so
/// [`build_completion_body`] can call it without pulling in `&Client`.
fn build_metadata(session_id: &str) -> RequestMetadata {
    let user_id = serde_json::json!({ "session_id": session_id }).to_string();
    RequestMetadata { user_id }
}

/// Computes the `anthropic-beta` header value for a request. Each beta
/// is gated on a [`Capabilities`][crate::model::Capabilities] flag, so
/// adding or bumping a model only means editing the lookup table.
///
/// `is_agentic` gates agent-only betas on the streaming chat path
/// (keeps one-shot calls like title generation minimal).
/// `want_structured` is cross-checked against the model's capability
/// flag so an unsupported `[OutputFormat]` silently drops back to
/// free-form text instead of 400ing the gateway.
/// `is_first_party` gates experimental betas that 3P proxies reject
/// (currently: `prompt-caching-scope`, which is a no-op without the
/// scope field it enables).
fn compute_betas(
    model: &str,
    auth: &Auth,
    is_agentic: bool,
    want_structured: bool,
    is_first_party: bool,
) -> Vec<&'static str> {
    let caps = crate::model::lookup(model)
        .map(|info| info.capabilities)
        .unwrap_or_default();
    let is_haiku = model.to_lowercase().contains("haiku");

    // Order mirrors `docs/research/anthropic-api.md` → Per-model beta
    // sets: identity / auth → universal agentic → capability-gated.
    let mut out = Vec::with_capacity(8);

    // Gateway tag: required for non-Haiku OAuth on 1P (429 without it).
    // Non-agentic Haiku one-shots skip it; agentic Haiku re-adds it.
    if !is_haiku || is_agentic {
        out.push(CLAUDE_CODE_BETA_HEADER);
    }
    if matches!(auth, Auth::OAuth(_)) {
        out.push(OAUTH_BETA_HEADER);
    }

    if is_agentic {
        if caps.context_management {
            out.push(CONTEXT_MANAGEMENT_BETA_HEADER);
        }
        // Prompt-caching scope is the beta that enables `scope: "global"`
        // on `cache_control`. Ship it only when we actually send the
        // scope field — i.e., on the 1P API. 3P gateways reject the
        // scope (tools taint the cache prefix) and the beta without
        // scope is a no-op.
        if is_first_party {
            out.push(PROMPT_CACHING_SCOPE_BETA_HEADER);
        }
        if caps.interleaved_thinking {
            out.push(INTERLEAVED_THINKING_BETA_HEADER);
        }
        if caps.effort {
            out.push(EFFORT_BETA_HEADER);
        }
    }

    // 1M context is explicit user opt-in via the `[1m]` model suffix.
    // Family-based auto-enable would break subscriptions without 1M
    // access, so we require the tag and cross-check the capability.
    if has_1m_tag(model) && caps.context_1m {
        out.push(CONTEXT_1M_BETA_HEADER);
    }
    // Structured outputs is one-shot only — streaming turns are free-form.
    if want_structured && caps.structured_outputs {
        out.push(STRUCTURED_OUTPUTS_BETA_HEADER);
    }

    out
}

/// Whether the target model accepts the `structured-outputs-2025-12-15`
/// beta. Thin wrapper over the capability table for pre-checks.
pub(crate) fn supports_structured_outputs(model: &str) -> bool {
    crate::model::lookup(model).is_some_and(|info| info.capabilities.structured_outputs)
}

/// Whether `base_url` points at the first-party Anthropic API, gating
/// features that strict 3P proxies reject (currently: global-scope
/// prompt caching + its beta header).
///
/// An unparsable URL, a URL with no host, or any host other than the
/// 1P list is treated as third-party; the safe fallback (drop scope +
/// beta) preserves org-level ephemeral caching, which every gateway
/// accepts.
fn is_first_party_base_url(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|h| {
            matches!(
                h.as_str(),
                "api.anthropic.com" | "api-staging.anthropic.com"
            )
        })
}

/// Cache-control for the static system-prompt prefix. On 1P, emit the
/// global scope so the prefix is shared across sessions; on 3P, fall
/// back to the default (org-scoped) ephemeral cache — 3P gateways
/// reject `scope: "global"` because tool definitions render first and
/// taint the cache prefix.
fn static_prefix_cache_control(is_first_party: bool) -> CacheControl {
    CacheControl {
        r#type: "ephemeral",
        scope: is_first_party.then_some("global"),
    }
}

/// Strips the `[1m]` tag from a caller-supplied model string. The tag
/// is a client-side convention; the API rejects it on the wire.
fn api_model_id(model: &str) -> &str {
    tag_offset(model).map_or(model, |i| model[..i].trim_end())
}

/// Whether `model` carries the `[1m]` tag — an explicit user opt-in
/// to the 1M-context window (auto-gating on family would 400 on
/// subscriptions without 1M access).
fn has_1m_tag(model: &str) -> bool {
    tag_offset(model).is_some()
}

/// Byte offset of the `[1m]` tag, case-insensitive. Shared by
/// [`has_1m_tag`] and [`api_model_id`] so the two agree on every
/// accepted spelling. Model IDs are ASCII, so lowercased byte indices
/// line up with the original string.
fn tag_offset(model: &str) -> Option<usize> {
    model.to_lowercase().find("[1m]")
}

/// Serializes the JSON request body for [`Client::complete`].
///
/// System block order matches [`Client::stream_message`]:
///
/// 1. Billing header (OAuth only; injected with `cch`).
/// 2. Identity prefix (required for non-Haiku OAuth).
/// 3. Caller-supplied system prompt (omitted when empty).
///
/// The caller is expected to have pre-gated `output_format` against
/// [`supports_structured_outputs`].
fn build_completion_body(
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    auth: &Auth,
    session_id: &str,
    output_format: Option<&OutputFormat>,
) -> Result<String> {
    let messages = [Message::user(user)];

    let billing_header = matches!(auth, Auth::OAuth(_)).then(|| {
        let fingerprint = billing::compute_fingerprint(user, CLAUDE_CLI_VERSION);
        billing::build_billing_header(CLAUDE_CLI_VERSION, &fingerprint)
    });

    let mut system_blocks = Vec::with_capacity(3);
    if let Some(ref header) = billing_header {
        system_blocks.push(SystemBlock {
            r#type: "text",
            text: header,
            cache_control: None,
        });
    }
    system_blocks.push(SystemBlock {
        r#type: "text",
        text: SYSTEM_PROMPT_PREFIX,
        cache_control: None,
    });
    if !system.is_empty() {
        system_blocks.push(SystemBlock {
            r#type: "text",
            text: system,
            cache_control: None,
        });
    }

    let mut body = serde_json::to_string(&CreateMessageRequest {
        // `[1m]` is a client-side tag; strip before the wire.
        model: api_model_id(model),
        max_tokens,
        stream: false,
        metadata: build_metadata(session_id),
        system: system_blocks,
        tools: None,
        thinking: None,
        output_config: output_format.map(|format| OutputConfig { format }),
        messages: &messages,
    })
    .context("failed to serialize request")?;

    if billing_header.is_some() {
        body = billing::inject_cch(&body);
    }
    Ok(body)
}

/// Flattens a `messages.create` response's content array into the
/// assistant's user-visible text (drops tool-use / thinking blocks).
fn join_text_blocks(content: Vec<ContentBlock>) -> String {
    content
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect()
}

/// Shape we accept back from the non-streaming `/v1/messages` endpoint.
/// The API sends many more fields (`id`, `role`, `model`, `stop_reason`,
/// `usage`); we only care about the content blocks — serde ignores the rest.
#[derive(Deserialize)]
struct CompletionResponse {
    content: Vec<ContentBlock>,
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

/// Hard cap on the unterminated SSE frame buffer. A misbehaving upstream that
/// never emits `\n\n` would otherwise let `buf` grow without bound until OOM.
const MAX_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;

async fn stream_sse(
    http: &reqwest::Client,
    url: &str,
    betas: String,
    body: String,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let response = http
        .post(url)
        .header("anthropic-beta", betas)
        .body(body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("API error (HTTP {status}): {body}");
    }

    let mut stream = response.bytes_stream();
    // Byte buffer: reassembles UTF-8 sequences split across chunk boundaries
    // intact (`from_utf8_lossy` would inject U+FFFD at the boundary).
    let mut buf: Vec<u8> = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("error reading response stream")?;
        buf.extend_from_slice(&chunk);

        loop {
            let Some(end) = buf.windows(2).position(|w| w == b"\n\n") else {
                if buf.len() > MAX_SSE_FRAME_BYTES {
                    bail!(
                        "SSE frame buffer exceeded {MAX_SSE_FRAME_BYTES} bytes without \
                         a terminating blank line; upstream may be misbehaving"
                    );
                }
                break;
            };
            let frame_bytes: Vec<u8> = buf.drain(..end + 2).take(end).collect();

            let frame = match std::str::from_utf8(&frame_bytes) {
                Ok(s) => s,
                Err(e) => {
                    debug!("skipping invalid UTF-8 SSE frame: {e}");
                    continue;
                }
            };

            // A single malformed frame should not poison the whole turn.
            match parse_sse_frame(frame) {
                Ok(Some(event)) => {
                    if tx.send(Ok(event)).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => {}
                Err(e) => debug!("skipping malformed SSE frame: {e:#}"),
            }
        }
    }

    Ok(())
}

/// Parses a single SSE frame into a [`StreamEvent`].
///
/// Per the SSE spec, multiple `data:` lines concatenate with `\n`.
/// Anthropic currently emits single-line data, but we follow the spec
/// so a future multi-line payload doesn't silently lose everything
/// but the last line.
fn parse_sse_frame(frame: &str) -> Result<Option<StreamEvent>> {
    let mut data_lines: Vec<&str> = Vec::new();

    for line in frame.lines() {
        if let Some(value) = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
        {
            data_lines.push(value);
        }
    }

    let data: std::borrow::Cow<'_, str> = match data_lines.as_slice() {
        [] => return Ok(None),
        [one] => std::borrow::Cow::Borrowed(*one),
        _ => std::borrow::Cow::Owned(data_lines.join("\n")),
    };

    let event: StreamEvent =
        serde_json::from_str(&data).with_context(|| format!("failed to parse SSE data: {data}"))?;

    Ok(Some(event))
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
    Config {
        auth,
        model: model.to_owned(),
        base_url: base_url.into(),
        max_tokens: 128,
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

    use indoc::indoc;
    use wiremock::matchers::{header, header_regex, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;

    // ── Fixtures ──

    const OFFLINE_URL: &str = "https://example.invalid";
    const TEST_MODEL: &str = "claude-sonnet-4-6";

    fn api_key() -> Auth {
        Auth::ApiKey("k".to_owned())
    }

    fn oauth() -> Auth {
        Auth::OAuth("t".to_owned())
    }

    /// Concatenates SSE frames into a valid response body, each
    /// followed by the required `\n\n` terminator.
    fn sse_body(frames: &[&str]) -> String {
        let mut body = String::new();
        for f in frames {
            body.push_str(f);
            body.push_str("\n\n");
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
            r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-6","usage":{"input_tokens":5,"output_tokens":0}}}"#,
            r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#,
            r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
            r#"event: message_stop
data: {"type":"message_stop"}"#,
        ])
    }

    /// Slot for the last request body captured by a wiremock responder.
    type Captured<T> = Arc<Mutex<Option<T>>>;

    fn captured<T>() -> Captured<T> {
        Arc::new(Mutex::new(None))
    }

    // ── OutputFormat ──

    #[test]
    fn output_format_json_schema_serializes_with_type_and_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"],
        });
        let fmt = OutputFormat::json_schema(schema.clone());
        let v = serde_json::to_value(&fmt).unwrap();
        assert_eq!(v["type"], "json_schema");
        assert_eq!(v["schema"], schema);
    }

    // ── StreamEvent ──

    #[test]
    fn stream_event_content_block_start_text() {
        let json =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let StreamEvent::ContentBlockStart {
            index,
            content_block,
        } = serde_json::from_str(json).unwrap()
        else {
            panic!("expected ContentBlockStart");
        };
        assert_eq!(index, 0);
        assert!(matches!(content_block, ContentBlockInfo::Text { text } if text.is_empty()));
    }

    #[test]
    fn stream_event_content_block_stop() {
        let json = r#"{"type":"content_block_stop","index":2}"#;
        let StreamEvent::ContentBlockStop { index } = serde_json::from_str(json).unwrap() else {
            panic!("expected ContentBlockStop");
        };
        assert_eq!(index, 2);
    }

    #[test]
    fn stream_event_message_delta_with_usage() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#;
        let StreamEvent::MessageDelta { delta, usage } = serde_json::from_str(json).unwrap() else {
            panic!("expected MessageDelta");
        };
        assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
        let usage = usage.expect("expected usage");
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 42);
    }

    #[test]
    fn stream_event_message_stop() {
        let event: StreamEvent = serde_json::from_str(r#"{"type":"message_stop"}"#).unwrap();
        assert!(matches!(event, StreamEvent::MessageStop));
    }

    // ── ContentBlockInfo ──

    #[test]
    fn content_block_info_text() {
        let json = r#"{"type":"text","text":"Hello world"}"#;
        let ContentBlockInfo::Text { text } = serde_json::from_str(json).unwrap() else {
            panic!("expected Text");
        };
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn content_block_info_tool_use() {
        let json = r#"{"type":"tool_use","id":"toolu_01","name":"bash"}"#;
        let ContentBlockInfo::ToolUse { id, name } = serde_json::from_str(json).unwrap() else {
            panic!("expected ToolUse");
        };
        assert_eq!(id, "toolu_01");
        assert_eq!(name, "bash");
    }

    #[test]
    fn content_block_info_server_tool_use() {
        let json = r#"{"type":"server_tool_use","id":"stu_01","name":"advisor"}"#;
        let ContentBlockInfo::ServerToolUse { id, name } = serde_json::from_str(json).unwrap()
        else {
            panic!("expected ServerToolUse");
        };
        assert_eq!(id, "stu_01");
        assert_eq!(name, "advisor");
    }

    #[test]
    fn content_block_info_thinking() {
        let json = r#"{"type":"thinking","thinking":"Let me analyze this","signature":"sig_xyz"}"#;
        let ContentBlockInfo::Thinking {
            thinking,
            signature,
        } = serde_json::from_str(json).unwrap()
        else {
            panic!("expected Thinking");
        };
        assert_eq!(thinking, "Let me analyze this");
        assert_eq!(signature, "sig_xyz");
    }

    #[test]
    fn content_block_info_redacted_thinking() {
        let json = r#"{"type":"redacted_thinking","data":"base64data=="}"#;
        let ContentBlockInfo::RedactedThinking { data } = serde_json::from_str(json).unwrap()
        else {
            panic!("expected RedactedThinking");
        };
        assert_eq!(data, "base64data==");
    }

    #[test]
    fn content_block_info_unknown_type() {
        let json = r#"{"type":"some_future_block","data":"opaque"}"#;
        let info: ContentBlockInfo = serde_json::from_str(json).unwrap();
        assert!(matches!(info, ContentBlockInfo::Unknown));
    }

    // ── Delta ──

    #[test]
    fn delta_text() {
        let Delta::TextDelta { text } =
            serde_json::from_str(r#"{"type":"text_delta","text":"Hello"}"#).unwrap()
        else {
            panic!("expected TextDelta");
        };
        assert_eq!(text, "Hello");
    }

    #[test]
    fn delta_input_json() {
        let Delta::InputJsonDelta { partial_json } =
            serde_json::from_str(r#"{"type":"input_json_delta","partial_json":"{\"key\":"}"#)
                .unwrap()
        else {
            panic!("expected InputJsonDelta");
        };
        assert_eq!(partial_json, r#"{"key":"#);
    }

    #[test]
    fn delta_thinking() {
        let Delta::ThinkingDelta { thinking } =
            serde_json::from_str(r#"{"type":"thinking_delta","thinking":"partial reasoning"}"#)
                .unwrap()
        else {
            panic!("expected ThinkingDelta");
        };
        assert_eq!(thinking, "partial reasoning");
    }

    #[test]
    fn delta_signature() {
        let Delta::SignatureDelta { signature } =
            serde_json::from_str(r#"{"type":"signature_delta","signature":"sig_abc123"}"#).unwrap()
        else {
            panic!("expected SignatureDelta");
        };
        assert_eq!(signature, "sig_abc123");
    }

    #[test]
    fn delta_unknown_type() {
        let json = r#"{"type":"some_future_delta","data":"opaque"}"#;
        let delta: Delta = serde_json::from_str(json).unwrap();
        assert!(matches!(delta, Delta::Unknown));
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
        assert!(matches!(
            events[5],
            StreamEvent::MessageStop | StreamEvent::Unknown,
        ));
    }

    #[tokio::test]
    async fn stream_message_preserves_multibyte_codepoints_in_deltas() {
        // Pins the byte-level SSE buffer: a chunk decoded as lossy UTF-8
        // would mangle a 4-byte emoji split across TCP chunk boundaries.
        let server = MockServer::start().await;
        let body = sse_body(&[
            r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"🦀rust"}}"#,
            r#"event: message_stop
data: {"type":"message_stop"}"#,
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
            r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r"event: content_block_delta
data: {not valid json",
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            r#"event: message_stop
data: {"type":"message_stop"}"#,
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
        let body = sse_body(&[r#"event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"Servers overloaded"}}"#]);
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
            !beta.contains(PROMPT_CACHING_SCOPE_BETA_HEADER),
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
    }

    // ── Client::complete ──

    #[tokio::test]
    async fn complete_happy_path_returns_assistant_text() {
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
    async fn complete_structured_output_gated_by_model_capability() {
        // Supported model → body carries output_config, header carries
        // the beta tag. Unsupported model → both are silently dropped
        // (mirrors the `[1m]` × `context_1m` cross-check).
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
        // Non-agentic Haiku drops the `claude-code-20250219` gateway tag
        // (1P / 3P both tolerate its absence for Haiku one-shots) while
        // still carrying the OAuth billing attestation.
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

    // ── compute_betas ──

    #[test]
    fn compute_betas_agentic_opus_4_6_plain_carries_full_set_except_1m() {
        // Plain model (no `[1m]` tag) must not auto-enable 1M context —
        // a gateway without 1M access would 400.
        let betas = compute_betas("claude-opus-4-6", &api_key(), true, false, true);
        assert!(betas.contains(&CLAUDE_CODE_BETA_HEADER));
        assert!(betas.contains(&INTERLEAVED_THINKING_BETA_HEADER));
        assert!(betas.contains(&CONTEXT_MANAGEMENT_BETA_HEADER));
        assert!(betas.contains(&EFFORT_BETA_HEADER));
        assert!(betas.contains(&PROMPT_CACHING_SCOPE_BETA_HEADER));
        assert!(!betas.contains(&CONTEXT_1M_BETA_HEADER));
        assert!(!betas.contains(&OAUTH_BETA_HEADER));
        assert!(!betas.contains(&STRUCTURED_OUTPUTS_BETA_HEADER));
    }

    #[test]
    fn compute_betas_opus_4_6_with_1m_tag_adds_context_1m() {
        let betas = compute_betas("claude-opus-4-6[1m]", &api_key(), true, false, true);
        assert!(betas.contains(&CONTEXT_1M_BETA_HEADER));
        assert!(betas.contains(&EFFORT_BETA_HEADER));
    }

    #[test]
    fn compute_betas_oauth_adds_oauth_header() {
        let betas = compute_betas("claude-opus-4-6", &oauth(), true, false, true);
        assert!(betas.contains(&OAUTH_BETA_HEADER));
    }

    #[test]
    fn compute_betas_sonnet_4_5_has_thinking_but_not_effort() {
        // Sonnet 4.5 supports interleaved thinking but not effort;
        // plain (no `[1m]` tag) means no 1M beta either.
        let betas = compute_betas("claude-sonnet-4-5", &api_key(), true, false, true);
        assert!(betas.contains(&INTERLEAVED_THINKING_BETA_HEADER));
        assert!(betas.contains(&CONTEXT_MANAGEMENT_BETA_HEADER));
        assert!(!betas.contains(&CONTEXT_1M_BETA_HEADER));
        assert!(!betas.contains(&EFFORT_BETA_HEADER));
    }

    #[test]
    fn compute_betas_haiku_4_5_agentic_omits_1m_effort_and_thinking() {
        // Haiku has a 200K window and no interleaved-thinking / effort
        // support on 3P gateways; all three must be absent.
        let betas = compute_betas("claude-haiku-4-5", &api_key(), true, false, true);
        assert!(betas.contains(&CLAUDE_CODE_BETA_HEADER));
        assert!(betas.contains(&CONTEXT_MANAGEMENT_BETA_HEADER));
        assert!(!betas.contains(&CONTEXT_1M_BETA_HEADER));
        assert!(!betas.contains(&INTERLEAVED_THINKING_BETA_HEADER));
        assert!(!betas.contains(&EFFORT_BETA_HEADER));
    }

    #[test]
    fn compute_betas_haiku_4_5_with_1m_tag_silently_drops_1m() {
        let betas = compute_betas("claude-haiku-4-5[1m]", &api_key(), true, false, true);
        assert!(!betas.contains(&CONTEXT_1M_BETA_HEADER));
    }

    #[test]
    fn compute_betas_haiku_non_agentic_minimal() {
        // Title-generator one-shot on API key → no agent tags, no gateway
        // tag. OAuth one-shot → only the OAuth tag.
        assert_eq!(
            compute_betas("claude-haiku-4-5", &api_key(), false, false, true),
            Vec::<&str>::new(),
        );
        assert_eq!(
            compute_betas("claude-haiku-4-5", &oauth(), false, false, true),
            vec![OAUTH_BETA_HEADER],
        );
    }

    #[test]
    fn compute_betas_non_haiku_non_agentic_keeps_claude_code_tag() {
        // OAuth on non-Haiku requires the gateway tag even for one-shots.
        let betas = compute_betas("claude-sonnet-4-6", &oauth(), false, false, true);
        assert!(betas.contains(&CLAUDE_CODE_BETA_HEADER));
        assert!(betas.contains(&OAUTH_BETA_HEADER));
        assert!(!betas.contains(&PROMPT_CACHING_SCOPE_BETA_HEADER));
        assert!(!betas.contains(&INTERLEAVED_THINKING_BETA_HEADER));
    }

    #[test]
    fn compute_betas_opus_4_7_matches_opus_4_6_family() {
        let plain = compute_betas("claude-opus-4-7", &api_key(), true, false, true);
        assert!(plain.contains(&INTERLEAVED_THINKING_BETA_HEADER));
        assert!(plain.contains(&CONTEXT_MANAGEMENT_BETA_HEADER));
        assert!(plain.contains(&EFFORT_BETA_HEADER));
        assert!(!plain.contains(&CONTEXT_1M_BETA_HEADER));

        let with_1m = compute_betas("claude-opus-4-7[1m]", &api_key(), true, false, true);
        assert!(with_1m.contains(&CONTEXT_1M_BETA_HEADER));
    }

    #[test]
    fn compute_betas_structured_outputs_gated_by_model_capability() {
        // Haiku 4.5 supports it → emitted alone on non-agentic API key.
        // Haiku 4 base predates the beta → silently dropped.
        assert_eq!(
            compute_betas("claude-haiku-4-5", &api_key(), false, true, true),
            vec![STRUCTURED_OUTPUTS_BETA_HEADER],
        );
        assert!(
            !compute_betas("claude-haiku-4", &api_key(), false, true, true)
                .contains(&STRUCTURED_OUTPUTS_BETA_HEADER),
        );
    }

    #[test]
    fn compute_betas_third_party_base_url_drops_prompt_caching_scope() {
        // 3P gateways reject `scope: "global"` because tool definitions
        // render before system blocks and taint the cache prefix. Keep
        // every other agentic beta — only the scope header goes.
        let betas = compute_betas("claude-opus-4-7", &api_key(), true, false, false);
        assert!(!betas.contains(&PROMPT_CACHING_SCOPE_BETA_HEADER));
        assert!(betas.contains(&CLAUDE_CODE_BETA_HEADER));
        assert!(betas.contains(&CONTEXT_MANAGEMENT_BETA_HEADER));
        assert!(betas.contains(&INTERLEAVED_THINKING_BETA_HEADER));
        assert!(betas.contains(&EFFORT_BETA_HEADER));
    }

    // ── supports_structured_outputs ──

    #[test]
    fn supports_structured_outputs_reflects_capability_table() {
        assert!(supports_structured_outputs("claude-haiku-4-5"));
        assert!(supports_structured_outputs("claude-opus-4-7"));
        assert!(!supports_structured_outputs("claude-haiku-4"));
        assert!(!supports_structured_outputs("claude-opus-5-0"));
    }

    // ── is_first_party_base_url ──

    #[test]
    fn is_first_party_base_url_accepts_official_hosts() {
        assert!(is_first_party_base_url("https://api.anthropic.com"));
        assert!(is_first_party_base_url("https://api.anthropic.com/"));
        assert!(is_first_party_base_url("https://api-staging.anthropic.com"));
        // Case-insensitive on the host (URL spec lowercases it).
        assert!(is_first_party_base_url("https://API.ANTHROPIC.COM"));
    }

    #[test]
    fn is_first_party_base_url_rejects_proxies_and_malformed_urls() {
        // Proxies and self-hosted gateways → 3P. Also anything that
        // doesn't parse as a URL falls through to the safe default.
        assert!(!is_first_party_base_url("https://api.openai.com"));
        assert!(!is_first_party_base_url("https://proxy.example.com"));
        assert!(!is_first_party_base_url("https://anthropic.com.evil.io"));
        assert!(!is_first_party_base_url("http://127.0.0.1:8080"));
        assert!(!is_first_party_base_url(""));
        assert!(!is_first_party_base_url("not-a-url"));
    }

    // ── static_prefix_cache_control ──

    #[test]
    fn static_prefix_cache_control_emits_global_scope_on_first_party_only() {
        // 1P → `{"type":"ephemeral","scope":"global"}` — global cache.
        // 3P → `{"type":"ephemeral"}` — default (org) scope; every
        // gateway accepts this.
        let first = static_prefix_cache_control(true);
        assert_eq!(first.r#type, "ephemeral");
        assert_eq!(first.scope, Some("global"));

        let third = static_prefix_cache_control(false);
        assert_eq!(third.r#type, "ephemeral");
        assert_eq!(third.scope, None);

        // Round-trip through JSON to pin the on-wire shape — the
        // `scope` key must be absent (not `null`) in the 3P case so
        // gateways that validate the field strictly accept it.
        let wire = serde_json::to_string(&third).unwrap();
        assert_eq!(wire, r#"{"type":"ephemeral"}"#);
    }

    // ── api_model_id / has_1m_tag ──

    #[test]
    fn api_model_id_strips_1m_tag_case_insensitively() {
        // Case-insensitive matching keeps `api_model_id` and `has_1m_tag`
        // in sync — a leaked `[1M]` in the API model field would 400.
        assert_eq!(api_model_id("claude-opus-4-7[1m]"), "claude-opus-4-7");
        assert_eq!(api_model_id("claude-opus-4-7[1M]"), "claude-opus-4-7");
        assert_eq!(api_model_id("claude-opus-4-7 [1m]"), "claude-opus-4-7");
        assert_eq!(api_model_id("claude-opus-4-7"), "claude-opus-4-7");
    }

    #[test]
    fn has_1m_tag_is_case_insensitive() {
        assert!(has_1m_tag("claude-opus-4-7[1m]"));
        assert!(has_1m_tag("claude-opus-4-7[1M]"));
        assert!(!has_1m_tag("claude-opus-4-7"));
    }

    // ── build_completion_body ──

    fn parse_body(body: &str) -> serde_json::Value {
        serde_json::from_str(body).expect("serialized body must be valid JSON")
    }

    #[test]
    fn build_completion_body_omits_tools_thinking_and_output_config_by_default() {
        let body =
            build_completion_body("claude-haiku-4-5", "sys", "hi", 40, &api_key(), "sid", None)
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
        // API key: identity prefix + caller's system = 2 blocks, no
        // billing attestation. OAuth: billing + identity + system = 3
        // blocks, `cch=00000` placeholder replaced.
        let api_body = build_completion_body(
            "claude-haiku-4-5",
            "sys-prompt",
            "hi",
            40,
            &api_key(),
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
        // Identity prefix must survive even without a caller-supplied
        // system prompt — non-Haiku OAuth requires it in block 0.
        let body = build_completion_body("claude-haiku-4-5", "", "hi", 40, &api_key(), "sid", None)
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
            "sid",
            Some(&fmt),
        )
        .unwrap();
        let v = parse_body(&body);
        assert_eq!(v["output_config"]["format"]["type"], "json_schema");
        assert_eq!(v["output_config"]["format"]["schema"], schema);
    }

    #[test]
    fn build_completion_body_routes_session_id_into_metadata() {
        let body = build_completion_body(
            "claude-haiku-4-5",
            "",
            "hi",
            40,
            &api_key(),
            "sid-789",
            None,
        )
        .unwrap();
        assert!(
            body.contains("sid-789"),
            "session_id threads into metadata.user_id: {body}",
        );
    }

    // ── join_text_blocks / CompletionResponse ──

    #[test]
    fn join_text_blocks_concatenates_text_and_drops_non_text_blocks() {
        // Round-trips through the real `CompletionResponse` deserializer
        // to pin the live-like wire shape, not just `ContentBlock` shapes.
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
    fn join_text_blocks_returns_empty_for_tool_only_response() {
        // Defensive: a tool_use-only reply must not surface as a title —
        // the caller treats empty as "parse failure, keep fallback".
        let blocks = vec![ContentBlock::ToolUse {
            id: "t1".to_owned(),
            name: "noop".to_owned(),
            input: serde_json::Value::Null,
        }];
        assert_eq!(join_text_blocks(blocks), "");
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

    // ── parse_sse_frame ──

    #[test]
    fn parse_sse_frame_text_delta() {
        let frame = indoc! {r#"
            event: content_block_delta
            data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}
        "#};
        let StreamEvent::ContentBlockDelta { index, delta } =
            parse_sse_frame(frame).unwrap().unwrap()
        else {
            panic!("expected ContentBlockDelta");
        };
        assert_eq!(index, 0);
        let Delta::TextDelta { text } = delta else {
            panic!("expected TextDelta");
        };
        assert_eq!(text, "Hello");
    }

    #[test]
    fn parse_sse_frame_ping() {
        let frame = indoc! {r#"
            event: ping
            data: {"type":"ping"}
        "#};
        assert!(matches!(
            parse_sse_frame(frame).unwrap().unwrap(),
            StreamEvent::Ping,
        ));
    }

    #[test]
    fn parse_sse_frame_message_start() {
        let frame = indoc! {r#"
            event: message_start
            data: {"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}
        "#};
        let StreamEvent::MessageStart { message } = parse_sse_frame(frame).unwrap().unwrap() else {
            panic!("expected MessageStart");
        };
        assert_eq!(message.id, "msg_123");
        assert_eq!(message.model, "claude-sonnet-4-6");
        let usage = message.usage.expect("expected usage");
        assert_eq!(usage.input_tokens, 25);
        assert_eq!(usage.output_tokens, 1);
    }

    #[test]
    fn parse_sse_frame_error_event() {
        let frame = indoc! {r#"
            event: error
            data: {"type":"error","error":{"type":"rate_limit_error","message":"Too many requests"}}
        "#};
        let StreamEvent::Error { error } = parse_sse_frame(frame).unwrap().unwrap() else {
            panic!("expected Error");
        };
        assert_eq!(error.error_type, "rate_limit_error");
        assert_eq!(error.message, "Too many requests");
    }

    #[test]
    fn parse_sse_frame_unknown_event_type() {
        let frame = indoc! {r#"
            event: some_future_event
            data: {"type":"some_future_event","payload":"data"}
        "#};
        assert!(matches!(
            parse_sse_frame(frame).unwrap().unwrap(),
            StreamEvent::Unknown,
        ));
    }

    #[test]
    fn parse_sse_frame_without_data_line_yields_none() {
        assert!(parse_sse_frame("").unwrap().is_none());
        assert!(parse_sse_frame(": comment line").unwrap().is_none());
    }

    #[test]
    fn parse_sse_frame_invalid_json_errors() {
        assert!(parse_sse_frame("data: {not valid json}").is_err());
    }

    #[test]
    fn parse_sse_frame_concatenates_multiple_data_lines_with_newline() {
        // Per the SSE spec, multiple data: lines join with \n. JSON
        // treats \n as token whitespace, so this round-trips cleanly.
        let frame = indoc! {r#"
            event: ping
            data: {"type":
            data: "ping"}
        "#};
        assert!(matches!(
            parse_sse_frame(frame).unwrap().unwrap(),
            StreamEvent::Ping,
        ));
    }

    #[test]
    fn parse_sse_frame_accepts_data_prefix_without_space() {
        // Some gateways emit `data:payload` (no leading space).
        let frame = indoc! {r#"
            data:{"type":"ping"}
        "#};
        assert!(matches!(
            parse_sse_frame(frame).unwrap().unwrap(),
            StreamEvent::Ping,
        ));
    }
}
