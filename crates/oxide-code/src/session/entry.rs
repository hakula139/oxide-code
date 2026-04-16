use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::message::Message;

/// A single line in a session JSONL file.
///
/// Each session file is a sequence of entries, one per line:
/// 1. A [`Header`][Entry::Header] on the first line (session metadata).
/// 2. Zero or more [`Message`][Entry::Message] entries (the conversation).
/// 3. An optional [`Summary`][Entry::Summary] at the end (for fast listing).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    /// First line of every session file.
    Header {
        session_id: String,
        /// If this session was resumed from another, the parent session ID.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        cwd: String,
        model: String,
        #[serde(with = "time::serde::rfc3339")]
        created_at: OffsetDateTime,
    },
    /// A conversation message (user or assistant turn).
    Message {
        message: Message,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// Written at session end. Enables fast listing without full-file parse.
    Summary {
        title: String,
        #[serde(with = "time::serde::rfc3339")]
        updated_at: OffsetDateTime,
        message_count: u32,
    },
}

/// Lightweight session metadata for listing, extracted from the header
/// and (optionally) the summary entry without parsing every message.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    #[expect(dead_code, reason = "populated by store, only read in tests")]
    pub cwd: String,
    pub model: String,
    pub created_at: OffsetDateTime,
    pub title: Option<String>,
    #[expect(dead_code, reason = "populated by store, only read in tests")]
    pub updated_at: Option<OffsetDateTime>,
    pub message_count: Option<u32>,
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use time::macros::datetime;

    use super::*;
    use crate::message::{ContentBlock, Role};

    // ── Entry::Header ──

    #[test]
    fn header_round_trips_through_json() {
        let entry = Entry::Header {
            session_id: "abc-123".to_owned(),
            parent_id: None,
            cwd: "/home/user/project".to_owned(),
            model: "claude-opus-4-6".to_owned(),
            created_at: datetime!(2026-04-16 12:00:00 UTC),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: Entry = serde_json::from_str(&json).unwrap();

        let Entry::Header {
            session_id,
            parent_id,
            cwd,
            model,
            created_at,
        } = parsed
        else {
            panic!("expected Header");
        };
        assert_eq!(session_id, "abc-123");
        assert!(parent_id.is_none());
        assert_eq!(cwd, "/home/user/project");
        assert_eq!(model, "claude-opus-4-6");
        assert_eq!(created_at, datetime!(2026-04-16 12:00:00 UTC));
    }

    #[test]
    fn header_has_correct_type_discriminator() {
        let entry = Entry::Header {
            session_id: "id".to_owned(),
            parent_id: None,
            cwd: "/".to_owned(),
            model: "m".to_owned(),
            created_at: datetime!(2026-01-01 0:00 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "header");
    }

    #[test]
    fn header_omits_parent_id_when_none() {
        let entry = Entry::Header {
            session_id: "id".to_owned(),
            parent_id: None,
            cwd: "/".to_owned(),
            model: "m".to_owned(),
            created_at: datetime!(2026-01-01 0:00 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert!(json.get("parent_id").is_none());
    }

    #[test]
    fn header_includes_parent_id_when_present() {
        let entry = Entry::Header {
            session_id: "child".to_owned(),
            parent_id: Some("parent".to_owned()),
            cwd: "/".to_owned(),
            model: "m".to_owned(),
            created_at: datetime!(2026-01-01 0:00 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["parent_id"], "parent");
    }

    // ── Entry::Message ──

    #[test]
    fn message_entry_round_trips_through_json() {
        let entry = Entry::Message {
            message: Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".to_owned(),
                }],
            },
            timestamp: datetime!(2026-04-16 12:00:01 UTC),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: Entry = serde_json::from_str(&json).unwrap();

        let Entry::Message { message, timestamp } = parsed else {
            panic!("expected Message");
        };
        assert_eq!(message.role, Role::User);
        assert_eq!(message.content.len(), 1);
        assert!(matches!(&message.content[0], ContentBlock::Text { text } if text == "hello"));
        assert_eq!(timestamp, datetime!(2026-04-16 12:00:01 UTC));
    }

    #[test]
    fn message_entry_has_correct_type_discriminator() {
        let entry = Entry::Message {
            message: Message::user("test"),
            timestamp: datetime!(2026-01-01 0:00 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "message");
    }

    // ── Entry::Summary ──

    #[test]
    fn summary_round_trips_through_json() {
        let entry = Entry::Summary {
            title: "Fix auth bug".to_owned(),
            updated_at: datetime!(2026-04-16 12:05:00 UTC),
            message_count: 8,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: Entry = serde_json::from_str(&json).unwrap();

        let Entry::Summary {
            title,
            updated_at,
            message_count,
        } = parsed
        else {
            panic!("expected Summary");
        };
        assert_eq!(title, "Fix auth bug");
        assert_eq!(updated_at, datetime!(2026-04-16 12:05:00 UTC));
        assert_eq!(message_count, 8);
    }

    #[test]
    fn summary_has_correct_type_discriminator() {
        let entry = Entry::Summary {
            title: "t".to_owned(),
            updated_at: datetime!(2026-01-01 0:00 UTC),
            message_count: 0,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "summary");
    }

    // ── JSONL format ──

    #[test]
    fn entries_parse_from_jsonl_lines() {
        let jsonl = indoc! {r#"
            {"type":"header","session_id":"s1","cwd":"/tmp","model":"m","created_at":"2026-04-16T12:00:00Z"}
            {"type":"message","message":{"role":"user","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-04-16T12:00:01Z"}
            {"type":"summary","title":"Greeting","updated_at":"2026-04-16T12:00:02Z","message_count":1}
        "#};
        let entries: Vec<Entry> = jsonl
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert_eq!(entries.len(), 3);
        assert!(matches!(&entries[0], Entry::Header { session_id, .. } if session_id == "s1"));
        assert!(matches!(&entries[1], Entry::Message { .. }));
        assert!(matches!(&entries[2], Entry::Summary { title, .. } if title == "Greeting"));
    }

    #[test]
    fn unknown_type_discriminator_returns_deserialization_error() {
        let json = r#"{"type":"future_type","data":"something"}"#;
        assert!(serde_json::from_str::<Entry>(json).is_err());
    }
}
