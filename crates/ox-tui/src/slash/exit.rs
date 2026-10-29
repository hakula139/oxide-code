//! `/exit` forwards [`Op::Shutdown`](ox_core::protocol::Op::Shutdown), which quits `ox` as Ctrl+D
//! on an empty idle input does.

use super::registry::{SlashCommand, SlashOutcome, typed_form};
use crate::session_info::LiveSessionInfo;

// ── ExitCmd ──

pub(super) struct ExitCmd;

impl SlashCommand for ExitCmd {
    fn name(&self) -> &'static str {
        "exit"
    }

    fn description(&self) -> &'static str {
        "Quit oxide-code"
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
        fn forwards_the_shutdown() {
            let outcome = run_execute(&ExitCmd, "").expect("/exit must succeed");

            assert_eq!(outcome.into_forward(), Action::Op(Op::Shutdown));
        }
    }
}
