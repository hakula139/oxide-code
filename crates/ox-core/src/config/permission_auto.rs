//! User-configured model and instructions for automatic tool approval.

use crate::model::ModelSpec;

#[derive(Debug, Clone)]
pub struct AutoPermissionConfig {
    pub model: ModelSpec,
    pub instructions: Vec<String>,
}

impl Default for AutoPermissionConfig {
    fn default() -> Self {
        Self {
            model: ModelSpec::resolve("claude-haiku-5-5"),
            instructions: Vec::new(),
        }
    }
}
