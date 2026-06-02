//! Shared plugin library management: `horsie plugin install/list/update/remove`,
//! plus helpers the daemon uses to expose the library to jobs.
//!
//! Plugins live under `storage.plugins_dir` (default `<data_dir>/plugins`), one
//! directory per plugin, cloned from git. A `plugins.json` lockfile records what is
//! installed for the `list` view and for `update`.

use crate::error::CliError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// One installed plugin, recorded in the lockfile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    pub name: String,
    pub source: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub version: Option<String>,
    pub sha: Option<String>,
}

/// The `plugins.json` lockfile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginLock {
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
}

fn lockfile_path(plugins_dir: &Path) -> PathBuf {
    plugins_dir.join("plugins.json")
}

fn load_lock(plugins_dir: &Path) -> PluginLock {
    std::fs::read_to_string(lockfile_path(plugins_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_lock(plugins_dir: &Path, lock: &PluginLock) -> Result<(), CliError> {
    let json = serde_json::to_vec_pretty(lock).map_err(|e| CliError::Config(e.to_string()))?;
    std::fs::write(lockfile_path(plugins_dir), json).map_err(|e| CliError::Io(e.to_string()))
}

/// The plugin directories under `plugins_dir` (excludes the lockfile).
pub fn count_installed(plugins_dir: &Path) -> usize {
    std::fs::read_dir(plugins_dir)
        .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).count())
        .unwrap_or(0)
}

/// The plugins root iff it exists and holds at least one plugin — otherwise `None`,
/// so the whole shared-library feature stays inert.
pub fn plugins_dir_if_populated(dir: &Path) -> Option<PathBuf> {
    (dir.is_dir() && count_installed(dir) > 0).then(|| dir.to_path_buf())
}

/// Resolve the hook interpreter dirs: the configured override, else auto-discover
/// `node` from the ambient environment (its parent dir). Empty when neither resolves.
pub fn resolve_hook_path(configured: Option<Vec<PathBuf>>) -> Vec<PathBuf> {
    if let Some(paths) = configured {
        return paths;
    }
    which_dir("node").into_iter().collect()
}

/// The directory containing `bin` on the current `PATH`, via `command -v`.
fn which_dir(bin: &str) -> Option<PathBuf> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return None;
    }
    PathBuf::from(path).parent().map(Path::to_path_buf)
}

/// Derive a plugin name from a git URL: the last path segment, minus `.git`.
fn name_from_url(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("plugin")
        .trim_end_matches(".git")
        .to_string()
}

/// Read `.claude-plugin/plugin.json` `name`/`version`, if present.
fn read_manifest_meta(plugin_dir: &Path) -> (Option<String>, Option<String>) {
    let path = plugin_dir.join(".claude-plugin").join("plugin.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (None, None);
    };
    let s = |k: &str| {
        json.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    (s("name"), s("version"))
}

/// `true` if the plugin dir exposes at least one `SKILL.md` (best-effort, default and
/// common custom locations).
fn has_skills(plugin_dir: &Path) -> bool {
    for loc in ["skills", "."] {
        let base = plugin_dir.join(loc);
        if let Ok(rd) = std::fs::read_dir(&base) {
            for entry in rd.flatten() {
                if entry.path().join("SKILL.md").is_file() {
                    return true;
                }
            }
        }
    }
    plugin_dir.join("SKILL.md").is_file()
}

fn git(args: &[&str]) -> Result<std::process::Output, CliError> {
    Command::new("git")
        .args(args)
        .output()
        .map_err(|e| CliError::Executor(format!("git: {e}")))
}

fn current_sha(plugin_dir: &Path) -> Option<String> {
    let out = git(&["-C", &plugin_dir.to_string_lossy(), "rev-parse", "HEAD"]).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `horsie plugin install <url>`: clone into `<plugins_dir>/<name>` and record it.
pub fn install(
    plugins_dir: &Path,
    url: &str,
    name: Option<String>,
    git_ref: Option<String>,
    force: bool,
) -> Result<String, CliError> {
    std::fs::create_dir_all(plugins_dir).map_err(|e| CliError::Io(e.to_string()))?;
    let name = name.unwrap_or_else(|| name_from_url(url));
    let target = plugins_dir.join(&name);
    if target.exists() {
        if !force {
            return Err(CliError::Config(format!(
                "plugin '{name}' is already installed (use --force to reinstall)"
            )));
        }
        std::fs::remove_dir_all(&target).map_err(|e| CliError::Io(e.to_string()))?;
    }

    let target_str = target.to_string_lossy().into_owned();
    let mut args = vec!["clone", "--depth", "1"];
    if let Some(r) = &git_ref {
        args.push("--branch");
        args.push(r);
    }
    args.push(url);
    args.push(&target_str);
    let out = git(&args)?;
    if !out.status.success() {
        return Err(CliError::Executor(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    if !has_skills(&target) {
        let _ = std::fs::remove_dir_all(&target);
        return Err(CliError::Config(format!(
            "'{url}' does not expose any SKILL.md; not a skills plugin"
        )));
    }

    let (manifest_name, version) = read_manifest_meta(&target);
    let mut lock = load_lock(plugins_dir);
    lock.plugins.retain(|p| p.name != name);
    lock.plugins.push(PluginEntry {
        name: manifest_name.unwrap_or_else(|| name.clone()),
        source: url.to_string(),
        git_ref,
        version,
        sha: current_sha(&target),
    });
    save_lock(plugins_dir, &lock)?;
    Ok(name)
}

/// `horsie plugin list`: the installed plugins, from the lockfile.
pub fn list(plugins_dir: &Path) -> Vec<PluginEntry> {
    load_lock(plugins_dir).plugins
}

/// `horsie plugin update <name>`: `git pull` (or re-checkout the recorded ref) and
/// refresh the lockfile's sha.
pub fn update(plugins_dir: &Path, name: &str) -> Result<(), CliError> {
    let target = plugins_dir.join(name);
    if !target.is_dir() {
        return Err(CliError::Config(format!(
            "plugin '{name}' is not installed"
        )));
    }
    let target_str = target.to_string_lossy().into_owned();
    let out = git(&["-C", &target_str, "pull", "--ff-only"])?;
    if !out.status.success() {
        return Err(CliError::Executor(format!(
            "git pull failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let (_, version) = read_manifest_meta(&target);
    let mut lock = load_lock(plugins_dir);
    if let Some(entry) = lock.plugins.iter_mut().find(|p| p.name == name) {
        entry.sha = current_sha(&target);
        if version.is_some() {
            entry.version = version;
        }
    }
    save_lock(plugins_dir, &lock)
}

/// `horsie plugin remove <name>`: delete the dir and drop the lockfile entry.
pub fn remove(plugins_dir: &Path, name: &str) -> Result<(), CliError> {
    let target = plugins_dir.join(name);
    if target.is_dir() {
        std::fs::remove_dir_all(&target).map_err(|e| CliError::Io(e.to_string()))?;
    }
    let mut lock = load_lock(plugins_dir);
    let before = lock.plugins.len();
    lock.plugins.retain(|p| p.name != name);
    if lock.plugins.len() == before && !target.exists() {
        return Err(CliError::Config(format!(
            "plugin '{name}' is not installed"
        )));
    }
    save_lock(plugins_dir, &lock)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn name_from_url_strips_git_suffix() {
        assert_eq!(
            name_from_url("https://github.com/obra/superpowers"),
            "superpowers"
        );
        assert_eq!(
            name_from_url("https://github.com/obra/superpowers.git"),
            "superpowers"
        );
        assert_eq!(name_from_url("git@github.com:x/y.git"), "y");
    }

    #[test]
    fn resolve_hook_path_prefers_override() {
        let p = resolve_hook_path(Some(vec![PathBuf::from("/opt/node/bin")]));
        assert_eq!(p, vec![PathBuf::from("/opt/node/bin")]);
        // Empty override stays empty (does not fall back to discovery).
        assert!(resolve_hook_path(Some(vec![])).is_empty());
    }

    #[test]
    fn populated_only_when_has_plugin_dir() {
        let dir = TempDir::new().unwrap();
        assert!(plugins_dir_if_populated(dir.path()).is_none());
        std::fs::create_dir(dir.path().join("sp")).unwrap();
        assert_eq!(
            plugins_dir_if_populated(dir.path()),
            Some(dir.path().to_path_buf())
        );
        assert_eq!(count_installed(dir.path()), 1);
    }

    #[test]
    fn lockfile_round_trips() {
        let dir = TempDir::new().unwrap();
        let lock = PluginLock {
            plugins: vec![PluginEntry {
                name: "sp".into(),
                source: "https://example/sp".into(),
                git_ref: Some("main".into()),
                version: Some("5.1.0".into()),
                sha: Some("abc".into()),
            }],
        };
        save_lock(dir.path(), &lock).unwrap();
        let back = load_lock(dir.path());
        assert_eq!(back.plugins.len(), 1);
        assert_eq!(back.plugins[0].name, "sp");
        assert_eq!(back.plugins[0].git_ref.as_deref(), Some("main"));
    }

    #[test]
    fn has_skills_detects_default_layout() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("p");
        std::fs::create_dir_all(p.join("skills/x")).unwrap();
        assert!(!has_skills(&p));
        std::fs::write(p.join("skills/x/SKILL.md"), "x").unwrap();
        assert!(has_skills(&p));
    }

    #[test]
    fn remove_missing_errors() {
        let dir = TempDir::new().unwrap();
        assert!(remove(dir.path(), "nope").is_err());
    }
}
