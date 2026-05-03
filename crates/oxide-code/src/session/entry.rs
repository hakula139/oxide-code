//! JSONL entry schema for session files.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::tool::ToolMetadata;

pub(crate) const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Entry {
    Header {
        session_id: String,
        cwd: String,
        model: String,
        #[serde(with = "time::serde::rfc3339")]
        created_at: OffsetDateTime,
        #[serde(default = "default_version")]
        version: u32,
    },
    Message {
        uuid: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_uuid: Option<Uuid>,
        message: Message,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    Title {
        title: String,
        source: TitleSource,
        #[serde(with = "time::serde::rfc3339")]
        updated_at: OffsetDateTime,
    },
    Summary {
        message_count: u32,
        #[serde(with = "time::serde::rfc3339")]
        updated_at: OffsetDateTime,
    },
    /// Display-only sidecar for tool results, indexed by `tool_use_id` on replay.
    ToolResultMetadata {
        tool_use_id: String,
        metadata: ToolMetadata,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    FileSnapshot {
        #[serde(flatten)]
        snapshot: FileSnapshot,
    },
    #[serde(other)]
    Unknown,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TitleSource {
    #[default]
    FirstPrompt,
    AiGenerated,
    UserProvided,
}

#[derive(Debug, Clone)]
pub(crate) struct TitleInfo {
    pub(crate) title: String,
    pub(crate) updated_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub(crate) struct ExitInfo {
    pub(crate) message_count: u32,
    pub(crate) updated_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionInfo {
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    #[expect(
        dead_code,
        reason = "read from header for completeness but not consumed by list output"
    )]
    pub(crate) model: String,
    #[expect(
        dead_code,
        reason = "kept for diagnostics but superseded by last_active_at for display and sort"
    )]
    pub(crate) created_at: OffsetDateTime,
    pub(crate) last_active_at: OffsetDateTime,
    pub(crate) title: Option<TitleInfo>,
    pub(crate) exit: Option<ExitInfo>,
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use time::macros::datetime;

    use super::*;
    use crate::message::{ContentBlock, Role};

    // ── Entry::Header ──

    #[test]
    fn header_round_trips_with_correct_discriminator_and_version() {
        let entry = Entry::Header {
            session_id: "abc-123".to_owned(),
            cwd: "/home/user/project".to_owned(),
            model: "claude-opus-4-6".to_owned(),
            created_at: datetime!(2026-04-16 12:00:00 UTC),
            version: CURRENT_VERSION,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "header");
        assert_eq!(json["version"], CURRENT_VERSION);

        let parsed: Entry = serde_json::from_str(&json.to_string()).unwrap();
        let Entry::Header {
            session_id,
            cwd,
            model,
            created_at,
            version,
        } = parsed
        else {
            panic!("expected Header");
        };
        assert_eq!(session_id, "abc-123");
        assert_eq!(cwd, "/home/user/project");
        assert_eq!(model, "claude-opus-4-6");
        assert_eq!(created_at, datetime!(2026-04-16 12:00:00 UTC));
        assert_eq!(version, CURRENT_VERSION);
    }

    #[test]
    fn header_missing_version_defaults_to_current() {
        let json = r#"{"type":"header","session_id":"s","cwd":"/","model":"m","created_at":"2026-04-16T12:00:00Z"}"#;
        let parsed: Entry = serde_json::from_str(json).unwrap();
        let Entry::Header { version, .. } = parsed else {
            panic!("expected Header");
        };
        assert_eq!(version, CURRENT_VERSION);
    }

    // ── Entry::Message ──

    #[test]
    fn message_round_trips_with_uuid_and_parent_uuid() {
        let uuid = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let entry = Entry::Message {
            uuid,
            parent_uuid: Some(parent),
            message: Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".to_owned(),
                }],
            },
            timestamp: datetime!(2026-04-16 12:00:01 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "message");
        assert_eq!(json["uuid"], uuid.to_string());
        assert_eq!(json["parent_uuid"], parent.to_string());

        let parsed: Entry = serde_json::from_str(&json.to_string()).unwrap();
        let Entry::Message {
            uuid: parsed_uuid,
            parent_uuid: parsed_parent,
            message,
            timestamp,
        } = parsed
        else {
            panic!("expected Message");
        };
        assert_eq!(parsed_uuid, uuid);
        assert_eq!(parsed_parent, Some(parent));
        assert_eq!(message.role, Role::User);
        assert!(matches!(&message.content[0], ContentBlock::Text { text } if text == "hello"));
        assert_eq!(timestamp, datetime!(2026-04-16 12:00:01 UTC));
    }

    #[test]
    fn message_omits_parent_uuid_when_none() {
        let entry = Entry::Message {
            uuid: Uuid::new_v4(),
            parent_uuid: None,
            message: Message::user("first"),
            timestamp: datetime!(2026-04-16 12:00:00 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert!(
            json.get("parent_uuid").is_none(),
            "parent_uuid should be omitted when None"
        );
    }

    #[test]
    fn message_missing_parent_uuid_defaults_to_none() {
        let uuid = Uuid::new_v4();
        let json = format!(
            r#"{{"type":"message","uuid":"{uuid}","message":{{"role":"user","content":[{{"type":"text","text":"x"}}]}},"timestamp":"2026-04-16T12:00:00Z"}}"#
        );
        let parsed: Entry = serde_json::from_str(&json).unwrap();
        let Entry::Message { parent_uuid, .. } = parsed else {
            panic!("expected Message");
        };
        assert!(parent_uuid.is_none());
    }

    // ── Entry::Title ──

    #[test]
    fn title_round_trips_with_source() {
        let entry = Entry::Title {
            title: "Fix auth bug".to_owned(),
            source: TitleSource::AiGenerated,
            updated_at: datetime!(2026-04-16 12:05:00 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "title");
        assert_eq!(json["source"], "ai_generated");

        let parsed: Entry = serde_json::from_str(&json.to_string()).unwrap();
        let Entry::Title {
            title,
            source,
            updated_at,
        } = parsed
        else {
            panic!("expected Title");
        };
        assert_eq!(title, "Fix auth bug");
        assert_eq!(source, TitleSource::AiGenerated);
        assert_eq!(updated_at, datetime!(2026-04-16 12:05:00 UTC));
    }

    // ── Entry::Summary ──

    #[test]
    fn summary_round_trips_without_title_field() {
        let entry = Entry::Summary {
            message_count: 8,
            updated_at: datetime!(2026-04-16 12:05:00 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "summary");
        assert!(
            json.get("title").is_none(),
            "title moved to its own entry type"
        );

        let parsed: Entry = serde_json::from_str(&json.to_string()).unwrap();
        let Entry::Summary {
            message_count,
            updated_at,
        } = parsed
        else {
            panic!("expected Summary");
        };
        assert_eq!(message_count, 8);
        assert_eq!(updated_at, datetime!(2026-04-16 12:05:00 UTC));
    }

    // ── Entry::ToolResultMetadata ──

    #[test]
    fn tool_result_metadata_round_trips_with_title_and_replacements() {
        let entry = Entry::ToolResultMetadata {
            tool_use_id: "edit1".to_owned(),
            metadata: ToolMetadata {
                title: Some("Edited f.rs".to_owned()),
                replacements: Some(3),
                ..ToolMetadata::default()
            },
            timestamp: datetime!(2026-04-16 12:06:00 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "tool_result_metadata");
        assert_eq!(json["tool_use_id"], "edit1");
        assert_eq!(json["metadata"]["title"], "Edited f.rs");
        assert_eq!(json["metadata"]["replacements"], 3);
        assert_eq!(json["timestamp"], "2026-04-16T12:06:00Z");

        let parsed: Entry = serde_json::from_str(&json.to_string()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    }

    #[test]
    fn tool_result_metadata_parses_with_unknown_metadata_fields() {
        let json = r#"{"type":"tool_result_metadata","tool_use_id":"e","metadata":{"title":"t","future_field":123},"timestamp":"2026-04-16T12:00:00Z"}"#;
        let parsed: Entry = serde_json::from_str(json).unwrap();
        let reserialized = serde_json::to_value(&parsed).unwrap();
        assert_eq!(reserialized["type"], "tool_result_metadata");
        assert_eq!(reserialized["tool_use_id"], "e");
        assert_eq!(reserialized["metadata"]["title"], "t");
        assert!(reserialized["metadata"].get("future_field").is_none());
    }

    // ── Entry::FileSnapshot ──

    #[test]
    fn file_snapshot_round_trips_with_inlined_payload_fields() {
        let snapshot = FileSnapshot {
            path: std::path::PathBuf::from("/tmp/a.rs"),
            content_hash: 0xDEAD_BEEF,
            mtime: datetime!(2026-04-29 12:00:00 UTC),
            size: 7,
            last_view: crate::file_tracker::LastView::Full,
            recorded_at: datetime!(2026-04-29 12:34:56 UTC),
        };
        let entry = Entry::FileSnapshot {
            snapshot: snapshot.clone(),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "file_snapshot");
        assert_eq!(json["path"], "/tmp/a.rs");
        assert_eq!(json["content_hash"], 0xDEAD_BEEF_u64);
        assert_eq!(json["mtime"], "2026-04-29T12:00:00Z");
        assert!(
            json.get("snapshot").is_none(),
            "payload must flatten, not nest under `snapshot`",
        );

        let parsed: Entry = serde_json::from_value(json).unwrap();
        let Entry::FileSnapshot { snapshot: parsed } = parsed else {
            panic!("expected FileSnapshot");
        };
        assert_eq!(parsed, snapshot);
    }

    #[test]
    fn file_snapshot_unknown_field_is_ignored_for_forward_compat() {
        let json = serde_json::json!({
            "type": "file_snapshot",
            "path": "/tmp/a.rs",
            "content_hash": 1_u64,
            "mtime": "2026-04-29T12:00:00Z",
            "size": 5,
            "last_view": {"kind": "full"},
            "recorded_at": "2026-04-29T12:00:00Z",
            "future_field": "from a newer writer",
        });
        let parsed: Entry = serde_json::from_value(json).unwrap();
        let Entry::FileSnapshot { snapshot } = parsed else {
            panic!("expected FileSnapshot, got {parsed:?}");
        };
        assert_eq!(snapshot.path, std::path::PathBuf::from("/tmp/a.rs"));
        assert_eq!(snapshot.content_hash, 1);
    }

    // ── Entry::Unknown ──

    #[test]
    fn unknown_discriminator_parses_as_unknown_variant() {
        let json = r#"{"type":"future_type","data":"something"}"#;
        let parsed: Entry = serde_json::from_str(json).unwrap();
        assert!(matches!(parsed, Entry::Unknown));
    }

    // ── JSONL format ──

    #[test]
    fn entries_parse_from_jsonl_lines() {
        let jsonl = indoc! {r#"
            {"type":"header","session_id":"s1","cwd":"/tmp","model":"m","created_at":"2026-04-16T12:00:00Z","version":1}
            {"type":"message","uuid":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","message":{"role":"user","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-04-16T12:00:01Z"}
            {"type":"title","title":"hi","source":"first_prompt","updated_at":"2026-04-16T12:00:01Z"}
            {"type":"summary","message_count":1,"updated_at":"2026-04-16T12:00:02Z"}
        "#};
        let entries: Vec<Entry> = jsonl
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert_eq!(entries.len(), 4);
        assert!(
            matches!(&entries[0], Entry::Header { session_id, version, .. } if session_id == "s1" && *version == CURRENT_VERSION)
        );
        let Entry::Message {
            uuid,
            parent_uuid,
            message,
            ..
        } = &entries[1]
        else {
            panic!("expected Entry::Message, got {:?}", &entries[1]);
        };
        assert_eq!(uuid.to_string(), "a1b2c3d4-e5f6-7890-abcd-1234567890ef");
        assert_eq!(*parent_uuid, None);
        assert_eq!(message.role, crate::message::Role::User);
        assert!(
            matches!(&entries[2], Entry::Title { title, source, .. } if title == "hi" && *source == TitleSource::FirstPrompt)
        );
        assert!(matches!(
            &entries[3],
            Entry::Summary {
                message_count: 1,
                ..
            }
        ));
    }
}
