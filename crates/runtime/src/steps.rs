//! Provision steps: setup the runtime executes inside its sandbox after
//! hackamore provisioning and before the agent message loop. Steps arrive as
//! JSON in the `HORSIE_PROVISION` env var (vendor-injected, mirroring the
//! hackamore env pattern). Fail closed: malformed JSON, an unknown step kind,
//! or a failed command aborts provisioning with a human-readable error.

use base64::Engine;
use horsie_models::executor::ProvisionStep;

use crate::workspace::WorkspaceRegistry;
use std::path::{Component, Path};

/// Parse the provision steps from the raw `HORSIE_PROVISION` value.
/// `None`/empty → no steps.
pub fn steps_from_env(raw: Option<String>) -> Result<Vec<ProvisionStep>, String> {
    match raw.filter(|s| !s.is_empty()) {
        None => Ok(vec![]),
        Some(json) => serde_json::from_str(&json).map_err(|e| {
            format!(
                "{} is not valid provision-steps JSON: {e}",
                horsie_models::ENV_PROVISION
            )
        }),
    }
}

/// Run all steps in order, failing on the first error.
pub async fn run_steps(
    registry: &WorkspaceRegistry,
    steps: &[ProvisionStep],
    github_token: Option<&str>,
) -> Result<(), String> {
    for step in steps {
        match step.uses.as_str() {
            "git_checkout" => git_checkout(registry, step, github_token)
                .await
                .map_err(|e| format!("provision step '{}' failed: {e}", step.name))?,
            other => {
                return Err(format!(
                    "provision step '{}' has unknown kind '{other}'",
                    step.name
                ));
            }
        }
    }
    Ok(())
}

fn param<'a>(step: &'a ProvisionStep, key: &str) -> Option<&'a str> {
    step.with
        .iter()
        .find(|p| p.key == key)
        .map(|p| p.value.as_str())
}

/// Repo directory derived from a clone URL: last path segment, minus a `.git`
/// suffix. The scheme is stripped first so a schemes-only URL (e.g.
/// "https:///") has no path segment and errors instead of yielding "https:".
fn dir_from_url(url: &str) -> Result<String, String> {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let base = without_scheme
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_end_matches(".git");
    if base.is_empty() {
        return Err(format!("cannot derive a directory name from url '{url}'"));
    }
    Ok(base.to_string())
}

/// A checkout `dir` must stay inside the workspace: relative, no `..`.
fn validate_dir(dir: &str) -> Result<(), String> {
    let p = Path::new(dir);
    if p.is_absolute() {
        return Err(format!("dir '{dir}' must be relative"));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("dir '{dir}' must not contain '..'"));
    }
    Ok(())
}

fn is_github(url: &str) -> bool {
    url.strip_prefix("https://")
        .map(|rest| rest.starts_with("github.com/"))
        .unwrap_or(false)
}

/// `http.extraHeader` value for a GitHub token. Passed to git via one-shot
/// `GIT_CONFIG_*` env vars, so the token never reaches argv or `.git/config`.
fn github_auth_header(token: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    format!("AUTHORIZATION: basic {b64}")
}

async fn git_checkout(
    registry: &WorkspaceRegistry,
    step: &ProvisionStep,
    github_token: Option<&str>,
) -> Result<(), String> {
    let url = param(step, "url").ok_or("git_checkout requires a 'url' param")?;
    let dir = match param(step, "dir") {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => dir_from_url(url)?,
    };
    validate_dir(&dir)?;
    let ws = registry.resolve(&param(step, "workspace").map(str::to_string))?;
    let target = ws.join(&dir);
    if target.join(".git").is_dir() {
        return Ok(()); // already cloned (preserved workspace): idempotent skip
    }
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("clone").arg(url).arg(&dir).current_dir(&ws);
    if let (Some(token), true) = (github_token, is_github(url)) {
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", github_auth_header(token));
    }
    run_git(cmd, "clone").await?;
    if let Some(r) = param(step, "ref").filter(|r| !r.is_empty()) {
        let mut co = tokio::process::Command::new("git");
        co.arg("checkout").arg(r).current_dir(&target);
        run_git(co, "checkout").await?;
    }
    Ok(())
}

/// Run a git command, mapping failure to its stderr tail (last 8 lines).
async fn run_git(mut cmd: tokio::process::Command, what: &str) -> Result<(), String> {
    let out = cmd.output().await.map_err(|e| format!("git {what}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    let start = lines.len().saturating_sub(8);
    Err(format!("git {what} failed: {}", lines[start..].join("\n")))
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
    use horsie_models::Workspace;
    use horsie_models::executor::StepParam;
    use std::path::PathBuf;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A local repo with one commit on `main` and a `feature` branch adding a file.
    fn fixture_repo(dir: &Path) -> String {
        git(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "hello").unwrap();
        git(dir, &["add", "."]);
        git(
            dir,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ],
        );
        git(dir, &["checkout", "-b", "feature"]);
        std::fs::write(dir.join("FEATURE.md"), "f").unwrap();
        git(dir, &["add", "."]);
        git(
            dir,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "feature",
            ],
        );
        git(dir, &["checkout", "main"]);
        format!("file://{}", dir.display())
    }

    fn step(uses: &str, with: &[(&str, &str)]) -> ProvisionStep {
        ProvisionStep {
            name: "test-step".into(),
            uses: uses.into(),
            with: with
                .iter()
                .map(|(k, v)| StepParam {
                    key: (*k).to_string(),
                    value: (*v).to_string(),
                })
                .collect(),
        }
    }

    fn registry(ws: &Path) -> WorkspaceRegistry {
        WorkspaceRegistry::new(vec![Workspace {
            name: "main".into(),
            path: PathBuf::from(ws),
        }])
    }

    #[test]
    fn steps_from_env_none_and_empty_are_no_steps() {
        assert_eq!(steps_from_env(None).unwrap(), vec![]);
        assert_eq!(steps_from_env(Some(String::new())).unwrap(), vec![]);
    }

    #[test]
    fn steps_from_env_rejects_malformed_json() {
        let err = steps_from_env(Some("not-json".into())).unwrap_err();
        assert!(err.contains("HORSIE_PROVISION"), "{err}");
    }

    #[test]
    fn steps_from_env_parses_steps() {
        let json = r#"[{"name":"co","uses":"git_checkout","with":[{"key":"url","value":"u"}]}]"#;
        let steps = steps_from_env(Some(json.into())).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].uses, "git_checkout");
    }

    #[tokio::test]
    async fn git_checkout_clones_and_is_idempotent() {
        let src = tempfile::tempdir().unwrap();
        let url = fixture_repo(src.path());
        let ws = tempfile::tempdir().unwrap();
        let reg = registry(ws.path());
        let steps = vec![step(
            "git_checkout",
            &[("url", url.as_str()), ("dir", "repo")],
        )];

        run_steps(&reg, &steps, None).await.unwrap();
        assert!(ws.path().join("repo/README.md").is_file());

        // Second run: the .git guard skips the clone instead of failing.
        run_steps(&reg, &steps, None).await.unwrap();
    }

    #[tokio::test]
    async fn git_checkout_derives_dir_and_checks_out_ref() {
        let src = tempfile::tempdir().unwrap();
        let url = fixture_repo(src.path());
        let ws = tempfile::tempdir().unwrap();
        let reg = registry(ws.path());
        // No dir param → basename of the fixture tempdir; ref → feature branch.
        let steps = vec![step(
            "git_checkout",
            &[("url", url.as_str()), ("ref", "feature")],
        )];

        run_steps(&reg, &steps, None).await.unwrap();
        let dir_name = src
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(ws.path().join(&dir_name).join("FEATURE.md").is_file());
    }

    #[tokio::test]
    async fn unknown_step_kind_fails_closed() {
        let ws = tempfile::tempdir().unwrap();
        let err = run_steps(&registry(ws.path()), &[step("npm_install", &[])], None)
            .await
            .unwrap_err();
        assert!(err.contains("unknown kind 'npm_install'"), "{err}");
    }

    #[tokio::test]
    async fn git_checkout_requires_url_and_validates_dir() {
        let ws = tempfile::tempdir().unwrap();
        let reg = registry(ws.path());
        let err = run_steps(&reg, &[step("git_checkout", &[])], None)
            .await
            .unwrap_err();
        assert!(err.contains("url"), "{err}");

        for bad in ["/abs", "../escape", "a/../../b"] {
            let err = run_steps(
                &reg,
                &[step(
                    "git_checkout",
                    &[("url", "https://x/y.git"), ("dir", bad)],
                )],
                None,
            )
            .await
            .unwrap_err();
            assert!(err.contains("dir"), "dir '{bad}' should be rejected: {err}");
        }
    }

    #[tokio::test]
    async fn clone_failure_surfaces_git_stderr() {
        let ws = tempfile::tempdir().unwrap();
        let err = run_steps(
            &registry(ws.path()),
            &[step(
                "git_checkout",
                &[("url", "file:///nonexistent-repo-xyz")],
            )],
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("git clone failed"), "{err}");
    }

    #[test]
    fn github_auth_is_scoped_and_never_plain() {
        assert!(is_github("https://github.com/org/repo"));
        assert!(!is_github("https://gitlab.com/org/repo"));
        assert!(!is_github("file:///tmp/x"));
        let header = github_auth_header("tok-123");
        assert!(header.starts_with("AUTHORIZATION: basic "));
        assert!(!header.contains("tok-123"), "raw token must not appear");
    }

    #[test]
    fn dir_from_url_strips_git_suffix() {
        assert_eq!(
            dir_from_url("https://github.com/o/repo.git").unwrap(),
            "repo"
        );
        assert_eq!(dir_from_url("https://github.com/o/repo").unwrap(), "repo");
        assert!(dir_from_url("https:///").is_err());
    }
}
