//! Inputs to [`SlashCommand::execute`](super::registry::SlashCommand::execute): [`SlashContext`]
//! borrows App-owned state, and [`LiveSessionInfo`] is the session-level snapshot.

use std::path::PathBuf;

use crate::config::ConfigSnapshot;
use crate::permission::rule::Rule;

/// Live snapshot of the running session handed to every slash command. Built at TUI startup and
/// rebound by `SessionRolled` / `ConfigChanged` so commands always see the current model, effort,
/// session id, and theme. Distinct from [`crate::session::entry::SessionInfo`], which is the
/// persisted JSONL record consumed by `--list`.
pub(crate) struct LiveSessionInfo {
    pub(crate) cwd: String,
    /// The real path behind the display `cwd`, for the status bar's git probes and `/diff`.
    pub(crate) git_cwd: Option<PathBuf>,
    /// The root `/init` checks for instruction files: the git repository containing the session's
    /// working directory, or that directory itself outside a repository.
    pub(crate) project_root: PathBuf,
    pub(crate) git_branch: Option<String>,
    pub(crate) version: &'static str,
    pub(crate) session_id: String,
    /// Rules granted from approvals for the rest of this session.
    pub(crate) session_allow: Vec<Rule>,
    pub(crate) config: ConfigSnapshot,
}

/// Read-only App state for one [`super::registry::SlashCommand::execute`] call. Commands report
/// their effects through the returned [`super::registry::SlashOutcome`].
pub(crate) struct SlashContext<'a> {
    pub(crate) info: &'a LiveSessionInfo,
    /// Live session title, which `/rename` pre-fills its modal from. `None` until seeded.
    pub(crate) title: Option<&'a str>,
}

impl<'a> SlashContext<'a> {
    pub(crate) fn new(info: &'a LiveSessionInfo, title: Option<&'a str>) -> Self {
        Self { info, title }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;

    use super::LiveSessionInfo;
    use crate::config::compaction::CompactionConfig;
    use crate::config::{ConfigSnapshot, Effort, PromptCacheTtl};

    /// Fully-populated `LiveSessionInfo` for per-command tests.
    pub(crate) fn test_session_info() -> LiveSessionInfo {
        // Real MODELS row so the display name resolves to a known label.
        LiveSessionInfo {
            cwd: "~/test".to_owned(),
            git_cwd: None,
            project_root: PathBuf::from("/repo"),
            git_branch: Some("main".to_owned()),
            version: "0.0.0-test",
            session_id: "test-session".to_owned(),
            session_allow: Vec::new(),
            config: ConfigSnapshot {
                auth: Some(crate::config::auth::AuthSource::ApiKeyEnv),
                signed_out_notice: None,
                base_url: "https://api.test.invalid".to_owned(),
                extra_ca_certs: None,
                gateway: crate::config::gateway::GatewaySnapshot {
                    custom_header_names: Vec::new(),
                    attribution: true,
                    structured_outputs: true,
                },
                model: crate::model::ModelSpec::resolve("claude-opus-5-5".to_owned()),
                effort: Some(Effort::High),
                max_tokens: None,
                max_tool_rounds: None,
                prompt_cache_ttl: PromptCacheTtl::OneHour,
                compaction: CompactionConfig::with_threshold_for_test(155_000),
                permission_mode: crate::permission::Mode::Auto,
                show_thinking: false,
                show_welcome: true,
                status_line: crate::config::StatusLineSegment::DEFAULT.to_vec(),
                theme_name: "mocha".to_owned(),
                provenance: crate::config::provenance::Provenance::default(),
            },
        }
    }
}
