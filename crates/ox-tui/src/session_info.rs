//! The [`App`](crate::app::App)'s snapshot of the running session, which slash commands, the status
//! bar, and the welcome screen read.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ox_core::client::RateLimits;
use ox_core::config::ConfigSnapshot;
use ox_core::permission::rule::Rule;
use ox_core::protocol::UsageSnapshot;
use ox_core::session::store::SessionStore;
use ox_core::tool::bash::background::BackgroundShell;

/// The running session's state, which the App keeps current from engine events and hands to every
/// slash command and the status bar. The saved record `--list` reads is
/// [`ox_core::session::store::listing::SessionInfo`].
pub(crate) struct LiveSessionInfo {
    /// The process's session store, which the `/resume` picker lists from.
    pub(crate) store: SessionStore,
    /// The working directory, which the git probes use.
    pub(crate) cwd: PathBuf,
    /// [`Self::cwd`] as shown, with the home directory as `~`.
    pub(crate) display_cwd: String,
    /// The session's project, which scopes the session store, where `/init` checks for instruction
    /// files, and where `/diff` runs git: the git repository containing the session's working
    /// directory, or that directory itself outside a repository.
    pub(crate) project_root: PathBuf,
    pub(crate) version: &'static str,
    pub(crate) session_id: String,
    /// Written through [`Self::set_title`].
    pub(crate) title: Option<String>,
    /// Context and cost of the session's last request, until the session or its context changes.
    pub(crate) usage: Option<UsageSnapshot>,
    /// The account's usage-limit windows that have not reset, as the last response reported them.
    pub(crate) rate_limits: Option<RateLimits>,
    /// Rules granted from approvals for the rest of this session.
    pub(crate) session_allow: Vec<Rule>,
    /// Failures in a row that paused automatic compaction, until the engine reports a resume.
    pub(crate) auto_compaction_paused_after: Option<u8>,
    /// The open background shells, as the engine last reported them.
    pub(crate) background_shells: Vec<BackgroundShell>,
    pub(crate) config: ConfigSnapshot,
    /// The `[tui.theme.overrides]` slots that apply over every theme, leaving out the ignored ones.
    pub(crate) theme_overrides: Vec<String>,
}

impl LiveSessionInfo {
    /// Sets the title, dropping a blank one.
    pub(crate) fn set_title(&mut self, title: Option<String>) {
        self.title = title.filter(|t| !t.trim().is_empty());
    }

    /// Drops each usage-limit window that reset by `unix_now`, since its spent share no longer
    /// applies until a response reports the new window. Returns whether any window dropped.
    pub(crate) fn expire_rate_limits(&mut self, unix_now: i64) -> bool {
        let Some(limits) = &mut self.rate_limits else {
            return false;
        };
        let mut expired = false;
        for window in [&mut limits.five_hour, &mut limits.seven_day] {
            expired |= window
                .take_if(|window| window.resets_at <= unix_now)
                .is_some();
        }
        expired
    }

    /// When the next usage-limit window resets, on the clock of `now`, which is `unix_now` on the
    /// wall clock.
    pub(crate) fn next_rate_limit_reset(&self, now: Instant, unix_now: i64) -> Option<Instant> {
        let limits = self.rate_limits?;
        [limits.five_hour, limits.seven_day]
            .into_iter()
            .flatten()
            .map(|window| {
                let wait = u64::try_from(window.resets_at - unix_now).unwrap_or(0);
                now + Duration::from_secs(wait)
            })
            .min()
    }
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;

    use ox_core::config::ConfigSnapshot;
    use ox_core::config::compaction::CompactionConfig;
    use ox_core::config::testing::test_snapshot;
    use ox_core::model::{Effort, ModelSpec};
    use ox_core::session::store::SessionStore;

    use super::LiveSessionInfo;

    /// Fully populated [`LiveSessionInfo`] that the slash, welcome, and app tests share. Its store
    /// was never set up, so a test that reads saved sessions supplies its own.
    pub(crate) fn test_session_info() -> LiveSessionInfo {
        LiveSessionInfo {
            store: SessionStore::unopened("repo"),
            cwd: PathBuf::from("/repo"),
            display_cwd: "~/test".to_owned(),
            project_root: PathBuf::from("/repo"),
            version: "0.0.0-test",
            session_id: "test-session".to_owned(),
            title: None,
            usage: None,
            rate_limits: None,
            session_allow: Vec::new(),
            auto_compaction_paused_after: None,
            background_shells: Vec::new(),
            config: ConfigSnapshot {
                effort: Some(Effort::High),
                max_tokens: None,
                compaction: CompactionConfig::with_threshold(155_000),
                ..test_snapshot()
            },
            theme_overrides: Vec::new(),
        }
    }

    /// [`test_session_info`] on `test-model`, an unknown ID whose display name falls back to the
    /// literal ID, keeping insta snapshots stable.
    pub(crate) fn unknown_model_session_info() -> LiveSessionInfo {
        let mut info = test_session_info();
        info.config.model = ModelSpec::resolve("test-model");
        info
    }
}

#[cfg(test)]
mod tests {
    use ox_core::client::RateLimitWindow;

    use super::testing::test_session_info;
    use super::*;

    /// 2026-01-01 12:00 UTC.
    const UNIX_NOW: i64 = 1_767_268_800;

    fn window(resets_at: i64) -> RateLimitWindow {
        RateLimitWindow {
            utilization: 0.5,
            resets_at,
        }
    }

    mod set_title {
        use super::*;

        #[test]
        fn keeps_a_title_and_drops_a_blank_or_missing_one() {
            for (title, kept) in [
                (Some("Fix auth bug"), Some("Fix auth bug")),
                (Some("   \n"), None),
                (None, None),
            ] {
                let mut info = test_session_info();
                info.title = Some("Earlier".to_owned());

                info.set_title(title.map(ToOwned::to_owned));

                assert_eq!(info.title.as_deref(), kept, "{title:?}");
            }
        }
    }

    mod expire_rate_limits {
        use super::*;

        #[test]
        fn drops_each_window_once_it_resets() {
            let mut info = test_session_info();
            info.rate_limits = Some(RateLimits {
                five_hour: Some(window(UNIX_NOW)),
                seven_day: Some(window(UNIX_NOW + 1)),
            });

            assert!(info.expire_rate_limits(UNIX_NOW));
            assert_eq!(
                info.rate_limits,
                Some(RateLimits {
                    five_hour: None,
                    seven_day: Some(window(UNIX_NOW + 1)),
                }),
            );
            assert!(!info.expire_rate_limits(UNIX_NOW), "the open window stays");
        }

        #[test]
        fn has_nothing_to_drop_without_limits() {
            let mut info = test_session_info();

            assert!(!info.expire_rate_limits(UNIX_NOW));
        }
    }

    mod next_rate_limit_reset {
        use super::*;

        #[test]
        fn is_the_earliest_reset_and_now_for_a_passed_one() {
            for (five_hour, seven_day, wait) in [
                (
                    Some(window(UNIX_NOW + 120)),
                    Some(window(UNIX_NOW + 240)),
                    120,
                ),
                (
                    Some(window(UNIX_NOW + 240)),
                    Some(window(UNIX_NOW + 120)),
                    120,
                ),
                (None, Some(window(UNIX_NOW + 240)), 240),
                (Some(window(UNIX_NOW - 1)), None, 0),
            ] {
                let mut info = test_session_info();
                info.rate_limits = Some(RateLimits {
                    five_hour,
                    seven_day,
                });
                let now = Instant::now();

                assert_eq!(
                    info.next_rate_limit_reset(now, UNIX_NOW),
                    Some(now + Duration::from_secs(wait)),
                    "{five_hour:?} / {seven_day:?}",
                );
            }
        }

        #[test]
        fn is_none_without_limits() {
            let info = test_session_info();

            assert_eq!(info.next_rate_limit_reset(Instant::now(), UNIX_NOW), None);
        }
    }
}
