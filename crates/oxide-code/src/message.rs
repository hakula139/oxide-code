use serde::{Deserialize, Serialize};

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A content block within a message.
///
/// User messages typically contain `Text` or `ToolResult` blocks.
/// Assistant messages typically contain `Text` or `ToolUse` blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_default")]
        is_error: bool,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Opaque safety-redacted thinking block. Must be preserved verbatim for
    /// round-tripping — the API validates its contents.
    RedactedThinking {
        data: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "called only in tests for now"))]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

// ── Message normalization ──

/// Strip trailing thinking / `redacted_thinking` blocks from assistant messages.
/// The API rejects assistant messages that end with thinking blocks.
pub fn strip_trailing_thinking(messages: &mut [Message]) {
    for msg in messages.iter_mut().filter(|m| m.role == Role::Assistant) {
        while msg.content.last().is_some_and(|b| {
            matches!(
                b,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        }) {
            msg.content.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ContentBlock::ToolResult ──

    #[test]
    fn tool_result_serializes_is_error_when_true() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "id".to_owned(),
            content: "error msg".to_owned(),
            is_error: true,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["tool_use_id"], "id");
        assert_eq!(json["content"], "error msg");
        assert_eq!(json["is_error"], true);
    }

    #[test]
    fn tool_result_omits_is_error_when_false() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "id".to_owned(),
            content: "ok".to_owned(),
            is_error: false,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert!(json.get("is_error").is_none());
    }

    #[test]
    fn tool_result_deserializes_missing_is_error_as_false() {
        let json = serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "id",
            "content": "ok"
        });
        let block: ContentBlock = serde_json::from_value(json).unwrap();
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = block
        else {
            panic!("expected ToolResult");
        };
        assert_eq!(tool_use_id, "id");
        assert_eq!(content, "ok");
        assert!(!is_error);
    }

    // ── ContentBlock::Thinking ──

    #[test]
    fn thinking_round_trips_through_json() {
        let block = ContentBlock::Thinking {
            thinking: "reasoning".to_owned(),
            signature: "sig_abc".to_owned(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "thinking");
        assert_eq!(json["thinking"], "reasoning");
        assert_eq!(json["signature"], "sig_abc");

        let deserialized: ContentBlock = serde_json::from_value(json).unwrap();
        let ContentBlock::Thinking {
            thinking,
            signature,
        } = deserialized
        else {
            panic!("expected Thinking");
        };
        assert_eq!(thinking, "reasoning");
        assert_eq!(signature, "sig_abc");
    }

    // ── ContentBlock::RedactedThinking ──

    #[test]
    fn redacted_thinking_round_trips_through_json() {
        let block = ContentBlock::RedactedThinking {
            data: "base64data==".to_owned(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "redacted_thinking");
        assert_eq!(json["data"], "base64data==");

        let deserialized: ContentBlock = serde_json::from_value(json).unwrap();
        let ContentBlock::RedactedThinking { data } = deserialized else {
            panic!("expected RedactedThinking");
        };
        assert_eq!(data, "base64data==");
    }

    // ── ContentBlock::ServerToolUse ──

    #[test]
    fn server_tool_use_round_trips_through_json() {
        let block = ContentBlock::ServerToolUse {
            id: "stu_01".to_owned(),
            name: "advisor".to_owned(),
            input: serde_json::json!({"query": "test"}),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "server_tool_use");
        assert_eq!(json["id"], "stu_01");
        assert_eq!(json["name"], "advisor");

        let deserialized: ContentBlock = serde_json::from_value(json).unwrap();
        let ContentBlock::ServerToolUse { id, name, input } = deserialized else {
            panic!("expected ServerToolUse");
        };
        assert_eq!(id, "stu_01");
        assert_eq!(name, "advisor");
        assert_eq!(input["query"], "test");
    }

    // ── Message::user ──

    #[test]
    fn user_creates_user_role_with_text() {
        let msg = Message::user("hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == "hello"));
    }

    // ── Message::assistant ──

    #[test]
    fn assistant_creates_assistant_role_with_text() {
        let msg = Message::assistant("hi");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == "hi"));
    }

    // ── strip_trailing_thinking ──

    #[test]
    fn strip_trailing_thinking_removes_thinking_at_end() {
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "answer".to_owned(),
                },
                ContentBlock::Thinking {
                    thinking: "reasoning".to_owned(),
                    signature: "sig".to_owned(),
                },
            ],
        }];
        strip_trailing_thinking(&mut messages);
        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { .. }));
    }

    #[test]
    fn strip_trailing_thinking_removes_redacted_at_end() {
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "answer".to_owned(),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque".to_owned(),
                },
            ],
        }];
        strip_trailing_thinking(&mut messages);
        assert_eq!(messages[0].content.len(), 1);
    }

    #[test]
    fn strip_trailing_thinking_preserves_non_trailing() {
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "reasoning".to_owned(),
                    signature: "sig".to_owned(),
                },
                ContentBlock::Text {
                    text: "answer".to_owned(),
                },
            ],
        }];
        strip_trailing_thinking(&mut messages);
        assert_eq!(messages[0].content.len(), 2);
    }

    #[test]
    fn strip_trailing_thinking_skips_user_messages() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "question".to_owned(),
            }],
        }];
        strip_trailing_thinking(&mut messages);
        assert_eq!(messages[0].content.len(), 1);
    }
}
