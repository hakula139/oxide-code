//! `/login` forwards [`Op::Login`], and the engine signs in through the browser. `/logout`
//! forwards [`Op::Logout`], which removes the oxide-code login and leaves Claude Code's in place.

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::protocol::Op;
use crate::tui::action::Action;

// ── LoginCmd ──

pub(super) struct LoginCmd;

impl SlashCommand for LoginCmd {
    fn name(&self) -> &'static str {
        "login"
    }

    fn description(&self) -> &'static str {
        "Sign in to a Claude account"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn execute(&self, _args: &str, _ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        Ok(SlashOutcome::Forward(Action::Op(Op::Login)))
    }
}

// ── LogoutCmd ──

pub(super) struct LogoutCmd;

impl SlashCommand for LogoutCmd {
    fn name(&self) -> &'static str {
        "logout"
    }

    fn description(&self) -> &'static str {
        "Sign out of the oxide-code login"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn execute(&self, _args: &str, _ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        Ok(SlashOutcome::Forward(Action::Op(Op::Logout)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;

    fn forwarded(cmd: &dyn SlashCommand) -> Action {
        let info = test_session_info();
        cmd.execute("", &SlashContext::new(&info, None))
            .expect("the command must succeed")
            .into_forward()
    }

    // ── LoginCmd metadata ──

    #[test]
    fn login_metadata_matches_built_ins_contract() {
        assert_eq!(LoginCmd.name(), "login");
        assert_eq!(LoginCmd.aliases(), [] as [&str; 0]);
        assert_ne!(LoginCmd.description(), "");
        assert_eq!(LoginCmd.classify(""), SlashKind::Mutating);
    }

    // ── LoginCmd::execute ──

    #[test]
    fn login_forwards_the_login_action() {
        assert_eq!(forwarded(&LoginCmd), Action::Op(Op::Login));
    }

    // ── LogoutCmd metadata ──

    #[test]
    fn logout_metadata_matches_built_ins_contract() {
        assert_eq!(LogoutCmd.name(), "logout");
        assert_eq!(LogoutCmd.aliases(), [] as [&str; 0]);
        assert_ne!(LogoutCmd.description(), "");
        assert_eq!(LogoutCmd.classify(""), SlashKind::Mutating);
    }

    // ── LogoutCmd::execute ──

    #[test]
    fn logout_forwards_the_logout_action() {
        assert_eq!(forwarded(&LogoutCmd), Action::Op(Op::Logout));
    }
}
