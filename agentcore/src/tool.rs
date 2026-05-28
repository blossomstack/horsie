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
