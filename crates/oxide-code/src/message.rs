use serde::{Deserialize, Serialize};

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
        #[serde(default)]
        is_error: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── ContentBlock::ToolResult ──

    #[test]
    fn tool_result_serializes_is_error() {
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
    fn tool_result_deserializes_missing_is_error_as_false() {
        let json = serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "id",
            "content": "ok"
        });
        let block: ContentBlock = serde_json::from_value(json).unwrap();
        assert!(matches!(
            block,
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
    }
}
