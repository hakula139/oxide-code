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
use std::time::SystemTime;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

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

/// Structured data for UI display and logging, not sent to the model.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ToolMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
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
    /// Converts `Ok` / `Err` into a `ToolOutput` with default metadata.
    pub(crate) fn from_result(result: Result<String, String>) -> Self {
        match result {
            Ok(content) => Self {
                content,
                is_error: false,
                metadata: ToolMetadata::default(),
            },
            Err(content) => Self {
                content,
                is_error: true,
                metadata: ToolMetadata::default(),
            },
        }
    }

    /// Attaches a display title.
    pub(crate) fn with_title(mut self, title: impl Into<String>) -> Self {
        self.metadata.title = Some(title.into());
        self
    }

    /// Records a replacement count (edit tool).
    pub(crate) fn with_replacements(mut self, count: usize) -> Self {
        self.metadata.replacements = Some(count);
        self
    }

    /// Attaches per-match diff hunks (edit tool).
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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DiffLine {
    pub(crate) number: usize,
    pub(crate) text: String,
}

/// A single boundary-trimmed edit hunk. Consumers must not re-trim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
pub(crate) trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> serde_json::Value;

    fn icon(&self) -> &'static str {
        "⟡"
    }

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

    /// Returns `None` to fall back to [`ToolResultView::Text`].
    fn result_view(
        &self,
        _input: &serde_json::Value,
        _content: &str,
        _metadata: &ToolMetadata,
    ) -> Option<ToolResultView> {
        None
    }

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

/// Fallback icon for unknown tool names.
pub(crate) const DEFAULT_TOOL_ICON: &str = "⟡";

/// Extracts a string field from a tool input object.
pub(crate) fn extract_input_field<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(serde_json::Value::as_str)
}

/// Capitalizes the first character of a tool name for display.
pub(crate) fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ── Tool Registry ──

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

    pub(crate) async fn run(&self, name: &str, input: serde_json::Value) -> ToolOutput {
        let Some(tool) = self.get(name) else {
            return ToolOutput {
                content: format!("Unknown tool: {name}"),
                is_error: true,
                metadata: ToolMetadata::default(),
            };
        };
        let mut output = tool.run(input).await;
        let (content, original_len) = cap_output(output.content);
        output.content = content;
        if let Some(len) = original_len {
            output.metadata.truncated_bytes = Some(len);
        }
        output
    }

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

/// Caps `content` at [`MAX_OUTPUT_BYTES`], keeping head and tail halves.
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

/// Deserializes raw JSON into a tool's input struct.
#[expect(
    clippy::result_large_err,
    reason = "ToolOutput carries the full tool result; the Err here is the cold input-validation path constructed at most once per tool call"
)]
pub(crate) fn parse_input<T: DeserializeOwned>(raw: serde_json::Value) -> Result<T, ToolOutput> {
    serde_json::from_value(raw).map_err(|e| ToolOutput {
        content: format!("Invalid input: {e}"),
        is_error: true,
        metadata: ToolMetadata::default(),
    })
}

// ── Path Utilities ──

/// Resolves `search_path` or falls back to the current working directory.
pub(crate) fn resolve_base_dir(search_path: Option<&str>) -> Result<PathBuf, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("Failed to get working directory: {e}"))?;
    Ok(search_path.map_or(cwd, PathBuf::from))
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

/// Returns a tool-call label with a cwd-relative path argument.
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

/// Returns a cwd-relative path, or absolute if outside `base`.
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

/// Extracts the filename component, falling back to the full path.
pub(crate) fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

// ── Binary Detection ──

const BINARY_CHECK_SIZE: usize = 8192;

/// Detects binary files by scanning for null bytes in the first 8 KB.
pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_CHECK_SIZE).any(|&b| b == 0)
}

// ── File Walking ──

const MAX_WALK_DEPTH: usize = 64;

/// Returns a gitignore-aware iterator over regular files under `base`.
pub(crate) fn walk_files(base: &Path) -> impl Iterator<Item = ignore::DirEntry> {
    ignore::WalkBuilder::new(base)
        .same_file_system(true)
        .max_depth(Some(MAX_WALK_DEPTH))
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
}

/// Extracts the modification time, falling back to `UNIX_EPOCH`.
pub(crate) fn entry_mtime(entry: &ignore::DirEntry) -> SystemTime {
    entry
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

// ── Formatting ──

/// Converts a byte count to megabytes for display.
pub(crate) fn bytes_to_mb(bytes: u64) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "MB display tolerates minor precision loss at > 2^53 bytes; file size caps are nowhere near that"
    )]
    let mb = bytes as f64 / (1024.0 * 1024.0);
    mb
}

pub(crate) const MAX_LINE_LENGTH: usize = 500;

/// Truncates a line beyond [`MAX_LINE_LENGTH`] characters.
pub(crate) fn truncate_line(line: &str) -> Cow<'_, str> {
    if line.len() <= MAX_LINE_LENGTH {
        return Cow::Borrowed(line);
    }
    let total_chars = line.chars().count();
    if total_chars <= MAX_LINE_LENGTH {
        return Cow::Borrowed(line);
    }
    let boundary = line
        .char_indices()
        .nth(MAX_LINE_LENGTH)
        .map_or(line.len(), |(i, _)| i);
    Cow::Owned(format!(
        "{}{ELLIPSIS} [{total_chars} chars]",
        &line[..boundary]
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::bash::BashTool;
    use super::edit::EditTool;
    use super::glob::GlobTool;
    use super::grep::GrepTool;
    use super::read::ReadTool;
    use super::write::WriteTool;
    use super::*;
    use crate::file_tracker::testing::tracker;

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

    // ── ToolOutput::from_result ──

    #[test]
    fn from_result_ok_clears_is_error() {
        let out = ToolOutput::from_result(Ok("success".into()));
        assert!(!out.is_error);
        assert_eq!(out.content, "success");
    }

    #[test]
    fn from_result_err_sets_is_error() {
        let out = ToolOutput::from_result(Err("something went wrong".into()));
        assert!(out.is_error);
        assert_eq!(out.content, "something went wrong");
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
    fn tool_summarize_input_returns_none_when_primary_field_missing() {
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
    fn display_cwd_path_from_without_cwd_falls_back_to_original_path() {
        assert_eq!(
            display_cwd_path_from("/tmp/example.rs", None),
            "/tmp/example.rs"
        );
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
    fn get_returns_registered_tool() {
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
    fn definitions_returns_tool_with_valid_schema() {
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

    fn write_oversize_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let line = "x".repeat(600);
        let content = format!("{line}\n").repeat(500);
        std::fs::write(&path, &content).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn run_dispatches_and_caps_byte_overflow() {
        let (_dir, path) = write_oversize_file();
        let registry = ToolRegistry::new(vec![Box::new(ReadTool::new(tracker()))]);
        let output = registry
            .run(
                "read",
                serde_json::json!({"file_path": path.to_str().unwrap()}),
            )
            .await;

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
    async fn run_byte_cap_on_read_falls_through_to_text_view() {
        let (_dir, path) = write_oversize_file();
        let registry = ToolRegistry::new(vec![Box::new(ReadTool::new(tracker()))]);
        let input = serde_json::json!({"file_path": path.to_str().unwrap()});
        let output = registry.run("read", input.clone()).await;

        let view = registry.result_view(
            "read",
            &input,
            &output.content,
            &output.metadata,
            output.is_error,
        );
        assert!(matches!(view, ToolResultView::Text { .. }));
    }

    #[tokio::test]
    async fn run_unknown_tool_returns_error_payload() {
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
    fn resolve_base_dir_some_returns_given_path() {
        let result = resolve_base_dir(Some("/tmp/foo")).unwrap();
        assert_eq!(result, PathBuf::from("/tmp/foo"));
    }

    #[test]
    fn resolve_base_dir_none_returns_cwd() {
        let result = resolve_base_dir(None).unwrap();
        assert_eq!(result, std::env::current_dir().unwrap());
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
    fn display_path_same_path_returns_filename() {
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

    // ── truncate_line ──

    #[test]
    fn truncate_line_short_unchanged() {
        assert_eq!(truncate_line("hello").as_ref(), "hello");
    }

    #[test]
    fn truncate_line_long_gets_truncated() {
        let long_line = "x".repeat(MAX_LINE_LENGTH + 100);
        let result = truncate_line(&long_line);
        let suffix = format!("... [{} chars]", MAX_LINE_LENGTH + 100);
        assert_eq!(result.len(), MAX_LINE_LENGTH + suffix.len());
        assert_eq!(&result[..MAX_LINE_LENGTH], "x".repeat(MAX_LINE_LENGTH));
        assert_eq!(&result[MAX_LINE_LENGTH..], suffix);
    }

    #[test]
    fn truncate_line_multibyte_safe() {
        let mut line = "a".repeat(MAX_LINE_LENGTH - 2);
        line.push('🦀');
        line.push_str(&"b".repeat(100));
        let result = truncate_line(&line);

        let (prefix, suffix) = result.split_once("... [").unwrap();
        let expected_prefix = "a".repeat(MAX_LINE_LENGTH - 2) + "🦀" + "b";
        assert_eq!(prefix, expected_prefix);
        assert_eq!(prefix.chars().count(), MAX_LINE_LENGTH);
        assert_eq!(suffix, format!("{} chars]", MAX_LINE_LENGTH + 99));
    }

    #[test]
    fn truncate_line_multibyte_under_char_cap_unchanged() {
        let line = "é".repeat(MAX_LINE_LENGTH);
        let result = truncate_line(&line);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result.as_ref(), line);
    }
}
