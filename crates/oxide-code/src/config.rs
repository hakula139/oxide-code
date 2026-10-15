//! Configuration loading.
//!
//! Precedence (highest wins): env > project `ox.toml` > user `~/.config/ox/config.toml` > defaults.
//! Auth stops at the first source that resolves, in [`auth::AuthSource`] order.

pub(crate) mod auth;
pub(crate) mod file;
pub(crate) mod gateway;
mod oauth;

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;

use self::auth::{Auth, AuthSource};
use self::gateway::{GatewayCompat, GatewaySnapshot};
use crate::model::ModelSpec;
use crate::tui::theme::{self, Theme};
use crate::util::env;
use crate::util::path::expand_user;
use crate::util::tls::load_extra_ca_certs;

const DEFAULT_MODEL: &str = "claude-opus-5-5";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const AUTO_COMPACTION_OUTPUT_RESERVE_CAP: u32 = 20_000;
const AUTO_COMPACTION_BUFFER_TOKENS: u32 = 13_000;
const MIN_AUTO_COMPACTION_THRESHOLD_TOKENS: u32 = 50_000;
/// Mirrors the fallback `loader::resolve_theme` applies when no `[tui.theme] base` is set.
pub(crate) const DEFAULT_THEME: &str = "mocha";

// ── ConfigSnapshot ──

/// Resolved-config view minus the secret. Survives [`Config`] being consumed by the client.
#[derive(Debug, Clone)]
pub(crate) struct ConfigSnapshot {
    pub(crate) model: ModelSpec,
    pub(crate) effort: Option<Effort>,
    pub(crate) auth: AuthSource,
    pub(crate) base_url: String,
    /// PEM bundle appended to reqwest's trust anchors.
    pub(crate) extra_ca_certs: Option<PathBuf>,
    pub(crate) gateway: GatewaySnapshot,
    pub(crate) max_tokens: u32,
    /// `None` means the agent loop runs without a per-turn round cap.
    pub(crate) max_tool_rounds: Option<u32>,
    pub(crate) prompt_cache_ttl: PromptCacheTtl,
    pub(crate) compaction: CompactionConfig,
    pub(crate) permission_mode: crate::permission::Mode,
    pub(crate) show_thinking: bool,
    pub(crate) show_welcome: bool,
    pub(crate) status_line: Vec<StatusLineSegment>,
    /// Resolved theme base name — built-in catalogue key or filesystem path. `/theme` reads this
    /// to mark the active row in the picker.
    pub(crate) theme_name: String,
}

// ── ThinkingConfig ──

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ThinkingConfig {
    /// Model decides the thinking budget.
    Adaptive {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
        block_binding: BlockBinding,
    },
}

/// `thinking.display` values. Omitting the field returns no thinking text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThinkingDisplay {
    Summarized,
    /// Only the progress notes written between tool calls come back as text. Requires the
    /// `thinking-display-updates-2026-08-18` beta.
    Updates,
}

/// `thinking.block_binding`. `DropBlock` makes the API drop a replayed thinking block whose
/// conversation prefix changed since it was produced, plus every later one, where it would
/// otherwise reject the request. Requires the `thinking-binding-controls-2026-08-01` beta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "prefix_mismatch_behavior", rename_all = "snake_case")]
pub(crate) enum BlockBinding {
    DropBlock,
}

// ── Effort ──

/// Intelligence-vs-latency tier sent as `output_config.effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub(crate) const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];
    pub(crate) const VALID_VALUES: &str = "low, medium, high, xhigh, max";

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// One-line UX hint for the typed-arg popup.
    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::Low => "Fastest, shallow reasoning",
            Self::Medium => "Balanced",
            Self::High => "Deep reasoning",
            Self::Xhigh => "Extended thinking",
            Self::Max => "Maximum thinking",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => bail!(
                "invalid effort {s:?}; expected one of: {}",
                Self::VALID_VALUES
            ),
        }
    }
}

// ── PromptCacheTtl ──

/// Sent as `cache_control.ttl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PromptCacheTtl {
    #[serde(rename = "5m")]
    FiveMin,
    #[serde(rename = "1h")]
    OneHour,
}

impl PromptCacheTtl {
    /// `None` for the server default (5 m).
    pub(crate) const fn wire(self) -> Option<&'static str> {
        match self {
            Self::FiveMin => None,
            Self::OneHour => Some("1h"),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::FiveMin => "5m",
            Self::OneHour => "1h",
        }
    }
}

impl fmt::Display for PromptCacheTtl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PromptCacheTtl {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "5m" => Ok(Self::FiveMin),
            "1h" => Ok(Self::OneHour),
            _ => bail!("invalid prompt_cache_ttl {s:?}; expected one of: 5m, 1h"),
        }
    }
}

// ── Status Line ──

/// User-configurable status-line segment identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StatusLineSegment {
    CurrentDir,
    GitBranch,
    PullRequest,
    Model,
    ModelWithEffort,
    ContextUsed,
    SessionCost,
    RunState,
    ThreadTitle,
    CurrentTime,
}

impl StatusLineSegment {
    /// Canonical name table consumed by `OX_STATUS_LINE` parsing and the empty-roster error
    /// formatter. TOML config goes through serde, so the strings here must match the kebab-case
    /// form `serde(rename_all = "kebab-case")` would produce.
    const ALL: &'static [(Self, &'static str)] = &[
        (Self::CurrentDir, "current-dir"),
        (Self::GitBranch, "git-branch"),
        (Self::PullRequest, "pull-request"),
        (Self::Model, "model"),
        (Self::ModelWithEffort, "model-with-effort"),
        (Self::ContextUsed, "context-used"),
        (Self::SessionCost, "session-cost"),
        (Self::RunState, "run-state"),
        (Self::ThreadTitle, "thread-title"),
        (Self::CurrentTime, "current-time"),
    ];

    /// Order rationale documented in `docs/design/tui/status-line.md`.
    pub(crate) const DEFAULT: &'static [Self] = &[
        Self::CurrentDir,
        Self::GitBranch,
        Self::PullRequest,
        Self::ModelWithEffort,
        Self::ContextUsed,
        Self::SessionCost,
        Self::RunState,
        Self::ThreadTitle,
    ];
}

impl FromStr for StatusLineSegment {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if let Some((segment, _)) = Self::ALL.iter().find(|(_, name)| *name == s) {
            return Ok(*segment);
        }
        bail!(
            "invalid status line segment {s:?}; expected one of: {}",
            Self::ALL
                .iter()
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

// ── CompactionConfig ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompactionConfig {
    pub(crate) auto: AutoCompactionConfig,
    policy: AutoCompactionPolicy,
}

impl CompactionConfig {
    #[cfg(test)]
    pub(crate) const fn disabled() -> Self {
        Self {
            auto: AutoCompactionConfig::disabled(),
            policy: AutoCompactionPolicy::Disabled,
        }
    }

    pub(crate) fn for_model(self, model: &ModelSpec, max_tokens: u32) -> Self {
        resolve_compaction_policy(self.policy, model, max_tokens)
    }

    #[cfg(test)]
    pub(crate) const fn resolved_for_test(auto: AutoCompactionConfig) -> Self {
        let policy = if auto.enabled {
            match auto.threshold_tokens {
                Some(tokens) => AutoCompactionPolicy::Tokens(tokens),
                None => AutoCompactionPolicy::Default,
            }
        } else {
            AutoCompactionPolicy::Disabled
        };
        Self { auto, policy }
    }

    #[cfg(test)]
    pub(crate) fn default_for_test(model: &ModelSpec, max_tokens: u32) -> Self {
        resolve_compaction_policy(AutoCompactionPolicy::Default, model, max_tokens)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoCompactionConfig {
    pub(crate) enabled: bool,
    pub(crate) threshold_tokens: Option<u32>,
}

impl AutoCompactionConfig {
    pub(crate) const fn disabled() -> Self {
        Self {
            enabled: false,
            threshold_tokens: None,
        }
    }

    pub(crate) const fn should_trigger(self, total_tokens: u32) -> bool {
        match (self.enabled, self.threshold_tokens) {
            (true, Some(threshold)) => total_tokens >= threshold,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoCompactionPolicy {
    Disabled,
    Default,
    Tokens(u32),
    Percent(ThresholdPercent),
}

/// A share of the context window, validated to 1 through 100 when the config loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ThresholdPercent(u8);

impl ThresholdPercent {
    fn new(percent: u8) -> Result<Self> {
        if !(1..=100).contains(&percent) {
            bail!("auto compaction threshold percent must be between 1 and 100, got {percent}");
        }
        Ok(Self(percent))
    }
}

// ── Config ──

/// Resolved configuration.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) model: ModelSpec,
    /// `None` for models outside the catalogue.
    pub(crate) effort: Option<Effort>,
    pub(crate) auth: Auth,
    pub(crate) base_url: String,
    /// Resolved path to a PEM bundle appended to reqwest's trust anchors. `None` means the
    /// client trusts only the platform roots.
    pub(crate) extra_ca_certs: Option<PathBuf>,
    pub(crate) gateway: GatewayCompat,
    /// User-set output cap. `None` defers to [`ModelSpec::resolve_max_tokens`].
    pub(crate) max_tokens: Option<u32>,
    /// `None` means the agent loop runs without a per-turn round cap.
    pub(crate) max_tool_rounds: Option<u32>,
    pub(crate) prompt_cache_ttl: PromptCacheTtl,
    pub(crate) compaction: CompactionConfig,
    pub(crate) permission: crate::permission::Policy,
    /// Opts adaptive thinking into summarized text. The request's `thinking` field derives from
    /// this plus the model.
    pub(crate) show_thinking: bool,
    pub(crate) show_welcome: bool,
    pub(crate) status_line: Vec<StatusLineSegment>,
    pub(crate) theme: Theme,
    /// Built-in catalogue key (e.g. `"mocha"`) or filesystem path. Mirrors `[tui.theme] base`,
    /// falling back to [`DEFAULT_THEME`] when unset.
    pub(crate) theme_name: String,
}

impl Config {
    /// Resolves config from layered sources. Per-field precedence is env > project `ox.toml` >
    /// user `~/.config/ox/config.toml` > built-in default. See the module docs for the auth
    /// chain. Parse errors (TOML, env, theme) propagate so a typo doesn't degrade silently into
    /// "no credentials".
    pub(crate) async fn load() -> Result<Self> {
        let fc = file::load()?;
        let client = fc.client.unwrap_or_default();
        let tui = fc.tui.unwrap_or_default();
        let theme_config = tui.theme.unwrap_or_default();

        let base_url = env::string("ANTHROPIC_BASE_URL")
            .or(client.base_url)
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        validate_base_url(&base_url)?;

        // Resolve before `auth` so the OAuth refresh can also thread the extra trust anchors
        // through reqwest (relevant under SSL-inspecting corporate proxies).
        let extra_ca_certs = resolve_extra_ca_certs(client.extra_ca_certs)?;

        let gateway = GatewayCompat::load()?;

        let auth = auth::resolve(
            auth::explicit_credentials(client.api_key),
            &base_url,
            oauth::load_token(extra_ca_certs.as_deref()),
        )
        .await?;
        gateway.check_credential_header(auth.source())?;

        let model = ModelSpec::resolve(
            env::string("ANTHROPIC_MODEL")
                .or(client.model)
                .unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
        );

        let effort_pick = match env::string("ANTHROPIC_EFFORT") {
            Some(raw) => Some(raw.parse::<Effort>().context("ANTHROPIC_EFFORT")?),
            None => client.effort,
        };
        let effort = model.resolve_effort(effort_pick);

        let max_tokens = match env::string("ANTHROPIC_MAX_TOKENS") {
            Some(raw) => Some(
                raw.parse::<u32>()
                    .with_context(|| format!("ANTHROPIC_MAX_TOKENS={raw:?}"))?,
            ),
            None => client.max_tokens,
        };

        let max_tool_rounds = match env::string("OX_MAX_TOOL_ROUNDS") {
            Some(raw) => Some(
                raw.parse::<u32>()
                    .with_context(|| format!("OX_MAX_TOOL_ROUNDS={raw:?}"))?,
            ),
            None => client.max_tool_rounds,
        };

        let show_thinking = env::bool("OX_SHOW_THINKING")?
            .or(tui.show_thinking)
            .unwrap_or(false);

        let show_welcome = env::bool("OX_SHOW_WELCOME")?
            .or(tui.show_welcome)
            .unwrap_or(true);

        let status_line = match env::string("OX_STATUS_LINE") {
            Some(raw) => parse_status_line_segments(&raw).context("OX_STATUS_LINE")?,
            None => validate_status_line(
                tui.status_line
                    .unwrap_or_else(|| StatusLineSegment::DEFAULT.to_vec()),
            )
            .context("tui.status_line")?,
        };

        let prompt_cache_ttl = match env::string("OX_PROMPT_CACHE_TTL") {
            Some(raw) => raw
                .parse::<PromptCacheTtl>()
                .context("OX_PROMPT_CACHE_TTL")?,
            None => client.prompt_cache_ttl.unwrap_or(PromptCacheTtl::OneHour),
        };

        let compaction = resolve_compaction(
            client.compaction,
            &model,
            model.resolve_max_tokens(max_tokens),
        )?;

        let permission = resolve_permission(fc.permission)?;

        let theme_name = theme_config
            .base
            .clone()
            .unwrap_or_else(|| DEFAULT_THEME.to_owned());
        let theme = theme::resolve_theme(
            theme_config.base.as_deref(),
            &theme_config.overrides.unwrap_or_default(),
        )?;

        Ok(Self {
            model,
            effort,
            auth,
            base_url,
            extra_ca_certs,
            gateway,
            max_tokens,
            max_tool_rounds,
            prompt_cache_ttl,
            compaction,
            permission,
            show_thinking,
            show_welcome,
            status_line,
            theme,
            theme_name,
        })
    }

    /// Descriptors for `/config` and `/status`, minus the auth secret.
    pub(crate) fn snapshot(&self) -> ConfigSnapshot {
        ConfigSnapshot {
            model: self.model.clone(),
            effort: self.effort,
            auth: self.auth.source(),
            base_url: self.base_url.clone(),
            extra_ca_certs: self.extra_ca_certs.clone(),
            gateway: self.gateway.snapshot(),
            max_tokens: self.model.resolve_max_tokens(self.max_tokens),
            max_tool_rounds: self.max_tool_rounds,
            prompt_cache_ttl: self.prompt_cache_ttl,
            compaction: self.compaction,
            permission_mode: self.permission.mode(),
            show_thinking: self.show_thinking,
            show_welcome: self.show_welcome,
            status_line: self.status_line.clone(),
            theme_name: self.theme_name.clone(),
        }
    }
}

// ── Helpers ──

fn parse_status_line_segments(raw: &str) -> Result<Vec<StatusLineSegment>> {
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    let all_empty = parts.iter().all(|part| part.is_empty());
    if all_empty {
        return validate_status_line(Vec::new());
    }
    parts
        .into_iter()
        .map(|part| {
            if part.is_empty() {
                bail!(
                    "OX_STATUS_LINE has an empty segment. Remove the stray comma or unset the \
                     variable to use the default"
                );
            }
            part.parse::<StatusLineSegment>()
        })
        .collect::<Result<Vec<_>>>()
        .and_then(validate_status_line)
}

fn validate_status_line(segments: Vec<StatusLineSegment>) -> Result<Vec<StatusLineSegment>> {
    if segments.is_empty() {
        bail!(
            "status_line must list at least one segment. Remove the key (or unset \
             OX_STATUS_LINE) to use the default. Valid segments: {}",
            StatusLineSegment::ALL
                .iter()
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    Ok(segments)
}

pub(crate) fn display_effort(effort: Option<Effort>) -> String {
    effort.map_or_else(|| "(no effort tier)".to_owned(), |e| e.to_string())
}

pub(crate) fn display_bool(flag: bool) -> &'static str {
    if flag { "on" } else { "off" }
}

pub(crate) fn display_max_tool_rounds(cap: Option<u32>) -> String {
    cap.map_or_else(|| "unbounded".to_owned(), |n| n.to_string())
}

pub(crate) fn display_auto_compaction(auto: AutoCompactionConfig) -> String {
    match (auto.enabled, auto.threshold_tokens) {
        (true, Some(threshold)) => format!("at {threshold} tokens"),
        (true, None) => "off (no threshold)".to_owned(),
        _ => "off".to_owned(),
    }
}

fn resolve_compaction(
    file: Option<file::CompactionConfig>,
    model: &ModelSpec,
    max_tokens: u32,
) -> Result<CompactionConfig> {
    let auto_requested = env::bool("OX_COMPACTION_AUTO_ENABLED")?
        .or_else(|| file.as_ref().and_then(|c| c.enabled))
        .unwrap_or(true);

    let policy = if auto_requested {
        resolve_auto_policy(file.as_ref())?
    } else {
        AutoCompactionPolicy::Disabled
    };
    Ok(resolve_compaction_policy(policy, model, max_tokens))
}

/// Resolves the permission policy from the merged file config plus the `OX_PERMISSION_MODE`
/// override. The env mode wins over the file mode with the empty-is-absent and parse-error-
/// propagates behavior `effort` uses, so a typo fails loudly rather than defaulting permissive.
/// Allow / deny rule strings are parsed here so a malformed rule surfaces at load. Deny prefixes
/// resolve against the process cwd, which the gate also resolves paths against.
fn resolve_permission(
    file: Option<file::PermissionFileConfig>,
) -> Result<crate::permission::Policy> {
    let file = file.unwrap_or_default();

    let mode = match env::string("OX_PERMISSION_MODE") {
        Some(raw) => raw.parse().context("OX_PERMISSION_MODE")?,
        None => file.mode.unwrap_or_default(),
    };

    let allow = file.allow.unwrap_or_default();
    let deny = file.deny.unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();
    crate::permission::Policy::resolve(mode, &allow, &deny, &cwd)
}

fn resolve_compaction_policy(
    policy: AutoCompactionPolicy,
    model: &ModelSpec,
    max_tokens: u32,
) -> CompactionConfig {
    let auto = resolve_auto_compaction(policy, model, max_tokens);
    CompactionConfig { auto, policy }
}

fn resolve_auto_compaction(
    policy: AutoCompactionPolicy,
    model: &ModelSpec,
    max_tokens: u32,
) -> AutoCompactionConfig {
    let threshold = match policy {
        AutoCompactionPolicy::Disabled => return AutoCompactionConfig::disabled(),
        AutoCompactionPolicy::Default => default_auto_threshold(model, max_tokens),
        AutoCompactionPolicy::Tokens(tokens) => {
            Some(threshold_from_tokens(tokens, model, max_tokens))
        }
        AutoCompactionPolicy::Percent(percent) => {
            threshold_from_percent(percent, model, max_tokens)
        }
    };
    AutoCompactionConfig {
        enabled: threshold.is_some(),
        threshold_tokens: threshold,
    }
}

fn resolve_auto_policy(file: Option<&file::CompactionConfig>) -> Result<AutoCompactionPolicy> {
    let env_tokens = env_u32("OX_COMPACTION_AUTO_THRESHOLD_TOKENS")?;
    let env_percent = env_u8("OX_COMPACTION_AUTO_THRESHOLD_PERCENT")?;
    let env_threshold_set = env_tokens.is_some() || env_percent.is_some();
    let file_tokens = file.and_then(|c| c.threshold_tokens);
    let file_percent = file.and_then(|c| c.threshold_percent);
    let (tokens, percent) = if env_threshold_set {
        (env_tokens, env_percent)
    } else {
        (file_tokens, file_percent)
    };

    match (tokens, percent) {
        (Some(_), Some(_)) => {
            bail!("set only one of auto_threshold_tokens or auto_threshold_percent for compaction")
        }
        (Some(tokens), None) => Ok(AutoCompactionPolicy::Tokens(tokens)),
        (None, Some(percent)) => ThresholdPercent::new(percent).map(AutoCompactionPolicy::Percent),
        (None, None) => Ok(AutoCompactionPolicy::Default),
    }
}

fn threshold_from_tokens(tokens: u32, model: &ModelSpec, max_tokens: u32) -> u32 {
    clamp_threshold_floor(clamp_threshold_ceil(tokens, model, max_tokens))
}

fn threshold_from_percent(
    ThresholdPercent(percent): ThresholdPercent,
    model: &ModelSpec,
    max_tokens: u32,
) -> Option<u32> {
    let context_window = model.context_window()?;
    let raw = context_window.saturating_mul(u32::from(percent)) / 100;
    Some(clamp_threshold_floor(clamp_threshold_ceil(
        raw, model, max_tokens,
    )))
}

/// Snaps `tokens` down to the model's safe trigger when one is known.
fn clamp_threshold_ceil(tokens: u32, model: &ModelSpec, max_tokens: u32) -> u32 {
    default_auto_threshold(model, max_tokens).map_or(tokens, |max| tokens.min(max))
}

/// Snaps `tokens` up to [`MIN_AUTO_COMPACTION_THRESHOLD_TOKENS`].
fn clamp_threshold_floor(tokens: u32) -> u32 {
    tokens.max(MIN_AUTO_COMPACTION_THRESHOLD_TOKENS)
}

fn default_auto_threshold(model: &ModelSpec, max_tokens: u32) -> Option<u32> {
    model
        .context_window()
        .and_then(|window| default_auto_threshold_for_window(window, max_tokens))
}

fn default_auto_threshold_for_window(context_window: u32, max_tokens: u32) -> Option<u32> {
    let reserve = max_tokens.min(AUTO_COMPACTION_OUTPUT_RESERVE_CAP);
    context_window
        .checked_sub(reserve)?
        .checked_sub(AUTO_COMPACTION_BUFFER_TOKENS)
}

fn env_u32(key: &'static str) -> Result<Option<u32>> {
    env::string(key)
        .map(|raw| raw.parse::<u32>().with_context(|| format!("{key}={raw:?}")))
        .transpose()
}

fn env_u8(key: &'static str) -> Result<Option<u8>> {
    env::string(key)
        .map(|raw| raw.parse::<u8>().with_context(|| format!("{key}={raw:?}")))
        .transpose()
}

/// `OX_EXTRA_CA_CERTS` > `client.extra_ca_certs` > `NODE_EXTRA_CA_CERTS`. Node is often exported
/// shell-wide for npm, so like Node itself, an unreadable bundle there warns and is skipped.
fn resolve_extra_ca_certs(file_path: Option<String>) -> Result<Option<PathBuf>> {
    let explicit = env::string("OX_EXTRA_CA_CERTS")
        .map(|raw| ("OX_EXTRA_CA_CERTS", raw))
        .or(file_path.map(|raw| ("client.extra_ca_certs", raw)));
    if let Some((source, raw)) = explicit {
        return expand_user(&raw)
            .map(Some)
            .with_context(|| format!("invalid {source}"));
    }

    let Some(raw) = env::string("NODE_EXTRA_CA_CERTS") else {
        return Ok(None);
    };
    match expand_user(&raw).and_then(|path| load_extra_ca_certs(&path).map(|_| path)) {
        Ok(path) => Ok(Some(path)),
        Err(e) => {
            warn!("ignoring NODE_EXTRA_CA_CERTS: {e:#}");
            Ok(None)
        }
    }
}

fn validate_base_url(raw: &str) -> Result<()> {
    let url = reqwest::Url::parse(raw).with_context(|| format!("invalid base URL {raw:?}"))?;
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_url(&url) => Ok(()),
        "http" => bail!("base URL must use https unless it points at localhost"),
        scheme => bail!("base URL must use http or https, got {scheme:?}"),
    }
}

fn is_loopback_url(url: &reqwest::Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    )
}

/// Whether `base_url` points at Anthropic's production or staging host. Gates the stored-login
/// fallback and body-side `cache_control.scope = "global"`, which gateways reject.
pub(crate) fn is_first_party_base_url(base_url: &str) -> bool {
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

/// Output of [`default_auto_threshold`] when `max_tokens` saturates the reserve cap.
#[cfg(test)]
pub(crate) mod test_thresholds {
    pub(crate) const WINDOW_1M: u32 = 967_000;
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    // ── ThinkingConfig ──

    #[test]
    fn thinking_config_adaptive_serializes_display_and_block_binding() {
        let binding = serde_json::json!({"prefix_mismatch_behavior": "drop_block"});
        for (display, wire) in [
            (None, None),
            (Some(ThinkingDisplay::Summarized), Some("summarized")),
            (Some(ThinkingDisplay::Updates), Some("updates")),
        ] {
            let json = serde_json::to_value(&ThinkingConfig::Adaptive {
                display,
                block_binding: BlockBinding::DropBlock,
            })
            .unwrap();
            let mut expected = serde_json::json!({"type": "adaptive", "block_binding": binding});
            if let Some(wire) = wire {
                expected["display"] = wire.into();
            }
            assert_eq!(json, expected);
        }
    }

    // ── Effort ──

    #[test]
    fn effort_round_trips_through_serde_and_fromstr() {
        for (variant, token) in Effort::ALL
            .into_iter()
            .zip(["low", "medium", "high", "xhigh", "max"])
        {
            assert_eq!(serde_json::to_value(variant).unwrap(), token);
            assert_eq!(variant.to_string(), token);
            assert_eq!(token.parse::<Effort>().unwrap(), variant);
        }
    }

    #[test]
    fn effort_rejects_unknown_tokens_with_actionable_error() {
        let err = "extra-high".parse::<Effort>().expect_err("unknown token");
        let msg = format!("{err:#}");
        assert!(msg.contains("extra-high"), "{msg}");
        for token in ["low", "medium", "high", "xhigh", "max"] {
            assert!(msg.contains(token), "{token}: {msg}");
        }
    }

    #[test]
    fn effort_round_trips_through_toml_deserialize() {
        #[derive(Deserialize)]
        struct Wrap {
            effort: Effort,
        }
        let wrap: Wrap = toml::from_str(r#"effort = "xhigh""#).unwrap();
        assert_eq!(wrap.effort, Effort::Xhigh);
    }

    // ── PromptCacheTtl ──

    #[test]
    fn prompt_cache_ttl_wire_shape() {
        assert_eq!(PromptCacheTtl::FiveMin.wire(), None);
        assert_eq!(PromptCacheTtl::OneHour.wire(), Some("1h"));
    }

    #[test]
    fn prompt_cache_ttl_round_trips_through_serde_and_fromstr() {
        for (variant, token) in [
            (PromptCacheTtl::FiveMin, "5m"),
            (PromptCacheTtl::OneHour, "1h"),
        ] {
            assert_eq!(serde_json::to_value(variant).unwrap(), token);
            assert_eq!(variant.to_string(), token);
            assert_eq!(token.parse::<PromptCacheTtl>().unwrap(), variant);
        }
    }

    #[test]
    fn prompt_cache_ttl_rejects_unknown_tokens_with_actionable_error() {
        let err = "30m".parse::<PromptCacheTtl>().expect_err("unknown token");
        let msg = format!("{err:#}");
        assert!(msg.contains("30m"), "{msg}");
        assert!(msg.contains("5m"), "{msg}");
        assert!(msg.contains("1h"), "{msg}");
    }

    // ── CompactionConfig::for_model ──

    #[test]
    fn for_model_re_resolves_a_percent_threshold_against_the_new_window() {
        let policy = AutoCompactionPolicy::Percent(ThresholdPercent::new(40).unwrap());
        let unknown = ModelSpec::resolve("custom-model".to_owned());
        let compaction = resolve_compaction_policy(policy, &unknown, 16_000);
        assert!(!compaction.auto.enabled, "precondition: no window");

        let known = ModelSpec::resolve("claude-opus-5-5".to_owned());
        let swapped = compaction.for_model(&known, 64_000);

        assert_eq!(
            swapped.auto,
            AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(400_000),
            }
        );
        assert_eq!(swapped.policy, policy);
    }

    // ── Config::load ──

    /// Env keys `Config::load` reads. `ANTHROPIC_API_KEY` defaults non-empty in [`env_vars`] so
    /// tests land on `ApiKey` without consulting the stored login.
    const ENV_KEYS: &[&str] = &[
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_CUSTOM_HEADERS",
        "CLAUDE_CODE_ATTRIBUTION_HEADER",
        "CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_MAX_TOKENS",
        "ANTHROPIC_EFFORT",
        "OX_EXTRA_CA_CERTS",
        "NODE_EXTRA_CA_CERTS",
        "OX_MAX_TOOL_ROUNDS",
        "OX_COMPACTION_AUTO_ENABLED",
        "OX_COMPACTION_AUTO_THRESHOLD_PERCENT",
        "OX_COMPACTION_AUTO_THRESHOLD_TOKENS",
        "OX_SHOW_THINKING",
        "OX_SHOW_WELCOME",
        "OX_STATUS_LINE",
        "OX_PROMPT_CACHE_TTL",
        "OX_PERMISSION_MODE",
        "XDG_CONFIG_HOME",
    ];

    fn write_user_config(xdg_dir: &Path, body: &str) {
        let config_dir = xdg_dir.join("ox");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), body).unwrap();
    }

    /// Baseline env (all unset, `ANTHROPIC_API_KEY` = `"sk-default"`) plus overrides.
    fn env_vars(
        overrides: impl IntoIterator<Item = (&'static str, Option<String>)>,
    ) -> Vec<(&'static str, Option<String>)> {
        let known: HashSet<&'static str> = ENV_KEYS.iter().copied().collect();
        let mut out: Vec<(&'static str, Option<String>)> = ENV_KEYS
            .iter()
            .copied()
            .map(|k| {
                (
                    k,
                    (k == "ANTHROPIC_API_KEY").then(|| "sk-default".to_owned()),
                )
            })
            .collect();
        for (key, value) in overrides {
            assert!(known.contains(key), "env key {key:?} not in ENV_KEYS");
            if let Some(slot) = out.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = value;
            }
        }
        out
    }

    fn xdg(dir: &TempDir) -> (&'static str, Option<String>) {
        (
            "XDG_CONFIG_HOME",
            Some(dir.path().to_string_lossy().into_owned()),
        )
    }

    fn env(key: &'static str, value: &str) -> (&'static str, Option<String>) {
        (key, Some(value.to_owned()))
    }

    fn assert_auth(config: &Config, source: AuthSource, secret: &str) {
        assert_eq!(config.auth.source(), source);
        assert_eq!(config.auth.secret(), secret);
    }

    #[tokio::test]
    async fn load_defaults_apply_when_no_config_and_no_env() {
        let dir = tempfile::tempdir().unwrap();
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.model.id(), "claude-opus-5-5");
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.max_tokens, None);
        assert_eq!(config.effort, Some(Effort::Medium));
        assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::OneHour);
        assert!(config.compaction.auto.enabled);
        assert_eq!(
            config.compaction.auto.threshold_tokens,
            Some(test_thresholds::WINDOW_1M),
        );
        assert!(!config.show_thinking);
        assert!(
            config.show_welcome,
            "default-on so the empty chat surfaces the welcome"
        );
        assert_eq!(config.status_line, StatusLineSegment::DEFAULT);
        assert_auth(&config, AuthSource::ApiKeyEnv, "sk-default");
        assert!(config.gateway.custom_headers.is_empty());
        assert!(config.gateway.attribution);
        assert!(config.gateway.structured_outputs);
        assert_eq!(config.theme_name, DEFAULT_THEME);
        assert_eq!(config.permission.mode(), crate::permission::Mode::Auto);
    }

    #[tokio::test]
    async fn load_theme_name_reflects_user_picked_base() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [tui.theme]
                base = "latte"
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.theme_name, "latte");
    }

    #[tokio::test]
    async fn load_env_overrides_every_client_field() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-sonnet-5-5"),
            env("ANTHROPIC_BASE_URL", "https://example.invalid"),
            env("ANTHROPIC_MAX_TOKENS", "64"),
            env("OX_MAX_TOOL_ROUNDS", "200"),
            env("OX_SHOW_THINKING", "1"),
            env("OX_SHOW_WELCOME", "0"),
            env("OX_STATUS_LINE", "run-state,model,current-dir"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.model.id(), "claude-sonnet-5-5");
        assert_eq!(config.base_url, "https://example.invalid");
        assert_eq!(config.max_tokens, Some(64));
        assert_eq!(config.max_tool_rounds, Some(200));
        assert!(config.compaction.auto.enabled);
        assert!(config.show_thinking);
        assert!(
            !config.show_welcome,
            "env `0` flips the default-on welcome off"
        );
        assert_eq!(
            config.status_line,
            vec![
                StatusLineSegment::RunState,
                StatusLineSegment::Model,
                StatusLineSegment::CurrentDir,
            ],
        );
    }

    #[tokio::test]
    async fn load_user_config_supplies_values_without_env_overrides() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                model = "claude-haiku-5-5"
                base_url = "https://config-file.invalid"
                max_tokens = 128
                max_tool_rounds = 75

                [tui]
                show_thinking = true
                show_welcome = false
                status_line = ["current-dir", "git-branch", "model-with-effort"]
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.model.id(), "claude-haiku-5-5");
        assert_eq!(config.base_url, "https://config-file.invalid");
        assert_eq!(config.max_tokens, Some(128));
        assert_eq!(config.max_tool_rounds, Some(75));
        assert!(config.compaction.auto.enabled);
        assert!(config.show_thinking);
        assert!(
            !config.show_welcome,
            "file `false` opts out of the default-on welcome"
        );
        assert_eq!(
            config.status_line,
            vec![
                StatusLineSegment::CurrentDir,
                StatusLineSegment::GitBranch,
                StatusLineSegment::ModelWithEffort,
            ],
        );
    }

    #[tokio::test]
    async fn load_status_line_invalid_segment_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![xdg(&dir), env("OX_STATUS_LINE", "model,nope")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("invalid segment must propagate");
        let msg = format!("{err:#}");
        assert!(msg.contains("OX_STATUS_LINE"), "{msg}");
        assert!(msg.contains("nope"), "{msg}");
        assert!(msg.contains("current-dir"), "{msg}");
    }

    #[tokio::test]
    async fn load_status_line_stray_comma_in_env_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![xdg(&dir), env("OX_STATUS_LINE", "model,,run-state")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("stray comma must surface as a typo");
        let msg = format!("{err:#}");
        assert!(msg.contains("OX_STATUS_LINE"), "{msg}");
        assert!(msg.contains("empty segment"), "{msg}");
        assert!(msg.contains("stray comma"), "{msg}");
    }

    #[tokio::test]
    async fn load_status_line_invalid_segment_in_file_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [tui]
                status_line = ["model", "nope"]
            "#},
        );
        let err = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .expect_err("invalid TOML segment must propagate");
        let msg = format!("{err:#}");
        assert!(msg.contains("status_line"), "{msg}");
        assert!(msg.contains("unknown variant `nope`"), "{msg}");
        assert!(msg.contains("current-dir"), "{msg}");
        assert!(msg.contains("run-state"), "{msg}");
    }

    #[tokio::test]
    async fn load_status_line_empty_env_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![xdg(&dir), env("OX_STATUS_LINE", ",")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("empty status line must not launch without run-state");
        let msg = format!("{err:#}");
        assert!(msg.contains("OX_STATUS_LINE"), "{msg}");
        assert!(msg.contains("at least one segment"), "{msg}");
        assert!(msg.contains("unset OX_STATUS_LINE"), "{msg}");
        assert!(msg.contains("run-state"), "{msg}");
    }

    #[tokio::test]
    async fn load_status_line_empty_file_roster_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r"
                [tui]
                status_line = []
            "},
        );
        let err = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .expect_err("empty status line must not launch without run-state");
        let msg = format!("{err:#}");
        assert!(msg.contains("tui.status_line"), "{msg}");
        assert!(msg.contains("at least one segment"), "{msg}");
        assert!(msg.contains("Remove the key"), "{msg}");
        assert!(msg.contains("run-state"), "{msg}");
    }

    #[tokio::test]
    async fn load_env_beats_config_file_field_by_field() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                model = "claude-haiku-5-5"
                max_tokens = 128

                [tui]
                show_thinking = true
            "#},
        );
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-sonnet-5-5"),
            // `max_tokens` env is unset — the file value must win.
            env("OX_SHOW_THINKING", "0"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.model.id(), "claude-sonnet-5-5");
        assert_eq!(config.max_tokens, Some(128));
        assert!(!config.show_thinking, "env `0` overrides file `true`");
    }

    #[tokio::test]
    async fn load_env_api_key_beats_file_api_key() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                api_key = "sk-from-file"
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_API_KEY", "sk-from-env")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_auth(&config, AuthSource::ApiKeyEnv, "sk-from-env");
    }

    #[tokio::test]
    async fn load_file_api_key_used_when_env_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                api_key = "sk-from-file"
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_API_KEY", "")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_auth(&config, AuthSource::ApiKeyFile, "sk-from-file");
    }

    // ── Config::load / auth ──

    #[tokio::test]
    async fn load_resolves_credentials_in_precedence_order() {
        // Each row drops the previous winner. `ANTHROPIC_API_KEY` stays unset throughout.
        for (auth_token, file_key, winner, secret) in [
            (
                Some("gateway-token"),
                true,
                AuthSource::AuthTokenEnv,
                "gateway-token",
            ),
            (None, true, AuthSource::ApiKeyFile, "sk-from-file"),
            (None, false, AuthSource::OAuthTokenEnv, "oauth-token"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            if file_key {
                write_user_config(
                    dir.path(),
                    indoc::indoc! {r#"
                        [client]
                        api_key = "sk-from-file"
                    "#},
                );
            }
            let vars = env_vars(vec![
                xdg(&dir),
                ("ANTHROPIC_API_KEY", None),
                ("ANTHROPIC_AUTH_TOKEN", auth_token.map(str::to_owned)),
                env("CLAUDE_CODE_OAUTH_TOKEN", "oauth-token"),
            ]);
            let config = temp_env::async_with_vars(vars, Config::load())
                .await
                .unwrap();
            assert_auth(&config, winner, secret);
        }
    }

    #[tokio::test]
    async fn load_malformed_custom_headers_fail_before_credentials_resolve() {
        // No credential exists for the gateway, so reaching auth would fail with a different error.
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            ("ANTHROPIC_API_KEY", None),
            env("ANTHROPIC_BASE_URL", "https://gateway.example.com"),
            env("ANTHROPIC_CUSTOM_HEADERS", "missing colon"),
        ]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("malformed header must fail startup");
        assert_eq!(
            format!("{err:#}"),
            "invalid ANTHROPIC_CUSTOM_HEADERS: line 1: expected `Name: value`",
        );
    }

    #[tokio::test]
    async fn load_custom_header_carrying_the_active_credential_fails() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_CUSTOM_HEADERS", "Authorization: Bearer other"),
            env("ANTHROPIC_AUTH_TOKEN", "gateway-token"),
        ]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("custom header must not replace the credential");
        assert_eq!(
            format!("{err:#}"),
            "invalid ANTHROPIC_CUSTOM_HEADERS: authorization carries the Auth token \
             (ANTHROPIC_AUTH_TOKEN) credential and cannot be overridden",
        );
    }

    #[tokio::test]
    async fn load_gateway_without_credential_names_the_env_vars() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            ("ANTHROPIC_API_KEY", None),
            env("ANTHROPIC_BASE_URL", "https://gateway.example.com"),
        ]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("stored login must not go to a gateway");
        let msg = format!("{err:#}");
        assert!(msg.contains("https://gateway.example.com"), "{msg}");
        assert!(msg.contains("ANTHROPIC_AUTH_TOKEN"), "{msg}");
    }

    #[tokio::test]
    async fn load_compaction_file_can_disable_default_on_auto_behavior() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r"
                [client.compaction]
                auto_enabled = false
            "},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert!(!config.compaction.auto.enabled);
    }

    #[tokio::test]
    async fn load_compaction_auto_env_beats_file() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r"
                [client.compaction]
                auto_enabled = false
            "},
        );
        let vars = env_vars(vec![xdg(&dir), env("OX_COMPACTION_AUTO_ENABLED", "TRUE")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert!(config.compaction.auto.enabled);
    }

    #[tokio::test]
    async fn load_compaction_auto_threshold_tokens_sets_absolute_trigger() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r"
                [client.compaction]
                auto_threshold_tokens = 400000
            "},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.compaction.auto.threshold_tokens, Some(400_000));
    }

    #[tokio::test]
    async fn load_compaction_auto_threshold_percent_uses_context_window() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-opus-5-5"),
            env("OX_COMPACTION_AUTO_THRESHOLD_PERCENT", "40"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.compaction.auto.threshold_tokens, Some(400_000));
    }

    #[tokio::test]
    async fn load_compaction_rejects_ambiguous_auto_thresholds() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r"
                [client.compaction]
                auto_threshold_tokens = 400000
                auto_threshold_percent = 40
            "},
        );
        let err = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .expect_err("ambiguous thresholds must fail config load");
        let msg = format!("{err:#}");
        assert!(msg.contains("only one"), "{msg}");
    }

    #[tokio::test]
    async fn load_compaction_clamps_zero_auto_threshold_tokens_up_to_floor() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("OX_COMPACTION_AUTO_THRESHOLD_TOKENS", "0"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.compaction.auto.threshold_tokens, Some(50_000));
    }

    #[tokio::test]
    async fn load_compaction_clamps_too_low_auto_threshold_tokens_up_to_floor() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r"
                [client.compaction]
                auto_threshold_tokens = 49999
            "},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.compaction.auto.threshold_tokens, Some(50_000));
    }

    #[tokio::test]
    async fn load_compaction_clamps_too_low_auto_threshold_percent_up_to_floor() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-opus-5-5"),
            env("OX_COMPACTION_AUTO_THRESHOLD_PERCENT", "4"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.compaction.auto.threshold_tokens, Some(50_000));
    }

    #[tokio::test]
    async fn load_compaction_clamps_threshold_above_model_safe_window() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                model = "claude-sonnet-5-5"

                [client.compaction]
                auto_threshold_tokens = 990000
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(
            config.compaction.auto.threshold_tokens,
            Some(test_thresholds::WINDOW_1M),
        );
    }

    #[tokio::test]
    async fn load_compaction_rejects_out_of_range_auto_threshold_percent() {
        for percent in ["0", "101"] {
            let dir = tempfile::tempdir().unwrap();
            let vars = env_vars(vec![
                xdg(&dir),
                env("OX_COMPACTION_AUTO_THRESHOLD_PERCENT", percent),
            ]);
            let err = temp_env::async_with_vars(vars, Config::load())
                .await
                .expect_err("out-of-range threshold percent must fail config load");
            let msg = format!("{err:#}");
            assert!(
                msg.contains(&format!("between 1 and 100, got {percent}")),
                "{msg}"
            );
        }
    }

    #[tokio::test]
    async fn load_compaction_percent_for_unknown_model_disables_auto_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "custom-model"),
            env("OX_COMPACTION_AUTO_THRESHOLD_PERCENT", "40"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert!(!config.compaction.auto.enabled);
        assert_eq!(config.compaction.auto.threshold_tokens, None);
    }

    // ── resolve_permission ──

    #[tokio::test]
    async fn load_permission_mode_env_beats_file() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [permission]
                mode = "plan"
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("OX_PERMISSION_MODE", "yolo")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.permission.mode(), crate::permission::Mode::Yolo);
    }

    #[tokio::test]
    async fn load_permission_invalid_mode_env_errors() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![xdg(&dir), env("OX_PERMISSION_MODE", "bypass")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("invalid mode must propagate");
        let msg = format!("{err:#}");
        assert!(msg.contains("OX_PERMISSION_MODE"), "{msg}");
        assert!(msg.contains("invalid permission mode"), "{msg}");
    }

    #[tokio::test]
    async fn load_permission_invalid_rule_in_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [permission]
                allow = ["edit(src/**/[)"]
            "#},
        );
        let vars = env_vars(vec![xdg(&dir)]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("malformed rule must propagate");
        assert!(format!("{err:#}").contains("invalid path glob"));
    }

    #[tokio::test]
    async fn load_invalid_max_tokens_env_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {"
                [client]
                max_tokens = 128
            "},
        );
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_MAX_TOKENS", "not-a-number")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("invalid max-token env should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("ANTHROPIC_MAX_TOKENS"), "{msg}");
        assert!(msg.contains("not-a-number"), "{msg}");
    }

    #[tokio::test]
    async fn load_propagates_invalid_config_file() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                api_key = "sk-from-file"
                show_thinking = true
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_API_KEY", "")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("misplaced field must surface as an error");
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid config at"), "{msg}");
        assert!(msg.contains("unknown field `show_thinking`"), "{msg}");
    }

    #[tokio::test]
    async fn load_propagates_theme_resolution_error() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [tui.theme]
                base = "no-such-theme"
            "#},
        );
        let err = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .expect_err("unknown theme name must propagate");
        let msg = format!("{err:#}");
        assert!(msg.contains("no-such-theme"), "{msg}");
        assert!(
            msg.contains("not a built-in name") || msg.contains("failed to read"),
            "{msg}",
        );
    }

    // ── Config::load / extra_ca_certs ──

    #[tokio::test]
    async fn load_extra_ca_certs_default_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert!(config.extra_ca_certs.is_none());
    }

    #[tokio::test]
    async fn load_extra_ca_certs_env_beats_file_and_expands_tilde() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                extra_ca_certs = "/etc/ssl/from-file.pem"
            "#},
        );
        // Env supplies a `~`-prefixed path so the expansion branch is also covered.
        let vars = env_vars(vec![xdg(&dir), env("OX_EXTRA_CA_CERTS", "~/certs/env.pem")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        let home = std::env::home_dir().expect("home directory");
        assert_eq!(config.extra_ca_certs, Some(home.join("certs/env.pem")));
    }

    #[tokio::test]
    async fn load_extra_ca_certs_node_env_is_the_last_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("node.pem");
        std::fs::write(&bundle, crate::util::tls::TEST_CA_PEM).unwrap();
        let node = env("NODE_EXTRA_CA_CERTS", bundle.to_str().unwrap());

        let vars = env_vars(vec![xdg(&dir), node.clone()]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.extra_ca_certs, Some(bundle));

        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                extra_ca_certs = "/etc/ssl/from-file.pem"
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), node]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(
            config.extra_ca_certs,
            Some(PathBuf::from("/etc/ssl/from-file.pem")),
        );
    }

    #[tokio::test]
    async fn load_extra_ca_certs_skips_unreadable_node_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.pem");
        let vars = env_vars(vec![
            xdg(&dir),
            env("NODE_EXTRA_CA_CERTS", missing.to_str().unwrap()),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert!(config.extra_ca_certs.is_none());
    }

    #[tokio::test]
    async fn load_extra_ca_certs_file_used_when_env_unset() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                extra_ca_certs = "/etc/ssl/from-file.pem"
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(
            config.extra_ca_certs,
            Some(PathBuf::from("/etc/ssl/from-file.pem")),
        );
    }

    #[tokio::test]
    async fn load_extra_ca_certs_file_wins_when_env_is_empty() {
        // `env::string` treats an empty env var as absent, so a file value must survive even
        // when the caller exports `OX_EXTRA_CA_CERTS=""` (e.g., by unsetting via shell).
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                extra_ca_certs = "/etc/ssl/from-file.pem"
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("OX_EXTRA_CA_CERTS", "")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(
            config.extra_ca_certs,
            Some(PathBuf::from("/etc/ssl/from-file.pem")),
        );
    }

    // ── Config::load / base URL validation ──

    #[tokio::test]
    async fn load_rejects_plain_http_base_url_unless_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_BASE_URL", "http://example.com"),
        ]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("non-loopback http must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("https"), "{msg}");
        assert!(msg.contains("localhost"), "{msg}");
    }

    #[tokio::test]
    async fn load_rejects_non_http_base_url_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_BASE_URL", "ftp://example.com"),
        ]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("non-http schemes must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("http or https"), "{msg}");
        assert!(msg.contains("ftp"), "{msg}");
    }

    #[tokio::test]
    async fn load_accepts_loopback_http_base_url_for_local_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_BASE_URL", "http://127.0.0.1:8080"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.base_url, "http://127.0.0.1:8080");
    }

    // ── Config::load / effort resolution ──

    #[tokio::test]
    async fn load_effort_defaults_to_per_model_value() {
        for (model, expected) in [
            ("claude-opus-5-5", Some(Effort::Medium)),
            ("claude-sonnet-5-5", Some(Effort::High)),
            ("custom-model", None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_MODEL", model)]);
            let config = temp_env::async_with_vars(vars, Config::load())
                .await
                .unwrap();
            assert_eq!(config.effort, expected, "model={model}");
        }
    }

    #[tokio::test]
    async fn load_effort_env_overrides_per_model_default() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-opus-5-5"),
            env("ANTHROPIC_EFFORT", "low"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.effort, Some(Effort::Low));
    }

    #[tokio::test]
    async fn load_effort_pick_is_dropped_on_unknown_model() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "custom-model"),
            env("ANTHROPIC_EFFORT", "max"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.effort, None);
    }

    #[tokio::test]
    async fn load_effort_file_picks_up_when_env_unset() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                model = "claude-sonnet-5-5"
                effort = "medium"
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.effort, Some(Effort::Medium));
    }

    #[tokio::test]
    async fn load_effort_env_beats_file() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                model = "claude-opus-5-5"
                effort = "low"
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_EFFORT", "max")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.effort, Some(Effort::Max));
    }

    #[tokio::test]
    async fn load_effort_invalid_env_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_EFFORT", "insane")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("invalid effort must propagate");
        let msg = format!("{err:#}");
        assert!(msg.contains("ANTHROPIC_EFFORT"), "{msg}");
        assert!(msg.contains("insane"), "{msg}");
    }

    // ── Config::load / prompt_cache_ttl ──

    #[tokio::test]
    async fn load_prompt_cache_ttl_env_overrides_default() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![xdg(&dir), env("OX_PROMPT_CACHE_TTL", "5m")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::FiveMin);
    }

    #[tokio::test]
    async fn load_prompt_cache_ttl_file_picks_up_when_env_unset() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                prompt_cache_ttl = "5m"
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::FiveMin);
    }

    #[tokio::test]
    async fn load_prompt_cache_ttl_env_beats_file() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                prompt_cache_ttl = "5m"
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("OX_PROMPT_CACHE_TTL", "1h")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::OneHour);
    }

    #[tokio::test]
    async fn load_prompt_cache_ttl_invalid_env_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![xdg(&dir), env("OX_PROMPT_CACHE_TTL", "forever")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("invalid ttl must propagate");
        let msg = format!("{err:#}");
        assert!(msg.contains("OX_PROMPT_CACHE_TTL"), "{msg}");
        assert!(msg.contains("forever"), "{msg}");
    }

    // ── Config::snapshot ──

    #[test]
    fn snapshot_copies_every_user_facing_field_and_drops_secret() {
        // `/config` prints from the snapshot, so secrets must reduce to their source and names.
        let cfg = Config {
            auth: Auth::new(AuthSource::ClaudeCodeLogin, "token-must-not-leak"),
            base_url: "https://api.example.test".to_owned(),
            extra_ca_certs: Some(PathBuf::from("/etc/ssl/corp-ca.pem")),
            gateway: GatewayCompat {
                custom_headers: gateway::parse_headers("x-team: secret-team").unwrap(),
                attribution: false,
                structured_outputs: true,
            },
            model: ModelSpec::resolve("claude-test-1-0".to_owned()),
            effort: Some(Effort::Xhigh),
            max_tokens: None,
            max_tool_rounds: Some(100),
            prompt_cache_ttl: PromptCacheTtl::FiveMin,
            compaction: CompactionConfig::resolved_for_test(AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(42),
            }),
            permission: crate::permission::Policy::resolve(
                crate::permission::Mode::Plan,
                &[],
                &[],
                Path::new(""),
            )
            .unwrap(),
            show_thinking: true,
            show_welcome: false,
            status_line: vec![
                StatusLineSegment::CurrentDir,
                StatusLineSegment::ModelWithEffort,
            ],
            theme: Theme::default(),
            theme_name: "macchiato".to_owned(),
        };
        let snap = cfg.snapshot();
        assert_eq!(snap.auth, AuthSource::ClaudeCodeLogin);
        assert_eq!(snap.gateway.custom_header_names, ["x-team"]);
        assert!(!snap.gateway.attribution);
        assert!(snap.gateway.structured_outputs);
        let debug = format!("{snap:?}");
        assert!(!debug.contains("token-must-not-leak"), "{debug}");
        assert!(!debug.contains("secret-team"), "{debug}");
        assert_eq!(snap.base_url, "https://api.example.test");
        assert_eq!(
            snap.extra_ca_certs.as_deref(),
            Some(Path::new("/etc/ssl/corp-ca.pem")),
        );
        assert_eq!(snap.model.id(), "claude-test-1-0");
        assert_eq!(snap.effort, Some(Effort::Xhigh));
        assert_eq!(
            snap.max_tokens, 16_000,
            "unknown ids resolve to the smaller cap"
        );
        assert_eq!(snap.max_tool_rounds, Some(100));
        assert_eq!(snap.prompt_cache_ttl, PromptCacheTtl::FiveMin);
        assert_eq!(snap.compaction.auto.threshold_tokens, Some(42));
        assert_eq!(snap.permission_mode, crate::permission::Mode::Plan);
        assert!(snap.show_thinking);
        assert!(!snap.show_welcome);
        assert_eq!(
            snap.status_line,
            vec![
                StatusLineSegment::CurrentDir,
                StatusLineSegment::ModelWithEffort,
            ],
        );
        assert_eq!(snap.theme_name, "macchiato");
    }

    // ── display_effort ──

    #[test]
    fn display_effort_names_effective_tier_or_no_tier() {
        assert_eq!(display_effort(Some(Effort::High)), "high");
        assert_eq!(display_effort(None), "(no effort tier)");
    }

    // ── display_bool ──

    #[test]
    fn display_bool_names_the_two_flag_states() {
        assert_eq!(display_bool(true), "on");
        assert_eq!(display_bool(false), "off");
    }

    // ── display_auto_compaction ──

    #[test]
    fn display_auto_compaction_names_enabled_threshold_or_off() {
        assert_eq!(
            display_auto_compaction(AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(400_000),
            }),
            "at 400000 tokens",
        );
        assert_eq!(
            display_auto_compaction(AutoCompactionConfig {
                enabled: false,
                threshold_tokens: Some(400_000),
            }),
            "off",
        );
        assert_eq!(
            display_auto_compaction(AutoCompactionConfig {
                enabled: true,
                threshold_tokens: None,
            }),
            "off (no threshold)",
        );
    }

    // ── AutoCompactionConfig::should_trigger ──

    #[test]
    fn should_trigger_requires_enabled_threshold_and_enough_tokens() {
        let enabled = AutoCompactionConfig {
            enabled: true,
            threshold_tokens: Some(10),
        };
        assert!(enabled.should_trigger(10));
        assert!(!enabled.should_trigger(9));
        assert!(!AutoCompactionConfig::disabled().should_trigger(100));
        assert!(
            !AutoCompactionConfig {
                enabled: true,
                threshold_tokens: None,
            }
            .should_trigger(100)
        );
    }

    // ── is_first_party_base_url ──

    #[test]
    fn is_first_party_base_url_accepts_official_hosts() {
        assert!(is_first_party_base_url("https://api.anthropic.com"));
        assert!(is_first_party_base_url("https://api.anthropic.com/"));
        assert!(is_first_party_base_url("https://api-staging.anthropic.com"));
        assert!(is_first_party_base_url("https://API.ANTHROPIC.COM"));
    }

    #[test]
    fn is_first_party_base_url_rejects_proxies_and_malformed_urls() {
        assert!(!is_first_party_base_url("https://api.openai.com"));
        assert!(!is_first_party_base_url("https://proxy.example.com"));
        assert!(!is_first_party_base_url("https://anthropic.com.evil.io"));
        assert!(!is_first_party_base_url("http://127.0.0.1:8080"));
        assert!(!is_first_party_base_url(""));
        assert!(!is_first_party_base_url("not-a-url"));
    }

    // ── test_thresholds ──

    #[test]
    fn test_thresholds_pin_default_auto_threshold_per_window() {
        for info in crate::model::MODELS {
            let model = ModelSpec::resolve(info.id.to_owned());
            assert_eq!(
                default_auto_threshold(&model, 64_000),
                Some(test_thresholds::WINDOW_1M),
                "{}",
                info.id,
            );
        }
        let unknown = ModelSpec::resolve("custom-model".to_owned());
        assert_eq!(default_auto_threshold(&unknown, 64_000), None);
    }
}
