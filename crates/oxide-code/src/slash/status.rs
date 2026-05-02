//! `/status` — print live session descriptors.
//!
//! Reads from the [`SessionInfo`] snapshot the dispatcher hands in;
//! never mutates state. Output mirrors `/help`'s shape: a heading, a
//! blank line, then key-value rows aligned to a shared gutter.

use super::context::{SessionInfo, SlashContext};
use super::format::write_kv_section;
use super::registry::{SlashCommand, SlashOutcome};

pub(crate) struct StatusCmd;

impl SlashCommand for StatusCmd {
    fn name(&self) -> &'static str {
        "status"
    }

    fn description(&self) -> &'static str {
        "Show session info: model, version, working directory, auth source, and session ID"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        ctx.chat.push_system_message(render_status(ctx.info));
        Ok(SlashOutcome::Local)
    }
}

/// `key  value` table. Keys live here (not derived from struct field
/// names) so the rendered labels stay stable when the struct grows.
/// `Model ID` sits next to `Model` so a routing-debug glance shows both.
fn render_status(info: &SessionInfo) -> String {
    let rows: [(&str, &str); 6] = [
        ("Model", &info.model),
        ("Model ID", &info.config.model_id),
        ("Working Directory", &info.cwd),
        ("Version", info.version),
        ("Auth", info.config.auth_label),
        ("Session ID", &info.session_id),
    ];
    let mut out = String::new();
    write_kv_section(&mut out, "Session Status", rows);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;

    // ── Status trait ──

    #[test]
    fn status_metadata_exposes_canonical_name_and_description() {
        // Pin canonical name + non-empty description.
        assert_eq!(StatusCmd.name(), "status");
        assert!(!StatusCmd.description().is_empty());
    }

    #[test]
    fn status_execute_pushes_a_non_error_block() {
        // Trait-method end-to-end success path.
        use crate::tui::components::chat::ChatView;
        use crate::tui::theme::Theme;

        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        StatusCmd.execute("", &mut ctx).unwrap();
        assert_eq!(chat.entry_count(), 1);
        assert!(!chat.last_is_error());
    }

    // ── render_status ──

    #[test]
    fn render_status_starts_with_heading_and_blank_line() {
        let body = render_status(&test_session_info());
        let mut lines = body.lines();
        assert_eq!(lines.next(), Some("Session Status"));
        assert_eq!(lines.next(), Some(""), "heading separated by blank line");
    }

    #[test]
    fn render_status_emits_one_row_per_session_field() {
        // Pin every field reaches the user, plus the row count — a
        // dropped row mustn't slip past the per-value checks.
        let info = test_session_info();
        let body = render_status(&info);
        for needle in [
            info.model.as_str(),
            info.config.model_id.as_str(),
            info.cwd.as_str(),
            info.version,
            info.config.auth_label,
            info.session_id.as_str(),
        ] {
            assert!(body.contains(needle), "missing `{needle}`: {body}");
        }
        let row_count = body.lines().skip(2).filter(|l| !l.is_empty()).count();
        assert_eq!(row_count, 6, "expected 6 rendered rows: {body}");
    }

    #[test]
    fn render_status_aligns_values_to_a_shared_gutter() {
        // Pin the absolute column, not just "all rows agree" — a
        // uniformly broken renderer would pass the latter.
        let info = test_session_info();
        let values = [
            info.model.as_str(),
            info.config.model_id.as_str(),
            info.cwd.as_str(),
            info.version,
            info.config.auth_label,
            info.session_id.as_str(),
        ];
        let body = render_status(&info);
        let cols: Vec<usize> = body
            .lines()
            .skip(2)
            .filter(|l| !l.is_empty())
            .zip(values)
            .map(|(line, value)| line.find(value).expect("value missing from row"))
            .collect();
        // Longest label is "Working Directory" (17) ⇒ prefix(2) + 17 + gap(2) = 21.
        assert!(
            cols.iter().all(|c| *c == 21),
            "value columns not aligned at col 21: {cols:?}",
        );
    }
}
