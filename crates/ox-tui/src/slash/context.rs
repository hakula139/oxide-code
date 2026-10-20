//! Inputs to [`SlashCommand::execute`](super::registry::SlashCommand::execute): [`SlashContext`]
//! borrows App-owned state, and [`LiveSessionInfo`] is the session-level snapshot.

use std::path::PathBuf;

use ox_core::config::ConfigSnapshot;
use ox_core::permission::rule::Rule;
use ox_core::tool::bash::background::BackgroundShell;

/// Snapshot of the running session handed to every slash command, kept current from engine
/// events. The saved record `--list` reads is [`ox_core::session::entry::SessionInfo`].
pub(crate) struct LiveSessionInfo {
    pub(crate) cwd: String,
    /// The real path behind the display `cwd`, for the status bar's git probes, `/diff`, and the
    /// session store.
    pub(crate) working_dir: Option<PathBuf>,
    /// The root `/init` checks for instruction files: the git repository containing the session's
    /// working directory, or that directory itself outside a repository.
    pub(crate) project_root: PathBuf,
    pub(crate) version: &'static str,
    pub(crate) session_id: String,
    /// Rules granted from approvals for the rest of this session.
    pub(crate) session_allow: Vec<Rule>,
    /// Failures in a row that paused automatic compaction, until the engine reports a resume.
    pub(crate) auto_compaction_paused_after: Option<u8>,
    /// The open background shells, as the engine last reported them.
    pub(crate) background_shells: Vec<BackgroundShell>,
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

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;

    use ox_core::config::auth::AuthSource;
    use ox_core::config::compaction::CompactionConfig;
    use ox_core::config::gateway::GatewaySnapshot;
    use ox_core::config::provenance::Provenance;
    use ox_core::config::{ConfigSnapshot, Effort, PromptCacheTtl, StatusLineSegment};
    use ox_core::model::ModelSpec;
    use ox_core::permission::Mode;

    use super::LiveSessionInfo;

    /// Fully-populated `LiveSessionInfo` that the slash command, welcome, and TUI app tests share.
    pub(crate) fn test_session_info() -> LiveSessionInfo {
        // Real MODELS row so the display name resolves to a known label.
        LiveSessionInfo {
            cwd: "~/test".to_owned(),
            working_dir: Some(PathBuf::from("/repo")),
            project_root: PathBuf::from("/repo"),
            version: "0.0.0-test",
            session_id: "test-session".to_owned(),
            session_allow: Vec::new(),
            auto_compaction_paused_after: None,
            background_shells: Vec::new(),
            config: ConfigSnapshot {
                auth: Some(AuthSource::ApiKeyEnv),
                signed_out_notice: None,
                base_url: "https://api.test.invalid".to_owned(),
                extra_ca_certs: None,
                gateway: GatewaySnapshot {
                    custom_header_names: Vec::new(),
                    attribution: true,
                    structured_outputs: true,
                },
                model: ModelSpec::resolve("claude-opus-5-5".to_owned()),
                effort: Some(Effort::High),
                max_tokens: None,
                max_tool_rounds: None,
                prompt_cache_ttl: PromptCacheTtl::OneHour,
                compaction: CompactionConfig::with_threshold_for_test(155_000),
                permission_mode: Mode::Manual,
                show_thinking: false,
                show_welcome: true,
                status_line: StatusLineSegment::DEFAULT.to_vec(),
                theme_name: "mocha".to_owned(),
                provenance: Provenance::default(),
            },
        }
    }
}
