//! `/clear` forwards [`Op::Clear`](ox_core::protocol::Op::Clear). The engine rolls the session.

use super::registry::{SlashCommand, SlashOutcome, typed_form};
use crate::session_info::LiveSessionInfo;

// ── ClearCmd ──

pub(super) struct ClearCmd;

impl SlashCommand for ClearCmd {
    fn name(&self) -> &'static str {
        "clear"
    }

    fn description(&self) -> &'static str {
        "Start a new session (this one stays resumable)"
    }

    fn idle_only(&self) -> bool {
        true
    }

    fn execute(&self, args: &str, info: &LiveSessionInfo) -> Result<SlashOutcome, String> {
        typed_form(self.name(), args, info)
    }
}

#[cfg(test)]
mod tests {
    use ox_core::protocol::Op;

    use super::*;
    use crate::action::Action;
    use crate::slash::registry::testing::run_execute;

    mod execute {
        use super::*;

        #[test]
        fn forwards_the_clear_action() {
            let outcome = run_execute(&ClearCmd, "").expect("/clear must succeed");
            assert_eq!(outcome.into_forward(), Action::Op(Op::Clear));
        }
    }
}
