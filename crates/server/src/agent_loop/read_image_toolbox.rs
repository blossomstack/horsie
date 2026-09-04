use super::ArtifactSink;
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, ToolValue, Toolbox};
use horsie_models::agent::ArtifactKind;
use horsie_models::runtime::{ReadImageInput, ToolCall};
use horsie_runtime_host::RuntimeClient;
use serde_json::{Value, json};
use std::path::Path;

pub const READ_IMAGE_TOOL: &str = "read_image";

pub struct ReadImageToolbox {
    client: RuntimeClient,
    artifacts: Option<ArtifactSink>,
}

impl ReadImageToolbox {
    #[must_use]
    pub fn new(client: RuntimeClient, artifacts: Option<ArtifactSink>) -> Self {
        Self { client, artifacts }
    }
}

#[async_trait]
impl Toolbox for ReadImageToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: READ_IMAGE_TOOL.to_string(),
            description: "Load an image file so you can view its contents.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        }]
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if name != READ_IMAGE_TOOL {
            return Err(ToolCallError::InvalidInput(format!(
                "no tool named '{name}'"
            )));
        }
        let path = input["path"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'path'".into()))?
            .to_string();
        let filename = Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let output = self
            .client
            .invoke(tool_call_id, ToolCall::ReadImage(ReadImageInput { path }))
            .await
            .map_err(|error| ToolCallError::ExecutionFailed(error.to_string()))?;
        let [bytes] = output.artifacts.as_slice() else {
            return Err(ToolCallError::ExecutionFailed(
                "runtime did not return exactly one image".to_string(),
            ));
        };
        let artifacts = self.artifacts.as_ref().ok_or_else(|| {
            ToolCallError::ExecutionFailed("artifact storage is unavailable".to_string())
        })?;
        let artifact = artifacts
            .store_one(Vec::from(bytes.clone()), filename)
            .await
            .map_err(ToolCallError::ExecutionFailed)?;
        if !matches!(artifact.kind, ArtifactKind::Image(_)) {
            return Err(ToolCallError::ExecutionFailed(
                "the file is not a supported image".to_string(),
            ));
        }

        let original_output_bytes = output.original_output_bytes;
        let spilled_output_bytes = output.spilled_output_bytes;
        Ok(ToolOutcome::Result(
            ToolValue::with_artifacts(Value::String(output.stdout), vec![artifact])
                .with_output_metrics(original_output_bytes, spilled_output_bytes),
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use crate::projects::ProjectId;
    use fluorite::Bytes;
    use horsie_models::runtime::ToolOutput;
    use horsie_runtime_host::{MockTransport, TransportProbe};
    use std::sync::Arc;

    fn png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00,
        ]
    }

    async fn toolbox(transport: MockTransport) -> (ReadImageToolbox, crate::db::Db) {
        let db = crate::db::testing::db().await;
        let service = Arc::new(crate::artifacts::ArtifactService::in_database(db.clone()));
        let sink = ArtifactSink::new(service, ProjectId::new("p1"));
        let client = RuntimeClient::detached(transport, "test-agent");
        (ReadImageToolbox::new(client, Some(sink)), db)
    }

    #[tokio::test]
    async fn stores_the_runtime_bytes_and_returns_an_image_artifact() {
        let probe = TransportProbe::new();
        let transport = MockTransport::output(ToolOutput {
            stdout: "Image loaded.".into(),
            stderr: String::new(),
            exit_code: 0,
            artifacts: vec![Bytes(png())],
            original_output_bytes: 0,
            spilled_output_bytes: 0,
        })
        .observed_by(&probe);
        let (toolbox, _db) = toolbox(transport).await;

        let outcome = toolbox
            .execute(READ_IMAGE_TOOL, json!({"path": "shots/page.png"}), "tc1")
            .await
            .unwrap();

        let ToolOutcome::Result(value) = outcome else {
            panic!("read_image stopped the run");
        };
        assert_eq!(value.value, Value::String("Image loaded.".into()));
        assert_eq!(value.artifacts.len(), 1);
        assert_eq!(value.artifacts[0].media_type, "image/png");
        assert_eq!(value.artifacts[0].filename.as_deref(), Some("page.png"));
        assert!(matches!(value.artifacts[0].kind, ArtifactKind::Image(_)));
        match &probe.invocations()[0] {
            ToolCall::ReadImage(input) => assert_eq!(input.path, "shots/page.png"),
            other => panic!("expected ReadImage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_a_non_image_artifact() {
        let transport = MockTransport::output(ToolOutput {
            stdout: "Image loaded.".into(),
            stderr: String::new(),
            exit_code: 0,
            artifacts: vec![Bytes(b"%PDF-1.4\n".to_vec())],
            original_output_bytes: 0,
            spilled_output_bytes: 0,
        });
        let (toolbox, _db) = toolbox(transport).await;

        let error = toolbox
            .execute(READ_IMAGE_TOOL, json!({"path": "document.pdf"}), "tc1")
            .await
            .unwrap_err();

        assert!(matches!(error, ToolCallError::ExecutionFailed(_)));
    }
}
