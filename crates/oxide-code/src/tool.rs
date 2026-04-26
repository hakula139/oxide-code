//! Tool dispatch.
//!
//! The [`Tool`] trait defines what the agent can invoke; the concrete
//! tools live in submodules (`bash`, `edit`, `glob`, `grep`, `read`,
//! `write`). [`ToolRegistry`] holds the set exposed to the model
//! along with its JSON schema; [`ToolOutput`] carries the wire result
//! back to the model plus structured [`ToolMetadata`] for UI display.

pub(crate) mod bash;
pub(crate) mod edit;
pub(crate) mod glob;
pub(crate) mod grep;
pub(crate) mod read;
pub(crate) mod write;

use std::borrow::Cow;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::SystemTime;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

// ── Tool Definition ──

/// Schema sent to the Anthropic API to describe an available tool.
#[derive(Clone, Serialize)]
pub(crate) struct ToolDefinition {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) input_schema: serde_json::Value,
}

// ── Tool Output ──

/// Result returned from a tool execution to the agent loop.
///
/// `content` is the text sent back to the model as the `tool_result`.
/// `metadata` carries structured data for UI display and logging — it is
/// never sent to the model.
///
/// `is_error` signals an infrastructure failure (timeout, spawn error, invalid
/// input) — not a semantic failure in the command itself. Nonzero exit codes,
/// missing files, and regex mismatches are reported in `content` with
/// `is_error: false` so the model can interpret severity from context.
pub(crate) struct ToolOutput {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) metadata: ToolMetadata,
}

/// Structured data for UI display and logging, not sent to the model.
///
/// Every tool should set `title` to a concise, human-readable summary
/// (e.g., "Read Cargo.toml", "Created src/main.rs", "3 matches in 2 files").
/// The TUI renders this as the one-line label for each tool invocation.
///
/// Serializable because the session writer persists this alongside
/// each tool result (via [`Entry::ToolResultMetadata`](crate::session::entry::Entry))
/// so resumed sessions see the same rendered shape as live — without
/// polluting the API-facing [`ContentBlock::ToolResult`](crate::message::ContentBlock)
/// wire format with TUI-only fields.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ToolMetadata {
    /// Short label for TUI display (5–15 words).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    /// Process exit code, present only for the bash tool. Read by
    /// the `PartialEq` derive (used to gate persistence of empty
    /// metadata) but not yet surfaced in the rendered view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    /// Number of replacements actually made, present only for the
    /// edit tool when `replace_all` matched multiple occurrences.
    /// Consumed by [`ToolResultView::Diff`] so the "N occurrences
    /// replaced" footer can be driven structurally instead of
    /// parsed from prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) replacements: Option<usize>,
    /// Per-match diff hunks with real file line numbers, present only
    /// for the edit tool. Consumed by [`ToolResultView::Diff`] so the
    /// renderer can show line-numbered, location-aware diffs instead
    /// of a placeholder pair of strings. Resumed sessions whose JSONL
    /// predates this field fall back to a synthesized single chunk
    /// inside [`crate::tool::edit::EditTool::result_view`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) diff_chunks: Option<Vec<DiffChunk>>,
    /// Unbounded match count when a tool capped its returned rows
    /// (currently glob's `MAX_RESULTS`; reserved for a future grep
    /// total). Lets the result-view renderer surface "X of N total"
    /// without re-parsing the tool's prose footer. `None` when no
    /// truncation occurred. Resumed sessions whose JSONL predates the
    /// field fall back to `files.len()` as the total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) truncated_total: Option<usize>,
}

impl ToolOutput {
    /// Converts a `Result<String, String>` into a `ToolOutput` with default
    /// metadata. `Err` maps to `is_error: true`.
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

    /// Fluent helper to attach a display title; chains after
    /// [`from_result`](Self::from_result) at the construction site.
    pub(crate) fn with_title(mut self, title: impl Into<String>) -> Self {
        self.metadata.title = Some(title.into());
        self
    }

    /// Fluent helper to record a replacement count; only meaningful
    /// for the edit tool with `replace_all` hitting multiple matches.
    pub(crate) fn with_replacements(mut self, count: usize) -> Self {
        self.metadata.replacements = Some(count);
        self
    }

    /// Fluent helper to attach the per-match diff hunks; only
    /// meaningful for the edit tool's success path. The chunks carry
    /// the real file line numbers so the diff renderer can show where
    /// each match landed without re-reading the (possibly already
    /// modified) file at render time.
    pub(crate) fn with_diff_chunks(mut self, chunks: Vec<DiffChunk>) -> Self {
        self.metadata.diff_chunks = Some(chunks);
        self
    }

    /// Fluent helper to record the unbounded match count when a tool
    /// truncated its returned rows. Read by the renderer to surface
    /// "X of N total" without re-parsing the prose footer that the
    /// tool also bakes into [`Self::content`] for the model.
    pub(crate) fn with_truncated_total(mut self, total: usize) -> Self {
        self.metadata.truncated_total = Some(total);
        self
    }
}

// ── Tool Result View ──

/// Per-tool shape of a completed tool call's body, produced by
/// [`Tool::result_view`] and rendered by the TUI's tool-result block.
///
/// This enum lives here — not in the TUI layer — so per-tool parsing
/// (Edit's diff extraction, Read's line-numbered excerpts, Grep's
/// per-file matches) stays in the module that owns each tool's
/// input/output contract. The TUI still owns rendering; this is pure data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolResultView {
    /// Default shape — the raw tool output, shown as a truncated
    /// monospace block with a `+N lines` footer when it overflows.
    Text { content: String },
    /// Read tool — renders as a line-numbered excerpt with path/range
    /// context while leaving the model-facing output unchanged.
    ReadExcerpt {
        path: String,
        lines: Vec<ReadExcerptLine>,
        total_lines: usize,
    },
    /// Edit tool — `-` old / `+` new unified diff with real file
    /// line numbers. Live invariant: chunks share trimmed content
    /// (one chunk for a single edit, one per match for `replace_all`).
    /// `replacements` matches `chunks.len()` on the live path; resumed
    /// sessions predating structured chunks may carry a count larger
    /// than 1 against a single synthesized chunk.
    Diff {
        chunks: Vec<DiffChunk>,
        replace_all: bool,
        replacements: usize,
    },
    /// Grep content-mode result. `truncated` mirrors grep's "Results
    /// limited to N lines" footer. Other modes and outputs with skipped-
    /// file warnings fall through to [`Text`].
    GrepMatches {
        groups: Vec<GrepFileGroup>,
        truncated: bool,
    },
    /// Glob result — flat list of cwd-relative paths. `pattern` is the
    /// input glob echoed back so the body can stay self-describing
    /// after the status header scrolls away. `total` preserves the
    /// unbounded match count from glob's `MAX_RESULTS` cap so the
    /// renderer can show "X more matched" when the tool itself
    /// truncated; equals `files.len()` otherwise.
    GlobFiles {
        pattern: String,
        files: Vec<String>,
        total: usize,
    },
}

/// One line in a structured `read` result view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadExcerptLine {
    pub(crate) number: usize,
    pub(crate) text: String,
}

/// One numbered line on either side of a structured `edit` diff.
/// Persisted in [`ToolMetadata::diff_chunks`] so resumed sessions
/// keep real file line numbers — not a render-only view type.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DiffLine {
    pub(crate) number: usize,
    pub(crate) text: String,
}

/// A single matched edit hunk — `-` and `+` line spans for one file
/// location. Producer emits one chunk per match site (always one for
/// non-`replace_all`).
///
/// Chunks are emitted boundary-trimmed by
/// `crate::tool::edit::trim_chunk`: surviving entries are the
/// user-visible delta. Consumers must not re-trim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DiffChunk {
    pub(crate) old: Vec<DiffLine>,
    pub(crate) new: Vec<DiffLine>,
}

/// One file's match block in a grep result view. Lines mix matches
/// (`:` in grep output) and surrounding context (`-`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrepFileGroup {
    pub(crate) path: String,
    pub(crate) lines: Vec<GrepMatchLine>,
}

/// One row in a [`GrepFileGroup`]. `is_match: false` flags context
/// lines so renderers can dim them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrepMatchLine {
    pub(crate) number: usize,
    pub(crate) text: String,
    pub(crate) is_match: bool,
}

// ── Tool Trait ──

/// A tool that the agent can invoke.
///
/// Uses `Pin<Box<dyn Future>>` for the async `run` method instead of `async fn`
/// so the trait remains object-safe (`Box<dyn Tool>`).
pub(crate) trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> serde_json::Value;

    /// Single-character display icon for TUI / stdio rendering.
    fn icon(&self) -> &'static str {
        "⟡"
    }

    /// Returns the most relevant input field as a one-line label
    /// (e.g., the command for bash, the `file_path` for read / write / edit).
    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        _ = input;
        None
    }

    /// Returns the human-readable tool-call label shown in the TUI,
    /// e.g. `Grep(fn foo)`, `Read(Cargo.toml)`. The default capitalizes
    /// [`Tool::name`] and wraps [`summarize_input`](Self::summarize_input)
    /// in parentheses; tools whose icon + argument already read as a
    /// complete line (bash's `$ <command>`) override to return the bare
    /// argument instead.
    fn summarize_call(&self, input: &serde_json::Value) -> String {
        let label = title_case(self.name());
        match self.summarize_input(input) {
            Some(arg) => format!("{label}({arg})"),
            None => label,
        }
    }

    /// Optional structured view of a completed tool call's output,
    /// used by the TUI in place of the default truncated text block.
    /// `input` is the original tool-call arguments (already used by
    /// `run`); `content` is the success-path `ToolOutput::content`;
    /// `metadata` is the same `ToolMetadata` the tool attached in
    /// `run` (title, exit code, replacements, ...) so tools can drive
    /// the view structurally instead of re-parsing `content`.
    ///
    /// Returning `None` — the default — falls back to [`ToolResultView::Text`].
    /// Tools should also return `None` when the input shape doesn't
    /// match their expectations, so malformed calls degrade gracefully
    /// rather than panic.
    ///
    /// Not called for error outputs: [`ToolRegistry::result_view`]
    /// short-circuits `is_error` to `Text` centrally since every
    /// tool's error message is free-form prose, not a structured
    /// shape.
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

/// Default icon used when a tool name is unknown to the registry.
pub(crate) const DEFAULT_TOOL_ICON: &str = "⟡";

/// Extracts a string field from a tool input object. Helper for per-tool
/// [`Tool::summarize_input`] implementations that simply pluck one key.
pub(crate) fn extract_input_field<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(serde_json::Value::as_str)
}

/// Capitalizes the first character of an ASCII tool name for display
/// (`"grep"` → `"Grep"`). Returns an empty string for empty input.
/// Used by the default [`Tool::summarize_call`] implementation and by
/// overrides that still want the default fallback shape when input
/// fields are missing (see [`BashTool::summarize_call`]).
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

    /// Looks up the display icon for `name`, falling back to
    /// [`DEFAULT_TOOL_ICON`] when the tool is not registered.
    pub(crate) fn icon(&self, name: &str) -> &'static str {
        self.get(name).map_or(DEFAULT_TOOL_ICON, Tool::icon)
    }

    /// Returns the display label for a tool call. Resolves `name` to
    /// a registered [`Tool`] and delegates to [`Tool::summarize_call`];
    /// falls back to the raw `name` for tools not in the registry so
    /// callers always get a non-empty label to render.
    pub(crate) fn label(&self, name: &str, input: &serde_json::Value) -> String {
        self.get(name)
            .map_or_else(|| name.to_owned(), |t| t.summarize_call(input))
    }

    /// Builds the structured [`ToolResultView`] for a completed tool
    /// call. Falls back to [`ToolResultView::Text`] in every case a
    /// per-tool renderer cannot cleanly represent:
    ///
    /// - `is_error`: error outputs carry the failure message as free
    ///   text; pretending they're a diff swaps signal for a lie.
    /// - Unregistered tool name: no renderer to consult.
    /// - [`Tool::result_view`] returns `None`: the tool has no
    ///   structured view yet, or the input shape wasn't what the
    ///   renderer expected.
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

// ── Input Parsing ──

/// Deserializes raw JSON into a tool's input struct, returning a
/// [`ToolOutput`] error that can be sent directly back to the model.
pub(crate) fn parse_input<T: DeserializeOwned>(raw: serde_json::Value) -> Result<T, ToolOutput> {
    serde_json::from_value(raw).map_err(|e| ToolOutput {
        content: format!("Invalid input: {e}"),
        is_error: true,
        metadata: ToolMetadata::default(),
    })
}

// ── Path Utilities ──

/// Returns `search_path` as a [`PathBuf`] if provided, otherwise the current
/// working directory.
pub(crate) fn resolve_base_dir(search_path: Option<&str>) -> Result<PathBuf, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("Failed to get working directory: {e}"))?;
    Ok(search_path.map_or(cwd, PathBuf::from))
}

/// Returns a relative path string when `path` is inside the current working
/// directory, otherwise the absolute path. Falls back to `path` when the cwd
/// cannot be read.
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

/// Returns a tool-call label using a cwd-relative path argument when possible.
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

/// Returns a relative path string when `path` is inside `base`, otherwise the
/// absolute path. When stripping the prefix yields an empty path (i.e.,
/// `path == base`), falls back to the filename. Saves tokens in tool output
/// and matches how developers think about file locations.
pub(crate) fn display_path(path: &Path, base: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(base) {
        if rel.as_os_str().is_empty() {
            // path == base (single-file search): show the filename
            return path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |n| n.to_string_lossy().into_owned(),
            );
        }
        return rel.to_string_lossy().into_owned();
    }
    path.to_string_lossy().into_owned()
}

/// Extracts the filename component from a path string, falling back to the
/// full path when no filename is present. Used by file tools to generate
/// concise TUI titles.
pub(crate) fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

// ── Binary Detection ──

const BINARY_CHECK_SIZE: usize = 8192;

/// Detects binary files by scanning for null bytes in the first 8 KB.
/// False negatives are possible for binary formats that avoid nulls (e.g.,
/// base64-encoded data), but this catches ELF, Mach-O, images, and most
/// compiled output cheaply.
pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_CHECK_SIZE).any(|&b| b == 0)
}

// ── File Walking ──

/// Depth cap for file-tree walks — guards against pathological trees
/// (generated code, deep artefact dumps) that `ignore` doesn't filter.
const MAX_WALK_DEPTH: usize = 64;

/// Returns a gitignore-aware iterator over regular files under `base`.
///
/// Respects `.gitignore`, `.ignore`, `.git/info/exclude`, and global ignore
/// rules. Stays within the same filesystem, caps depth at [`MAX_WALK_DEPTH`],
/// silently skips permission errors and symlink loops.
pub(crate) fn walk_files(base: &Path) -> impl Iterator<Item = ignore::DirEntry> {
    ignore::WalkBuilder::new(base)
        .same_file_system(true)
        .max_depth(Some(MAX_WALK_DEPTH))
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
}

/// Extracts the modification time from a walker entry, falling back to
/// `UNIX_EPOCH` when metadata is unavailable.
pub(crate) fn entry_mtime(entry: &ignore::DirEntry) -> SystemTime {
    entry
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

// ── Formatting ──

/// Converts a byte count to megabytes for display. Centralizes the
/// `clippy::cast_precision_loss` suppression — every file-size cap in
/// this crate is well below 2^53 bytes, so the f64 cast is exact.
pub(crate) fn bytes_to_mb(bytes: u64) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "MB display tolerates minor precision loss at > 2^53 bytes; file size caps are nowhere near that"
    )]
    let mb = bytes as f64 / (1024.0 * 1024.0);
    mb
}

/// Cap on tool output size. Prevents flooding the LLM context window.
/// Roughly 32K tokens at ~4 chars / token.
pub(crate) const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// Per-line character cap for read and grep output. Long lines (minified
/// bundles, base64 blobs) rarely help the model and crowd out useful
/// context; 500 chars captures ~80 code columns with margin for structured
/// output like `rg --vimgrep` locations.
pub(crate) const MAX_LINE_LENGTH: usize = 500;

/// Truncates a line beyond [`MAX_LINE_LENGTH`] characters, appending a
/// `[N chars]` suffix. Returns a borrowed slice when no truncation is needed.
pub(crate) fn truncate_line(line: &str) -> Cow<'_, str> {
    // Fast path: fewer bytes than the char cap means we can't exceed the cap.
    if line.len() <= MAX_LINE_LENGTH {
        return Cow::Borrowed(line);
    }
    let total_chars = line.chars().count();
    if total_chars <= MAX_LINE_LENGTH {
        return Cow::Borrowed(line);
    }
    // `nth(MAX_LINE_LENGTH)` is Some: we just proved `total_chars > MAX_LINE_LENGTH`.
    let boundary = line
        .char_indices()
        .nth(MAX_LINE_LENGTH)
        .map_or(line.len(), |(i, _)| i);
    Cow::Owned(format!("{}... [{total_chars} chars]", &line[..boundary]))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use super::bash::BashTool;
    use super::edit::EditTool;
    use super::glob::GlobTool;
    use super::grep::GrepTool;
    use super::read::ReadTool;
    use super::write::WriteTool;
    use super::*;

    /// Every registered tool, to parameterize trait-contract tests.
    fn all_tools() -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(BashTool),
            Box::new(EditTool),
            Box::new(GlobTool),
            Box::new(GrepTool),
            Box::new(ReadTool),
            Box::new(WriteTool),
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
        // Duplicate names collide in registry lookup; duplicate icons
        // make the TUI tool-call rows indistinguishable.
        let tools = all_tools();
        let names: HashSet<_> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names.len(), tools.len(), "duplicate name");
        let icons: HashSet<_> = tools.iter().map(|t| t.icon()).collect();
        assert_eq!(icons.len(), tools.len(), "duplicate icon");
    }

    #[test]
    fn tool_catalog_icons_match_the_published_prefix_set() {
        // Pins the published icons — see docs / roadmap TUI section.
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
        // Table of (tool, input JSON, expected summary). Each entry pins
        // which field the TUI's tool-call label sources from.
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
        // Default format: `Grep(fn foo)`, `Read(/a/b.rs)` — clearly
        // identifies which tool is running even without the icon.
        // Bash overrides to the bare command (the `$` icon carries the
        // "shell prompt" semantics; wrapping in `Bash(...)` is redundant).
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
        // Missing primary field → every tool (bash included) falls
        // back to the title-cased tool name. Without this, bash would
        // render as a bare `$ ` in the TUI; `$ Bash` keeps the status
        // line readable even on malformed input.
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
        // Already-capitalized / empty / single-char inputs pass through
        // without panicking or mangling subsequent characters.
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
        // Unknown tool → the raw API name keeps the UI showing
        // *something*. Pinned so a future "defer to first registered
        // tool" bug would flip this to a mismatched label.
        let registry = ToolRegistry::new(vec![Box::new(BashTool)]);
        let input = serde_json::json!({"command": "echo hi"});
        assert_eq!(registry.label("nonexistent", &input), "nonexistent");
    }

    // ── ToolRegistry::result_view ──

    #[test]
    fn result_view_delegates_to_tool_for_structured_output() {
        // Edit was the first registered override; routing through
        // the registry must produce the same `Diff` the tool owns —
        // including the field values, so a mutation returning an
        // empty diff wouldn't pass.
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
        let registry = ToolRegistry::new(vec![Box::new(ReadTool)]);
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
        // Error outputs are prose ("old_string not found ..."); rendering
        // them as a diff would hide the failure. The short-circuit lives
        // in the registry so individual tools don't each re-implement it.
        let registry = ToolRegistry::new(vec![Box::new(EditTool)]);
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
        // Bash has no `result_view` override yet — free-form shell
        // output renders as the default truncated text block. Pin
        // the full content so a mutation returning an empty Text
        // wouldn't pass.
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
        // Pin the exact shape: MAX_LINE_LENGTH x's, then the suffix — no
        // off-by-one slack that would hide a truncation-boundary regression.
        assert_eq!(result.len(), MAX_LINE_LENGTH + suffix.len());
        assert_eq!(&result[..MAX_LINE_LENGTH], "x".repeat(MAX_LINE_LENGTH));
        assert_eq!(&result[MAX_LINE_LENGTH..], suffix);
    }

    #[test]
    fn truncate_line_multibyte_safe() {
        // The 🦀 sits at char `MAX_LINE_LENGTH - 2`, straddling the byte
        // boundary a naive cut at `MAX_LINE_LENGTH` bytes would land in.
        let mut line = "a".repeat(MAX_LINE_LENGTH - 2);
        line.push('🦀');
        line.push_str(&"b".repeat(100));
        let result = truncate_line(&line);

        // Prefix is exactly the first MAX_LINE_LENGTH chars of the body:
        // (MAX_LINE_LENGTH - 2) a's, then 🦀, then a single b — no partial
        // emoji bytes, no over-read. Suffix reports the original char count.
        let (prefix, suffix) = result.split_once("... [").unwrap();
        let expected_prefix = "a".repeat(MAX_LINE_LENGTH - 2) + "🦀" + "b";
        assert_eq!(prefix, expected_prefix);
        assert_eq!(prefix.chars().count(), MAX_LINE_LENGTH);
        assert_eq!(suffix, format!("{} chars]", MAX_LINE_LENGTH + 99));
    }

    #[test]
    fn truncate_line_multibyte_under_char_cap_unchanged() {
        // Regression: MAX_LINE_LENGTH é's take 2*MAX_LINE_LENGTH bytes,
        // tripping a byte-length gate but staying within the char cap.
        // A bad implementation returns `"... [N chars]"` with an empty
        // prefix (silent data loss); the correct behavior is a no-op
        // borrow.
        let line = "é".repeat(MAX_LINE_LENGTH);
        let result = truncate_line(&line);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result.as_ref(), line);
    }
}
