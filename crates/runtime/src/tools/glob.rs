use horsie_models::runtime::{GlobInput, ToolError, ToolOutput, ToolResult};
use ignore::{WalkBuilder, WalkState};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

fn lock_matches(matches: &Mutex<Vec<String>>) -> MutexGuard<'_, Vec<String>> {
    matches
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub async fn exec(working_dir: &Path, input: GlobInput) -> ToolResult {
    let base = match &input.path {
        Some(p) => working_dir.join(p),
        None => working_dir.to_path_buf(),
    };
    let max = input.max_results.unwrap_or(1000) as usize;
    match tokio::task::spawn_blocking(move || {
        // The pattern selects among paths the walker admits; it does not
        // re-include ignored or hidden content.
        let matcher = globset::Glob::new(&input.pattern)
            .map_err(|e| e.to_string())?
            .compile_matcher();
        let stop = Arc::new(AtomicBool::new(false));
        let matches = Arc::new(Mutex::new(Vec::<String>::new()));
        let walker = WalkBuilder::new(&base).build_parallel();
        let (stop_w, matches_w) = (Arc::clone(&stop), Arc::clone(&matches));
        walker.run(move || {
            let (stop, matches) = (Arc::clone(&stop_w), Arc::clone(&matches_w));
            let (matcher, base) = (matcher.clone(), base.clone());
            Box::new(move |entry| {
                if stop.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                // Traversal errors are not matches and do not count against
                // the result cap, matching the old best-effort flatten.
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };
                let path = entry.path();
                let Ok(rel) = path.strip_prefix(&base) else {
                    return WalkState::Continue;
                };
                if !matcher.is_match(rel) {
                    return WalkState::Continue;
                }
                let mut m = lock_matches(&matches);
                if m.len() >= max {
                    stop.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                m.push(path.to_string_lossy().into_owned());
                WalkState::Continue
            })
        });
        let m = lock_matches(&matches);
        Ok::<String, String>(m.join("\n"))
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
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/deep.txt"), "visible token").unwrap();
    }

    #[tokio::test]
    async fn recursive_glob_skips_ignored_and_hidden_paths() {
        let dir = TempDir::new().unwrap();
        seed_repo(&dir);
        let result = exec(
            dir.path(),
            GlobInput {
                pattern: "**/*.txt".into(),
                path: None,
                max_results: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert!(
                    o.stdout.lines().any(|l| l.ends_with("visible.txt")),
                    "visible match missing: {}",
                    o.stdout
                );
                assert!(
                    o.stdout.lines().any(|l| l.ends_with("sub/deep.txt")),
                    "nested visible match missing: {}",
                    o.stdout
                );
                assert!(
                    !o.stdout.contains("ignored"),
                    "gitignored path surfaced: {}",
                    o.stdout
                );
                assert!(
                    !o.stdout.contains(".hidden"),
                    "hidden path surfaced: {}",
                    o.stdout
                );
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn pattern_filters_the_visible_walk() {
        let dir = TempDir::new().unwrap();
        seed_repo(&dir);
        let result = exec(
            dir.path(),
            GlobInput {
                pattern: "sub/*.txt".into(),
                path: None,
                max_results: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                let lines: Vec<&str> = o.stdout.lines().collect();
                assert_eq!(lines.len(), 1, "unexpected matches: {}", o.stdout);
                assert!(lines[0].ends_with("sub/deep.txt"), "{}", lines[0]);
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn malformed_pattern_is_a_tool_error() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            GlobInput {
                pattern: "[".into(),
                path: None,
                max_results: None,
            },
        )
        .await;
        assert!(matches!(result, ToolResult::Err(_)));
    }
}
