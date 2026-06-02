use crate::client::{RuntimeCallError, RuntimeClient};
use agentcore::{Tool, ToolCallError, ToolSpec};
use async_trait::async_trait;
use models::runtime::{FindAndReplaceInput, ToolCall};
use serde_json::{Value, json};

pub struct FindAndReplaceTool {
    client: RuntimeClient,
}
impl FindAndReplaceTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for FindAndReplaceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "find_and_replace".to_string(),
            description: "Find and replace text in a file. 'find' is a literal string by \
                default; set 'regex' true to treat it as a pattern (with $1-style capture \
                groups in 'replace'). The match must be unique unless 'replace_all' is true, \
                which changes every occurrence. Returns how many were replaced."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "find": { "type": "string" },
                    "replace": { "type": "string" },
                    "regex": { "type": "boolean" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "find", "replace"]
            }),
        }
    }
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let path = input["path"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'path'".into()))?
            .to_string();
        let find = input["find"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'find'".into()))?
            .to_string();
        let replace = input["replace"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'replace'".into()))?
            .to_string();
        let regex = input["regex"].as_bool();
        let replace_all = input["replace_all"].as_bool();
        self.client
            .invoke(ToolCall::FindAndReplace(FindAndReplaceInput {
                path,
                find,
                replace,
                regex,
                replace_all,
            }))
            .await
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
            .and_then(super::render_output)
    }
}
