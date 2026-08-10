//! Thin wrappers over the `git` binary. Behind the `git` feature: only the CLI
//! and server clone; the runtime reads already-materialised trees.

use std::path::Path;
use std::process::Command;

/// Shallow-clone `url` into `dest`, optionally at `git_ref`.
///
/// `git_ref` may be a branch, a tag **or a commit sha**. The three need two
/// different mechanisms: `clone --branch` resolves a name and flatly refuses a
/// sha, so a pinned commit falls back to fetching that one object. Pinning is
/// the case that matters most — it is the only `git_ref` that makes an install
/// reproducible — so it cannot be the one that does not work.
pub fn clone(url: &str, git_ref: Option<&str>, dest: &Path) -> Result<(), String> {
    // Here rather than at each caller: the URL becomes one of `git`'s
    // arguments, so a value beginning with `-` is an option and `ext::` is
    // arbitrary command execution. Any caller that forgot would be a hole, and
    // this is the one place all of them funnel through.
    crate::remote_url::check_git_url(url)?;
    let Some(git_ref) = git_ref else {
        return clone_at_name(url, None, dest);
    };
    match clone_at_name(url, Some(git_ref), dest) {
        Ok(()) => Ok(()),
        Err(by_name) => {
            // A failed clone leaves a partial directory behind, and the retry
            // needs to `git init` into an empty one.
            let _ = std::fs::remove_dir_all(dest);
            fetch_commit(url, git_ref, dest)
                .map_err(|by_sha| format!("{by_name}; and not a commit either: {by_sha}"))
        }
    }
}

/// `git clone --depth 1`, optionally `--branch <name>`.
fn clone_at_name(url: &str, name: Option<&str>, dest: &Path) -> Result<(), String> {
    let dest_str = dest.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["clone", "--depth", "1"];
    if let Some(r) = name {
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

/// Materialise exactly one commit: init an empty repo and fetch that object.
///
/// Still `--depth 1`, so pinning a sha costs no more than cloning a branch.
/// A server that refuses to serve an arbitrary sha fails here rather than
/// silently handing back a different commit.
fn fetch_commit(url: &str, sha: &str, dest: &Path) -> Result<(), String> {
    let dest_str = dest.to_string_lossy().into_owned();
    std::fs::create_dir_all(dest).map_err(|e| format!("create {dest_str}: {e}"))?;
    run_git("init", &["init", "--quiet", &dest_str])?;
    run_git(
        "remote add",
        &["-C", &dest_str, "remote", "add", "origin", url],
    )?;
    run_git(
        "fetch",
        &["-C", &dest_str, "fetch", "--depth", "1", "origin", sha],
    )?;
    run_git("checkout", &["-C", &dest_str, "checkout", "--quiet", sha])
}

fn run_git(what: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {what} failed: {}",
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

    /// The case pinning exists for. `clone --branch` refuses a sha outright, so
    /// this exercises the fetch-one-object path rather than a second name lookup.
    #[test]
    fn clone_at_a_commit_sha() {
        let src = TempDir::new().unwrap();
        fixture_repo(src.path());
        let first = head_sha(src.path()).unwrap();

        // A second commit, so checking out the first proves the sha was honoured
        // rather than the default branch quietly being taken.
        std::fs::write(src.path().join("skills/x/SKILL.md"), "---\nname: y\n---\n").unwrap();
        for args in [vec!["add", "-A"], vec!["commit", "-qm", "second"]] {
            let out = Command::new("git")
                .args(&args)
                .current_dir(src.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }

        let dst = TempDir::new().unwrap();
        let dest = dst.path().join("clone");
        clone(
            &format!("file://{}", src.path().display()),
            Some(&first),
            &dest,
        )
        .unwrap();

        assert_eq!(head_sha(&dest).as_deref(), Some(first.as_str()));
        let skill = std::fs::read_to_string(dest.join("skills/x/SKILL.md")).unwrap();
        assert!(skill.contains("name: x"), "checked out the wrong commit");
    }

    /// A ref that is neither a name nor a commit has to say both attempts
    /// failed; reporting only the second reads as "that is not a sha", which
    /// sends the reader looking in the wrong place.
    #[test]
    fn clone_at_an_unknown_ref_reports_both_attempts() {
        let src = TempDir::new().unwrap();
        fixture_repo(src.path());
        let dst = TempDir::new().unwrap();
        let err = clone(
            &format!("file://{}", src.path().display()),
            Some("no-such-ref"),
            &dst.path().join("clone"),
        )
        .unwrap_err();
        assert!(err.contains("git clone failed"), "err: {err}");
        assert!(err.contains("not a commit either"), "err: {err}");
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
