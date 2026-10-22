//! `/login` forwards [`Op::Login`], and the engine signs in through the browser. `/logout`
//! forwards [`Op::Logout`], which removes the oxide-code login and leaves Claude Code's in place.

use ox_core::protocol::Op;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::action::Action;

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
    use crate::slash::registry::testing::run_execute;

    fn forwarded(cmd: &dyn SlashCommand) -> Action {
        run_execute(cmd, "")
            .expect("the command must succeed")
            .into_forward()
    }

    mod classify {
        use super::*;

        #[test]
        fn is_mutating() {
            assert_eq!(LoginCmd.classify(""), SlashKind::Mutating);
        }
    }

    mod execute {
        use super::*;

        #[test]
        fn forwards_the_login_action() {
            assert_eq!(forwarded(&LoginCmd), Action::Op(Op::Login));
        }
    }

    mod logout_cmd {
        use super::*;

        mod classify {
            use super::*;

            #[test]
            fn is_mutating() {
                assert_eq!(LogoutCmd.classify(""), SlashKind::Mutating);
            }
        }

        mod execute {
            use super::*;

            #[test]
            fn forwards_the_logout_action() {
                assert_eq!(forwarded(&LogoutCmd), Action::Op(Op::Logout));
            }
        }
    }
}
