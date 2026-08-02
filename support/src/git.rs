//! Thin wrappers over the `git` binary. Behind the `git` feature: only the CLI
//! and server clone; the runtime reads already-materialised trees.

use std::path::Path;
use std::process::Command;

/// Shallow-clone `url` into `dest`, optionally at `git_ref`.
pub fn clone(url: &str, git_ref: Option<&str>, dest: &Path) -> Result<(), String> {
    let dest_str = dest.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["clone", "--depth", "1"];
    if let Some(r) = git_ref {
        args.push("--branch");
        args.push(r);
    }
    args.push(url);
    args.push(&dest_str);
    let out = Command::new("git")
        .args(&args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// `git pull --ff-only` in an existing clone.
///
/// A `--depth 1` clone cannot always fast-forward; when it cannot, the git
/// error is returned rather than silently re-cloning, so the caller can say
/// what happened.
pub fn pull_ff_only(dir: &Path) -> Result<(), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["pull", "--ff-only"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git pull failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// `HEAD` sha of a clone, or `None` when `dir` is not a repo.
pub fn head_sha(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
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

    /// A real local repo with one commit, usable as a `file://` clone source so
    /// tests never touch the network.
    fn fixture_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        std::fs::create_dir_all(dir.join("skills/x")).unwrap();
        std::fs::write(dir.join("skills/x/SKILL.md"), "---\nname: x\n---\n").unwrap();
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        run(&["add", "-A"]);
        run(&["commit", "-qm", "init"]);
    }

    #[test]
    fn clone_then_head_sha_then_pull() {
        let src = TempDir::new().unwrap();
        fixture_repo(src.path());
        let dst = TempDir::new().unwrap();
        let dest = dst.path().join("clone");

        clone(&format!("file://{}", src.path().display()), None, &dest).unwrap();
        assert!(dest.join("skills/x/SKILL.md").is_file());

        let sha = head_sha(&dest).unwrap();
        assert_eq!(sha.len(), 40, "sha: {sha}");

        // Fast-forward pull against an unchanged source is a no-op that succeeds.
        pull_ff_only(&dest).unwrap();
    }

    #[test]
    fn clone_at_a_ref() {
        let src = TempDir::new().unwrap();
        fixture_repo(src.path());
        let out = Command::new("git")
            .args(["branch", "other"])
            .current_dir(src.path())
            .output()
            .unwrap();
        assert!(out.status.success());

        let dst = TempDir::new().unwrap();
        let dest = dst.path().join("clone");
        clone(
            &format!("file://{}", src.path().display()),
            Some("other"),
            &dest,
        )
        .unwrap();
        assert!(dest.join("skills/x/SKILL.md").is_file());
    }

    #[test]
    fn clone_failure_reports_stderr() {
        let dst = TempDir::new().unwrap();
        let err = clone("file:///definitely/not/a/repo", None, &dst.path().join("c")).unwrap_err();
        assert!(err.contains("git clone failed"), "err: {err}");
    }

    #[test]
    fn head_sha_of_a_non_repo_is_none() {
        let dir = TempDir::new().unwrap();
        assert!(head_sha(dir.path()).is_none());
    }
}
