use horsie_models::runtime::{GrepInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

/// Per-line cap so a single minified or generated line can't eat the budget.
const MAX_LINE_CHARS: usize = 500;
/// Total output budget. Tool output rides the conversation history, so grep
/// stops well below the dispatcher's 50 KB saw and says so.
const MAX_OUTPUT_BYTES: usize = 20_000;

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
        let mut results: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        // Why the scan stopped early; drives the footer.
        let mut stopped: Option<String> = None;
        'outer: for path in glob::glob(&glob_pat).map_err(|e| e.to_string())?.flatten() {
            if path.is_file()
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        let line = if line.chars().count() > MAX_LINE_CHARS {
                            format!("{}…", line.chars().take(MAX_LINE_CHARS).collect::<String>())
                        } else {
                            line.to_string()
                        };
                        let row = format!("{}:{}: {}", path.display(), i + 1, line);
                        bytes += row.len() + 1;
                        results.push(row);
                        if results.len() >= max {
                            stopped = Some(format!(
                                "stopped at max_results={max} — narrow with `path`, \
                                 `file_pattern`, or a tighter `pattern`"
                            ));
                            break 'outer;
                        }
                        if bytes > MAX_OUTPUT_BYTES {
                            stopped = Some(format!(
                                "truncated after {} matches ({} KB cap) — narrow with \
                                 `path`, `file_pattern`, or a tighter `pattern`",
                                results.len(),
                                MAX_OUTPUT_BYTES / 1000
                            ));
                            break 'outer;
                        }
                    }
                }
            }
        }
        let mut out = results.join("\n");
        if let Some(reason) = stopped {
            out.push_str(&format!("\n[{reason}]"));
        }
        Ok::<String, String>(out)
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
    async fn long_matching_lines_are_truncated() {
        let dir = TempDir::new().unwrap();
        let long = format!("hit{}", "x".repeat(2000));
        std::fs::write(dir.path().join("f.txt"), long).unwrap();
        let result = exec(
            dir.path(),
            GrepInput {
                pattern: "hit".into(),
                path: None,
                file_pattern: None,
                max_results: None,
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert!(
                    o.stdout.len() < 1000,
                    "line not truncated: {}",
                    o.stdout.len()
                );
                assert!(o.stdout.contains('…'), "no truncation marker: {}", o.stdout);
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn byte_budget_stops_early_with_footer() {
        let dir = TempDir::new().unwrap();
        // 400 files x ~90-byte match rows ≈ 36 KB — past the 20 KB budget.
        for i in 0..400 {
            std::fs::write(
                dir.path().join(format!("f{i}.txt")),
                format!("hit {:>50}\n", i),
            )
            .unwrap();
        }
        let result = exec(
            dir.path(),
            GrepInput {
                pattern: "hit".into(),
                path: None,
                file_pattern: None,
                max_results: None,
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                let tail = &o.stdout[o.stdout.len().saturating_sub(300)..];
                assert!(tail.contains("20 KB cap"), "no footer: {tail}");
                assert!(
                    o.stdout.len() < 23_000,
                    "output not bounded: {}",
                    o.stdout.len()
                );
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

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
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert!(o.stdout.contains("hello world")),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}
