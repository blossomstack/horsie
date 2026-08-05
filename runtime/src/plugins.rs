//! Shared plugin library: enumerate installed plugins under the `plugins_dir`
//! (the `horsie_shared` workspace), discover their skills, and run their
//! `SessionStart` hooks inside the sandbox.
//!
//! A plugin is a directory under `plugins_dir`. Its skills live under `skills/`
//! and its agents under `agents/` by default, or wherever the matching
//! `.claude-plugin/plugin.json` field points (string or array of paths). Hooks
//! are declared in `hooks/hooks.json`.

use horsie_models::runtime::{PluginAgent, PluginSkill};
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

/// Enumerate every installed plugin's agent definitions.
///
/// `rel_path` is each file's path relative to `plugins_dir`, so it identifies
/// the definition without depending on where the library is mounted. The bytes
/// travel unparsed: reading frontmatter is the server's job, exactly as it is
/// for skills.
pub fn discover_agents(plugins_dir: &Path) -> Vec<PluginAgent> {
    let mut out = Vec::new();
    for plugin_root in plugin_dirs(plugins_dir) {
        // Best-effort, like `discover_skills`: one bad manifest contributes
        // nothing rather than blanking the library.
        let Ok(root) = horsie_support::plugin::PluginRoot::inspect(&plugin_root) else {
            continue;
        };
        let fallback = plugin_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = root.name(&fallback);
        for file in &root.agent_files {
            let Ok(rel) = file.strip_prefix(plugins_dir) else {
                continue;
            };
            if let Ok(content) = std::fs::read_to_string(file) {
                out.push(PluginAgent {
                    plugin: name.clone(),
                    rel_path: rel.to_string_lossy().into_owned(),
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

/// The client every HTTP hook shares.
///
/// One per process rather than one per invocation: a `PreToolUse` webhook runs
/// on every tool call, and building a client each time rebuilds the TLS config
/// and throws away the connection pool that would have made the second call
/// cheap. The per-hook budget rides the request instead of the client.
///
/// Redirects are refused. reqwest would otherwise follow up to ten, and a 302
/// turns the POST into a GET — the endpoint would receive no payload at all and
/// horsie would read whatever came back as the hook's reply. Refusing also keeps
/// a hook pointed where its declaration says it is pointed.
static HTTP_HOOK_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn http_hook_client() -> Option<&'static reqwest::Client> {
    if let Some(client) = HTTP_HOOK_CLIENT.get() {
        return Some(client);
    }
    let built = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    Some(HTTP_HOOK_CLIENT.get_or_init(|| built))
}

/// POST `payload` to `url` and read the response as a hook's reply.
///
/// Mapped onto the same [`HookRun`] a command hook produces, so everything
/// downstream — the reply processor, the record, the clamp — is shared. The
/// mapping loses exactly one thing: there is no exit-code channel over HTTP, so
/// an HTTP hook can only block through `decision` in its body. It is never
/// reported as exit 2.
///
/// A transport failure is an *outage* in horsie's vocabulary, which is not the
/// same as harmless: `PreToolUse` fails closed, so an unreachable endpoint denies
/// the calls its hook guards. That is deliberate and it is what a command hook
/// that cannot be spawned already does — but the spec has HTTP failures continue,
/// so the guide says which one horsie is.
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

    let Some(client) = http_hook_client() else {
        return failed("http client could not be built".to_string());
    };
    let mut request = client
        .post(url)
        .timeout(timeout)
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
    let body = read_capped(response).await;
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

/// The response body, stopping at the clamp instead of buffering the whole of
/// it and truncating after.
///
/// A command hook's output is bounded by a process horsie started; an HTTP
/// hook's is chosen by whatever is at the other end of the URL, so the cap has
/// to hold before the bytes are in memory rather than after.
async fn read_capped(mut response: reqwest::Response) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= HOOK_OUTPUT_CLAMP {
            tracing::warn!("a plugin http hook's response hit the output clamp");
            break;
        }
    }
    String::from_utf8_lossy(&bytes)
        .chars()
        .take(HOOK_OUTPUT_CLAMP)
        .collect()
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
