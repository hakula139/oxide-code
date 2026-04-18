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

use serde::Serialize;
use serde::de::DeserializeOwned;

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
#[derive(Default)]
pub(crate) struct ToolMetadata {
    /// Short label for TUI display (5–15 words).
    pub(crate) title: Option<String>,
    /// Process exit code, present only for the bash tool.
    #[expect(
        dead_code,
        reason = "recorded by the bash tool but unused by the current TUI tool-result renderer"
    )]
    pub(crate) exit_code: Option<i32>,
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

    /// Sets the [`ToolMetadata::title`] field.
    pub(crate) fn with_title(mut self, title: impl Into<String>) -> Self {
        self.metadata.title = Some(title.into());
        self
    }
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

/// Depth cap for file-tree walks.
///
/// The `ignore` crate bounds symlink cycles within the same filesystem, but
/// a non-ignored directory tree of unbounded depth (generated code, mount
/// points, artefact dumps) can still monopolize `spawn_blocking` threads.
/// 64 covers anything a real source tree contains.
const MAX_WALK_DEPTH: usize = 64;

/// Returns a gitignore-aware iterator over regular files under `base`.
///
/// Respects `.gitignore`, `.ignore`, `.git/info/exclude`, and global ignore
/// rules. Stays within the same filesystem to avoid crossing mount points,
/// and caps recursion depth at [`MAX_WALK_DEPTH`]. Permission errors and
/// symlink loops are silently skipped.
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

/// Cap on tool output size. Prevents flooding the LLM context window.
/// Roughly 32K tokens at ~4 chars / token.
pub(crate) const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// Per-line character cap for read and grep output. Matches the
/// `--max-columns` default that ripgrep uses in Claude Code.
pub(crate) const MAX_LINE_LENGTH: usize = 500;

/// Truncates a line beyond [`MAX_LINE_LENGTH`] characters, appending a
/// `[N chars]` suffix. Returns a borrowed slice when no truncation is needed.
pub(crate) fn truncate_line(line: &str) -> Cow<'_, str> {
    if line.len() <= MAX_LINE_LENGTH {
        return Cow::Borrowed(line);
    }

    // Single pass: find both the truncation boundary and the total char count.
    let mut boundary = 0;
    let mut total_chars = 0;
    for (i, (byte_idx, _)) in line.char_indices().enumerate() {
        if i == MAX_LINE_LENGTH {
            boundary = byte_idx;
        }
        total_chars = i + 1;
    }

    Cow::Owned(format!("{}... [{total_chars} chars]", &line[..boundary]))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::bash::BashTool;
    use super::*;

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

    // ── ToolRegistry::get ──

    #[test]
    fn get_returns_registered_tool() {
        let registry = ToolRegistry::new(vec![Box::new(BashTool)]);
        assert_eq!(registry.get("bash").unwrap().name(), "bash");
    }

    #[test]
    fn get_returns_none_for_unknown() {
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
}
