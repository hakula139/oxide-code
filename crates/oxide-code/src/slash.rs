//! Slash-command surface.
//!
//! [`parse_slash`] decides whether a submitted prompt is a slash
//! command; [`dispatch`] resolves the parsed command against the
//! registry and runs it. The TUI's [`App::dispatch_user_action`] calls
//! both: an unmatched prompt continues to the agent loop, a matched
//! one stays local.
//!
//! Each command is a `SlashCommand` impl in its own submodule. Adding
//! a new command means one file plus one entry in [`registry::BUILT_INS`].
//!
//! Output: every command pushes either a `SystemMessageBlock` (info /
//! results) or an `ErrorBlock` (unknown command, bad args) to the
//! chat. No modal overlays, no toasts.
//!
//! Persistence: slash commands never write to user config files
//! (see `docs/research/design/slash-commands.md` § Design Decisions
//! 6). Mutations are session-local; restart returns to the
//! user-declared config.

mod config;
mod context;
mod diff;
mod format;
mod help;
mod parser;
mod registry;
mod status;

pub(crate) use context::{SessionInfo, SlashContext};
pub(crate) use parser::{Parsed, parse_slash};
pub(crate) use registry::lookup;

/// Resolves and runs a parsed slash command. Renders an `ErrorBlock`
/// on unknown name or on `Err` returned by the command. Commands
/// push their own informational output (`SystemMessageBlock`) before
/// returning `Ok`.
pub(crate) fn dispatch(parsed: &Parsed, ctx: &mut SlashContext<'_>) {
    let Some(cmd) = lookup(&parsed.name) else {
        ctx.chat.push_error(&format!(
            "unknown command: /{name}. Available: {available}. \
             Use //{name} to send the literal text.",
            name = parsed.name,
            available = available_commands(),
        ));
        return;
    };
    if let Err(msg) = cmd.execute(&parsed.args, ctx) {
        ctx.chat.push_error(&format!("/{}: {msg}", parsed.name));
    }
}

/// Comma-separated `/name` list for the unknown-command hint.
fn available_commands() -> String {
    let names: Vec<String> = registry::BUILT_INS
        .iter()
        .map(|c| format!("/{}", c.name()))
        .collect();
    names.join(", ")
}

/// Shared test fixture — a fully-populated `SessionInfo` so per-command
/// tests don't repeat the boilerplate. Lives at the module root so
/// every sibling `slash::*::tests` module can pull it in.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn fresh_chat() -> ChatView {
        ChatView::new(&Theme::default(), false)
    }

    // ── dispatch ──

    #[test]
    fn dispatch_unknown_command_pushes_error_block() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "no-such-command".to_owned(),
            args: String::new(),
        };
        dispatch(&parsed, &mut SlashContext::new(&mut chat, &info));
        assert!(
            chat.last_is_error(),
            "unknown command should land as an ErrorBlock",
        );
    }

    #[test]
    fn dispatch_unknown_command_message_lists_available_and_escape_hint() {
        // The error message must surface (a) what to try instead and
        // (b) the `//foo` escape so the user can send the literal text.
        // Without these, the user has no way to discover the command
        // set or to send `/etc/hosts`-style prompts.
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "etc".to_owned(),
            args: String::new(),
        };
        dispatch(&parsed, &mut SlashContext::new(&mut chat, &info));
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

    #[test]
    fn dispatch_known_command_runs_and_does_not_push_error() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "help".to_owned(),
            args: String::new(),
        };
        dispatch(&parsed, &mut SlashContext::new(&mut chat, &info));
        assert!(!chat.last_is_error());
        assert_eq!(chat.entry_count(), 1);
    }

    #[test]
    fn dispatch_command_failure_renders_error_block_prefixed_with_name() {
        // Commands return `Err(message)`; the dispatcher must wrap it
        // as `/name: message` and push an `ErrorBlock`. Pin the prefix
        // so the user always sees which command failed.
        struct Failing;
        impl registry::SlashCommand for Failing {
            fn name(&self) -> &'static str {
                "failing"
            }
            fn description(&self) -> &'static str {
                "test"
            }
            fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<(), String> {
                Err("explicit failure".to_owned())
            }
        }
        // Bypass the registry and call execute directly through the
        // wrapper-style code path. Since dispatch resolves through
        // `lookup`, exercise the same shape it uses (the ErrorBlock
        // wrapping happens in dispatch's tail). For Failing to be
        // reachable through dispatch, it'd need to be in BUILT_INS;
        // instead we mimic dispatch's tail directly.
        let mut chat = fresh_chat();
        let info = test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        let cmd: &dyn registry::SlashCommand = &Failing;
        if let Err(msg) = cmd.execute("", &mut ctx) {
            ctx.chat.push_error(&format!("/{}: {msg}", cmd.name()));
        }
        let body = chat.last_error_text().expect("error block present");
        assert_eq!(body, "/failing: explicit failure");
    }
}
