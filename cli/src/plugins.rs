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
    /// Where inside the checkout the plugin root sits, when it is not the repo
    /// root. Recorded so `update` re-points the symlink at the same subtree
    /// instead of falling back to the repo root.
    #[serde(default)]
    pub subpath: Option<String>,
    /// The marketplace this plugin was installed from, if any. `update`
    /// re-resolves through it, so a plugin that the index has since moved or
    /// re-pinned follows along.
    #[serde(default)]
    pub marketplace: Option<String>,
    /// The name the index knows this plugin by, which is not always the name it
    /// installs as — a plugin's own manifest may disagree with its catalogue
    /// entry (`42crunch-api-security-testing` installs as
    /// `api-security-testing`). Re-resolution must use the index's name.
    #[serde(default)]
    pub marketplace_entry: Option<String>,
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

/// What `horsie plugin install <arg>` was pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallTarget {
    Url(String),
    FromMarketplace { plugin: String, marketplace: String },
}

impl InstallTarget {
    /// `<plugin>@<marketplace>` only when both sides match the name shape the
    /// Agent Skills spec guarantees — lowercase alphanumerics and hyphens.
    ///
    /// This is what keeps SSH URLs safe: `git@github.com:x/y.git` has a `.`,
    /// `:` and `/` on its right-hand side, so it can never be mistaken for a
    /// marketplace reference.
    pub fn parse(arg: &str) -> InstallTarget {
        let is_name = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        };
        match arg.split_once('@') {
            Some((plugin, market)) if is_name(plugin) && is_name(market) => {
                InstallTarget::FromMarketplace {
                    plugin: plugin.to_string(),
                    marketplace: market.to_string(),
                }
            }
            _ => InstallTarget::Url(arg.to_string()),
        }
    }
}

/// Where a plugin's tree comes from, once the target has been resolved.
struct Resolved {
    url: String,
    git_ref: Option<String>,
    subpath: Option<String>,
    /// Name to fall back on when the plugin's own manifest declares none.
    fallback: String,
    /// What the user asked for, for error messages: the plugin name when it came
    /// from a marketplace, else the URL they typed.
    label: String,
    /// The index's own name for this entry, recorded so `update` can re-resolve.
    entry_name: String,
}

/// `horsie plugin install <url|plugin@marketplace>`: resolve where the plugin
/// lives, check it out under the shared sources dir, and symlink its root into
/// the library.
/// One sentence per hook event this plugin declares that horsie cannot fire.
///
/// Reported rather than refused, because a plugin's skills are worth installing
/// even when one of its hooks is not yet supported — but installing to silence
/// is exactly what the event classification exists to prevent, so the user is
/// told. An unreadable `hooks.json` yields nothing here; `read` already fails
/// the install for that.
#[must_use]
pub fn hook_report(plugin_root: &std::path::Path) -> Vec<String> {
    horsie_support::plugin::hooks::read(plugin_root)
        .map(|h| {
            h.unsupported
                .iter()
                .map(|(name, why)| why.explain(name))
                .collect()
        })
        .unwrap_or_default()
}

/// Every installed plugin's unrunnable hook events, keyed by plugin name.
///
/// The re-check a session start wants: a plugin installed against a build that
/// supported its hooks, then carried onto one that does not, would otherwise go
/// quiet with nothing said.
#[must_use]
pub fn unsupported_hooks(paths: &PluginPaths) -> Vec<(String, Vec<String>)> {
    let Ok(entries) = std::fs::read_dir(&paths.plugins) else {
        return Vec::new();
    };
    let mut out: Vec<(String, Vec<String>)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let reasons = hook_report(&e.path());
            (!reasons.is_empty()).then(|| (e.file_name().to_string_lossy().into_owned(), reasons))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn install(
    paths: &PluginPaths,
    target: &InstallTarget,
    name: Option<String>,
    git_ref: Option<String>,
    force: bool,
) -> Result<String, CliError> {
    std::fs::create_dir_all(&paths.plugins).map_err(|e| CliError::Io(e.to_string()))?;
    std::fs::create_dir_all(&paths.sources).map_err(|e| CliError::Io(e.to_string()))?;

    let resolved = resolve_target(paths, target, git_ref)?;
    let checkout = horsie_support::plugin::ensure_checkout(
        &paths.sources,
        &resolved.url,
        resolved.git_ref.as_deref(),
    )
    .map_err(CliError::Executor)?;

    let root_dir = match &resolved.subpath {
        Some(p) => horsie_support::plugin::join_declared(&checkout.dir, p),
        None => checkout.dir.clone(),
    };
    let root = horsie_support::plugin::PluginRoot::inspect(&root_dir).map_err(CliError::Config)?;
    if !root.is_installable() {
        gc_checkout(paths, &checkout.key);
        // Common enough to be worth spelling out: a large share of published
        // plugins ship only agents, commands or MCP servers, none of which
        // horsie loads yet. Naming the subtree that was checked keeps this
        // distinguishable from a resolution failure.
        let checked = resolved
            .subpath
            .as_deref()
            .map(|p| format!(" in {p}"))
            .unwrap_or_default();
        // A plugin whose only content is hooks horsie cannot fire deserves to
        // be told *which* ones, rather than the generic "no skills" — that is
        // the difference between "wrong plugin" and "wrong version of horsie".
        let hooks = hook_report(&root_dir);
        let because = if hooks.is_empty() {
            String::new()
        } else {
            format!(" Its hooks cannot run either: {}.", hooks.join("; "))
        };
        return Err(CliError::Config(format!(
            "'{}'{checked} provides no skills to install: {}. \
             horsie loads plugin skills; plugin agents, commands and MCP servers are not supported yet.{because}",
            resolved.label,
            root.rejection()
        )));
    }

    let install_name = name.unwrap_or_else(|| root.name(&resolved.fallback));
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
        source: resolved.url,
        git_ref: resolved.git_ref,
        version: root.version().map(str::to_string),
        // Resolves through the symlink into the real checkout.
        sha: horsie_support::git::head_sha(&link),
        source_key: Some(checkout.key),
        subpath: resolved.subpath,
        marketplace: match target {
            InstallTarget::FromMarketplace { marketplace, .. } => Some(marketplace.clone()),
            InstallTarget::Url(_) => None,
        },
        marketplace_entry: match target {
            InstallTarget::FromMarketplace { .. } => Some(resolved.entry_name),
            InstallTarget::Url(_) => None,
        },
    });
    save_lock(&paths.plugins, &lock)?;
    Ok(install_name)
}

/// Turn an install target into a concrete `(url, ref, subpath)`.
fn resolve_target(
    paths: &PluginPaths,
    target: &InstallTarget,
    git_ref: Option<String>,
) -> Result<Resolved, CliError> {
    match target {
        InstallTarget::FromMarketplace {
            plugin,
            marketplace,
        } => {
            let (url, entry_ref, subpath, entry_name) =
                crate::marketplace::resolve_entry(paths, marketplace, plugin)?;
            Ok(Resolved {
                url,
                // An explicit `--ref` overrides what the index pinned.
                git_ref: git_ref.or(entry_ref),
                subpath,
                label: entry_name.clone(),
                fallback: entry_name.clone(),
                entry_name,
            })
        }
        InstallTarget::Url(url) => {
            // A plain repo URL may still point at a marketplace, in which case
            // the index says where the plugin actually is.
            let checkout =
                horsie_support::plugin::ensure_checkout(&paths.sources, url, git_ref.as_deref())
                    .map_err(CliError::Executor)?;
            let market = horsie_support::plugin::Marketplace::read(&checkout.dir)
                .map_err(CliError::Config)?;
            let Some(market) = market else {
                return Ok(Resolved {
                    url: url.clone(),
                    git_ref,
                    subpath: None,
                    fallback: name_from_url(url),
                    label: url.clone(),
                    entry_name: String::new(),
                });
            };
            for why in &market.skipped {
                tracing::warn!(marketplace = %url, "skipping unreadable entry: {why}");
            }
            match market.plugins.as_slice() {
                [only] => {
                    let (entry_url, entry_ref, subpath) = horsie_support::plugin::source_location(
                        &only.source,
                        url,
                        git_ref.as_deref(),
                    );
                    Ok(Resolved {
                        url: entry_url,
                        git_ref: entry_ref,
                        subpath,
                        fallback: only.name.clone(),
                        label: only.name.clone(),
                        entry_name: only.name.clone(),
                    })
                }
                [] => Ok(Resolved {
                    url: url.clone(),
                    git_ref,
                    subpath: None,
                    fallback: name_from_url(url),
                    label: url.clone(),
                    entry_name: String::new(),
                }),
                many => Err(CliError::Config(format!(
                    "'{url}' is a marketplace listing {} plugins. Add it with `horsie marketplace add {url}`, then install one by name. Available: {}",
                    many.len(),
                    market.names().join(", ")
                ))),
            }
        }
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

/// `horsie plugin update <name>`: fast-forward the backing checkout and refresh
/// the lockfile. Re-points the symlink in case the plugin's root moved.
///
/// A plugin installed from a marketplace is re-resolved through that index
/// first, so an entry the marketplace has since moved or re-pinned follows
/// along rather than silently staying on the old location.
pub fn update(paths: &PluginPaths, name: &str) -> Result<(), CliError> {
    let mut lock = load_lock(&paths.plugins);
    let entry = lock
        .plugins
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| CliError::Config(format!("plugin '{name}' is not installed")))?
        .clone();
    let old_key = entry.source_key.clone().ok_or_else(|| {
        CliError::Config(format!(
            "plugin '{name}' predates the shared-clone layout; reinstall it with --force"
        ))
    })?;

    let (url, git_ref, subpath) = match (&entry.marketplace, &entry.marketplace_entry) {
        // Re-resolve by the *index's* name for this plugin, not the name it
        // installed as; the two differ whenever a plugin's manifest disagrees
        // with its catalogue entry.
        (Some(m), Some(index_name)) => {
            let (url, r, sub, _) = crate::marketplace::resolve_entry(paths, m, index_name)?;
            (url, r, sub)
        }
        _ => (
            entry.source.clone(),
            entry.git_ref.clone(),
            entry.subpath.clone(),
        ),
    };

    let checkout =
        horsie_support::plugin::ensure_checkout(&paths.sources, &url, git_ref.as_deref())
            .map_err(CliError::Executor)?;
    horsie_support::git::pull_ff_only(&checkout.dir).map_err(CliError::Executor)?;

    let root_dir = match &subpath {
        Some(p) => horsie_support::plugin::join_declared(&checkout.dir, p),
        None => checkout.dir.clone(),
    };
    let root = horsie_support::plugin::PluginRoot::inspect(&root_dir).map_err(CliError::Config)?;
    let link = paths.plugins.join(name);
    remove_link(&link)?;
    symlink_dir(&root_dir, &link)?;

    let version = root.version().map(str::to_string);
    let sha = horsie_support::git::head_sha(&link);
    if let Some(e) = lock.plugins.iter_mut().find(|p| p.name == name) {
        e.source = url;
        e.git_ref = git_ref;
        e.subpath = subpath;
        e.source_key = Some(checkout.key.clone());
        e.sha = sha;
        if version.is_some() {
            e.version = version;
        }
    }
    save_lock(&paths.plugins, &lock)?;
    // The index may have moved the plugin to a different repo entirely.
    if checkout.key != old_key {
        gc_checkout(paths, &old_key);
    }
    Ok(())
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
                subpath: None,
                marketplace: None,
                marketplace_entry: None,
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

    /// Initialise `dir` as a repo (if needed) and commit everything in it.
    fn commit_all(dir: &Path) {
        if !dir.join(".git").exists() {
            git_run(dir, &["init", "-q", "-b", "main"]);
            git_run(dir, &["config", "user.email", "t@example.com"]);
            git_run(dir, &["config", "user.name", "t"]);
        }
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

        let name = install(
            &p,
            &InstallTarget::Url(file_url(src.path())),
            None,
            None,
            false,
        )
        .unwrap();
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
        install(
            &p,
            &InstallTarget::Url(file_url(src.path())),
            None,
            None,
            false,
        )
        .unwrap();

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
        let err = install(
            &p,
            &InstallTarget::Url(file_url(src.path())),
            None,
            None,
            false,
        )
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
        let err = install(
            &p,
            &InstallTarget::Url(file_url(src.path())),
            None,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("alpha"), "err: {err}");
        assert!(err.contains("beta"), "err: {err}");
    }

    /// SSH URLs contain `@`, so the marketplace form is recognised only by the
    /// strict shape the Agent Skills spec guarantees for names: lowercase
    /// alphanumerics and hyphens on both sides.
    #[test]
    fn install_target_parsing_never_mistakes_a_url_for_a_marketplace_ref() {
        assert_eq!(
            InstallTarget::parse("impeccable@official"),
            InstallTarget::FromMarketplace {
                plugin: "impeccable".into(),
                marketplace: "official".into(),
            }
        );
        for url in [
            "git@github.com:x/y.git",
            "https://github.com/o/r",
            "ssh://git@host/x.git",
            "file:///tmp/x",
            "https://user@host/x.git",
            "Impeccable@Official",
            "a@",
            "@b",
        ] {
            assert!(
                matches!(InstallTarget::parse(url), InstallTarget::Url(_)),
                "{url} must parse as a URL"
            );
        }
    }

    /// A marketplace repo indexing two plugins that live inside it.
    fn market_fixture(dir: &Path) {
        let cp = dir.join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        )
        .unwrap();
        for n in ["alpha", "beta"] {
            let d = dir.join(format!("plugins/{n}/skills/{n}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SKILL.md"), format!("---\nname: {n}\n---\nbody")).unwrap();
        }
        commit_all(dir);
    }

    #[test]
    #[cfg(unix)]
    fn installs_a_named_plugin_out_of_a_registered_marketplace() {
        let src = TempDir::new().unwrap();
        market_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        crate::marketplace::add(&p, &file_url(src.path()), None, None, false).unwrap();

        let name = install(&p, &InstallTarget::parse("beta@acme"), None, None, false).unwrap();
        assert_eq!(name, "beta");
        assert!(p.plugins.join("beta/skills/beta/SKILL.md").is_file());

        let entry = list(&p).into_iter().next().unwrap();
        assert_eq!(entry.marketplace.as_deref(), Some("acme"));
        assert_eq!(entry.subpath.as_deref(), Some("./plugins/beta"));
        // The marketplace was already checked out; installing from it must not
        // clone the same repo a second time.
        assert_eq!(std::fs::read_dir(&p.sources).unwrap().count(), 1);
    }

    /// The case PR1 could not reach: a marketplace entry whose `source` points
    /// at a different repository, with a subdirectory.
    #[test]
    #[cfg(unix)]
    fn installs_an_external_source_from_a_marketplace() {
        let plugin_repo = TempDir::new().unwrap();
        let d = plugin_repo.path().join("packages/gamma/skills/gamma");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), "---\nname: gamma\n---\nbody").unwrap();
        commit_all(plugin_repo.path());

        let market = TempDir::new().unwrap();
        let cp = market.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            format!(
                r#"{{"name":"ext","plugins":[{{"name":"gamma","source":{{"source":"git-subdir","url":"{}","path":"packages/gamma"}}}}]}}"#,
                file_url(plugin_repo.path())
            ),
        )
        .unwrap();
        commit_all(market.path());

        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        crate::marketplace::add(&p, &file_url(market.path()), None, None, false).unwrap();

        let name = install(&p, &InstallTarget::parse("gamma@ext"), None, None, false).unwrap();
        assert_eq!(name, "gamma");
        assert!(p.plugins.join("gamma/skills/gamma/SKILL.md").is_file());
        // Two distinct repos → two checkouts.
        assert_eq!(std::fs::read_dir(&p.sources).unwrap().count(), 2);

        // Removing the marketplace must not disturb the installed plugin.
        crate::marketplace::remove(&p, "ext").unwrap();
        assert!(p.plugins.join("gamma/skills/gamma/SKILL.md").is_file());
    }

    /// `update` must re-point at the same subtree, not at the repo root.
    #[test]
    #[cfg(unix)]
    fn update_keeps_a_subpath_plugin_pointed_at_its_subtree() {
        let src = TempDir::new().unwrap();
        market_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        crate::marketplace::add(&p, &file_url(src.path()), None, None, false).unwrap();
        install(&p, &InstallTarget::parse("alpha@acme"), None, None, false).unwrap();

        update(&p, "alpha").unwrap();
        assert!(
            p.plugins.join("alpha/skills/alpha/SKILL.md").is_file(),
            "the symlink must still point at plugins/alpha, not the repo root"
        );
    }

    /// Most published plugins ship only agents/commands/MCP, which horsie does
    /// not load yet. The error must name the plugin and say so, rather than
    /// reading like a failure to find the repo.
    #[test]
    fn a_marketplace_plugin_with_no_skills_says_what_is_missing() {
        let src = TempDir::new().unwrap();
        let cp = src.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[
                 {"name":"cmdsonly","source":"./plugins/cmdsonly"},
                 {"name":"other","source":"./plugins/other"}]}"#,
        )
        .unwrap();
        // Ships commands, no skills — the agent-sdk-dev shape.
        let d = src.path().join("plugins/cmdsonly/commands");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("go.md"), "---\ndescription: go\n---\n").unwrap();
        let o = src.path().join("plugins/other/skills/other");
        std::fs::create_dir_all(&o).unwrap();
        std::fs::write(o.join("SKILL.md"), "---\nname: other\n---\nb").unwrap();
        commit_all(src.path());

        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        crate::marketplace::add(&p, &file_url(src.path()), None, None, false).unwrap();

        let err = install(
            &p,
            &InstallTarget::parse("cmdsonly@acme"),
            None,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cmdsonly"), "must name the plugin: {err}");
        assert!(
            err.contains("./plugins/cmdsonly"),
            "must name the subtree it checked: {err}"
        );
        assert!(
            err.contains("not supported yet"),
            "must explain that agents/commands are unsupported: {err}"
        );
        // The failure must not strand the marketplace's own checkout.
        assert!(!crate::marketplace::list(&p).is_empty());
        assert!(
            install(&p, &InstallTarget::parse("other@acme"), None, None, false).is_ok(),
            "a sibling plugin in the same marketplace must still install"
        );
    }

    /// A marketplace with one plugin, whose `hooks.json` is `hooks_json` and
    /// which ships a skill so it is installable at all.
    fn marketplace_with_hooks(name: &str, hooks_json: &str) -> (PluginPaths, TempDir, TempDir) {
        let src = TempDir::new().unwrap();
        let cp = src.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            format!(
                r#"{{"name":"acme","plugins":[{{"name":"{name}","source":"./plugins/{name}"}}]}}"#
            ),
        )
        .unwrap();
        let root = src.path().join(format!("plugins/{name}"));
        std::fs::create_dir_all(root.join("skills/s")).unwrap();
        std::fs::write(root.join("skills/s/SKILL.md"), "---\nname: s\n---\nb").unwrap();
        std::fs::create_dir_all(root.join("hooks")).unwrap();
        std::fs::write(root.join("hooks/hooks.json"), hooks_json).unwrap();
        commit_all(src.path());

        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        crate::marketplace::add(&p, &file_url(src.path()), None, None, false).unwrap();
        (p, home, src)
    }

    /// `Stop` is the most-declared event across the official marketplace and is
    /// now wired, so it must stop being refused. This is what fails if the
    /// continuation work regresses.
    #[test]
    fn stop_is_no_longer_reported_as_unrunnable() {
        let (p, _home, _src) = marketplace_with_hooks(
            "stopper",
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"node hook.mjs"}]}]}}"#,
        );
        let installed =
            install(&p, &InstallTarget::parse("stopper@acme"), None, None, false).unwrap();
        assert!(
            hook_report(&p.plugins.join(&installed)).is_empty(),
            "Stop is wired: {:?}",
            hook_report(&p.plugins.join(&installed))
        );
    }

    /// A plugin's skills are worth installing even when one of its hooks cannot
    /// fire — but the user is told, rather than left with a guard that silently
    /// never runs.
    #[test]
    fn a_partly_supported_plugin_installs_and_names_the_hook_it_cannot_run() {
        let (p, _home, _src) = marketplace_with_hooks(
            "mixed",
            r#"{"hooks":{
                 "PostToolUse":[{"hooks":[{"type":"command","command":"node hook.mjs"}]}],
                 "CwdChanged":[{"hooks":[{"type":"command","command":"node hook.mjs"}]}]}}"#,
        );
        let installed =
            install(&p, &InstallTarget::parse("mixed@acme"), None, None, false).unwrap();
        let reasons = hook_report(&p.plugins.join(&installed));
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("CwdChanged"), "{reasons:?}");
        // Described and seam-able, just unwired — distinct from an event horsie
        // has no concept of, and the two read differently on purpose.
        assert!(reasons[0].contains("not implemented"), "{reasons:?}");
    }

    /// The re-check: a plugin installed against a build that ran its hooks, then
    /// carried onto one that does not, must not go quiet.
    #[test]
    fn unsupported_hooks_reports_every_installed_plugin() {
        let (p, _home, _src) = marketplace_with_hooks(
            "batcher",
            r#"{"hooks":{"PostToolBatch":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        );
        assert!(unsupported_hooks(&p).is_empty(), "nothing installed yet");
        install(&p, &InstallTarget::parse("batcher@acme"), None, None, false).unwrap();
        let found = unsupported_hooks(&p);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "batcher");
        assert!(found[0].1[0].contains("PostToolBatch"), "{:?}", found[0].1);
        assert!(
            found[0].1[0].contains("not implemented"),
            "{:?}",
            found[0].1
        );
    }

    /// A plugin with nothing but unrunnable hooks fails to install, and the
    /// error says which hooks — the difference between "wrong plugin" and
    /// "wrong version of horsie".
    #[test]
    fn a_hooks_only_plugin_is_refused_and_names_the_events() {
        let src = TempDir::new().unwrap();
        let cp = src.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[{"name":"hooksonly","source":"./plugins/hooksonly"}]}"#,
        )
        .unwrap();
        let root = src.path().join("plugins/hooksonly");
        std::fs::create_dir_all(root.join("hooks")).unwrap();
        std::fs::write(
            root.join("hooks/hooks.json"),
            r#"{"hooks":{"WorktreeCreate":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        )
        .unwrap();
        commit_all(src.path());

        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        crate::marketplace::add(&p, &file_url(src.path()), None, None, false).unwrap();
        let err = install(
            &p,
            &InstallTarget::parse("hooksonly@acme"),
            None,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("WorktreeCreate"), "must name the event: {err}");
    }

    /// A plugin's manifest name may differ from its catalogue name, so `update`
    /// must re-resolve by the index's name rather than the installed one.
    #[test]
    #[cfg(unix)]
    fn update_re_resolves_by_the_index_name_not_the_installed_name() {
        let src = TempDir::new().unwrap();
        let cp = src.path().join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[{"name":"vendor-long-name","source":"./plugins/p"}]}"#,
        )
        .unwrap();
        let pcp = src.path().join("plugins/p/.claude-plugin");
        std::fs::create_dir_all(&pcp).unwrap();
        // The plugin calls itself something shorter than its catalogue entry.
        std::fs::write(pcp.join("plugin.json"), r#"{"name":"shortname"}"#).unwrap();
        let d = src.path().join("plugins/p/skills/s");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), "---\nname: s\n---\nb").unwrap();
        commit_all(src.path());

        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        crate::marketplace::add(&p, &file_url(src.path()), None, None, false).unwrap();
        let installed = install(
            &p,
            &InstallTarget::parse("vendor-long-name@acme"),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(installed, "shortname", "the manifest name wins for install");

        let entry = list(&p).into_iter().next().unwrap();
        assert_eq!(entry.marketplace_entry.as_deref(), Some("vendor-long-name"));

        update(&p, "shortname").expect("update must re-resolve via the index name");
        assert!(p.plugins.join("shortname/skills/s/SKILL.md").is_file());
    }

    #[test]
    #[cfg(unix)]
    fn duplicate_install_needs_force() {
        let src = TempDir::new().unwrap();
        impeccable_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        let url = InstallTarget::Url(file_url(src.path()));
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
