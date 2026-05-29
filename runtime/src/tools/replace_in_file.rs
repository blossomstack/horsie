use models::runtime::{ReplaceInFileInput, ReplaceMode, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: ReplaceInFileInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let new_content = match &input.mode {
            ReplaceMode::Regex(r) => {
                let re = regex::Regex::new(&r.pattern).map_err(|e| e.to_string())?;
                re.replace_all(&content, input.replacement.as_str())
                    .into_owned()
            }
            ReplaceMode::Lines(l) => {
                let mut lines: Vec<&str> = content.lines().collect();
                let start = (l.start_line as usize).saturating_sub(1).min(lines.len());
                let end = (l.end_line as usize).min(lines.len());
                let replacement_lines: Vec<&str> = input.replacement.lines().collect();
                lines.splice(start..end, replacement_lines);
                lines.join("\n")
            }
        };
        std::fs::write(&path, new_content).map_err(|e| e.to_string())?;
        Ok::<String, String>(format!("Replaced in '{}'.", input.path))
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
