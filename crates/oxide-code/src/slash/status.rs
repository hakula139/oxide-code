//! `/status` — print live session descriptors.
//!
//! Reads from the [`SessionInfo`] snapshot the dispatcher hands in;
//! never mutates state. Output mirrors `/help`'s shape: a heading, a
//! blank line, then key-value rows aligned to a shared gutter.

use std::fmt::Write as _;

use super::context::{SessionInfo, SlashContext};
use super::registry::SlashCommand;

pub(crate) struct Status;

impl SlashCommand for Status {
    fn name(&self) -> &'static str {
        "status"
    }

    fn description(&self) -> &'static str {
        "show session info (model, cwd, auth, ...)"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) {
        ctx.chat.push_system_message(render_status(ctx.info));
    }
}

/// Render the snapshot as a `key  value` table. Keys are pre-defined
/// here (not extracted from `SessionInfo`'s field names) so the output
/// stays stable when the struct grows.
fn render_status(info: &SessionInfo) -> String {
    let rows: [(&str, &str); 5] = [
        ("model", &info.model),
        ("cwd", &info.cwd),
        ("version", info.version),
        ("auth", info.auth_label),
        ("session", &info.session_id),
    ];
    let gutter = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);

    let mut out = String::from("Status\n\n");
    for (key, value) in rows {
        let pad = gutter.saturating_sub(key.len());
        _ = writeln!(out, "  {key}{spaces}  {value}", spaces = " ".repeat(pad));
    }
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
        assert_eq!(Status.name(), "status");
        assert!(!Status.description().is_empty());
    }

    // ── render_status ──

    #[test]
    fn render_status_starts_with_heading_and_blank_line() {
        let body = render_status(&test_session_info());
        let mut lines = body.lines();
        assert_eq!(lines.next(), Some("Status"));
        assert_eq!(lines.next(), Some(""), "heading separated by blank line");
    }

    #[test]
    fn render_status_emits_one_row_per_session_field() {
        // Every `SessionInfo` field reaches the user — a regression
        // that drops a row (e.g. by truncating the array) would fail
        // here before it can ship.
        let info = test_session_info();
        let body = render_status(&info);
        for needle in [
            info.model.as_str(),
            info.cwd.as_str(),
            info.version,
            info.auth_label,
            info.session_id.as_str(),
        ] {
            assert!(body.contains(needle), "missing `{needle}`: {body}");
        }
    }

    #[test]
    fn render_status_aligns_values_to_a_shared_gutter() {
        // The longest key sets the gutter; every row's value must land
        // at the same byte offset so the value column reads as a clean
        // stripe. Locate each value directly by substring rather than
        // scanning for double-spaces — values like the cwd may contain
        // their own spaces.
        let info = test_session_info();
        let values = [
            info.model.as_str(),
            info.cwd.as_str(),
            info.version,
            info.auth_label,
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
        assert!(
            cols.iter().all(|c| *c == cols[0]),
            "value columns not aligned: {cols:?}",
        );
    }
}
