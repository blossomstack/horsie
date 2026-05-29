use models::runtime::{GrepInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: GrepInput) -> ToolResult {
    let base = match &input.path {
        Some(p) => working_dir.join(p),
        None => working_dir.to_path_buf(),
    };
    let file_pat = input
        .file_pattern
        .clone()
        .unwrap_or_else(|| "**/*".to_string());
    let max = input.max_results.unwrap_or(1000) as usize;
    let pattern = input.pattern.clone();
    match tokio::task::spawn_blocking(move || {
        let re = regex::Regex::new(&pattern).map_err(|e| e.to_string())?;
        let glob_pat = format!("{}/{}", base.display(), file_pat);
        let mut results = Vec::new();
        'outer: for path in glob::glob(&glob_pat).map_err(|e| e.to_string())?.flatten() {
            if path.is_file()
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        results.push(format!("{}:{}: {}", path.display(), i + 1, line));
                        if results.len() >= max {
                            break 'outer;
                        }
                    }
                }
            }
        }
        Ok::<String, String>(results.join("\n"))
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn grep_finds_match() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello world\nfoo bar").unwrap();
        let result = exec(
            dir.path(),
            GrepInput {
                pattern: "hello".into(),
                path: None,
                file_pattern: None,
                max_results: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert!(o.stdout.contains("hello world")),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}
