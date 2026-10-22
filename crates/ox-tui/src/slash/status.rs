//! `/status` opens a read-only [`KvOverview`] of the live session and its runtime settings.

use ox_core::config::{display_auth, display_auto_compaction, display_bool, display_effort};

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashOutcome};
use crate::modal::kv_overview::{KvOverview, KvSection};
use crate::session_info::LiveSessionInfo;

// ── StatusCmd ──

pub(super) struct StatusCmd;

impl SlashCommand for StatusCmd {
    fn name(&self) -> &'static str {
        "status"
    }

    fn description(&self) -> &'static str {
        "Show current session info"
    }

    fn echoes_input(&self, _args: &str) -> bool {
        false
    }

    fn execute(&self, _args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        Ok(SlashOutcome::Modal(Box::new(build_modal(ctx.info))))
    }
}

// ── Helpers ──

fn build_modal(info: &LiveSessionInfo) -> KvOverview {
    let model = &info.config.model;
    let model = format!("{} ({})", model.display_name(), model.id());
    let auto_compaction = match info.auto_compaction_paused_after {
        Some(failures) => {
            format!("paused after {failures} failures in a row (run `/compact` to resume)")
        }
        None => display_auto_compaction(info.config.compaction.auto),
    };
    let rows = vec![
        ("Model".to_owned(), model),
        ("Effort".to_owned(), display_effort(info.config.effort)),
        (
            "Permission Mode".to_owned(),
            info.config.permission_mode.to_string(),
        ),
        ("Working Directory".to_owned(), info.display_cwd.clone()),
        ("Session".to_owned(), info.session_id.clone()),
        ("Auth".to_owned(), display_auth(info.config.auth)),
        ("Version".to_owned(), info.version.to_owned()),
        (
            "Prompt Cache TTL".to_owned(),
            info.config.prompt_cache_ttl.to_string(),
        ),
        ("Auto Compaction".to_owned(), auto_compaction),
        (
            "Show Thinking".to_owned(),
            display_bool(info.config.show_thinking).to_owned(),
        ),
        (
            "Show Welcome".to_owned(),
            display_bool(info.config.show_welcome).to_owned(),
        ),
    ];
    let mut sections = vec![KvSection::new(rows)];
    if !info.session_allow.is_empty() {
        let rules = info
            .session_allow
            .iter()
            .map(|rule| (format!("allow.{}", rule.class()), rule.pattern().to_owned()))
            .collect();
        sections.push(KvSection::new(rules).with_heading("Session Rules"));
    }
    if !info.background_shells.is_empty() {
        let shells = info
            .background_shells
            .iter()
            .map(|shell| {
                (
                    shell.id.clone(),
                    shell.state.describe(),
                    shell.command_preview(),
                )
            })
            .collect();
        sections.push(KvSection::annotated(shells).with_heading("Background Shells"));
    }
    KvOverview::new("Status", sections)
}

#[cfg(test)]
mod tests {
    use ox_core::permission::PermissionClass;
    use ox_core::permission::rule::Rule;
    use ox_core::tool::bash::background::testing::shells_in_each_state;

    use super::*;
    use crate::modal::Modal;
    use crate::modal::testing::{render_rows, rendered_text};
    use crate::session_info::testing::test_session_info;
    use crate::slash::registry::testing::run_execute;

    mod execute {
        use super::*;

        #[test]
        fn opens_the_overview_modal() {
            let outcome = run_execute(&StatusCmd, "");
            outcome.unwrap().into_modal();
        }
    }

    mod build_modal {
        use super::*;

        #[test]
        fn lists_each_session_descriptor() {
            let modal = build_modal(&test_session_info());

            insta::assert_debug_snapshot!(render_rows(&modal, 80, modal.height(80, u16::MAX)));
        }

        #[test]
        fn lists_session_rules_under_their_own_heading() {
            let mut info = test_session_info();
            info.session_allow = [
                (PermissionClass::Exec, "cargo test *"),
                (PermissionClass::Edit, "./out/**"),
            ]
            .map(|(class, pattern)| Rule::parse(class, pattern).unwrap())
            .to_vec();

            let rendered = rendered_text(&build_modal(&info), 80);

            let rules: Vec<Vec<&str>> = rendered
                .lines()
                .skip_while(|line| !line.contains("Session Rules"))
                .skip(1)
                .map(|line| line.split_whitespace().collect::<Vec<_>>())
                .filter(|words| !words.is_empty())
                .collect();
            assert_eq!(
                rules,
                [
                    vec!["allow.exec", "cargo", "test", "*"],
                    vec!["allow.edit", "./out/**"],
                    vec!["Esc", "to", "close"],
                ],
                "{rendered}"
            );
        }

        #[test]
        fn lists_background_shells_with_their_state() {
            let mut info = test_session_info();
            info.background_shells = shells_in_each_state();

            let rendered = rendered_text(&build_modal(&info), 80);

            let shells: Vec<&str> = rendered
                .lines()
                .skip_while(|line| !line.contains("Background Shells"))
                .filter(|line| line.contains("shell-"))
                .map(str::trim)
                .collect();
            assert_eq!(
                shells,
                [
                    "shell-1            running              cargo watch",
                    "shell-2            exit code 2          make test",
                    "shell-3            killed by signal 15  sleep 60",
                ],
                "{rendered}",
            );
        }

        #[test]
        fn shows_the_auto_compaction_pause_and_how_to_resume() {
            let mut info = test_session_info();
            info.auto_compaction_paused_after = Some(3);

            let rendered = rendered_text(&build_modal(&info), 80);

            let row = rendered
                .lines()
                .find(|line| line.contains("Auto Compaction"));
            assert_eq!(
                row.map(str::trim),
                Some(
                    "Auto Compaction    paused after 3 failures in a row (run `/compact` to resume)"
                ),
            );
        }
    }
}
