//! The TOML schema every config file shares: the `[client]`, `[permission]`, and `[tui]` tables.

use serde::Deserialize;

use super::{Effort, PromptCacheTtl, StatusLineSegment};
use crate::permission::Mode;

/// One config file, or several merged. Every key is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct FileConfig {
    pub(super) client: ClientTable,
    pub(super) permission: PermissionTable,
    pub(super) tui: TuiTable,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ClientTable {
    // Any new field that influences credentials, endpoint identity, or TLS trust must also be
    // listed in `load::reject_project_secrets` so a checked-in project file cannot set it.
    pub(super) api_key: Option<String>,
    pub(super) base_url: Option<String>,
    pub(super) extra_ca_certs: Option<String>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<Effort>,
    pub(super) max_tokens: Option<u32>,
    pub(super) max_tool_rounds: Option<u32>,
    pub(super) prompt_cache_ttl: Option<PromptCacheTtl>,
    pub(super) compaction: CompactionTable,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct CompactionTable {
    #[serde(rename = "auto_enabled")]
    pub(super) enabled: Option<bool>,
    #[serde(rename = "auto_threshold_tokens")]
    pub(super) threshold_tokens: Option<u32>,
    #[serde(rename = "auto_threshold_percent")]
    pub(super) threshold_percent: Option<u8>,
}

/// `allow` and `deny` hold raw rule strings that `Config::load` parses, so a malformed rule fails
/// at startup.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct PermissionTable {
    pub(super) mode: Option<Mode>,
    pub(super) allow: Option<Vec<String>>,
    pub(super) deny: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct TuiTable {
    pub(super) show_thinking: Option<bool>,
    pub(super) show_welcome: Option<bool>,
    pub(super) status_line: Option<Vec<StatusLineSegment>>,
    pub(super) theme: ThemeTable,
}

/// `overrides` stays raw TOML because the TUI owns the slot patch format.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ThemeTable {
    pub(super) base: Option<String>,
    pub(super) overrides: toml::Table,
}
