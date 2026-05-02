//! Slash-command surface.
//!
//! [`parse_slash`] decides whether a submitted prompt is a slash
//! command; [`dispatch`] resolves it against the registry and runs it.
//! Each command is a [`registry::SlashCommand`] impl in its own
//! submodule — adding one is one file plus one entry in
//! [`registry::BUILT_INS`].
//!
//! Output: every command pushes a `SystemMessageBlock` (info / results)
//! or an `ErrorBlock` (unknown command, bad args) to the chat. No modal
//! overlays.
//!
//! Persistence: commands never write user config files. Mutations are
//! session-local; restart returns to the user-declared config (see
//! `docs/research/design/slash-commands.md` § Design Decisions 6).

mod clear;
mod config;
mod context;
mod diff;
mod format;
mod help;
mod matcher;
mod parser;
mod registry;
mod status;

pub(crate) use context::{SessionInfo, SlashContext};
pub(crate) use matcher::MatchedCommand;
pub(crate) use parser::{Parsed, parse_slash, popup_query};

/// Filter the built-in registry against a popup query (the buffer
/// with the leading `/` stripped). Convenience wrapper around
/// [`matcher::filter_and_rank`] so the popup component never touches
/// `BUILT_INS` directly.
pub(crate) fn filter_built_ins(query: &str) -> Vec<MatchedCommand> {
    matcher::filter_and_rank(query, registry::BUILT_INS)
}

/// Resolves and runs a parsed slash command against the built-in
/// registry. See [`dispatch_with`].
pub(crate) fn dispatch(parsed: &Parsed, ctx: &mut SlashContext<'_>) {
    dispatch_with(registry::BUILT_INS, parsed, ctx);
}

/// Resolves `parsed` against `commands` and runs the matching impl.
/// Renders an `ErrorBlock` on unknown name or on `Err` from `execute`.
/// Successful commands push their own output before returning `Ok`.
///
/// Extracted so tests can drive the dispatcher with a synthetic registry.
fn dispatch_with(
    commands: &[&dyn registry::SlashCommand],
    parsed: &Parsed,
    ctx: &mut SlashContext<'_>,
) {
    let Some(cmd) = registry::lookup_in(commands, &parsed.name) else {
        ctx.chat.push_error(&format!(
            "unknown command: /{name}. Available: {available}. \
             Use //{name} to send the literal text.",
            name = parsed.name,
            available = format_available(commands),
        ));
        return;
    };
    if let Err(msg) = cmd.execute(&parsed.args, ctx) {
        ctx.chat.push_error(&format!("/{}: {msg}", parsed.name));
    }
}

/// Comma-separated `/name` list for the unknown-command hint.
fn format_available(commands: &[&dyn registry::SlashCommand]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for cmd in commands {
        if !out.is_empty() {
            out.push_str(", ");
        }
        _ = write!(out, "/{}", cmd.name());
    }
    out
}

/// Shared test fixture — a fully-populated `SessionInfo` for the
/// per-command test modules.
#[cfg(test)]
pub(crate) fn test_session_info() -> SessionInfo {
    use crate::config::{ConfigSnapshot, Effort, PromptCacheTtl};

    SessionInfo {
        model: "Test Model".to_owned(),
        cwd: "~/test".to_owned(),
        version: "0.0.0-test",
        session_id: "test-session".to_owned(),
        config: ConfigSnapshot {
            auth_label: "API key",
            base_url: "https://api.test.invalid".to_owned(),
            model_id: "claude-test-1-0".to_owned(),
            effort: Some(Effort::High),
            max_tokens: 32_000,
            prompt_cache_ttl: PromptCacheTtl::OneHour,
            show_thinking: false,
        },
    }
}

/// Fresh `(Sender, Receiver)` pair for slash-command test contexts.
/// `/clear`-style commands hold the receiver to assert what was sent;
/// read-only commands drop it.
#[cfg(test)]
pub(crate) fn test_user_tx() -> (
    tokio::sync::mpsc::Sender<crate::agent::event::UserAction>,
    tokio::sync::mpsc::Receiver<crate::agent::event::UserAction>,
) {
    tokio::sync::mpsc::channel(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::registry::SlashCommand;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn fresh_chat() -> ChatView {
        ChatView::new(&Theme::default(), false)
    }

    // ── dispatch ──

    #[test]
    fn dispatch_known_command_runs_and_does_not_push_error() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "help".to_owned(),
            args: String::new(),
        };
        let (user_tx, _user_rx) = test_user_tx();
        dispatch(&parsed, &mut SlashContext::new(&mut chat, &info, &user_tx));
        assert!(!chat.last_is_error());
        assert_eq!(chat.entry_count(), 1);
        // Pin: SystemMessageBlock inherits `error_text` default `None`
        // — flipping that default would let non-error blocks claim
        // error wording.
        assert_eq!(chat.last_error_text(), None);
    }

    #[test]
    fn dispatch_unknown_command_pushes_error_block() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "no-such-command".to_owned(),
            args: String::new(),
        };
        let (user_tx, _user_rx) = test_user_tx();
        dispatch(&parsed, &mut SlashContext::new(&mut chat, &info, &user_tx));
        assert!(
            chat.last_is_error(),
            "unknown command should land as an ErrorBlock",
        );
    }

    #[test]
    fn dispatch_unknown_command_message_lists_available_and_escape_hint() {
        // The error must surface alternatives (commands) and the
        // `//foo` escape — those are the user's two recovery paths.
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "etc".to_owned(),
            args: String::new(),
        };
        let (user_tx, _user_rx) = test_user_tx();
        dispatch(&parsed, &mut SlashContext::new(&mut chat, &info, &user_tx));
        let msg = chat.last_error_text().expect("error block present");
        assert!(msg.contains("/help"), "should list /help: {msg}");
        for cmd in registry::BUILT_INS {
            let needle = format!("/{}", cmd.name());
            assert!(msg.contains(&needle), "should list `{needle}`: {msg}");
        }
        assert!(
            msg.contains("//etc"),
            "should mention `//etc` escape: {msg}"
        );
    }

    // ── dispatch_with ──

    /// Synthetic command with two aliases that always errors — drives
    /// the dispatcher's alias-resolution and error-wrapping branches
    /// against a test-controlled registry.
    struct Failing;
    impl registry::SlashCommand for Failing {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn aliases(&self) -> &'static [&'static str] {
            &["bust", "boom"]
        }
        fn description(&self) -> &'static str {
            "test"
        }
        fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<(), String> {
            Err("explicit failure".to_owned())
        }
    }

    #[test]
    fn failing_fixture_metadata_matches_what_dispatcher_tests_assume() {
        // Pin so a fixture drift (one alias missing, no Err) fails
        // here rather than silently misleading the dispatcher tests.
        assert_eq!(Failing.name(), "failing");
        assert_eq!(Failing.aliases(), &["bust", "boom"]);
        assert_eq!(Failing.description(), "test");
    }

    #[test]
    fn dispatch_with_command_failure_renders_error_block_prefixed_with_name() {
        // `Err` from `execute` must land as `/name: msg`. Driven through
        // the real `dispatch_with`, not a reimplementation of its tail.
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "failing".to_owned(),
            args: String::new(),
        };
        let registry: &[&dyn registry::SlashCommand] = &[&Failing];
        let (user_tx, _user_rx) = test_user_tx();
        dispatch_with(
            registry,
            &parsed,
            &mut SlashContext::new(&mut chat, &info, &user_tx),
        );
        assert_eq!(chat.last_error_text(), Some("/failing: explicit failure"),);
    }

    #[test]
    fn dispatch_with_alias_routes_to_canonical_impl() {
        // Alias must run the canonical impl — and the error wrapping
        // must echo the typed name back (the alias), not the canonical.
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "boom".to_owned(),
            args: String::new(),
        };
        let registry: &[&dyn registry::SlashCommand] = &[&Failing];
        let (user_tx, _user_rx) = test_user_tx();
        dispatch_with(
            registry,
            &parsed,
            &mut SlashContext::new(&mut chat, &info, &user_tx),
        );
        assert_eq!(chat.last_error_text(), Some("/boom: explicit failure"));
    }
}
