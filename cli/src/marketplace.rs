//! Marketplace registry: `horsie marketplace add/list/show/update/remove`.
//!
//! A marketplace is a git repo carrying `.claude-plugin/marketplace.json`, an
//! index of plugins you can then install by name. Its clone lives in the same
//! `sources/` root as every plugin checkout, keyed by `(url, ref)` — so a
//! marketplace whose plugins are paths into its own repo is cloned exactly once
//! and shared with everything installed out of it.
//!
//! Removing a marketplace does not uninstall plugins installed from it:
//! dropping a *source* is not dropping the software. The registry only releases
//! its own claim on the checkout.

use crate::error::CliError;
use crate::plugins::PluginPaths;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One registered marketplace, recorded in the lockfile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    pub name: String,
    pub source: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub sha: Option<String>,
    /// How many plugins the index declared when it was last read.
    pub plugin_count: u32,
    /// Names the shared checkout under `sources/`.
    pub source_key: String,
}

/// The `marketplaces.json` lockfile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MarketplaceLock {
    #[serde(default)]
    pub marketplaces: Vec<MarketplaceEntry>,
}

fn lockfile_path(dir: &Path) -> PathBuf {
    dir.join("marketplaces.json")
}

fn load_lock(dir: &Path) -> MarketplaceLock {
    std::fs::read_to_string(lockfile_path(dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_lock(dir: &Path, lock: &MarketplaceLock) -> Result<(), CliError> {
    std::fs::create_dir_all(dir).map_err(|e| CliError::Io(e.to_string()))?;
    let json = serde_json::to_vec_pretty(lock).map_err(|e| CliError::Config(e.to_string()))?;
    std::fs::write(lockfile_path(dir), json).map_err(|e| CliError::Io(e.to_string()))
}

/// Derive a marketplace name: the index's own `name`, else the repo basename.
fn name_from_url(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("marketplace")
        .trim_end_matches(".git")
        .to_string()
}

/// Read a marketplace out of a checkout, warning about entries it could not
/// understand. A repo with no index is an error: the user asked to add a
/// marketplace, and a plain plugin repo is not one.
fn read_index(dir: &Path, url: &str) -> Result<horsie_support::plugin::Marketplace, CliError> {
    let market = horsie_support::plugin::Marketplace::read(dir).map_err(CliError::Config)?;
    let Some(market) = market else {
        return Err(CliError::Config(format!(
            "'{url}' has no .claude-plugin/marketplace.json; it is not a marketplace \
             (install a plain plugin repo with `horsie plugin install <url>`)"
        )));
    };
    for why in &market.skipped {
        tracing::warn!(marketplace = %url, "skipping unreadable entry: {why}");
    }
    Ok(market)
}

/// `horsie marketplace add <url>`: check out the index and register it.
pub fn add(
    paths: &PluginPaths,
    url: &str,
    name: Option<String>,
    git_ref: Option<String>,
    force: bool,
) -> Result<String, CliError> {
    let checkout = horsie_support::plugin::ensure_checkout(&paths.sources, url, git_ref.as_deref())
        .map_err(CliError::Executor)?;
    let market = read_index(&checkout.dir, url)?;

    let registered = name
        .or_else(|| market.name.clone())
        .unwrap_or_else(|| name_from_url(url));
    let mut lock = load_lock(&paths.marketplaces);
    if lock.marketplaces.iter().any(|m| m.name == registered) && !force {
        return Err(CliError::Config(format!(
            "marketplace '{registered}' is already added (use --force to re-add)"
        )));
    }
    lock.marketplaces.retain(|m| m.name != registered);
    lock.marketplaces.push(MarketplaceEntry {
        name: registered.clone(),
        source: url.to_string(),
        git_ref,
        sha: horsie_support::git::head_sha(&checkout.dir),
        plugin_count: u32::try_from(market.plugins.len()).unwrap_or(u32::MAX),
        source_key: checkout.key,
    });
    save_lock(&paths.marketplaces, &lock)?;
    Ok(registered)
}

/// `horsie marketplace list`: the registered marketplaces, from the lockfile.
pub fn list(paths: &PluginPaths) -> Vec<MarketplaceEntry> {
    load_lock(&paths.marketplaces).marketplaces
}

fn entry(paths: &PluginPaths, name: &str) -> Result<MarketplaceEntry, CliError> {
    load_lock(&paths.marketplaces)
        .marketplaces
        .into_iter()
        .find(|m| m.name == name)
        .ok_or_else(|| CliError::Config(format!("marketplace '{name}' is not added")))
}

/// `horsie marketplace show <name>`: the plugins the index declares.
pub fn show(
    paths: &PluginPaths,
    name: &str,
) -> Result<Vec<horsie_support::plugin::MarketplaceEntry>, CliError> {
    let e = entry(paths, name)?;
    let dir = paths.sources.join(&e.source_key);
    Ok(read_index(&dir, &e.source)?.plugins)
}

/// `horsie marketplace update <name>`: fast-forward the index and refresh the
/// recorded sha and plugin count.
pub fn update(paths: &PluginPaths, name: &str) -> Result<(), CliError> {
    let e = entry(paths, name)?;
    let dir = paths.sources.join(&e.source_key);
    horsie_support::git::pull_ff_only(&dir).map_err(CliError::Executor)?;
    let market = read_index(&dir, &e.source)?;

    let mut lock = load_lock(&paths.marketplaces);
    if let Some(row) = lock.marketplaces.iter_mut().find(|m| m.name == name) {
        row.sha = horsie_support::git::head_sha(&dir);
        row.plugin_count = u32::try_from(market.plugins.len()).unwrap_or(u32::MAX);
    }
    save_lock(&paths.marketplaces, &lock)
}

/// `horsie marketplace remove <name>`: drop the registration, then release its
/// claim on the shared checkout. Plugins installed from it keep working.
pub fn remove(paths: &PluginPaths, name: &str) -> Result<(), CliError> {
    let e = entry(paths, name)?;
    let mut lock = load_lock(&paths.marketplaces);
    lock.marketplaces.retain(|m| m.name != name);
    save_lock(&paths.marketplaces, &lock)?;
    crate::plugins::gc_checkout(paths, &e.source_key);
    Ok(())
}

/// How many candidate names an error message lists before deferring to
/// `marketplace show`. The public marketplace has 276 entries; dumping them all
/// buries the error itself.
const MAX_SUGGESTIONS: usize = 8;

/// A short "did you mean" tail for an unknown plugin name.
fn suggest(names: &[&str], marketplace: &str) -> String {
    if names.len() <= MAX_SUGGESTIONS {
        return format!("Available: {}", names.join(", "));
    }
    format!(
        "Available (first {MAX_SUGGESTIONS} of {}): {}. Run `horsie marketplace show {marketplace}` for the full list.",
        names.len(),
        names[..MAX_SUGGESTIONS].join(", ")
    )
}

/// Resolve `<plugin>@<marketplace>` to everything an install needs:
/// `(url, ref, subpath, entry name)`.
pub fn resolve_entry(
    paths: &PluginPaths,
    marketplace: &str,
    plugin: &str,
) -> Result<(String, Option<String>, Option<String>, String), CliError> {
    let e = entry(paths, marketplace)?;
    let dir = paths.sources.join(&e.source_key);
    let market = read_index(&dir, &e.source)?;
    let found = market.find(plugin).ok_or_else(|| {
        CliError::Config(format!(
            "marketplace '{marketplace}' has no plugin '{plugin}'. {}",
            suggest(&market.names(), marketplace)
        ))
    })?;
    let (url, git_ref, subpath) =
        horsie_support::plugin::source_location(&found.source, &e.source, e.git_ref.as_deref());
    Ok((url, git_ref, subpath, found.name.clone()))
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

    fn git_run(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn commit_all(dir: &Path) {
        git_run(dir, &["add", "-A"]);
        git_run(dir, &["commit", "-qm", "change"]);
    }

    fn init_repo(dir: &Path) {
        git_run(dir, &["init", "-q", "-b", "main"]);
        git_run(dir, &["config", "user.email", "t@example.com"]);
        git_run(dir, &["config", "user.name", "t"]);
    }

    fn write_skill(root: &Path, rel: &str, name: &str) {
        let d = root.join(rel);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), format!("---\nname: {name}\n---\nbody")).unwrap();
    }

    /// A marketplace repo indexing two plugins that live inside it.
    fn market_fixture(dir: &Path) {
        let cp = dir.join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("marketplace.json"),
            r#"{"name":"acme","plugins":[
                 {"name":"alpha","description":"first","version":"1.0","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        )
        .unwrap();
        write_skill(dir, "plugins/alpha/skills/alpha", "alpha");
        write_skill(dir, "plugins/beta/skills/beta", "beta");
        init_repo(dir);
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
    fn add_registers_the_index_name_and_plugin_count() {
        let src = TempDir::new().unwrap();
        market_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());

        let name = add(&p, &file_url(src.path()), None, None, false).unwrap();
        assert_eq!(
            name, "acme",
            "the index's own name wins over the repo basename"
        );

        let rows = list(&p);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].plugin_count, 2);
        assert!(rows[0].sha.is_some());
        assert!(p.sources.join(&rows[0].source_key).is_dir());
    }

    #[test]
    fn adding_twice_needs_force() {
        let src = TempDir::new().unwrap();
        market_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        let url = file_url(src.path());
        add(&p, &url, None, None, false).unwrap();
        assert!(add(&p, &url, None, None, false).is_err());
        add(&p, &url, None, None, true).unwrap();
        assert_eq!(list(&p).len(), 1, "re-adding must not duplicate the row");
    }

    #[test]
    fn a_repo_without_an_index_is_not_a_marketplace() {
        let src = TempDir::new().unwrap();
        write_skill(src.path(), "skills/x", "x");
        init_repo(src.path());
        commit_all(src.path());

        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        let err = add(&p, &file_url(src.path()), None, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("marketplace.json"), "err: {err}");
        assert!(err.contains("horsie plugin install"), "err: {err}");
    }

    #[test]
    fn show_lists_the_declared_plugins() {
        let src = TempDir::new().unwrap();
        market_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        add(&p, &file_url(src.path()), None, None, false).unwrap();

        let plugins = show(&p, "acme").unwrap();
        let names: Vec<&str> = plugins.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(plugins[0].description.as_deref(), Some("first"));
    }

    #[test]
    fn update_refreshes_the_plugin_count() {
        let src = TempDir::new().unwrap();
        market_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        add(&p, &file_url(src.path()), None, None, false).unwrap();
        assert_eq!(list(&p)[0].plugin_count, 2);

        std::fs::write(
            src.path().join(".claude-plugin/marketplace.json"),
            r#"{"name":"acme","plugins":[{"name":"alpha","source":"./plugins/alpha"}]}"#,
        )
        .unwrap();
        commit_all(src.path());

        update(&p, "acme").unwrap();
        assert_eq!(list(&p)[0].plugin_count, 1);
    }

    #[test]
    fn resolve_entry_maps_a_path_source_onto_the_marketplace_repo() {
        let src = TempDir::new().unwrap();
        market_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        let url = file_url(src.path());
        add(&p, &url, None, None, false).unwrap();

        let (resolved_url, git_ref, subpath, name) = resolve_entry(&p, "acme", "beta").unwrap();
        assert_eq!(resolved_url, url);
        assert!(git_ref.is_none());
        assert_eq!(subpath.as_deref(), Some("./plugins/beta"));
        assert_eq!(name, "beta");
    }

    #[test]
    fn resolve_entry_names_the_alternatives_when_the_plugin_is_unknown() {
        let src = TempDir::new().unwrap();
        market_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        add(&p, &file_url(src.path()), None, None, false).unwrap();

        let err = resolve_entry(&p, "acme", "nope").unwrap_err().to_string();
        assert!(err.contains("alpha"), "err: {err}");
        assert!(err.contains("beta"), "err: {err}");
    }

    /// The public marketplace has 276 entries; an error that lists them all
    /// buries itself.
    #[test]
    fn unknown_plugin_truncates_a_long_candidate_list() {
        let names: Vec<String> = (0..40).map(|i| format!("p{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let msg = suggest(&refs, "big");
        assert!(msg.contains("first 8 of 40"), "msg: {msg}");
        assert!(msg.contains("horsie marketplace show big"), "msg: {msg}");
        assert!(!msg.contains("p39"), "must not dump the tail: {msg}");

        // A short list is still shown in full.
        let short = suggest(&["a", "b"], "small");
        assert!(short.contains("Available: a, b"), "msg: {short}");
    }

    #[test]
    fn unknown_marketplace_errors() {
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        assert!(show(&p, "nope").is_err());
        assert!(update(&p, "nope").is_err());
        assert!(remove(&p, "nope").is_err());
    }

    #[test]
    fn remove_drops_the_row_and_releases_the_checkout() {
        let src = TempDir::new().unwrap();
        market_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        add(&p, &file_url(src.path()), None, None, false).unwrap();
        let key = list(&p)[0].source_key.clone();

        remove(&p, "acme").unwrap();
        assert!(list(&p).is_empty());
        assert!(
            !p.sources.join(&key).exists(),
            "nothing else claimed the checkout, so it should be gone"
        );
    }
}
