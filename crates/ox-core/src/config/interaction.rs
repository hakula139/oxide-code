//! User-configured deadlines for questions and permission approvals.

use std::num::NonZeroU32;

use serde::Deserialize;

/// Independent policies for the two kinds of user interaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InteractionConfig {
    pub questions: QuestionConfig,
    pub approvals: ApprovalConfig,
}

/// Questions wait indefinitely unless a deadline is configured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuestionConfig {
    pub timeout_seconds: Option<NonZeroU32>,
    pub on_timeout: QuestionTimeoutAction,
}

/// A recommended choice is used only when the question identifies one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum QuestionTimeoutAction {
    #[default]
    Recommended,
    Skip,
}

/// Approval deadlines apply to one call and never create a session grant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovalConfig {
    pub timeout_seconds: Option<NonZeroU32>,
    pub on_timeout: ApprovalTimeoutAction,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ApprovalTimeoutAction {
    #[default]
    Deny,
    Allow,
}
