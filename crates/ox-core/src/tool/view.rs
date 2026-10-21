//! The structured shapes frontends render for a tool call: result views and approval previews.

use serde::{Deserialize, Serialize};

use super::tasks::Task;

// ── Tool Result View ──

/// Per-tool structured shape of a completed tool call's body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ToolResultView {
    Text {
        content: String,
    },
    ReadExcerpt {
        path: String,
        lines: Vec<ReadExcerptLine>,
        total_lines: usize,
    },
    Diff(EditDiff),
    GrepMatches {
        groups: Vec<GrepFileGroup>,
        truncated: bool,
    },
    GlobFiles {
        pattern: String,
        files: Vec<String>,
        total: usize,
    },
    /// The task list a `tasks` call sent, before a finished list clears.
    Tasks {
        tasks: Vec<Task>,
    },
}

/// One line in a `ReadExcerpt` view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadExcerptLine {
    pub number: usize,
    pub text: String,
}

/// One numbered line on either side of a diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub number: usize,
    pub text: String,
}

/// A single boundary-trimmed edit hunk. Consumers must not re-trim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffChunk {
    pub old: Vec<DiffLine>,
    pub new: Vec<DiffLine>,
}

/// What an edit changed. Every replacement swaps the same text, so the first one's hunk stands for
/// them all, and `locations` lists the line each one applied at: its first removed line, or for a
/// pure insertion its first added line. Both are empty when the hunk trims to nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditDiff {
    pub chunk: DiffChunk,
    pub locations: Vec<usize>,
}

/// One file's match block in a grep result view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepFileGroup {
    pub path: String,
    pub lines: Vec<GrepMatchLine>,
}

/// One row in a [`GrepFileGroup`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatchLine {
    pub number: usize,
    pub text: String,
    pub is_match: bool,
}

// ── Approval Preview ──

/// What a pending approval shows the user, rendered by the tool so the modal only displays it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPreview {
    /// Tool display label, such as `Bash` or `Write(src/main.rs)`.
    pub title: String,
    pub body: ApprovalBody,
}

/// The renderable body of an [`ApprovalPreview`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ApprovalBody {
    /// The call's label, which for `bash` is the whole command and may span lines.
    Command(String),
    /// The change an `edit` or `write` would make, in the chat's `+` / `-` chunk shape.
    Diff(DiffChunk),
}
