//! JSONL entry schema for session files.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::model::{PromptCacheTtl, TokenUsage};
use crate::prompt::SessionContext;
use crate::tool::ToolMetadata;
use crate::tool::tasks::Task;

/// On-disk schema version stamped into every `Header`.
pub(super) const CURRENT_VERSION: u32 = 1;

/// The first line of every session file, identifying the session and where it started.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Header {
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    /// Session project root. Releases through 0.1.0-alpha.5 omit it and use `cwd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) project_root: Option<String>,
    pub(crate) model: String,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) created_at: OffsetDateTime,
    pub(crate) version: u32,
    /// The branch the session started on, which tells sibling sessions apart in the picker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) git_branch: Option<String>,
}

/// One JSONL line in the session transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Entry {
    Header(Header),
    /// Prompt state for rebuilding the request prefix on resume. Written in the same batch before
    /// its user message, with a matching timestamp. The next message with that timestamp binds it
    /// to the chain, whose latest context wins on resume.
    Context {
        #[serde(flatten)]
        context: Box<SessionContext>,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// A transcript message, linked through `parent_uuid` to the message it follows. Resume
    /// rebuilds the transcript along that chain.
    Message {
        uuid: Uuid,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_uuid: Option<Uuid>,
        message: Message,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// Moves the active branch's tip back to the message `tip` names, or to an empty transcript
    /// when `None`. The messages it abandons stay in the file. Resume follows a rewind until a
    /// message line follows it in the file.
    Rewind {
        #[serde(skip_serializing_if = "Option::is_none")]
        tip: Option<Uuid>,
        /// Whether an earlier prompt or manual title consumed the first-prompt title opportunity.
        #[serde(skip_serializing_if = "Option::is_none")]
        first_user_prompt_seen: Option<bool>,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// The session's display title. The newest `updated_at` wins.
    Title(TitleInfo),
    /// Sets whether default listings and `--continue` skip the session. The last one in the file
    /// wins.
    Archive {
        archived: bool,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// The message count listings show, written when a session that gained messages finishes. The
    /// newest `updated_at` wins.
    Summary(ExitInfo),
    /// Display-only sidecar for tool results, indexed by `tool_use_id` on replay.
    ToolResultMetadata {
        tool_use_id: String,
        metadata: ToolMetadata,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// One billed request, whatever branch or compaction it belongs to. Resume sums the cost of
    /// each `uuid` once, since a retried write can repeat a line.
    Usage {
        uuid: Uuid,
        #[serde(flatten)]
        usage: UsageRecord,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// A tracked file's state, written at finish just ahead of the summary.
    FileSnapshot {
        #[serde(flatten)]
        snapshot: FileSnapshot,
    },
    /// Compaction boundary preceding the refreshed `Context` and synthetic post-compact `Message`.
    /// Preserves the message count for display and the task list until a later `tasks` call
    /// replaces it.
    Compact {
        #[serde(flatten)]
        info: CompactInfo,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleInfo {
    pub title: String,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitInfo {
    pub message_count: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// A record a file may repeat, where the newest `updated_at` wins.
pub(super) trait Updated {
    fn updated_at(&self) -> OffsetDateTime;
}

impl Updated for TitleInfo {
    fn updated_at(&self) -> OffsetDateTime {
        self.updated_at
    }
}

impl Updated for ExitInfo {
    fn updated_at(&self) -> OffsetDateTime {
        self.updated_at
    }
}

/// Puts `record` in `slot` unless the record there is at least as new.
pub(super) fn keep_newer<T: Updated>(slot: &mut Option<T>, record: T) {
    if slot
        .as_ref()
        .is_none_or(|current| record.updated_at() > current.updated_at())
    {
        *slot = Some(record);
    }
}

/// A compaction's summary text, its `/compact` focus, and the task list the summarized transcript
/// left, which the summary message carries over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompactRecord {
    pub(crate) summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tasks: Vec<Task>,
}

/// A finished summary and what its compaction boundary records beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactBoundary {
    pub(crate) record: CompactRecord,
    /// Prompt state recorded beside the summary so resume restores the same request prefix.
    pub(crate) context: SessionContext,
}

/// The compaction record a file's boundary entry stores, with the message count it replaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompactInfo {
    #[serde(flatten)]
    pub(crate) record: CompactRecord,
    pub(crate) pre_message_count: u32,
}

/// One billed request's token usage, with the model and prompt-cache TTL that price it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UsageRecord {
    pub(crate) model: String,
    pub(crate) cache_ttl: PromptCacheTtl,
    #[serde(flatten)]
    pub(crate) tokens: TokenUsage,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    /// Source file used by resume and delete, since a renamed file's header ID may name another
    /// file.
    pub path: PathBuf,
    pub cwd: String,
    /// The header's project root, `None` in sessions that releases up to 0.1.0-alpha.5 wrote.
    pub project_root: Option<String>,
    pub last_active_at: OffsetDateTime,
    pub title: Option<TitleInfo>,
    pub exit: Option<ExitInfo>,
    /// Branch the session started on, or `None` outside a git repo.
    pub git_branch: Option<String>,
    pub archive_state: ArchiveState,
}

/// Whether a session is archived, which keeps it out of the default listings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveState {
    #[default]
    Active,
    Archived,
}

impl From<bool> for ArchiveState {
    fn from(archived: bool) -> Self {
        if archived {
            Self::Archived
        } else {
            Self::Active
        }
    }
}

impl ArchiveState {
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Active => Self::Archived,
            Self::Archived => Self::Active,
        }
    }
}

impl SessionInfo {
    /// The project the session belongs to, which is its `cwd` when the header records none.
    pub fn project(&self) -> &str {
        self.project_root.as_deref().unwrap_or(&self.cwd)
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::file_tracker::{FileSource, FileState, LastView};
    use crate::message::{ContentBlock, Role};
    use crate::tool::StoredView;
    use crate::tool::tasks::testing::plan;

    #[test]
    fn header_round_trips_with_correct_discriminator_and_version() {
        let entry = Entry::Header(Header {
            session_id: "abc-123".to_owned(),
            cwd: "/home/user/project".to_owned(),
            project_root: Some("/home/user".to_owned()),
            model: "claude-opus-5-5".to_owned(),
            created_at: datetime!(2026-04-16 12:00:00 UTC),
            version: CURRENT_VERSION,
            git_branch: None,
        });
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "header");
        assert_eq!(json["version"], CURRENT_VERSION);

        let parsed: Entry = serde_json::from_str(&json.to_string()).unwrap();
        let Entry::Header(Header {
            session_id,
            cwd,
            project_root,
            model,
            created_at,
            version,
            git_branch,
        }) = parsed
        else {
            panic!("expected Header");
        };
        assert_eq!(session_id, "abc-123");
        assert_eq!(cwd, "/home/user/project");
        assert_eq!(project_root.as_deref(), Some("/home/user"));
        assert_eq!(model, "claude-opus-5-5");
        assert_eq!(created_at, datetime!(2026-04-16 12:00:00 UTC));
        assert_eq!(version, CURRENT_VERSION);
        assert!(git_branch.is_none());
    }

    #[test]
    fn message_round_trips_with_uuid_and_parent_uuid() {
        let uuid = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let entry = Entry::Message {
            uuid,
            parent_uuid: Some(parent),
            message: Message {
                role: Role::User,
                content: vec![ContentBlock::text("hello")],
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
        assert!(matches!(&message.content[0], ContentBlock::Text { text, .. } if text == "hello"));
        assert_eq!(timestamp, datetime!(2026-04-16 12:00:01 UTC));
    }

    #[test]
    fn message_omits_an_absent_parent_uuid() {
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
    fn message_missing_parent_uuid_parses_as_a_root_message() {
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

    #[test]
    fn title_round_trips() {
        let entry = Entry::Title(TitleInfo {
            title: "Fix auth bug".to_owned(),
            updated_at: datetime!(2026-04-16 12:05:00 UTC),
        });
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "title");

        let parsed: Entry = serde_json::from_str(&json.to_string()).unwrap();
        let Entry::Title(TitleInfo { title, updated_at }) = parsed else {
            panic!("expected Title");
        };
        assert_eq!(title, "Fix auth bug");
        assert_eq!(updated_at, datetime!(2026-04-16 12:05:00 UTC));
    }

    #[test]
    fn summary_round_trips_without_title_field() {
        let entry = Entry::Summary(ExitInfo {
            message_count: 8,
            updated_at: datetime!(2026-04-16 12:05:00 UTC),
        });
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "summary");
        assert!(json.get("title").is_none(), "a summary carries no title");

        let parsed: Entry = serde_json::from_str(&json.to_string()).unwrap();
        let Entry::Summary(ExitInfo {
            message_count,
            updated_at,
        }) = parsed
        else {
            panic!("expected Summary");
        };
        assert_eq!(message_count, 8);
        assert_eq!(updated_at, datetime!(2026-04-16 12:05:00 UTC));
    }

    #[test]
    fn tool_result_metadata_round_trips_with_title_and_stored_view() {
        let entry = Entry::ToolResultMetadata {
            tool_use_id: "grep1".to_owned(),
            metadata: ToolMetadata {
                title: Some("Found 2 files".to_owned()),
                result_view: Some(StoredView::Text),
            },
            timestamp: datetime!(2026-04-16 12:06:00 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "tool_result_metadata");
        assert_eq!(json["tool_use_id"], "grep1");
        assert_eq!(json["metadata"]["title"], "Found 2 files");
        assert_eq!(json["metadata"]["result_view"], "text");
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

    #[test]
    fn usage_round_trips_with_flat_token_fields() {
        let json = serde_json::json!({
            "type": "usage",
            "uuid": "00000000-0000-4000-8000-000000000001",
            "model": "claude-opus-5-5",
            "cache_ttl": "1h",
            "input_tokens": 12,
            "cache_creation_input_tokens": 3_000,
            "cache_read_input_tokens": 40_000,
            "output_tokens": 500,
            "timestamp": "2026-04-16T12:07:00Z",
        });

        let parsed: Entry = serde_json::from_value(json.clone()).unwrap();
        assert!(
            matches!(
                &parsed,
                Entry::Usage { uuid, usage, timestamp }
                    if uuid.as_u128() == 0x0000_0000_0000_4000_8000_0000_0000_0001
                        && usage.model == "claude-opus-5-5"
                        && usage.cache_ttl == PromptCacheTtl::OneHour
                        && [
                            usage.tokens.input_tokens,
                            usage.tokens.cache_creation_input_tokens,
                            usage.tokens.cache_read_input_tokens,
                            usage.tokens.output_tokens,
                        ] == [12, 3_000, 40_000, 500]
                        && *timestamp == datetime!(2026-04-16 12:07:00 UTC)
            ),
            "{parsed:?}"
        );
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    }

    #[test]
    fn file_snapshot_round_trips_with_inlined_payload_fields() {
        let snapshot = FileSnapshot {
            path: std::path::PathBuf::from("/tmp/a.rs"),
            state: FileState {
                content_hash: 0xDEAD_BEEF,
                last_view: LastView::Full,
                source: FileSource::Read,
                recorded_at: datetime!(2026-04-29 12:34:56 UTC),
                tool_use_id: "toolu_1".to_owned(),
            },
        };
        let entry = Entry::FileSnapshot {
            snapshot: snapshot.clone(),
        };

        let json = serde_json::to_value(&entry).unwrap();
        let parsed: Entry = serde_json::from_value(json.clone()).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "type": "file_snapshot",
                "path": "/tmp/a.rs",
                "content_hash": 0xDEAD_BEEF_u64,
                "last_view": {"kind": "full"},
                "recorded_at": "2026-04-29T12:34:56Z",
                "tool_use_id": "toolu_1",
            }),
        );
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
            "last_view": {"kind": "full"},
            "recorded_at": "2026-04-29T12:00:00Z",
            "tool_use_id": "toolu_1",
            "future_field": "from a newer writer",
        });

        let parsed: Entry = serde_json::from_value(json).unwrap();

        let Entry::FileSnapshot { snapshot } = parsed else {
            panic!("expected FileSnapshot, got {parsed:?}");
        };
        assert_eq!(
            snapshot,
            FileSnapshot {
                path: std::path::PathBuf::from("/tmp/a.rs"),
                state: FileState {
                    content_hash: 1,
                    last_view: LastView::Full,
                    source: FileSource::Read,
                    recorded_at: datetime!(2026-04-29 12:00:00 UTC),
                    tool_use_id: "toolu_1".to_owned(),
                },
            },
        );
    }

    #[test]
    fn compact_round_trips_with_instructions_and_tasks() {
        let entry = Entry::Compact {
            info: CompactInfo {
                record: CompactRecord {
                    summary: "user wanted X, did Y, next step Z".to_owned(),
                    instructions: Some("focus on the build error".to_owned()),
                    tasks: plan(),
                },
                pre_message_count: 42,
            },
            timestamp: datetime!(2026-04-16 13:00:00 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "compact");
        assert_eq!(json["summary"], "user wanted X, did Y, next step Z");
        assert_eq!(json["pre_message_count"], 42);
        assert_eq!(json["instructions"], "focus on the build error");
        assert_eq!(json["tasks"], serde_json::to_value(plan()).unwrap());
        assert_eq!(json["timestamp"], "2026-04-16T13:00:00Z");

        let parsed: Entry = serde_json::from_str(&json.to_string()).unwrap();
        let Entry::Compact {
            info:
                CompactInfo {
                    record:
                        CompactRecord {
                            summary,
                            instructions,
                            tasks,
                        },
                    pre_message_count,
                },
            timestamp,
        } = parsed
        else {
            panic!("expected Compact");
        };
        assert_eq!(summary, "user wanted X, did Y, next step Z");
        assert_eq!(pre_message_count, 42);
        assert_eq!(instructions.as_deref(), Some("focus on the build error"));
        assert_eq!(tasks, plan());
        assert_eq!(timestamp, datetime!(2026-04-16 13:00:00 UTC));
    }

    #[test]
    fn compact_omits_absent_instructions_and_tasks_and_reads_them_back_empty() {
        let entry = Entry::Compact {
            info: CompactInfo {
                record: CompactRecord {
                    summary: "s".to_owned(),
                    instructions: None,
                    tasks: Vec::new(),
                },
                pre_message_count: 4,
            },
            timestamp: datetime!(2026-04-16 13:00:00 UTC),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json.get("instructions"), None);
        assert_eq!(json.get("tasks"), None);

        let parsed: Entry = serde_json::from_value(json).unwrap();
        let Entry::Compact {
            info:
                CompactInfo {
                    record:
                        CompactRecord {
                            instructions,
                            tasks,
                            ..
                        },
                    ..
                },
            ..
        } = parsed
        else {
            panic!("expected Compact");
        };
        assert_eq!(instructions, None);
        assert_eq!(tasks, []);
    }

    #[test]
    fn entries_from_older_builds_load_without_the_title_source_or_compaction_trigger() {
        let cases = [
            (
                r#"{"type":"title","title":"hi","source":"ai_generated","updated_at":"2026-04-16T12:00:01Z"}"#,
                r#"{"type":"title","title":"hi","updated_at":"2026-04-16T12:00:01Z"}"#,
            ),
            (
                r#"{"type":"compact","summary":"s","pre_message_count":4,"trigger":"mid_turn","timestamp":"2026-04-16T13:00:00Z"}"#,
                r#"{"type":"compact","summary":"s","pre_message_count":4,"timestamp":"2026-04-16T13:00:00Z"}"#,
            ),
        ];

        for (old, current) in cases {
            let parsed: Entry = serde_json::from_str(old).unwrap();

            assert_eq!(serde_json::to_string(&parsed).unwrap(), current);
        }
    }
}
