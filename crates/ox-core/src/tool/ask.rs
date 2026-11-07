//! Questions answered by the user while the agent's turn remains active.

use std::pin::Pin;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::{Prepared, Tool, ToolCtx, ToolDefinition, ToolOutput, ToolSpec};
use crate::permission::{Access, PermissionClass};

/// A question and the choices the frontend can offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub question: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// Zero-based index of the recommended option.
    pub recommended: Option<usize>,
}

/// One suggested answer. Freeform answers remain available beside these choices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

/// The answer to a question, including deliberate skips and dismissed prompts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "answer", rename_all = "snake_case")]
pub enum QuestionAnswer {
    Answered(String),
    Skipped,
    Aborted,
}

pub(crate) struct QuestionRequest {
    pub(crate) id: String,
    pub(crate) question: Question,
    pub(crate) reply: oneshot::Sender<QuestionAnswer>,
}

pub(crate) struct AskTool;

impl Tool for AskTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            definition: ToolDefinition {
                name: "ask".to_owned(),
                description: "Ask the user a question and continue this turn with their answer. \
                              Offer choices when useful, and identify a recommended choice by its \
                              zero-based index. The user may answer freely, skip, or dismiss it."
                    .to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "question": {"type": "string"},
                        "options": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {"type": "string"},
                                    "description": {"type": "string"}
                                },
                                "required": ["label"],
                                "additionalProperties": false
                            }
                        },
                        "recommended": {"type": "integer", "minimum": 0}
                    },
                    "required": ["question"],
                    "additionalProperties": false
                }),
            },
            class: PermissionClass::Read,
            parallel_safe: false,
        }
    }

    fn prepare(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<Prepared, String> {
        Prepared::parse(input, |question: &Question| {
            if question.question.trim().is_empty() {
                return Err("The question is empty. Provide a question for the user.".to_owned());
            }
            if question
                .options
                .iter()
                .any(|option| option.label.trim().is_empty())
            {
                return Err("An option has an empty label. Give every option a label.".to_owned());
            }
            if question
                .recommended
                .is_some_and(|index| index >= question.options.len())
            {
                return Err("The recommended index does not name an option.".to_owned());
            }
            Ok(Access::None)
        })
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            let request = QuestionRequest {
                id: ctx.tool_use_id.clone(),
                question: prepared.into_input(),
                reply,
            };
            let answer = if ctx.questions.send(request).await.is_ok() {
                answer.await.unwrap_or(QuestionAnswer::Aborted)
            } else {
                QuestionAnswer::Aborted
            };
            ToolOutput::success(serde_json::json!({"source": "user", "result": answer}).to_string())
        })
    }
}
