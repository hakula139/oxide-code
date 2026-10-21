//! The [`App`](crate::app::App)'s snapshot of the running session, which slash commands and the
//! welcome screen read.

use std::path::PathBuf;

use ox_core::config::ConfigSnapshot;
use ox_core::permission::rule::Rule;
use ox_core::tool::bash::background::BackgroundShell;

/// The running session's state, which the App keeps current from engine events and hands to
/// every slash command.
/// The saved record `--list` reads is [`ox_core::session::entry::SessionInfo`].
pub(crate) struct LiveSessionInfo {
    /// The working directory, which the git probes and the session store use.
    pub(crate) cwd: PathBuf,
    /// [`Self::cwd`] as shown, with the home directory as `~`.
    pub(crate) display_cwd: String,
    /// The session's project, which scopes the session store, where `/init` checks for instruction
    /// files, and where `/diff` runs git: the git repository containing the session's working
    /// directory, or that directory itself outside a repository.
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

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;

    use ox_core::config::compaction::CompactionConfig;
    use ox_core::config::testing::test_snapshot;
    use ox_core::config::{ConfigSnapshot, Effort};

    use super::LiveSessionInfo;

    /// Fully populated [`LiveSessionInfo`] that the slash, welcome, and app tests share.
    pub(crate) fn test_session_info() -> LiveSessionInfo {
        LiveSessionInfo {
            cwd: PathBuf::from("/repo"),
            display_cwd: "~/test".to_owned(),
            project_root: PathBuf::from("/repo"),
            version: "0.0.0-test",
            session_id: "test-session".to_owned(),
            session_allow: Vec::new(),
            auto_compaction_paused_after: None,
            background_shells: Vec::new(),
            config: ConfigSnapshot {
                effort: Some(Effort::High),
                max_tokens: None,
                compaction: CompactionConfig::with_threshold_for_test(155_000),
                ..test_snapshot()
            },
        }
    }
}
