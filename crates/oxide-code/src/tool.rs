//! The [`Tool`] trait, the [`ToolRegistry`] that dispatches calls and caps their output, the result
//! views the TUI renders, and helpers shared by the built-in tools.

pub(crate) mod bash;
pub(crate) mod edit;
pub(crate) mod glob;
pub(crate) mod grep;
pub(crate) mod read;
pub(crate) mod tasks;
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
use tokio_util::sync::CancellationToken;

use self::tasks::Task;
use crate::file_tracker::{FileTracker, TrackableFileError, validate_trackable_file};
use crate::permission::{Access, PermissionClass, Workspace};
use crate::util::path::{lexical_normalize, slash_separated};
use crate::util::text::{ELLIPSIS, plural};

// ── Tool Spec ──

/// What the model and the gate know of a tool. The registry takes it once, when the tool registers.
#[derive(Debug, Clone)]
pub(crate) struct ToolSpec {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: serde_json::Value,
    /// The permission class the gate files the tool's calls under.
    pub(crate) class: PermissionClass,
    /// Whether a call may run alongside the other parallel-safe calls of its round, since it
    /// changes nothing another call reads.
    pub(crate) parallel_safe: bool,
}

impl ToolSpec {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

/// Tool schema exposed to the Anthropic API.
#[derive(Clone, Serialize)]
pub(crate) struct ToolDefinition {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: serde_json::Value,
}

// ── Tool Context ──

/// The context a call prepares and runs in. Its workspace, session ID, and tracker are shared with
/// the session's other calls.
#[derive(Clone)]
pub(crate) struct ToolCtx {
    pub(crate) workspace: Arc<Workspace>,
    pub(crate) session_id: String,
    pub(crate) tracker: Arc<FileTracker>,
    /// The tool-use ID the model gave the call, which the tracker attributes the call's records to.
    pub(crate) call_id: String,
    /// Cancelled when the user interrupts the call, so work its dropped future leaves behind, such
    /// as a blocking file walk, stops early.
    pub(crate) cancel: CancellationToken,
}

impl ToolCtx {
    pub(crate) fn cwd(&self) -> &Path {
        self.workspace.cwd()
    }
}

/// A call resolved for the gate and its run: the input with any path pinned to where it resolves,
/// and what the call accesses.
#[derive(Debug, Clone)]
pub(crate) struct Prepared {
    pub(crate) input: serde_json::Value,
    pub(crate) access: Access,
}

// ── Tool Output ──

/// Result returned from a tool execution to the agent loop.
pub(crate) struct ToolOutput {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) metadata: ToolMetadata,
    /// The task list a `tasks` call leaves, which the engine adopts in place of its own.
    pub(crate) tasks: Option<Vec<Task>>,
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
            tasks: None,
        }
    }

    pub(crate) fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: ToolMetadata::default(),
            tasks: None,
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

    pub(crate) fn with_tasks(mut self, tasks: Vec<Task>) -> Self {
        self.tasks = Some(tasks);
        self
    }
}

// ── Tool Result View ──

/// Per-tool structured shape of a completed tool call's body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
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
    /// The task list a `tasks` call sent, before a finished list clears.
    Tasks {
        tasks: Vec<Task>,
    },
}

/// One line in a `ReadExcerpt` view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GrepFileGroup {
    pub(crate) path: String,
    pub(crate) lines: Vec<GrepMatchLine>,
}

/// One row in a [`GrepFileGroup`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GrepMatchLine {
    pub(crate) number: usize,
    pub(crate) text: String,
    pub(crate) is_match: bool,
}

// ── Approval Preview ──

/// What a pending approval shows the user. The body is pre-rendered by the tool's gate seam so the
/// modal stays display-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ApprovalPreview {
    /// Tool display label, e.g. `Bash(cargo test)` or `Write(src/main.rs)`.
    pub(crate) title: String,
    /// Body lines: an edit / write diff or the command string, capped to a display height when the
    /// modal flattens it for render.
    pub(crate) body: ApprovalBody,
}

/// The renderable body of an [`ApprovalPreview`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(crate) enum ApprovalBody {
    /// A shell command string (`bash`).
    Command(String),
    /// A unified diff (`edit` / `write`), reusing the chat's `+` / `-` chunk shape.
    Diff(Vec<DiffChunk>),
}

// ── Tool Trait ──

/// A tool that the agent can invoke.
///
/// Per-instance metadata (spec, icon, input summary) lives on the trait, so adding a tool means
/// editing only its module.
pub(crate) trait Tool: Send + Sync {
    /// The tool's name, description, schema, permission class, and parallel safety. The class has
    /// no default, so a new tool cannot silently inherit a permissive one.
    fn spec(&self) -> ToolSpec;

    /// The input field naming the file this call acts on, if any, which the default
    /// [`Self::prepare`] resolves.
    fn path_field(&self) -> Option<&'static str> {
        None
    }

    /// Resolves what the call accesses from `input`, once, in the context's workspace. The default
    /// resolves [`Self::path_field`] and pins it in the input to the resolved path, so the tool
    /// opens what the gate checked, though not past a symlink swapped into an intermediate
    /// directory afterward. Without a path field the call accesses [`Access::None`], which only a
    /// class-wide rule matches.
    fn prepare(&self, mut input: serde_json::Value, ctx: &ToolCtx) -> Prepared {
        let access = self
            .path_field()
            .and_then(|field| extract_input_field(&input, field))
            .map(|path| Access::for_path(path, &ctx.workspace))
            .unwrap_or_default();
        if let (Some(field), Access::Path { canonical, .. }) = (self.path_field(), &access)
            && let Some(value) = input.get_mut(field)
        {
            *value = serde_json::Value::String(canonical.clone());
        }
        Prepared { input, access }
    }

    fn icon(&self) -> &'static str {
        DEFAULT_TOOL_ICON
    }

    /// Pulls the tool's primary argument out of `input` for terse `Name(arg)` labels in the UI.
    /// Returning `None` falls back to the bare name.
    fn summarize_input<'a>(&self, _input: &'a serde_json::Value) -> Option<&'a str> {
        None
    }

    /// The label the UI shows for a call of the tool registered as `name`, with a path argument
    /// shown relative to `cwd`.
    fn summarize_call(&self, name: &str, input: &serde_json::Value, _cwd: &Path) -> String {
        let label = pascal_case(name);
        match self.summarize_input(input) {
            Some(arg) => format!("{label}({arg})"),
            None => label,
        }
    }

    /// Returns the structured shape of a successful tool result for richer UI rendering, with
    /// paths shown relative to `cwd`, or `None` to fall back to [`ToolResultView::Text`]. Errors
    /// short-circuit to text in the registry.
    fn result_view(
        &self,
        _input: &serde_json::Value,
        _content: &str,
        _metadata: &ToolMetadata,
        _cwd: &Path,
    ) -> Option<ToolResultView> {
        None
    }

    /// Builds the approval modal's preview of the prepared `input`: the tool name over the
    /// [`Self::summarize_call`] label by default, a diff for edit and write, or `Bash (background)`
    /// over a background command.
    fn approval_preview(
        &self,
        name: &str,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> ApprovalPreview {
        ApprovalPreview {
            title: pascal_case(name),
            body: ApprovalBody::Command(self.summarize_call(name, input, cwd)),
        }
    }

    /// Executes the prepared call. A user cancel can drop the future at any await and cancels
    /// [`ToolCtx::cancel`], so work it started must either stop with the drop, as a bash process
    /// tree does, stop on the cancel, as a blocking walk does, or finish together with its
    /// bookkeeping, as a file write and its tracker record do.
    fn run<'a>(
        &'a self,
        prepared: Prepared,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;
}

pub(crate) const DEFAULT_TOOL_ICON: &str = "⟡";

pub(crate) fn extract_input_field<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(serde_json::Value::as_str)
}

/// Renders a tool name as the label the UI shows, `bash_output` as `BashOutput`.
fn pascal_case(name: &str) -> String {
    let mut label = String::with_capacity(name.len());
    for word in name.split('_') {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            label.extend(first.to_uppercase());
            label.push_str(chars.as_str());
        }
    }
    label
}

// ── Tool Registry ──

/// Owns the set of [`Tool`]s available to the agent, each with the spec it registered with, and
/// dispatches calls by name.
///
/// The registry is built once at session start with the tools the model is allowed to invoke.
/// Lookups are linear over a small `Vec` (registry size is fixed at < 10), so no map is needed.
pub(crate) struct ToolRegistry {
    tools: Vec<(ToolSpec, Box<dyn Tool>)>,
}

impl ToolRegistry {
    pub(crate) fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self {
            tools: tools.into_iter().map(|tool| (tool.spec(), tool)).collect(),
        }
    }

    pub(crate) fn get(&self, name: &str) -> Option<(&ToolSpec, &dyn Tool)> {
        self.tools
            .iter()
            .find(|(spec, _)| spec.name == name)
            .map(|(spec, tool)| (spec, tool.as_ref()))
    }

    pub(crate) fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|(spec, _)| spec.definition())
            .collect()
    }

    pub(crate) fn icon(&self, name: &str) -> &'static str {
        self.get(name)
            .map_or(DEFAULT_TOOL_ICON, |(_, tool)| tool.icon())
    }

    /// The label a call shows, with a path argument relative to `cwd`.
    pub(crate) fn label(&self, name: &str, input: &serde_json::Value, cwd: &Path) -> String {
        self.get(name).map_or_else(
            || name.to_owned(),
            |(_, tool)| tool.summarize_call(name, input, cwd),
        )
    }

    /// Resolves the structured result view for a tool call, with paths relative to `cwd`. Errors
    /// short-circuit to [`ToolResultView::Text`] regardless of the tool's preference, so failure
    /// messages render consistently across the chat. Unknown tool names also fall back to text.
    pub(crate) fn result_view(
        &self,
        name: &str,
        input: &serde_json::Value,
        content: &str,
        metadata: &ToolMetadata,
        is_error: bool,
        cwd: &Path,
    ) -> ToolResultView {
        if is_error {
            return ToolResultView::Text {
                content: content.to_owned(),
            };
        }
        self.get(name)
            .and_then(|(_, tool)| tool.result_view(input, content, metadata, cwd))
            .unwrap_or_else(|| ToolResultView::Text {
                content: content.to_owned(),
            })
    }
}

// ── Output Cap ──

pub(crate) const MAX_OUTPUT_BYTES: usize = 128 * 1024;

const TRUNCATION_OVERHEAD: usize = 80;

/// Runs the prepared call on `tool` and enforces the [`MAX_OUTPUT_BYTES`] byte safety net on its
/// content, recording the pre-cap length on `metadata.truncated_bytes` if trimming fired. Per-tool
/// row caps run inside each tool, so this is the last-line defense against pathological output.
pub(crate) async fn run_capped(tool: &dyn Tool, prepared: Prepared, ctx: &ToolCtx) -> ToolOutput {
    let mut output = tool.run(prepared, ctx).await;
    let (content, original_len) = cap_output(output.content);
    output.content = content;
    if let Some(len) = original_len {
        output.metadata.truncated_bytes = Some(len);
    }
    output
}

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
    serde_json::from_value(raw).map_err(|e| ToolOutput::error(invalid_input(&e)))
}

fn invalid_input(error: &serde_json::Error) -> String {
    format!("Invalid input: {error}")
}

// ── Path Utilities ──

/// Prepares a search tool's call, resolving its `path` argument, the cwd by default, as the root
/// the gate checks and the walk covers.
fn prepare_search_root(input: serde_json::Value, ctx: &ToolCtx) -> Prepared {
    let path = extract_input_field(&input, "path").unwrap_or(".");
    let access = Access::for_path(path, &ctx.workspace);
    Prepared { input, access }
}

/// Where a search tool's walk starts, which its prepared call resolved.
struct SearchRoot {
    /// The resolved root, which exists.
    base: PathBuf,
    /// The root as the call spells it, for the read deny and ask rules each found file faces.
    spelled: PathBuf,
    /// The canonical working directory, so hits under the canonical base render relative to it.
    cwd: PathBuf,
    /// The session's workspace, whose anchor directories those rules apply below.
    workspace: Arc<Workspace>,
}

impl SearchRoot {
    /// The root `access` resolved for a search in `workspace` whose call spells it as `path`, the
    /// cwd by default. The root must exist.
    fn new(access: &Access, path: Option<&str>, workspace: Arc<Workspace>) -> Result<Self, String> {
        let cwd = workspace.cwd();
        let spelled = PathBuf::from(path.unwrap_or("."));
        let Some(base) = access.resolved() else {
            return Err(format!(
                "Cannot access {}: the search root was not resolved.",
                display_path(&spelled, cwd)
            ));
        };
        if let Err(e) = std::fs::metadata(&base) {
            return Err(format!(
                "Cannot access {}: {e}",
                display_path(&cwd.join(&spelled), cwd)
            ));
        }
        let cwd = dunce::canonicalize(cwd)
            .map_err(|e| format!("Failed to get working directory: {e}"))?;
        Ok(Self {
            base,
            spelled,
            cwd,
            workspace,
        })
    }
}

/// The [`FileTracker`] key for the absolute `path` a file tool acts on. An existing path
/// canonicalizes, so dotted and symlinked spellings of one file share an entry. A missing path
/// falls back to its lexically clean form.
async fn tracker_key(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path).await.map_or_else(
        |_| lexical_normalize(path),
        |path| dunce::simplified(&path).to_path_buf(),
    )
}

/// Renders a `Name(path)` summary for a call of the tool registered as `name` whose primary
/// argument is a filesystem path.
///
/// `path_key` names the JSON field to extract from `input` (e.g. `"file_path"`, `"path"`).
/// Falls back to the bare tool label when the field is absent or non-string. The path renders
/// through [`display_path`] so it appears relative to `cwd`.
fn summarize_path_call(
    name: &str,
    input: &serde_json::Value,
    path_key: &str,
    cwd: &Path,
) -> String {
    let label = pascal_case(name);
    match extract_input_field(input, path_key) {
        Some(path) => format!("{label}({})", display_path(Path::new(path), cwd)),
        None => label,
    }
}

/// Renders `path` for chat display.
///
/// Strips `base` if it's a prefix. When `path == base` the relative path is empty, so this returns
/// the file name (`foo.rs`). Paths outside `base` render as their full string. Either form is
/// [`slash_separated`], and lossy UTF-8 conversion is fine since the result is display-only.
pub(crate) fn display_path(path: &Path, base: &Path) -> String {
    let shown = match path.strip_prefix(base) {
        Ok(rel) if rel.as_os_str().is_empty() => path.file_name().map_or(path, Path::new),
        Ok(rel) => rel,
        Err(_) => path,
    };
    slash_separated(shown)
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
/// the opened file, so a file swapped in between cannot skip the check. The open refuses a symlink
/// at the final component and never blocks on a fifo, as [`open_no_follow`] describes. Errors name
/// the file as `shown`.
async fn read_validated(
    path: &Path,
    shown: &str,
    validate: impl Fn(&std::fs::Metadata) -> Result<(), String>,
) -> Result<(std::fs::Metadata, Vec<u8>), String> {
    let error = |e: std::io::Error| format!("Error reading {shown}: {e}");
    validate(&tokio::fs::metadata(path).await.map_err(error)?)?;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    let mut file = open_no_follow(path, options).await.map_err(error)?;
    let metadata = file.metadata().await.map_err(error)?;
    validate(&metadata)?;

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.map_err(error)?;
    Ok((metadata, bytes))
}

// ── File Writing ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteMode {
    CreateNew,
    Overwrite,
}

/// Writes `bytes` through [`open_no_follow`], refusing existing entries in `CreateNew` mode and
/// truncating only after a successful open in `Overwrite` mode.
async fn write_no_follow(path: &Path, bytes: &[u8], mode: WriteMode) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options
        .write(true)
        .create_new(matches!(mode, WriteMode::CreateNew));
    let mut file = open_no_follow(path, options).await?;

    if matches!(mode, WriteMode::Overwrite) {
        file.set_len(0).await?;
    }
    file.write_all(bytes).await?;
    file.flush().await
}

/// Writes `bytes` to the absolute `path` like [`write_no_follow`] and records them in the context's
/// tracker under the path's [`tracker_key`] as one unit, so a caller dropped mid-write cannot leave
/// the file written but unrecorded.
async fn write_tracked(
    ctx: &ToolCtx,
    path: &Path,
    bytes: Vec<u8>,
    mode: WriteMode,
) -> std::io::Result<()> {
    let path = path.to_owned();
    ctx.tracker
        .write_and_record(&ctx.call_id, async move {
            write_no_follow(&path, &bytes, mode).await?;
            Ok((tracker_key(&path).await, bytes))
        })
        .await
}

// ── No-Follow Open ──

/// Opens `path` without following a symlink at the final component, so a link swapped in after the
/// gate resolved the path cannot redirect the call, and without blocking on a fifo.
async fn open_no_follow(
    path: &Path,
    options: std::fs::OpenOptions,
) -> std::io::Result<tokio::fs::File> {
    let path = path.to_owned();
    let file =
        tokio::task::spawn_blocking(move || open_no_follow_blocking(&path, options)).await??;
    Ok(tokio::fs::File::from_std(file))
}

#[cfg(unix)]
fn open_no_follow_blocking(
    path: &Path,
    mut options: std::fs::OpenOptions,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    use nix::fcntl::OFlag;

    options
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)
}

/// `FILE_FLAG_OPEN_REPARSE_POINT` opens a reparse point itself, so a symlink or junction there
/// fails the open as `O_NOFOLLOW` makes it fail on Unix. Any other reparse point, such as a
/// deduplicated file, reads only through the file system filter the flag bypasses, so it opens
/// again without the flag and keeps that handle only when it reaches the same file.
#[cfg(windows)]
fn open_no_follow_blocking(
    path: &Path,
    mut options: std::fs::OpenOptions,
) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    let file = options
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return Ok(file);
    }
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "the final path component is a symlink",
        ));
    }

    let reopened = options.custom_flags(0).open(path)?;
    if file_id(&reopened)? == file_id(&file)? {
        Ok(reopened)
    } else {
        Err(std::io::Error::other(
            "the file changed while it was opened",
        ))
    }
}

/// The volume and file index that identify a file across handles.
#[cfg(windows)]
fn file_id(file: &std::fs::File) -> std::io::Result<(u64, u64)> {
    let info = winapi_util::file::information(file)?;
    Ok((info.volume_serial_number(), info.file_index()))
}

// ── File Walking ──

const MAX_WALK_DEPTH: usize = 64;

const VCS_DIRS: &[&str] = &[".bzr", ".git", ".hg", ".jj", ".sl", ".svn"];

/// Recursively walks `base`, yielding regular files only, until `cancel` fires.
///
/// Honors `.gitignore` and global ignore files, includes hidden entries such as `.github/` so
/// dotfile config stays searchable, skips VCS metadata directories below `base`, and pins the walk
/// to the starting filesystem so symlinked mounts don't escape.
fn walk_files<'a>(
    base: &Path,
    cancel: &'a CancellationToken,
) -> impl Iterator<Item = ignore::DirEntry> + 'a {
    ignore::WalkBuilder::new(base)
        .hidden(false)
        .filter_entry(|e| e.depth() == 0 || !VCS_DIRS.iter().any(|d| e.file_name() == *d))
        .same_file_system(true)
        .max_depth(Some(MAX_WALK_DEPTH))
        .build()
        .take_while(|_| !cancel.is_cancelled())
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

const WITHHELD_NOTE_END: &str = "withheld by read deny or ask rules)";

/// Appends the note a search tool adds when read deny or ask rules withheld `count` of the files
/// it found. The note counts them without naming any.
pub(crate) fn push_withheld_note(content: &mut String, count: usize) {
    if count == 0 {
        return;
    }
    let word = plural(count, "file", "files");
    _ = write!(content, "\n\n({count} {word} {WITHHELD_NOTE_END}");
}

/// `content` without the note [`push_withheld_note`] appended, for parsing the rest.
pub(crate) fn strip_withheld_note(content: &str) -> &str {
    match content.rsplit_once("\n\n") {
        Some((rest, note)) if note.starts_with('(') && note.ends_with(WITHHELD_NOTE_END) => rest,
        _ => content,
    }
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::{SearchRoot, Tool, ToolCtx, ToolOutput, ToolRegistry, run_capped};
    use crate::file_tracker::FileTracker;
    use crate::permission::rule::Rule;
    use crate::permission::{Access, Mode, PermissionClass, Policy, ReadFilter, Workspace};

    /// A fresh tracker's context in `cwd`, for call `toolu_test` of session `test-session`.
    pub(crate) fn ctx(cwd: &Path) -> ToolCtx {
        ctx_with(cwd, Arc::new(FileTracker::default()))
    }

    /// [`ctx`] in the system temp dir, for calls that name absolute paths.
    pub(crate) fn temp_ctx() -> ToolCtx {
        ctx(&std::env::temp_dir())
    }

    /// [`ctx`] with `tracker` as the session's tracker.
    pub(crate) fn ctx_with(cwd: &Path, tracker: Arc<FileTracker>) -> ToolCtx {
        ToolCtx {
            workspace: Arc::new(Workspace::new(cwd)),
            session_id: "test-session".to_owned(),
            tracker,
            call_id: "toolu_test".to_owned(),
            cancel: CancellationToken::new(),
        }
    }

    /// Prepares `input` for `tool` and runs it in `ctx`, the way dispatch does past the gate.
    pub(crate) async fn run(
        tool: &dyn Tool,
        input: serde_json::Value,
        ctx: &ToolCtx,
    ) -> ToolOutput {
        tool.run(tool.prepare(input, ctx), ctx).await
    }

    /// The read filter of a policy that denies reading whatever one of `patterns` matches.
    pub(crate) fn deny_reads(patterns: &[&str]) -> ReadFilter {
        let deny = patterns
            .iter()
            .map(|pattern| Rule::parse(PermissionClass::Read, pattern).unwrap())
            .collect();
        Policy::new(Mode::Manual, deny, Vec::new(), Vec::new()).read_filter()
    }

    /// The root a search call in `cwd` that names `path` walks, resolved the way the search tools'
    /// `prepare` resolves it.
    pub(super) fn search_root(cwd: &Path, path: Option<&str>) -> Result<SearchRoot, String> {
        let workspace = Arc::new(Workspace::new(cwd));
        let access = Access::for_path(path.unwrap_or("."), &workspace);
        SearchRoot::new(&access, path, workspace)
    }

    impl ToolRegistry {
        /// [`run`] for the tool registered as `name`, through the output cap.
        pub(crate) async fn run_input(
            &self,
            name: &str,
            input: serde_json::Value,
            ctx: &ToolCtx,
        ) -> ToolOutput {
            let (_, tool) = self.get(name).expect("tool is registered");
            run_capped(tool, tool.prepare(input, ctx), ctx).await
        }
    }

    /// A temp dir with a canonical path, the form a resolved workspace path takes.
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
        let path = dunce::canonicalize(dir.path()).unwrap();
        CanonicalTempDir { _dir: dir, path }
    }

    /// A symlink to a file. Windows needs Developer Mode or admin rights for it, which CI has.
    pub(crate) fn symlink_file(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(target, link).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use indoc::{formatdoc, indoc};

    use super::bash::BashTool;
    use super::bash::background::{BashOutputTool, BashStopTool};
    use super::edit::EditTool;
    use super::glob::GlobTool;
    use super::grep::GrepTool;
    use super::read::ReadTool;
    use super::tasks::TasksTool;
    use super::testing::{canonical_tempdir, ctx, search_root, symlink_file, temp_ctx};
    use super::write::WriteTool;
    use super::*;
    use crate::permission::rule::Rule;
    use crate::permission::{Decision, Mode, PermissionClass, Policy, Zone};

    fn all_tools() -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(BashTool::default()),
            Box::new(BashOutputTool::default()),
            Box::new(BashStopTool::default()),
            Box::new(EditTool),
            Box::new(GlobTool::default()),
            Box::new(GrepTool::default()),
            Box::new(ReadTool),
            Box::new(TasksTool),
            Box::new(WriteTool),
        ]
    }

    fn find<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> &'a dyn Tool {
        tools
            .iter()
            .find(|t| t.spec().name == name)
            .unwrap_or_else(|| panic!("tool {name} missing from catalog"))
            .as_ref()
    }

    // ── Tool: trait contract ──

    #[test]
    fn every_tool_exposes_non_empty_name_description_and_object_schema() {
        for t in all_tools() {
            let ToolSpec {
                name,
                description,
                input_schema,
                ..
            } = t.spec();
            assert!(!name.is_empty(), "tool with empty name in catalog");
            assert!(!description.is_empty(), "{name}: empty description");
            assert_eq!(input_schema["type"], "object", "{name}: schema type");
            assert!(
                input_schema["properties"].is_object(),
                "{name}: schema.properties"
            );
            assert!(
                input_schema["required"].is_array(),
                "{name}: schema.required"
            );
        }
    }

    #[test]
    fn tool_catalog_classes_and_parallel_safety_match_each_tool() {
        // Only the file reads and searches may run alongside each other.
        let catalog: Vec<_> = all_tools()
            .iter()
            .map(|t| {
                let spec = t.spec();
                (spec.name, spec.class, spec.parallel_safe)
            })
            .collect();
        assert_eq!(
            catalog,
            [
                ("bash".to_owned(), PermissionClass::Exec, false),
                ("bash_output".to_owned(), PermissionClass::Read, false),
                ("bash_stop".to_owned(), PermissionClass::Read, false),
                ("edit".to_owned(), PermissionClass::Edit, false),
                ("glob".to_owned(), PermissionClass::Read, true),
                ("grep".to_owned(), PermissionClass::Read, true),
                ("read".to_owned(), PermissionClass::Read, true),
                ("tasks".to_owned(), PermissionClass::Read, false),
                ("write".to_owned(), PermissionClass::Edit, false),
            ],
        );
    }

    #[test]
    fn tool_catalog_names_and_icons_are_unique() {
        let tools = all_tools();
        let names: HashSet<_> = tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(names.len(), tools.len(), "duplicate name");
        let icons: HashSet<_> = tools.iter().map(|t| t.icon()).collect();
        assert_eq!(icons.len(), tools.len(), "duplicate icon");
    }

    #[test]
    fn tool_catalog_icons_match_the_published_prefix_set() {
        let expected = [
            ("bash", "$"),
            ("bash_output", "»"),
            ("bash_stop", "■"),
            ("edit", "✎"),
            ("glob", "✱"),
            ("grep", "⌕"),
            ("read", "→"),
            ("tasks", "≡"),
            ("write", "←"),
        ];
        let tools = all_tools();
        for (name, icon) in expected {
            assert_eq!(find(&tools, name).icon(), icon, "tool {name}");
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
            let t = find(&tools, name);
            assert_eq!(t.summarize_input(input), *expected, "tool {name}");
        }
    }

    #[test]
    fn tool_summarize_input_is_none_when_primary_field_missing() {
        for t in all_tools() {
            assert!(
                t.summarize_input(&serde_json::json!({})).is_none(),
                "tool {} returned Some on empty input",
                t.spec().name,
            );
        }
    }

    #[test]
    fn tool_summarize_call_wraps_arg_in_the_tool_label() {
        let cases = [
            ("bash", serde_json::json!({"command": "ls"}), "ls"),
            (
                "bash_output",
                serde_json::json!({"shell_id": "shell-2"}),
                "BashOutput(shell-2)",
            ),
            (
                "bash_stop",
                serde_json::json!({"shell_id": "shell-2"}),
                "BashStop(shell-2)",
            ),
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
            let t = find(&tools, name);
            assert_eq!(
                t.summarize_call(name, input, Path::new("/repo")),
                *expected,
                "tool {name}",
            );
        }
    }

    #[test]
    fn tool_summarize_call_falls_back_to_bare_name_when_arg_missing() {
        for t in all_tools() {
            let name = t.spec().name;
            let got = t.summarize_call(&name, &serde_json::json!({}), Path::new("/repo"));
            assert_eq!(got, pascal_case(&name), "tool {name}");
        }
    }

    #[test]
    fn tool_summarize_call_shortens_file_paths_inside_cwd() {
        let path = "/repo/crates/oxide-code/src/tool/read.rs";
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
            let t = find(&tools, name);
            assert_eq!(
                t.summarize_call(name, input, Path::new("/repo")),
                *expected,
                "tool {name}",
            );
        }
    }

    // ── ToolSpec::class ──

    #[test]
    fn spec_class_rules_cover_every_tool_in_the_class() {
        // Rules address classes: a `read` deny must stop grep and glob from dumping the file it
        // names, and an `edit` deny must stop write as well as edit.
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join(".env"), "SECRET=1").unwrap();
        let deny = [
            (PermissionClass::Read, ".env"),
            (PermissionClass::Edit, ".env"),
            (PermissionClass::Exec, "cat .env"),
        ]
        .map(|(class, pattern)| Rule::parse(class, pattern).unwrap());
        let policy = Policy::new(Mode::Manual, deny.to_vec(), Vec::new(), Vec::new());
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
        let ctx = ctx(dir.path());
        for (name, input) in &cases {
            let t = find(&tools, name);
            let prepared = t.prepare(input.clone(), &ctx);
            assert_eq!(
                policy.decide(t.spec().class, &prepared.access.as_target()),
                Decision::Deny,
                "tool {name}",
            );
        }
    }

    // ── Tool::prepare ──

    #[test]
    fn prepare_resolves_and_pins_the_path_field_of_each_path_tool() {
        let dir = canonical_tempdir();
        let ctx = ctx(dir.path());
        let resolved = dir.path().join("a.rs");
        let tools = all_tools();
        for name in ["read", "edit", "write"] {
            let t = find(&tools, name);

            let prepared = t.prepare(
                serde_json::json!({"file_path": "a.rs", "content": "a.rs"}),
                &ctx,
            );

            let Access::Path {
                canonical,
                relative,
                zone,
                ..
            } = &prepared.access
            else {
                panic!("tool {name}: expected a path target, got {prepared:?}");
            };
            assert_eq!(canonical, resolved.to_str().unwrap(), "tool {name}");
            assert_eq!(relative.as_deref(), Some("a.rs"), "tool {name}");
            assert_eq!(*zone, Zone::Workspace, "tool {name}");
            assert_eq!(
                prepared.input,
                serde_json::json!({"file_path": resolved, "content": "a.rs"}),
                "tool {name}: only the path field is pinned",
            );

            let missing = t.prepare(serde_json::json!({}), &ctx);
            assert_eq!(missing.access, Access::None, "tool {name}");
            assert_eq!(missing.input, serde_json::json!({}), "tool {name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn prepare_follows_a_symlink_out_of_the_workspace_to_its_target() {
        // A link inside the cwd that points elsewhere must resolve to where the call lands, so the
        // edit gate sees an outside path and the tool opens that same path.
        let dir = canonical_tempdir();
        let (cwd, outside) = (dir.path().join("repo"), dir.path().join("outside"));
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink("../outside/notes.md", cwd.join("notes.md")).unwrap();
        let target = outside.join("notes.md");

        let prepared = WriteTool.prepare(
            serde_json::json!({"file_path": "notes.md", "content": "x"}),
            &ctx(&cwd),
        );

        let Access::Path {
            canonical,
            relative,
            zone,
            ..
        } = &prepared.access
        else {
            panic!("expected a path target, got {prepared:?}");
        };
        assert_eq!(canonical, target.to_str().unwrap());
        assert_eq!(*relative, None);
        assert_eq!(*zone, Zone::Outside);
        assert_eq!(prepared.input["file_path"], serde_json::json!(target));
    }

    #[test]
    fn prepare_resolves_a_search_root_but_leaves_its_spelling_in_the_input() {
        // The read deny and ask rules a search's hits face match against the root as the call
        // spells it.
        let dir = canonical_tempdir();
        let input = serde_json::json!({"pattern": "x", "path": "."});

        let prepared = GrepTool::default().prepare(input.clone(), &ctx(dir.path()));

        let Access::Path { canonical, .. } = &prepared.access else {
            panic!("expected a path target, got {prepared:?}");
        };
        assert_eq!(canonical, dir.path().to_str().unwrap());
        assert_eq!(prepared.input, input);
    }

    // ── Tool::approval_preview ──

    #[test]
    fn approval_preview_default_titles_with_the_tool_name() {
        let preview = GlobTool::default().approval_preview(
            "glob",
            &serde_json::json!({"pattern": "*.rs"}),
            Path::new("/repo"),
        );
        assert_eq!(preview.title, "Glob");
        assert_eq!(preview.body, ApprovalBody::Command("Glob(*.rs)".to_owned()));
    }

    // ── pascal_case ──

    #[test]
    fn pascal_case_capitalizes_each_snake_case_word() {
        assert_eq!(pascal_case("grep"), "Grep");
        assert_eq!(pascal_case("bash_output"), "BashOutput");
        assert_eq!(pascal_case("Foo"), "Foo");
        assert_eq!(pascal_case(""), "");
        assert_eq!(pascal_case("a"), "A");
    }

    // ── ToolRegistry::get ──

    #[test]
    fn get_finds_registered_tool() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
        assert_eq!(registry.get("bash").unwrap().0.name, "bash");
    }

    #[test]
    fn get_unknown_tool_has_no_entry() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
        assert!(registry.get("nonexistent").is_none());
    }

    // ── ToolRegistry::definitions ──

    #[test]
    fn definitions_produces_tool_with_valid_schema() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
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
        let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
        assert_eq!(registry.icon("bash"), "$");
    }

    #[test]
    fn icon_unknown_tool_falls_back_to_default() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
        assert_eq!(registry.icon("nonexistent"), DEFAULT_TOOL_ICON);
    }

    // ── ToolRegistry::label ──

    #[test]
    fn label_delegates_to_registered_tool() {
        let registry = ToolRegistry::new(vec![
            Box::new(BashTool::default()),
            Box::new(GrepTool::default()),
        ]);
        assert_eq!(
            registry.label(
                "bash",
                &serde_json::json!({"command": "echo hi"}),
                Path::new("/repo")
            ),
            "echo hi",
        );
        assert_eq!(
            registry.label(
                "grep",
                &serde_json::json!({"pattern": "fn "}),
                Path::new("/repo")
            ),
            "Grep(fn )",
        );
    }

    #[test]
    fn label_unknown_tool_falls_back_to_raw_name() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
        let input = serde_json::json!({"command": "echo hi"});
        assert_eq!(
            registry.label("nonexistent", &input, Path::new("/repo")),
            "nonexistent"
        );
    }

    // ── run_capped ──

    fn oversize_grep_input() -> (tempfile::TempDir, serde_json::Value) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let line = "x".repeat(400);
        std::fs::write(&path, format!("{line}\n").repeat(500)).unwrap();
        let input = serde_json::json!({"pattern": "x", "path": path, "head_limit": 0});
        (dir, input)
    }

    #[tokio::test]
    async fn run_capped_caps_byte_overflow() {
        let (_dir, input) = oversize_grep_input();
        let registry = ToolRegistry::new(vec![Box::new(GrepTool::default())]);
        let output = registry.run_input("grep", input, &temp_ctx()).await;

        assert!(!output.is_error);
        assert!(output.content.len() <= MAX_OUTPUT_BYTES + TRUNCATION_OVERHEAD);
        assert!(output.content.contains("bytes truncated; head + tail kept"));
        assert!(output.metadata.truncated_bytes.is_some());
        assert!(output.metadata.truncated_total.is_none());
    }

    #[tokio::test]
    async fn run_capped_within_cap_leaves_content_and_metadata_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();

        let registry = ToolRegistry::new(vec![Box::new(ReadTool)]);
        let output = registry
            .run_input(
                "read",
                serde_json::json!({"file_path": path.to_str().unwrap()}),
                &temp_ctx(),
            )
            .await;

        assert!(!output.is_error);
        assert_eq!(output.content, "1\thello\n2\tworld");
        assert!(output.metadata.truncated_bytes.is_none());
    }

    #[tokio::test]
    async fn run_capped_byte_cap_on_grep_falls_through_to_text_view() {
        let (_dir, input) = oversize_grep_input();
        let registry = ToolRegistry::new(vec![Box::new(GrepTool::default())]);
        let output = registry.run_input("grep", input.clone(), &temp_ctx()).await;
        assert!(output.metadata.truncated_bytes.is_some());

        let view = registry.result_view(
            "grep",
            &input,
            &output.content,
            &output.metadata,
            output.is_error,
            Path::new("/repo"),
        );
        assert!(matches!(view, ToolResultView::Text { .. }));
    }

    // ── ToolRegistry::result_view ──

    #[test]
    fn result_view_delegates_to_tool_for_structured_output() {
        let registry = ToolRegistry::new(vec![Box::new(EditTool)]);
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
            Path::new("/repo"),
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
        let registry = ToolRegistry::new(vec![Box::new(GrepTool::default())]);
        let input = serde_json::json!({"pattern": "fn"});
        let metadata = ToolMetadata::default();
        let view = registry.result_view(
            "grep",
            &input,
            "src/main.rs:10:fn main()",
            &metadata,
            false,
            Path::new("/repo"),
        );
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
        let registry = ToolRegistry::new(vec![Box::new(ReadTool)]);
        let input = serde_json::json!({"file_path": "/tmp/lib.rs"});
        let metadata = ToolMetadata::default();
        let view = registry.result_view(
            "read",
            &input,
            "1\tmod foo;",
            &metadata,
            false,
            Path::new("/repo"),
        );
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
        let registry = ToolRegistry::new(vec![Box::new(GlobTool::default())]);
        let input = serde_json::json!({"pattern": "*.rs"});
        let metadata = ToolMetadata::default();
        let view = registry.result_view(
            "glob",
            &input,
            "src/main.rs\nsrc/lib.rs",
            &metadata,
            false,
            Path::new("/repo"),
        );
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
        let registry = ToolRegistry::new(vec![Box::new(EditTool)]);
        let input = serde_json::json!({
            "file_path": "/tmp/f.rs",
            "old_string": "a",
            "new_string": "b",
        });
        let metadata = ToolMetadata::default();
        let view = registry.result_view(
            "edit",
            &input,
            "old_string not found",
            &metadata,
            true,
            Path::new("/repo"),
        );
        assert_eq!(
            view,
            ToolResultView::Text {
                content: "old_string not found".to_owned(),
            },
        );
    }

    #[test]
    fn result_view_falls_back_to_text_for_unknown_tool() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
        let metadata = ToolMetadata::default();
        let view = registry.result_view(
            "nonexistent",
            &serde_json::json!({}),
            "anything",
            &metadata,
            false,
            Path::new("/repo"),
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
        let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
        let metadata = ToolMetadata::default();
        let view = registry.result_view(
            "bash",
            &serde_json::json!({"command": "ls"}),
            "file1\nfile2",
            &metadata,
            false,
            Path::new("/repo"),
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

    // ── SearchRoot::new ──

    #[test]
    fn search_root_resolves_relative_and_absolute_paths_and_keeps_their_spelling() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join("lib")).unwrap();
        let lib = dir.path().join("lib");

        let relative = search_root(dir.path(), Some("./src/../lib")).unwrap();
        let absolute = search_root(Path::new("/"), Some(lib.to_str().unwrap())).unwrap();

        assert_eq!(
            (relative.base, relative.spelled, relative.cwd),
            (
                lib.clone(),
                PathBuf::from("./src/../lib"),
                dir.path().to_owned()
            ),
        );
        assert_eq!(absolute.base, lib);
    }

    #[cfg(unix)]
    #[test]
    fn search_root_resolves_dot_dot_after_a_symlink_physically() {
        let dir = canonical_tempdir();
        std::fs::create_dir_all(dir.path().join("real/target")).unwrap();
        let cwd = dir.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real/target"), cwd.join("link")).unwrap();

        let root = search_root(&cwd, Some("link/..")).unwrap();

        assert_eq!(root.base, dir.path().join("real"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn search_root_walks_from_a_root_whose_name_is_not_utf8() {
        use std::os::unix::ffi::OsStrExt as _;
        let dir = canonical_tempdir();
        let cwd = dir.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
        std::fs::create_dir(&cwd).unwrap();

        let root = search_root(&cwd, None).unwrap();

        assert_eq!(root.base, cwd);
    }

    #[test]
    fn search_root_none_uses_cwd() {
        let dir = canonical_tempdir();

        let root = search_root(dir.path(), None).unwrap();

        assert_eq!(
            (root.base, root.spelled),
            (dir.path().to_owned(), PathBuf::from(".")),
        );
    }

    #[test]
    fn search_root_missing_path_is_named_relative_to_cwd() {
        let dir = canonical_tempdir();
        let err = search_root(dir.path(), Some("missing")).err().unwrap();
        assert!(err.starts_with("Cannot access missing: "), "{err}");
    }

    #[test]
    fn search_root_refuses_an_access_that_is_not_a_path() {
        let workspace = Arc::new(Workspace::new(Path::new("/repo")));
        let err = SearchRoot::new(&Access::None, Some("src"), workspace)
            .err()
            .unwrap();
        assert_eq!(err, "Cannot access src: the search root was not resolved.");
    }

    // ── tracker_key ──

    #[tokio::test]
    async fn tracker_key_resolves_dotted_spellings_to_one_key() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let file = dir.path().join("src/main.rs");
        std::fs::write(&file, "").unwrap();

        assert_eq!(tracker_key(&file).await, file);
        assert_eq!(
            tracker_key(&dir.path().join("./src/../src/main.rs")).await,
            file
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tracker_key_resolves_a_symlinked_spelling_to_the_target() {
        let dir = canonical_tempdir();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::fs::write(dir.path().join("real/a.rs"), "").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();

        assert_eq!(
            tracker_key(&dir.path().join("link/a.rs")).await,
            dir.path().join("real/a.rs"),
        );
    }

    #[tokio::test]
    async fn tracker_key_missing_path_falls_back_to_its_lexically_clean_form() {
        let dir = canonical_tempdir();
        assert_eq!(
            tracker_key(&dir.path().join("missing/../a.rs")).await,
            dir.path().join("a.rs")
        );
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

    #[tokio::test]
    async fn read_validated_reads_a_regular_file_but_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "original").unwrap();
        let link = dir.path().join("link.txt");
        symlink_file(&target, &link);

        let accept = |_: &std::fs::Metadata| Ok(());
        let (metadata, bytes) = read_validated(&target, "target.txt", accept).await.unwrap();
        assert_eq!(
            (metadata.len(), bytes.as_slice()),
            (8, b"original".as_slice())
        );
        let err = read_validated(&link, "link.txt", accept).await.unwrap_err();
        assert!(err.starts_with("Error reading link.txt: "), "{err}");
        #[cfg(windows)]
        assert!(
            err.ends_with("the final path component is a symlink"),
            "{err}"
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
            let err = read_validated(&path, "notes.md", validate)
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

        write_no_follow(&path, b"short", WriteMode::Overwrite)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short");
    }

    #[tokio::test]
    async fn write_no_follow_create_new_refuses_existing_entries_and_dangling_links() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.txt");
        std::fs::write(&existing, "original").unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        symlink_file(&target, &link);

        assert!(
            write_no_follow(&existing, b"x", WriteMode::CreateNew)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(
            write_no_follow(&link, b"x", WriteMode::CreateNew)
                .await
                .is_err()
        );
        assert!(!target.exists(), "the link target must not be created");
    }

    #[tokio::test]
    async fn write_no_follow_overwrite_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "original").unwrap();
        let link = dir.path().join("link.txt");
        symlink_file(&target, &link);

        let err = write_no_follow(&link, b"x", WriteMode::Overwrite)
            .await
            .unwrap_err();
        #[cfg(unix)]
        assert_eq!(err.raw_os_error(), Some(nix::errno::Errno::ELOOP as i32));
        #[cfg(windows)]
        assert_eq!(err.to_string(), "the final path component is a symlink");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_no_follow_overwrite_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();

        let write = write_no_follow(&fifo, b"x", WriteMode::Overwrite);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), write).await;
        assert!(result.expect("the open must not block").is_err());
    }

    // ── open_no_follow ──

    #[cfg(windows)]
    #[tokio::test]
    async fn open_no_follow_reads_and_rewrites_a_compressed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        let content = "a compressible line\n".repeat(4096);
        std::fs::write(&path, &content).unwrap();
        // `compact /EXE` stores the data compressed behind the WOF filter. Skip where the volume
        // cannot compress.
        let compacted = std::process::Command::new("compact.exe")
            .args(["/C", "/EXE:XPRESS4K"])
            .arg(&path)
            .output()
            .is_ok_and(|output| output.status.success());
        if !compacted {
            return;
        }

        let accept = |_: &std::fs::Metadata| Ok(());
        let (_, bytes) = read_validated(&path, "notes.txt", accept).await.unwrap();
        assert_eq!(bytes, content.as_bytes());
        write_no_follow(&path, b"rewritten", WriteMode::Overwrite)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"rewritten");
    }

    // ── walk_files ──

    #[test]
    fn walk_files_stops_once_cancelled() {
        let dir = canonical_tempdir();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let cancel = CancellationToken::new();
        let mut walk = walk_files(dir.path(), &cancel);

        let first = walk.next();
        cancel.cancel();

        assert!(first.is_some(), "an uncancelled walk yields files");
        assert_eq!(walk.count(), 0, "the walk stops at the cancel");
    }

    // ── truncate_line ──

    const MAX_CHARS: usize = 500;

    #[test]
    fn truncate_line_short_unchanged() {
        assert_eq!(truncate_line("hello", MAX_CHARS).as_ref(), "hello");
    }

    #[test]
    fn truncate_line_long_text_ends_with_ellipsis() {
        let long_line = "x".repeat(MAX_CHARS + 100);
        let result = truncate_line(&long_line, MAX_CHARS);
        let suffix = format!("... [{} chars]", MAX_CHARS + 100);
        assert_eq!(result.len(), MAX_CHARS + suffix.len());
        assert_eq!(&result[..MAX_CHARS], "x".repeat(MAX_CHARS));
        assert_eq!(&result[MAX_CHARS..], suffix);
    }

    #[test]
    fn truncate_line_multibyte_text_preserves_character_boundaries() {
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

    // ── push_withheld_note ──

    #[test]
    fn push_withheld_note_counts_files_and_strips_back_off() {
        for (count, note) in [
            (1, "(1 file withheld by read deny or ask rules)"),
            (3, "(3 files withheld by read deny or ask rules)"),
        ] {
            let mut content = "a.rs".to_owned();
            push_withheld_note(&mut content, count);
            assert_eq!(
                content,
                formatdoc! {"
                    a.rs

                    {note}"}
            );
            assert_eq!(strip_withheld_note(&content), "a.rs");
        }

        let mut content = "a.rs".to_owned();
        push_withheld_note(&mut content, 0);
        assert_eq!(content, "a.rs");
    }

    // ── strip_withheld_note ──

    #[test]
    fn strip_withheld_note_keeps_content_without_one() {
        for content in [
            "a.rs",
            indoc! {"
                a.rs

                (Showing 100 of 120 matches. Use a more specific pattern.)"},
        ] {
            assert_eq!(strip_withheld_note(content), content);
        }
    }
}
