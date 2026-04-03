pub(crate) mod bash;
pub(crate) mod edit;
pub(crate) mod glob;
pub(crate) mod grep;
pub(crate) mod read;
pub(crate) mod write;

use std::borrow::Cow;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

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

pub(crate) struct ToolOutput {
    pub(crate) content: String,
    pub(crate) is_error: bool,
}

impl ToolOutput {
    pub(crate) fn from_result(result: Result<String, String>) -> Self {
        match result {
            Ok(content) => Self {
                content,
                is_error: false,
            },
            Err(content) => Self {
                content,
                is_error: true,
            },
        }
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

pub(crate) fn parse_input<T: DeserializeOwned>(raw: serde_json::Value) -> Result<T, ToolOutput> {
    serde_json::from_value(raw).map_err(|e| ToolOutput {
        content: format!("Invalid input: {e}"),
        is_error: true,
    })
}

// ── Path Resolution ──

pub(crate) fn resolve_base_dir(path: Option<&str>) -> Result<PathBuf, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("Failed to get working directory: {e}"))?;
    Ok(path.map_or(cwd, PathBuf::from))
}

// ── Binary Detection ──

const BINARY_CHECK_SIZE: usize = 8192;

pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_CHECK_SIZE).any(|&b| b == 0)
}

// ── Formatting ──

pub(crate) const MAX_LINE_LENGTH: usize = 500;

/// Returns a borrowed slice when no truncation is needed.
pub(crate) fn truncate_line(line: &str) -> Cow<'_, str> {
    if line.len() <= MAX_LINE_LENGTH {
        return Cow::Borrowed(line);
    }
    let boundary = line.floor_char_boundary(MAX_LINE_LENGTH);
    Cow::Owned(format!(
        "{}... [{} chars]",
        &line[..boundary],
        line.chars().count(),
    ))
}

#[cfg(test)]
mod tests {
    use super::bash::BashTool;
    use super::*;

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

    // ── truncate_line ──

    #[test]
    fn truncate_line_short_unchanged() {
        assert_eq!(truncate_line("hello").as_ref(), "hello");
    }

    #[test]
    fn truncate_line_long_gets_truncated() {
        let long_line = "x".repeat(MAX_LINE_LENGTH + 100);
        let result = truncate_line(&long_line);
        assert!(result.starts_with(&"x".repeat(MAX_LINE_LENGTH)));
        assert!(result.ends_with(&format!("[{} chars]", MAX_LINE_LENGTH + 100)));
        assert!(result.len() < long_line.len());
    }

    #[test]
    fn truncate_line_multibyte_safe() {
        let mut line = "a".repeat(MAX_LINE_LENGTH - 2);
        line.push('🦀');
        line.push_str(&"b".repeat(100));
        let result = truncate_line(&line);
        assert!(result.contains("chars]"));
    }
}
