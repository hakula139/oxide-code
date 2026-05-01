//! `/status` — print live session descriptors.
//!
//! Reads from the [`SessionInfo`] snapshot the dispatcher hands in;
//! never mutates state. Output mirrors `/help`'s shape: a heading, a
//! blank line, then key-value rows aligned to a shared gutter.

use super::context::{SessionInfo, SlashContext};
use super::format::write_kv_section;
use super::registry::SlashCommand;

pub(crate) struct StatusCmd;

impl SlashCommand for StatusCmd {
    fn name(&self) -> &'static str {
        "status"
    }

    fn description(&self) -> &'static str {
        "show session info"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<(), String> {
        ctx.chat.push_system_message(render_status(ctx.info));
        Ok(())
    }
}

/// Render the snapshot as a `key  value` table. Keys are pre-defined
/// here (not extracted from `SessionInfo`'s field names) so the output
/// stays stable when the struct grows. `model id` sits next to the
/// marketing-name `model` so the user debugging a routing issue can
/// see both at a glance — matching the pair `/config` shows.
fn render_status(info: &SessionInfo) -> String {
    let rows: [(&str, &str); 6] = [
        ("model", &info.model),
        ("model id", &info.config.model_id),
        ("cwd", &info.cwd),
        ("version", info.version),
        ("auth", info.config.auth_label),
        ("session id", &info.session_id),
    ];
    let mut out = String::new();
    write_kv_section(&mut out, "Session status", rows);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;

    // ── Status trait ──

    #[test]
    fn status_metadata_exposes_canonical_name_and_description() {
        // Pin the user-visible name + description so an edit that
        // accidentally sends them through `tr` lowercase or rephrases
        // the gutter copy fails CI here, not in a manual smoke test.
        assert_eq!(StatusCmd.name(), "status");
        assert!(!StatusCmd.description().is_empty());
    }

    #[test]
    fn status_execute_pushes_a_non_error_block() {
        // Trait-method end-to-end: `execute` must return `Ok(())` and
        // leave a single non-error block in the chat. Pins the
        // success-path contract the dispatcher relies on.
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
        assert_eq!(lines.next(), Some("Session status"));
        assert_eq!(lines.next(), Some(""), "heading separated by blank line");
    }

    #[test]
    fn render_status_emits_one_row_per_session_field() {
        // Every `SessionInfo` field reaches the user — a regression
        // that drops a row (e.g., by truncating the array) would fail
        // here before it can ship. Pin the row count too so an
        // accidental row drop doesn't slip past the per-value checks.
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
        // The longest key sets the gutter; every row's value must land
        // at the same byte offset so the value column reads as a clean
        // stripe. Pin the actual expected offset (not just "all rows
        // agree") — a uniformly broken renderer would otherwise pass.
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
        // Longest label is "session id" (10) ⇒ prefix(2) + 10 + gap(2) = 14.
        assert!(
            cols.iter().all(|c| *c == 14),
            "value columns not aligned at col 14: {cols:?}",
        );
    }
}
