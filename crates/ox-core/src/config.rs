//! Configuration loading.
//!
//! Precedence (highest wins): env > project `.ox/config.toml` > user `config.d/*.toml` drop-ins >
//! user `~/.config/ox/config.toml` > defaults. Auth stops at the first source that resolves, in
//! [`auth::AuthSource`] order.

pub mod auth;
pub mod compaction;
pub mod gateway;
pub mod load;
pub(crate) mod login;
mod merge;
pub(crate) mod oauth;
pub mod provenance;
mod schema;

use std::fmt;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;

use self::auth::{Auth, AuthSource, LoginUnavailable, ResolveError};
use self::compaction::{AutoCompactionConfig, CompactionConfig};
use self::gateway::{GatewayCompat, GatewaySnapshot};
use self::load::Loaded;
use self::merge::Origins;
use self::oauth::ClaudeCodeKeychain;
use self::provenance::{Provenance, Source, Sourced};
use self::schema::{FileConfig, PermissionTable, TierTable};
use crate::model::ModelSpec;
use crate::permission::{Mode, Policy, Tier, Workspace};
use crate::util::env;
use crate::util::git::project_root;
use crate::util::path::expand_user;
use crate::util::tls::{PlatformTrust, Trust, load_extra_ca_certs};

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub(crate) const DEFAULT_THEME: &str = "mocha";

// ── ConfigSnapshot ──

/// Resolved-config view minus the secret. Survives [`Config`] being consumed by the client.
#[derive(Debug, Clone)]
pub struct ConfigSnapshot {
    pub model: ModelSpec,
    pub effort: Option<Effort>,
    /// `None` while signed out.
    pub auth: Option<AuthSource>,
    pub base_url: String,
    /// PEM bundle appended to reqwest's trust anchors.
    pub extra_ca_certs: Option<PathBuf>,
    pub gateway: GatewaySnapshot,
    /// User-set output cap. `None` defers to [`ModelSpec::resolve_max_tokens`].
    pub max_tokens: Option<u32>,
    /// `None` means the agent loop runs without a per-turn round cap.
    pub max_tool_rounds: Option<u32>,
    pub prompt_cache_ttl: PromptCacheTtl,
    pub compaction: CompactionConfig,
    pub permission_mode: Mode,
    pub show_thinking: bool,
    pub show_welcome: bool,
    pub status_line: Vec<StatusLineSegment>,
    /// Resolved theme base name, either a built-in catalog key or a filesystem path.
    pub theme_name: String,
    pub provenance: Provenance,
}

impl ConfigSnapshot {
    /// Applies a `/model` or `/effort` swap. A changed model and an honored effort pick become
    /// session-set, while an effort the new model derived on its own reverts to the default source.
    pub fn apply_swap(
        &mut self,
        model: ModelSpec,
        effort: Option<Effort>,
        requested_effort: Option<Effort>,
        auto_compaction: AutoCompactionConfig,
    ) {
        if model.id() != self.model.id() {
            self.provenance.model = Source::Session;
        }
        if requested_effort.is_some() && effort == requested_effort {
            self.provenance.effort = Source::Session;
        } else if effort != self.effort {
            self.provenance.effort = Source::Default;
        }
        self.model = model;
        self.effort = effort;
        self.compaction.auto = auto_compaction;
    }

    /// Applies a `/mode` or Shift+Tab switch, which `/config` then credits to the session.
    pub fn apply_mode(&mut self, mode: Mode) {
        self.permission_mode = mode;
        self.provenance.permission_mode = Source::Session;
    }
}

// ── Effort ──

/// Intelligence-vs-latency tier sent as `output_config.effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];
    pub(crate) const VALID_VALUES: &str = "low, medium, high, xhigh, max";

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// One-line UX hint for the typed-arg popup.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Low => "Fastest, shallow reasoning",
            Self::Medium => "Balanced",
            Self::High => "Deep reasoning",
            Self::Xhigh => "Extended thinking",
            Self::Max => "Maximum thinking",
        }
    }

    /// Notes that the model has no effort tier, so this requested effort did not apply.
    pub fn unapplied_notice(self) -> String {
        format!("Effort unchanged (model has no effort tier, asked for {self})")
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
            _ => bail!("expected one of {}", Self::VALID_VALUES),
        }
    }
}

// ── PromptCacheTtl ──

/// Sent as `cache_control.ttl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptCacheTtl {
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
            _ => bail!("expected one of 5m, 1h"),
        }
    }
}

// ── Status Line ──

/// User-configurable status-line segment identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusLineSegment {
    CurrentDir,
    GitBranch,
    PullRequest,
    PullRequestChecks,
    PullRequestReview,
    Model,
    ModelWithEffort,
    ContextUsed,
    SessionCost,
    FiveHourLimit,
    WeeklyLimit,
    PermissionMode,
    RunState,
    QueuedPrompts,
    TaskProgress,
    ThreadTitle,
    SessionId,
    Theme,
    CurrentTime,
}

impl StatusLineSegment {
    /// Names for `OX_STATUS_LINE` parsing and error messages, which must match the kebab-case names
    /// serde gives TOML config.
    const ALL: &'static [(Self, &'static str)] = &[
        (Self::CurrentDir, "current-dir"),
        (Self::GitBranch, "git-branch"),
        (Self::PullRequest, "pull-request"),
        (Self::PullRequestChecks, "pull-request-checks"),
        (Self::PullRequestReview, "pull-request-review"),
        (Self::Model, "model"),
        (Self::ModelWithEffort, "model-with-effort"),
        (Self::ContextUsed, "context-used"),
        (Self::SessionCost, "session-cost"),
        (Self::FiveHourLimit, "five-hour-limit"),
        (Self::WeeklyLimit, "weekly-limit"),
        (Self::PermissionMode, "permission-mode"),
        (Self::RunState, "run-state"),
        (Self::QueuedPrompts, "queued-prompts"),
        (Self::TaskProgress, "task-progress"),
        (Self::ThreadTitle, "thread-title"),
        (Self::SessionId, "session-id"),
        (Self::Theme, "theme"),
        (Self::CurrentTime, "current-time"),
    ];

    /// Order rationale documented in `docs/design/tui/status-line.md`.
    pub const DEFAULT: &'static [Self] = &[
        Self::CurrentDir,
        Self::GitBranch,
        Self::PullRequest,
        Self::ModelWithEffort,
        Self::ContextUsed,
        Self::SessionCost,
        Self::PermissionMode,
        Self::RunState,
        Self::ThreadTitle,
    ];

    fn names() -> String {
        Self::ALL
            .iter()
            .map(|(_, name)| *name)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl FromStr for StatusLineSegment {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if let Some((segment, _)) = Self::ALL.iter().find(|(_, name)| *name == s) {
            return Ok(*segment);
        }
        bail!("unknown segment {s:?}: expected one of {}", Self::names())
    }
}

// ── ThemeConfig ──

/// `[tui.theme]` as configured. The TUI resolves it into a palette.
#[derive(Debug, Clone)]
pub struct ThemeConfig {
    /// Built-in catalog key, such as `"mocha"`, or filesystem path, [`DEFAULT_THEME`] when unset.
    pub base: String,
    /// Where `base` came from. A project file may only name a built-in theme.
    pub base_source: Source,
    /// `[tui.theme.overrides]` slot patches, still raw TOML.
    pub overrides: toml::Table,
}

// ── Config ──

/// Resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub model: ModelSpec,
    /// `None` for models outside the catalog.
    pub(crate) effort: Option<Effort>,
    /// `None` while signed out, which only an interactive session starts in, since `/login` can
    /// sign it in.
    pub(crate) auth: Option<Auth>,
    /// Why the session started signed out.
    pub(crate) signed_out: Option<LoginUnavailable>,
    pub(crate) base_url: String,
    /// Resolved path to a PEM bundle appended to reqwest's trust anchors. `None` means the
    /// client trusts only the platform roots.
    pub extra_ca_certs: Option<PathBuf>,
    pub(crate) gateway: GatewayCompat,
    /// User-set output cap. `None` defers to [`ModelSpec::resolve_max_tokens`].
    pub(crate) max_tokens: Option<u32>,
    /// `None` means the agent loop runs without a per-turn round cap.
    pub(crate) max_tool_rounds: Option<u32>,
    pub(crate) prompt_cache_ttl: PromptCacheTtl,
    pub(crate) compaction: CompactionConfig,
    pub(crate) permission: Policy,
    /// The launch directory and the workspace roots the tools run in.
    pub(crate) workspace: Workspace,
    /// Whether adaptive thinking opts into summarized text. The request's `thinking` field
    /// derives from this plus the model.
    pub show_thinking: bool,
    pub(crate) show_welcome: bool,
    pub(crate) status_line: Vec<StatusLineSegment>,
    pub theme: ThemeConfig,
    pub(crate) provenance: Provenance,
}

impl Config {
    /// Resolves config from env, the config files [`load::load`] finds from `cwd` within its
    /// [`project_root`], and built-in defaults. A TOML or env parse error fails the load. Without
    /// a working credential, an `interactive` session starts signed out and any other session
    /// fails.
    pub async fn load(cwd: &Path, interactive: bool) -> Result<Self> {
        let Loaded { config, origins } = load::load(cwd, &project_root(cwd))?;
        let FileConfig {
            client,
            permission,
            tui,
        } = config;

        let base_url = env_string("ANTHROPIC_BASE_URL")
            .or_else(|| origins.sourced("client.base_url", client.base_url))
            .unwrap_or_else(|| Sourced::fallback(DEFAULT_BASE_URL.to_owned()));
        validate_base_url(&base_url.value)?;

        // Resolved before `auth`, since the OAuth refresh needs the extra trust anchors too.
        let extra_ca_certs = resolve_extra_ca_certs(
            origins.sourced("client.extra_ca_certs", client.extra_ca_certs),
        )?;

        let gateway = GatewayCompat::load()?;

        let credential = resolve_auth(
            client.api_key,
            &origins,
            &base_url.value,
            &PlatformTrust(extra_ca_certs.value.clone()),
            &gateway,
            interactive,
        )
        .await?;

        let model_id = env_string("ANTHROPIC_MODEL")
            .or_else(|| origins.sourced("client.model", client.model))
            .unwrap_or_else(|| Sourced::fallback(DEFAULT_MODEL.to_owned()));
        let model = ModelSpec::resolve(model_id.value);

        let effort = resolve_effort(&model, client.effort, &origins)?;

        let max_tokens = Sourced::from_option(
            env_number("OX_MAX_TOKENS", 1..=u32::MAX)?
                .or_else(|| origins.sourced("client.max_tokens", client.max_tokens)),
        );
        let max_tool_rounds = Sourced::from_option(
            env_number("OX_MAX_TOOL_ROUNDS", 1..=u32::MAX)?
                .or_else(|| origins.sourced("client.max_tool_rounds", client.max_tool_rounds)),
        );

        let show_thinking = env_bool("OX_SHOW_THINKING")?
            .or_else(|| origins.sourced("tui.show_thinking", tui.show_thinking))
            .unwrap_or_else(|| Sourced::fallback(false));

        let show_welcome = env::bool("OX_SHOW_WELCOME")?
            .or(tui.show_welcome)
            .unwrap_or(true);

        let status_line = resolve_status_line(tui.status_line)?;

        let prompt_cache_ttl = env_parse::<PromptCacheTtl>("OX_PROMPT_CACHE_TTL")?
            .or_else(|| origins.sourced("client.prompt_cache_ttl", client.prompt_cache_ttl))
            .unwrap_or_else(|| Sourced::fallback(PromptCacheTtl::OneHour));

        let compaction = compaction::resolve(
            &client.compaction,
            &origins,
            &model,
            model.resolve_max_tokens(max_tokens.value),
        )?;

        let (permission, workspace, permission_mode) =
            resolve_permission(permission, &origins, cwd)?;

        let theme = ThemeConfig {
            base: tui.theme.base.unwrap_or_else(|| DEFAULT_THEME.to_owned()),
            base_source: origins.source("tui.theme.base"),
            overrides: tui.theme.overrides,
        };

        let provenance = Provenance {
            layers: origins.into_layers(),
            model: model_id.source,
            effort: effort.source,
            auth: credential.source,
            base_url: base_url.source,
            extra_ca_certs: extra_ca_certs.source,
            custom_headers: Source::env_if_set(gateway::CUSTOM_HEADERS_VAR),
            attribution: Source::env_if_set("CLAUDE_CODE_ATTRIBUTION_HEADER"),
            structured_outputs: Source::env_if_set("CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS"),
            max_tokens: max_tokens.source,
            max_tool_rounds: max_tool_rounds.source,
            prompt_cache_ttl: prompt_cache_ttl.source,
            compaction: compaction.source,
            permission_mode,
            show_thinking: show_thinking.source,
        };

        Ok(Self {
            model,
            effort: effort.value,
            auth: credential.auth,
            signed_out: credential.signed_out,
            base_url: base_url.value,
            extra_ca_certs: extra_ca_certs.value,
            gateway,
            max_tokens: max_tokens.value,
            max_tool_rounds: max_tool_rounds.value,
            prompt_cache_ttl: prompt_cache_ttl.value,
            compaction: compaction.value,
            permission,
            workspace,
            show_thinking: show_thinking.value,
            show_welcome,
            status_line,
            theme,
            provenance,
        })
    }

    /// Startup notices: an unsupported model, then why the session is signed out.
    pub(crate) fn notices(&self) -> Vec<String> {
        self.model
            .unsupported_notice()
            .into_iter()
            .chain(
                self.signed_out
                    .as_ref()
                    .map(|signed_out| signed_out.notice.clone()),
            )
            .collect()
    }

    /// Applies `--mode`, which overrides the config files and `OX_PERMISSION_MODE`.
    pub fn apply_mode_flag(&mut self, mode: Mode) {
        self.permission.set_mode(mode);
        self.provenance.permission_mode = Source::Flag("--mode");
    }

    /// Descriptors for `/config` and `/status`, minus the auth secret.
    pub fn snapshot(&self) -> ConfigSnapshot {
        ConfigSnapshot {
            model: self.model.clone(),
            effort: self.effort,
            auth: self.auth.as_ref().map(Auth::source),
            base_url: self.base_url.clone(),
            extra_ca_certs: self.extra_ca_certs.clone(),
            gateway: self.gateway.snapshot(),
            max_tokens: self.max_tokens,
            max_tool_rounds: self.max_tool_rounds,
            prompt_cache_ttl: self.prompt_cache_ttl,
            compaction: self.compaction,
            permission_mode: self.permission.mode(),
            show_thinking: self.show_thinking,
            show_welcome: self.show_welcome,
            status_line: self.status_line.clone(),
            theme_name: self.theme.base.clone(),
            provenance: self.provenance.clone(),
        }
    }
}

// ── Resolution ──

/// `OX_EXTRA_CA_CERTS` > `client.extra_ca_certs` > `NODE_EXTRA_CA_CERTS`. Node is often exported
/// shell-wide for npm, so like Node itself, an unreadable bundle there warns and is skipped.
fn resolve_extra_ca_certs(file: Option<Sourced<String>>) -> Result<Sourced<Option<PathBuf>>> {
    let explicit = env_string("OX_EXTRA_CA_CERTS")
        .map(|raw| ("OX_EXTRA_CA_CERTS", raw))
        .or(file.map(|raw| ("client.extra_ca_certs", raw)));
    if let Some((key, raw)) = explicit {
        let path = expand_user(&raw.value).with_context(|| format!("invalid {key}"))?;
        return Ok(Sourced::new(Some(path), raw.source));
    }

    let Some(raw) = env_string("NODE_EXTRA_CA_CERTS") else {
        return Ok(Sourced::fallback(None));
    };
    match expand_user(&raw.value).and_then(|path| load_extra_ca_certs(&path).map(|_| path)) {
        Ok(path) => Ok(Sourced::new(Some(path), raw.source)),
        Err(e) => {
            warn!("ignoring NODE_EXTRA_CA_CERTS: {e:#}");
            Ok(Sourced::fallback(None))
        }
    }
}

/// The session credential, where it came from, and why the session is signed out without one.
struct ResolvedAuth {
    auth: Option<Auth>,
    source: Source,
    signed_out: Option<LoginUnavailable>,
}

/// Resolves the credential and its source, rejecting a custom header that would replace the
/// credential. A stored login is the fallback when nothing configures one, so its source is the
/// default. A signed-out session checks the credential header used by `/login`.
async fn resolve_auth(
    file_api_key: Option<String>,
    origins: &Origins,
    base_url: &str,
    trust: &dyn Trust,
    gateway: &GatewayCompat,
    interactive: bool,
) -> Result<ResolvedAuth> {
    let resolved = auth::resolve(
        auth::explicit_credentials(file_api_key),
        base_url,
        auth::stored_login(ClaudeCodeKeychain::SYSTEM, trust),
    )
    .await;
    let (auth, signed_out) = match resolved {
        Ok(auth) => (Some(auth), None),
        Err(ResolveError::SignedOut(e)) if interactive => (None, Some(LoginUnavailable::new(&e))),
        Err(e) => return Err(e.into()),
    };
    let source = auth.as_ref().map_or(AuthSource::OxLogin, Auth::source);
    gateway.check_credential_header(source)?;

    let source = match source {
        AuthSource::ApiKeyFile => origins.source("client.api_key"),
        source => source.env_var().map_or(Source::Default, Source::Env),
    };
    Ok(ResolvedAuth {
        auth,
        source,
        signed_out,
    })
}

/// The effort the model takes. A pick the model ignores leaves the source at the default.
fn resolve_effort(
    model: &ModelSpec,
    file: Option<Effort>,
    origins: &Origins,
) -> Result<Sourced<Option<Effort>>> {
    let pick = env_parse::<Effort>("OX_EFFORT")?.or_else(|| origins.sourced("client.effort", file));
    let effort = model.resolve_effort(pick.as_ref().map(|pick| pick.value));
    Ok(match pick {
        Some(pick) if effort.is_some() => Sourced::new(effort, pick.source),
        _ => Sourced::fallback(effort),
    })
}

/// `OX_STATUS_LINE` > `tui.status_line` > the default roster.
fn resolve_status_line(file: Option<Vec<StatusLineSegment>>) -> Result<Vec<StatusLineSegment>> {
    match env::string("OX_STATUS_LINE") {
        Some(raw) => parse_status_line_segments(&raw)
            .with_context(|| format!("invalid OX_STATUS_LINE {raw:?}")),
        None => validate_status_line(file.unwrap_or_else(|| StatusLineSegment::DEFAULT.to_vec()))
            .context("invalid tui.status_line"),
    }
}

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
                bail!("empty segment between commas. Remove the stray comma.");
            }
            part.parse::<StatusLineSegment>()
        })
        .collect::<Result<Vec<_>>>()
        .and_then(validate_status_line)
}

fn validate_status_line(segments: Vec<StatusLineSegment>) -> Result<Vec<StatusLineSegment>> {
    if segments.is_empty() {
        bail!(
            "no segments. List at least one of {}, or remove it to use the default.",
            StatusLineSegment::names()
        );
    }
    Ok(segments)
}

/// Resolves the permission policy, the workspace launched in `cwd`, and the mode's source from the
/// file config and `OX_PERMISSION_MODE`, which wins over the file mode.
fn resolve_permission(
    file: PermissionTable,
    origins: &Origins,
    cwd: &Path,
) -> Result<(Policy, Workspace, Source)> {
    let workspace = Workspace::resolve(cwd, &file.extra_roots.unwrap_or_default())?;
    let mode = env_parse::<Mode>("OX_PERMISSION_MODE")?
        .or_else(|| origins.sourced("permission.mode", file.mode))
        .unwrap_or_else(|| Sourced::fallback(Mode::default()));

    let rules = |table: Option<TierTable>, tier| {
        table.map_or_else(|| Ok(Vec::new()), |table| table.rules(tier))
    };
    let disabled: Vec<_> = file
        .defaults
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(group, on)| (!on).then_some(group))
        .collect();
    let policy = Policy::resolve(
        mode.value,
        rules(file.deny, Tier::Deny)?,
        rules(file.ask, Tier::Ask)?,
        rules(file.allow, Tier::Allow)?,
        &disabled,
        &workspace,
    )?;
    Ok((policy, workspace, mode.source))
}

// ── Env Overrides ──

fn env_string(var: &'static str) -> Option<Sourced<String>> {
    env::string(var).map(|value| Sourced::new(value, Source::Env(var)))
}

fn env_bool(var: &'static str) -> Result<Option<Sourced<bool>>> {
    Ok(env::bool(var)?.map(|value| Sourced::new(value, Source::Env(var))))
}

fn env_parse<T>(var: &'static str) -> Result<Option<Sourced<T>>>
where
    T: FromStr<Err = anyhow::Error>,
{
    env_string(var)
        .map(|raw| {
            let value = raw
                .value
                .parse::<T>()
                .map_err(|e| anyhow!("invalid {var} {:?}: {e:#}", raw.value))?;
            Ok(Sourced::new(value, raw.source))
        })
        .transpose()
}

fn env_number<T>(var: &'static str, range: RangeInclusive<T>) -> Result<Option<Sourced<T>>>
where
    T: fmt::Display + FromStr + PartialOrd,
{
    Ok(env::number(var, range)?.map(|value| Sourced::new(value, Source::Env(var))))
}

// ── Base URL ──

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

/// Returns whether `base_url` points at Anthropic's production or staging host. Gates the
/// stored-login fallback, body-side `cache_control.scope = "global"`, and the billing `cch` hash.
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

// ── Display ──

pub fn display_auth(auth: Option<AuthSource>) -> String {
    auth.map_or_else(|| "Not signed in".to_owned(), |source| source.to_string())
}

pub fn display_effort(effort: Option<Effort>) -> String {
    effort.map_or_else(|| "(no effort tier)".to_owned(), |e| e.to_string())
}

pub fn display_bool(flag: bool) -> &'static str {
    if flag { "on" } else { "off" }
}

pub fn display_max_tool_rounds(cap: Option<u32>) -> String {
    cap.map_or_else(|| "unbounded".to_owned(), |n| n.to_string())
}

pub fn display_auto_compaction(auto: AutoCompactionConfig) -> String {
    match (auto.enabled, auto.threshold_tokens) {
        (true, Some(threshold)) => format!("at {threshold} tokens"),
        (true, None) => "off (no threshold)".to_owned(),
        _ => "off".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;

    use indoc::indoc;
    use tempfile::TempDir;

    use super::compaction::testing::WINDOW_1M_THRESHOLD;
    use super::oauth::LoginError;
    use super::oauth::testing::write_creds;
    use super::provenance::Layer;
    use super::*;
    use crate::permission::{Access, Decision, PermissionClass};
    use crate::util::path::testing::HOME_VAR;
    use crate::util::tls::local_testing::TEST_CA_PEM;

    fn sample_config() -> Config {
        Config {
            auth: Some(Auth::new(
                AuthSource::ClaudeCodeLogin,
                "token-must-not-leak",
            )),
            signed_out: None,
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
            compaction: CompactionConfig::with_threshold_for_test(42),
            permission: Policy::new(Mode::Plan, Vec::new(), Vec::new(), Vec::new()),
            workspace: Workspace::new(Path::new("/repo")),
            show_thinking: true,
            show_welcome: false,
            status_line: vec![
                StatusLineSegment::CurrentDir,
                StatusLineSegment::ModelWithEffort,
            ],
            theme: ThemeConfig {
                base: "macchiato".to_owned(),
                base_source: Source::Default,
                overrides: toml::Table::new(),
            },
            provenance: Provenance {
                model: Source::Env("ANTHROPIC_MODEL"),
                effort: Source::Env("OX_EFFORT"),
                ..Provenance::default()
            },
        }
    }

    mod config_snapshot {
        use super::*;

        mod apply_swap {
            use super::*;

            #[test]
            fn marks_the_model_and_an_honored_effort_pick_as_session_set() {
                let mut snap = sample_config().snapshot();
                let model = snap.model.clone();
                let auto = AutoCompactionConfig::disabled();

                snap.apply_swap(model.clone(), Some(Effort::Xhigh), None, auto);
                assert_eq!(snap.provenance.model, Source::Env("ANTHROPIC_MODEL"));
                assert_eq!(snap.provenance.effort, Source::Env("OX_EFFORT"));
                assert_eq!(snap.compaction.auto, auto);

                snap.apply_swap(model, Some(Effort::Low), Some(Effort::Low), auto);
                assert_eq!(snap.provenance.model, Source::Env("ANTHROPIC_MODEL"));
                assert_eq!(snap.provenance.effort, Source::Session);
                assert_eq!(snap.effort, Some(Effort::Low));

                let opus = ModelSpec::resolve("claude-opus-5-5".to_owned());
                snap.apply_swap(opus, Some(Effort::Low), None, auto);
                assert_eq!(snap.provenance.model, Source::Session);
                assert_eq!(snap.provenance.effort, Source::Session);
                assert_eq!(snap.model.id(), "claude-opus-5-5");
            }

            #[test]
            fn reverts_an_effort_the_model_derived_to_the_default_source() {
                let mut snap = sample_config().snapshot();
                let custom = ModelSpec::resolve("gateway-alias".to_owned());
                let auto = AutoCompactionConfig::disabled();

                snap.apply_swap(custom, None, Some(Effort::High), auto);
                assert_eq!(snap.provenance.effort, Source::Default);
                assert_eq!(snap.effort, None);

                let opus = ModelSpec::resolve("claude-opus-5-5".to_owned());
                snap.apply_swap(opus, Some(Effort::Medium), None, auto);
                assert_eq!(snap.provenance.effort, Source::Default);
                assert_eq!(snap.effort, Some(Effort::Medium));
            }
        }

        mod apply_mode {
            use super::*;

            #[test]
            fn marks_the_switch_as_session_set() {
                let mut snap = sample_config().snapshot();

                snap.apply_mode(Mode::Plan);

                assert_eq!(snap.permission_mode, Mode::Plan);
                assert_eq!(snap.provenance.permission_mode, Source::Session);
            }
        }
    }

    mod effort {
        use super::*;

        #[test]
        fn round_trips_through_serde_and_fromstr() {
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
        fn round_trips_through_toml_deserialize() {
            #[derive(Deserialize)]
            struct Wrap {
                effort: Effort,
            }
            let wrap: Wrap = toml::from_str(r#"effort = "xhigh""#).unwrap();
            assert_eq!(wrap.effort, Effort::Xhigh);
        }

        mod from_str {
            use super::*;

            #[test]
            fn rejects_unknown_tokens_listing_the_accepted_ones() {
                let err = "extra-high".parse::<Effort>().expect_err("unknown token");
                assert_eq!(
                    format!("{err:#}"),
                    "expected one of low, medium, high, xhigh, max"
                );
            }
        }
    }

    mod prompt_cache_ttl {
        use super::*;

        #[test]
        fn round_trips_through_serde_and_fromstr() {
            for (variant, token) in [
                (PromptCacheTtl::FiveMin, "5m"),
                (PromptCacheTtl::OneHour, "1h"),
            ] {
                assert_eq!(serde_json::to_value(variant).unwrap(), token);
                assert_eq!(variant.to_string(), token);
                assert_eq!(token.parse::<PromptCacheTtl>().unwrap(), variant);
            }
        }

        mod wire {
            use super::*;

            #[test]
            fn omits_the_five_minute_server_default() {
                assert_eq!(PromptCacheTtl::FiveMin.wire(), None);
                assert_eq!(PromptCacheTtl::OneHour.wire(), Some("1h"));
            }
        }

        mod from_str {
            use super::*;

            #[test]
            fn rejects_unknown_tokens_listing_the_accepted_ones() {
                let err = "30m".parse::<PromptCacheTtl>().expect_err("unknown token");
                assert_eq!(format!("{err:#}"), "expected one of 5m, 1h");
            }
        }
    }

    mod status_line_segment {
        use super::*;

        #[test]
        fn names_round_trip_through_serde_and_fromstr() {
            for &(segment, name) in StatusLineSegment::ALL {
                assert_eq!(serde_json::to_value(segment).unwrap(), name);
                assert_eq!(name.parse::<StatusLineSegment>().unwrap(), segment);
            }
        }
    }

    mod load {
        use super::*;

        /// Env keys `Config::load` reads. `ANTHROPIC_API_KEY` defaults non-empty in [`env_vars`] so
        /// tests land on `ApiKey` without consulting the stored login.
        const ENV_KEYS: &[&str] = &[
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_CUSTOM_HEADERS",
            "CLAUDE_CODE_ATTRIBUTION_HEADER",
            "CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS",
            "ANTHROPIC_MODEL",
            "ANTHROPIC_BASE_URL",
            "OX_EFFORT",
            "OX_MAX_TOKENS",
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
            "XDG_DATA_HOME",
        ];

        fn write_user_config(xdg_dir: &Path, body: &str) -> Layer {
            let path = xdg_dir.join("ox").join("config.toml");
            write_config(path.clone(), body);
            Layer::User(path)
        }

        fn write_drop_in(xdg_dir: &Path, name: &str, body: &str) -> Layer {
            let path = xdg_dir.join("ox").join("config.d").join(name);
            write_config(path.clone(), body);
            Layer::DropIn(path)
        }

        fn write_config(path: PathBuf, body: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
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

        /// Loads a non-interactive session from the user config under `dir` and the baseline env
        /// plus `overrides`.
        async fn load_with(dir: &TempDir, overrides: &[(&'static str, &str)]) -> Result<Config> {
            let overrides = overrides.iter().map(|&(key, value)| env(key, value));
            let vars = env_vars([xdg(dir)].into_iter().chain(overrides));
            temp_env::async_with_vars(vars, Config::load(dir.path(), false)).await
        }

        fn assert_auth(config: &Config, source: AuthSource, secret: &str) {
            let auth = config.auth.as_ref().expect("the session must be signed in");
            assert_eq!(auth.source(), source);
            assert_eq!(auth.secret(), secret);
        }

        /// Env with no explicit credential and an oxide-code login under `data` that expires at
        /// `expires_at`, so the stored-login search stops before Claude Code's store.
        fn ox_login_env(
            config: &TempDir,
            data: &TempDir,
            expires_at: u64,
        ) -> Vec<(&'static str, Option<String>)> {
            write_config(
                data.path().join("ox").join("credentials.json"),
                &serde_json::json!({
                    "claudeAiOauth": { "accessToken": "ox-token", "expiresAt": expires_at },
                })
                .to_string(),
            );
            env_vars(vec![
                xdg(config),
                ("ANTHROPIC_API_KEY", None),
                (
                    "XDG_DATA_HOME",
                    Some(data.path().to_string_lossy().into_owned()),
                ),
            ])
        }

        #[tokio::test]
        async fn defaults_apply_when_no_config_and_no_env() {
            let dir = tempfile::tempdir().unwrap();
            let config = load_with(&dir, &[]).await.unwrap();
            assert_eq!(config.model.id(), "claude-opus-5-5");
            assert_eq!(config.base_url, DEFAULT_BASE_URL);
            assert_eq!(config.max_tokens, None);
            assert_eq!(config.effort, Some(Effort::Medium));
            assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::OneHour);
            assert!(config.compaction.auto.enabled);
            assert_eq!(
                config.compaction.auto.threshold_tokens,
                Some(WINDOW_1M_THRESHOLD),
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
            assert_eq!(config.theme.base, DEFAULT_THEME);
            assert!(config.theme.overrides.is_empty());
            assert_eq!(config.permission.mode(), Mode::Manual);
            assert_eq!(
                config.provenance,
                Provenance {
                    auth: Source::Env("ANTHROPIC_API_KEY"),
                    ..Provenance::default()
                },
            );
        }

        #[tokio::test]
        async fn theme_passes_base_and_raw_overrides_through() {
            let dir = tempfile::tempdir().unwrap();
            let layer = write_user_config(
                dir.path(),
                indoc! {r##"
                    [tui.theme]
                    base = "latte"

                    [tui.theme.overrides]
                    error = "#ff0000"
                    accent = { bold = false }
                "##},
            );
            let config = load_with(&dir, &[]).await.unwrap();
            assert_eq!(config.theme.base, "latte");
            assert_eq!(config.theme.base_source, Source::File(layer));
            assert_eq!(
                config.theme.overrides,
                indoc! {r##"
                    error = "#ff0000"
                    accent = { bold = false }
                "##}
                .parse::<toml::Table>()
                .unwrap(),
            );
        }

        #[tokio::test]
        async fn env_overrides_every_client_field() {
            let dir = tempfile::tempdir().unwrap();
            let config = load_with(
                &dir,
                &[
                    ("ANTHROPIC_MODEL", "claude-sonnet-5-5"),
                    ("ANTHROPIC_BASE_URL", "https://example.invalid"),
                    ("OX_MAX_TOKENS", "64"),
                    ("OX_MAX_TOOL_ROUNDS", "200"),
                    ("OX_SHOW_THINKING", "1"),
                    ("OX_SHOW_WELCOME", "0"),
                    ("OX_STATUS_LINE", "run-state,model,current-dir"),
                ],
            )
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
        async fn user_config_supplies_values_without_env_overrides() {
            let dir = tempfile::tempdir().unwrap();
            let user = write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    model = "claude-haiku-5-5"
                    base_url = "https://config-file.invalid"
                    extra_ca_certs = "/etc/ssl/corp-ca.pem"
                    max_tokens = 128
                    max_tool_rounds = 75

                    [client.compaction]
                    auto_threshold_percent = 40

                    [tui]
                    show_thinking = true
                    show_welcome = false
                    status_line = ["current-dir", "git-branch", "model-with-effort"]
                "#},
            );
            let config = load_with(&dir, &[]).await.unwrap();
            assert_eq!(config.model.id(), "claude-haiku-5-5");
            assert_eq!(config.base_url, "https://config-file.invalid");
            assert_eq!(
                config.extra_ca_certs.as_deref(),
                Some(Path::new("/etc/ssl/corp-ca.pem"))
            );
            assert_eq!(config.max_tokens, Some(128));
            assert_eq!(config.max_tool_rounds, Some(75));
            assert!(config.compaction.auto.enabled);
            let file = Source::File(user.clone());
            assert_eq!(
                config.provenance,
                Provenance {
                    layers: vec![user],
                    model: file.clone(),
                    auth: Source::Env("ANTHROPIC_API_KEY"),
                    base_url: file.clone(),
                    extra_ca_certs: file.clone(),
                    max_tokens: file.clone(),
                    max_tool_rounds: file.clone(),
                    compaction: file.clone(),
                    show_thinking: file,
                    ..Provenance::default()
                },
            );
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
        async fn env_beats_config_file_field_by_field() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    model = "claude-haiku-5-5"
                    max_tokens = 128

                    [tui]
                    show_thinking = true
                "#},
            );
            let config = load_with(
                &dir,
                &[
                    ("ANTHROPIC_MODEL", "claude-sonnet-5-5"),
                    // `max_tokens` has no env value, so the file value must win.
                    ("OX_SHOW_THINKING", "0"),
                ],
            )
            .await
            .unwrap();
            assert_eq!(config.model.id(), "claude-sonnet-5-5");
            assert_eq!(config.max_tokens, Some(128));
            assert!(!config.show_thinking, "env `0` overrides file `true`");
        }

        #[tokio::test]
        async fn numeric_env_errors_name_the_variable_and_range() {
            let whole = format!("1 to {}", u32::MAX);
            for (var, raw, range) in [
                ("OX_MAX_TOKENS", "32k", whole.clone()),
                ("OX_MAX_TOKENS", "0", whole.clone()),
                ("OX_MAX_TOOL_ROUNDS", "-3", whole),
                (
                    "OX_COMPACTION_AUTO_THRESHOLD_TOKENS",
                    "4294967296",
                    format!("0 to {}", u32::MAX),
                ),
                (
                    "OX_COMPACTION_AUTO_THRESHOLD_PERCENT",
                    "300",
                    "1 to 100".to_owned(),
                ),
                (
                    "OX_COMPACTION_AUTO_THRESHOLD_PERCENT",
                    "0",
                    "1 to 100".to_owned(),
                ),
            ] {
                let dir = tempfile::tempdir().unwrap();
                // A bad threshold must fail the load even with auto compaction off.
                let err = load_with(&dir, &[("OX_COMPACTION_AUTO_ENABLED", "0"), (var, raw)])
                    .await
                    .expect_err("an invalid number must fail config load");
                assert_eq!(
                    format!("{err:#}"),
                    format!("invalid {var} {raw:?}: expected a whole number from {range}"),
                );
            }
        }

        #[tokio::test]
        async fn propagates_invalid_config_file() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    api_key = "sk-from-file"
                    show_thinking = true
                "#},
            );
            let err = load_with(&dir, &[("ANTHROPIC_API_KEY", "")])
                .await
                .expect_err("misplaced field must surface as an error");
            let msg = format!("{err:#}");
            assert!(msg.contains("invalid config at"), "{msg}");
            assert!(msg.contains("unknown field `show_thinking`"), "{msg}");
        }

        #[tokio::test]
        async fn rejects_plain_http_base_url_unless_loopback() {
            let dir = tempfile::tempdir().unwrap();
            let err = load_with(&dir, &[("ANTHROPIC_BASE_URL", "http://example.com")])
                .await
                .expect_err("non-loopback http must be rejected");
            let msg = format!("{err:#}");
            assert!(msg.contains("https"), "{msg}");
            assert!(msg.contains("localhost"), "{msg}");
        }

        #[tokio::test]
        async fn rejects_non_http_base_url_scheme() {
            let dir = tempfile::tempdir().unwrap();
            let err = load_with(&dir, &[("ANTHROPIC_BASE_URL", "ftp://example.com")])
                .await
                .expect_err("non-http schemes must be rejected");
            let msg = format!("{err:#}");
            assert!(msg.contains("http or https"), "{msg}");
            assert!(msg.contains("ftp"), "{msg}");
        }

        #[tokio::test]
        async fn accepts_loopback_http_base_url_for_local_proxy() {
            let dir = tempfile::tempdir().unwrap();
            let config = load_with(&dir, &[("ANTHROPIC_BASE_URL", "http://127.0.0.1:8080")])
                .await
                .unwrap();
            assert_eq!(config.base_url, "http://127.0.0.1:8080");
        }

        #[tokio::test]
        async fn extra_ca_certs_default_to_no_bundle() {
            let dir = tempfile::tempdir().unwrap();
            let config = load_with(&dir, &[]).await.unwrap();
            assert!(config.extra_ca_certs.is_none());
        }

        #[tokio::test]
        async fn extra_ca_certs_env_beats_file_and_expands_tilde() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    extra_ca_certs = "/etc/ssl/from-file.pem"
                "#},
            );
            let home = [(HOME_VAR, Some(dir.path()))];
            let overrides = [("OX_EXTRA_CA_CERTS", "~/certs/env.pem")];
            let config = Box::pin(temp_env::async_with_vars(home, load_with(&dir, &overrides)))
                .await
                .unwrap();
            assert_eq!(
                config.extra_ca_certs,
                Some(dir.path().join("certs/env.pem"))
            );
        }

        #[tokio::test]
        async fn extra_ca_certs_node_env_is_the_last_fallback() {
            let dir = tempfile::tempdir().unwrap();
            let bundle = dir.path().join("node.pem");
            std::fs::write(&bundle, TEST_CA_PEM).unwrap();
            let node = [("NODE_EXTRA_CA_CERTS", bundle.to_str().unwrap())];

            let config = load_with(&dir, &node).await.unwrap();
            assert_eq!(config.extra_ca_certs.as_deref(), Some(bundle.as_path()));

            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    extra_ca_certs = "/etc/ssl/from-file.pem"
                "#},
            );
            let config = load_with(&dir, &node).await.unwrap();
            assert_eq!(
                config.extra_ca_certs,
                Some(PathBuf::from("/etc/ssl/from-file.pem")),
            );
        }

        #[tokio::test]
        async fn extra_ca_certs_skips_unreadable_node_bundle() {
            let dir = tempfile::tempdir().unwrap();
            let missing = dir.path().join("missing.pem");
            let config = load_with(&dir, &[("NODE_EXTRA_CA_CERTS", missing.to_str().unwrap())])
                .await
                .unwrap();
            assert!(config.extra_ca_certs.is_none());
        }

        #[tokio::test]
        async fn extra_ca_certs_file_used_when_env_unset() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    extra_ca_certs = "/etc/ssl/from-file.pem"
                "#},
            );
            let config = load_with(&dir, &[]).await.unwrap();
            assert_eq!(
                config.extra_ca_certs,
                Some(PathBuf::from("/etc/ssl/from-file.pem")),
            );
        }

        #[tokio::test]
        async fn extra_ca_certs_file_wins_when_env_is_empty() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    extra_ca_certs = "/etc/ssl/from-file.pem"
                "#},
            );
            let config = load_with(&dir, &[("OX_EXTRA_CA_CERTS", "")]).await.unwrap();
            assert_eq!(
                config.extra_ca_certs,
                Some(PathBuf::from("/etc/ssl/from-file.pem")),
            );
        }

        #[tokio::test]
        async fn env_api_key_beats_file_api_key() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    api_key = "sk-from-file"
                "#},
            );
            let config = load_with(&dir, &[("ANTHROPIC_API_KEY", "sk-from-env")])
                .await
                .unwrap();
            assert_auth(&config, AuthSource::ApiKeyEnv, "sk-from-env");
        }

        #[tokio::test]
        async fn file_api_key_used_when_env_is_empty() {
            let dir = tempfile::tempdir().unwrap();
            let user = write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    api_key = "sk-from-file"
                "#},
            );
            let config = load_with(&dir, &[("ANTHROPIC_API_KEY", "")]).await.unwrap();
            assert_auth(&config, AuthSource::ApiKeyFile, "sk-from-file");
            assert_eq!(config.provenance.auth, Source::File(user));
        }

        #[tokio::test]
        async fn resolves_credentials_in_precedence_order() {
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
                        indoc! {r#"
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
                let config = temp_env::async_with_vars(vars, Config::load(dir.path(), false))
                    .await
                    .unwrap();
                assert_auth(&config, winner, secret);
            }
        }

        #[tokio::test]
        async fn falls_back_to_the_oxide_code_login() {
            let (dir, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let vars = ox_login_env(&dir, &data, 9_999_999_999_999);
            let config = temp_env::async_with_vars(vars, Config::load(dir.path(), false))
                .await
                .unwrap();
            assert_auth(&config, AuthSource::OxLogin, "ox-token");
            assert_eq!(config.provenance.auth, Source::Default);
            assert!(config.signed_out.is_none());
        }

        #[tokio::test]
        async fn starts_an_interactive_session_signed_out_when_the_stored_login_fails() {
            let (dir, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let vars = ox_login_env(&dir, &data, 1);
            let (interactive, other) = temp_env::async_with_vars(vars, async {
                (
                    Config::load(dir.path(), true).await,
                    Config::load(dir.path(), false).await,
                )
            })
            .await;

            let interactive = interactive.unwrap();
            assert!(interactive.auth.is_none());
            let signed_out = interactive.signed_out.expect("signed out");
            assert_eq!(
                signed_out.notice,
                "Not signed in: oxide-code login has expired. Run `/login` to sign in again.",
            );
            assert!(!signed_out.retry, "an expired login needs a new sign-in");
            assert_eq!(
                format!(
                    "{:#}",
                    other.expect_err("only an interactive session starts signed out")
                ),
                "oxide-code login has expired. Run `/login` to sign in again.",
            );
        }

        #[tokio::test(start_paused = true)]
        async fn retries_a_stored_login_a_held_lock_kept_out() {
            let (dir, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let vars = ox_login_env(&dir, &data, 0);
            let store = data.path().join("ox");
            write_creds(&store.join("credentials.json"), "dead", Some("refresh"), 0);
            std::fs::create_dir(store.join("credentials.lock")).unwrap();

            let config = temp_env::async_with_vars(vars, Config::load(dir.path(), true))
                .await
                .unwrap();

            let signed_out = config.signed_out.expect("signed out");
            assert_eq!(
                signed_out.notice,
                format!(
                    "Not signed in: failed to refresh the expired oxide-code login: \
                     another process held {} for over 20 s. \
                     Try again in a moment. \
                     The next prompt tries the stored login again.",
                    store.join("credentials.lock").display(),
                ),
            );
            assert!(signed_out.retry);
        }

        #[tokio::test]
        async fn malformed_custom_headers_fail_before_credentials_resolve() {
            // No credential exists for the gateway, so reaching auth would fail
            // with a different error.
            let dir = tempfile::tempdir().unwrap();
            let vars = env_vars(vec![
                xdg(&dir),
                ("ANTHROPIC_API_KEY", None),
                env("ANTHROPIC_BASE_URL", "https://gateway.example.com"),
                env("ANTHROPIC_CUSTOM_HEADERS", "missing colon"),
            ]);
            let err = temp_env::async_with_vars(vars, Config::load(dir.path(), false))
                .await
                .expect_err("malformed header must fail startup");
            assert_eq!(
                format!("{err:#}"),
                "invalid ANTHROPIC_CUSTOM_HEADERS: line 1: expected `Name: value`",
            );
        }

        #[tokio::test]
        async fn custom_header_carrying_the_active_credential_fails() {
            let dir = tempfile::tempdir().unwrap();
            let err = load_with(
                &dir,
                &[
                    ("ANTHROPIC_CUSTOM_HEADERS", "Authorization: Bearer other"),
                    ("ANTHROPIC_AUTH_TOKEN", "gateway-token"),
                ],
            )
            .await
            .expect_err("custom header must not replace the credential");
            assert_eq!(
                format!("{err:#}"),
                "invalid ANTHROPIC_CUSTOM_HEADERS: \
                 authorization carries the Auth token (ANTHROPIC_AUTH_TOKEN) credential \
                 and cannot be overridden",
            );
        }

        #[tokio::test]
        async fn gateway_without_credential_names_the_env_vars() {
            let dir = tempfile::tempdir().unwrap();
            let vars = env_vars(vec![
                xdg(&dir),
                ("ANTHROPIC_API_KEY", None),
                env("ANTHROPIC_BASE_URL", "https://gateway.example.com"),
            ]);
            let err = temp_env::async_with_vars(vars, Config::load(dir.path(), false))
                .await
                .expect_err("stored login must not go to a gateway");
            let msg = format!("{err:#}");
            assert!(msg.contains("https://gateway.example.com"), "{msg}");
            assert!(msg.contains("ANTHROPIC_AUTH_TOKEN"), "{msg}");
        }

        #[tokio::test]
        async fn effort_defaults_to_per_model_value() {
            for (model, expected) in [
                ("claude-opus-5-5", Some(Effort::Medium)),
                ("claude-sonnet-5-5", Some(Effort::High)),
                ("custom-model", None),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let config = load_with(&dir, &[("ANTHROPIC_MODEL", model)])
                    .await
                    .unwrap();
                assert_eq!(config.effort, expected, "model={model}");
            }
        }

        #[tokio::test]
        async fn effort_env_overrides_per_model_default() {
            let dir = tempfile::tempdir().unwrap();
            let config = load_with(
                &dir,
                &[("ANTHROPIC_MODEL", "claude-opus-5-5"), ("OX_EFFORT", "low")],
            )
            .await
            .unwrap();
            assert_eq!(config.effort, Some(Effort::Low));
        }

        #[tokio::test]
        async fn effort_pick_is_dropped_on_unknown_model() {
            let dir = tempfile::tempdir().unwrap();
            let config = load_with(
                &dir,
                &[("ANTHROPIC_MODEL", "custom-model"), ("OX_EFFORT", "max")],
            )
            .await
            .unwrap();
            assert_eq!(config.effort, None);
            assert_eq!(config.provenance.effort, Source::Default);
        }

        #[tokio::test]
        async fn effort_file_picks_up_when_env_unset() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    model = "claude-sonnet-5-5"
                    effort = "medium"
                "#},
            );
            let config = load_with(&dir, &[]).await.unwrap();
            assert_eq!(config.effort, Some(Effort::Medium));
        }

        #[tokio::test]
        async fn effort_env_beats_file() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    model = "claude-opus-5-5"
                    effort = "low"
                "#},
            );
            let config = load_with(&dir, &[("OX_EFFORT", "max")]).await.unwrap();
            assert_eq!(config.effort, Some(Effort::Max));
        }

        #[tokio::test]
        async fn effort_invalid_env_lists_the_accepted_tiers() {
            let dir = tempfile::tempdir().unwrap();
            let err = load_with(&dir, &[("OX_EFFORT", "insane")])
                .await
                .expect_err("invalid effort must propagate");
            assert_eq!(
                format!("{err:#}"),
                r#"invalid OX_EFFORT "insane": expected one of low, medium, high, xhigh, max"#,
            );
        }

        #[tokio::test]
        async fn status_line_invalid_segment_surfaces_parse_error() {
            let dir = tempfile::tempdir().unwrap();
            let err = load_with(&dir, &[("OX_STATUS_LINE", "model,nope")])
                .await
                .expect_err("invalid segment must propagate");
            let msg = format!("{err:#}");
            assert!(msg.contains("OX_STATUS_LINE"), "{msg}");
            assert!(msg.contains("nope"), "{msg}");
            assert!(msg.contains("current-dir"), "{msg}");
        }

        #[tokio::test]
        async fn status_line_stray_comma_in_env_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let err = load_with(&dir, &[("OX_STATUS_LINE", "model,,run-state")])
                .await
                .expect_err("stray comma must surface as a typo");
            assert_eq!(
                format!("{err:#}"),
                "invalid OX_STATUS_LINE \"model,,run-state\": empty segment between commas. \
                 Remove the stray comma.",
            );
        }

        #[tokio::test]
        async fn status_line_invalid_segment_in_file_surfaces_parse_error() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [tui]
                    status_line = ["model", "nope"]
                "#},
            );
            let err = load_with(&dir, &[])
                .await
                .expect_err("invalid TOML segment must propagate");
            let msg = format!("{err:#}");
            assert!(msg.contains("status_line"), "{msg}");
            assert!(msg.contains("unknown variant `nope`"), "{msg}");
            assert!(msg.contains("current-dir"), "{msg}");
            assert!(msg.contains("run-state"), "{msg}");
        }

        #[tokio::test]
        async fn status_line_empty_env_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let err = load_with(&dir, &[("OX_STATUS_LINE", ",")])
                .await
                .expect_err("empty status line must not launch without run-state");
            let msg = format!("{err:#}");
            assert!(
                msg.starts_with(r#"invalid OX_STATUS_LINE ",": no segments. List at least one of"#),
                "{msg}"
            );
            assert!(msg.contains("run-state"), "{msg}");
            assert!(msg.ends_with("or remove it to use the default."), "{msg}");
        }

        #[tokio::test]
        async fn status_line_empty_file_roster_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {"
                    [tui]
                    status_line = []
                "},
            );
            let err = load_with(&dir, &[])
                .await
                .expect_err("empty status line must not launch without run-state");
            let msg = format!("{err:#}");
            assert!(
                msg.starts_with("invalid tui.status_line: no segments. List at least one of"),
                "{msg}"
            );
            assert!(msg.contains("run-state"), "{msg}");
        }

        #[tokio::test]
        async fn prompt_cache_ttl_env_overrides_default() {
            let dir = tempfile::tempdir().unwrap();
            let config = load_with(&dir, &[("OX_PROMPT_CACHE_TTL", "5m")])
                .await
                .unwrap();
            assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::FiveMin);
        }

        #[tokio::test]
        async fn prompt_cache_ttl_file_picks_up_when_env_unset() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    prompt_cache_ttl = "5m"
                "#},
            );
            let config = load_with(&dir, &[]).await.unwrap();
            assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::FiveMin);
        }

        #[tokio::test]
        async fn prompt_cache_ttl_env_beats_file() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    prompt_cache_ttl = "5m"
                "#},
            );
            let config = load_with(&dir, &[("OX_PROMPT_CACHE_TTL", "1h")])
                .await
                .unwrap();
            assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::OneHour);
        }

        #[tokio::test]
        async fn prompt_cache_ttl_invalid_env_surfaces_parse_error() {
            let dir = tempfile::tempdir().unwrap();
            let err = load_with(&dir, &[("OX_PROMPT_CACHE_TTL", "forever")])
                .await
                .expect_err("invalid ttl must propagate");
            assert_eq!(
                format!("{err:#}"),
                r#"invalid OX_PROMPT_CACHE_TTL "forever": expected one of 5m, 1h"#,
            );
        }

        #[tokio::test]
        async fn compaction_file_can_disable_default_on_auto_behavior() {
            let dir = tempfile::tempdir().unwrap();
            let user = write_user_config(
                dir.path(),
                indoc! {"
                    [client.compaction]
                    auto_enabled = false
                "},
            );
            write_drop_in(
                dir.path(),
                "10-threshold.toml",
                "client.compaction.auto_threshold_percent = 40",
            );
            let config = load_with(&dir, &[]).await.unwrap();
            assert!(!config.compaction.auto.enabled);
            assert_eq!(
                config.provenance.compaction,
                Source::File(user),
                "a disabled trigger ignores the threshold"
            );
        }

        #[tokio::test]
        async fn compaction_auto_env_beats_file() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {"
                    [client.compaction]
                    auto_enabled = false
                "},
            );
            let config = load_with(&dir, &[("OX_COMPACTION_AUTO_ENABLED", "TRUE")])
                .await
                .unwrap();
            assert!(config.compaction.auto.enabled);
        }

        #[tokio::test]
        async fn compaction_auto_threshold_tokens_sets_absolute_trigger() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {"
                    [client.compaction]
                    auto_threshold_tokens = 400000
                "},
            );
            let config = load_with(&dir, &[]).await.unwrap();
            assert_eq!(config.compaction.auto.threshold_tokens, Some(400_000));
        }

        #[tokio::test]
        async fn compaction_rejects_ambiguous_auto_thresholds() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {"
                    [client.compaction]
                    auto_threshold_tokens = 400000
                    auto_threshold_percent = 40
                "},
            );
            let home = [(HOME_VAR, Some(dir.path()))];
            let err = Box::pin(temp_env::async_with_vars(home, load_with(&dir, &[])))
                .await
                .expect_err("ambiguous file thresholds must fail config load");
            assert_eq!(
                format!("{err:#}"),
                "~/ox/config.toml sets both auto_threshold_tokens and auto_threshold_percent. \
                 Set only one compaction threshold.",
            );

            let err = load_with(
                &dir,
                &[
                    ("OX_COMPACTION_AUTO_THRESHOLD_TOKENS", "400000"),
                    ("OX_COMPACTION_AUTO_THRESHOLD_PERCENT", "40"),
                ],
            )
            .await
            .expect_err("ambiguous env thresholds must fail config load");
            assert_eq!(
                format!("{err:#}"),
                "both OX_COMPACTION_AUTO_THRESHOLD_TOKENS \
                 and OX_COMPACTION_AUTO_THRESHOLD_PERCENT are set. \
                 Set only one compaction threshold.",
            );
        }

        #[tokio::test]
        async fn permission_resolves_each_tier_and_the_defaults_across_layers() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [permission.deny]
                    exec = ["git push *"]

                    [permission.ask]
                    exec = ["cargo publish *"]

                    [permission.allow]
                    exec = ["git *", "cargo *", "rm *"]

                    [permission.defaults]
                    git = false
                    rm = false
                "#},
            );
            write_drop_in(dir.path(), "10-team.toml", "permission.defaults.git = true");

            let config = load_with(&dir, &[]).await.unwrap();

            let decide =
                |class, target: Access| config.permission.decide(class, &target.as_target());
            let command = |command: &str| Access::for_command(command, &config.workspace);
            assert_eq!(
                decide(PermissionClass::Exec, command("git push origin")),
                Decision::Deny
            );
            assert_eq!(
                decide(PermissionClass::Exec, command("cargo publish")),
                Decision::Ask
            );
            assert_eq!(
                decide(PermissionClass::Exec, command("cargo build")),
                Decision::Allow
            );
            assert_eq!(
                decide(PermissionClass::Exec, command("rm -rf target")),
                Decision::Allow,
                "`rm = false` turns the shipped `rm` rule off"
            );
            assert_eq!(
                decide(
                    PermissionClass::Edit,
                    Access::for_path(".git/config", &config.workspace)
                ),
                Decision::Ask,
                "a later layer turns the `git` default back on"
            );
        }

        #[tokio::test]
        async fn permission_denies_ssh_reads_under_a_home_spelled_with_a_trailing_slash_or_dot() {
            let dir = tempfile::tempdir().unwrap();
            let home = dunce::canonicalize(dir.path()).unwrap().join("home");
            std::fs::create_dir(&home).unwrap();
            let key = home.join(".ssh").join("id_ed25519");

            for spelling in [
                format!("{}/", home.display()),
                format!("{}/.", home.display()),
            ] {
                let config = Box::pin(temp_env::async_with_vars(
                    [(HOME_VAR, Some(spelling.as_str()))],
                    load_with(&dir, &[]),
                ))
                .await
                .unwrap();

                let target = Access::for_path(&key.to_string_lossy(), &config.workspace);
                assert_eq!(
                    config
                        .permission
                        .decide(PermissionClass::Read, &target.as_target()),
                    Decision::Deny,
                    "{spelling}"
                );
            }
        }

        #[tokio::test]
        async fn permission_invalid_built_in_pattern_errors() {
            let dir = tempfile::tempdir().unwrap();

            let err = Box::pin(temp_env::async_with_vars(
                [(HOME_VAR, Some("!home"))],
                load_with(&dir, &[]),
            ))
            .await
            .expect_err("a built-in pattern that fails to parse must fail the load");

            assert_eq!(
                format!("{err:#}"),
                "invalid built-in `ssh` pattern \"!home/.ssh\": \
                 a pattern cannot start with `!`, since patterns have no negation. \
                 Write the paths to match instead, or turn off the built-in rule that covers them."
            );
        }

        #[tokio::test]
        async fn permission_mode_env_beats_file() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [permission]
                    mode = "plan"
                "#},
            );
            let config = load_with(&dir, &[("OX_PERMISSION_MODE", "yolo")])
                .await
                .unwrap();
            assert_eq!(config.permission.mode(), Mode::Yolo);
        }

        #[tokio::test]
        async fn permission_invalid_mode_env_errors() {
            let dir = tempfile::tempdir().unwrap();
            let err = load_with(&dir, &[("OX_PERMISSION_MODE", "bypass")])
                .await
                .expect_err("invalid mode must propagate");
            assert_eq!(
                format!("{err:#}"),
                r#"invalid OX_PERMISSION_MODE "bypass": expected one of manual, plan, yolo"#,
            );
        }

        #[tokio::test]
        async fn permission_extra_roots_join_the_cwd_across_layers() {
            let dir = tempfile::tempdir().unwrap();
            let (shared, docs) = (dir.path().join("shared"), dir.path().join("docs"));
            std::fs::create_dir(&shared).unwrap();
            std::fs::create_dir(&docs).unwrap();
            let roots = |paths: &[&Path]| {
                let quoted: Vec<String> = paths
                    .iter()
                    .map(|path| toml::Value::from(path.to_str().unwrap()).to_string())
                    .collect();
                format!("permission.extra_roots = [{}]", quoted.join(", "))
            };
            write_user_config(dir.path(), &roots(&[&shared]));
            write_drop_in(dir.path(), "10-docs.toml", &roots(&[&docs, &shared]));

            let config = load_with(&dir, &[]).await.unwrap();

            let canonical = |path: &Path| dunce::canonicalize(path).unwrap();
            assert_eq!(config.workspace.cwd(), dir.path());
            assert_eq!(
                config.workspace.roots(),
                [canonical(dir.path()), canonical(&shared), canonical(&docs)],
            );
        }

        #[tokio::test]
        async fn permission_invalid_extra_root_names_its_file() {
            let dir = tempfile::tempdir().unwrap();
            let drop_in = write_drop_in(
                dir.path(),
                "10-roots.toml",
                r#"permission.extra_roots = ["shared"]"#,
            );

            let err = load_with(&dir, &[])
                .await
                .expect_err("a relative extra root must propagate");

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "invalid config at {}: invalid extra root \"shared\": \
                     expected an absolute or `~/` path",
                    drop_in.path().display()
                ),
            );
        }

        #[tokio::test]
        async fn permission_invalid_rule_in_file_errors() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [permission.allow]
                    edit = ["src/**/["]
                "#},
            );
            let err = load_with(&dir, &[])
                .await
                .expect_err("malformed rule must propagate");
            assert!(
                format!("{err:#}")
                    .contains(r#"invalid permission.allow.edit pattern "src/**/[": "#),
                "{err:#}"
            );
        }

        #[tokio::test]
        async fn records_the_layer_behind_each_setting() {
            let dir = tempfile::tempdir().unwrap();
            let user = write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    model = "claude-haiku-5-5"
                    effort = "high"
                    max_tool_rounds = 50

                    [client.compaction]
                    auto_threshold_tokens = 400000
                "#},
            );
            let drop_in = write_drop_in(
                dir.path(),
                "10-team.toml",
                indoc! {r#"
                    [client]
                    effort = "low"
                    prompt_cache_ttl = "5m"

                    [permission]
                    mode = "plan"
                "#},
            );
            let config = load_with(
                &dir,
                &[
                    ("ANTHROPIC_MODEL", "claude-sonnet-5-5"),
                    ("ANTHROPIC_CUSTOM_HEADERS", "x-team: platform"),
                    ("CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS", "1"),
                    ("OX_SHOW_THINKING", "1"),
                ],
            )
            .await
            .unwrap();

            assert_eq!(config.effort, Some(Effort::Low));
            assert_eq!(
                config.provenance,
                Provenance {
                    layers: vec![user.clone(), drop_in.clone()],
                    model: Source::Env("ANTHROPIC_MODEL"),
                    effort: Source::File(drop_in.clone()),
                    auth: Source::Env("ANTHROPIC_API_KEY"),
                    custom_headers: Source::Env("ANTHROPIC_CUSTOM_HEADERS"),
                    structured_outputs: Source::Env("CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS"),
                    max_tool_rounds: Source::File(user.clone()),
                    prompt_cache_ttl: Source::File(drop_in.clone()),
                    compaction: Source::File(user),
                    permission_mode: Source::File(drop_in),
                    show_thinking: Source::Env("OX_SHOW_THINKING"),
                    ..Provenance::default()
                },
            );
        }

        #[tokio::test]
        async fn env_sources_the_settings_it_overrides() {
            let dir = tempfile::tempdir().unwrap();
            let config = load_with(
                &dir,
                &[
                    ("ANTHROPIC_BASE_URL", "https://gateway.example.com"),
                    ("ANTHROPIC_AUTH_TOKEN", "gateway-token"),
                    ("OX_MAX_TOKENS", "64"),
                    ("CLAUDE_CODE_ATTRIBUTION_HEADER", "0"),
                    ("OX_COMPACTION_AUTO_THRESHOLD_PERCENT", "40"),
                    ("OX_EXTRA_CA_CERTS", "/etc/ssl/ca.pem"),
                    ("OX_PERMISSION_MODE", "yolo"),
                    ("OX_PROMPT_CACHE_TTL", "5m"),
                ],
            )
            .await
            .unwrap();

            assert_eq!(
                config.provenance,
                Provenance {
                    auth: Source::Env("ANTHROPIC_AUTH_TOKEN"),
                    base_url: Source::Env("ANTHROPIC_BASE_URL"),
                    extra_ca_certs: Source::Env("OX_EXTRA_CA_CERTS"),
                    attribution: Source::Env("CLAUDE_CODE_ATTRIBUTION_HEADER"),
                    max_tokens: Source::Env("OX_MAX_TOKENS"),
                    prompt_cache_ttl: Source::Env("OX_PROMPT_CACHE_TTL"),
                    compaction: Source::Env("OX_COMPACTION_AUTO_THRESHOLD_PERCENT"),
                    permission_mode: Source::Env("OX_PERMISSION_MODE"),
                    ..Provenance::default()
                },
            );
        }
    }

    mod notices {
        use super::*;

        #[test]
        fn list_an_unsupported_model_then_why_the_session_is_signed_out() {
            let mut cfg = sample_config();
            cfg.auth = None;
            cfg.signed_out = Some(LoginUnavailable::new(&LoginError::Missing));

            let notices = cfg.notices();

            assert_eq!(notices.len(), 2, "{notices:?}");
            assert!(
                notices[0].starts_with("`claude-test-1-0` is not a supported model"),
                "{notices:?}"
            );
            assert_eq!(notices[1], "Not signed in. Run `/login` to sign in.");
        }

        #[test]
        fn are_empty_for_a_current_setup() {
            let mut cfg = sample_config();
            cfg.model = ModelSpec::resolve("claude-opus-5-5".to_owned());
            assert_eq!(cfg.notices(), [] as [String; 0]);
        }
    }

    mod apply_mode_flag {
        use super::*;

        #[test]
        fn sets_the_gate_mode_and_credits_the_flag() {
            let mut config = sample_config();

            config.apply_mode_flag(Mode::Yolo);

            assert_eq!(config.permission.mode(), Mode::Yolo);
            let snap = config.snapshot();
            assert_eq!(snap.permission_mode, Mode::Yolo);
            assert_eq!(snap.provenance.permission_mode, Source::Flag("--mode"));
        }
    }

    mod snapshot {
        use super::*;

        #[test]
        fn copies_every_user_facing_field_and_drops_secret() {
            let cfg = sample_config();
            let snap = cfg.snapshot();
            assert_eq!(snap.auth, Some(AuthSource::ClaudeCodeLogin));
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
            assert_eq!(snap.max_tokens, None);
            assert_eq!(snap.max_tool_rounds, Some(100));
            assert_eq!(snap.prompt_cache_ttl, PromptCacheTtl::FiveMin);
            assert_eq!(snap.compaction.auto.threshold_tokens, Some(42));
            assert_eq!(snap.permission_mode, Mode::Plan);
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
            assert_eq!(snap.provenance, cfg.provenance);
        }
    }

    mod is_first_party_base_url {
        use super::*;

        #[test]
        fn accepts_official_hosts() {
            assert!(is_first_party_base_url("https://api.anthropic.com"));
            assert!(is_first_party_base_url("https://api.anthropic.com/"));
            assert!(is_first_party_base_url("https://api-staging.anthropic.com"));
            assert!(is_first_party_base_url("https://API.ANTHROPIC.COM"));
        }

        #[test]
        fn rejects_proxies_and_malformed_urls() {
            assert!(!is_first_party_base_url("https://api.openai.com"));
            assert!(!is_first_party_base_url("https://proxy.example.com"));
            assert!(!is_first_party_base_url("https://anthropic.com.evil.io"));
            assert!(!is_first_party_base_url("http://127.0.0.1:8080"));
            assert!(!is_first_party_base_url(""));
            assert!(!is_first_party_base_url("not-a-url"));
        }
    }

    mod display_auth {
        use super::*;

        #[test]
        fn names_the_source_or_the_signed_out_state() {
            assert_eq!(
                display_auth(Some(AuthSource::OxLogin)),
                "OAuth (oxide-code login)"
            );
            assert_eq!(display_auth(None), "Not signed in");
        }
    }

    mod display_effort {
        use super::*;

        #[test]
        fn names_effective_tier_or_no_tier() {
            assert_eq!(display_effort(Some(Effort::High)), "high");
            assert_eq!(display_effort(None), "(no effort tier)");
        }
    }

    mod display_bool {
        use super::*;

        #[test]
        fn names_the_two_flag_states() {
            assert_eq!(display_bool(true), "on");
            assert_eq!(display_bool(false), "off");
        }
    }

    mod display_auto_compaction {
        use super::*;

        #[test]
        fn names_enabled_threshold_or_off() {
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
    }
}
