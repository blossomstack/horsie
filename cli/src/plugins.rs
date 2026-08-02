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
    /// Names the shared clone under `sources/` this plugin is symlinked into.
    /// Absent for entries installed before the shared-clone layout.
    #[serde(default)]
    pub source_key: Option<String>,
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

/// Resolve the shared plugin library for a spawned runtime: the plugins root iff
/// it holds ≥1 plugin, plus the hook interpreter dirs — resolved only when there
/// is a library to run hooks for. Shared by the daemon and `horsie connect`.
pub fn library_for_runtime(
    plugins_dir: &Path,
    hook_path: Option<Vec<PathBuf>>,
) -> (Option<PathBuf>, Vec<PathBuf>) {
    let dir = plugins_dir_if_populated(plugins_dir);
    let hooks = if dir.is_some() {
        resolve_hook_path(hook_path)
    } else {
        Vec::new()
    };
    (dir, hooks)
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

/// The two directories the plugin library spans: the symlink farm the runtime
/// reads, and the clones those links point into.
#[derive(Debug, Clone)]
pub struct PluginPaths {
    /// `storage.plugins_dir` — one symlink per installed plugin.
    pub plugins: PathBuf,
    /// `<data_dir>/sources` — one clone per `(url, ref)`, shared by every
    /// plugin resolved out of it.
    pub sources: PathBuf,
    /// `<data_dir>/marketplaces` — the registry lockfile and nothing else; the
    /// clones themselves live in `sources`, like every other checkout.
    pub marketplaces: PathBuf,
}

/// `horsie plugin install <url>`: clone into the shared sources dir, resolve the
/// plugin root (honouring `marketplace.json`), and symlink it into the library.
pub fn install(
    paths: &PluginPaths,
    url: &str,
    name: Option<String>,
    git_ref: Option<String>,
    force: bool,
) -> Result<String, CliError> {
    std::fs::create_dir_all(&paths.plugins).map_err(|e| CliError::Io(e.to_string()))?;
    std::fs::create_dir_all(&paths.sources).map_err(|e| CliError::Io(e.to_string()))?;

    let key = horsie_support::plugin::source_key(url, git_ref.as_deref());
    let clone_dir = paths.sources.join(&key);
    if !clone_dir.exists() {
        horsie_support::git::clone(url, git_ref.as_deref(), &clone_dir).map_err(|e| {
            // Do not leave a half-clone behind for the next attempt to trip over.
            let _ = std::fs::remove_dir_all(&clone_dir);
            CliError::Executor(e)
        })?;
    }

    let (root_dir, entry_name) = resolve_plugin_root(&clone_dir, url)?;
    let root = horsie_support::plugin::PluginRoot::inspect(&root_dir).map_err(CliError::Config)?;
    if !root.is_installable() {
        gc_checkout(paths, &key);
        return Err(CliError::Config(format!(
            "'{url}' is not a skills plugin: {}",
            root.rejection()
        )));
    }

    let fallback = entry_name.unwrap_or_else(|| name_from_url(url));
    let install_name = name.unwrap_or_else(|| root.name(&fallback));
    let link = paths.plugins.join(&install_name);
    if link.symlink_metadata().is_ok() {
        if !force {
            return Err(CliError::Config(format!(
                "plugin '{install_name}' is already installed (use --force to reinstall)"
            )));
        }
        remove_link(&link)?;
    }
    symlink_dir(&root_dir, &link)?;

    let mut lock = load_lock(&paths.plugins);
    lock.plugins.retain(|p| p.name != install_name);
    lock.plugins.push(PluginEntry {
        name: install_name.clone(),
        source: url.to_string(),
        git_ref,
        version: root.version().map(str::to_string),
        // Resolves through the symlink into the real clone.
        sha: horsie_support::git::head_sha(&link),
        source_key: Some(key),
    });
    save_lock(&paths.plugins, &lock)?;
    Ok(install_name)
}

/// The plugin root inside a clone: the marketplace-declared entry when the repo
/// is a marketplace, else the repo root. Returns the entry name when a
/// marketplace named it.
fn resolve_plugin_root(clone_dir: &Path, url: &str) -> Result<(PathBuf, Option<String>), CliError> {
    let market = horsie_support::plugin::Marketplace::read(clone_dir).map_err(CliError::Config)?;
    let Some(market) = market else {
        return Ok((clone_dir.to_path_buf(), None));
    };
    for why in &market.skipped {
        tracing::warn!(marketplace = %url, "skipping unreadable marketplace {why}");
    }
    match market.plugins.as_slice() {
        [only] => match &only.source {
            horsie_support::plugin::PluginSource::Path(p) => Ok((
                horsie_support::plugin::join_declared(clone_dir, p),
                Some(only.name.clone()),
            )),
            // Resolving an external source needs the marketplace registry, which
            // lands with `horsie marketplace add` (issue #105, PR2).
            horsie_support::plugin::PluginSource::Git { url: u, .. } => {
                Err(CliError::Config(format!(
                    "'{}' is published from another repo ({u}); install it from there directly",
                    only.name
                )))
            }
        },
        [] => Ok((clone_dir.to_path_buf(), None)),
        many => Err(CliError::Config(format!(
            "'{url}' is a marketplace listing {} plugins; install one directly by its own repo URL. Available: {}",
            many.len(),
            market.names().join(", ")
        ))),
    }
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> Result<(), CliError> {
    std::os::unix::fs::symlink(target, link).map_err(|e| CliError::Io(e.to_string()))
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> Result<(), CliError> {
    std::os::windows::fs::symlink_dir(target, link).map_err(|e| CliError::Io(e.to_string()))
}

/// Remove an installed plugin's library entry, whether it is a symlink (current
/// layout) or a real directory (installed before this change).
fn remove_link(link: &Path) -> Result<(), CliError> {
    let Ok(meta) = link.symlink_metadata() else {
        return Ok(());
    };
    let r = if meta.file_type().is_symlink() {
        std::fs::remove_file(link)
    } else {
        std::fs::remove_dir_all(link)
    };
    r.map_err(|e| CliError::Io(e.to_string()))
}

/// Delete a checkout once neither an installed plugin nor a registered
/// marketplace references it.
///
/// `pub(crate)` because removing a marketplace releases its claim through here
/// too — the two registries share one `sources/` root.
pub(crate) fn gc_checkout(paths: &PluginPaths, key: &str) {
    let claimed_by_plugin = load_lock(&paths.plugins)
        .plugins
        .iter()
        .any(|p| p.source_key.as_deref() == Some(key));
    let claimed_by_marketplace = crate::marketplace::list(paths)
        .iter()
        .any(|m| m.source_key == key);
    if !claimed_by_plugin && !claimed_by_marketplace {
        let _ = std::fs::remove_dir_all(paths.sources.join(key));
    }
}

/// `horsie plugin list`: the installed plugins, from the lockfile.
pub fn list(paths: &PluginPaths) -> Vec<PluginEntry> {
    load_lock(&paths.plugins).plugins
}

/// `horsie plugin update <name>`: fast-forward the backing clone and refresh the
/// lockfile. Re-points the symlink in case the plugin's declared root moved.
pub fn update(paths: &PluginPaths, name: &str) -> Result<(), CliError> {
    let mut lock = load_lock(&paths.plugins);
    let entry = lock
        .plugins
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| CliError::Config(format!("plugin '{name}' is not installed")))?
        .clone();
    let key = entry.source_key.clone().ok_or_else(|| {
        CliError::Config(format!(
            "plugin '{name}' predates the shared-clone layout; reinstall it with --force"
        ))
    })?;
    let clone_dir = paths.sources.join(&key);
    horsie_support::git::pull_ff_only(&clone_dir).map_err(CliError::Executor)?;

    let (root_dir, _) = resolve_plugin_root(&clone_dir, &entry.source)?;
    let link = paths.plugins.join(name);
    remove_link(&link)?;
    symlink_dir(&root_dir, &link)?;

    let root = horsie_support::plugin::PluginRoot::inspect(&root_dir).map_err(CliError::Config)?;
    let version = root.version().map(str::to_string);
    let sha = horsie_support::git::head_sha(&link);
    if let Some(e) = lock.plugins.iter_mut().find(|p| p.name == name) {
        e.sha = sha;
        if version.is_some() {
            e.version = version;
        }
    }
    save_lock(&paths.plugins, &lock)
}

/// `horsie plugin remove <name>`: drop the library entry and the lockfile row,
/// then garbage-collect the backing clone if nothing else uses it.
pub fn remove(paths: &PluginPaths, name: &str) -> Result<(), CliError> {
    let mut lock = load_lock(&paths.plugins);
    let before = lock.plugins.len();
    let key = lock
        .plugins
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| p.source_key.clone());
    lock.plugins.retain(|p| p.name != name);
    let link = paths.plugins.join(name);
    let existed = link.symlink_metadata().is_ok();
    remove_link(&link)?;
    if lock.plugins.len() == before && !existed {
        return Err(CliError::Config(format!(
            "plugin '{name}' is not installed"
        )));
    }
    save_lock(&paths.plugins, &lock)?;
    if let Some(k) = key {
        gc_checkout(paths, &k);
    }
    Ok(())
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
    fn library_for_runtime_empty_dir_yields_nothing() {
        let dir = TempDir::new().unwrap();
        let (plugins, hooks) =
            library_for_runtime(dir.path(), Some(vec![PathBuf::from("/opt/node/bin")]));
        assert!(plugins.is_none());
        // No library → hook path not resolved, even with an override configured.
        assert!(hooks.is_empty());
    }

    #[test]
    fn library_for_runtime_populated_resolves_hooks() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sp")).unwrap();
        let (plugins, hooks) =
            library_for_runtime(dir.path(), Some(vec![PathBuf::from("/opt/node/bin")]));
        assert_eq!(plugins, Some(dir.path().to_path_buf()));
        assert_eq!(hooks, vec![PathBuf::from("/opt/node/bin")]);
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
                source_key: Some("deadbeefdeadbeef".into()),
            }],
        };
        save_lock(dir.path(), &lock).unwrap();
        let back = load_lock(dir.path());
        assert_eq!(back.plugins.len(), 1);
        assert_eq!(back.plugins[0].name, "sp");
        assert_eq!(back.plugins[0].git_ref.as_deref(), Some("main"));
    }

    fn git_run(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn commit_all(dir: &Path) {
        git_run(dir, &["init", "-q", "-b", "main"]);
        git_run(dir, &["config", "user.email", "t@example.com"]);
        git_run(dir, &["config", "user.name", "t"]);
        git_run(dir, &["add", "-A"]);
        git_run(dir, &["commit", "-qm", "init"]);
    }

    /// A repo whose plugin root is a subdirectory declared by a marketplace,
    /// with a manifest pointing skills outside the default location — the
    /// impeccable shape, which the old filesystem-only gate rejected.
    fn impeccable_fixture(dir: &Path) {
        let cp = dir.join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"impeccable","plugins":[{"name":"impeccable","source":"./plugin"}]}"#,
        )
        .unwrap();
        let pcp = dir.join("plugin/.claude-plugin");
        std::fs::create_dir_all(&pcp).unwrap();
        std::fs::write(
            pcp.join("plugin.json"),
            r#"{"name":"impeccable","version":"4.0.4","skills":"./skills/"}"#,
        )
        .unwrap();
        let s = dir.join("plugin/skills/impeccable");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("SKILL.md"), "---\nname: impeccable\n---\nbody").unwrap();
        commit_all(dir);
    }

    fn paths(root: &Path) -> PluginPaths {
        PluginPaths {
            plugins: root.join("plugins"),
            sources: root.join("sources"),
            marketplaces: root.join("marketplaces"),
        }
    }

    fn file_url(dir: &Path) -> String {
        format!("file://{}", dir.display())
    }

    #[test]
    #[cfg(unix)]
    fn installs_a_marketplace_subdir_plugin_as_a_symlink() {
        let src = TempDir::new().unwrap();
        impeccable_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());

        let name = install(&p, &file_url(src.path()), None, None, false).unwrap();
        assert_eq!(name, "impeccable");

        let link = p.plugins.join("impeccable");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(link.join("skills/impeccable/SKILL.md").is_file());

        let entry = list(&p).into_iter().next().unwrap();
        assert_eq!(entry.name, "impeccable");
        assert_eq!(entry.version.as_deref(), Some("4.0.4"));
        assert!(entry.source_key.is_some());
        assert!(entry.sha.is_some(), "sha resolves through the symlink");
    }

    #[test]
    #[cfg(unix)]
    fn update_pulls_and_remove_cleans_up_the_clone() {
        let src = TempDir::new().unwrap();
        impeccable_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        install(&p, &file_url(src.path()), None, None, false).unwrap();

        update(&p, "impeccable").unwrap();
        assert!(
            p.plugins
                .join("impeccable/skills/impeccable/SKILL.md")
                .is_file()
        );

        remove(&p, "impeccable").unwrap();
        assert!(p.plugins.join("impeccable").symlink_metadata().is_err());
        assert!(list(&p).is_empty());
        assert!(
            std::fs::read_dir(&p.sources)
                .map(|rd| rd.flatten().count() == 0)
                .unwrap_or(true),
            "orphaned clone left behind"
        );
    }

    #[test]
    fn rejects_a_repo_with_no_skills_and_says_where_it_looked() {
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("README.md"), "hi").unwrap();
        commit_all(src.path());

        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        let err = install(&p, &file_url(src.path()), None, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("SKILL.md"), "err: {err}");
        assert!(
            err.contains("skills"),
            "error must name where it looked: {err}"
        );
        // A rejected install leaves neither a link nor a clone behind.
        assert!(!p.plugins.join("src").exists());
        assert!(
            std::fs::read_dir(&p.sources)
                .map(|rd| rd.flatten().count() == 0)
                .unwrap_or(true)
        );
    }

    /// A marketplace listing several plugins is ambiguous; the error must name
    /// them rather than guessing.
    #[test]
    fn multi_plugin_marketplace_errors_with_the_available_names() {
        let src = TempDir::new().unwrap();
        let cp = src.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"plugins":[{"name":"alpha","source":"./a"},{"name":"beta","source":"./b"}]}"#,
        )
        .unwrap();
        commit_all(src.path());

        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        let err = install(&p, &file_url(src.path()), None, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("alpha"), "err: {err}");
        assert!(err.contains("beta"), "err: {err}");
    }

    #[test]
    #[cfg(unix)]
    fn duplicate_install_needs_force() {
        let src = TempDir::new().unwrap();
        impeccable_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        let url = file_url(src.path());
        install(&p, &url, None, None, false).unwrap();
        assert!(install(&p, &url, None, None, false).is_err());
        install(&p, &url, None, None, true).unwrap();
    }

    #[test]
    fn remove_missing_errors() {
        let dir = TempDir::new().unwrap();
        assert!(remove(&paths(dir.path()), "nope").is_err());
    }
}
