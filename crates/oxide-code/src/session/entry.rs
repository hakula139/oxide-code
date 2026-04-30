//! JSONL entry schema for session files.
//!
//! Each session file is a sequence of [`Entry`] values, one per line.
//! The format is forward-compatible: [`Entry::Unknown`] absorbs entry
//! types this reader doesn't recognize, so newer writers can emit
//! additional variants without breaking older readers.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::message::Message;
use crate::tool::ToolMetadata;
use crate::tool::tracker::FileSnapshot;

/// Current session file format version. Bump on incompatible changes.
pub(crate) const CURRENT_VERSION: u32 = 1;

/// A single line in a session JSONL file.
///
/// Each session file is a sequence of entries:
///
/// 1. A [`Header`][Entry::Header] on the first line (session metadata).
/// 2. Zero or more other entries — [`Message`][Entry::Message] carries the
///    conversation; [`Title`][Entry::Title] may appear multiple times (latest
///    wins); [`Summary`][Entry::Summary] marks a clean exit (latest wins).
/// 3. The [`Unknown`][Entry::Unknown] variant absorbs entry types this
///    reader does not recognize, so newer writers can emit additional
///    types without breaking older readers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Entry {
    /// First line of every session file.
    Header {
        session_id: String,
        cwd: String,
        model: String,
        #[serde(with = "time::serde::rfc3339")]
        created_at: OffsetDateTime,
        /// Format version. Readers reject files with a newer version.
        #[serde(default = "default_version")]
        version: u32,
    },
    /// A conversation message with stable identity, chained via
    /// [`parent_uuid`][Self::Message::parent_uuid].
    ///
    /// The chain enables future forking / partial replay without schema
    /// migration — a message can be identified by its UUID and branched
    /// from without rewriting the parent file.
    Message {
        /// Stable identity for this message.
        uuid: Uuid,
        /// Immediate predecessor in the chain. `None` only for the first
        /// message in the file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_uuid: Option<Uuid>,
        message: Message,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// Session title. Re-appendable — the latest occurrence in the tail
    /// wins. Written early (on first user prompt) so interrupted sessions
    /// still have a title; may be superseded later by an AI-generated or
    /// user-provided title.
    Title {
        title: String,
        source: TitleSource,
        #[serde(with = "time::serde::rfc3339")]
        updated_at: OffsetDateTime,
    },
    /// Session exit marker. Written on clean exit. Latest wins.
    Summary {
        message_count: u32,
        #[serde(with = "time::serde::rfc3339")]
        updated_at: OffsetDateTime,
    },
    /// TUI-only metadata for a completed tool call, persisted
    /// alongside the [`Entry::Message`] that carries the matching
    /// [`ContentBlock::ToolResult`](crate::message::ContentBlock).
    ///
    /// The API-facing `ContentBlock::ToolResult` wire format only
    /// carries `tool_use_id` + `content` + `is_error`; display fields
    /// like the result's header title or the edit tool's replacement
    /// count would pollute that contract. This sidecar entry lets
    /// the replay path reconstruct a rich view without polluting the
    /// wire format — resumed sessions match what the user saw live.
    ///
    /// On replay, [`SessionData`] (via `load_session_data`) indexes
    /// these by `tool_use_id`; a missing entry means pre-upgrade
    /// sessions (fallback to content-derived defaults) or a tool
    /// that attached no metadata.
    ToolResultMetadata {
        tool_use_id: String,
        metadata: ToolMetadata,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// Persisted file-tracker state, one per tracked file. Written by
    /// [`super::state::SessionState::finish_entries`] at session end so
    /// resume can skip the cold-tracker re-Read on every previously-
    /// observed file. The shape is wire-stable; see
    /// [`FileSnapshot`][crate::tool::tracker::FileSnapshot].
    FileSnapshot {
        #[serde(flatten)]
        snapshot: FileSnapshot,
    },
    /// Catch-all for unrecognized entry types. Preserves parse
    /// compatibility when a newer writer emits a type this reader
    /// doesn't know.
    #[serde(other)]
    Unknown,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}

/// How a session title was derived.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TitleSource {
    /// Derived from the first user prompt.
    #[default]
    FirstPrompt,
    /// AI-generated via a background summarization call. Emitted by
    /// the planned AI-title feature; accepted on read today.
    AiGenerated,
    /// Explicitly set by the user. Emitted by the planned `/title`
    /// command; accepted on read today.
    UserProvided,
}

/// Title fields extracted from the latest [`Entry::Title`] in a session.
///
/// `source` from [`Entry::Title`] is preserved on disk but not projected
/// here — consumers only need the text today. The AI-title feature will
/// reintroduce it together with a display annotation.
#[derive(Debug, Clone)]
pub(crate) struct TitleInfo {
    pub(crate) title: String,
    /// Used by the merge between head and tail scan to pick the latest
    /// title when a session has more than one [`Entry::Title`].
    pub(crate) updated_at: OffsetDateTime,
}

/// Exit fields extracted from the latest [`Entry::Summary`] in a session.
#[derive(Debug, Clone)]
pub(crate) struct ExitInfo {
    pub(crate) message_count: u32,
    /// Drives the latest-wins tiebreak in `store::read_session_info`
    /// when a resumed session has appended more than one summary.
    pub(crate) updated_at: OffsetDateTime,
}

/// Lightweight session metadata for listing, extracted from the header
/// (first line) and a tail scan (latest [`Title`][Entry::Title] and
/// [`Summary`][Entry::Summary]) without parsing every message.
#[derive(Debug, Clone)]
pub(crate) struct SessionInfo {
    pub(crate) session_id: String,
    /// Working directory the session was started from. Surfaced as the
    /// `Project` column under `--list --all` so cross-project listings
    /// remain disambiguable.
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
    /// File mtime. Drives the `--list` sort order and display column so
    /// resumed sessions bubble to the top. Falls back to `created_at` if
    /// mtime cannot be read.
    pub(crate) last_active_at: OffsetDateTime,
    /// Present when the session file contains a [`Title`][Entry::Title].
    pub(crate) title: Option<TitleInfo>,
    /// Present when the session exited cleanly (i.e., a
    /// [`Summary`][Entry::Summary] entry was written). Absent for
    /// interrupted sessions.
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
        // Unset fields must be absent, not serialized as null — the
        // file bytes grow quickly across many tool turns.
        assert!(json["metadata"].get("exit_code").is_none());

        // Bytes → Entry → bytes must be identical so every field
        // (not just the ones spot-checked above) round-trips.
        let parsed: Entry = serde_json::from_str(&json.to_string()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    }

    #[test]
    fn tool_result_metadata_parses_with_unknown_metadata_fields() {
        // Forward-compat: a newer writer may emit metadata fields
        // this version doesn't know about. The entry must still
        // parse and surface the known fields — per `#[serde(other)]`
        // on `Entry`, unknown entry *types* fall through to
        // `Entry::Unknown`, but unknown metadata *fields* should
        // just be ignored by serde default behavior.
        let json = r#"{"type":"tool_result_metadata","tool_use_id":"e","metadata":{"title":"t","future_field":123},"timestamp":"2026-04-16T12:00:00Z"}"#;
        let parsed: Entry = serde_json::from_str(json).unwrap();
        // Re-serialize to observe the surviving known fields without
        // destructuring (which would require an unreachable else-arm).
        let reserialized = serde_json::to_value(&parsed).unwrap();
        assert_eq!(reserialized["type"], "tool_result_metadata");
        assert_eq!(reserialized["tool_use_id"], "e");
        assert_eq!(reserialized["metadata"]["title"], "t");
        // Unknown field is silently dropped on re-serialization.
        assert!(reserialized["metadata"].get("future_field").is_none());
    }

    // ── Entry::FileSnapshot ──

    #[test]
    fn file_snapshot_round_trips_with_inlined_payload_fields() {
        // The variant uses `#[serde(flatten)]` so the JSON shape is
        // `{"type":"file_snapshot","path":...,"content_hash":...,...}`,
        // not nested under a `snapshot` key. Pin the flattened layout
        // so a future inline-table refactor can't silently change the
        // wire format.
        let snapshot = FileSnapshot {
            path: std::path::PathBuf::from("/tmp/a.rs"),
            content_hash: 0xDEAD_BEEF,
            mtime: datetime!(2026-04-29 12:00:00 UTC),
            size: 7,
            last_view: crate::tool::tracker::LastView::Full,
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
