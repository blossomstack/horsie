//! Shared plugin library: enumerate installed plugins under the `plugins_dir`
//! (the `horsie_shared` workspace), discover their skills, and run their
//! `SessionStart` hooks inside the sandbox.
//!
//! A plugin is a directory under `plugins_dir`. Its skills live under `skills/`
//! by default, or wherever its `.claude-plugin/plugin.json` `skills` field points
//! (string or array of paths). Hooks are declared in `hooks/hooks.json`.

use horsie_models::runtime::PluginSkill;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Max bytes of a single hook's captured context (mirrors the bash tool clamp).
const HOOK_OUTPUT_CLAMP: usize = 50_000;
/// Per-hook wall-clock budget.
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Plugin directories under `plugins_dir`, sorted for stable ordering. Best-effort:
/// an unreadable `plugins_dir` yields an empty list.
fn plugin_dirs(plugins_dir: &Path) -> Vec<PathBuf> {
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

/// Parse `<plugin>/.claude-plugin/plugin.json`, if present and valid.
fn read_manifest(plugin_root: &Path) -> Option<Value> {
    let path = plugin_root.join(".claude-plugin").join("plugin.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// The plugin's display name: manifest `name`, else the directory name.
fn plugin_name(plugin_root: &Path, manifest: Option<&Value>) -> String {
    manifest
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            plugin_root
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
}

/// Skill-directory roots for a plugin: manifest `skills` override (string or array),
/// else the default `skills/`.
fn skills_locations(plugin_root: &Path, manifest: Option<&Value>) -> Vec<PathBuf> {
    match manifest.and_then(|m| m.get("skills")) {
        Some(Value::String(s)) => vec![plugin_root.join(s)],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(|s| plugin_root.join(s))
            .collect(),
        _ => vec![plugin_root.join("skills")],
    }
}

/// Enumerate every installed plugin's skills. `rel_dir` is each skill's directory
/// relative to `plugins_dir` so the agent can read sibling resources via the
/// filesystem tools against `horsie_shared`.
pub fn discover_skills(plugins_dir: &Path) -> Vec<PluginSkill> {
    let mut out = Vec::new();
    for plugin_root in plugin_dirs(plugins_dir) {
        let manifest = read_manifest(&plugin_root);
        let name = plugin_name(&plugin_root, manifest.as_ref());
        for loc in skills_locations(&plugin_root, manifest.as_ref()) {
            let pattern = format!("{}/*/SKILL.md", loc.display());
            let Ok(paths) = glob::glob(&pattern) else {
                continue;
            };
            for skill_md in paths.flatten() {
                let Some(dir) = skill_md.parent() else {
                    continue;
                };
                let Ok(rel) = dir.strip_prefix(plugins_dir) else {
                    continue;
                };
                if let Ok(content) = std::fs::read_to_string(&skill_md) {
                    out.push(PluginSkill {
                        plugin: name.clone(),
                        rel_dir: rel.to_string_lossy().into_owned(),
                        content,
                    });
                }
            }
        }
    }
    out
}

/// Extract the `SessionStart` command strings from a plugin's `hooks/hooks.json`,
/// substituting `${CLAUDE_PLUGIN_ROOT}`.
fn session_start_commands(plugin_root: &Path) -> Vec<String> {
    let hooks_file = plugin_root.join("hooks").join("hooks.json");
    let Ok(text) = std::fs::read_to_string(hooks_file) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let root = plugin_root.to_string_lossy();
    json.get("hooks")
        .and_then(|h| h.get("SessionStart"))
        .and_then(Value::as_array)
        .map(|matchers| {
            matchers
                .iter()
                .filter_map(|m| m.get("hooks").and_then(Value::as_array))
                .flatten()
                .filter(|h| h.get("type").and_then(Value::as_str) == Some("command"))
                .filter_map(|h| h.get("command").and_then(Value::as_str))
                .map(|cmd| cmd.replace("${CLAUDE_PLUGIN_ROOT}", &root))
                .collect()
        })
        .unwrap_or_default()
}

/// Interpret a hook's stdout: the Claude-Code envelope
/// `{"hookSpecificOutput":{"additionalContext":"…"}}` if present, else raw stdout.
fn extract_context(stdout: &str) -> String {
    let ctx = serde_json::from_str::<Value>(stdout)
        .ok()
        .and_then(|v| {
            v.get("hookSpecificOutput")
                .and_then(|h| h.get("additionalContext"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| stdout.to_string());
    ctx.chars().take(HOOK_OUTPUT_CLAMP).collect()
}

/// Run one hook command via `sh -c` with the plugin dir as cwd, `CLAUDE_PLUGIN_ROOT`
/// set, and `hook_path` prepended to PATH. Returns its injected context, or `None`
/// on spawn/timeout/non-zero-exit (logged, non-fatal).
async fn run_hook(plugin_root: &Path, command: &str, hook_path: &[PathBuf]) -> Option<String> {
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

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(plugin_root)
        .env("CLAUDE_PLUGIN_ROOT", plugin_root)
        .env("PATH", path_var)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| tracing::warn!(error = %e, "plugin hook spawn failed"))
        .ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin
            .write_all(br#"{"hook_event_name":"SessionStart","source":"startup"}"#)
            .await;
        // drop closes stdin → the hook sees EOF
    }

    let output = match tokio::time::timeout(HOOK_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "plugin hook failed");
            return None;
        }
        Err(_) => {
            tracing::warn!("plugin hook timed out");
            return None;
        }
    };
    if !output.status.success() {
        tracing::warn!(status = ?output.status, "plugin hook exited non-zero");
        return None;
    }
    Some(extract_context(&String::from_utf8_lossy(&output.stdout)))
}

/// Run every installed plugin's `SessionStart` hooks (in stable plugin order) and
/// return their concatenated injected context. Empty when there are no hooks.
pub async fn run_session_start(plugins_dir: &Path, hook_path: &[PathBuf]) -> String {
    let mut sections = Vec::new();
    for plugin_root in plugin_dirs(plugins_dir) {
        for command in session_start_commands(&plugin_root) {
            if let Some(ctx) = run_hook(&plugin_root, &command, hook_path).await
                && !ctx.trim().is_empty()
            {
                sections.push(ctx);
            }
        }
    }
    sections.join("\n\n")
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

    #[test]
    fn empty_or_missing_dir_is_empty() {
        assert!(discover_skills(Path::new("/no/such/dir")).is_empty());
        let dir = TempDir::new().unwrap();
        assert!(discover_skills(dir.path()).is_empty());
    }

    #[test]
    fn extract_context_prefers_envelope() {
        let raw = r#"{"hookSpecificOutput":{"additionalContext":"hello"}}"#;
        assert_eq!(extract_context(raw), "hello");
        assert_eq!(extract_context("plain text"), "plain text");
    }

    #[test]
    fn session_start_commands_substitutes_root() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("p");
        write(
            &root.join("hooks/hooks.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"cat ${CLAUDE_PLUGIN_ROOT}/x"}]}]}}"#,
        );
        let cmds = session_start_commands(&root);
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].ends_with("/p/x"));
        assert!(!cmds[0].contains("CLAUDE_PLUGIN_ROOT"));
    }

    #[tokio::test]
    async fn runs_session_start_hook() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            &root.join("p/hooks/hooks.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo BOOTSTRAP"}]}]}}"#,
        );
        let ctx = run_session_start(root, &[]).await;
        assert_eq!(ctx.trim(), "BOOTSTRAP");
    }

    #[tokio::test]
    async fn failing_hook_is_skipped() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        write(
            &root.join("p/hooks/hooks.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"exit 1"}]}]}}"#,
        );
        assert!(run_session_start(root, &[]).await.is_empty());
    }
}
