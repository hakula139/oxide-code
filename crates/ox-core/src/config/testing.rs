//! The [`Config`] fixture that every crate's tests build on.

use std::path::Path;

use reqwest::header::HeaderMap;

use super::auth::{Auth, AuthSource};
use super::compaction::CompactionConfig;
use super::gateway::GatewayCompat;
use super::provenance::{Provenance, Source};
use super::{
    Config, ConfigSnapshot, DEFAULT_THEME, PromptCacheTtl, StatusLineSegment, ThemeConfig,
};
use crate::model::ModelSpec;
use crate::permission::{Policy, Workspace};

/// Minimal [`Config`] signed in with `auth` against `base_url`: compaction off, a 128-token
/// output cap, and the default permission policy in `/repo`. Tests override fields with struct
/// update syntax.
pub(crate) fn test_config(base_url: impl Into<String>, auth: Auth, model: &str) -> Config {
    Config {
        auth: Some(auth),
        signed_out: None,
        base_url: base_url.into(),
        extra_ca_certs: None,
        gateway: GatewayCompat {
            custom_headers: HeaderMap::new(),
            attribution: true,
            structured_outputs: true,
        },
        model: ModelSpec::resolve(model.to_owned()),
        effort: None,
        max_tokens: Some(128),
        max_tool_rounds: None,
        prompt_cache_ttl: PromptCacheTtl::OneHour,
        compaction: CompactionConfig::disabled(),
        permission: Policy::default(),
        workspace: Workspace::new(Path::new("/repo")),
        show_thinking: false,
        show_welcome: true,
        status_line: StatusLineSegment::DEFAULT.to_vec(),
        theme: ThemeConfig {
            base: DEFAULT_THEME.to_owned(),
            base_source: Source::Default,
            overrides: toml::Table::new(),
        },
        provenance: Provenance::default(),
    }
}

/// The snapshot of a [`test_config`] for Opus 5.5 signed in with `ANTHROPIC_API_KEY`, which
/// frontend tests show.
pub fn test_snapshot() -> ConfigSnapshot {
    test_config(
        "https://api.test.invalid",
        Auth::new(AuthSource::ApiKeyEnv, "sk-test"),
        "claude-opus-5-5",
    )
    .snapshot()
}
