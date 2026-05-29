use crate::error::ToolCallError;
use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[async_trait]
pub trait Toolbox: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError>;
}

/// A single named tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError>;
}

/// Generic Toolbox impl — register individual Tool implementations into it.
pub struct ToolboxImpl {
    tools: Vec<Box<dyn Tool>>,
}

impl Default for ToolboxImpl {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolboxImpl {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn add(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Box::new(tool));
        self
    }
}

#[async_trait]
impl Toolbox for ToolboxImpl {
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        match self.tools.iter().find(|t| t.spec().name == name) {
            Some(tool) => tool.execute(input).await,
            None => Err(ToolCallError::InvalidInput(format!(
                "no tool named '{name}'"
            ))),
        }
    }
}

pub struct EmptyToolbox;

#[async_trait]
impl Toolbox for EmptyToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![]
    }

    async fn execute(&self, name: &str, _input: Value) -> Result<Value, ToolCallError> {
        Err(ToolCallError::InvalidInput(format!(
            "no tool named '{name}'"
        )))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".to_string(),
                description: "echoes input".to_string(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
            Ok(input)
        }
    }

    #[tokio::test]
    async fn toolbox_impl_routes_by_name() {
        let tb = ToolboxImpl::new().add(EchoTool);
        let result = tb.execute("echo", json!({"x": 1})).await.unwrap();
        assert_eq!(result, json!({"x": 1}));
    }

    #[tokio::test]
    async fn toolbox_impl_unknown_tool_returns_error() {
        let tb = ToolboxImpl::new();
        let err = tb.execute("nope", json!({})).await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn toolbox_impl_specs_returns_all() {
        let tb = ToolboxImpl::new().add(EchoTool);
        let specs = tb.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
    }
}
