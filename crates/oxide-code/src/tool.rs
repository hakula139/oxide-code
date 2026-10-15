//! Tool dispatch.

pub(crate) mod bash;
pub(crate) mod edit;
pub(crate) mod glob;
pub(crate) mod grep;
pub(crate) mod read;
pub(crate) mod write;

use std::borrow::Cow;
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::agent::event::{ApprovalBody, ApprovalPreview};
use crate::file_tracker::{FileTracker, TrackableFileError, validate_trackable_file};
use crate::permission::{GateTarget, PermissionClass};
use crate::util::path::lexical_normalize;
use crate::util::text::ELLIPSIS;

// ── Tool Definition ──

/// Tool schema exposed to the Anthropic API.
#[derive(Clone, Serialize)]
pub(crate) struct ToolDefinition {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) input_schema: serde_json::Value,
}

// ── Tool Output ──

/// Result returned from a tool execution to the agent loop.
pub(crate) struct ToolOutput {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) metadata: ToolMetadata,
}

/// Structured data for UI display and logging. The model never sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) replacements: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) diff_chunks: Option<Vec<DiffChunk>>,
    /// Unbounded match count when a tool capped returned rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) truncated_total: Option<usize>,
    /// Pre-cap byte count when the registry byte safety net fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) truncated_bytes: Option<usize>,
}

impl ToolOutput {
    pub(crate) fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: ToolMetadata::default(),
        }
    }

    pub(crate) fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: ToolMetadata::default(),
        }
    }

    pub(crate) fn with_title(mut self, title: impl Into<String>) -> Self {
        self.metadata.title = Some(title.into());
        self
    }

    pub(crate) fn with_replacements(mut self, count: usize) -> Self {
        self.metadata.replacements = Some(count);
        self
    }

    pub(crate) fn with_diff_chunks(mut self, chunks: Vec<DiffChunk>) -> Self {
        self.metadata.diff_chunks = Some(chunks);
        self
    }

    /// Records the unbounded match count for a truncated result.
    pub(crate) fn with_truncated_total(mut self, total: usize) -> Self {
        self.metadata.truncated_total = Some(total);
        self
    }
}

// ── Tool Result View ──

/// Per-tool structured shape of a completed tool call's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolResultView {
    Text {
        content: String,
    },
    ReadExcerpt {
        path: String,
        lines: Vec<ReadExcerptLine>,
        total_lines: usize,
    },
    Diff {
        chunks: Vec<DiffChunk>,
        replace_all: bool,
        replacements: usize,
    },
    GrepMatches {
        groups: Vec<GrepFileGroup>,
        truncated: bool,
    },
    GlobFiles {
        pattern: String,
        files: Vec<String>,
        total: usize,
    },
}

/// One line in a `ReadExcerpt` view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadExcerptLine {
    pub(crate) number: usize,
    pub(crate) text: String,
}

/// One numbered line on either side of a diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DiffLine {
    pub(crate) number: usize,
    pub(crate) text: String,
}

/// A single boundary-trimmed edit hunk. Consumers must not re-trim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DiffChunk {
    pub(crate) old: Vec<DiffLine>,
    pub(crate) new: Vec<DiffLine>,
}

/// One file's match block in a grep result view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrepFileGroup {
    pub(crate) path: String,
    pub(crate) lines: Vec<GrepMatchLine>,
}

/// One row in a [`GrepFileGroup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrepMatchLine {
    pub(crate) number: usize,
    pub(crate) text: String,
    pub(crate) is_match: bool,
}

// ── Tool Trait ──

/// A tool that the agent can invoke.
///
/// Per-instance metadata (name, icon, schema, input summary) lives on the trait, so adding a tool
/// means editing only its module.
pub(crate) trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> serde_json::Value;

    /// The permission class the gate files this tool's calls under. No default, so a new tool
    /// cannot silently inherit a permissive class.
    fn permission_class(&self) -> PermissionClass;

    /// The input field naming the file this call acts on, if any. [`Self::gate_target`] resolves it
    /// by default, and [`Self::pin_target`] rewrites it to the resolved path.
    fn path_field(&self) -> Option<&'static str> {
        None
    }

    /// Extracts what the permission gate matches rules against from this call's `input`, resolving
    /// any path against `cwd`. Defaults to the resolved [`Self::path_field`], or
    /// [`GateTarget::None`] without one, so only a class-wide rule matches.
    fn gate_target(&self, input: &serde_json::Value, cwd: &Path) -> GateTarget {
        self.path_field()
            .and_then(|field| extract_input_field(input, field))
            .map(|path| GateTarget::for_path(path, cwd))
            .unwrap_or_default()
    }

    /// Rewrites [`Self::path_field`] in `input` to the resolved path so the tool opens what the
    /// gate checked. A symlink swapped into an intermediate directory afterwards is not covered.
    fn pin_target(&self, input: &mut serde_json::Value, target: &GateTarget) {
        if let (Some(field), GateTarget::Path { canonical, .. }) = (self.path_field(), target)
            && let Some(value) = input.get_mut(field)
        {
            *value = serde_json::Value::String(canonical.clone());
        }
    }

    fn icon(&self) -> &'static str {
        DEFAULT_TOOL_ICON
    }

    /// Pulls the tool's primary argument out of `input` for terse `Name(arg)` labels in the UI.
    /// Returning `None` falls back to the bare name.
    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        _ = input;
        None
    }

    fn summarize_call(&self, input: &serde_json::Value) -> String {
        let label = title_case(self.name());
        match self.summarize_input(input) {
            Some(arg) => format!("{label}({arg})"),
            None => label,
        }
    }

    /// Returns the structured shape of a successful tool result for richer UI rendering, or `None`
    /// to fall back to [`ToolResultView::Text`]. Errors short-circuit to text in the registry.
    fn result_view(
        &self,
        _input: &serde_json::Value,
        _content: &str,
        _metadata: &ToolMetadata,
    ) -> Option<ToolResultView> {
        None
    }

    /// Builds what the approval modal shows when the gate asks. Defaults to the tool name over the
    /// `summarize_call` label. Edit and write override it with a diff.
    fn approval_preview(&self, input: &serde_json::Value) -> ApprovalPreview {
        ApprovalPreview {
            title: title_case(self.name()),
            body: ApprovalBody::Command(self.summarize_call(input)),
        }
    }

    /// Executes the call. A user cancel can drop the future at any await, so work it started must
    /// either stop with the drop, as a bash process group does, or finish together with its
    /// bookkeeping, as a file write and its tracker record do.
    fn run(
        &self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>>;

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name(),
            description: self.description(),
            input_schema: self.input_schema(),
        }
    }
}

pub(crate) const DEFAULT_TOOL_ICON: &str = "⟡";

pub(crate) fn extract_input_field<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(serde_json::Value::as_str)
}

pub(crate) fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ── Tool Registry ──

/// Owns the set of [`Tool`]s available to the agent and dispatches calls by name.
///
/// The registry is built once at session start with the tools the model is allowed to invoke.
/// Lookups are linear over a small `Vec` (registry size is fixed at < 10), so no map is needed.
pub(crate) struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub(crate) fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self { tools }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(AsRef::as_ref)
    }

    pub(crate) fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    pub(crate) fn icon(&self, name: &str) -> &'static str {
        self.get(name).map_or(DEFAULT_TOOL_ICON, Tool::icon)
    }

    pub(crate) fn label(&self, name: &str, input: &serde_json::Value) -> String {
        self.get(name)
            .map_or_else(|| name.to_owned(), |t| t.summarize_call(input))
    }

    /// Dispatches to the named tool and enforces the `MAX_OUTPUT_BYTES` byte safety net on the
    /// returned content, recording the pre-cap length on `metadata.truncated_bytes` if trimming
    /// fired. Per-tool row caps run inside each tool, so this is the last-line defense against
    /// pathological output sizes.
    pub(crate) async fn run(&self, name: &str, input: serde_json::Value) -> ToolOutput {
        let Some(tool) = self.get(name) else {
            return ToolOutput::error(format!("Unknown tool: {name}"));
        };
        let mut output = tool.run(input).await;
        let (content, original_len) = cap_output(output.content);
        output.content = content;
        if let Some(len) = original_len {
            output.metadata.truncated_bytes = Some(len);
        }
        output
    }

    /// Resolves the structured result view for a tool call. Errors short-circuit to
    /// [`ToolResultView::Text`] regardless of the tool's preference, so failure messages render
    /// consistently across the chat. Unknown tool names also fall back to text.
    pub(crate) fn result_view(
        &self,
        name: &str,
        input: &serde_json::Value,
        content: &str,
        metadata: &ToolMetadata,
        is_error: bool,
    ) -> ToolResultView {
        if is_error {
            return ToolResultView::Text {
                content: content.to_owned(),
            };
        }
        self.get(name)
            .and_then(|t| t.result_view(input, content, metadata))
            .unwrap_or_else(|| ToolResultView::Text {
                content: content.to_owned(),
            })
    }
}

// ── Output Cap ──

pub(crate) const MAX_OUTPUT_BYTES: usize = 128 * 1024;

const TRUNCATION_OVERHEAD: usize = 80;

/// Trims `content` to fit under [`MAX_OUTPUT_BYTES`], keeping the head and tail and replacing the
/// middle with a `[N bytes truncated]` marker.
///
/// Returns `(content, original_len)`, where `original_len` is `Some` only when truncation fired so
/// the caller can record the pre-cap size. Skips truncation when the omitted slice is smaller than
/// the marker, which would otherwise grow the output.
/// Char-boundary floors keep the returned `String` valid UTF-8 even when the cap lands inside a
/// multibyte sequence.
fn cap_output(content: String) -> (String, Option<usize>) {
    if content.len() <= MAX_OUTPUT_BYTES {
        return (content, None);
    }

    let half = MAX_OUTPUT_BYTES / 2;
    let head_end = content.floor_char_boundary(half);
    let tail_start = content.floor_char_boundary(content.len() - half);

    let omitted_bytes = tail_start - head_end;
    if omitted_bytes < TRUNCATION_OVERHEAD {
        return (content, None);
    }

    let original_len = content.len();
    let mut truncated = String::with_capacity(MAX_OUTPUT_BYTES + TRUNCATION_OVERHEAD);
    truncated.push_str(&content[..head_end]);
    _ = write!(
        truncated,
        "\n... [{omitted_bytes} bytes truncated; head + tail kept] ...\n"
    );
    truncated.push_str(&content[tail_start..]);

    (truncated, Some(original_len))
}

// ── Input Parsing ──

/// Deserializes a tool's typed input from the raw JSON the agent loop hands us. On failure returns
/// an "Invalid input: ..." error [`ToolOutput`] that callers hand back to the model as-is.
#[expect(
    clippy::result_large_err,
    reason = "ToolOutput carries the full tool result; the Err here is the cold input-validation path constructed at most once per tool call"
)]
pub(crate) fn parse_input<T: DeserializeOwned>(raw: serde_json::Value) -> Result<T, ToolOutput> {
    serde_json::from_value(raw).map_err(|e| ToolOutput::error(format!("Invalid input: {e}")))
}

// ── Path Utilities ──

/// The canonical working directory, so hits under a canonical search base render relative to it.
pub(crate) fn working_dir() -> Result<PathBuf, String> {
    std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|e| format!("Failed to get working directory: {e}"))
}

/// Resolves a search tool's `path` argument against `cwd`, defaulting to `cwd` itself. The base
/// must exist, and it is canonical so the walk covers the directory the permission gate resolved.
pub(crate) fn resolve_base_dir(search_path: Option<&str>, cwd: &Path) -> Result<PathBuf, String> {
    let base = search_path.map_or_else(|| cwd.to_path_buf(), |p| cwd.join(p));
    std::fs::canonicalize(&base)
        .map_err(|e| format!("Cannot access {}: {e}", display_path(&base, cwd)))
}

pub(crate) fn display_cwd_path(path: &str) -> String {
    let cwd = std::env::current_dir().ok();
    display_cwd_path_from(path, cwd.as_deref())
}

fn display_cwd_path_from(path: &str, cwd: Option<&Path>) -> String {
    match cwd {
        Some(cwd) => display_path(Path::new(path), cwd),
        None => path.to_owned(),
    }
}

/// The [`FileTracker`](crate::file_tracker::FileTracker) key for a file tool's `file_path`. An
/// existing path canonicalizes, so relative, dotted, and symlinked spellings of one file share an
/// entry. A missing path falls back to its lexically clean absolute form.
pub(crate) async fn tracker_key(path: &str) -> PathBuf {
    let absolute = std::env::current_dir().unwrap_or_default().join(path);
    tokio::fs::canonicalize(&absolute)
        .await
        .unwrap_or_else(|_| lexical_normalize(&absolute))
}

/// Renders a `Name(path)` summary for a tool call whose primary argument is a filesystem path.
///
/// `path_key` names the JSON field to extract from `input` (e.g. `"file_path"`, `"path"`).
/// Falls back to the bare title-cased tool name when the field is absent or non-string. Path
/// is rendered through [`display_cwd_path`] so it appears relative to cwd.
pub(crate) fn summarize_path_call(
    tool_name: &str,
    input: &serde_json::Value,
    path_key: &str,
) -> String {
    let label = title_case(tool_name);
    match extract_input_field(input, path_key) {
        Some(path) => format!("{label}({})", display_cwd_path(path)),
        None => label,
    }
}

/// Renders `path` for chat display.
///
/// Strips `base` if it's a prefix. When `path == base` the relative path is empty, so this returns
/// the file name (`foo.rs`). Paths outside `base` render as their full string. Lossy UTF-8
/// conversion is acceptable here because the result is for display only.
pub(crate) fn display_path(path: &Path, base: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(base) {
        if rel.as_os_str().is_empty() {
            return path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |n| n.to_string_lossy().into_owned(),
            );
        }
        return rel.to_string_lossy().into_owned();
    }
    path.to_string_lossy().into_owned()
}

/// Returns the trailing filename component, falling back to the input string when no component
/// can be extracted (root path, non-UTF-8 component). The fallback is what makes this safe to
/// call on user-supplied path strings.
pub(crate) fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

// ── File Target Validation ──

/// Rejects targets the [`FileTracker`](crate::file_tracker::FileTracker) cannot gate, phrasing each
/// refusal for `verb` (`read`, `edit`, `write`).
pub(crate) fn validate_file_target(
    path: &str,
    metadata: &std::fs::Metadata,
    verb: &str,
) -> Result<(), String> {
    match validate_trackable_file(metadata) {
        Ok(()) => Ok(()),
        Err(TrackableFileError::Directory) => Err(format!(
            "{path} is a directory. Use the glob tool to list directory contents."
        )),
        Err(TrackableFileError::NonRegular) => Err(format!(
            "{path} is not a regular file (fifo, socket, or device). Refusing to {verb}.",
        )),
        Err(TrackableFileError::TooLarge { size, max }) => {
            let mb = bytes_to_mb(size);
            let limit_mb = max / (1024 * 1024);
            Err(format!(
                "File is too large ({mb:.1} MB, max {limit_mb} MB). \
                 Use the bash tool to {verb} large files.",
            ))
        }
    }
}

// ── Binary Detection ──

const BINARY_CHECK_SIZE: usize = 8192;

/// Heuristic: a NUL byte in the first 8 KiB marks the file as binary. Git uses the same rule, and
/// it is cheap enough to run before every text-mode read.
pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_CHECK_SIZE).any(|&b| b == 0)
}

// ── File Reading ──

/// Reads `path` once `validate` accepts its metadata, checked by path before the open and again on
/// the opened file, so a file swapped in between cannot skip the check. On Unix the open also
/// refuses a symlink at the final component and never blocks on a fifo.
pub(crate) async fn read_validated(
    path: &str,
    validate: impl Fn(&std::fs::Metadata) -> Result<(), String>,
) -> Result<(std::fs::Metadata, Vec<u8>), String> {
    let error = |e: std::io::Error| format!("Error reading {path}: {e}");
    validate(&tokio::fs::metadata(path).await.map_err(error)?)?;

    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags((nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_NONBLOCK).bits());
    let mut file = options.open(path).await.map_err(error)?;
    let metadata = file.metadata().await.map_err(error)?;
    validate(&metadata)?;

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.map_err(error)?;
    Ok((metadata, bytes))
}

// ── File Writing ──

/// Writes `bytes` to `path`, on Unix without following a symlink at the final component, so a link
/// swapped in after the gate resolved the path cannot redirect the write. `create_new` refuses any
/// existing entry, a dangling symlink included.
pub(crate) async fn write_no_follow(
    path: &Path,
    bytes: &[u8],
    create_new: bool,
) -> std::io::Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.truncate(true);
    }
    #[cfg(unix)]
    options.custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());

    let mut file = options.open(path).await?;
    file.write_all(bytes).await?;
    file.flush().await
}

/// Writes `bytes` like [`write_no_follow`] and records them under the path's [`tracker_key`] as one
/// unit, so a caller dropped mid-write cannot leave the file written but unrecorded.
pub(crate) async fn write_tracked(
    tracker: &Arc<FileTracker>,
    path: &str,
    bytes: Vec<u8>,
    create_new: bool,
) -> std::io::Result<()> {
    let path = path.to_owned();
    tracker
        .write_and_record(async move {
            write_no_follow(Path::new(&path), &bytes, create_new).await?;
            Ok((tracker_key(&path).await, bytes))
        })
        .await
}

// ── File Walking ──

const MAX_WALK_DEPTH: usize = 64;

const VCS_DIRS: &[&str] = &[".bzr", ".git", ".hg", ".jj", ".sl", ".svn"];

/// Recursively walks `base`, yielding regular files only.
///
/// Honors `.gitignore` and global ignore files, includes hidden entries such as `.github/` so
/// dotfile config stays searchable, skips VCS metadata directories below `base`, and pins the walk
/// to the starting filesystem so symlinked mounts don't escape.
pub(crate) fn walk_files(base: &Path) -> impl Iterator<Item = ignore::DirEntry> {
    ignore::WalkBuilder::new(base)
        .hidden(false)
        .filter_entry(|e| e.depth() == 0 || !VCS_DIRS.iter().any(|d| e.file_name() == *d))
        .same_file_system(true)
        .max_depth(Some(MAX_WALK_DEPTH))
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
}

/// Reads the entry's modification time for sorting / display.
///
/// Falls back to [`SystemTime::UNIX_EPOCH`] when stat fails (entry vanished, permission denied,
/// platform without mtime). Callers use the result only for ordering, where the epoch sorts
/// unreadable entries oldest.
pub(crate) fn entry_mtime(entry: &ignore::DirEntry) -> SystemTime {
    entry
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

// ── Formatting ──

#[expect(
    clippy::cast_precision_loss,
    reason = "MB display tolerates minor precision loss at > 2^53 bytes; file size caps are nowhere near that"
)]
pub(crate) fn bytes_to_mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Caps a line at `max_chars` characters, appending `... [N chars]` with the original length so the
/// model can decide whether to read more. Borrows when the line already fits.
pub(crate) fn truncate_line(line: &str, max_chars: usize) -> Cow<'_, str> {
    if line.len() <= max_chars {
        return Cow::Borrowed(line);
    }
    let total_chars = line.chars().count();
    if total_chars <= max_chars {
        return Cow::Borrowed(line);
    }
    let boundary = line
        .char_indices()
        .nth(max_chars)
        .map_or(line.len(), |(i, _)| i);
    Cow::Owned(format!(
        "{}{ELLIPSIS} [{total_chars} chars]",
        &line[..boundary]
    ))
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    /// A temp dir with a canonical path, matching the cwd [`super::working_dir`] returns.
    pub(crate) struct CanonicalTempDir {
        _dir: TempDir,
        path: PathBuf,
    }

    impl CanonicalTempDir {
        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    pub(crate) fn canonical_tempdir() -> CanonicalTempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        CanonicalTempDir { _dir: dir, path }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use super::bash::BashTool;
    use super::edit::EditTool;
    use super::glob::GlobTool;
    use super::grep::GrepTool;
    use super::read::ReadTool;
    use super::testing::canonical_tempdir;
    use super::write::WriteTool;
    use super::*;
    use crate::file_tracker::testing::tracker;
    use crate::permission::{Decision, Mode, Policy, Zone};

    fn all_tools() -> Vec<Box<dyn Tool>> {
        let tracker = tracker();
        vec![
            Box::new(BashTool),
            Box::new(EditTool::new(Arc::clone(&tracker))),
            Box::new(GlobTool),
            Box::new(GrepTool),
            Box::new(ReadTool::new(Arc::clone(&tracker))),
            Box::new(WriteTool::new(tracker)),
        ]
    }

    // ── Tool trait contract (all tools) ──

    #[test]
    fn every_tool_exposes_non_empty_name_description_and_object_schema() {
        for t in all_tools() {
            let name = t.name();
            assert!(!name.is_empty(), "tool with empty name in catalog");
            assert!(!t.description().is_empty(), "{name}: empty description");
            let schema = t.input_schema();
            assert_eq!(schema["type"], "object", "{name}: schema type");
            assert!(
                schema["properties"].is_object(),
                "{name}: schema.properties"
            );
            assert!(schema["required"].is_array(), "{name}: schema.required");
        }
    }

    #[test]
    fn tool_catalog_names_and_icons_are_unique() {
        let tools = all_tools();
        let names: HashSet<_> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names.len(), tools.len(), "duplicate name");
        let icons: HashSet<_> = tools.iter().map(|t| t.icon()).collect();
        assert_eq!(icons.len(), tools.len(), "duplicate icon");
    }

    #[test]
    fn tool_catalog_icons_match_the_published_prefix_set() {
        let expected = [
            ("bash", "$"),
            ("edit", "✎"),
            ("glob", "✱"),
            ("grep", "⌕"),
            ("read", "→"),
            ("write", "←"),
        ];
        let tools = all_tools();
        for (name, icon) in expected {
            let t = tools
                .iter()
                .find(|t| t.name() == name)
                .unwrap_or_else(|| panic!("tool {name} missing from catalog"));
            assert_eq!(t.icon(), icon, "tool {name}: expected icon {icon}");
        }
    }

    #[test]
    fn tool_summarize_input_plucks_the_primary_field() {
        let cases = [
            ("bash", serde_json::json!({"command": "ls"}), Some("ls")),
            (
                "edit",
                serde_json::json!({
                    "file_path": "/a/b.rs",
                    "old_string": "x",
                    "new_string": "y",
                }),
                Some("/a/b.rs"),
            ),
            (
                "glob",
                serde_json::json!({"pattern": "**/*.rs"}),
                Some("**/*.rs"),
            ),
            ("grep", serde_json::json!({"pattern": "fn "}), Some("fn ")),
            (
                "read",
                serde_json::json!({"file_path": "/a/b.rs"}),
                Some("/a/b.rs"),
            ),
            (
                "write",
                serde_json::json!({"file_path": "/a/b.rs", "content": "x"}),
                Some("/a/b.rs"),
            ),
        ];
        let tools = all_tools();
        for (name, input, expected) in &cases {
            let t = tools.iter().find(|t| t.name() == *name).unwrap();
            assert_eq!(t.summarize_input(input), *expected, "tool {name}");
        }
    }

    #[test]
    fn tool_summarize_input_is_none_when_primary_field_missing() {
        let tools = all_tools();
        for t in &tools {
            assert!(
                t.summarize_input(&serde_json::json!({})).is_none(),
                "tool {} returned Some on empty input",
                t.name(),
            );
        }
    }

    #[test]
    fn tool_summarize_call_wraps_arg_in_title_cased_name() {
        let cases = [
            ("bash", serde_json::json!({"command": "ls"}), "ls"),
            (
                "edit",
                serde_json::json!({"file_path": "/a/b.rs", "old_string": "x", "new_string": "y"}),
                "Edit(/a/b.rs)",
            ),
            (
                "glob",
                serde_json::json!({"pattern": "**/*.rs"}),
                "Glob(**/*.rs)",
            ),
            ("grep", serde_json::json!({"pattern": "fn "}), "Grep(fn )"),
            (
                "read",
                serde_json::json!({"file_path": "/a/b.rs"}),
                "Read(/a/b.rs)",
            ),
            (
                "write",
                serde_json::json!({"file_path": "/a/b.rs", "content": "x"}),
                "Write(/a/b.rs)",
            ),
        ];
        let tools = all_tools();
        for (name, input, expected) in &cases {
            let t = tools.iter().find(|t| t.name() == *name).unwrap();
            assert_eq!(t.summarize_call(input), *expected, "tool {name}");
        }
    }

    #[test]
    fn tool_summarize_call_falls_back_to_bare_name_when_arg_missing() {
        let tools = all_tools();
        for t in &tools {
            let got = t.summarize_call(&serde_json::json!({}));
            assert_eq!(got, title_case(t.name()), "tool {}", t.name());
        }
    }

    #[test]
    fn tool_summarize_call_shortens_file_paths_inside_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let path = cwd.join("crates/oxide-code/src/tool/read.rs");
        let path = path.to_str().unwrap();
        let cases = [
            (
                "edit",
                serde_json::json!({"file_path": path, "old_string": "x", "new_string": "y"}),
                "Edit(crates/oxide-code/src/tool/read.rs)",
            ),
            (
                "read",
                serde_json::json!({"file_path": path}),
                "Read(crates/oxide-code/src/tool/read.rs)",
            ),
            (
                "write",
                serde_json::json!({"file_path": path, "content": "x"}),
                "Write(crates/oxide-code/src/tool/read.rs)",
            ),
        ];
        let tools = all_tools();
        for (name, input, expected) in &cases {
            let t = tools.iter().find(|t| t.name() == *name).unwrap();
            assert_eq!(t.summarize_call(input), *expected, "tool {name}");
        }
    }

    // ── Tool::permission_class ──

    #[test]
    fn permission_class_rules_cover_every_tool_in_the_class() {
        // Rules address classes: a `read(...)` deny must stop grep and glob from dumping the file
        // it names, and an `edit(...)` deny must stop write as well as edit.
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(cwd.join(".env"), "SECRET=1").unwrap();
        let deny = [
            "read(.env)".to_owned(),
            "edit(.env)".to_owned(),
            "bash(cat .env)".to_owned(),
        ];
        let policy = Policy::resolve(Mode::Auto, &[], &deny, &cwd).unwrap();
        let cases = [
            ("read", serde_json::json!({"file_path": ".env"})),
            ("grep", serde_json::json!({"pattern": ".", "path": ".env"})),
            ("glob", serde_json::json!({"pattern": "*", "path": ".env"})),
            (
                "edit",
                serde_json::json!({"file_path": ".env", "old_string": "a", "new_string": "b"}),
            ),
            (
                "write",
                serde_json::json!({"file_path": ".env", "content": "x"}),
            ),
            ("bash", serde_json::json!({"command": "cat .env"})),
        ];
        let tools = all_tools();
        for (name, input) in &cases {
            let t = tools.iter().find(|t| t.name() == *name).unwrap();
            let target = t.gate_target(input, &cwd);
            assert_eq!(
                policy.decide(t.permission_class(), &target.as_target()),
                Decision::Deny,
                "tool {name}",
            );
        }
    }

    // ── Tool::gate_target ──

    #[test]
    fn gate_target_resolves_the_path_field_of_each_path_tool() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        let tools = all_tools();
        for name in ["read", "edit", "write"] {
            let t = tools.iter().find(|t| t.name() == name).unwrap();
            let target = t.gate_target(&serde_json::json!({"file_path": "a.rs"}), &cwd);
            let GateTarget::Path { relative, .. } = target else {
                panic!("tool {name}: expected a path target, got {target:?}");
            };
            assert_eq!(relative.as_deref(), Some("a.rs"), "tool {name}");

            let missing = t.gate_target(&serde_json::json!({}), &cwd);
            assert!(matches!(missing, GateTarget::None), "tool {name}");
        }
    }

    // ── Tool::pin_target ──

    #[test]
    fn pin_target_rewrites_only_the_path_field() {
        let resolved = GateTarget::Path {
            canonical: "/repo/real.rs".to_owned(),
            relative: Some("real.rs".to_owned()),
            requested: "/repo/link.rs".to_owned(),
            deny_relative: vec!["real.rs".to_owned(), "link.rs".to_owned()],
            zone: Zone::Workspace,
        };
        let mut input = serde_json::json!({"file_path": "link.rs", "content": "link.rs"});
        WriteTool::new(tracker()).pin_target(&mut input, &resolved);
        assert_eq!(
            input,
            serde_json::json!({"file_path": "/repo/real.rs", "content": "link.rs"}),
        );
    }

    #[test]
    fn pin_target_leaves_tools_without_a_path_field_untouched() {
        let resolved = GateTarget::Path {
            canonical: "/repo".to_owned(),
            relative: Some(String::new()),
            requested: "/repo".to_owned(),
            deny_relative: vec![String::new()],
            zone: Zone::Workspace,
        };
        let mut input = serde_json::json!({"pattern": "x", "path": "."});
        GrepTool.pin_target(&mut input, &resolved);
        assert_eq!(input, serde_json::json!({"pattern": "x", "path": "."}));
    }

    // ── Tool::approval_preview ──

    #[test]
    fn approval_preview_default_titles_with_the_tool_name() {
        // The body carries the full command, wrapped, so the title only names the tool.
        let preview = BashTool.approval_preview(&serde_json::json!({"command": "cargo test"}));
        assert_eq!(preview.title, "Bash");
        assert_eq!(preview.body, ApprovalBody::Command("cargo test".to_owned()));
    }

    // ── title_case ──

    #[test]
    fn title_case_capitalizes_first_char_only() {
        assert_eq!(title_case("grep"), "Grep");
        assert_eq!(title_case("bash"), "Bash");
        assert_eq!(title_case("Foo"), "Foo");
        assert_eq!(title_case(""), "");
        assert_eq!(title_case("a"), "A");
    }

    // ── ToolRegistry::get ──

    #[test]
    fn get_finds_registered_tool() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool)]);
        assert_eq!(registry.get("bash").unwrap().name(), "bash");
    }

    #[test]
    fn get_unknown_tool() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool)]);
        assert!(registry.get("nonexistent").is_none());
    }

    // ── ToolRegistry::definitions ──

    #[test]
    fn definitions_produces_tool_with_valid_schema() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool)]);
        let defs = registry.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "bash");
        let schema = &defs[0].input_schema;
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["command"].is_object());
        assert_eq!(schema["required"], serde_json::json!(["command"]));
    }

    // ── ToolRegistry::icon ──

    #[test]
    fn icon_delegates_to_registered_tool() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool)]);
        assert_eq!(registry.icon("bash"), "$");
    }

    #[test]
    fn icon_unknown_tool_falls_back_to_default() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool)]);
        assert_eq!(registry.icon("nonexistent"), DEFAULT_TOOL_ICON);
    }

    // ── ToolRegistry::label ──

    #[test]
    fn label_delegates_to_registered_tool() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool), Box::new(GrepTool)]);
        assert_eq!(
            registry.label("bash", &serde_json::json!({"command": "echo hi"})),
            "echo hi",
        );
        assert_eq!(
            registry.label("grep", &serde_json::json!({"pattern": "fn "})),
            "Grep(fn )",
        );
    }

    #[test]
    fn label_unknown_tool_falls_back_to_raw_name() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool)]);
        let input = serde_json::json!({"command": "echo hi"});
        assert_eq!(registry.label("nonexistent", &input), "nonexistent");
    }

    // ── ToolRegistry::run ──

    fn oversize_grep_input() -> (tempfile::TempDir, serde_json::Value) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let line = "x".repeat(400);
        std::fs::write(&path, format!("{line}\n").repeat(500)).unwrap();
        let input = serde_json::json!({"pattern": "x", "path": path, "head_limit": 0});
        (dir, input)
    }

    #[tokio::test]
    async fn run_dispatches_and_caps_byte_overflow() {
        let (_dir, input) = oversize_grep_input();
        let registry = ToolRegistry::new(vec![Box::new(GrepTool)]);
        let output = registry.run("grep", input).await;

        assert!(!output.is_error);
        assert!(output.content.len() <= MAX_OUTPUT_BYTES + TRUNCATION_OVERHEAD);
        assert!(output.content.contains("bytes truncated; head + tail kept"));
        assert!(output.metadata.truncated_bytes.is_some());
        assert!(output.metadata.truncated_total.is_none());
    }

    #[tokio::test]
    async fn run_within_cap_leaves_content_and_metadata_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();

        let registry = ToolRegistry::new(vec![Box::new(ReadTool::new(tracker()))]);
        let output = registry
            .run(
                "read",
                serde_json::json!({"file_path": path.to_str().unwrap()}),
            )
            .await;

        assert!(!output.is_error);
        assert_eq!(output.content, "1\thello\n2\tworld");
        assert!(output.metadata.truncated_bytes.is_none());
    }

    #[tokio::test]
    async fn run_byte_cap_on_grep_falls_through_to_text_view() {
        let (_dir, input) = oversize_grep_input();
        let registry = ToolRegistry::new(vec![Box::new(GrepTool)]);
        let output = registry.run("grep", input.clone()).await;
        assert!(output.metadata.truncated_bytes.is_some());

        let view = registry.result_view(
            "grep",
            &input,
            &output.content,
            &output.metadata,
            output.is_error,
        );
        assert!(matches!(view, ToolResultView::Text { .. }));
    }

    #[tokio::test]
    async fn run_unknown_tool_errors() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool)]);
        let output = registry.run("nonexistent", serde_json::json!({})).await;
        assert!(output.is_error);
        assert_eq!(output.content, "Unknown tool: nonexistent");
        assert!(output.metadata.truncated_bytes.is_none());
    }

    // ── ToolRegistry::result_view ──

    #[test]
    fn result_view_delegates_to_tool_for_structured_output() {
        let registry = ToolRegistry::new(vec![Box::new(EditTool::new(tracker()))]);
        let input = serde_json::json!({
            "file_path": "/tmp/f.rs",
            "old_string": "a",
            "new_string": "b",
        });
        let metadata = ToolMetadata::default();
        let view = registry.result_view(
            "edit",
            &input,
            "Successfully edited /tmp/f.rs.",
            &metadata,
            false,
        );
        assert_eq!(
            view,
            ToolResultView::Diff {
                chunks: vec![DiffChunk {
                    old: vec![DiffLine {
                        number: 1,
                        text: "a".to_owned()
                    }],
                    new: vec![DiffLine {
                        number: 1,
                        text: "b".to_owned()
                    }],
                }],
                replace_all: false,
                replacements: 1,
            },
        );
    }

    #[test]
    fn result_view_delegates_grep_matches() {
        let registry = ToolRegistry::new(vec![Box::new(GrepTool)]);
        let input = serde_json::json!({"pattern": "fn"});
        let metadata = ToolMetadata::default();
        let view =
            registry.result_view("grep", &input, "src/main.rs:10:fn main()", &metadata, false);
        assert_eq!(
            view,
            ToolResultView::GrepMatches {
                groups: vec![GrepFileGroup {
                    path: "src/main.rs".to_owned(),
                    lines: vec![GrepMatchLine {
                        number: 10,
                        text: "fn main()".to_owned(),
                        is_match: true,
                    }],
                }],
                truncated: false,
            },
        );
    }

    #[test]
    fn result_view_delegates_read_excerpt() {
        let registry = ToolRegistry::new(vec![Box::new(ReadTool::new(tracker()))]);
        let input = serde_json::json!({"file_path": "/tmp/lib.rs"});
        let metadata = ToolMetadata::default();
        let view = registry.result_view("read", &input, "1\tmod foo;", &metadata, false);
        assert_eq!(
            view,
            ToolResultView::ReadExcerpt {
                path: "/tmp/lib.rs".to_owned(),
                lines: vec![ReadExcerptLine {
                    number: 1,
                    text: "mod foo;".to_owned(),
                }],
                total_lines: 1,
            },
        );
    }

    #[test]
    fn result_view_delegates_glob_files() {
        let registry = ToolRegistry::new(vec![Box::new(GlobTool)]);
        let input = serde_json::json!({"pattern": "*.rs"});
        let metadata = ToolMetadata::default();
        let view =
            registry.result_view("glob", &input, "src/main.rs\nsrc/lib.rs", &metadata, false);
        assert_eq!(
            view,
            ToolResultView::GlobFiles {
                pattern: "*.rs".to_owned(),
                files: vec!["src/main.rs".to_owned(), "src/lib.rs".to_owned()],
                total: 2,
            },
        );
    }

    #[test]
    fn result_view_short_circuits_errors_to_text() {
        let registry = ToolRegistry::new(vec![Box::new(EditTool::new(tracker()))]);
        let input = serde_json::json!({
            "file_path": "/tmp/f.rs",
            "old_string": "a",
            "new_string": "b",
        });
        let metadata = ToolMetadata::default();
        let view = registry.result_view("edit", &input, "old_string not found", &metadata, true);
        assert_eq!(
            view,
            ToolResultView::Text {
                content: "old_string not found".to_owned(),
            },
        );
    }

    #[test]
    fn result_view_falls_back_to_text_for_unknown_tool() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool)]);
        let metadata = ToolMetadata::default();
        let view = registry.result_view(
            "nonexistent",
            &serde_json::json!({}),
            "anything",
            &metadata,
            false,
        );
        assert_eq!(
            view,
            ToolResultView::Text {
                content: "anything".to_owned(),
            },
        );
    }

    #[test]
    fn result_view_falls_back_to_text_when_tool_has_no_structured_view() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool)]);
        let metadata = ToolMetadata::default();
        let view = registry.result_view(
            "bash",
            &serde_json::json!({"command": "ls"}),
            "file1\nfile2",
            &metadata,
            false,
        );
        assert_eq!(
            view,
            ToolResultView::Text {
                content: "file1\nfile2".to_owned(),
            },
        );
    }

    // ── cap_output ──

    #[test]
    fn cap_output_short_content_unchanged() {
        let (out, original) = cap_output("hello".to_owned());
        assert_eq!(out, "hello");
        assert!(original.is_none());
    }

    #[test]
    fn cap_output_keeps_head_and_tail() {
        let head = "HEAD_SENTINEL\n";
        let tail = "TAIL_SENTINEL\n";
        let filler_len = MAX_OUTPUT_BYTES * 2 - head.len() - tail.len();

        let mut content = String::with_capacity(head.len() + filler_len + tail.len());
        content.push_str(head);
        content.extend(std::iter::repeat_n('x', filler_len));
        content.push_str(tail);
        let original_len = content.len();

        let (out, original) = cap_output(content);

        assert!(out.starts_with(head));
        assert!(out.ends_with(tail));
        assert!(out.contains("bytes truncated; head + tail kept"));
        assert!(out.len() <= MAX_OUTPUT_BYTES + TRUNCATION_OVERHEAD);
        let sep_pos = out.find("bytes truncated").unwrap();
        assert!(sep_pos > head.len());
        assert!(sep_pos < out.len() - tail.len());
        assert_eq!(original, Some(original_len));
    }

    #[test]
    fn cap_output_multibyte_at_split_boundary() {
        let half = MAX_OUTPUT_BYTES / 2;
        let emoji = "🦀"; // 4 bytes
        let prefix_len = half - 2;

        let mut content = String::new();
        content.push_str(&"a".repeat(prefix_len));
        content.push_str(emoji);
        content.push_str(&"b".repeat(MAX_OUTPUT_BYTES * 2));

        let (out, original) = cap_output(content);

        assert!(out.contains("bytes truncated"));
        assert!(out.starts_with("aaaa"));
        assert!(out.ends_with('b'));
        assert!(!out.contains(emoji));
        assert!(original.is_some());
    }

    #[test]
    fn cap_output_barely_over_limit_unchanged() {
        let original = "a".repeat(MAX_OUTPUT_BYTES + 1);
        let (out, original_len) = cap_output(original.clone());
        assert_eq!(out, original);
        assert!(original_len.is_none());
    }

    // ── resolve_base_dir ──

    #[test]
    fn resolve_base_dir_canonicalizes_relative_and_absolute_paths() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join("lib")).unwrap();
        let lib = dir.path().join("lib");

        assert_eq!(
            resolve_base_dir(Some("./src/../lib"), dir.path()),
            Ok(lib.clone())
        );
        assert_eq!(
            resolve_base_dir(Some(lib.to_str().unwrap()), Path::new("/")),
            Ok(lib)
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_base_dir_resolves_dot_dot_after_a_symlink_physically() {
        let dir = canonical_tempdir();
        std::fs::create_dir_all(dir.path().join("real/target")).unwrap();
        let cwd = dir.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real/target"), cwd.join("link")).unwrap();

        assert_eq!(
            resolve_base_dir(Some("link/.."), &cwd),
            Ok(dir.path().join("real"))
        );
    }

    #[test]
    fn resolve_base_dir_none_uses_cwd() {
        let dir = canonical_tempdir();
        assert_eq!(
            resolve_base_dir(None, dir.path()),
            Ok(dir.path().to_path_buf())
        );
    }

    #[test]
    fn resolve_base_dir_missing_path_is_named_relative_to_cwd() {
        let dir = canonical_tempdir();
        let err = resolve_base_dir(Some("missing"), dir.path()).unwrap_err();
        assert!(err.starts_with("Cannot access missing: "), "{err}");
    }

    // ── display_cwd_path_from ──

    #[test]
    fn display_cwd_path_from_without_cwd_falls_back_to_original_path() {
        assert_eq!(
            display_cwd_path_from("/tmp/example.rs", None),
            "/tmp/example.rs"
        );
    }

    // ── tracker_key ──

    #[tokio::test]
    async fn tracker_key_resolves_relative_and_dotted_spellings_to_one_key() {
        let absolute = std::env::current_dir().unwrap().join("src/main.rs");
        assert_eq!(tracker_key("src/main.rs").await, absolute);
        assert_eq!(tracker_key("./src/../src/main.rs").await, absolute);
        assert_eq!(tracker_key(absolute.to_str().unwrap()).await, absolute);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tracker_key_resolves_a_symlinked_spelling_to_the_target() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::fs::write(dir.path().join("real/a.rs"), "").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();

        let via_link = dir.path().join("link/a.rs");
        assert_eq!(
            tracker_key(via_link.to_str().unwrap()).await,
            dir.path().join("real/a.rs"),
        );
    }

    #[tokio::test]
    async fn tracker_key_missing_path_falls_back_to_a_lexically_clean_absolute_path() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(tracker_key("missing/../a.rs").await, cwd.join("a.rs"));
    }

    // ── display_path ──

    #[test]
    fn display_path_relative_inside_base() {
        let base = Path::new("/home/user/project");
        let path = Path::new("/home/user/project/src/main.rs");
        assert_eq!(display_path(path, base), "src/main.rs");
    }

    #[test]
    fn display_path_outside_base_stays_absolute() {
        let base = Path::new("/home/user/project");
        let path = Path::new("/etc/config.toml");
        assert_eq!(display_path(path, base), "/etc/config.toml");
    }

    #[test]
    fn display_path_same_path_is_filename() {
        let base = Path::new("/home/user/project/src/main.rs");
        let path = Path::new("/home/user/project/src/main.rs");
        assert_eq!(display_path(path, base), "main.rs");
    }

    // ── file_name ──

    #[test]
    fn file_name_extracts_basename() {
        assert_eq!(file_name("/home/user/project/src/main.rs"), "main.rs");
    }

    #[test]
    fn file_name_bare_name_unchanged() {
        assert_eq!(file_name("README.md"), "README.md");
    }

    // ── read_validated ──

    #[cfg(unix)]
    #[tokio::test]
    async fn read_validated_reads_a_regular_file_but_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "original").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let accept = |_: &std::fs::Metadata| Ok(());
        let (metadata, bytes) = read_validated(target.to_str().unwrap(), accept)
            .await
            .unwrap();
        assert_eq!(
            (metadata.len(), bytes.as_slice()),
            (8, b"original".as_slice())
        );
        assert!(
            read_validated(link.to_str().unwrap(), accept)
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_validated_rechecks_a_file_swapped_after_the_path_check() {
        // Each swap runs once the path check passes, as a racing writer's would: a larger file the
        // size limit must catch, then a fifo the open must not block on.
        fn grow(path: &Path) {
            let staged = path.with_extension("new");
            std::fs::write(&staged, "SECRET=1").unwrap();
            std::fs::rename(&staged, path).unwrap();
        }
        fn fifo(path: &Path) {
            std::fs::remove_file(path).unwrap();
            nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRWXU).unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        for swap in [grow as fn(&Path), fifo] {
            std::fs::write(&path, "ok").unwrap();
            let swapped = std::cell::Cell::new(false);
            let validate = |meta: &std::fs::Metadata| {
                if !meta.is_file() || meta.len() > 4 {
                    return Err("rejected".to_owned());
                }
                if !swapped.replace(true) {
                    swap(&path);
                }
                Ok(())
            };
            let err = read_validated(path.to_str().unwrap(), validate)
                .await
                .unwrap_err();
            assert_eq!(err, "rejected");
        }
    }

    // ── write_no_follow ──

    #[tokio::test]
    async fn write_no_follow_overwrite_truncates_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "a much longer original").unwrap();

        write_no_follow(&path, b"short", false).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_no_follow_create_new_refuses_existing_entries_and_dangling_links() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.txt");
        std::fs::write(&existing, "original").unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(write_no_follow(&existing, b"x", true).await.is_err());
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(write_no_follow(&link, b"x", true).await.is_err());
        assert!(!target.exists(), "the link target must not be created");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_no_follow_overwrite_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "original").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(write_no_follow(&link, b"x", false).await.is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }

    // ── truncate_line ──

    const MAX_CHARS: usize = 500;

    #[test]
    fn truncate_line_short_unchanged() {
        assert_eq!(truncate_line("hello", MAX_CHARS).as_ref(), "hello");
    }

    #[test]
    fn truncate_line_long_gets_truncated() {
        let long_line = "x".repeat(MAX_CHARS + 100);
        let result = truncate_line(&long_line, MAX_CHARS);
        let suffix = format!("... [{} chars]", MAX_CHARS + 100);
        assert_eq!(result.len(), MAX_CHARS + suffix.len());
        assert_eq!(&result[..MAX_CHARS], "x".repeat(MAX_CHARS));
        assert_eq!(&result[MAX_CHARS..], suffix);
    }

    #[test]
    fn truncate_line_multibyte_safe() {
        let mut line = "a".repeat(MAX_CHARS - 2);
        line.push('🦀');
        line.push_str(&"b".repeat(100));
        let result = truncate_line(&line, MAX_CHARS);

        let (prefix, suffix) = result.split_once("... [").unwrap();
        let expected_prefix = "a".repeat(MAX_CHARS - 2) + "🦀" + "b";
        assert_eq!(prefix, expected_prefix);
        assert_eq!(prefix.chars().count(), MAX_CHARS);
        assert_eq!(suffix, format!("{} chars]", MAX_CHARS + 99));
    }

    #[test]
    fn truncate_line_multibyte_under_char_cap_unchanged() {
        let line = "é".repeat(MAX_CHARS);
        let result = truncate_line(&line, MAX_CHARS);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result.as_ref(), line);
    }
}
