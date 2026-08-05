//! Shared plugin library: enumerate installed plugins under the `plugins_dir`
//! (the `horsie_shared` workspace), discover their skills, and run their
//! `SessionStart` hooks inside the sandbox.
//!
//! A plugin is a directory under `plugins_dir`. Its skills live under `skills/`
//! by default, or wherever its `.claude-plugin/plugin.json` `skills` field points
//! (string or array of paths). Hooks are declared in `hooks/hooks.json`.

use horsie_models::runtime::PluginSkill;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Max bytes of a single hook's captured stdout/stderr (mirrors the bash tool
/// clamp). The per-hook wall-clock budget lives with the runner, in `hooks`.
const HOOK_OUTPUT_CLAMP: usize = 50_000;

/// Plugin directories under `plugins_dir`, sorted for stable ordering. Best-effort:
/// an unreadable `plugins_dir` yields an empty list.
pub(crate) fn plugin_dirs(plugins_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(plugins_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Enumerate every installed plugin's skills. `rel_dir` is each skill's directory
/// relative to `plugins_dir` so the agent can read sibling resources via the
/// filesystem tools against `horsie_shared`.
pub fn discover_skills(plugins_dir: &Path) -> Vec<PluginSkill> {
    let mut out = Vec::new();
    for plugin_root in plugin_dirs(plugins_dir) {
        // Best-effort: a plugin with a malformed manifest contributes nothing
        // rather than failing the whole scan.
        let root = match horsie_support::plugin::PluginRoot::inspect(&plugin_root) {
            Ok(root) => root,
            Err(e) => {
                tracing::warn!(
                    plugin = %plugin_root.display(),
                    error = %e,
                    "skipping plugin with unreadable manifest"
                );
                continue;
            }
        };
        let fallback = plugin_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = root.name(&fallback);
        for dir in &root.skill_dirs {
            let Ok(rel) = dir.strip_prefix(plugins_dir) else {
                continue;
            };
            if let Ok(content) = std::fs::read_to_string(dir.join("SKILL.md")) {
                out.push(PluginSkill {
                    plugin: name.clone(),
                    rel_dir: rel.to_string_lossy().into_owned(),
                    content,
                });
            }
        }
    }
    out
}

/// What one hook process produced. `code` is `None` when it could not be run to
/// completion — spawn failure or timeout — which callers treat as an outage
/// rather than as a decision.
pub(crate) struct HookRun {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run one hook command via `sh -c` with the plugin dir as cwd,
/// `CLAUDE_PLUGIN_ROOT` set, `hook_path` prepended to PATH, and `payload` on
/// stdin.
///
/// Returns the raw result rather than an interpretation: `SessionStart` wants
/// only injected context, while the tool-hook dispatcher needs the exit code
/// and stderr to tell a block from an outage.
pub(crate) async fn run_hook_raw(
    plugin_root: &Path,
    command: &str,
    hook_path: &[PathBuf],
    payload: &str,
    timeout: Duration,
) -> HookRun {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut path_var = std::env::var("PATH").unwrap_or_default();
    if !hook_path.is_empty() {
        let prefix = hook_path
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        path_var = if path_var.is_empty() {
            prefix
        } else {
            format!("{prefix}:{path_var}")
        };
    }

    let failed = |why: &str| {
        tracing::warn!(command, why, "plugin hook did not run");
        HookRun {
            code: None,
            stdout: String::new(),
            stderr: why.to_string(),
        }
    };

    let spawned = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(plugin_root)
        .env("CLAUDE_PLUGIN_ROOT", plugin_root)
        .env("PATH", path_var)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return failed(&format!("spawn failed: {e}")),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes()).await;
        // drop closes stdin → the hook sees EOF
    }

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return failed(&format!("wait failed: {e}")),
        Err(_) => return failed("timed out"),
    };
    let clamp =
        |s: std::borrow::Cow<'_, str>| -> String { s.chars().take(HOOK_OUTPUT_CLAMP).collect() };
    HookRun {
        code: output.status.code(),
        stdout: clamp(String::from_utf8_lossy(&output.stdout)),
        stderr: clamp(String::from_utf8_lossy(&output.stderr)),
    }
}

/// POST `payload` to `url` and read the response as a hook's reply.
///
/// Mapped onto the same [`HookRun`] a command hook produces, so everything
/// downstream — the reply processor, the record, the clamp — is shared. The
/// mapping loses exactly one thing: there is no exit-code channel over HTTP, so
/// an HTTP hook can only block through `decision` in its body. It is never
/// reported as exit 2.
pub(crate) async fn run_http_hook(
    url: &str,
    headers: &[(String, String)],
    payload: &str,
    timeout: Duration,
) -> HookRun {
    let failed = |why: String| {
        tracing::warn!(url, why, "plugin http hook did not run");
        HookRun {
            code: None,
            stdout: String::new(),
            stderr: why,
        }
    };

    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => return failed(format!("http client: {e}")),
    };
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .body(payload.to_string());
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => return failed(format!("request failed: {e}")),
    };
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let body: String = body.chars().take(HOOK_OUTPUT_CLAMP).collect();
    if status.is_success() {
        return HookRun {
            code: Some(0),
            stdout: body,
            stderr: String::new(),
        };
    }
    // A non-2xx is an outage, not a refusal: the hook had its chance to refuse
    // in the body, and a 500 means it never got that far. Exit 1 rather than
    // exit 2 for exactly that reason.
    HookRun {
        code: Some(1),
        stdout: String::new(),
        stderr: format!("the hook answered {status}"),
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
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn discovers_default_skills_dir() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            &root.join("sp/skills/brainstorming/SKILL.md"),
            "---\nname: brainstorming\ndescription: d\n---\nbody",
        );
        let skills = discover_skills(root);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].plugin, "sp");
        assert_eq!(skills[0].rel_dir, "sp/skills/brainstorming");
        assert!(skills[0].content.contains("body"));
    }

    #[test]
    fn manifest_name_and_skills_override() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            &root.join("p/.claude-plugin/plugin.json"),
            r#"{ "name": "fancy", "skills": "custom/skills" }"#,
        );
        write(
            &root.join("p/custom/skills/x/SKILL.md"),
            "---\nname: x\ndescription: d\n---\nb",
        );
        // a skill under the default location must be ignored when overridden
        write(
            &root.join("p/skills/ignored/SKILL.md"),
            "---\nname: ignored\ndescription: d\n---\nb",
        );
        let skills = discover_skills(root);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].plugin, "fancy");
        assert_eq!(skills[0].rel_dir, "p/custom/skills/x");
    }

    #[test]
    fn skills_array_override() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            &root.join("p/.claude-plugin/plugin.json"),
            r#"{ "skills": ["a/skills", "b/skills"] }"#,
        );
        write(
            &root.join("p/a/skills/one/SKILL.md"),
            "---\nname: one\ndescription: d\n---\nb",
        );
        write(
            &root.join("p/b/skills/two/SKILL.md"),
            "---\nname: two\ndescription: d\n---\nb",
        );
        let mut skills = discover_skills(root);
        skills.sort_by(|a, b| a.rel_dir.cmp(&b.rel_dir));
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].rel_dir, "p/a/skills/one");
        assert_eq!(skills[1].rel_dir, "p/b/skills/two");
    }

    /// The CLI installs plugins as symlinks into a shared clone. Discovery must
    /// follow the link but keep `rel_dir` relative to the library root — i.e.
    /// nothing in this path may canonicalise.
    #[test]
    #[cfg(unix)]
    fn discovers_skills_through_a_symlinked_plugin_dir() {
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("sources/abc/plugin");
        write(
            &real.join("skills/impeccable/SKILL.md"),
            "---\nname: impeccable\ndescription: d\n---\nbody",
        );
        let library = dir.path().join("plugins");
        fs::create_dir_all(&library).unwrap();
        std::os::unix::fs::symlink(&real, library.join("impeccable")).unwrap();

        let skills = discover_skills(&library);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].rel_dir, "impeccable/skills/impeccable");
    }

    #[test]
    fn empty_or_missing_dir_is_empty() {
        assert!(discover_skills(Path::new("/no/such/dir")).is_empty());
        let dir = TempDir::new().unwrap();
        assert!(discover_skills(dir.path()).is_empty());
    }
}
