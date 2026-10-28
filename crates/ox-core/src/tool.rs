//! The [`Tool`] trait, the [`ToolRegistry`] that looks tools up by name, and helpers shared by the
//! built-in tools.

pub mod bash;
pub(crate) mod edit;
mod file;
pub(crate) mod glob;
pub(crate) mod grep;
pub(crate) mod read;
mod search;
pub(crate) mod spill;
pub mod tasks;
pub mod view;
pub(crate) mod write;

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use self::bash::BashTool;
use self::bash::background::{BackgroundShells, BashOutputTool, BashStopTool};
use self::edit::EditTool;
use self::glob::GlobTool;
use self::grep::GrepTool;
use self::read::ReadTool;
use self::spill::Spills;
use self::tasks::{Task, TasksTool};
use self::view::{ApprovalBody, ApprovalPreview, ToolResultView};
use self::write::WriteTool;
use crate::file_tracker::FileTracker;
use crate::permission::{Access, PermissionClass, ReadFilter, Workspace};
use crate::util::path::{expand_user, slash_separated};
use crate::util::text::ELLIPSIS;

// ── Tool Spec ──

/// What the model and the gate know of a tool. The registry takes it once, when the tool registers.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// The name, description, and input schema the model sees.
    pub definition: ToolDefinition,
    /// The permission class the gate files the tool's calls under.
    pub class: PermissionClass,
    /// Whether a call may run alongside the other parallel-safe calls of its round, since it
    /// changes nothing another call reads.
    pub parallel_safe: bool,
}

/// The name, description, and input schema of a tool, as the API receives them.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
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

    /// Where the call's spill files go.
    fn spills(&self) -> Spills {
        Spills::new(&self.artifact_dir, &self.tool_use_id)
    }
}

/// A call resolved for the gate and its run: the input with any path pinned to where it resolves,
/// and what the call accesses.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub(crate) input: serde_json::Value,
    pub(crate) access: Access,
}

impl Prepared {
    /// The parsed input and resolved `file_path` of a file tool.
    pub(crate) fn into_path_input<T: DeserializeOwned>(self) -> Result<(T, PathBuf), String> {
        let input = parse_input(self.input)?;
        let Some(path) = self.access.resolved() else {
            return Err("Invalid input: missing field `file_path`".to_owned());
        };
        Ok((input, path.clone()))
    }
}

// ── Tool Output ──

/// Result returned from a tool execution to the agent loop.
#[derive(Debug)]
pub struct ToolOutput {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) metadata: ToolMetadata,
    /// The task list a `tasks` call leaves, which the engine adopts in place of its own.
    pub(crate) tasks: Option<Vec<Task>>,
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

    fn with_result_view(self, view: ToolResultView) -> Self {
        self.with_stored_view(StoredView::View(view))
    }

    /// Marks the content as the whole result body, so it renders as text.
    fn with_text_view(self) -> Self {
        self.with_stored_view(StoredView::Text)
    }

    fn with_stored_view(mut self, view: StoredView) -> Self {
        self.metadata.result_view = Some(view);
        self
    }

    fn with_tasks(mut self, tasks: Vec<Task>) -> Self {
        self.tasks = Some(tasks);
        self
    }
}

/// Structured data for UI display and logging. The model never sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMetadata {
    /// A one-line summary of the call, which the UI shows on its result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    /// `None` when the tool derives its view from the call's input and result on each render.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result_view: Option<StoredView>,
}

/// What a result stores for replay beyond its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredView {
    /// The content renders as text without the tool's own view derivation.
    Text,
    /// Structured view whose paths already use the call's working directory.
    View(ToolResultView),
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
            .map(|path| Access::for_path(tool_path(path), &ctx.workspace))
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

    /// Pulls the primary argument of a tool without a [`Self::path_field`] out of `input` for terse
    /// `Name(arg)` labels in the UI. Returning `None` falls back to the bare name.
    fn summarize_input<'a>(&self, _input: &'a serde_json::Value) -> Option<&'a str> {
        None
    }

    /// The label the UI shows for a call of the tool registered as `name`. By default it is
    /// `Name(arg)`, where `arg` is the [`Self::path_field`] relative to `cwd` or else the
    /// [`Self::summarize_input`] argument, and the bare name when the call has neither.
    fn summarize_call(&self, name: &str, input: &serde_json::Value, cwd: &Path) -> String {
        let label = pascal_case(name);
        let arg = match self.path_field() {
            Some(field) => extract_input_field(input, field)
                .map(|path| Cow::Owned(display_path(&tool_path(path), cwd))),
            None => self.summarize_input(input).map(Cow::Borrowed),
        };
        match arg {
            Some(arg) => format!("{label}({arg})"),
            None => label,
        }
    }

    /// Returns the structured shape of a successful tool result for richer UI rendering, with paths
    /// shown relative to `cwd`, or `None` to fall back to [`ToolResultView::Text`]. The registry
    /// calls it only for a result that stored no view, and short-circuits errors to text.
    fn result_view(
        &self,
        _input: &serde_json::Value,
        _content: &str,
        _cwd: &Path,
    ) -> Option<ToolResultView> {
        None
    }

    /// Builds the approval modal's preview of `prepared`, by default the tool name over its label
    /// from [`Self::summarize_call`]. Async because the write preview reads the file it would
    /// replace through the resolved path.
    fn approval_preview<'a>(
        &'a self,
        name: &'a str,
        prepared: &'a Prepared,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = ApprovalPreview> + Send + 'a>> {
        Box::pin(std::future::ready(ApprovalPreview {
            title: pascal_case(name),
            body: ApprovalBody::Command(self.summarize_call(name, &prepared.input, cwd)),
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
            .find(|(spec, _)| spec.definition.name == name)
            .map(|(spec, tool)| (spec, tool.as_ref()))
    }

    /// The definitions a request sends, in registration order.
    pub(crate) fn definitions(&self) -> Vec<&ToolDefinition> {
        self.tools
            .iter()
            .map(|(spec, _)| &spec.definition)
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

    /// Resolves the result view for a tool call, with paths relative to `cwd`. Errors, results
    /// marked as plain text, and unknown tools render as [`ToolResultView::Text`].
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
        match &metadata.result_view {
            Some(StoredView::View(view)) => view.clone(),
            Some(StoredView::Text) => ToolResultView::Text {
                content: content.to_owned(),
            },
            None => self
                .get(name)
                .and_then(|(_, tool)| tool.result_view(input, content, cwd))
                .unwrap_or_else(|| ToolResultView::Text {
                    content: content.to_owned(),
                }),
        }
    }
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

/// A file tool's path argument with a leading `~` or `~/` expanded to the home directory, as config
/// patterns read it. With no home directory, `~` stays literal.
fn tool_path(raw: &str) -> PathBuf {
    expand_user(raw).unwrap_or_else(|_| PathBuf::from(raw))
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

/// Caps a line at `max_chars` characters, appending `... [N chars]` with the original length so the
/// model can decide whether to read more. Borrows when the line already fits.
fn truncate_line(line: &str, max_chars: usize) -> Cow<'_, str> {
    if line.len() <= max_chars {
        return Cow::Borrowed(line);
    }
    let Some((boundary, _)) = line.char_indices().nth(max_chars) else {
        return Cow::Borrowed(line);
    };
    let total_chars = max_chars + line[boundary..].chars().count();
    Cow::Owned(format!(
        "{}{ELLIPSIS} [{total_chars} chars]",
        &line[..boundary]
    ))
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod local_testing {
    use std::fs::{self, File};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use tokio_util::sync::CancellationToken;

    use super::search::{SearchRoot, search_path};
    use super::spill::run_capped;
    use super::{Prepared, Tool, ToolCtx, ToolOutput, ToolRegistry, ToolSpec};
    use crate::file_tracker::FileTracker;
    use crate::permission::rule::Rule;
    use crate::permission::{Access, Mode, PermissionClass, Policy, ReadFilter, Workspace};

    /// Original-file evidence for one workspace write attempt.
    pub(crate) struct WorkspaceWrite {
        path: PathBuf,
        original: Option<(File, fs::Metadata, Vec<u8>)>,
    }

    impl WorkspaceWrite {
        pub(crate) fn new(path: &Path) -> Self {
            let original = match File::open(path) {
                Ok(file) => {
                    let metadata = file.metadata().unwrap();
                    let bytes = fs::read(path).unwrap();
                    Some((file, metadata, bytes))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => panic!("cannot inspect {}: {error}", path.display()),
            };
            Self {
                path: path.to_owned(),
                original,
            }
        }

        pub(crate) fn assert_unchanged(&self) {
            if let Some((original, metadata, bytes)) = &self.original {
                let current = File::open(&self.path).unwrap();
                assert_eq!(fs::read(&self.path).unwrap(), *bytes);
                assert_eq!(
                    current.metadata().unwrap().permissions(),
                    metadata.permissions()
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt as _;

                    let current = current.metadata().unwrap();
                    let original = original.metadata().unwrap();
                    assert_eq!(
                        (current.dev(), current.ino(), current.uid(), current.gid()),
                        (
                            original.dev(),
                            original.ino(),
                            original.uid(),
                            original.gid()
                        )
                    );
                }
                #[cfg(windows)]
                assert_eq!(
                    crate::util::fs::file_id(&current).unwrap(),
                    crate::util::fs::file_id(original).unwrap()
                );
            } else {
                assert!(!self.path.exists());
            }
            let parent = self.path.parent().unwrap();
            if parent.exists() {
                let temporary: Vec<_> = fs::read_dir(parent)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .filter(|name| {
                        let name = name.to_string_lossy();
                        name.starts_with(".ox-write.") || name.starts_with(".ox-mode.")
                    })
                    .collect();
                assert_eq!(temporary, [] as [std::ffi::OsString; 0]);
            }
        }
    }

    /// A prepared call carrying `input` alone, for tests of a part that does not resolve a path.
    pub(crate) fn prepared(input: serde_json::Value) -> Prepared {
        Prepared {
            input,
            access: Access::default(),
        }
    }

    /// A fresh tracker's context in `cwd`, for call `toolu_test` of session `test-session`, whose
    /// artifact directory is `cwd/session`.
    pub(crate) fn ctx(cwd: &Path) -> ToolCtx {
        ctx_with(cwd, Arc::new(FileTracker::default()))
    }

    /// [`ctx`] in the system temp dir, for calls that name absolute paths. Its artifact directory
    /// is shared, so a call that spills output needs [`ctx`] in a temp dir of its own.
    pub(super) fn temp_ctx() -> ToolCtx {
        ctx(&std::env::temp_dir())
    }

    /// [`ctx`] with `tracker` as the session's tracker.
    pub(super) fn ctx_with(cwd: &Path, tracker: Arc<FileTracker>) -> ToolCtx {
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
    pub(super) fn spill_path(cwd: &Path, suffix: &str) -> PathBuf {
        cwd.join("session")
            .join("tool-results")
            .join(format!("toolu_test{suffix}"))
    }

    /// Prepares `input` for `tool` and runs it in `ctx`, the way dispatch does past the gate.
    pub(super) async fn run(
        tool: &dyn Tool,
        input: serde_json::Value,
        ctx: &ToolCtx,
    ) -> ToolOutput {
        tool.run(tool.prepare(input, ctx), ctx).await
    }

    /// The read filter of a policy that denies reading whatever one of `patterns` matches.
    pub(super) fn deny_reads(patterns: &[&str]) -> ReadFilter {
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
        let access = Access::for_path(search_path(path), &workspace);
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

        /// The registered tools with their specs, in registration order.
        pub(super) fn iter(&self) -> impl Iterator<Item = (&ToolSpec, &dyn Tool)> {
            self.tools.iter().map(|(spec, tool)| (spec, tool.as_ref()))
        }
    }

    /// Creates an empty file at `path`, last modified `secs` seconds after the Unix epoch.
    pub(super) fn create_modified_at(path: &Path, secs: u64) {
        let file = File::create(path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    /// Marks the file at `path` read-only, so opening it for writing fails on every platform.
    pub(super) fn make_read_only(path: &Path) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions).unwrap();
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::Arc;

    use super::bash::BashTool;
    use super::edit::{anchor_line, synthesize_chunk};
    use super::{ApprovalBody, ApprovalPreview, Tool, ToolRegistry};
    use crate::tool::view::EditDiff;

    /// The approval preview of a foreground `bash` call that runs `command`.
    pub fn bash_preview(command: &str) -> ApprovalPreview {
        ApprovalPreview {
            title: "Bash".to_owned(),
            body: ApprovalBody::Command(command.to_owned()),
        }
    }

    /// A registry of `tools` followed by a `bash` tool with background shells of its own.
    pub fn bash_registry(mut tools: Vec<Box<dyn Tool>>) -> ToolRegistry {
        tools.push(Box::new(BashTool::new(Arc::default())));
        ToolRegistry::new(tools)
    }

    /// An edit diff replacing `old` with `new` at line 1, as the TUI tests render it.
    pub fn edit_diff(old: &str, new: &str) -> EditDiff {
        let chunk = synthesize_chunk(old, new);
        let locations = anchor_line(&chunk, 1, 1).into_iter().collect();
        EditDiff { chunk, locations }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::local_testing::{ctx, deny_reads, temp_ctx};
    use super::testing::bash_registry;
    use super::view::NumberedLine;
    use super::*;
    use crate::permission::rule::Rule;
    use crate::permission::{Decision, Mode, PermissionClass, Policy, Zone};
    use crate::util::path::local_testing::canonical_tempdir;
    use crate::util::path::testing::HOME_VAR;

    /// The built-in tools, with no read rules and background shells of their own.
    fn built_in() -> ToolRegistry {
        ToolRegistry::built_in(&ReadFilter::default(), &Arc::default())
    }

    mod spec {
        use super::*;

        #[test]
        fn every_tool_exposes_non_empty_name_description_and_object_schema() {
            for (spec, _) in built_in().iter() {
                let ToolSpec {
                    definition:
                        ToolDefinition {
                            name,
                            description,
                            input_schema,
                        },
                    ..
                } = spec;
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
            let tools = built_in();

            let catalog: Vec<_> = tools
                .iter()
                .map(|(spec, _)| {
                    (
                        spec.definition.name.as_str(),
                        spec.class,
                        spec.parallel_safe,
                    )
                })
                .collect();

            assert_eq!(
                catalog,
                [
                    ("bash", PermissionClass::Exec, false),
                    ("bash_output", PermissionClass::Read, false),
                    ("bash_stop", PermissionClass::Read, false),
                    ("read", PermissionClass::Read, true),
                    ("write", PermissionClass::Edit, false),
                    ("edit", PermissionClass::Edit, false),
                    ("glob", PermissionClass::Read, true),
                    ("grep", PermissionClass::Read, true),
                    ("tasks", PermissionClass::Read, false),
                ],
            );
        }

        #[test]
        fn class_rules_cover_every_tool_in_the_class() {
            let dir = canonical_tempdir();
            fs::write(dir.path().join(".env"), "SECRET=1").unwrap();
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
            let tools = built_in();
            let ctx = ctx(dir.path());
            for (name, input) in &cases {
                let (spec, t) = tools.get(name).unwrap();
                let prepared = t.prepare(input.clone(), &ctx);
                assert_eq!(
                    policy.decide(spec.class, &prepared.access.as_target()),
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
            let tools = built_in();
            for name in ["read", "edit", "write"] {
                let (_, t) = tools.get(name).unwrap();

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

        #[test]
        fn reads_a_leading_tilde_in_a_path_field_as_the_home_directory() {
            let dir = canonical_tempdir();
            let (cwd, home) = (dir.path().join("repo"), dir.path().join("home"));
            fs::create_dir(&cwd).unwrap();
            fs::create_dir(&home).unwrap();
            let tools = built_in();

            temp_env::with_var(HOME_VAR, Some(&home), || {
                let deny = vec![Rule::parse(PermissionClass::Read, "~/.aws/**").unwrap()];
                let ctx = ctx(&cwd);
                let policy = Policy::resolve(
                    Mode::Manual,
                    deny,
                    Vec::new(),
                    Vec::new(),
                    &[],
                    &ctx.workspace,
                )
                .unwrap();
                let (spec, read) = tools.get("read").unwrap();
                let prepare =
                    |path: &str| read.prepare(serde_json::json!({"file_path": path}), &ctx);

                for (path, pinned) in [
                    ("~/notes.md", home.join("notes.md")),
                    ("~", home.clone()),
                    ("~user/x", cwd.join("~user").join("x")),
                ] {
                    assert_eq!(
                        prepare(path).input,
                        serde_json::json!({"file_path": pinned}),
                        "{path}"
                    );
                }
                for path in ["~/.aws/credentials", "~/.ssh/id_ed25519"] {
                    assert_eq!(
                        policy.decide(spec.class, &prepare(path).access.as_target()),
                        Decision::Deny,
                        "{path}"
                    );
                }
            });
        }

        #[tokio::test]
        async fn matches_native_file_access_for_directory_suffixed_paths() {
            for name in ["notes/", "notes/."] {
                let dir = canonical_tempdir();
                let ctx = ctx(dir.path());
                let path = dir.path().join("notes");
                fs::write(&path, "original").unwrap();
                let native = fs::read_to_string(dir.path().join(name));
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
                    fs::read_to_string(&path).unwrap(),
                    if succeeds { "replacement" } else { "original" },
                    "{name}",
                );
            }
        }

        #[tokio::test]
        async fn keeps_missing_directory_paths_from_becoming_files() {
            for name in [
                "missing/",
                "missing/.",
                "missing//",
                "missing/./",
                #[cfg(windows)]
                r"missing\",
                #[cfg(windows)]
                r"missing\.",
                #[cfg(windows)]
                r"missing/\.",
            ] {
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

                assert_eq!(ctx.tracker.snapshot_all(), [], "{name}");
                let entries: Vec<_> = fs::read_dir(dir.path())
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect();
                assert_eq!(entries, [] as [std::ffi::OsString; 0], "{name}");
            }
        }

        #[test]
        fn resolves_a_search_root_but_leaves_its_spelling_in_the_input() {
            // The read deny and ask rules a search's hits face match against the root as the call
            // spells it.
            let dir = canonical_tempdir();
            let input = serde_json::json!({"pattern": "x", "path": "."});

            let prepared =
                GrepTool::new(ReadFilter::default()).prepare(input.clone(), &ctx(dir.path()));

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
            fs::create_dir(&cwd).unwrap();
            fs::create_dir(&outside).unwrap();
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

    mod approval_preview {
        use super::*;

        #[tokio::test]
        async fn default_titles_with_the_tool_name() {
            let preview = GlobTool::new(ReadFilter::default())
                .approval_preview(
                    "glob",
                    &local_testing::prepared(serde_json::json!({"pattern": "*.rs"})),
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
                    .map(|tool| tool.name.as_str())
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
                fs::write(dir.path().join("notes.txt"), "token").unwrap();
                fs::write(dir.path().join("secret.txt"), "token").unwrap();
                let tools = ToolRegistry::built_in(&deny_reads(&["secret.txt"]), &Arc::default());
                let ctx = ctx(dir.path());

                for (tool, pattern, rows) in [
                    ("glob", "*.txt", "notes.txt"),
                    ("grep", "token", "notes.txt:1:token"),
                ] {
                    let input = serde_json::json!({"pattern": pattern});
                    let output = tools.run_input(tool, input, &ctx).await;

                    assert!(!output.is_error, "{tool}: {}", output.content);
                    assert_eq!(
                        output.content,
                        format!("{rows}\n\n(1 file withheld by read deny or ask rules)"),
                        "{tool}",
                    );
                }
            }
        }

        mod definitions {
            use super::*;

            #[test]
            fn lists_each_tool_with_the_spec_it_registered() {
                let registry = built_in();

                let defs = registry.definitions();

                let listed: Vec<_> = defs
                    .iter()
                    .map(|def| (&def.name, &def.description, &def.input_schema))
                    .collect();
                let registered: Vec<_> = registry
                    .iter()
                    .map(|(spec, _)| {
                        (
                            &spec.definition.name,
                            &spec.definition.description,
                            &spec.definition.input_schema,
                        )
                    })
                    .collect();
                assert_eq!(listed, registered);
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
                let tools = built_in();
                for (name, icon) in expected {
                    assert_eq!(tools.icon(name), icon, "tool {name}");
                }
            }

            #[test]
            fn unknown_tool_falls_back_to_default() {
                let registry = bash_registry(Vec::new());
                assert_eq!(registry.icon("nonexistent"), DEFAULT_TOOL_ICON);
            }
        }

        mod label {
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
                let tools = built_in();
                for (name, input, expected) in &cases {
                    assert_eq!(
                        tools.label(name, input, Path::new("/repo")),
                        *expected,
                        "tool {name}",
                    );
                }
            }

            #[test]
            fn names_a_home_path_by_where_it_lands() {
                let tools = built_in();
                let input = serde_json::json!({"file_path": "~/notes.md"});

                let labels = temp_env::with_var(HOME_VAR, Some("/home/u"), || {
                    [
                        tools.label("read", &input, Path::new("/repo")),
                        tools.label("read", &input, Path::new("/home/u")),
                    ]
                });

                assert_eq!(labels, ["Read(/home/u/notes.md)", "Read(notes.md)"]);
            }

            #[test]
            fn falls_back_to_bare_name_when_arg_missing() {
                let tools = built_in();
                for (spec, _) in tools.iter() {
                    let name = &spec.definition.name;
                    let got = tools.label(name, &serde_json::json!({}), Path::new("/repo"));
                    assert_eq!(got, pascal_case(name), "tool {name}");
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
                let tools = built_in();
                for (name, input, expected) in &cases {
                    assert_eq!(
                        tools.label(name, input, Path::new("/repo")),
                        *expected,
                        "tool {name}",
                    );
                }
            }

            #[test]
            fn unknown_tool_falls_back_to_raw_name() {
                let registry = bash_registry(Vec::new());
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
                let registry = ToolRegistry::new(vec![Box::new(ReadTool)]);
                let input = serde_json::json!({"file_path": "/tmp/f.rs"});
                let metadata = ToolMetadata::default();

                let view = registry.result_view(
                    "read",
                    &input,
                    "1\ta\n2\tb",
                    &metadata,
                    false,
                    Path::new("/repo"),
                );

                assert_eq!(
                    view,
                    ToolResultView::ReadExcerpt {
                        path: "/tmp/f.rs".to_owned(),
                        lines: vec![
                            NumberedLine {
                                number: 1,
                                text: "a".to_owned()
                            },
                            NumberedLine {
                                number: 2,
                                text: "b".to_owned()
                            },
                        ],
                        total_lines: 2,
                    },
                );
            }

            #[test]
            fn short_circuits_errors_to_text() {
                let registry = ToolRegistry::new(vec![Box::new(ReadTool)]);
                let input = serde_json::json!({"file_path": "/tmp/f.rs"});
                let metadata = ToolMetadata {
                    result_view: Some(StoredView::View(ToolResultView::GlobFiles {
                        pattern: "*.rs".to_owned(),
                        files: vec!["stale.rs".to_owned()],
                        total: 1,
                    })),
                    ..ToolMetadata::default()
                };

                let view = registry.result_view(
                    "read",
                    &input,
                    "File not found: /tmp/f.rs",
                    &metadata,
                    true,
                    Path::new("/repo"),
                );

                assert_eq!(
                    view,
                    ToolResultView::Text {
                        content: "File not found: /tmp/f.rs".to_owned(),
                    },
                );
            }

            #[test]
            fn stored_view_precedes_the_tools_own_derivation() {
                let registry =
                    ToolRegistry::new(vec![Box::new(GlobTool::new(ReadFilter::default()))]);
                let expected = ToolResultView::GlobFiles {
                    pattern: "*.rs".to_owned(),
                    files: vec!["kept.rs".to_owned()],
                    total: 7,
                };
                let metadata = ToolMetadata {
                    result_view: Some(StoredView::View(expected.clone())),
                    ..ToolMetadata::default()
                };

                let view = registry.result_view(
                    "glob",
                    &serde_json::json!({"pattern": "*.rs"}),
                    "a.rs\nb.rs",
                    &metadata,
                    false,
                    Path::new("/repo"),
                );

                assert_eq!(view, expected);
            }

            #[test]
            fn stored_text_suppresses_the_tools_own_derivation() {
                let registry =
                    ToolRegistry::new(vec![Box::new(GlobTool::new(ReadFilter::default()))]);
                let metadata = ToolMetadata {
                    result_view: Some(StoredView::Text),
                    ..ToolMetadata::default()
                };
                let content = "a.rs\nb.rs";

                let view = registry.result_view(
                    "glob",
                    &serde_json::json!({"pattern": "*.rs"}),
                    content,
                    &metadata,
                    false,
                    Path::new("/repo"),
                );

                assert_eq!(
                    view,
                    ToolResultView::Text {
                        content: content.to_owned(),
                    },
                );
            }

            #[test]
            fn falls_back_to_text_for_unknown_tool() {
                let registry = bash_registry(Vec::new());
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
                let registry = bash_registry(Vec::new());
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

    mod display_path {
        use super::*;

        #[test]
        fn chooses_the_name_from_the_path_relation_to_base() {
            for (case, path, base, expected) in [
                (
                    "inside base",
                    "/home/user/project/src/main.rs",
                    "/home/user/project",
                    "src/main.rs",
                ),
                (
                    "outside base",
                    "/etc/config.toml",
                    "/home/user/project",
                    "/etc/config.toml",
                ),
                (
                    "base itself",
                    "/home/user/project/src/main.rs",
                    "/home/user/project/src/main.rs",
                    "main.rs",
                ),
            ] {
                assert_eq!(
                    display_path(Path::new(path), Path::new(base)),
                    expected,
                    "{case}"
                );
            }
        }
    }

    mod file_name {
        use super::*;

        #[test]
        fn keeps_only_the_trailing_name() {
            for (case, path, expected) in [
                ("full path", "/home/user/project/src/main.rs", "main.rs"),
                ("bare name", "README.md", "README.md"),
            ] {
                assert_eq!(file_name(path), expected, "{case}");
            }
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
        fn borrows_a_line_within_the_cap() {
            assert!(matches!(
                truncate_line("hello", MAX_CHARS),
                Cow::Borrowed("hello")
            ));
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
        fn counts_characters_against_the_cap_not_bytes() {
            let line = "é".repeat(MAX_CHARS);
            let result = truncate_line(&line, MAX_CHARS);

            assert!(matches!(result, Cow::Borrowed(_)));
            assert_eq!(result.as_ref(), line);
        }
    }
}
