use horsie_models::runtime::{ListFilesInput, ToolError, ToolOutput, ToolResult};
use ignore::WalkBuilder;
use std::path::Path;

pub async fn exec(working_dir: &Path, input: ListFilesInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        // read_dir's contract: listing a non-directory is an error, not an
        // empty result. The check follows symlinks, so a link to a directory
        // still lists.
        if !path.is_dir() {
            return Err(format!("not a directory: {}", path.display()));
        }
        let mut lines = Vec::new();
        // Depth one, ignore-aware: hidden and gitignored immediate children
        // never appear, unlike the old read_dir listing.
        for entry in WalkBuilder::new(&path).max_depth(Some(1)).build() {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.depth() == 0 {
                continue;
            }
            // metadata() (not the entry's file type) so a symlink to a
            // directory keeps its "d" prefix, as read_dir's metadata() did.
            let meta = std::fs::metadata(entry.path()).map_err(|e| e.to_string())?;
            let kind = if meta.is_dir() { "d" } else { "f" };
            let name = entry.file_name().to_string_lossy().into_owned();
            lines.push(format!("{kind} {name}"));
        }
        lines.sort();
        Ok::<String, String>(lines.join("\n"))
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput {
            stdout,
            stderr: String::new(),
            exit_code: 0,
            artifacts: Vec::new(),
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
    async fn list_files_shows_entries() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let result = exec(dir.path(), ListFilesInput { path: ".".into() }).await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("a.txt"));
                assert!(o.stdout.contains("sub"));
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn list_files_skips_ignored_and_hidden_entries() {
        let dir = TempDir::new().unwrap();
        seed_repo(&dir);
        let result = exec(dir.path(), ListFilesInput { path: ".".into() }).await;
        match result {
            ToolResult::Ok(o) => {
                let lines: Vec<&str> = o.stdout.lines().collect();
                assert!(lines.contains(&"f visible.txt"), "{}", o.stdout);
                assert!(lines.contains(&"d sub"), "{}", o.stdout);
                assert!(!lines.contains(&"f ignored.txt"), "{}", o.stdout);
                assert!(!lines.contains(&"d ignored_dir"), "{}", o.stdout);
                assert!(!lines.contains(&"d .hidden"), "{}", o.stdout);
                assert!(!lines.contains(&"f .hidden_file.txt"), "{}", o.stdout);
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn list_files_errors_on_a_non_directory() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "").unwrap();
        let result = exec(
            dir.path(),
            ListFilesInput {
                path: "f.txt".into(),
            },
        )
        .await;
        assert!(matches!(result, ToolResult::Err(_)));
    }
}
