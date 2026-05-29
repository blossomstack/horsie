use models::runtime::{GlobInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: GlobInput) -> ToolResult {
    let base = match &input.path {
        Some(p) => working_dir.join(p),
        None => working_dir.to_path_buf(),
    };
    let pattern = format!("{}/{}", base.display(), input.pattern);
    let max = input.max_results.unwrap_or(1000) as usize;
    match tokio::task::spawn_blocking(move || {
        let matches: Vec<String> = glob::glob(&pattern)
            .map_err(|e| e.to_string())?
            .take(max)
            .filter_map(|e| e.ok())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        Ok::<String, String>(matches.join("\n"))
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput {
            stdout,
            stderr: String::new(),
            exit_code: 0,
        }),
        Ok(Err(reason)) => ToolResult::Err(ToolError { reason }),
        Err(e) => ToolResult::Err(ToolError {
            reason: e.to_string(),
        }),
    }
}
