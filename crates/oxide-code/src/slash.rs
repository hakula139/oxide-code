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

mod context;
mod diff;
mod help;
mod parser;
mod registry;

pub(crate) use context::SlashContext;
pub(crate) use parser::{Parsed, parse_slash};
pub(crate) use registry::lookup;

/// Resolves and runs a parsed slash command. Renders an `ErrorBlock`
/// on unknown name. Successful commands write their own output via
/// `ctx`.
pub(crate) fn dispatch(parsed: &Parsed, ctx: &mut SlashContext<'_>) {
    let Some(cmd) = lookup(&parsed.name) else {
        ctx.chat
            .push_error(&format!("unknown command: /{}. try /help.", parsed.name));
        return;
    };
    cmd.execute(&parsed.args, ctx);
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
        let parsed = Parsed {
            name: "no-such-command".to_owned(),
            args: String::new(),
        };
        dispatch(&parsed, &mut SlashContext::new(&mut chat));
        assert!(
            chat.last_is_error(),
            "unknown command should land as an ErrorBlock",
        );
    }

    #[test]
    fn dispatch_known_command_runs_and_does_not_push_error() {
        let mut chat = fresh_chat();
        let parsed = Parsed {
            name: "help".to_owned(),
            args: String::new(),
        };
        dispatch(&parsed, &mut SlashContext::new(&mut chat));
        // /help pushes a SystemMessageBlock, not an ErrorBlock.
        assert!(!chat.last_is_error());
        assert_eq!(chat.entry_count(), 1);
    }
}
