//! The input to [`SlashCommand::execute`](super::registry::SlashCommand::execute), borrowing
//! App-owned state.

use crate::session_info::LiveSessionInfo;

/// Read-only App state for one [`super::registry::SlashCommand::execute`] call. Commands report
/// their effects through the returned [`super::registry::SlashOutcome`].
pub(crate) struct SlashContext<'a> {
    pub(crate) info: &'a LiveSessionInfo,
}

impl<'a> SlashContext<'a> {
    pub(crate) fn new(info: &'a LiveSessionInfo) -> Self {
        Self { info }
    }
}
