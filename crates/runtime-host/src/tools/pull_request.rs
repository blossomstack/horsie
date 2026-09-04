use crate::client::{RuntimeCallError, RuntimeClient};
use async_trait::async_trait;
use horsie_agentcore::{Tool, ToolCallError, ToolSpec, ToolValue};
use horsie_models::runtime::{InspectPullRequestDiffInput, InspectPullRequestInput, ToolCall};
use serde_json::{Value, json};

pub struct InspectPullRequestTool {
    client: RuntimeClient,
}

impl InspectPullRequestTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for InspectPullRequestTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "inspect_pull_request".to_string(),
            description: "Read compact pull-request metadata: title, branches, size, mergeability, checks, URL, and a bounded description. The reference may be a PR number in the current repository or a GitHub pull URL.".to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "reference": { "type": "string" }
                },
                "required": ["reference"]
            }),
        }
    }

    async fn execute(&self, input: Value, tool_call_id: &str) -> Result<ToolValue, ToolCallError> {
        let reference = reference(&input)?;
        self.client
            .invoke(
                tool_call_id,
                ToolCall::InspectPullRequest(InspectPullRequestInput { reference }),
            )
            .await
            .map_err(runtime_error)
            .and_then(super::render_output)
    }
}

pub struct InspectPullRequestDiffTool {
    client: RuntimeClient,
}

impl InspectPullRequestDiffTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for InspectPullRequestDiffTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "inspect_pull_request_diff".to_string(),
            description: "List changed paths and line counts for a pull request, or return one named file's patch. Prefer the listing before requesting individual files; this avoids loading an entire large PR diff.".to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "reference": { "type": "string" },
                    "path": { "type": "string", "description": "One changed path to return as a patch; omit for the compact file list." }
                },
                "required": ["reference"]
            }),
        }
    }

    async fn execute(&self, input: Value, tool_call_id: &str) -> Result<ToolValue, ToolCallError> {
        let reference = reference(&input)?;
        let path = input["path"].as_str().map(str::to_string);
        self.client
            .invoke(
                tool_call_id,
                ToolCall::InspectPullRequestDiff(InspectPullRequestDiffInput { reference, path }),
            )
            .await
            .map_err(runtime_error)
            .and_then(super::render_output)
    }
}

fn reference(input: &Value) -> Result<String, ToolCallError> {
    input["reference"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| ToolCallError::InvalidInput("missing 'reference'".to_string()))
}

fn runtime_error(error: RuntimeCallError) -> ToolCallError {
    ToolCallError::ExecutionFailed(error.to_string())
}
