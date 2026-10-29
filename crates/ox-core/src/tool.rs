//! The [`Tool`] trait, the [`ToolRegistry`] that looks tools up by name, and helpers shared by the
//! built-in tools.

pub mod bash;
mod diff;
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
use crate::permission::{Access, PathTarget, PermissionClass, ReadFilter, Workspace};
use crate::util::path::{expand_user, slash_separated};
use crate::util::text::capitalize_first;

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

    /// What a call naming the file or directory `path` accesses, with a leading `~` read as the
    /// home directory.
    fn path_access(&self, path: &str) -> Access {
        Access::for_path(tool_path(path), &self.workspace)
    }
}

/// A call resolved for the gate and its run: the input its tool's [`Tool::prepare`] validated, and
/// what the call accesses.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub(crate) input: serde_json::Value,
    pub(crate) access: Access,
}

impl Prepared {
    /// Validates `input` as a `T`, then lets `access` check the parsed input further and resolve
    /// what the call accesses. An error is the message the model reads, `Invalid input: ...` for
    /// input that does not parse.
    fn parse<T: DeserializeOwned>(
        input: serde_json::Value,
        access: impl FnOnce(&T) -> Result<Access, String>,
    ) -> Result<Self, String> {
        let parsed = T::deserialize(&input).map_err(|e| invalid_input(&e))?;
        Ok(Self {
            access: access(&parsed)?,
            input,
        })
    }

    /// The input as the `T` that [`Self::parse`] validated it as.
    fn parsed<T: DeserializeOwned>(&self) -> T {
        T::deserialize(&self.input).expect(VALIDATED)
    }

    /// The `Name(path)` title of a path tool's call: its resolved path relative to the resolved cwd
    /// when it lies below it, and the cwd's own name for the cwd. Any other path shows through
    /// [`display_path`] against `cwd`, so an outside path shows in full, while a path that cannot
    /// resolve, such as one below a file, shows relative to `cwd`.
    fn path_title(&self, name: &str, cwd: &Path) -> String {
        let path = self.path();
        let shown = match &self.access {
            Access::Path(PathTarget {
                relative: Some(relative),
                ..
            }) if relative.is_empty() => file_name(&path).into_owned(),
            Access::Path(PathTarget {
                relative: Some(relative),
                ..
            }) => slash_separated(Path::new(relative)),
            _ => display_path(&path, cwd),
        };
        call_label(name, Some(&shown))
    }

    /// The parsed input and resolved path of a path tool.
    fn into_path_input<T: DeserializeOwned>(self) -> (T, PathBuf) {
        let path = self.path();
        (self.into_input(), path)
    }

    /// The input as the `T` that [`Self::parse`] validated it as.
    fn into_input<T: DeserializeOwned>(self) -> T {
        serde_json::from_value(self.input).expect(VALIDATED)
    }

    /// The resolved path of a path tool.
    fn path(&self) -> PathBuf {
        self.access
            .resolved()
            .expect("a path tool's prepare resolves its path")
    }
}

const VALIDATED: &str = "prepare validated the input as this type";

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
pub(crate) struct ToolMetadata {
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

    /// The input field naming the file a call acts on, if any, which its label shows. A tool that
    /// names one must resolve it in `prepare` through [`ToolCtx::path_access`], since the default
    /// approval preview names the resolved path.
    fn path_field(&self) -> Option<&'static str> {
        None
    }

    /// Validates `input` and resolves what the call accesses from it, once, in the context's
    /// workspace, so the gate checks and the run acts on the same values. An error is the message
    /// the model reads in place of a result, since a call that cannot run needs no approval. The
    /// default accepts any input and accesses [`Access::None`], which only a class-wide rule
    /// matches, so every built-in tool overrides it to parse its input.
    fn prepare(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<Prepared, String> {
        Ok(Prepared {
            input,
            access: Access::None,
        })
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
        let arg = match self.path_field() {
            Some(field) => extract_input_field(input, field)
                .map(|path| Cow::Owned(display_path(&tool_path(path), cwd))),
            None => self.summarize_input(input).map(Cow::Borrowed),
        };
        call_label(name, arg.as_deref())
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
    /// from [`Self::summarize_call`], or over where its path resolved for a tool with a
    /// [`Self::path_field`]. Async because the edit and write previews read the file they would
    /// change through the resolved path.
    fn approval_preview<'a>(
        &'a self,
        name: &'a str,
        prepared: &'a Prepared,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = ApprovalPreview> + Send + 'a>> {
        let label = match self.path_field() {
            Some(_) => prepared.path_title(name, cwd),
            None => self.summarize_call(name, &prepared.input, cwd),
        };
        Box::pin(std::future::ready(ApprovalPreview {
            title: pascal_case(name),
            body: ApprovalBody::Command(label),
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

/// `Name(arg)` for a call of the tool registered as `name`, or the bare `Name` without an `arg`.
fn call_label(name: &str, arg: Option<&str>) -> String {
    let label = pascal_case(name);
    match arg {
        Some(arg) => format!("{label}({arg})"),
        None => label,
    }
}

/// Renders a tool name as the label the UI shows, `bash_output` as `BashOutput`.
fn pascal_case(name: &str) -> String {
    name.split('_').map(capitalize_first).collect()
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

    /// The specs the tools registered with, in registration order.
    pub(crate) fn specs(&self) -> impl Iterator<Item = &ToolSpec> {
        self.tools.iter().map(|(spec, _)| spec)
    }

    /// The definitions a request sends, in registration order.
    pub(crate) fn definitions(&self) -> Vec<&ToolDefinition> {
        self.specs().map(|spec| &spec.definition).collect()
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

/// The `Invalid input: ...` message the model reads for input that does not parse.
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

/// Returns the trailing file name of `path`, or `path` itself when it has none.
fn file_name(path: &Path) -> Cow<'_, str> {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
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

// ── Testing ──

#[cfg(test)]
pub(crate) mod local_testing {
    use std::fs::File;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use strum::VariantArray as _;
    use tokio_util::sync::CancellationToken;

    use super::search::{SearchRoot, search_arg};
    use super::spill::run_capped;
    use super::{Tool, ToolCtx, ToolOutput, ToolRegistry, ToolSpec};
    use crate::file_tracker::FileTracker;
    use crate::permission::defaults::Shipped;
    use crate::permission::rule::Rule;
    use crate::permission::{Mode, PermissionClass, Policy, ReadFilter, Workspace};

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

    /// Prepares valid `input` for `tool` and runs it in `ctx`, the way dispatch does when the gate
    /// allows the call.
    pub(super) async fn run(
        tool: &dyn Tool,
        input: serde_json::Value,
        ctx: &ToolCtx,
    ) -> ToolOutput {
        let prepared = tool.prepare(input, ctx).expect("the input is valid");
        tool.run(prepared, ctx).await
    }

    /// The read filter of a policy in `cwd` that denies reading whatever one of `patterns`
    /// matches, and nothing else.
    pub(super) fn deny_reads(patterns: &[&str], cwd: &Path) -> ReadFilter {
        let deny = patterns
            .iter()
            .map(|pattern| Rule::parse(PermissionClass::Read, pattern).unwrap())
            .collect();
        let workspace = Workspace::new(cwd);
        Policy::resolve(
            Mode::Manual,
            deny,
            Vec::new(),
            Vec::new(),
            Shipped::VARIANTS,
            &workspace,
        )
        .unwrap()
        .read_filter()
    }

    /// The root a search call in `cwd` that names `path` walks, resolved the way the search tools'
    /// `prepare` resolves it.
    pub(super) fn search_root(cwd: &Path, path: Option<&str>) -> Result<SearchRoot, String> {
        let ctx = ctx(cwd);
        let base = ctx.path_access(search_arg(path)).resolved().unwrap();
        SearchRoot::new(base, path, ctx.workspace)
    }

    impl ToolRegistry {
        /// [`run`] for valid input to the tool registered as `name`, through the output cap.
        pub(crate) async fn run_input(
            &self,
            name: &str,
            input: serde_json::Value,
            ctx: &ToolCtx,
        ) -> ToolOutput {
            let (_, tool) = self.get(name).expect("tool is registered");
            let prepared = tool.prepare(input, ctx).expect("the input is valid");
            run_capped(tool, prepared, ctx).await
        }

        /// The registered tools with their specs, in registration order.
        pub(super) fn iter(&self) -> impl Iterator<Item = (&ToolSpec, &dyn Tool)> {
            self.tools.iter().map(|(spec, tool)| (spec, tool.as_ref()))
        }
    }

    /// Writes `contents` to `path`, last modified `secs` seconds after the Unix epoch.
    pub(super) fn write_modified_at(path: &Path, contents: &str, secs: u64) {
        let mut file = File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::Arc;

    use super::bash::BashTool;
    use super::diff::{Change, diff_with_changes};
    use super::{ApprovalBody, ApprovalPreview, Tool, ToolRegistry};
    use crate::tool::view::{DiffLine, DiffLineKind, EditDiff};

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

    /// An edit diff replacing the whole text `old` with `new`, as the frontend tests render it.
    pub fn edit_diff(old: &str, new: &str) -> EditDiff {
        let (diff, changes) = diff_with_changes(old, new);
        EditDiff {
            diff,
            locations: changes.first().map(Change::location).into_iter().collect(),
        }
    }

    /// A diff row of `kind` with no changed spans.
    pub fn diff_line(kind: DiffLineKind, number: usize, text: &str) -> DiffLine {
        DiffLine {
            kind,
            number,
            text: text.to_owned(),
            changed: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use serde_json::Value;
    use strum::VariantArray as _;

    use super::local_testing::{ctx, deny_reads, temp_ctx};
    use super::testing::bash_registry;
    use super::view::NumberedLine;
    use super::*;
    use crate::permission::defaults::Shipped;
    use crate::permission::rule::Rule;
    use crate::permission::{Decision, Mode, PathTarget, PermissionClass, Policy, Zone};
    use crate::util::fs::testing::{symlink_dir, symlink_file};
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
                assert_eq!(
                    input_schema["additionalProperties"], false,
                    "{name}: schema.additionalProperties"
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
            let policy = Policy::resolve(
                Mode::Manual,
                deny.to_vec(),
                Vec::new(),
                Vec::new(),
                Shipped::VARIANTS,
                &Workspace::new(dir.path()),
            )
            .unwrap();
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
                let prepared = t.prepare(input.clone(), &ctx).unwrap();
                assert_eq!(
                    policy.decide(spec.class, &prepared.access),
                    Decision::Deny,
                    "tool {name}",
                );
            }
        }
    }

    mod prepare {
        use super::*;

        #[test]
        fn resolves_the_path_of_each_file_tool_and_keeps_its_input() {
            let dir = canonical_tempdir();
            let ctx = ctx(dir.path());
            let resolved = dir.path().join("a.rs");
            let tools = built_in();
            for (name, input) in [
                ("read", serde_json::json!({"file_path": "a.rs"})),
                (
                    "edit",
                    serde_json::json!({"file_path": "a.rs", "old_string": "a", "new_string": "b"}),
                ),
                (
                    "write",
                    serde_json::json!({"file_path": "a.rs", "content": "a"}),
                ),
            ] {
                let (_, t) = tools.get(name).unwrap();

                let prepared = t.prepare(input.clone(), &ctx).unwrap();

                let Access::Path(PathTarget {
                    canonical,
                    relative,
                    zone,
                    ..
                }) = &prepared.access
                else {
                    panic!("tool {name}: expected a path target, got {prepared:?}");
                };
                assert_eq!(canonical, resolved.to_str().unwrap(), "tool {name}");
                assert_eq!(relative.as_deref(), Some("a.rs"), "tool {name}");
                assert_eq!(*zone, Zone::Workspace, "tool {name}");
                assert_eq!(prepared.input, input, "tool {name}");
            }
        }

        #[test]
        fn every_built_in_tool_parses_the_fields_its_schema_lists() {
            let ctx = temp_ctx();
            for (spec, t) in built_in().iter() {
                let ToolDefinition {
                    name, input_schema, ..
                } = &spec.definition;
                let listed: BTreeSet<&str> = input_schema["properties"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();

                let required: BTreeSet<&str> = input_schema["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|field| field.as_str().unwrap())
                    .collect();

                let unknown = t
                    .prepare(serde_json::json!({"unknown": true}), &ctx)
                    .unwrap_err();
                let mut input = serde_json::Map::new();
                while let Err(error) = t.prepare(Value::Object(input.clone()), &ctx)
                    && let Some(field) = error.strip_prefix("Invalid input: missing field `")
                {
                    let field = field.trim_end_matches('`');
                    let placeholder = if input_schema["properties"][field]["type"] == "array" {
                        Value::Array(Vec::new())
                    } else {
                        Value::from(field)
                    };
                    input.insert(field.to_owned(), placeholder);
                }

                let prefix = "Invalid input: unknown field `unknown`, expected ";
                assert!(unknown.starts_with(prefix), "tool {name}: {unknown}");
                let accepted: BTreeSet<&str> = unknown[prefix.len()..]
                    .split('`')
                    .skip(1)
                    .step_by(2)
                    .collect();
                assert_eq!(accepted, listed, "tool {name}: {unknown}");
                let reported: BTreeSet<&str> = input.keys().map(String::as_str).collect();
                assert_eq!(reported, required, "tool {name}");
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
                let prepare = |path: &str| {
                    read.prepare(serde_json::json!({"file_path": path}), &ctx)
                        .unwrap()
                };

                for (path, resolved) in [
                    ("~/notes.md", home.join("notes.md")),
                    ("~", home.clone()),
                    ("~user/x", cwd.join("~user").join("x")),
                ] {
                    assert_eq!(prepare(path).access.resolved(), Some(resolved), "{path}");
                }
                for path in ["~/.aws/credentials", "~/.ssh/id_ed25519"] {
                    assert_eq!(
                        policy.decide(spec.class, &prepare(path).access),
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

                let prepared = ReadTool
                    .prepare(serde_json::json!({"file_path": name}), &ctx)
                    .unwrap();
                let Access::Path(PathTarget { zone, .. }) = &prepared.access else {
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

            let prepared = GrepTool::new(ReadFilter::default())
                .prepare(input.clone(), &ctx(dir.path()))
                .unwrap();

            let Access::Path(PathTarget { canonical, .. }) = &prepared.access else {
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

            let prepared = WriteTool
                .prepare(
                    serde_json::json!({"file_path": "notes.md", "content": "x"}),
                    &ctx(&cwd),
                )
                .unwrap();

            let Access::Path(PathTarget {
                canonical,
                relative,
                zone,
                ..
            }) = &prepared.access
            else {
                panic!("expected a path target, got {prepared:?}");
            };
            assert_eq!(canonical, target.to_str().unwrap());
            assert_eq!(*relative, None);
            assert_eq!(*zone, Zone::Outside);
        }
    }

    mod approval_preview {
        use super::*;

        #[tokio::test]
        async fn default_titles_with_the_tool_name() {
            let tool = GlobTool::new(ReadFilter::default());
            let prepared = tool
                .prepare(serde_json::json!({"pattern": "*.rs"}), &temp_ctx())
                .unwrap();

            let preview = tool
                .approval_preview("glob", &prepared, Path::new("/repo"))
                .await;

            assert_eq!(preview.title, "Glob");
            assert_eq!(preview.body, ApprovalBody::Command("Glob(*.rs)".to_owned()));
        }

        #[tokio::test]
        async fn default_names_where_a_path_tool_call_lands() {
            let dir = canonical_tempdir();
            let target = dir.path().join("target.txt");
            fs::write(&target, "x").unwrap();
            symlink_file(&target, &dir.path().join("link.txt"));
            let prepared = ReadTool
                .prepare(
                    serde_json::json!({"file_path": "link.txt"}),
                    &ctx(dir.path()),
                )
                .unwrap();

            let preview = ReadTool
                .approval_preview("read", &prepared, dir.path())
                .await;

            assert_eq!(preview.title, "Read");
            assert_eq!(
                preview.body,
                ApprovalBody::Command("Read(target.txt)".to_owned())
            );
        }

        #[tokio::test]
        async fn default_names_the_resolved_path_from_a_plain_or_symlinked_cwd() {
            let dir = canonical_tempdir();
            let (real, outside) = (dir.path().join("real"), dir.path().join("outside"));
            fs::create_dir_all(real.join("src")).unwrap();
            fs::create_dir(&outside).unwrap();
            fs::write(real.join("src/main.rs"), "x").unwrap();
            fs::write(outside.join("notes.md"), "x").unwrap();
            symlink_dir(&outside, &real.join("src/out"));
            let link = dir.path().join("link");
            symlink_dir(&real, &link);
            let outside_notes = slash_separated(&outside.join("notes.md"));

            for (cwd_case, cwd) in [("plain", &real), ("symlinked", &link)] {
                for (target, expected) in [
                    ("src/main.rs", "src/main.rs"),
                    ("src/out/notes.md", outside_notes.as_str()),
                    (".", "real"),
                    ("src/main.rs/sub", "src/main.rs/sub"),
                ] {
                    let prepared = ReadTool
                        .prepare(serde_json::json!({"file_path": target}), &ctx(cwd))
                        .unwrap();

                    let preview = ReadTool.approval_preview("read", &prepared, cwd).await;

                    assert_eq!(
                        preview.body,
                        ApprovalBody::Command(format!("Read({expected})")),
                        "{target} from a {cwd_case} cwd"
                    );
                }
            }
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
                let tools = ToolRegistry::built_in(
                    &deny_reads(&["secret.txt"], dir.path()),
                    &Arc::default(),
                );
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
                ("root", "/", "/"),
            ] {
                assert_eq!(file_name(Path::new(path)), expected, "{case}");
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
}
