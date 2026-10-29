//! Configuration loading.
//!
//! Precedence (highest wins): env > project `.ox/config.toml` > user `config.d/*.toml` drop-ins >
//! user `~/.config/ox/config.toml` > defaults. Auth stops at the first source that resolves, in
//! [`AuthSource`] order.

mod base_url;
pub mod compaction;
pub(crate) mod credentials;
pub mod gateway;
pub mod load;
mod merge;
pub mod provenance;
mod schema;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::fmt;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};
use tracing::warn;

pub(crate) use self::base_url::BaseUrl;
use self::compaction::{AutoCompactionConfig, CompactionConfig};
use self::credentials::{AuthState, CredentialError};
use self::gateway::{GatewayCompat, GatewaySnapshot, ResolvedGateway};
use self::load::Loaded;
use self::merge::Origins;
use self::provenance::{Provenance, Source, Sourced, TierPattern};
use self::schema::{ClientAuth, FileConfig, PermissionTable, ThemeTable, TierTable, TuiTable};
use crate::auth::oauth::ClaudeCodeKeychain;
use crate::auth::{self, AuthSource, LoginUnavailable};
use crate::model::{Effort, ModelSettings, ModelSpec, PromptCacheTtl};
use crate::permission::defaults::Shipped;
use crate::permission::{Mode, Policy, Tier, Workspace};
use crate::util::env;
use crate::util::path::{expand_absolute, expand_user, project_root, tildify};
use crate::util::tls::{PlatformTrust, load_extra_ca_certs};
use crate::util::variant;

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub(crate) const DEFAULT_THEME: &str = "mocha";

// ── ConfigSnapshot ──

/// Resolved-config view without the auth secret.
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

    /// Applies a `/theme` switch to the built-in `name`, which `/config` credits to the session.
    pub fn apply_theme(&mut self, name: &str) {
        name.clone_into(&mut self.theme_name);
        self.provenance.theme = Source::Session;
    }
}

// ── Status Line ──

/// User-configurable status-line segment identifier.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "kebab-case")]
#[strum(
    serialize_all = "kebab-case",
    parse_err_ty = anyhow::Error,
    parse_err_fn = unknown_segment
)]
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
    /// Order rationale documented in `docs/design/status-line.md`.
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
}

fn unknown_segment(s: &str) -> anyhow::Error {
    anyhow!(
        "unknown segment {s:?}: expected one of {}",
        variant::names::<StatusLineSegment>()
    )
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

// ── ConfigCheck ──

/// What `ox config check` reports about a configuration that loads.
#[derive(Debug, Clone)]
pub struct ConfigCheck {
    /// The config files read, in load order.
    pub files: Vec<PathBuf>,
    /// Startup notices, such as an unsupported model.
    pub notices: Vec<String>,
    pub theme: ThemeConfig,
}

// ── Config ──

/// Resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub model: ModelSpec,
    /// `None` for models outside the catalog.
    pub(crate) effort: Option<Effort>,
    pub(crate) auth: AuthState,
    pub(crate) base_url: BaseUrl,
    /// Resolved path to a PEM bundle appended to reqwest's trust anchors. `None` means the client
    /// trusts only the platform roots.
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
    /// Whether adaptive thinking opts into summarized text. The request's `thinking` field derives
    /// from this plus the model.
    pub show_thinking: bool,
    pub(crate) show_welcome: bool,
    pub(crate) status_line: Vec<StatusLineSegment>,
    pub theme: ThemeConfig,
    pub(crate) provenance: Provenance,
}

impl Config {
    /// Resolves defaults, user and project config files, and environment values. Project discovery
    /// stays within `cwd`'s [`project_root`]. Invalid TOML or environment values fail the load.
    /// Resolves inline explicit credentials, leaving secret files and stored logins to
    /// [`Config::resolve_auth`].
    pub fn load(cwd: &Path) -> Result<Self> {
        Self::resolve(cwd, |path| load_extra_ca_certs(path).map(drop))
    }

    /// Checks declarations without reading a credential file, CA bundle, or stored login.
    pub fn check(cwd: &Path) -> Result<ConfigCheck> {
        let config = Self::resolve(cwd, |_| Ok(()))?;
        Ok(ConfigCheck {
            files: config
                .provenance
                .layers
                .iter()
                .map(|layer| layer.path().to_owned())
                .collect(),
            notices: config.notices(),
            theme: config.theme,
        })
    }

    /// Resolves the config, checking the CA bundle it picks with `check_ca_bundle`.
    fn resolve(cwd: &Path, check_ca_bundle: fn(&Path) -> Result<()>) -> Result<Self> {
        let Loaded { config, origins } = load::load(cwd, &project_root(cwd))?;
        let FileConfig {
            client,
            permission,
            tui,
        } = config;

        let gateway = GatewayCompat::resolve(&client, &origins)?;

        let base_url = resolve_base_url(
            env_string("ANTHROPIC_BASE_URL")
                .or_else(|| origins.sourced("client.base_url", client.base_url))
                .unwrap_or_else(|| Sourced::fallback(DEFAULT_BASE_URL.to_owned())),
        )?;

        let extra_ca_certs = resolve_extra_ca_certs(
            origins.sourced("client.extra_ca_certs", client.extra_ca_certs),
            check_ca_bundle,
        )?;

        let auth = resolve_explicit_auth(client.auth, &origins, &base_url.value, &gateway)?;

        let model_id = env_string("OX_MODEL")
            .or_else(|| origins.sourced("client.model", client.model))
            .unwrap_or_else(|| Sourced::fallback(DEFAULT_MODEL.to_owned()));
        let model = ModelSpec::resolve(model_id.value);

        let effort = resolve_effort(&model, client.effort, &origins)?;

        let max_tokens = resolve_max(
            "OX_MAX_TOKENS",
            origins.sourced("client.max_tokens", client.max_tokens),
        )?;
        let max_tool_rounds = resolve_max(
            "OX_MAX_TOOL_ROUNDS",
            origins.sourced("client.max_tool_rounds", client.max_tool_rounds),
        )?;

        let prompt_cache_ttl = env_parse::<PromptCacheTtl>("OX_PROMPT_CACHE_TTL")?
            .or_else(|| origins.sourced("client.prompt_cache_ttl", client.prompt_cache_ttl))
            .unwrap_or_else(|| Sourced::fallback(PromptCacheTtl::OneHour));

        let compaction = compaction::resolve(
            &client.compaction,
            &origins,
            &model,
            model.resolve_max_tokens(max_tokens.value),
        )?;

        let permission = resolve_permission(permission, &origins, cwd)?;

        let tui = resolve_tui(tui, &origins)?;

        let provenance = Provenance {
            layers: origins.into_layers(),
            model: model_id.source,
            effort: effort.source,
            auth: auth.source,
            base_url: base_url.source,
            extra_ca_certs: extra_ca_certs.source,
            custom_headers: gateway.custom_headers.source,
            attribution: gateway.attribution.source,
            structured_outputs: gateway.structured_outputs.source,
            max_tokens: max_tokens.source,
            max_tool_rounds: max_tool_rounds.source,
            prompt_cache_ttl: prompt_cache_ttl.source,
            compaction: compaction.source,
            permission_mode: permission.mode_source,
            show_thinking: tui.show_thinking.source,
            show_welcome: tui.show_welcome.source,
            status_line: tui.status_line.source,
            theme: tui.theme.base_source.clone(),
            theme_overrides: tui.theme_overrides,
            extra_roots: permission.extra_roots,
            rules: permission.rules,
            disabled_defaults: permission.disabled_defaults,
        };

        Ok(Self {
            model,
            effort: effort.value,
            auth: auth.value,
            base_url: base_url.value,
            extra_ca_certs: extra_ca_certs.value,
            gateway: GatewayCompat {
                custom_headers: gateway.custom_headers.value,
                attribution: gateway.attribution.value,
                structured_outputs: gateway.structured_outputs.value,
            },
            max_tokens: max_tokens.value,
            max_tool_rounds: max_tool_rounds.value,
            prompt_cache_ttl: prompt_cache_ttl.value,
            compaction: compaction.value,
            permission: permission.policy,
            workspace: permission.workspace,
            show_thinking: tui.show_thinking.value,
            show_welcome: tui.show_welcome.value,
            status_line: tui.status_line.value,
            theme: tui.theme,
            provenance,
        })
    }

    /// Loads a selected credential file or falls back to a stored login. Run after other startup
    /// checks so invalid settings fail before a Keychain prompt or token refresh. A TUI that
    /// `can_sign_in` may start signed out against the first-party API. Other sessions and gateways
    /// require credentials.
    pub async fn resolve_auth(&mut self, can_sign_in: bool) -> Result<()> {
        let AuthState::Unresolved(pending) = &self.auth else {
            return Ok(());
        };
        if let Some(pending) = pending {
            self.auth = AuthState::Ready(pending.load()?);
            return Ok(());
        }
        let trust = PlatformTrust(self.extra_ca_certs.clone());
        let stored = auth::stored_login(ClaudeCodeKeychain::SYSTEM, &trust);
        self.auth = match credentials::fallback(&self.base_url, stored).await {
            Ok(auth) => AuthState::Ready(auth),
            Err(CredentialError::SignedOut { cause, .. }) if can_sign_in => {
                AuthState::SignedOut(LoginUnavailable::new(&cause))
            }
            Err(e) => return Err(e.into()),
        };
        Ok(())
    }

    /// The model settings a session built from this config starts with.
    pub(crate) fn model_settings(&self) -> ModelSettings {
        ModelSettings::new(
            self.model.clone(),
            self.effort,
            self.max_tokens,
            self.prompt_cache_ttl,
            self.show_thinking,
        )
    }

    /// Startup notices: an unsupported model, then why the session is signed out.
    pub(crate) fn notices(&self) -> Vec<String> {
        self.model
            .unsupported_notice()
            .into_iter()
            .chain(match &self.auth {
                AuthState::SignedOut(unavailable) => Some(unavailable.notice.clone()),
                AuthState::Unresolved(_) | AuthState::Ready(_) => None,
            })
            .collect()
    }

    /// Applies `--mode`, which overrides the config files and `OX_PERMISSION_MODE`.
    pub fn apply_mode_flag(&mut self, mode: Mode) {
        self.permission.set_mode(mode);
        self.provenance.permission_mode = Source::Flag("--mode");
    }

    /// Marks the run as one nobody can answer an approval request in, so the engine denies each
    /// call that would ask without requesting approval, and words the denial for the model.
    pub fn set_unattended(&mut self) {
        self.permission.set_unattended();
    }

    /// Descriptors for `/config` and `/status`, minus the auth secret.
    pub fn snapshot(&self) -> ConfigSnapshot {
        ConfigSnapshot {
            model: self.model.clone(),
            effort: self.effort,
            auth: self.auth.source(),
            base_url: self.base_url.to_string(),
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
fn resolve_extra_ca_certs(
    file: Option<Sourced<String>>,
    check_ca_bundle: fn(&Path) -> Result<()>,
) -> Result<Sourced<Option<PathBuf>>> {
    let explicit = env_string("OX_EXTRA_CA_CERTS")
        .map(|raw| ("OX_EXTRA_CA_CERTS", raw))
        .or(file.map(|raw| ("client.extra_ca_certs", raw)));
    if let Some((key, raw)) = explicit {
        let path = if matches!(raw.source, Source::File(_)) {
            expand_absolute(&raw.value)
        } else {
            expand_user(&raw.value)
        }
        .and_then(|path| check_ca_bundle(&path).map(|()| path))
        .with_context(|| format!("invalid {key}"))?;
        return Ok(Sourced::new(Some(path), raw.source));
    }

    let Some(raw) = env_string("NODE_EXTRA_CA_CERTS") else {
        return Ok(Sourced::fallback(None));
    };
    match expand_user(&raw.value).and_then(|path| check_ca_bundle(&path).map(|()| path)) {
        Ok(path) => Ok(Sourced::new(Some(path), raw.source)),
        Err(e) => {
            warn!(error = %format_args!("{e:#}"), "ignoring NODE_EXTRA_CA_CERTS");
            Ok(Sourced::fallback(None))
        }
    }
}

/// Validates `base_url`, naming the setting that supplied it in an error.
fn resolve_base_url(base_url: Sourced<String>) -> Result<Sourced<BaseUrl>> {
    let value = BaseUrl::parse(&base_url.value).with_context(|| {
        let raw = base_url::redacted(&base_url.value);
        let setting = match &base_url.source {
            Source::Env(var) => format!("{var} {raw:?}"),
            Source::File(layer) => format!("client.base_url {raw:?} in {}", tildify(layer.path())),
            _ => format!("base URL {raw:?}"),
        };
        format!("invalid {setting}")
    })?;
    Ok(Sourced::new(value, base_url.source))
}

/// The explicit credential and its source, rejecting a custom header that would replace it. Without
/// one, the source is the default, since [`Config::resolve_auth`] falls back to a stored login, and
/// on a first-party URL the header check covers the OAuth header that a stored login or `/login`
/// sends. A gateway never receives a stored login, so there the missing credential reports itself.
fn resolve_explicit_auth(
    file_auth: Option<ClientAuth>,
    origins: &Origins,
    base_url: &BaseUrl,
    gateway: &ResolvedGateway,
) -> Result<Sourced<AuthState>> {
    let config = file_auth
        .map(credentials::configured_auth)
        .transpose()?
        .flatten();
    let explicit = credentials::first_explicit(config)?;
    let explicit_source = explicit.as_ref().map(credentials::ExplicitAuth::source);
    if explicit_source.is_some() || base_url.is_first_party() {
        gateway.check_credential_header(explicit_source)?;
    }

    let source = match explicit_source.unwrap_or(AuthSource::OxLogin) {
        AuthSource::ClientBearer | AuthSource::ClientApiKey | AuthSource::ClientOAuth => {
            origins.source("client.auth")
        }
        source => source.env_var().map_or(Source::Default, Source::Env),
    };
    let state = match explicit {
        Some(credentials::ExplicitAuth::Inline(auth)) => AuthState::Ready(auth),
        Some(credentials::ExplicitAuth::File(pending)) => AuthState::Unresolved(Some(pending)),
        None => AuthState::Unresolved(None),
    };
    Ok(Sourced::new(state, source))
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

/// The TUI settings with the source of each.
struct ResolvedTui {
    show_thinking: Sourced<bool>,
    show_welcome: Sourced<bool>,
    status_line: Sourced<Vec<StatusLineSegment>>,
    theme: ThemeConfig,
    theme_overrides: Vec<Source>,
}

/// Reads `tui.*`, letting each environment variable replace the file value.
fn resolve_tui(tui: TuiTable, origins: &Origins) -> Result<ResolvedTui> {
    let (theme, theme_overrides) = resolve_theme(tui.theme, origins);
    Ok(ResolvedTui {
        show_thinking: resolve_flag(
            "OX_SHOW_THINKING",
            origins.sourced("tui.show_thinking", tui.show_thinking),
            false,
        )?,
        show_welcome: resolve_flag(
            "OX_SHOW_WELCOME",
            origins.sourced("tui.show_welcome", tui.show_welcome),
            true,
        )?,
        status_line: resolve_status_line(origins.sourced("tui.status_line", tui.status_line))?,
        theme,
        theme_overrides,
    })
}

/// The resolved theme and the layers that set any override slot.
fn resolve_theme(theme: ThemeTable, origins: &Origins) -> (ThemeConfig, Vec<Source>) {
    let base = ThemeConfig {
        base: theme.base.unwrap_or_else(|| DEFAULT_THEME.to_owned()),
        base_source: origins.source("tui.theme.base"),
        overrides: theme.overrides,
    };
    (base, origins.sources_under("tui.theme.overrides"))
}

/// `var` > `file` > `None`, a whole-number cap.
fn resolve_max(var: &'static str, file: Option<Sourced<u32>>) -> Result<Sourced<Option<u32>>> {
    Ok(Sourced::from_option(
        env_number(var, 1..=u32::MAX)?.or(file),
    ))
}

/// `var` > `file` > `default`, a boolean setting.
fn resolve_flag(
    var: &'static str,
    file: Option<Sourced<bool>>,
    default: bool,
) -> Result<Sourced<bool>> {
    Ok(env_bool(var)?
        .or(file)
        .unwrap_or_else(|| Sourced::fallback(default)))
}

/// `OX_STATUS_LINE` > `tui.status_line` > the default roster.
fn resolve_status_line(
    file: Option<Sourced<Vec<StatusLineSegment>>>,
) -> Result<Sourced<Vec<StatusLineSegment>>> {
    if let Some(raw) = env_string("OX_STATUS_LINE") {
        let segments = parse_status_line_segments(&raw.value)
            .with_context(|| format!("invalid OX_STATUS_LINE {:?}", raw.value))?;
        return Ok(Sourced::new(segments, raw.source));
    }
    let segments = file.unwrap_or_else(|| Sourced::fallback(StatusLineSegment::DEFAULT.to_vec()));
    let source = segments.source;
    let segments = validate_status_line(segments.value).context("invalid tui.status_line")?;
    Ok(Sourced::new(segments, source))
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
            variant::names::<StatusLineSegment>()
        );
    }
    Ok(segments)
}

/// The resolved permission settings and where each came from.
struct ResolvedPermission {
    policy: Policy,
    workspace: Workspace,
    mode_source: Source,
    extra_roots: Vec<Sourced<String>>,
    rules: Vec<Sourced<TierPattern>>,
    disabled_defaults: Vec<Sourced<Shipped>>,
}

/// Resolves the permission policy and the workspace launched in `cwd` from the file config and
/// `OX_PERMISSION_MODE`, which wins over the file mode.
fn resolve_permission(
    file: PermissionTable,
    origins: &Origins,
    cwd: &Path,
) -> Result<ResolvedPermission> {
    let roots = file.extra_roots.unwrap_or_default();
    let workspace = Workspace::resolve(cwd, &roots)?;
    let extra_roots = origins.sourced_elements("permission.extra_roots", roots);
    let mode = env_parse::<Mode>("OX_PERMISSION_MODE")?
        .or_else(|| origins.sourced("permission.mode", file.mode))
        .unwrap_or_else(|| Sourced::fallback(Mode::default()));

    let tiers = [
        (Tier::Deny, file.deny.unwrap_or_default()),
        (Tier::Ask, file.ask.unwrap_or_default()),
        (Tier::Allow, file.allow.unwrap_or_default()),
    ];
    let rules = tiers
        .iter()
        .flat_map(|(tier, table)| tier_patterns(*tier, table, origins))
        .collect();
    let disabled_defaults: Vec<_> = file
        .defaults
        .unwrap_or_default()
        .into_iter()
        .filter(|&(_, on)| !on)
        .map(|(group, _)| {
            Sourced::new(
                group,
                origins.source(&format!("permission.defaults.{group}")),
            )
        })
        .collect();
    let [deny, ask, allow] = tiers.map(|(tier, table)| table.rules(tier));
    let disabled: Vec<_> = disabled_defaults.iter().map(|group| group.value).collect();
    let policy = Policy::resolve(mode.value, deny?, ask?, allow?, &disabled, &workspace)?;
    Ok(ResolvedPermission {
        policy,
        workspace,
        mode_source: mode.source,
        extra_roots,
        rules,
        disabled_defaults,
    })
}

/// The patterns of the `tier` table, each with the layer that added it.
fn tier_patterns(tier: Tier, table: &TierTable, origins: &Origins) -> Vec<Sourced<TierPattern>> {
    table
        .classes()
        .into_iter()
        .flat_map(|(class, patterns)| {
            let patterns = patterns.iter().flatten().map(|pattern| TierPattern {
                tier,
                class,
                pattern: pattern.clone(),
            });
            origins.sourced_elements(&format!("permission.{tier}.{class}"), patterns)
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::{BTreeSet, HashSet};

    use indoc::{formatdoc, indoc};
    use regex::Regex;
    use serde::de::{self, value::StrDeserializer};
    use tempfile::TempDir;

    use super::compaction::testing::WINDOW_1M_THRESHOLD;
    use super::provenance::Layer;
    use super::testing::test_config;
    use super::*;
    use crate::auth::Auth;
    use crate::auth::oauth::LoginError;
    use crate::auth::oauth::testing::{creds_json, write_creds};
    use crate::permission::{Access, Decision, PermissionClass};
    use crate::util::fs::testing::deny_read_access;
    use crate::util::path::testing::HOME_VAR;
    use crate::util::tls::local_testing::TEST_CA_PEM;
    use crate::util::tls::testing::TEST_CA_PATH;

    fn sample_config() -> Config {
        Config {
            extra_ca_certs: Some(PathBuf::from("/etc/ssl/corp-ca.pem")),
            gateway: GatewayCompat {
                custom_headers: gateway::parse_headers("x-team: secret-team").unwrap(),
                attribution: false,
                structured_outputs: true,
            },
            effort: Some(Effort::Xhigh),
            max_tokens: None,
            max_tool_rounds: Some(100),
            prompt_cache_ttl: PromptCacheTtl::FiveMin,
            compaction: CompactionConfig::with_threshold(42),
            permission: Policy::new(Mode::Plan, Vec::new(), Vec::new(), Vec::new()),
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
                model: Source::Env("OX_MODEL"),
                effort: Source::Env("OX_EFFORT"),
                ..Provenance::default()
            },
            ..test_config(
                "https://api.example.test",
                Auth::test(AuthSource::ClaudeCodeLogin, "token-must-not-leak"),
                "claude-test-1-0",
            )
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
                assert_eq!(snap.provenance.model, Source::Env("OX_MODEL"));
                assert_eq!(snap.provenance.effort, Source::Env("OX_EFFORT"));
                assert_eq!(snap.compaction.auto, auto);

                snap.apply_swap(model, Some(Effort::Low), Some(Effort::Low), auto);
                assert_eq!(snap.provenance.model, Source::Env("OX_MODEL"));
                assert_eq!(snap.provenance.effort, Source::Session);
                assert_eq!(snap.effort, Some(Effort::Low));

                let opus = ModelSpec::resolve("claude-opus-5-5");
                snap.apply_swap(opus, Some(Effort::Low), None, auto);
                assert_eq!(snap.provenance.model, Source::Session);
                assert_eq!(snap.provenance.effort, Source::Session);
                assert_eq!(snap.model.id(), "claude-opus-5-5");
            }

            #[test]
            fn reverts_an_effort_the_model_derived_to_the_default_source() {
                let mut snap = sample_config().snapshot();
                let custom = ModelSpec::resolve("gateway-alias");
                let auto = AutoCompactionConfig::disabled();

                snap.apply_swap(custom, None, Some(Effort::High), auto);
                assert_eq!(snap.provenance.effort, Source::Default);
                assert_eq!(snap.effort, None);

                let opus = ModelSpec::resolve("claude-opus-5-5");
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

        mod apply_theme {
            use super::*;

            #[test]
            fn marks_the_switch_as_session_set() {
                let mut snap = sample_config().snapshot();

                snap.apply_theme("latte");

                assert_eq!(snap.theme_name, "latte");
                assert_eq!(snap.provenance.theme, Source::Session);
            }
        }
    }

    mod status_line_segment {
        use super::*;

        #[test]
        fn names_round_trip_through_serde_and_fromstr() {
            for &segment in StatusLineSegment::VARIANTS {
                let name = <&str>::from(segment);
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
            "ANTHROPIC_BASE_URL",
            "OX_MODEL",
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

        fn write_project_config(cwd: &Path, body: &str) -> Layer {
            let path = cwd.join(".ox").join("config.toml");
            write_config(path.clone(), body);
            Layer::Project(path)
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

        /// Loads a session that cannot sign in from the user config under `dir` and the baseline
        /// env plus `overrides`.
        async fn load_with(dir: &TempDir, overrides: &[(&'static str, &str)]) -> Result<Config> {
            let overrides = overrides.iter().map(|&(key, value)| env(key, value));
            let vars = env_vars([xdg(dir)].into_iter().chain(overrides));
            load_in(dir, vars, false).await
        }

        /// Runs both startup steps, [`Config::load`] and [`Config::resolve_auth`], in `dir` with
        /// exactly `vars` set.
        async fn load_in(
            dir: &TempDir,
            vars: Vec<(&'static str, Option<String>)>,
            can_sign_in: bool,
        ) -> Result<Config> {
            temp_env::async_with_vars(vars, async {
                let mut config = Config::load(dir.path())?;
                config.resolve_auth(can_sign_in).await?;
                Ok(config)
            })
            .await
        }

        fn assert_auth(config: &Config, source: AuthSource, secret: &str) {
            assert!(
                matches!(
                    &config.auth,
                    AuthState::Ready(auth) if auth.source() == source && auth.secret() == secret
                ),
                "{:?} must send {secret} from {source:?}",
                config.auth,
            );
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
                &creds_json("ox-token", None, expires_at).to_string(),
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
            assert_eq!(config.base_url.to_string(), DEFAULT_BASE_URL);
            assert_eq!(config.max_tokens, None);
            assert_eq!(config.effort, Some(Effort::Medium));
            assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::OneHour);
            assert_eq!(config.extra_ca_certs, None);
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
                    ("OX_MODEL", "claude-sonnet-5-5"),
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
            assert_eq!(config.base_url.to_string(), "https://example.invalid");
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
        async fn claude_codes_model_variable_leaves_the_default_model() {
            let dir = tempfile::tempdir().unwrap();
            let mut vars = env_vars([xdg(&dir)]);
            vars.push(env("ANTHROPIC_MODEL", "claude-sonnet-5-5"));

            let config = load_in(&dir, vars, false).await.unwrap();

            assert_eq!(config.model.id(), "claude-opus-5-5");
            assert_eq!(config.provenance.model, Source::Default);
        }

        #[tokio::test]
        async fn user_config_supplies_values_without_env_overrides() {
            let dir = tempfile::tempdir().unwrap();
            let user = write_user_config(
                dir.path(),
                &formatdoc! {r#"
                    [client]
                    model = "claude-haiku-5-5"
                    base_url = "https://config-file.invalid"
                    extra_ca_certs = '{TEST_CA_PATH}'
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
            assert_eq!(config.base_url.to_string(), "https://config-file.invalid");
            assert_eq!(
                config.extra_ca_certs.as_deref(),
                Some(Path::new(TEST_CA_PATH))
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
                    show_thinking: file.clone(),
                    show_welcome: file.clone(),
                    status_line: file,
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
                    ("OX_MODEL", "claude-sonnet-5-5"),
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
        async fn boolean_env_errors_name_the_variable_and_values() {
            for var in [
                "OX_COMPACTION_AUTO_ENABLED",
                "OX_SHOW_THINKING",
                "OX_SHOW_WELCOME",
            ] {
                let dir = tempfile::tempdir().unwrap();

                let err = load_with(&dir, &[(var, "maybe")])
                    .await
                    .expect_err("an invalid flag must fail config load");

                assert_eq!(
                    format!("{err:#}"),
                    format!(
                        "invalid {var} \"maybe\": expected one of 1, true, yes, on, 0, false, no, off"
                    ),
                );
            }
        }

        #[tokio::test]
        async fn propagates_invalid_config_file() {
            let dir = tempfile::tempdir().unwrap();
            let user = write_user_config(
                dir.path(),
                indoc! {"
                    [client]
                    show_thinking = true
                "},
            );

            let err = load_with(&dir, &[("ANTHROPIC_API_KEY", "")])
                .await
                .expect_err("misplaced field must surface as an error");

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "invalid config at {}: line 2, column 1, in `client`: \
                     unknown field `show_thinking`, expected one of `auth`, `base_url`, \
                     `extra_ca_certs`, `headers`, `attribution`, `structured_outputs`, `model`, \
                     `effort`, `max_tokens`, `max_tool_rounds`, \
                     `prompt_cache_ttl`, `compaction`",
                    user.path().display()
                ),
            );
        }

        #[tokio::test]
        async fn rejects_a_base_url_naming_its_variable_value_and_reason() {
            let components =
                "query strings and fragments are not supported. Remove them from the base URL.";
            for (raw, reason) in [
                ("http://gw:8080", "http is allowed only for localhost"),
                ("ftp://example.com", "expected http or https, got \"ftp\""),
                ("gw:8080/v1", "expected http or https, got \"gw\""),
                ("not a url", "relative URL without a base"),
                ("https://example.com/prefix?tenant=x", components),
                ("https://example.com/prefix#anchor", components),
                ("https://example.com/prefix?", components),
                ("https://example.com/prefix#", components),
                ("https://example.com/prefix?tenant=x#anchor", components),
            ] {
                let dir = tempfile::tempdir().unwrap();

                let err = load_with(&dir, &[("ANTHROPIC_BASE_URL", raw)])
                    .await
                    .expect_err("the base URL must be rejected");

                assert_eq!(
                    format!("{err:#}"),
                    format!("invalid ANTHROPIC_BASE_URL {raw:?}: {reason}"),
                );
            }
        }

        #[tokio::test]
        async fn rejects_a_base_url_naming_the_file_that_sets_it() {
            let components =
                "query strings and fragments are not supported. Remove them from the base URL.";
            for (raw, reason) in [
                ("http://gw:8080", "http is allowed only for localhost"),
                ("https://example.com/prefix?tenant=x", components),
                ("https://example.com/prefix#anchor", components),
            ] {
                let dir = tempfile::tempdir().unwrap();
                write_user_config(dir.path(), &format!("client.base_url = {raw:?}"));
                let home = [(HOME_VAR, Some(dir.path()))];

                let err = Box::pin(temp_env::async_with_vars(home, load_with(&dir, &[])))
                    .await
                    .expect_err("the base URL must be rejected");

                assert_eq!(
                    format!("{err:#}"),
                    format!("invalid client.base_url {raw:?} in ~/ox/config.toml: {reason}"),
                );
            }
        }

        #[tokio::test]
        async fn rejects_a_base_url_with_userinfo_and_redacts_the_password() {
            let advice = "a base URL cannot carry a username or password. \
                          Remove them, and set the credential with ANTHROPIC_AUTH_TOKEN, \
                          ANTHROPIC_API_KEY, or [client.auth].";
            for (raw, redacted, reason) in [
                (
                    "https://user:secret@gw.example/anthropic",
                    "https://gw.example/anthropic",
                    advice,
                ),
                (
                    "https://user@gw.example/anthropic",
                    "https://gw.example/anthropic",
                    advice,
                ),
                (
                    "https://:secret@gw.example/anthropic",
                    "https://gw.example/anthropic",
                    advice,
                ),
                (
                    "https://user:secret@gw.example:99999/anthropic",
                    "https://gw.example:99999/anthropic",
                    "invalid port number",
                ),
            ] {
                let dir = tempfile::tempdir().unwrap();

                let err = load_with(&dir, &[("ANTHROPIC_BASE_URL", raw)])
                    .await
                    .expect_err("a base URL must not carry userinfo");
                let message = format!("{err:#}");

                assert_eq!(
                    message,
                    format!("invalid ANTHROPIC_BASE_URL {redacted:?}: {reason}"),
                );
            }
        }

        #[tokio::test]
        async fn accepts_base_urls_with_optional_path_prefix_and_trailing_slash() {
            for raw in [
                "https://example.com",
                "https://example.com/",
                "https://example.com/anthropic",
                "https://example.com/anthropic/",
                "http://127.0.0.1:8080",
                "http://127.0.0.2:8080",
                "http://localhost:8080",
                "http://[::1]:8080",
                "http://localhost:8080/anthropic/",
            ] {
                let dir = tempfile::tempdir().unwrap();

                let config = load_with(&dir, &[("ANTHROPIC_BASE_URL", raw)])
                    .await
                    .unwrap();

                assert_eq!(config.base_url.to_string(), raw);
            }
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
            std::fs::create_dir(dir.path().join("certs")).unwrap();
            std::fs::write(dir.path().join("certs/env.pem"), TEST_CA_PEM).unwrap();
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
                &formatdoc! {"
                    [client]
                    extra_ca_certs = '{TEST_CA_PATH}'
                "},
            );
            let config = load_with(&dir, &node).await.unwrap();
            assert_eq!(config.extra_ca_certs, Some(PathBuf::from(TEST_CA_PATH)),);
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
        async fn extra_ca_certs_reject_an_unreadable_explicit_bundle() {
            let dir = tempfile::tempdir().unwrap();
            let missing = dir.path().join("missing.pem");

            let err = load_with(&dir, &[("OX_EXTRA_CA_CERTS", missing.to_str().unwrap())])
                .await
                .expect_err("an explicit bundle must be readable");

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "invalid OX_EXTRA_CA_CERTS: failed to read extra CA bundle at {}: {}",
                    missing.display(),
                    std::fs::read(&missing).unwrap_err(),
                ),
            );
        }

        #[tokio::test]
        async fn extra_ca_certs_file_rejects_a_relative_bundle() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {r#"
                    [client]
                    extra_ca_certs = "certs/corp.pem"
                "#},
            );

            let err = load_with(&dir, &[])
                .await
                .expect_err("a relative bundle depends on the working directory");

            assert_eq!(
                format!("{err:#}"),
                "invalid client.extra_ca_certs: expected an absolute or `~/` path",
            );
        }

        #[tokio::test]
        async fn extra_ca_certs_file_wins_when_env_is_empty() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                &formatdoc! {"
                    [client]
                    extra_ca_certs = '{TEST_CA_PATH}'
                "},
            );
            let config = load_with(&dir, &[("OX_EXTRA_CA_CERTS", "")]).await.unwrap();
            assert_eq!(config.extra_ca_certs, Some(PathBuf::from(TEST_CA_PATH)),);
        }

        #[tokio::test]
        async fn native_auth_reads_only_the_selected_credential_file() {
            let dir = tempfile::tempdir().unwrap();
            let token = dir.path().join("token");
            std::fs::write(&token, "private-token-sentinel\n").unwrap();
            let path = token.display();
            write_user_config(
                dir.path(),
                &formatdoc! {"
                    [client.auth]
                    type = 'bearer'
                    file = '{path}'
                "},
            );
            let _denied = deny_read_access(&token);

            for (overrides, expected_source, expected_secret) in [
                (
                    vec![("ANTHROPIC_AUTH_TOKEN", "env-token")],
                    AuthSource::AuthTokenEnv,
                    "env-token",
                ),
                (vec![], AuthSource::ApiKeyEnv, "sk-default"),
            ] {
                let config = load_with(&dir, &overrides).await.unwrap();

                assert_auth(&config, expected_source, expected_secret);
            }

            let err = load_with(&dir, &[("ANTHROPIC_API_KEY", "")])
                .await
                .expect_err("the selected unreadable file must fail startup");

            let message = format!("{err:#}");
            assert!(message.contains("client.auth.file"), "{message}");
            assert!(message.contains(&token.display().to_string()), "{message}");
            assert!(!message.contains("private-token-sentinel"), "{message}");
        }

        #[tokio::test]
        async fn native_auth_rejects_a_relative_credential_file() {
            let dir = tempfile::tempdir().unwrap();
            let user = write_user_config(
                dir.path(),
                indoc! {"
                    [client.auth]
                    type = 'bearer'
                    file = 'secrets/token'
                "},
            );

            let err = load_with(&dir, &[])
                .await
                .expect_err("a relative credential file depends on the working directory");

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "invalid config at {}: line 1, column 1, in `client.auth`: \
                     invalid client.auth.file: expected an absolute or `~/` path",
                    user.path().display(),
                ),
            );
        }

        #[tokio::test]
        async fn native_auth_keeps_the_loaded_file_credential_for_the_session() {
            let dir = tempfile::tempdir().unwrap();
            let token = dir.path().join("token");
            std::fs::write(&token, "first-token \r\n").unwrap();
            let path = token.display();
            write_user_config(
                dir.path(),
                &formatdoc! {"
                    [client.auth]
                    type = 'bearer'
                    file = '{path}'
                "},
            );
            let mut config = load_with(&dir, &[("ANTHROPIC_API_KEY", "")]).await.unwrap();
            std::fs::write(&token, "second-token\n").unwrap();

            config.resolve_auth(false).await.unwrap();

            assert_auth(&config, AuthSource::ClientBearer, "first-token ");
        }

        #[tokio::test]
        async fn native_auth_preserves_inline_credential_bytes() {
            for (kind, source) in [
                ("bearer", AuthSource::ClientBearer),
                ("api-key", AuthSource::ClientApiKey),
                ("oauth", AuthSource::ClientOAuth),
            ] {
                let dir = tempfile::tempdir().unwrap();
                write_user_config(
                    dir.path(),
                    &formatdoc! {"
                        [client.auth]
                        type = '{kind}'
                        value = ' inline-token '
                    "},
                );

                let config = load_with(&dir, &[("ANTHROPIC_API_KEY", "")]).await.unwrap();

                assert_auth(&config, source, " inline-token ");
            }
        }

        #[tokio::test]
        async fn native_auth_records_the_winning_file_without_exposing_private_values() {
            let dir = tempfile::tempdir().unwrap();
            write_user_config(
                dir.path(),
                indoc! {"
                    [client]
                    auth = { type = 'bearer', value = 'lower-token' }
                    headers = { x-route = 'lower' }
                    attribution = false
                    structured_outputs = false
                "},
            );
            let token = dir.path().join("token");
            std::fs::write(&token, "private-token-sentinel\n").unwrap();
            let path = token.display();
            let upper = write_drop_in(
                dir.path(),
                "50-profile.toml",
                &formatdoc! {"
                    [client]
                    auth = {{ type = 'api-key', file = '{path}' }}
                    headers = {{ x-route = 'private-header-sentinel' }}
                    attribution = true
                    structured_outputs = true
                "},
            );

            let config = load_with(&dir, &[("ANTHROPIC_API_KEY", "")]).await.unwrap();

            assert_auth(&config, AuthSource::ClientApiKey, "private-token-sentinel");
            for source in [
                &config.provenance.auth,
                &config.provenance.custom_headers,
                &config.provenance.attribution,
                &config.provenance.structured_outputs,
            ] {
                assert_eq!(source, &Source::File(upper.clone()));
            }
            for debug in [format!("{config:?}"), format!("{:?}", config.snapshot())] {
                assert!(!debug.contains("private-token-sentinel"), "{debug}");
                assert!(!debug.contains("private-header-sentinel"), "{debug}");
            }
        }

        #[tokio::test]
        async fn native_auth_login_preserves_the_environment_credential_chain() {
            for (auth_token, api_key, expected_source, expected_secret) in [
                (
                    Some("env-token"),
                    Some("env-key"),
                    AuthSource::AuthTokenEnv,
                    "env-token",
                ),
                (None, Some("env-key"), AuthSource::ApiKeyEnv, "env-key"),
                (None, None, AuthSource::OAuthTokenEnv, "oauth-token"),
            ] {
                let dir = tempfile::tempdir().unwrap();
                write_user_config(dir.path(), "client.auth = { type = 'login' }\n");
                let vars = env_vars(vec![
                    xdg(&dir),
                    ("ANTHROPIC_AUTH_TOKEN", auth_token.map(str::to_owned)),
                    ("ANTHROPIC_API_KEY", api_key.map(str::to_owned)),
                    env("CLAUDE_CODE_OAUTH_TOKEN", "oauth-token"),
                ]);

                let config = load_in(&dir, vars, false).await.unwrap();

                assert_auth(&config, expected_source, expected_secret);
            }
        }

        #[tokio::test]
        async fn resolves_credentials_in_precedence_order() {
            // Each row drops the previous winner, and an empty `ANTHROPIC_API_KEY` counts as unset.
            for (auth_token, api_key, file_key, winner, secret, variable) in [
                (
                    Some("gateway-token"),
                    Some("sk-from-env"),
                    true,
                    AuthSource::AuthTokenEnv,
                    "gateway-token",
                    Some("ANTHROPIC_AUTH_TOKEN"),
                ),
                (
                    None,
                    Some("sk-from-env"),
                    true,
                    AuthSource::ApiKeyEnv,
                    "sk-from-env",
                    Some("ANTHROPIC_API_KEY"),
                ),
                (
                    None,
                    Some(""),
                    true,
                    AuthSource::ClientApiKey,
                    "sk-from-file",
                    None,
                ),
                (
                    None,
                    None,
                    false,
                    AuthSource::OAuthTokenEnv,
                    "oauth-token",
                    Some("CLAUDE_CODE_OAUTH_TOKEN"),
                ),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let user = file_key.then(|| {
                    write_user_config(
                        dir.path(),
                        indoc! {r#"
                            [client]
                            auth = { type = "api-key", value = "sk-from-file" }
                        "#},
                    )
                });
                let vars = env_vars(vec![
                    xdg(&dir),
                    ("ANTHROPIC_API_KEY", api_key.map(str::to_owned)),
                    ("ANTHROPIC_AUTH_TOKEN", auth_token.map(str::to_owned)),
                    env("CLAUDE_CODE_OAUTH_TOKEN", "oauth-token"),
                ]);

                let config = load_in(&dir, vars, false).await.unwrap();

                assert_auth(&config, winner, secret);
                let source = variable.map_or_else(|| Source::File(user.unwrap()), Source::Env);
                assert_eq!(config.provenance.auth, source, "{winner:?}");
            }
        }

        #[tokio::test]
        async fn falls_back_to_the_oxide_code_login() {
            let (dir, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let vars = ox_login_env(&dir, &data, 9_999_999_999_999);
            let config = load_in(&dir, vars, false).await.unwrap();
            assert_auth(&config, AuthSource::OxLogin, "ox-token");
            assert_eq!(config.provenance.auth, Source::Default);
        }

        #[tokio::test]
        async fn only_a_session_that_can_sign_in_starts_signed_out_when_the_stored_login_fails() {
            let (dir, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let vars = ox_login_env(&dir, &data, 1);
            let can_sign_in = load_in(&dir, vars.clone(), true).await;
            let cannot_sign_in = load_in(&dir, vars, false).await;

            let auth = can_sign_in.unwrap().auth;
            assert!(
                matches!(
                    &auth,
                    AuthState::SignedOut(LoginUnavailable { notice, retry: false })
                        if notice == "Not signed in: oxide-code login has expired. \
                                      Run `/login` to sign in again."
                ),
                "an expired login signs out until a new sign-in: {auth:?}",
            );
            assert_eq!(
                format!(
                    "{:#}",
                    cannot_sign_in.expect_err("only a session that can sign in starts signed out")
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

            let config = load_in(&dir, vars, true).await.unwrap();

            let expected = format!(
                "Not signed in: failed to refresh the expired oxide-code login: \
                 another process held {} for over 20 s. \
                 Try again in a moment. \
                 The next prompt tries the stored login again.",
                store.join("credentials.lock").display(),
            );
            assert!(
                matches!(
                    &config.auth,
                    AuthState::SignedOut(LoginUnavailable { notice, retry: true })
                        if *notice == expected
                ),
                "{:?}",
                config.auth,
            );
        }

        #[tokio::test]
        async fn leaves_the_stored_login_to_resolve_auth() {
            let dir = tempfile::tempdir().unwrap();
            let mut vars = env_vars(vec![
                xdg(&dir),
                ("ANTHROPIC_API_KEY", None),
                env("ANTHROPIC_BASE_URL", "https://gateway.example.com"),
            ]);
            vars.push((HOME_VAR, Some(dir.path().to_string_lossy().into_owned())));

            let (loaded, resolved) = temp_env::async_with_vars(vars, async {
                let mut config = Config::load(dir.path()).expect("load reads no credential");
                let loaded = matches!(config.auth, AuthState::Unresolved(None));
                (loaded, config.resolve_auth(true).await)
            })
            .await;
            let err = resolved.expect_err("a gateway gets no stored login");

            assert!(loaded, "the load leaves the stored login unread");
            assert_eq!(
                format!("{err:#}"),
                "no credentials for https://gateway.example.com. \
                 Set ANTHROPIC_AUTH_TOKEN, ANTHROPIC_API_KEY, or CLAUDE_CODE_OAUTH_TOKEN, \
                 or add a credential under `[client.auth]` in ~/ox/config.toml. \
                 Stored logins are sent only to the Anthropic API.",
            );
        }

        #[tokio::test]
        async fn malformed_custom_headers_fail_before_credentials_resolve() {
            // No credential exists for the gateway, so reaching auth would fail with a different
            // error.
            let dir = tempfile::tempdir().unwrap();
            let vars = env_vars(vec![
                xdg(&dir),
                ("ANTHROPIC_API_KEY", None),
                env("ANTHROPIC_BASE_URL", "https://gateway.example.com"),
                env("ANTHROPIC_CUSTOM_HEADERS", "missing colon"),
            ]);
            let err = load_in(&dir, vars, false)
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
        async fn a_gateway_custom_authorization_header_defers_to_the_missing_credential() {
            let dir = tempfile::tempdir().unwrap();
            let vars = env_vars([
                xdg(&dir),
                ("ANTHROPIC_API_KEY", None::<String>),
                env("ANTHROPIC_BASE_URL", "http://127.0.0.1:8080"),
                env(
                    "ANTHROPIC_CUSTOM_HEADERS",
                    "Authorization: Basic placeholder",
                ),
            ]);

            let err = load_in(&dir, vars, false)
                .await
                .expect_err("a gateway needs a credential");
            let message = format!("{err:#}");

            assert!(
                message.starts_with("no credentials for http://127.0.0.1:8080"),
                "{message}"
            );
            assert!(!message.contains("cannot be overridden"), "{message}");
        }

        #[tokio::test]
        async fn a_first_party_custom_authorization_header_names_the_stored_login() {
            let dir = tempfile::tempdir().unwrap();
            let vars = env_vars([
                xdg(&dir),
                ("ANTHROPIC_API_KEY", None::<String>),
                env("ANTHROPIC_CUSTOM_HEADERS", "Authorization: Bearer other"),
            ]);

            let err = load_in(&dir, vars, false)
                .await
                .expect_err("a stored login owns the Authorization header");

            assert_eq!(
                format!("{err:#}"),
                "invalid ANTHROPIC_CUSTOM_HEADERS: \
                 authorization carries a stored login and cannot be overridden",
            );
        }

        #[tokio::test]
        async fn effort_defaults_to_per_model_value() {
            for (model, expected) in [
                ("claude-opus-5-5", Some(Effort::Medium)),
                ("claude-sonnet-5-5", Some(Effort::High)),
                ("custom-model", None),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let config = load_with(&dir, &[("OX_MODEL", model)]).await.unwrap();
                assert_eq!(config.effort, expected, "model={model}");
            }
        }

        #[tokio::test]
        async fn effort_pick_is_dropped_on_unknown_model() {
            let dir = tempfile::tempdir().unwrap();
            let config = load_with(&dir, &[("OX_MODEL", "custom-model"), ("OX_EFFORT", "max")])
                .await
                .unwrap();
            assert_eq!(config.effort, None);
            assert_eq!(config.provenance.effort, Source::Default);
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
        async fn effort_invalid_env_lists_the_accepted_levels() {
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

            assert_eq!(
                format!("{err:#}"),
                "invalid OX_STATUS_LINE \"model,nope\": unknown segment \"nope\": expected one of \
                 current-dir, git-branch, pull-request, pull-request-checks, pull-request-review, \
                 model, model-with-effort, context-used, session-cost, five-hour-limit, \
                 weekly-limit, permission-mode, run-state, queued-prompts, task-progress, \
                 thread-title, session-id, theme, current-time",
            );
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
            let user = write_user_config(
                dir.path(),
                indoc! {r#"
                    [tui]
                    status_line = ["model", "nope"]
                "#},
            );

            let err = load_with(&dir, &[])
                .await
                .expect_err("invalid TOML segment must propagate");

            let variants: Vec<_> = StatusLineSegment::VARIANTS
                .iter()
                .map(|&segment| format!("`{}`", <&str>::from(segment)))
                .collect();
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "invalid config at {}: line 2, column 25, in `tui.status_line`: \
                     unknown variant `nope`, expected one of {}",
                    user.path().display(),
                    variants.join(", ")
                ),
            );
        }

        #[tokio::test]
        async fn status_line_empty_env_is_rejected() {
            let dir = tempfile::tempdir().unwrap();

            let err = load_with(&dir, &[("OX_STATUS_LINE", ",")])
                .await
                .expect_err("empty status line must not launch without run-state");

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "invalid OX_STATUS_LINE \",\": no segments. List at least one of {}, \
                     or remove it to use the default.",
                    variant::names::<StatusLineSegment>()
                ),
            );
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

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "invalid tui.status_line: no segments. List at least one of {}, \
                     or remove it to use the default.",
                    variant::names::<StatusLineSegment>()
                ),
            );
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
        async fn compaction_for_an_unknown_model_requires_tokens_and_explains_the_policy() {
            for (name, setting, enabled, threshold_tokens) in [
                ("default", "", true, None),
                (
                    "tokens",
                    "auto_threshold_tokens = 400000",
                    true,
                    Some(400_000),
                ),
                ("percent", "auto_threshold_percent = 40", true, None),
                (
                    "disabled tokens",
                    "auto_enabled = false\nauto_threshold_tokens = 400000",
                    false,
                    None,
                ),
            ] {
                let dir = tempfile::tempdir().unwrap();
                write_user_config(
                    dir.path(),
                    &formatdoc! {r#"
                        [client]
                        model = "custom-model"

                        [client.compaction]
                        {setting}
                    "#},
                );

                let config = load_with(&dir, &[]).await.unwrap();

                assert_eq!(
                    config.compaction.auto,
                    AutoCompactionConfig {
                        enabled,
                        threshold_tokens,
                    },
                    "{name}",
                );
                assert!(!config.compaction.auto.should_trigger(399_999), "{name}");
                assert_eq!(
                    config.compaction.auto.should_trigger(400_000),
                    threshold_tokens.is_some(),
                    "{name}",
                );
                assert_eq!(
                    config.notices(),
                    ["`custom-model` is not a supported model, \
                      so requests go without thinking, effort, and context management, \
                      and cost tracking stays off. \
                      Auto-compaction requires an explicit token threshold."],
                    "{name}",
                );
            }
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

            let decide = |class, target: Access| config.permission.decide(class, &target);
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

                let target = Access::for_path(&key, &config.workspace);
                assert_eq!(
                    config.permission.decide(PermissionClass::Read, &target),
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
            let user = write_user_config(dir.path(), &roots(&[&shared]));
            let drop_in = write_drop_in(dir.path(), "10-docs.toml", &roots(&[&docs, &shared]));

            let config = load_with(&dir, &[]).await.unwrap();

            let canonical = |path: &Path| dunce::canonicalize(path).unwrap();
            assert_eq!(config.workspace.cwd(), dir.path());
            assert_eq!(
                config.workspace.roots(),
                [canonical(dir.path()), canonical(&shared), canonical(&docs)],
            );
            let root = |path: &Path, layer: &Layer| {
                let raw = path.to_str().unwrap().to_owned();
                Sourced::new(raw, Source::File(layer.clone()))
            };
            assert_eq!(
                config.provenance.extra_roots,
                [
                    root(&shared, &user),
                    root(&docs, &drop_in),
                    root(&shared, &drop_in)
                ],
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
                    ("OX_MODEL", "claude-sonnet-5-5"),
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
                    model: Source::Env("OX_MODEL"),
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
        async fn records_the_layer_behind_each_tui_setting() {
            let dir = tempfile::tempdir().unwrap();
            let user = write_user_config(
                dir.path(),
                indoc! {r##"
                    [tui]
                    show_welcome = false
                    status_line = ["model"]

                    [tui.theme]
                    base = "latte"
                    overrides.text = "#ff0000"
                "##},
            );
            let drop_in = write_drop_in(
                dir.path(),
                "10-team.toml",
                r##"tui.theme.overrides.accent = "#00ff00""##,
            );
            let project =
                write_project_config(dir.path(), r#"tui.status_line = ["model", "run-state"]"#);

            let config = load_with(&dir, &[]).await.unwrap();

            assert_eq!(
                config.status_line,
                [StatusLineSegment::Model, StatusLineSegment::RunState]
            );
            let sources = &config.provenance;
            assert_eq!(sources.show_welcome, Source::File(user.clone()));
            assert_eq!(sources.status_line, Source::File(project));
            assert_eq!(sources.theme, Source::File(user.clone()));
            assert_eq!(
                sources.theme_overrides,
                [Source::File(user), Source::File(drop_in)]
            );
        }

        #[tokio::test]
        async fn records_the_layer_behind_each_permission_rule() {
            let dir = tempfile::tempdir().unwrap();
            let user = write_user_config(
                dir.path(),
                indoc! {r#"
                    [permission]
                    deny.exec = ["git push *"]
                    allow.exec = ["cargo *"]
                    defaults.rm = false
                    defaults.ssh = false
                "#},
            );
            let drop_in = write_drop_in(
                dir.path(),
                "10-team.toml",
                indoc! {r#"
                    [permission]
                    deny.exec = ["curl *"]
                    defaults.git = false
                    defaults.ssh = true
                "#},
            );
            let project = write_project_config(
                dir.path(),
                indoc! {r#"
                    [permission]
                    deny.read = ["./secrets/**"]
                    ask.exec = ["cargo publish *"]
                "#},
            );

            let config = load_with(&dir, &[]).await.unwrap();

            let rule = |tier, class, pattern: &str, layer: &Layer| {
                let pattern = pattern.to_owned();
                let source = Source::File(layer.clone());
                Sourced::new(
                    TierPattern {
                        tier,
                        class,
                        pattern,
                    },
                    source,
                )
            };
            assert_eq!(
                config.provenance.rules,
                [
                    rule(Tier::Deny, PermissionClass::Read, "./secrets/**", &project),
                    rule(Tier::Deny, PermissionClass::Exec, "git push *", &user),
                    rule(Tier::Deny, PermissionClass::Exec, "curl *", &drop_in),
                    rule(
                        Tier::Ask,
                        PermissionClass::Exec,
                        "cargo publish *",
                        &project
                    ),
                    rule(Tier::Allow, PermissionClass::Exec, "cargo *", &user),
                ],
            );
            assert_eq!(
                config.provenance.disabled_defaults,
                [
                    Sourced::new(Shipped::Git, Source::File(drop_in)),
                    Sourced::new(Shipped::Rm, Source::File(user)),
                ],
                "a later layer turns `ssh` back on",
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
                    ("OX_EXTRA_CA_CERTS", TEST_CA_PATH),
                    ("OX_PERMISSION_MODE", "yolo"),
                    ("OX_PROMPT_CACHE_TTL", "5m"),
                    ("OX_SHOW_WELCOME", "0"),
                    ("OX_STATUS_LINE", "model,run-state"),
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
                    show_welcome: Source::Env("OX_SHOW_WELCOME"),
                    status_line: Source::Env("OX_STATUS_LINE"),
                    ..Provenance::default()
                },
            );
        }
    }

    mod model_settings {
        use super::*;

        #[test]
        fn carries_the_model_request_settings() {
            let mut cfg = sample_config();
            cfg.model = ModelSpec::resolve("claude-opus-5-5");
            cfg.max_tokens = Some(2_048);

            let settings = cfg.model_settings();

            assert_eq!(settings.model().id(), "claude-opus-5-5");
            assert_eq!(settings.effort(), Some(Effort::Xhigh));
            assert_eq!(settings.max_tokens(), 2_048);
            assert_eq!(settings.prompt_cache_ttl(), PromptCacheTtl::FiveMin);
            assert!(settings.show_thinking());
        }
    }

    mod notices {
        use super::*;

        #[test]
        fn list_an_unsupported_model_then_why_the_session_is_signed_out() {
            let mut cfg = sample_config();
            cfg.auth = AuthState::SignedOut(LoginUnavailable::new(&LoginError::Missing));

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
            cfg.model = ModelSpec::resolve("claude-opus-5-5");
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

        #[test]
        fn names_no_auth_source_until_a_credential_loads() {
            let mut cfg = sample_config();
            let signed_out = AuthState::SignedOut(LoginUnavailable::new(&LoginError::Missing));

            for state in [AuthState::Unresolved(None), signed_out] {
                cfg.auth = state;

                assert_eq!(cfg.snapshot().auth, None, "{:?}", cfg.auth);
            }
        }
    }

    mod reference {
        use super::*;

        const GUIDE: &str = include_str!("../../../docs/guide/configuration.md");

        /// The first-column code spans of the table under `heading` in the configuration guide.
        fn reference_rows(heading: &str) -> Vec<&'static str> {
            let rows: Vec<_> = GUIDE
                .lines()
                .skip_while(|line| *line != heading)
                .skip(1)
                .skip_while(|line| !line.starts_with('|'))
                .take_while(|line| line.starts_with('|'))
                .skip(2)
                .map(|row| row.split('|').nth(1).unwrap().trim().trim_matches('`'))
                .collect();
            assert_ne!(
                rows,
                [] as [&str; 0],
                "the guide has no table under {heading:?}"
            );
            rows
        }

        #[derive(Clone, Copy)]
        enum Shape {
            Table(&'static [&'static str]),
            Key,
        }

        /// A deserializer that leads `FileConfig` along `path`, one field per table, and records
        /// the shape of the value at its end. A struct, such as `[client]`, is a table, and any
        /// other value is a key, even an optional table such as `permission.allow`.
        #[derive(Clone, Copy)]
        struct Probe<'a> {
            path: &'a [&'static str],
            shape: &'a Cell<Option<Shape>>,
        }

        impl Probe<'_> {
            fn end(self, shape: Shape) -> de::value::Error {
                self.shape.set(Some(shape));
                de::Error::custom("probed")
            }
        }

        impl<'de> de::Deserializer<'de> for Probe<'_> {
            type Error = de::value::Error;

            fn deserialize_any<V: de::Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
                Err(self.end(Shape::Key))
            }

            fn deserialize_struct<V: de::Visitor<'de>>(
                self,
                _: &'static str,
                fields: &'static [&'static str],
                visitor: V,
            ) -> Result<V::Value, Self::Error> {
                match self.path.split_first() {
                    Some((&field, path)) => visitor.visit_map(Field {
                        name: Some(field),
                        value: Self { path, ..self },
                    }),
                    None => Err(self.end(Shape::Table(fields))),
                }
            }

            serde::forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
                byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map enum
                identifier ignored_any
            }
        }

        /// A table holding only the next field of a [`Probe`]'s path.
        struct Field<'a> {
            name: Option<&'static str>,
            value: Probe<'a>,
        }

        impl<'de> de::MapAccess<'de> for Field<'_> {
            type Error = de::value::Error;

            fn next_key_seed<K: de::DeserializeSeed<'de>>(
                &mut self,
                seed: K,
            ) -> Result<Option<K::Value>, Self::Error> {
                self.name
                    .take()
                    .map(|name| seed.deserialize(StrDeserializer::new(name)))
                    .transpose()
            }

            fn next_value_seed<V: de::DeserializeSeed<'de>>(
                &mut self,
                seed: V,
            ) -> Result<V::Value, Self::Error> {
                seed.deserialize(self.value)
            }
        }

        /// Appends every key under `path` to `keys` as a dotted path.
        fn collect_keys(path: &mut Vec<&'static str>, keys: &mut Vec<String>) {
            let shape = Cell::new(None);
            FileConfig::deserialize(Probe {
                path,
                shape: &shape,
            })
            .unwrap_err();
            match shape.get().expect("the probe reaches the end of its path") {
                Shape::Table(fields) => {
                    for field in fields {
                        path.push(field);
                        collect_keys(path, keys);
                        path.pop();
                    }
                }
                Shape::Key => keys.push(path.join(".")),
            }
        }

        /// Returns whether `name` is a variable that oxide-code owns or shares with Claude Code.
        fn is_owned_var(name: &str) -> bool {
            ["ANTHROPIC_", "CLAUDE_", "OX_"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
        }

        /// The owned variables that string literals name in the production lines of each workspace
        /// member's `src` files other than `testing.rs` and `local_testing.rs`, in byte order.
        fn owned_vars_in_sources() -> Vec<String> {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap();
            let manifest: toml::Table = std::fs::read_to_string(root.join("Cargo.toml"))
                .unwrap()
                .parse()
                .unwrap();
            let literal = Regex::new(r#""([A-Z][A-Z0-9_]+)""#).unwrap();
            let mut vars = BTreeSet::new();
            for member in manifest["workspace"]["members"].as_array().unwrap() {
                for entry in ignore::Walk::new(root.join(member.as_str().unwrap()).join("src")) {
                    let path = entry.unwrap().into_path();
                    let stem = path.file_stem().unwrap();
                    if path.extension().is_some_and(|ext| ext == "rs")
                        && stem != "testing"
                        && stem != "local_testing"
                    {
                        let source = std::fs::read_to_string(path).unwrap();
                        vars.extend(
                            production_lines(&source)
                                .flat_map(|line| literal.captures_iter(line))
                                .map(|name| name[1].to_owned())
                                .filter(|name| is_owned_var(name)),
                        );
                    }
                }
            }
            vars.into_iter().collect()
        }

        /// The lines of `source` outside its top-level `tests`, `testing`, and `local_testing`
        /// modules, each of which ends at the next line holding `}` alone.
        fn production_lines(source: &str) -> impl Iterator<Item = &str> {
            let test_module =
                Regex::new(r"^(pub(\(\w+\))? )?mod (local_testing|testing|tests) \{$").unwrap();
            let mut in_test_module = false;
            source.lines().filter(move |line| {
                let skip = in_test_module || test_module.is_match(line);
                in_test_module = skip && *line != "}";
                !skip
            })
        }

        #[test]
        fn lists_every_config_key_in_order() {
            let mut keys = Vec::new();
            collect_keys(&mut Vec::new(), &mut keys);
            keys.sort();

            assert_eq!(
                reference_rows("### Config Keys"),
                keys,
                "the Config Keys table must list exactly the `FileConfig` keys, in byte order",
            );
        }

        #[test]
        fn lists_every_environment_variable_in_order() {
            let documented = reference_rows("### Environment Variables");
            let read = owned_vars_in_sources();

            assert!(
                documented.is_sorted(),
                "the Environment Variables table must be in byte order: {documented:?}",
            );
            let owned: Vec<_> = documented
                .into_iter()
                .filter(|name| is_owned_var(name))
                .collect();
            assert_eq!(
                owned, read,
                "the Environment Variables table must list exactly the `ANTHROPIC_*`, `CLAUDE_*`, \
                 and `OX_*` variables that production code names",
            );
        }
    }
}
