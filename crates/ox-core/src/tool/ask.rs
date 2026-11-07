//! Questions answered by the user while the agent's turn remains active.

use std::pin::Pin;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::{Prepared, Tool, ToolCtx, ToolDefinition, ToolOutput, ToolSpec};
use crate::permission::{Access, PermissionClass};
use crate::protocol::ReplySource;

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
    pub(crate) reply: oneshot::Sender<QuestionReply>,
}

#[derive(Debug)]
pub(crate) struct QuestionReply {
    pub(crate) answer: QuestionAnswer,
    pub(crate) source: ReplySource,
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
                answer.await.ok()
            } else {
                None
            }
            .unwrap_or(QuestionReply {
                answer: QuestionAnswer::Aborted,
                source: ReplySource::User,
            });
            ToolOutput::success(
                serde_json::json!({"source": answer.source, "result": answer.answer}).to_string(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;
    use crate::tool::local_testing::temp_ctx;

    mod prepare {
        use super::*;

        #[test]
        fn rejects_unanswerable_questions_and_invalid_recommendations() {
            for (input, expected) in [
                (
                    json!({"question": " "}),
                    "The question is empty. Provide a question for the user.",
                ),
                (
                    json!({"question": "Choose a color.", "options": [{"label": " "}]}),
                    "An option has an empty label. Give every option a label.",
                ),
                (
                    json!({"question": "Choose a color.", "recommended": 0}),
                    "The recommended index does not name an option.",
                ),
                (
                    json!({"question": "Choose a color.", "options": [{"label": "Blue"}], "recommended": 1}),
                    "The recommended index does not name an option.",
                ),
            ] {
                let error = AskTool.prepare(input.clone(), &temp_ctx()).unwrap_err();

                assert_eq!(error, expected, "{input}");
            }
        }
    }

    mod run {
        use super::*;

        #[tokio::test]
        async fn preserves_answer_outcomes_and_their_source() {
            for (answer, source) in [
                (
                    QuestionAnswer::Answered("Blue".to_owned()),
                    ReplySource::User,
                ),
                (QuestionAnswer::Answered(String::new()), ReplySource::User),
                (QuestionAnswer::Skipped, ReplySource::User),
                (QuestionAnswer::Aborted, ReplySource::User),
                (
                    QuestionAnswer::Answered("Green".to_owned()),
                    ReplySource::Timeout,
                ),
                (QuestionAnswer::Skipped, ReplySource::Timeout),
            ] {
                let (questions, mut requests) = mpsc::channel(1);
                let ctx = ToolCtx {
                    questions,
                    ..temp_ctx()
                };
                let input = json!({"question": "Choose a color.", "options": [{"label": "Blue"}], "recommended": 0});
                let prepared = AskTool.prepare(input.clone(), &ctx).unwrap();
                let expected = answer.clone();

                let (output, ()) = tokio::join!(AskTool.run(prepared, &ctx), async {
                    let request = requests.recv().await.unwrap();
                    assert_eq!(request.id, "toolu_test");
                    assert_eq!(
                        request.question,
                        serde_json::from_value::<Question>(input).unwrap()
                    );
                    request
                        .reply
                        .send(QuestionReply { answer, source })
                        .unwrap();
                });

                assert!(!output.is_error, "{}", output.content);
                let expected_source = match source {
                    ReplySource::User => "user",
                    ReplySource::Timeout => "timeout",
                };
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&output.content).unwrap(),
                    json!({"source": expected_source, "result": expected}),
                );
            }
        }

        #[tokio::test]
        async fn a_closed_question_channel_aborts_without_an_answer() {
            let ctx = temp_ctx();
            let prepared = AskTool
                .prepare(json!({"question": "Choose a color."}), &ctx)
                .unwrap();

            let output = AskTool.run(prepared, &ctx).await;

            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&output.content).unwrap(),
                json!({"source": "user", "result": {"status": "aborted"}}),
            );
        }
    }
}
