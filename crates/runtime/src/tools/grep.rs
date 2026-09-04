use horsie_models::runtime::{GrepInput, ToolError, ToolOutput, ToolResult};
use ignore::{WalkBuilder, WalkState};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Per-line cap so a single minified or generated line can't eat the budget.
const MAX_LINE_CHARS: usize = 500;
/// Total output budget. Tool output rides the conversation history, so grep
/// stops well below the dispatcher's 50 KB saw and says so.
const MAX_OUTPUT_BYTES: usize = 20_000;

/// Shared accumulation for the parallel walk: rows collected so far, the
/// running byte count, and why the scan stopped (drives the footer).
struct GrepState {
    rows: Vec<String>,
    bytes: usize,
    stopped: Option<String>,
}

fn lock_state(state: &Mutex<GrepState>) -> MutexGuard<'_, GrepState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
        let file_glob = globset::Glob::new(&file_pat)
            .map_err(|e| e.to_string())?
            .compile_matcher();
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(GrepState {
            rows: Vec::new(),
            bytes: 0,
            stopped: None,
        }));
        let walker = WalkBuilder::new(&base).build_parallel();
        let (stop_w, state_w) = (Arc::clone(&stop), Arc::clone(&state));
        walker.run(move || {
            let (stop, state) = (Arc::clone(&stop_w), Arc::clone(&state_w));
            let (re, file_glob, base) = (re.clone(), file_glob.clone(), base.clone());
            Box::new(move |entry| {
                if stop.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                // Traversal errors, unreadable files and non-UTF-8 content keep
                // the old best-effort contract: grep skips them silently.
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return WalkState::Continue;
                }
                let path = entry.path();
                let Ok(rel) = path.strip_prefix(&base) else {
                    return WalkState::Continue;
                };
                if !file_glob.is_match(rel) {
                    return WalkState::Continue;
                }
                // How many rows the global cap still admits, so one huge file
                // can't blow the budget (or memory) before the cap is checked.
                let remaining = {
                    let s = lock_state(&state);
                    if stop.load(Ordering::Relaxed) {
                        return WalkState::Quit;
                    }
                    max.saturating_sub(s.rows.len())
                };
                if remaining == 0 {
                    return WalkState::Quit;
                }
                let Ok(content) = std::fs::read_to_string(path) else {
                    return WalkState::Continue;
                };
                let mut rows: Vec<String> = Vec::new();
                for (i, line) in content.lines().enumerate() {
                    if !re.is_match(line) {
                        continue;
                    }
                    let line = if line.chars().count() > MAX_LINE_CHARS {
                        format!("{}…", line.chars().take(MAX_LINE_CHARS).collect::<String>())
                    } else {
                        line.to_string()
                    };
                    rows.push(format!("{}:{}: {}", path.display(), i + 1, line));
                    if rows.len() >= remaining {
                        break;
                    }
                }
                if rows.is_empty() {
                    return WalkState::Continue;
                }
                let mut s = lock_state(&state);
                if stop.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                // Fit the file's rows under both global caps. The row that
                // crosses the byte budget is kept so the footer still fires,
                // matching the old per-row accounting.
                let row_cap = max.saturating_sub(s.rows.len());
                let byte_cap = MAX_OUTPUT_BYTES.saturating_sub(s.bytes);
                let mut take = 0;
                let mut used = 0;
                for r in &rows {
                    if take >= row_cap {
                        break;
                    }
                    used += r.len() + 1;
                    take += 1;
                    if used > byte_cap {
                        break;
                    }
                }
                if take == 0 {
                    stop.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                let count = s.rows.len() + take;
                s.bytes += used;
                s.rows.extend(rows.into_iter().take(take));
                if count >= max {
                    s.stopped = Some(format!(
                        "stopped at max_results={max} — narrow with `path`, \
                         `file_pattern`, or a tighter `pattern`"
                    ));
                } else if s.bytes > MAX_OUTPUT_BYTES {
                    s.stopped = Some(format!(
                        "truncated after {} matches ({} KB cap) — narrow with \
                         `path`, `file_pattern`, or a tighter `pattern`",
                        count,
                        MAX_OUTPUT_BYTES / 1000
                    ));
                }
                if s.stopped.is_some() {
                    stop.store(true, Ordering::Relaxed);
                }
                WalkState::Continue
            })
        });
        let s = lock_state(&state);
        let mut out = s.rows.join("\n");
        if let Some(reason) = &s.stopped {
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
            artifacts: Vec::new(),
            original_output_bytes: 0,
            spilled_output_bytes: 0,
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

    /// A repo root for the ignore walker's defaults: `.gitignore` is only
    /// honored inside a git repository (`require_git` is on by default), so
    /// the fixture carries a `.git` marker the same way a real workspace does.
    fn seed_repo(dir: &TempDir) {
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored_dir/\nignored.txt\n").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "visible token").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "ignored token").unwrap();
        std::fs::create_dir(dir.path().join("ignored_dir")).unwrap();
        std::fs::write(dir.path().join("ignored_dir/nested.txt"), "ignored token").unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join(".hidden/hidden.txt"), "hidden token").unwrap();
        std::fs::write(dir.path().join(".hidden_file.txt"), "hidden token").unwrap();
    }

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
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert!(o.stdout.contains("hello world")),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn ignored_and_hidden_files_are_not_searched() {
        let dir = TempDir::new().unwrap();
        seed_repo(&dir);
        let result = exec(
            dir.path(),
            GrepInput {
                pattern: "token".into(),
                path: None,
                file_pattern: None,
                max_results: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert!(
                    o.stdout.contains("visible.txt"),
                    "visible match missing: {}",
                    o.stdout
                );
                assert!(
                    !o.stdout.contains("ignored token"),
                    "gitignored match surfaced: {}",
                    o.stdout
                );
                assert!(
                    !o.stdout.contains("hidden token"),
                    "hidden match surfaced: {}",
                    o.stdout
                );
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}
