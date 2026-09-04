use horsie_models::runtime::{ReadImageInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: ReadImageInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || std::fs::read(path)).await {
        Ok(Ok(bytes)) => ToolResult::Ok(ToolOutput {
            stdout: "Image loaded.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            artifacts: vec![fluorite::Bytes(bytes)],
            original_output_bytes: 0,
            spilled_output_bytes: 0,
        }),
        Ok(Err(error)) => ToolResult::Err(ToolError {
            reason: error.to_string(),
        }),
        Err(error) => ToolResult::Err(ToolError {
            reason: error.to_string(),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn reads_image_bytes_as_an_artifact() {
        let dir = TempDir::new().unwrap();
        let bytes = b"not validated in the runtime";
        std::fs::write(dir.path().join("image.png"), bytes).unwrap();

        let result = exec(
            dir.path(),
            ReadImageInput {
                path: "image.png".into(),
            },
        )
        .await;

        match result {
            ToolResult::Ok(output) => {
                assert_eq!(output.stdout, "Image loaded.");
                assert_eq!(output.artifacts, vec![fluorite::Bytes(bytes.to_vec())]);
            }
            ToolResult::Err(error) => panic!("{}", error.reason),
        }
    }

    #[tokio::test]
    async fn missing_image_is_an_error() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            ReadImageInput {
                path: "missing.png".into(),
            },
        )
        .await;

        assert!(matches!(result, ToolResult::Err(_)));
    }
}
