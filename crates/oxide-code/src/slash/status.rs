//! `/status` — open a read-only [`KvOverview`] of the live session: model, effort, cwd,
//! session id, auth source, version, runtime knobs.

use super::context::{LiveSessionInfo, SlashContext};
use super::registry::{SlashCommand, SlashOutcome};
use crate::config::{display_auth, display_auto_compaction, display_bool, display_effort};
use crate::tui::modal::kv_overview::{KvOverview, KvSection};

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

fn build_modal(info: &LiveSessionInfo) -> KvOverview {
    let model = &info.config.model;
    let model = format!("{} ({})", model.display_name(), model.id());
    let auto_compaction = match info.auto_compaction_paused_after {
        Some(failures) => {
            format!("paused after {failures} failures in a row (run /compact to resume)")
        }
        None => display_auto_compaction(info.config.compaction.auto),
    };
    let rows = vec![
        ("Model".to_owned(), model),
        ("Effort".to_owned(), display_effort(info.config.effort)),
        ("Working Directory".to_owned(), info.cwd.clone()),
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
            .map(|rule| ("Allow".to_owned(), rule.to_string()))
            .collect();
        sections.push(KvSection::new(rules).with_heading("Session Rules"));
    }
    KvOverview::new("Status", sections)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::rule::Rule;
    use crate::slash::test_session_info;
    use crate::tui::modal::Modal;
    use crate::tui::modal::testing::render_rows;

    fn render_modal(modal: &KvOverview, width: u16) -> String {
        let height = modal.height(width, u16::MAX);
        format!("{}\n", render_rows(modal, width, height).join("\n"))
    }

    // ── StatusCmd::name ──

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(StatusCmd.name(), "status");
        assert_eq!(StatusCmd.aliases(), [] as [&str; 0]);
        assert_ne!(StatusCmd.description(), "");
    }

    // ── StatusCmd::execute ──

    #[test]
    fn execute_opens_the_overview_modal() {
        let info = test_session_info();
        let outcome = StatusCmd.execute("", &SlashContext::new(&info, None));
        outcome.unwrap().into_modal();
    }

    // ── build_modal ──

    #[test]
    fn build_modal_renders_one_row_per_session_descriptor() {
        let info = test_session_info();
        let m = build_modal(&info);
        // Title + blank + 10 rows + blank + footer = 14.
        assert_eq!(m.height(80, u16::MAX), 14);
    }

    #[test]
    fn build_modal_lists_session_rules_under_their_own_heading() {
        let mut info = test_session_info();
        let without = render_modal(&build_modal(&info), 80);
        assert!(!without.contains("Session Rules"), "{without}");

        info.session_allow = ["bash(cargo test:*)", "edit(out/**)"]
            .map(|text| Rule::parse(text).unwrap())
            .to_vec();
        let rendered = render_modal(&build_modal(&info), 80);
        let rules: Vec<&str> = rendered
            .lines()
            .skip_while(|line| !line.contains("Session Rules"))
            .filter(|line| line.contains("Allow"))
            .map(str::trim)
            .collect();
        assert_eq!(rules.len(), 2, "{rendered}");
        assert!(rules[0].ends_with("bash(cargo test:*)"), "{rendered}");
        assert!(rules[1].ends_with("edit(out/**)"), "{rendered}");
    }

    #[test]
    fn build_modal_renders_the_auto_compaction_threshold_until_paused() {
        let mut info = test_session_info();
        let auto_compaction_row = |info: &LiveSessionInfo| {
            let rendered = render_modal(&build_modal(info), 80);
            rendered
                .lines()
                .find(|line| line.contains("Auto Compaction"))
                .map(|line| line.trim().to_owned())
        };

        let threshold = auto_compaction_row(&info);
        info.auto_compaction_paused_after = Some(3);
        let paused = auto_compaction_row(&info);

        assert_eq!(
            threshold.as_deref(),
            Some("Auto Compaction    at 155000 tokens"),
        );
        assert_eq!(
            paused.as_deref(),
            Some("Auto Compaction    paused after 3 failures in a row (run /compact to resume)"),
        );
    }
}
