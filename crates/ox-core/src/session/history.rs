//! Display projection of a loaded transcript. JSONL batches all `ToolUse` blocks then all
//! `ToolResult` blocks per turn. Live rendering pairs each call with its result inline.
//! [`interactions`] reorders into the live shape, merges adjacent text blocks, and resolves each
//! tool call's presentation, so a frontend renders the result without a tool registry.

use std::collections::HashMap;
use std::path::Path;

use super::entry::CompactInfo;
use super::post_compact::strip_synthetic_post_compact_prefix;
use crate::message::{ContentBlock, INTERRUPTION_MARKER, Message, Role, progress_update_before};
use crate::prompt::is_system_reminder;
use crate::protocol::{Interaction, ToolCall, ToolResult};
use crate::tool::view::ToolResultView;
use crate::tool::{ToolMetadata, ToolRegistry};

/// Projects a resumed transcript for display, opening with its compaction boundary when `compact`
/// is set. `tool_metadata` holds the per-call metadata the session persisted, keyed by tool-use ID,
/// and path arguments show relative to `cwd`.
pub(crate) fn interactions(
    messages: &[Message],
    compact: Option<&CompactInfo>,
    tool_metadata: &HashMap<String, ToolMetadata>,
    tools: &ToolRegistry,
    cwd: &Path,
) -> Vec<Interaction> {
    let Some(info) = compact else {
        return walk_transcript(messages, tool_metadata, tools, cwd);
    };
    let mut out = vec![Interaction::Compacted {
        summary: info.summary.clone(),
        pre_count: info.pre_message_count,
        instructions: info.instructions.clone(),
    }];
    out.extend(walk_transcript(
        &post_compact_messages(messages),
        tool_metadata,
        tools,
        cwd,
    ));
    out
}

/// Drops the summary a compaction prepended to the first message, which the boundary shows.
fn post_compact_messages(messages: &[Message]) -> Vec<Message> {
    let mut messages = messages.to_vec();
    if let Some(first) = messages.first_mut()
        && strip_synthetic_post_compact_prefix(first)
        && first.content.is_empty()
    {
        messages.remove(0);
    }
    messages
}

/// Walks a transcript in display order. Tool calls are paired inline with their `tool_use_id`
/// results, and an interruption marker becomes [`Interaction::Interrupted`]. Whitespace-only text
/// and the `<system-reminder>` blocks in user messages are dropped.
fn walk_transcript(
    messages: &[Message],
    tool_metadata: &HashMap<String, ToolMetadata>,
    tools: &ToolRegistry,
    cwd: &Path,
) -> Vec<Interaction> {
    let pairs: HashMap<&str, (&str, bool)> = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.as_str(), (content.as_str(), *is_error))),
            _ => None,
        })
        .collect();

    let mut walker = TranscriptWalker {
        tools,
        cwd,
        tool_metadata,
        pairs,
        out: Vec::new(),
    };
    for message in messages {
        walker.push_message(message);
    }
    walker.out
}

struct TranscriptWalker<'a> {
    tools: &'a ToolRegistry,
    cwd: &'a Path,
    tool_metadata: &'a HashMap<String, ToolMetadata>,
    pairs: HashMap<&'a str, (&'a str, bool)>,
    out: Vec<Interaction>,
}

impl TranscriptWalker<'_> {
    fn push_message(&mut self, msg: &Message) {
        let mut text_buf = String::new();
        for (index, block) in msg.content.iter().enumerate() {
            match block {
                ContentBlock::Text { text }
                    if msg.role == Role::User && is_system_reminder(text) => {}
                ContentBlock::Text { text }
                    if msg.role == Role::User && text == INTERRUPTION_MARKER =>
                {
                    self.flush_text(&mut text_buf, msg.role);
                    self.out.push(Interaction::Interrupted);
                }
                ContentBlock::Text { text } if !text.trim().is_empty() => {
                    if !text_buf.is_empty() {
                        text_buf.push('\n');
                    }
                    text_buf.push_str(text);
                }
                ContentBlock::ToolUse { id, name, input }
                | ContentBlock::ServerToolUse { id, name, input } => {
                    self.flush_text(&mut text_buf, msg.role);
                    if let Some(text) = progress_update_before(&msg.content, index) {
                        self.out.push(Interaction::ProgressUpdate(text.to_owned()));
                    }
                    self.out.push(Interaction::ToolCall(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        icon: self.tools.icon(name).to_owned(),
                        label: self.tools.label(name, input, self.cwd),
                    }));
                    if let Some((content, is_error)) = self.pairs.remove(id.as_str()) {
                        let call = Some((name.as_str(), input));
                        self.push_tool_result(id, call, content, is_error);
                    }
                }
                // Paired results were already emitted inline, so only orphans reach here.
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if self.pairs.remove(tool_use_id.as_str()).is_some() => {
                    self.flush_text(&mut text_buf, msg.role);
                    self.push_tool_result(tool_use_id, None, content, *is_error);
                }
                ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                    self.flush_text(&mut text_buf, msg.role);
                    self.out
                        .push(Interaction::AssistantThinking(thinking.clone()));
                }
                _ => {}
            }
        }
        self.flush_text(&mut text_buf, msg.role);
    }

    /// Resolves a result's view through the tool its `call` names, or as plain text when the
    /// transcript lost the call.
    fn push_tool_result(
        &mut self,
        id: &str,
        call: Option<(&str, &serde_json::Value)>,
        content: &str,
        is_error: bool,
    ) {
        let default = ToolMetadata::default();
        let metadata = self.tool_metadata.get(id).unwrap_or(&default);
        let view = match call {
            Some((name, input)) => self
                .tools
                .result_view(name, input, content, metadata, is_error, self.cwd),
            None => ToolResultView::Text {
                content: content.to_owned(),
            },
        };
        self.out.push(Interaction::ToolResult(ToolResult {
            id: id.to_owned(),
            content: content.to_owned(),
            is_error,
            title: metadata.title.clone(),
            view,
        }));
    }

    /// Flushes `text_buf` as a role-tagged text interaction.
    fn flush_text(&mut self, text_buf: &mut String, role: Role) {
        if text_buf.is_empty() {
            return;
        }
        let text = std::mem::take(text_buf);
        self.out.push(match role {
            Role::User => Interaction::UserText(text),
            Role::Assistant => Interaction::AssistantText(text),
        });
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::message::Replay;
    use crate::protocol::testing;
    use crate::session::post_compact::synthesize_post_compact_message;
    use crate::tool::DEFAULT_TOOL_ICON;
    use crate::tool::edit::EditTool;
    use crate::tool::tasks::testing::plan;
    use crate::tool::testing::bash_registry;
    use crate::tool::view::{DiffChunk, DiffLine, EditDiff};

    fn tools() -> ToolRegistry {
        bash_registry(vec![Box::new(EditTool)])
    }

    mod interactions {
        use super::*;

        fn compact_info() -> CompactInfo {
            CompactInfo {
                summary: "Kept the important decisions.".to_owned(),
                pre_message_count: 4,
                instructions: Some("focus on auth".to_owned()),
                tasks: plan(),
            }
        }

        fn compacted() -> Interaction {
            Interaction::Compacted {
                summary: "Kept the important decisions.".to_owned(),
                pre_count: 4,
                instructions: Some("focus on auth".to_owned()),
            }
        }

        #[test]
        fn opens_with_the_compaction_boundary_and_hides_the_synthetic_root() {
            let messages = [
                synthesize_post_compact_message("internal summary", &plan(), &[]),
                Message::assistant("post-compact reply"),
            ];

            let out = interactions(
                &messages,
                Some(&compact_info()),
                &HashMap::new(),
                &tools(),
                Path::new("/repo"),
            );

            assert_eq!(
                out,
                [
                    compacted(),
                    Interaction::AssistantText("post-compact reply".to_owned()),
                ]
            );
        }

        #[test]
        fn keeps_a_real_prompt_merged_into_the_compaction_root() {
            let mut merged = synthesize_post_compact_message("internal", &[], &[]);
            merged.content.push(ContentBlock::Text {
                text: "real follow-up".to_owned(),
            });

            let out = interactions(
                &[merged, Message::assistant("post-compact reply")],
                Some(&compact_info()),
                &HashMap::new(),
                &tools(),
                Path::new("/repo"),
            );

            assert_eq!(
                out,
                [
                    compacted(),
                    Interaction::UserText("real follow-up".to_owned()),
                    Interaction::AssistantText("post-compact reply".to_owned()),
                ]
            );
        }

        #[test]
        fn without_compaction_keeps_the_first_message_whole() {
            let root = synthesize_post_compact_message("internal summary", &[], &[]);

            let out = interactions(&[root], None, &HashMap::new(), &tools(), Path::new("/repo"));

            assert!(
                matches!(
                    &out[..],
                    [Interaction::UserText(text)] if text.contains("internal summary")
                ),
                "{out:?}",
            );
        }
    }

    mod walk_transcript {
        use super::*;

        const MODEL: &str = "claude-opus-5-5";

        fn walk(messages: &[Message]) -> Vec<Interaction> {
            walk_transcript(messages, &HashMap::new(), &tools(), Path::new("/repo"))
        }

        fn tool_use(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
            ContentBlock::ToolUse {
                id: id.to_owned(),
                name: name.to_owned(),
                input,
            }
        }

        fn tool_result_block(id: &str, content: &str, is_error: bool) -> ContentBlock {
            ContentBlock::ToolResult {
                tool_use_id: id.to_owned(),
                content: content.to_owned(),
                is_error,
            }
        }

        fn call(
            id: &str,
            name: &str,
            input: serde_json::Value,
            icon: &str,
            label: &str,
        ) -> Interaction {
            Interaction::ToolCall(ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                input,
                icon: icon.to_owned(),
                label: label.to_owned(),
            })
        }

        fn bash_call(id: &str, command: &str) -> Interaction {
            call(
                id,
                "bash",
                serde_json::json!({"command": command}),
                "$",
                command,
            )
        }

        fn text_result(id: &str, content: &str, is_error: bool) -> Interaction {
            Interaction::ToolResult(testing::text_result(id, content, is_error, None))
        }

        #[test]
        fn emits_user_and_assistant_text() {
            let out = walk(&[Message::user("hi"), Message::assistant("hello")]);
            assert_eq!(
                out,
                [
                    Interaction::UserText("hi".to_owned()),
                    Interaction::AssistantText("hello".to_owned()),
                ]
            );
        }

        #[test]
        fn joins_adjacent_text_blocks_with_newline() {
            let messages = [Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "first".to_owned(),
                    },
                    ContentBlock::Text {
                        text: "second".to_owned(),
                    },
                ],
            }];
            assert_eq!(
                walk(&messages),
                [Interaction::AssistantText("first\nsecond".to_owned())]
            );
        }

        #[test]
        fn drops_whitespace_only_text() {
            let messages = [Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "  \n  ".to_owned(),
                }],
            }];
            assert_eq!(walk(&messages), Vec::<Interaction>::new());
        }

        #[test]
        fn drops_context_reminders_from_user_prompts() {
            let reminder = indoc! {"
                <system-reminder>
                Today's date is 2026-04-13.
                </system-reminder>"
            };
            let messages = [
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::Text {
                            text: "fix it".to_owned(),
                        },
                        ContentBlock::Text {
                            text: reminder.to_owned(),
                        },
                    ],
                },
                Message::assistant(reminder),
            ];
            assert_eq!(
                walk(&messages),
                [
                    Interaction::UserText("fix it".to_owned()),
                    Interaction::AssistantText(reminder.to_owned()),
                ]
            );
        }

        #[test]
        fn marks_each_interruption_in_prompts_merged_on_resume() {
            let text = |text: &str| ContentBlock::Text {
                text: text.to_owned(),
            };
            let messages = [
                Message {
                    role: Role::User,
                    content: vec![
                        text("write a story"),
                        text(INTERRUPTION_MARKER),
                        text("run ls"),
                    ],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![tool_use("t1", "bash", serde_json::json!({"command": "ls"}))],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        tool_result_block("t1", "interrupted", true),
                        text(INTERRUPTION_MARKER),
                        text("what is 7 times 6?"),
                    ],
                },
                Message::assistant("42"),
            ];

            let out = walk(&messages);

            assert_eq!(
                out,
                [
                    Interaction::UserText("write a story".to_owned()),
                    Interaction::Interrupted,
                    Interaction::UserText("run ls".to_owned()),
                    bash_call("t1", "ls"),
                    text_result("t1", "interrupted", true),
                    Interaction::Interrupted,
                    Interaction::UserText("what is 7 times 6?".to_owned()),
                    Interaction::AssistantText("42".to_owned()),
                ]
            );
        }

        #[test]
        fn pairs_tool_call_and_result_inline() {
            let messages = [
                Message::user("ask"),
                Message {
                    role: Role::Assistant,
                    content: vec![tool_use("t1", "bash", serde_json::json!({"command": "ls"}))],
                },
                Message {
                    role: Role::User,
                    content: vec![tool_result_block("t1", "output", false)],
                },
                Message::assistant("reply"),
            ];
            assert_eq!(
                walk(&messages),
                [
                    Interaction::UserText("ask".to_owned()),
                    bash_call("t1", "ls"),
                    text_result("t1", "output", false),
                    Interaction::AssistantText("reply".to_owned()),
                ]
            );
        }

        #[test]
        fn pairs_multiple_calls_in_order_even_across_batched_results() {
            let messages = [
                Message {
                    role: Role::Assistant,
                    content: vec![
                        tool_use("t1", "bash", serde_json::json!({"command": "ls"})),
                        tool_use("t2", "bash", serde_json::json!({"command": "pwd"})),
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        tool_result_block("t1", "file a", false),
                        tool_result_block("t2", "/repo", false),
                    ],
                },
            ];
            assert_eq!(
                walk(&messages),
                [
                    bash_call("t1", "ls"),
                    text_result("t1", "file a", false),
                    bash_call("t2", "pwd"),
                    text_result("t2", "/repo", false),
                ]
            );
        }

        #[test]
        fn resolves_a_result_through_its_tool_with_persisted_metadata() {
            let diff = EditDiff {
                chunk: DiffChunk {
                    old: vec![DiffLine {
                        number: 10,
                        text: "a".to_owned(),
                    }],
                    new: vec![DiffLine {
                        number: 10,
                        text: "b".to_owned(),
                    }],
                },
                locations: vec![10, 20],
            };
            let input = serde_json::json!({
                "file_path": "/repo/f.rs",
                "old_string": "a",
                "new_string": "b",
                "replace_all": true,
            });
            let messages = [
                Message {
                    role: Role::Assistant,
                    content: vec![tool_use("e1", "edit", input)],
                },
                Message {
                    role: Role::User,
                    content: vec![tool_result_block("e1", "Edited.", false)],
                },
            ];
            let metadata = HashMap::from([(
                "e1".to_owned(),
                ToolMetadata {
                    title: Some("Edited f.rs".to_owned()),
                    diff: Some(diff.clone()),
                    ..ToolMetadata::default()
                },
            )]);

            let out = walk_transcript(&messages, &metadata, &tools(), Path::new("/repo"));

            assert_eq!(
                out[1],
                Interaction::ToolResult(ToolResult {
                    id: "e1".to_owned(),
                    content: "Edited.".to_owned(),
                    is_error: false,
                    title: Some("Edited f.rs".to_owned()),
                    view: ToolResultView::Diff(diff),
                })
            );
        }

        #[test]
        fn preserves_orphan_tool_result_at_original_position() {
            // "ghost" has no matching call, so it stays in place and never takes a sibling's label.
            let messages = [
                Message {
                    role: Role::Assistant,
                    content: vec![tool_use("t1", "bash", serde_json::json!({"command": "ls"}))],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        tool_result_block("t1", "file a", false),
                        tool_result_block("ghost", "stale", true),
                    ],
                },
            ];
            assert_eq!(
                walk(&messages),
                [
                    bash_call("t1", "ls"),
                    text_result("t1", "file a", false),
                    text_result("ghost", "stale", true),
                ]
            );
        }

        #[test]
        fn orphan_result_in_user_only_message_keeps_its_persisted_title() {
            // Sanitizing drops such a message on resume, but the projection still renders one.
            let messages = [Message {
                role: Role::User,
                content: vec![tool_result_block("missing", "stderr", true)],
            }];
            let metadata = HashMap::from([(
                "missing".to_owned(),
                ToolMetadata {
                    title: Some("Ran build".to_owned()),
                    ..ToolMetadata::default()
                },
            )]);

            let out = walk_transcript(&messages, &metadata, &tools(), Path::new("/repo"));

            assert_eq!(
                out,
                [Interaction::ToolResult(ToolResult {
                    id: "missing".to_owned(),
                    content: "stderr".to_owned(),
                    is_error: true,
                    title: Some("Ran build".to_owned()),
                    view: ToolResultView::Text {
                        content: "stderr".to_owned(),
                    },
                })]
            );
        }

        #[test]
        fn shows_unregistered_and_server_tools_by_their_raw_name() {
            let messages = [Message {
                role: Role::Assistant,
                content: vec![
                    tool_use("c1", "custom_tool", serde_json::json!({"arg": "value"})),
                    ContentBlock::ServerToolUse {
                        id: "srv1".to_owned(),
                        name: "web_search".to_owned(),
                        input: serde_json::json!({"query": "rust"}),
                    },
                ],
            }];
            assert_eq!(
                walk(&messages),
                [
                    call(
                        "c1",
                        "custom_tool",
                        serde_json::json!({"arg": "value"}),
                        DEFAULT_TOOL_ICON,
                        "custom_tool",
                    ),
                    call(
                        "srv1",
                        "web_search",
                        serde_json::json!({"query": "rust"}),
                        DEFAULT_TOOL_ICON,
                        "web_search",
                    ),
                ]
            );
        }

        #[test]
        fn hides_server_tool_results() {
            let messages = [Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ServerToolUse {
                        id: "srv1".to_owned(),
                        name: "web_search".to_owned(),
                        input: serde_json::json!({"query": "rust"}),
                    },
                    ContentBlock::ServerToolResult {
                        tool_use_id: "srv1".to_owned(),
                        kind: "web_search_tool_result".to_owned(),
                        fields: serde_json::Map::from_iter([(
                            "content".to_owned(),
                            serde_json::json!([{"type": "web_search_result"}]),
                        )]),
                    },
                    ContentBlock::Text {
                        text: "Found it".to_owned(),
                    },
                ],
            }];
            assert_eq!(
                walk(&messages),
                [
                    call(
                        "srv1",
                        "web_search",
                        serde_json::json!({"query": "rust"}),
                        DEFAULT_TOOL_ICON,
                        "web_search",
                    ),
                    Interaction::AssistantText("Found it".to_owned()),
                ]
            );
        }

        #[test]
        fn flushes_text_before_tool_call() {
            let messages = [Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Let me check".to_owned(),
                    },
                    tool_use("t1", "bash", serde_json::json!({"command": "ls"})),
                    ContentBlock::Text {
                        text: "Done".to_owned(),
                    },
                ],
            }];
            assert_eq!(
                walk(&messages),
                [
                    Interaction::AssistantText("Let me check".to_owned()),
                    bash_call("t1", "ls"),
                    Interaction::AssistantText("Done".to_owned()),
                ]
            );
        }

        #[test]
        fn emits_thinking_and_drops_redacted() {
            let messages = [Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "reasoning".to_owned(),
                        replay: Replay::with_model(MODEL, "sig"),
                    },
                    ContentBlock::RedactedThinking {
                        replay: Replay::with_model(MODEL, "opaque"),
                    },
                    ContentBlock::Text {
                        text: "answer".to_owned(),
                    },
                ],
            }];
            assert_eq!(
                walk(&messages),
                [
                    Interaction::AssistantThinking("reasoning".to_owned()),
                    Interaction::AssistantText("answer".to_owned()),
                ]
            );
        }

        #[test]
        fn drops_whitespace_only_thinking() {
            let messages = [Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "  \n  ".to_owned(),
                        replay: Replay::with_model(MODEL, "sig"),
                    },
                    ContentBlock::Text {
                        text: "reply".to_owned(),
                    },
                ],
            }];
            assert_eq!(
                walk(&messages),
                [Interaction::AssistantText("reply".to_owned())]
            );
        }

        #[test]
        fn emits_progress_update_between_thinking_and_its_tool_call() {
            let messages = [Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "Running the suite.".to_owned(),
                        replay: Replay::with_model(MODEL, "sig"),
                    },
                    tool_use("t1", "bash", serde_json::json!({"command": "cargo test"})),
                ],
            }];
            assert_eq!(
                walk(&messages),
                [
                    Interaction::AssistantThinking("Running the suite.".to_owned()),
                    Interaction::ProgressUpdate("Running the suite.".to_owned()),
                    bash_call("t1", "cargo test"),
                ]
            );
        }
    }
}
