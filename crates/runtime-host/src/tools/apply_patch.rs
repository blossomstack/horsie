use crate::client::{RuntimeCallError, RuntimeClient};
use async_trait::async_trait;
use horsie_agentcore::{Tool, ToolCallError, ToolSpec, ToolValue};
use horsie_models::runtime::{ApplyPatchInput, ToolCall};
use serde_json::{Value, json};

pub struct ApplyPatchTool {
    client: RuntimeClient,
}

impl ApplyPatchTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".to_string(),
            description: "Apply one validated patch containing ordered changes to one or more \
                files. Use the *** Begin Patch format with *** Update File, *** Add File, \
                *** Delete File, or *** Move File: <source> -> <destination> sections. An \
                update may contain several @@ hunks. Already-applied and context-only hunks \
                are reported and skipped. Every changing hunk must include enough unchanged \
                or removed lines to match exactly one location. The complete syntax and every \
                hunk are validated before any file is changed. \
                Prefer this over repeated edit calls when related changes touch several \
                locations. Do not issue other file mutations in parallel with apply_patch."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "A patch delimited by *** Begin Patch and *** End Patch. Each file section starts with *** Update File: <path>, *** Add File: <path>, *** Delete File: <path>, or *** Move File: <source> -> <destination>. Each update hunk starts with @@; its following lines start with a space for context, - for removal, or + for addition. Added-file content uses + on every line."
                    }
                },
                "required": ["patch"]
            }),
        }
    }

    async fn execute(&self, input: Value, tool_call_id: &str) -> Result<ToolValue, ToolCallError> {
        let patch = input["patch"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'patch'".into()))?
            .to_string();
        self.client
            .invoke(
                tool_call_id,
                ToolCall::ApplyPatch(ApplyPatchInput { patch }),
            )
            .await
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
            .and_then(super::render_output)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::testkit::MockTransport;
    use horsie_agentcore::Toolbox;

    #[test]
    fn schema_accepts_only_the_patch_document() {
        let tool =
            ApplyPatchTool::new(RuntimeClient::detached(MockTransport::ok(""), "test-agent"));
        let spec = tool.spec();
        assert_eq!(spec.name, "apply_patch");
        assert_eq!(spec.input_schema["required"], json!(["patch"]));
        assert_eq!(spec.input_schema["additionalProperties"], false);
    }

    #[test]
    fn registered_tool_is_advertised() {
        let client = RuntimeClient::detached(MockTransport::ok(""), "test-agent");
        let toolbox =
            super::super::add_runtime_tools(horsie_agentcore::ToolboxImpl::default(), client);
        assert!(
            toolbox
                .specs()
                .iter()
                .any(|spec| spec.name == "apply_patch")
        );
    }
}
