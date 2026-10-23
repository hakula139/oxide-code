//! The TOML schema every config file shares: the `[client]`, `[permission]`, and `[tui]` tables.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::RangeInclusive;

use anyhow::{Context as _, Result};
use serde::de::{self, Unexpected};
use serde::{Deserialize, Deserializer};

use super::StatusLineSegment;
use super::compaction::ThresholdPercent;
use crate::model::{Effort, PromptCacheTtl};
use crate::permission::defaults::Shipped;
use crate::permission::rule::Rule;
use crate::permission::{Mode, PermissionClass, Tier};
use crate::util::env::whole_number_in;

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
    pub(super) api_key: Option<String>,
    pub(super) base_url: Option<String>,
    pub(super) extra_ca_certs: Option<String>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<Effort>,
    #[serde(deserialize_with = "count")]
    pub(super) max_tokens: Option<u32>,
    #[serde(deserialize_with = "count")]
    pub(super) max_tool_rounds: Option<u32>,
    pub(super) prompt_cache_ttl: Option<PromptCacheTtl>,
    pub(super) compaction: CompactionTable,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct CompactionTable {
    #[serde(rename = "auto_enabled")]
    pub(super) enabled: Option<bool>,
    #[serde(rename = "auto_threshold_tokens", deserialize_with = "tokens")]
    pub(super) threshold_tokens: Option<u32>,
    #[serde(rename = "auto_threshold_percent", deserialize_with = "percent")]
    pub(super) threshold_percent: Option<u8>,
}

/// The tier tables hold raw patterns and `extra_roots` raw paths, which the loader parses per file,
/// so a malformed entry fails at startup. `defaults` turns shipped rule groups off by name.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct PermissionTable {
    pub(super) mode: Option<Mode>,
    pub(super) extra_roots: Option<Vec<String>>,
    pub(super) deny: Option<TierTable>,
    pub(super) ask: Option<TierTable>,
    pub(super) allow: Option<TierTable>,
    pub(super) defaults: Option<BTreeMap<Shipped, bool>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct TierTable {
    pub(super) read: Option<Vec<String>>,
    pub(super) edit: Option<Vec<String>>,
    pub(super) exec: Option<Vec<String>>,
    pub(super) net: Option<Vec<String>>,
    pub(super) mcp: Option<Vec<String>>,
}

impl TierTable {
    /// Parses every pattern of the `tier` table, naming the key and pattern of a malformed one.
    pub(super) fn rules(&self, tier: Tier) -> Result<Vec<Rule>> {
        [
            (PermissionClass::Read, &self.read),
            (PermissionClass::Edit, &self.edit),
            (PermissionClass::Exec, &self.exec),
            (PermissionClass::Net, &self.net),
            (PermissionClass::Mcp, &self.mcp),
        ]
        .into_iter()
        .flat_map(|(class, patterns)| {
            patterns
                .iter()
                .flatten()
                .map(move |pattern| (class, pattern))
        })
        .map(|(class, pattern)| {
            Rule::parse(class, pattern)
                .with_context(|| format!("invalid permission.{tier}.{class} pattern {pattern:?}"))
        })
        .collect()
    }
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

// ── Whole Numbers ──

// Each range matches the env var that overrides the key, so both report the same accepted values.

fn count<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    whole_number(deserializer, 1..=u32::MAX)
}

fn tokens<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    whole_number(deserializer, 0..=u32::MAX)
}

fn percent<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u8>, D::Error> {
    whole_number(deserializer, ThresholdPercent::RANGE)
}

fn whole_number<'de, D, T>(deserializer: D, range: RangeInclusive<T>) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: fmt::Display + PartialOrd + TryFrom<i64>,
{
    deserializer.deserialize_option(WholeNumber(range))
}

struct WholeNumber<T>(RangeInclusive<T>);

impl<'de, T> de::Visitor<'de> for WholeNumber<T>
where
    T: fmt::Display + PartialOrd + TryFrom<i64>,
{
    type Value = Option<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&whole_number_in(&self.0))
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_i64(self)
    }

    fn visit_i64<E: de::Error>(self, raw: i64) -> Result<Self::Value, E> {
        match T::try_from(raw) {
            Ok(value) if self.0.contains(&value) => Ok(Some(value)),
            _ => Err(E::invalid_value(Unexpected::Signed(raw), &self)),
        }
    }
}
