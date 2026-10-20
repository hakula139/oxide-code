//! The [`Tool`] trait, the [`ToolRegistry`] that dispatches calls and caps their output, the result
//! views the TUI renders, and helpers shared by the built-in tools.

pub mod bash;
pub(crate) mod edit;
pub(crate) mod glob;
pub(crate) mod grep;
pub(crate) mod read;
pub mod tasks;
pub(crate) mod write;

use std::borrow::Cow;
use std::cmp::Reverse;
use std::fmt::Write as _;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use self::bash::BashTool;
use self::bash::background::{BackgroundShells, BashOutputTool, BashStopTool};
use self::edit::EditTool;
use self::glob::GlobTool;
use self::grep::GrepTool;
use self::read::ReadTool;
use self::tasks::{Task, TasksTool};
use self::write::WriteTool;
use crate::file_tracker::{
    FileTracker, MAX_TRACKED_FILE_SIZE, TrackableFileError, validate_trackable_file,
};
use crate::permission::{Access, PermissionClass, ReadFilter, WalkFilter, Workspace};
use crate::util::fs::create_private_dir_all;
use crate::util::path::{lexical_normalize, slash_separated};
use crate::util::text::{ELLIPSIS, plural};

// ── Tool Spec ──

/// What the model and the gate know of a tool. The registry takes it once, when the tool registers.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// The permission class the gate files the tool's calls under.
    pub class: PermissionClass,
    /// Whether a call may run alongside the other parallel-safe calls of its round, since it
    /// changes nothing another call reads.
    pub parallel_safe: bool,
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

/// The context a call prepares and runs in. Its workspace, session ID, artifact directory, and
/// tracker are shared with the session's other calls.
#[derive(Clone)]
pub struct ToolCtx {
    pub(crate) workspace: Arc<Workspace>,
    pub(crate) session_id: String,
    /// The directory that holds the session's artifacts, such as spilled tool output. It may not
    /// exist yet, so the first call that writes there creates it.
    pub(crate) artifact_dir: Arc<Path>,
    pub(crate) tracker: Arc<FileTracker>,
    /// The ID the model gave the call, which the tracker attributes the call's records to.
    pub(crate) tool_use_id: String,
    /// Cancelled when the user interrupts the call, so work its dropped future leaves behind, such
    /// as a blocking file walk, stops early.
    pub(crate) cancel: CancellationToken,
}

impl ToolCtx {
    fn cwd(&self) -> &Path {
        self.workspace.cwd()
    }

    /// Where the call's spill files go: [`SPILL_DIR`] in the artifact directory, under names that
    /// start with the call's ID.
    fn spills(&self) -> Spills {
        Spills {
            dir: self.artifact_dir.join(SPILL_DIR),
            stem: spill_stem(&self.tool_use_id),
        }
    }
}

/// A call resolved for the gate and its run: the input with any path pinned to where it resolves,
/// and what the call accesses.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub(crate) input: serde_json::Value,
    pub(crate) access: Access,
}

// ── Tool Output ──

/// Result returned from a tool execution to the agent loop.
pub struct ToolOutput {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) metadata: ToolMetadata,
    /// The task list a `tasks` call leaves, which the engine adopts in place of its own.
    pub(crate) tasks: Option<Vec<Task>>,
}

/// Structured data for UI display and logging. The model never sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
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
    pub fn success(content: impl Into<String>) -> Self {
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

    fn with_diff_chunks(mut self, chunks: Vec<DiffChunk>) -> Self {
        self.metadata.diff_chunks = Some(chunks);
        self
    }

    fn with_truncated_total(mut self, total: usize) -> Self {
        self.metadata.truncated_total = Some(total);
        self
    }

    fn with_tasks(mut self, tasks: Vec<Task>) -> Self {
        self.tasks = Some(tasks);
        self
    }
}

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
    /// Tool display label, such as `Bash(cargo test)` or `Write(src/main.rs)`.
    pub title: String,
    pub body: ApprovalBody,
}

/// The renderable body of an [`ApprovalPreview`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ApprovalBody {
    /// A shell command string (`bash`).
    Command(String),
    /// A unified diff (`edit` / `write`), reusing the chat's `+` / `-` chunk shape.
    Diff(Vec<DiffChunk>),
}

// ── Tool Trait ──

/// A tool the agent can invoke.
pub trait Tool: Send + Sync {
    /// The tool's spec. Its permission class has no default, so a new tool cannot inherit a
    /// permissive one.
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

    /// Builds the approval modal's preview of `input`, by default the tool name over its label from
    /// [`Self::summarize_call`]. Async because the write preview reads the file it would replace.
    fn approval_preview<'a>(
        &'a self,
        name: &'a str,
        input: &'a serde_json::Value,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = ApprovalPreview> + Send + 'a>> {
        Box::pin(std::future::ready(ApprovalPreview {
            title: pascal_case(name),
            body: ApprovalBody::Command(self.summarize_call(name, input, cwd)),
        }))
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

fn extract_input_field<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
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

/// The [`Tool`]s available to the agent, each with the spec it registered with, looked up by name.
/// The set is small and fixed, so lookups scan a `Vec`.
pub struct ToolRegistry {
    tools: Vec<(ToolSpec, Box<dyn Tool>)>,
}

impl ToolRegistry {
    pub(crate) fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self {
            tools: tools.into_iter().map(|tool| (tool.spec(), tool)).collect(),
        }
    }

    /// The built-in tools, with `read_filter` handed to the search tools and `shells` shared by the
    /// `bash` tools.
    pub(crate) fn built_in(read_filter: &ReadFilter, shells: &Arc<BackgroundShells>) -> Self {
        Self::new(vec![
            Box::new(BashTool::new(Arc::clone(shells))),
            Box::new(BashOutputTool::new(Arc::clone(shells))),
            Box::new(BashStopTool::new(Arc::clone(shells))),
            Box::new(ReadTool),
            Box::new(WriteTool),
            Box::new(EditTool),
            Box::new(GlobTool::new(read_filter.clone())),
            Box::new(GrepTool::new(read_filter.clone())),
            Box::new(TasksTool),
        ])
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

    /// Resolves the result view for a tool call, with paths relative to `cwd`. Errors and unknown
    /// tools render as [`ToolResultView::Text`].
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

const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// Runs the prepared call on `tool`, applying [`cap_output`] after each tool's row caps, with the
/// call's `.txt` spill file keeping the full output. Records the original length in
/// `metadata.truncated_bytes` when truncated.
pub(crate) async fn run_capped(tool: &dyn Tool, prepared: Prepared, ctx: &ToolCtx) -> ToolOutput {
    let mut output = tool.run(prepared, ctx).await;
    let original_len = output.content.len();
    if original_len <= MAX_OUTPUT_BYTES {
        return output;
    }

    let content = std::mem::take(&mut output.content);
    let spill = ctx.spills().file(".txt");
    match run_blocking(move || Ok(cap_output(&content, spill))).await {
        Ok(capped) => output.content = capped,
        Err(e) => return ToolOutput::error(e),
    }
    output.metadata.truncated_bytes = Some(original_len);
    output
}

/// Writes `content`, which must be longer than [`MAX_OUTPUT_BYTES`], whole to `spill`, and returns
/// equal parts of each end at UTF-8 boundaries around an omission marker, all within the cap.
fn cap_output(content: &str, mut spill: Spill) -> String {
    spill.write(content.as_bytes());
    let note = spill.note("output");
    // The full length has at least as many digits as the omitted count the marker ends up with.
    let reserve = omission_marker(content.len(), &note).len() + "\n\n".len();
    let half = (MAX_OUTPUT_BYTES - reserve) / 2;
    let head_end = content.floor_char_boundary(half);
    let tail_start = content.ceil_char_boundary(content.len() - half);

    let marker = omission_marker(tail_start - head_end, &note);
    let mut capped = String::with_capacity(MAX_OUTPUT_BYTES);
    capped.push_str(&content[..head_end]);
    _ = write!(capped, "\n{marker}\n");
    capped.push_str(&content[tail_start..]);
    capped
}

/// The line that stands in for the `omitted` bytes cut from a result, with `note` saying where the
/// full output is kept.
fn omission_marker(omitted: usize, note: &str) -> String {
    format!("... [{omitted} bytes omitted, {note}] ...")
}

// ── Output Spill ──

/// The directory in [`ToolCtx::artifact_dir`] that keeps the full output of results too large to
/// return whole.
const SPILL_DIR: &str = "tool-results";

/// Most bytes a spill file keeps, so a runaway command cannot fill the disk. It matches the largest
/// file `read` opens, so every spill file reads back.
const MAX_SPILL_BYTES: u64 = MAX_TRACKED_FILE_SIZE;

/// Where one call's spill files go, each named by the call's ID and a suffix.
struct Spills {
    dir: PathBuf,
    stem: String,
}

impl Spills {
    /// The spill into the file named by the call's ID followed by `suffix`, such as `.stdout.txt`.
    fn file(&self, suffix: &str) -> Spill {
        Spill {
            path: self.dir.join(format!("{}{suffix}", self.stem)),
            state: SpillState::Pending,
        }
    }
}

/// `id` with each character other than an ASCII letter, digit, `-`, or `_` replaced by `_`, so a
/// tool call ID from the API cannot name a path outside the spill directory.
fn spill_stem(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A file that keeps the full output of a result too large to return whole. The first write creates
/// it with `0600` perms in a `0700` directory, so a result that fits leaves nothing on disk.
struct Spill {
    path: PathBuf,
    state: SpillState,
}

enum SpillState {
    Pending,
    /// `full` once the file reached [`MAX_SPILL_BYTES`] and dropped bytes.
    Open {
        file: std::fs::File,
        written: u64,
        full: bool,
    },
    /// Why creating or writing the file failed.
    Failed(String),
}

impl Spill {
    fn path(&self) -> &Path {
        &self.path
    }

    /// Returns whether a write has started the spill, whether or not it succeeded.
    fn started(&self) -> bool {
        !matches!(self.state, SpillState::Pending)
    }

    /// Appends `bytes`, creating the file on the first write. A failure ends the spill, and
    /// [`Self::note`] reports it.
    fn write(&mut self, bytes: &[u8]) {
        if let SpillState::Pending = self.state {
            self.state = match create_spill_file(&self.path) {
                Ok(file) => SpillState::Open {
                    file,
                    written: 0,
                    full: false,
                },
                Err(e) => self.fail(&e),
            };
        }
        let SpillState::Open {
            file,
            written,
            full,
        } = &mut self.state
        else {
            return;
        };

        let room = usize::try_from(MAX_SPILL_BYTES - *written).unwrap_or(usize::MAX);
        let kept = &bytes[..bytes.len().min(room)];
        *full |= kept.len() < bytes.len();
        if let Err(e) = file.write_all(kept) {
            self.state = self.fail(&e.to_string());
        } else {
            *written += kept.len() as u64;
        }
    }

    /// Logs `error` and returns the state that ends the spill.
    fn fail(&self, error: &str) -> SpillState {
        warn!(
            "failed to spill tool output to {}: {error}",
            self.path.display()
        );
        SpillState::Failed(error.to_owned())
    }

    /// Where the full `what`, such as `stdout`, is kept, as the omission marker names it. Call it
    /// only once a write has started the spill.
    fn note(&self, what: &str) -> String {
        let path = self.path.display();
        match &self.state {
            SpillState::Pending => unreachable!("a result omits bytes only after they spill"),
            SpillState::Open { full: false, .. } => format!("full {what} saved to {path}"),
            SpillState::Open { full: true, .. } => {
                let mib = MAX_SPILL_BYTES >> 20;
                format!("first {mib} MiB of {what} saved to {path}")
            }
            SpillState::Failed(error) => {
                format!("saving the full {what} to {path} failed: {error}")
            }
        }
    }
}

/// Creates the new file at `path` with `0600` perms on Unix, and its directory with `0700` perms.
fn create_spill_file(path: &Path) -> Result<std::fs::File, String> {
    let dir = path
        .parent()
        .expect("a spill file sits in the spill directory");
    create_private_dir_all(dir).map_err(|e| e.root_cause().to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    options.open(path).map_err(|e| e.to_string())
}

// ── Input Parsing ──

/// Deserializes a tool's typed input, or returns the `Invalid input: ...` message the model reads.
fn parse_input<T: DeserializeOwned>(raw: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(raw).map_err(|e| invalid_input(&e))
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
                "Failed to access {}: the search root was not resolved.",
                display_path(&spelled, cwd)
            ));
        };
        if let Err(e) = std::fs::metadata(&base) {
            return Err(format!(
                "Failed to access {}: {e}",
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

/// Renders a `Name(path)` summary for a call of the tool registered as `name`, showing the
/// `path_key` field relative to `cwd`, or the bare label when that field is absent or not a string.
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

/// Renders `path` for chat display, relative to `base` when under it, as its file name when it is
/// `base` itself, and in full otherwise.
fn display_path(path: &Path, base: &Path) -> String {
    let shown = match path.strip_prefix(base) {
        Ok(rel) if rel.as_os_str().is_empty() => path.file_name().map_or(path, Path::new),
        Ok(rel) => rel,
        Err(_) => path,
    };
    slash_separated(shown)
}

/// Returns the trailing file name of `path`, or `path` itself when it has none or it is not UTF-8.
fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

// ── File Target Validation ──

/// Rejects targets the [`FileTracker`] cannot gate, phrasing each refusal for `verb` (`read`,
/// `edit`, `write`).
fn validate_file_target(
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

/// Returns whether `bytes` hold a NUL in the first 8 KiB, which Git takes to mean binary.
fn is_binary(bytes: &[u8]) -> bool {
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
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Failed to access {shown}: {e}"))?;
    validate(&metadata)?;

    let error = |e: std::io::Error| format!("Failed to read {shown}: {e}");
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
        .write_and_record(&ctx.tool_use_id, async move {
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

/// Recursively walks `base`, yielding regular files only, until `cancel` fires. Each path the walk
/// cannot read goes to `unreadable`.
///
/// Honors `.gitignore` and global ignore files, includes hidden entries such as `.github/` so
/// dotfile config stays searchable, skips VCS metadata directories below `base`, and pins the walk
/// to the starting filesystem so symlinked mounts don't escape.
fn walk_files<'a>(
    base: &Path,
    cancel: &'a CancellationToken,
    unreadable: &'a mut Vec<Unreadable>,
) -> impl Iterator<Item = ignore::DirEntry> + 'a {
    ignore::WalkBuilder::new(base)
        .hidden(false)
        .filter_entry(|e| e.depth() == 0 || !VCS_DIRS.iter().any(|d| e.file_name() == *d))
        .same_file_system(true)
        .max_depth(Some(MAX_WALK_DEPTH))
        .build()
        .take_while(|_| !cancel.is_cancelled())
        .filter_map(move |entry| match entry {
            Ok(entry) => Some(entry),
            Err(e) => {
                unreadable.extend(Unreadable::from_walk(&e));
                None
            }
        })
        .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
}

/// A path a search could not read, with the error that stopped it.
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
struct Unreadable {
    /// The path, unless the walk failed partway through a directory listing without naming it.
    path: Option<PathBuf>,
    error: String,
}

impl Unreadable {
    /// What walk error `e` failed to read, or `None` if `e` hides nothing from the walk. An ignore
    /// file that failed to load only loses its rules, and a path that vanished mid-walk holds
    /// nothing left to search.
    fn from_walk(e: &ignore::Error) -> Option<Self> {
        e.depth()?;
        let (path, cause) = match e {
            ignore::Error::WithPath { path, err } => (Some(path.clone()), err.as_ref()),
            _ => (None, e),
        };
        let error = match cause.io_error() {
            Some(io) if io.kind() == std::io::ErrorKind::NotFound => return None,
            Some(io) => innermost_error(io),
            None => cause.to_string(),
        };
        Some(Self { path, error })
    }

    /// The file at `path`, whose read failed with `e`.
    fn from_read(path: &Path, e: &std::io::Error) -> Self {
        Self {
            path: Some(path.to_owned()),
            error: innermost_error(e),
        }
    }
}

/// The text of the error at the bottom of `e`'s source chain, without the context wrapping it.
fn innermost_error(e: &std::io::Error) -> String {
    let mut error: &dyn std::error::Error = e;
    while let Some(source) = error.source() {
        error = source;
    }
    error.to_string()
}

/// Drops each path in `unreadable` that `read_filter` withholds, so a note never names one, and
/// returns how many it dropped.
fn withhold_unreadable(unreadable: &mut Vec<Unreadable>, read_filter: &WalkFilter<'_>) -> usize {
    let found = unreadable.len();
    unreadable.retain(|u| {
        u.path
            .as_deref()
            .is_none_or(|path| !read_filter.withholds(path))
    });
    found - unreadable.len()
}

/// `entries` sorted newest first by modification time, keeping walk order among equal times.
fn newest_first(entries: impl Iterator<Item = ignore::DirEntry>) -> Vec<ignore::DirEntry> {
    let mut entries: Vec<_> = entries.collect();
    entries.sort_by_cached_key(|entry| Reverse(entry_mtime(entry)));
    entries
}

/// The entry's modification time, or [`SystemTime::UNIX_EPOCH`] when stat fails (entry vanished,
/// permission denied, platform without mtime), which sorts unreadable entries oldest.
fn entry_mtime(entry: &ignore::DirEntry) -> SystemTime {
    entry
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

// ── Blocking Work ──

/// Runs `work` on the blocking thread pool, reporting a panic that ends it as an error.
async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|e| Err(format!("Internal error: {e}")))
}

// ── Formatting ──

#[expect(
    clippy::cast_precision_loss,
    reason = "MB display loses precision only above 2^53 bytes, far beyond any file size cap"
)]
fn bytes_to_mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Caps a line at `max_chars` characters, appending `... [N chars]` with the original length so the
/// model can decide whether to read more. Borrows when the line already fits.
fn truncate_line(line: &str, max_chars: usize) -> Cow<'_, str> {
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

const NO_FILES_FOUND: &str = "No files found";

/// Most paths a search note names before it counts the rest.
const MAX_NOTE_PATHS: usize = 10;

/// Appends a search note listing `lines` under `header`. It names at most [`MAX_NOTE_PATHS`] and
/// counts the rest, so a tree full of failures cannot flood the result.
fn push_path_note(content: &mut String, header: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    _ = write!(content, "\n\n{header}");
    for line in lines.iter().take(MAX_NOTE_PATHS) {
        _ = write!(content, "\n  {line}");
    }
    if lines.len() > MAX_NOTE_PATHS {
        let more = lines.len() - MAX_NOTE_PATHS;
        _ = write!(content, "\n  {ELLIPSIS} and {more} more");
    }
}

const UNREADABLE_HEADER: &str = "Could not read:";

/// Appends the note a search tool adds for the paths in `unreadable`, so a search that missed them
/// does not pass for a complete one. Paths render relative to `cwd`.
fn push_unreadable_note(content: &mut String, unreadable: &[Unreadable], cwd: &Path) {
    let lines: Vec<String> = unreadable
        .iter()
        .map(|Unreadable { path, error }| match path {
            Some(path) => format!("{}: {error}", display_path(path, cwd)),
            None => error.clone(),
        })
        .collect();
    push_path_note(content, UNREADABLE_HEADER, &lines);
}

const WITHHELD_NOTE_END: &str = "withheld by read deny or ask rules)";

/// Appends the note a search tool adds when read deny or ask rules withheld `count` of the files
/// it found. The note counts them without naming any.
fn push_withheld_note(content: &mut String, count: usize) {
    if count == 0 {
        return;
    }
    let word = plural(count, "file", "files");
    _ = write!(content, "\n\n({count} {word} {WITHHELD_NOTE_END}");
}

/// `content` without the notes [`push_unreadable_note`] and [`push_withheld_note`] appended, for
/// parsing the rest.
fn strip_search_notes(content: &str) -> &str {
    let content = match content.rsplit_once("\n\n") {
        Some((rest, note)) if note.starts_with('(') && note.ends_with(WITHHELD_NOTE_END) => rest,
        _ => content,
    };
    match content.rsplit_once("\n\n") {
        Some((rest, note)) if note.starts_with(UNREADABLE_HEADER) => rest,
        _ => content,
    }
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod local_testing {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::{SearchRoot, Tool, ToolCtx, ToolOutput, ToolRegistry, run_capped};
    use crate::file_tracker::FileTracker;
    use crate::permission::rule::Rule;
    use crate::permission::{Access, Mode, PermissionClass, Policy, ReadFilter, Workspace};

    /// A fresh tracker's context in `cwd`, for call `toolu_test` of session `test-session`, whose
    /// artifact directory is `cwd/session`.
    pub(crate) fn ctx(cwd: &Path) -> ToolCtx {
        ctx_with(cwd, Arc::new(FileTracker::default()))
    }

    /// [`ctx`] in the system temp dir, for calls that name absolute paths. Its artifact directory
    /// is shared, so a call that spills output needs [`ctx`] in a temp dir of its own.
    pub(crate) fn temp_ctx() -> ToolCtx {
        ctx(&std::env::temp_dir())
    }

    /// [`ctx`] with `tracker` as the session's tracker.
    pub(crate) fn ctx_with(cwd: &Path, tracker: Arc<FileTracker>) -> ToolCtx {
        ToolCtx {
            workspace: Arc::new(Workspace::new(cwd)),
            session_id: "test-session".to_owned(),
            artifact_dir: Arc::from(cwd.join("session")),
            tracker,
            tool_use_id: "toolu_test".to_owned(),
            cancel: CancellationToken::new(),
        }
    }

    /// Where a spill of call `toolu_test` in [`ctx`] in `cwd` with `suffix`, such as `.txt`, goes.
    pub(crate) fn spill_path(cwd: &Path, suffix: &str) -> PathBuf {
        cwd.join("session")
            .join("tool-results")
            .join(format!("toolu_test{suffix}"))
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

    /// Creates an empty file at `path`, last modified `secs` seconds after the Unix epoch.
    pub(crate) fn create_modified_at(path: &Path, secs: u64) {
        let file = std::fs::File::create(path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    /// Denies reading the file or directory at `path` until the returned guard drops, which
    /// restores access so the temp dir can be removed.
    pub(crate) fn deny_read_access(path: &Path) -> ReadDenied {
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt as _;

            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
            mode
        };
        // `(RD)` is the right to read a file's data or list a directory.
        #[cfg(windows)]
        icacls(path, &["/deny", format!("{EVERYONE}:(RD)").as_str()]);
        let denied = ReadDenied {
            path: path.to_owned(),
            #[cfg(unix)]
            mode,
        };

        let readable = if path.is_dir() {
            std::fs::read_dir(path).is_ok()
        } else {
            std::fs::File::open(path).is_ok()
        };
        assert!(!readable, "{} stayed readable", path.display());
        denied
    }

    /// Read access denied by [`deny_read_access`], restored on drop.
    pub(crate) struct ReadDenied {
        path: PathBuf,
        #[cfg(unix)]
        mode: u32,
    }

    impl Drop for ReadDenied {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;

                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.mode))
                    .unwrap();
            }
            #[cfg(windows)]
            icacls(&self.path, &["/remove:d", EVERYONE]);
        }
    }

    /// The well-known SID of Everyone. A deny entry for it binds the file's owner too.
    #[cfg(windows)]
    const EVERYONE: &str = "*S-1-1-0";

    #[cfg(windows)]
    fn icacls(path: &Path, args: &[&str]) {
        let output = std::process::Command::new("icacls")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::bash::BashTool;
    use super::edit::synthesize_chunk;
    use super::{ApprovalBody, ApprovalPreview, DiffChunk, Tool, ToolRegistry};

    /// The approval preview of a foreground `bash` call that runs `command`.
    pub fn bash_preview(command: &str) -> ApprovalPreview {
        ApprovalPreview {
            title: "Bash".to_owned(),
            body: ApprovalBody::Command(command.to_owned()),
        }
    }

    /// A registry of `tools` followed by a `bash` tool with background shells of its own.
    pub fn bash_registry(mut tools: Vec<Box<dyn Tool>>) -> ToolRegistry {
        tools.push(Box::new(BashTool::default()));
        ToolRegistry::new(tools)
    }

    /// The diff chunk the `edit` tool shows for replacing `old` with `new`, numbered from line 1.
    pub fn diff_chunk(old: &str, new: &str) -> DiffChunk {
        synthesize_chunk(old, new)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use indoc::{formatdoc, indoc};

    use super::local_testing::{
        canonical_tempdir, create_modified_at, ctx, deny_read_access, deny_reads, search_root,
        spill_path, temp_ctx,
    };
    use super::*;
    use crate::permission::rule::Rule;
    use crate::permission::{Decision, Mode, PermissionClass, Policy, Zone};
    use crate::util::fs::testing::symlink_file;

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

    /// Splits output capped by [`cap_output`] into its head, the text inside its omission marker,
    /// and its tail.
    fn split_capped(out: &str) -> (&str, &str, &str) {
        let (head, rest) = out.split_once("\n... [").unwrap();
        let (marker, tail) = rest.split_once("] ...\n").unwrap();
        (head, marker, tail)
    }

    #[test]
    fn catalog_names_and_icons_are_unique() {
        let tools = all_tools();
        let names: HashSet<_> = tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(names.len(), tools.len(), "duplicate name");
        let icons: HashSet<_> = tools.iter().map(|t| t.icon()).collect();
        assert_eq!(icons.len(), tools.len(), "duplicate icon");
    }

    mod spec {
        use super::*;

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
        fn catalog_classes_and_parallel_safety_match_each_tool() {
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
        fn class_rules_cover_every_tool_in_the_class() {
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
    }

    mod prepare {
        use super::*;

        #[test]
        fn resolves_and_pins_the_path_field_of_each_path_tool() {
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

        #[tokio::test]
        async fn matches_native_file_access_for_directory_suffixed_paths() {
            for name in ["notes/", "notes/."] {
                let dir = canonical_tempdir();
                let ctx = ctx(dir.path());
                let path = dir.path().join("notes");
                std::fs::write(&path, "original").unwrap();
                let native = std::fs::read_to_string(dir.path().join(name));
                let succeeds = native.is_ok();

                let prepared = ReadTool.prepare(serde_json::json!({"file_path": name}), &ctx);
                let Access::Path { zone, .. } = &prepared.access else {
                    panic!("expected a path target, got {prepared:?}");
                };
                assert_eq!(*zone, Zone::Outside, "{name}");
                let read = ReadTool.run(prepared, &ctx).await;
                assert_eq!(read.is_error, !succeeds, "{name}: {}", read.content);
                if let Ok(content) = native {
                    assert_eq!(content, "original");
                    assert_eq!(read.content, "1\toriginal");
                }

                let write = local_testing::run(
                    &WriteTool,
                    serde_json::json!({"file_path": name, "content": "replacement"}),
                    &ctx,
                )
                .await;
                assert_eq!(write.is_error, !succeeds, "{name}: {}", write.content);

                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    if succeeds { "replacement" } else { "original" },
                    "{name}",
                );
            }
        }

        #[tokio::test]
        async fn keeps_missing_directory_paths_from_becoming_files() {
            for name in ["missing/", "missing/."] {
                let dir = canonical_tempdir();
                let ctx = ctx(dir.path());

                let read =
                    local_testing::run(&ReadTool, serde_json::json!({"file_path": name}), &ctx)
                        .await;
                assert!(read.is_error, "{name}: {}", read.content);
                let write = local_testing::run(
                    &WriteTool,
                    serde_json::json!({"file_path": name, "content": "replacement"}),
                    &ctx,
                )
                .await;
                assert!(write.is_error, "{name}: {}", write.content);

                assert!(!dir.path().join("missing").is_file());
            }
        }

        #[test]
        fn resolves_a_search_root_but_leaves_its_spelling_in_the_input() {
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

        #[cfg(unix)]
        #[test]
        fn follows_a_symlink_out_of_the_workspace_to_its_target() {
            // The edit gate must see the outside path that the tool then opens.
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
    }

    mod icon {
        use super::*;

        #[test]
        fn catalog_icons_match_the_published_prefix_set() {
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
    }

    mod summarize_input {
        use super::*;

        #[test]
        fn plucks_the_primary_field() {
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
        fn plucks_no_argument_without_the_primary_field() {
            for t in all_tools() {
                assert!(
                    t.summarize_input(&serde_json::json!({})).is_none(),
                    "tool {} returned Some on empty input",
                    t.spec().name,
                );
            }
        }
    }

    mod summarize_call {
        use super::*;

        #[test]
        fn wraps_arg_in_the_tool_label() {
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
                    serde_json::json!({
                        "file_path": "/a/b.rs",
                        "old_string": "x",
                        "new_string": "y",
                    }),
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
        fn falls_back_to_bare_name_when_arg_missing() {
            for t in all_tools() {
                let name = t.spec().name;
                let got = t.summarize_call(&name, &serde_json::json!({}), Path::new("/repo"));
                assert_eq!(got, pascal_case(&name), "tool {name}");
            }
        }

        #[test]
        fn shortens_file_paths_inside_cwd() {
            let path = "/repo/crates/ox-core/src/tool/read.rs";
            let cases = [
                (
                    "edit",
                    serde_json::json!({"file_path": path, "old_string": "x", "new_string": "y"}),
                    "Edit(crates/ox-core/src/tool/read.rs)",
                ),
                (
                    "read",
                    serde_json::json!({"file_path": path}),
                    "Read(crates/ox-core/src/tool/read.rs)",
                ),
                (
                    "write",
                    serde_json::json!({"file_path": path, "content": "x"}),
                    "Write(crates/ox-core/src/tool/read.rs)",
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
    }

    mod approval_preview {
        use super::*;

        #[tokio::test]
        async fn default_titles_with_the_tool_name() {
            let preview = GlobTool::default()
                .approval_preview(
                    "glob",
                    &serde_json::json!({"pattern": "*.rs"}),
                    Path::new("/repo"),
                )
                .await;
            assert_eq!(preview.title, "Glob");
            assert_eq!(preview.body, ApprovalBody::Command("Glob(*.rs)".to_owned()));
        }
    }

    mod pascal_case {
        use super::*;

        #[test]
        fn capitalizes_each_snake_case_word() {
            assert_eq!(pascal_case("grep"), "Grep");
            assert_eq!(pascal_case("bash_output"), "BashOutput");
            assert_eq!(pascal_case("Foo"), "Foo");
            assert_eq!(pascal_case(""), "");
            assert_eq!(pascal_case("a"), "A");
        }
    }

    mod tool_registry {
        use super::*;

        mod built_in {
            use super::*;

            #[test]
            fn registers_each_tool_in_order() {
                let tools = ToolRegistry::built_in(&ReadFilter::default(), &Arc::default());

                let names: Vec<_> = tools
                    .definitions()
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect();

                assert_eq!(
                    names,
                    [
                        "bash",
                        "bash_output",
                        "bash_stop",
                        "read",
                        "write",
                        "edit",
                        "glob",
                        "grep",
                        "tasks",
                    ],
                );
            }

            #[tokio::test]
            async fn shares_the_background_shells_among_the_bash_tools() {
                let shells = Arc::new(BackgroundShells::default());
                let tools = ToolRegistry::built_in(&ReadFilter::default(), &shells);
                let ctx = temp_ctx();

                let started = tools
                    .run_input(
                        "bash",
                        serde_json::json!({"command": "echo hi", "run_in_background": true}),
                        &ctx,
                    )
                    .await;
                assert!(!started.is_error, "{}", started.content);
                let listed: Vec<_> = shells.list().into_iter().map(|s| s.id).collect();
                let read = tools
                    .run_input(
                        "bash_output",
                        serde_json::json!({"shell_id": "shell-1", "timeout": 5_000}),
                        &ctx,
                    )
                    .await;
                let stopped = tools
                    .run_input(
                        "bash_stop",
                        serde_json::json!({"shell_id": "shell-1"}),
                        &ctx,
                    )
                    .await;

                assert_eq!(listed, ["shell-1"]);
                assert_eq!(read.content, "hi\n\n(exit code 0)");
                assert_eq!(
                    stopped.content,
                    "(no new output)\n\n(stopped after it exited with code 0)"
                );
                assert_eq!(shells.list(), []);
            }

            #[tokio::test]
            async fn hands_the_read_filter_to_the_search_tools() {
                let dir = canonical_tempdir();
                std::fs::write(dir.path().join("notes.txt"), "token").unwrap();
                std::fs::write(dir.path().join("secret.txt"), "token").unwrap();
                let tools = ToolRegistry::built_in(&deny_reads(&["secret.txt"]), &Arc::default());
                let ctx = ctx(dir.path());

                for (tool, pattern) in [("glob", "*.txt"), ("grep", "token")] {
                    let input = serde_json::json!({"pattern": pattern});
                    let output = tools.run_input(tool, input, &ctx).await;

                    assert!(!output.is_error, "{tool}: {}", output.content);
                    assert!(
                        output.content.contains("notes.txt"),
                        "{tool}: {}",
                        output.content
                    );
                    assert!(
                        !output.content.contains("secret"),
                        "{tool}: {}",
                        output.content
                    );
                    assert!(
                        output
                            .content
                            .ends_with("(1 file withheld by read deny or ask rules)"),
                        "{tool}: {}",
                        output.content
                    );
                }
            }
        }

        mod get {
            use super::*;

            #[test]
            fn finds_registered_tool() {
                let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
                assert_eq!(registry.get("bash").unwrap().0.name, "bash");
            }

            #[test]
            fn unknown_tool_has_no_entry() {
                let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
                assert!(registry.get("nonexistent").is_none());
            }
        }

        mod definitions {
            use super::*;

            #[test]
            fn produces_tool_with_valid_schema() {
                let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
                let defs = registry.definitions();
                assert_eq!(defs.len(), 1);
                assert_eq!(defs[0].name, "bash");
                let schema = &defs[0].input_schema;
                assert_eq!(schema["type"], "object");
                assert!(schema["properties"]["command"].is_object());
                assert_eq!(schema["required"], serde_json::json!(["command"]));
            }
        }

        mod icon {
            use super::*;

            #[test]
            fn delegates_to_registered_tool() {
                let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
                assert_eq!(registry.icon("bash"), "$");
            }

            #[test]
            fn unknown_tool_falls_back_to_default() {
                let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
                assert_eq!(registry.icon("nonexistent"), DEFAULT_TOOL_ICON);
            }
        }

        mod label {
            use super::*;

            #[test]
            fn delegates_to_registered_tool() {
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
            fn unknown_tool_falls_back_to_raw_name() {
                let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);
                let input = serde_json::json!({"command": "echo hi"});
                assert_eq!(
                    registry.label("nonexistent", &input, Path::new("/repo")),
                    "nonexistent"
                );
            }
        }

        mod result_view {
            use super::*;

            #[test]
            fn delegates_to_tool_for_structured_output() {
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
            fn delegates_grep_matches() {
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
            fn delegates_read_excerpt() {
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
            fn delegates_glob_files() {
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
            fn short_circuits_errors_to_text() {
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
            fn falls_back_to_text_for_unknown_tool() {
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
            fn falls_back_to_text_when_tool_has_no_structured_view() {
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
        }
    }

    mod run_capped {
        use super::*;

        /// A tool whose every call returns its text.
        struct Fixed(String);

        impl Tool for Fixed {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "fixed".to_owned(),
                    description: "return fixed text".to_owned(),
                    input_schema: serde_json::json!({"type": "object"}),
                    class: PermissionClass::Read,
                    parallel_safe: true,
                }
            }

            fn run<'a>(
                &'a self,
                _prepared: Prepared,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
                Box::pin(std::future::ready(ToolOutput::success(self.0.clone())))
            }
        }

        async fn run_fixed(text: &str, ctx: &ToolCtx) -> ToolOutput {
            let registry = ToolRegistry::new(vec![Box::new(Fixed(text.to_owned()))]);
            registry
                .run_input("fixed", serde_json::json!({}), ctx)
                .await
        }

        fn oversize_grep_input() -> (tempfile::TempDir, serde_json::Value) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("big.txt");
            let line = "x".repeat(400);
            std::fs::write(&path, format!("{line}\n").repeat(500)).unwrap();
            let input = serde_json::json!({"pattern": "x", "path": path, "head_limit": 0});
            (dir, input)
        }

        #[tokio::test]
        async fn spills_an_oversized_result_and_returns_its_head_tail_and_path() {
            let dir = tempfile::tempdir().unwrap();
            let full = (0..25_600).fold(String::new(), |mut full, i| {
                _ = writeln!(full, "line {i:06}");
                full
            });
            assert_eq!(full.len(), 300 << 10);

            let output = run_fixed(&full, &ctx(dir.path())).await;

            let (head, marker, tail) = split_capped(&output.content);
            assert_eq!(head.len(), tail.len());
            assert!(full.starts_with(head) && full.ends_with(tail));
            let omitted = full.len() - head.len() - tail.len();
            let path = spill_path(dir.path(), ".txt");
            assert_eq!(
                marker,
                format!(
                    "{omitted} bytes omitted, full output saved to {}",
                    path.display()
                ),
            );
            assert!((MAX_OUTPUT_BYTES - 1..=MAX_OUTPUT_BYTES).contains(&output.content.len()));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), full);
            assert_eq!(output.metadata.truncated_bytes, Some(full.len()));
        }

        #[tokio::test]
        async fn within_cap_leaves_content_and_metadata_untouched_and_writes_nothing() {
            let dir = tempfile::tempdir().unwrap();
            let text = "x".repeat(MAX_OUTPUT_BYTES);

            let output = run_fixed(&text, &ctx(dir.path())).await;

            assert_eq!(output.content, text);
            assert!(output.metadata.truncated_bytes.is_none());
            assert!(!dir.path().join("session").exists());
        }

        #[tokio::test]
        async fn byte_cap_on_grep_keeps_the_row_count_apart_and_falls_through_to_text_view() {
            let (dir, input) = oversize_grep_input();
            let registry = ToolRegistry::new(vec![Box::new(GrepTool::default())]);

            let output = registry
                .run_input("grep", input.clone(), &ctx(dir.path()))
                .await;

            let spilled = std::fs::read(spill_path(dir.path(), ".txt")).unwrap();
            assert_eq!(output.metadata.truncated_bytes, Some(spilled.len()));
            assert!(output.metadata.truncated_total.is_none());
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

        #[tokio::test]
        async fn binary_bash_output_fits_the_cap_with_only_its_pipe_spill() {
            // Each byte decodes as the three bytes of U+FFFD.
            let dir = tempfile::tempdir().unwrap();
            let bytes = vec![0xFF; 300 << 10];
            std::fs::write(dir.path().join("image.bin"), &bytes).unwrap();
            let registry = ToolRegistry::new(vec![Box::new(BashTool::default())]);

            let output = registry
                .run_input(
                    "bash",
                    serde_json::json!({"command": "cat image.bin"}),
                    &ctx(dir.path()),
                )
                .await;

            assert!(!output.is_error, "{}", output.content);
            assert!(output.metadata.truncated_bytes.is_none());
            let stdout = spill_path(dir.path(), ".stdout.txt");
            let note = format!("full stdout saved to {}] ...", stdout.display());
            assert!(output.content.contains(&note));
            assert!(
                std::fs::read(&stdout).unwrap() == bytes,
                "the spill holds the raw bytes"
            );
            let spills: Vec<_> = std::fs::read_dir(stdout.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(spills, [stdout]);
        }
    }

    mod cap_output {
        use super::*;

        #[test]
        fn cuts_both_ends_at_char_boundaries() {
            // Wherever the cuts fall, three of the four offsets put each one inside a 4-byte char.
            for offset in 0..4 {
                let dir = tempfile::tempdir().unwrap();
                let pad = "a".repeat(offset);
                let content = format!("{pad}{}{pad}", "🦀".repeat(MAX_OUTPUT_BYTES / 2));

                let out = cap_output(&content, ctx(dir.path()).spills().file(".txt"));

                let (head, marker, tail) = split_capped(&out);
                assert!(content.starts_with(head) && content.ends_with(tail));
                let omitted = content.len() - head.len() - tail.len();
                assert!(marker.starts_with(&format!("{omitted} bytes omitted, ")));
                assert!(
                    out.len() <= MAX_OUTPUT_BYTES,
                    "offset {offset}: {}",
                    out.len()
                );
            }
        }

        #[test]
        fn fits_the_cap_beside_a_long_spill_path() {
            let dir = tempfile::tempdir().unwrap();
            let cwd = (0..3).fold(dir.path().to_owned(), |cwd, _| cwd.join("d".repeat(100)));
            let content = "x".repeat(MAX_OUTPUT_BYTES + 1);

            let out = cap_output(&content, ctx(&cwd).spills().file(".txt"));

            let path = spill_path(&cwd, ".txt");
            assert!(path.as_os_str().len() > 300);
            assert!(out.len() <= MAX_OUTPUT_BYTES, "{}", out.len());
            let (_, marker, _) = split_capped(&out);
            assert!(marker.ends_with(&format!("full output saved to {}", path.display())));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }

        #[test]
        fn names_why_the_spill_failed_in_place_of_its_path() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("session"), "a file").unwrap();
            let content = "x".repeat(MAX_OUTPUT_BYTES * 2);

            let out = cap_output(&content, ctx(dir.path()).spills().file(".txt"));

            let (_, marker, _) = split_capped(&out);
            let path = spill_path(dir.path(), ".txt");
            let failed = format!("saving the full output to {} failed: ", path.display());
            assert!(marker.contains(&failed), "{marker}");
            assert!(out.len() <= MAX_OUTPUT_BYTES, "{}", out.len());
        }
    }

    mod spill_stem {
        use super::*;

        #[test]
        fn replaces_characters_that_could_leave_the_spill_directory() {
            assert_eq!(spill_stem("toolu_01Ab-9"), "toolu_01Ab-9");
            assert_eq!(spill_stem("../a/b\\c.d"), "___a_b_c_d");
        }
    }

    mod spill {
        use super::*;

        mod write {
            use super::*;

            #[test]
            fn keeps_at_most_the_size_cap_and_says_so() {
                let dir = tempfile::tempdir().unwrap();
                let mut spill = ctx(dir.path()).spills().file(".stdout.txt");
                let chunk = vec![b'a'; 1 << 20];

                for _ in 0..=MAX_SPILL_BYTES >> 20 {
                    spill.write(&chunk);
                }

                let path = spill_path(dir.path(), ".stdout.txt");
                let spilled = std::fs::metadata(&path).unwrap().len();
                assert_eq!(spilled, MAX_SPILL_BYTES);
                assert_eq!(
                    spill.note("stdout"),
                    format!("first 10 MiB of stdout saved to {}", path.display()),
                );
            }

            #[test]
            fn names_the_error_of_a_write_the_file_refuses() {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("toolu_test.stdout.txt");
                std::fs::write(&path, "").unwrap();
                let read_only = || std::fs::File::open(&path).unwrap();
                let mut spill = Spill {
                    path: path.clone(),
                    state: SpillState::Open {
                        file: read_only(),
                        written: 0,
                        full: false,
                    },
                };

                spill.write(b"x");

                let error = read_only().write_all(b"x").unwrap_err();
                assert_eq!(
                    spill.note("stdout"),
                    format!(
                        "saving the full stdout to {} failed: {error}",
                        path.display()
                    ),
                );
            }
        }
    }

    mod create_spill_file {
        use super::*;

        #[test]
        fn refuses_to_replace_an_existing_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("toolu_test.txt");
            std::fs::write(&path, "earlier").unwrap();

            assert!(create_spill_file(&path).is_err());

            assert_eq!(std::fs::read_to_string(&path).unwrap(), "earlier");
        }

        #[cfg(unix)]
        #[test]
        fn creates_a_private_file_in_a_private_directory() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let path = spill_path(dir.path(), ".txt");

            create_spill_file(&path).unwrap();

            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
    }

    mod search_root {
        use super::*;

        mod new {
            use super::*;

            #[test]
            fn resolves_relative_and_absolute_paths_and_keeps_their_spelling() {
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

            #[test]
            fn defaults_to_the_cwd_without_a_path() {
                let dir = canonical_tempdir();

                let root = search_root(dir.path(), None).unwrap();

                assert_eq!(
                    (root.base, root.spelled),
                    (dir.path().to_owned(), PathBuf::from(".")),
                );
            }

            #[test]
            fn missing_path_is_named_relative_to_cwd() {
                let dir = canonical_tempdir();

                let err = search_root(dir.path(), Some("missing")).err().unwrap();

                let cause = std::fs::metadata(dir.path().join("missing")).unwrap_err();
                assert_eq!(err, format!("Failed to access missing: {cause}"));
            }

            #[test]
            fn refuses_an_access_that_is_not_a_path() {
                let workspace = Arc::new(Workspace::new(Path::new("/repo")));
                let err = SearchRoot::new(&Access::None, Some("src"), workspace)
                    .err()
                    .unwrap();
                assert_eq!(
                    err,
                    "Failed to access src: the search root was not resolved."
                );
            }

            #[cfg(unix)]
            #[test]
            fn resolves_dot_dot_after_a_symlink_physically() {
                let dir = canonical_tempdir();
                std::fs::create_dir_all(dir.path().join("real/target")).unwrap();
                let cwd = dir.path().join("cwd");
                std::fs::create_dir(&cwd).unwrap();
                std::os::unix::fs::symlink(dir.path().join("real/target"), cwd.join("link"))
                    .unwrap();

                let root = search_root(&cwd, Some("link/..")).unwrap();

                assert_eq!(root.base, dir.path().join("real"));
            }

            #[cfg(target_os = "linux")]
            #[test]
            fn walks_from_a_root_whose_name_is_not_utf8() {
                use std::os::unix::ffi::OsStrExt as _;
                let dir = canonical_tempdir();
                let cwd = dir.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
                std::fs::create_dir(&cwd).unwrap();

                let root = search_root(&cwd, None).unwrap();

                assert_eq!(root.base, cwd);
            }
        }
    }

    mod tracker_key {
        use super::*;

        #[tokio::test]
        async fn resolves_dotted_spellings_to_one_key() {
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

        #[tokio::test]
        async fn missing_path_falls_back_to_its_lexically_clean_form() {
            let dir = canonical_tempdir();
            assert_eq!(
                tracker_key(&dir.path().join("missing/../a.rs")).await,
                dir.path().join("a.rs")
            );
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn resolves_a_symlinked_spelling_to_the_target() {
            let dir = canonical_tempdir();
            std::fs::create_dir(dir.path().join("real")).unwrap();
            std::fs::write(dir.path().join("real/a.rs"), "").unwrap();
            std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();

            assert_eq!(
                tracker_key(&dir.path().join("link/a.rs")).await,
                dir.path().join("real/a.rs"),
            );
        }
    }

    mod display_path {
        use super::*;

        #[test]
        fn relative_inside_base() {
            let base = Path::new("/home/user/project");
            let path = Path::new("/home/user/project/src/main.rs");
            assert_eq!(display_path(path, base), "src/main.rs");
        }

        #[test]
        fn outside_base_stays_absolute() {
            let base = Path::new("/home/user/project");
            let path = Path::new("/etc/config.toml");
            assert_eq!(display_path(path, base), "/etc/config.toml");
        }

        #[test]
        fn same_path_is_filename() {
            let base = Path::new("/home/user/project/src/main.rs");
            let path = Path::new("/home/user/project/src/main.rs");
            assert_eq!(display_path(path, base), "main.rs");
        }
    }

    mod file_name {
        use super::*;

        #[test]
        fn extracts_basename() {
            assert_eq!(file_name("/home/user/project/src/main.rs"), "main.rs");
        }

        #[test]
        fn bare_name_unchanged() {
            assert_eq!(file_name("README.md"), "README.md");
        }
    }

    mod read_validated {
        use super::*;

        #[tokio::test]
        async fn reads_a_regular_file_but_refuses_a_symlink() {
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
            #[cfg(unix)]
            let cause = std::io::Error::from_raw_os_error(nix::errno::Errno::ELOOP as i32);
            #[cfg(windows)]
            let cause = "the final path component is a symlink";
            assert_eq!(err, format!("Failed to read link.txt: {cause}"));
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn rechecks_a_file_swapped_after_the_path_check() {
            // Each swap runs once the path check passes, as a racing writer's would: a larger file
            // the size limit must catch, then a fifo the open must not block on.
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
    }

    mod write_no_follow {
        use super::*;

        #[tokio::test]
        async fn overwrite_truncates_a_regular_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file.txt");
            std::fs::write(&path, "a much longer original").unwrap();

            write_no_follow(&path, b"short", WriteMode::Overwrite)
                .await
                .unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "short");
        }

        #[tokio::test]
        async fn create_new_refuses_existing_entries_and_dangling_links() {
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
        async fn overwrite_refuses_a_symlink() {
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
        async fn overwrite_refuses_a_fifo_without_blocking() {
            let dir = tempfile::tempdir().unwrap();
            let fifo = dir.path().join("pipe");
            nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();

            let write = write_no_follow(&fifo, b"x", WriteMode::Overwrite);
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), write).await;
            assert!(result.expect("the open must not block").is_err());
        }
    }

    #[cfg(windows)]
    mod open_no_follow {
        use super::*;

        #[tokio::test]
        async fn reads_and_rewrites_a_compressed_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("notes.txt");
            let content = "a compressible line\n".repeat(4096);
            std::fs::write(&path, &content).unwrap();
            // `compact /EXE` stores the data compressed behind the WOF filter, which hides the
            // file's reparse point from user mode, so the no-follow open keeps its first handle.
            let compact = std::process::Command::new("compact.exe")
                .args(["/C", "/EXE:XPRESS4K"])
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                compact.status.success(),
                "{}",
                String::from_utf8_lossy(&compact.stdout)
            );

            let accept = |_: &std::fs::Metadata| Ok(());
            let (_, bytes) = read_validated(&path, "notes.txt", accept).await.unwrap();
            assert_eq!(bytes, content.as_bytes());
            write_no_follow(&path, b"rewritten", WriteMode::Overwrite)
                .await
                .unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"rewritten");
        }
    }

    mod walk_files {
        use super::*;

        #[test]
        fn stops_once_cancelled() {
            let dir = canonical_tempdir();
            for name in ["a.txt", "b.txt", "c.txt"] {
                std::fs::write(dir.path().join(name), "").unwrap();
            }
            let cancel = CancellationToken::new();
            let mut unreadable = Vec::new();
            let mut walk = walk_files(dir.path(), &cancel, &mut unreadable);

            let first = walk.next();
            cancel.cancel();

            assert!(first.is_some(), "an uncancelled walk yields files");
            assert_eq!(walk.count(), 0, "the walk stops at the cancel");
        }

        #[test]
        fn records_a_directory_it_cannot_read_and_walks_on() {
            let dir = canonical_tempdir();
            let readable = dir.path().join("a.txt");
            std::fs::write(&readable, "").unwrap();
            let locked = dir.path().join("locked");
            std::fs::create_dir(&locked).unwrap();
            std::fs::write(locked.join("b.txt"), "").unwrap();
            let _denied = deny_read_access(&locked);
            let mut unreadable = Vec::new();

            let files: Vec<_> = walk_files(dir.path(), &CancellationToken::new(), &mut unreadable)
                .map(ignore::DirEntry::into_path)
                .collect();

            let cause = std::fs::read_dir(&locked).unwrap_err().to_string();
            assert_eq!(files, [readable]);
            assert_eq!(
                unreadable,
                [Unreadable {
                    path: Some(locked),
                    error: cause,
                }]
            );
        }
    }

    mod unreadable {
        use super::*;

        mod from_walk {
            use super::*;

            #[test]
            fn keeps_only_errors_that_hide_files() {
                let denied = || ignore::Error::Io(std::io::Error::other("denied"));
                let at_depth = |err| ignore::Error::WithDepth {
                    depth: 1,
                    err: Box::new(err),
                };
                let cases = [
                    (
                        ignore::Error::WithPath {
                            path: "locked".into(),
                            err: Box::new(at_depth(denied())),
                        },
                        Some((Some("locked"), "denied")),
                    ),
                    (at_depth(denied()), Some((None, "denied"))),
                    (
                        ignore::Error::WithPath {
                            path: "gone".into(),
                            err: Box::new(at_depth(ignore::Error::Io(
                                std::io::ErrorKind::NotFound.into(),
                            ))),
                        },
                        None,
                    ),
                    (
                        ignore::Error::WithPath {
                            path: ".gitignore".into(),
                            err: Box::new(denied()),
                        },
                        None,
                    ),
                    (ignore::Error::Partial(vec![denied(), denied()]), None),
                ];

                for (e, expected) in cases {
                    let expected = expected.map(|(path, error): (Option<&str>, &str)| Unreadable {
                        path: path.map(PathBuf::from),
                        error: error.to_owned(),
                    });
                    assert_eq!(Unreadable::from_walk(&e), expected, "{e}");
                }
            }
        }
    }

    mod withhold_unreadable {
        use super::*;

        #[test]
        fn drops_and_counts_only_the_paths_the_filter_withholds() {
            let dir = canonical_tempdir();
            let root = search_root(dir.path(), None).unwrap();
            let read_filter = deny_reads(&["secrets"]);
            let read_filter = read_filter.walk(&root.base, &root.spelled, &root.workspace);
            let at = |path: Option<&str>| Unreadable {
                path: path.map(|path| dir.path().join(path)),
                error: "denied".to_owned(),
            };
            let mut unreadable = vec![at(Some("secrets")), at(Some("src")), at(None)];

            let withheld = withhold_unreadable(&mut unreadable, &read_filter);

            assert_eq!(withheld, 1);
            assert_eq!(unreadable, [at(Some("src")), at(None)]);
        }
    }

    mod newest_first {
        use super::*;

        #[test]
        fn orders_by_mtime_and_keeps_input_order_among_ties() {
            let dir = canonical_tempdir();
            for (name, secs) in [
                ("a.txt", 1_000_000),
                ("b.txt", 1_000_060),
                ("c.txt", 1_000_000),
            ] {
                create_modified_at(&dir.path().join(name), secs);
            }
            let mut unreadable = Vec::new();
            let mut entries: Vec<_> =
                walk_files(dir.path(), &CancellationToken::new(), &mut unreadable).collect();
            entries.sort_by_key(|entry| Reverse(entry.file_name().to_owned()));

            let sorted = newest_first(entries.into_iter());

            let names: Vec<_> = sorted
                .iter()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["b.txt", "c.txt", "a.txt"]);
        }
    }

    mod run_blocking {
        use super::*;

        #[tokio::test]
        async fn reports_a_panic_as_an_internal_error() {
            let err = run_blocking(|| -> Result<(), String> { panic!("boom") })
                .await
                .unwrap_err();

            let (task, message) = err.split_once(" panicked ").unwrap();
            assert!(task.starts_with("Internal error: task "), "{err}");
            assert_eq!(message, "with message \"boom\"");
        }
    }

    mod truncate_line {
        use super::*;

        const MAX_CHARS: usize = 500;

        #[test]
        fn short_unchanged() {
            assert_eq!(truncate_line("hello", MAX_CHARS).as_ref(), "hello");
        }

        #[test]
        fn long_text_ends_with_ellipsis() {
            let long_line = "x".repeat(MAX_CHARS + 100);
            let result = truncate_line(&long_line, MAX_CHARS);
            let suffix = format!("... [{} chars]", MAX_CHARS + 100);
            assert_eq!(result.len(), MAX_CHARS + suffix.len());
            assert_eq!(&result[..MAX_CHARS], "x".repeat(MAX_CHARS));
            assert_eq!(&result[MAX_CHARS..], suffix);
        }

        #[test]
        fn multibyte_text_preserves_character_boundaries() {
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
        fn multibyte_under_char_cap_unchanged() {
            let line = "é".repeat(MAX_CHARS);
            let result = truncate_line(&line, MAX_CHARS);
            assert!(matches!(result, Cow::Borrowed(_)));
            assert_eq!(result.as_ref(), line);
        }
    }

    mod push_path_note {
        use super::*;

        #[test]
        fn names_up_to_the_cap_and_counts_the_rest() {
            let lines: Vec<String> = (1..=13).map(|i| format!("{i}.rs")).collect();
            let listed = indoc! {"
                a.rs

                Header:
                  1.rs
                  2.rs
                  3.rs
                  4.rs
                  5.rs
                  6.rs
                  7.rs
                  8.rs
                  9.rs
                  10.rs"
            };
            for (count, expected) in [
                (10, listed.to_owned()),
                (13, format!("{listed}\n  ... and 3 more")),
            ] {
                let mut content = "a.rs".to_owned();

                push_path_note(&mut content, "Header:", &lines[..count]);

                assert_eq!(content, expected);
            }

            let mut content = "a.rs".to_owned();
            push_path_note(&mut content, "Header:", &[]);
            assert_eq!(content, "a.rs");
        }
    }

    mod push_unreadable_note {
        use super::*;

        #[test]
        fn names_each_path_relative_to_the_cwd() {
            let cwd = Path::new("/repo");
            let unreadable = [
                Unreadable {
                    path: Some(cwd.join("locked")),
                    error: "Permission denied (os error 13)".to_owned(),
                },
                Unreadable {
                    path: None,
                    error: "Input/output error (os error 5)".to_owned(),
                },
            ];
            let mut content = "a.rs".to_owned();

            push_unreadable_note(&mut content, &unreadable, cwd);

            assert_eq!(
                content,
                indoc! {"
                    a.rs

                    Could not read:
                      locked: Permission denied (os error 13)
                      Input/output error (os error 5)"
                }
            );
            assert_eq!(strip_search_notes(&content), "a.rs");
        }
    }

    mod push_withheld_note {
        use super::*;

        #[test]
        fn counts_files_and_strips_back_off() {
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

                        {note}"
                    }
                );
                assert_eq!(strip_search_notes(&content), "a.rs");
            }

            let mut content = "a.rs".to_owned();
            push_withheld_note(&mut content, 0);
            assert_eq!(content, "a.rs");
        }
    }

    mod strip_search_notes {
        use super::*;

        #[test]
        fn strips_both_notes_and_keeps_other_sections() {
            let notes = indoc! {"
                a.rs

                (Showing 100 of 120 matches. Use a more specific pattern.)

                Could not read:
                  locked: Permission denied (os error 13)

                (1 file withheld by read deny or ask rules)"
            };
            assert_eq!(
                strip_search_notes(notes),
                indoc! {"
                    a.rs

                    (Showing 100 of 120 matches. Use a more specific pattern.)"
                }
            );

            for content in [
                "a.rs",
                indoc! {"
                    a.rs

                    (Showing 100 of 120 matches. Use a more specific pattern.)"
                },
            ] {
                assert_eq!(strip_search_notes(content), content);
            }
        }
    }
}
