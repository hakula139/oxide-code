pub mod bash;

use std::future::Future;
use std::pin::Pin;

use serde::Serialize;

// ── Tool Definition ──

/// Schema sent to the Anthropic API to describe an available tool.
#[derive(Clone, Serialize)]
pub(crate) struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
}

// ── Tool Output ──

pub(crate) struct ToolOutput {
    pub content: String,
    pub is_error: bool,
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
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self { tools }
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(AsRef::as_ref)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }
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
}
