# CLI Marketplace Registry Implementation Plan (PR2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make marketplaces first-class sources you add once and install from by name — `horsie marketplace add/list/show/update/remove` and `horsie plugin install <plugin>@<marketplace>` — including the external `source` forms that 223 of `claude-plugins-public`'s 276 entries use.

**Architecture:** Every clone, whether it backs a marketplace or a plugin, lands in `<data_dir>/sources/<key>` keyed by a hash of `(url, ref)`. A marketplace is therefore just a lockfile row pointing at one of those clones, and installing from it reduces to the checkout the CLI already performs. Because both `Path` and `Git` sources resolve to `(url, ref, subpath)`, one materialisation path serves every case.

**Tech Stack:** Rust 1.96.0, serde/serde_json, clap, tempfile.

Spec: `docs/superpowers/specs/2026-08-02-plugin-marketplace-design.md` (PR2 of the three-PR staging).

## Global Constraints

- Protocol types are ONLY defined in `models/fluorite/*.fl`. **PR2 adds no wire types** — it touches no `.fl` file. The server surface is PR3.
- Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm` (workspace lints).
- Test modules open with the standard opt-out (`#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]`).
- Unit tests live in-file under `#[cfg(test)] mod tests`, using `tempfile::TempDir`.
- Tests must not touch the network — clone from `file://` fixture repos built in a `TempDir`.
- Pre-PR gates, in order: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace`.

## Design decisions carried in

- **Marketplace clones live in `sources/`, not a private directory.** They are keyed by `(url, ref)` like any other checkout, so a single-plugin marketplace whose plugin is a path into its own repo clones exactly once, and one garbage collector covers both kinds of reference.
- **Removing a marketplace must not break plugins installed from it.** Uninstalling a *source* is not uninstalling the software. Installed plugins hold their own lockfile row and their own claim on the backing clone, so `marketplace remove` only drops the marketplace's claim.
- **No new sandbox grant.** PR1 already grants all of `sources/` read-only, and marketplace clones now live there.

## File Structure

**Created:**
- `support/src/plugin/checkout.rs` — `ensure_checkout` + `PluginSource` → `(url, ref, subpath)` resolution, behind the `git` feature.
- `cli/src/marketplace.rs` — the registry: lockfile, add/list/show/update/remove.

**Modified:**
- `support/src/plugin/mod.rs` — declare `checkout`.
- `cli/src/plugins.rs` — `PluginPaths` gains `marketplaces`; `install` gains a marketplace path; GC counts marketplace claims.
- `cli/src/main.rs` — `Marketplace` subcommand; `plugin install` argument disambiguation.
- `cli/src/lib.rs` — export `marketplace`.
- `docs/guide/skills-and-plugins.md` — document the registry.

---

### Task 1: `plugin::checkout` — one materialisation path for every source

**Files:**
- Create: `support/src/plugin/checkout.rs`
- Modify: `support/src/plugin/mod.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Checkout { pub dir: PathBuf, pub key: String }
  pub fn ensure_checkout(sources_dir: &Path, url: &str, git_ref: Option<&str>) -> Result<Checkout, String>;
  pub fn source_location(source: &PluginSource, marketplace_url: &str, marketplace_ref: Option<&str>)
      -> (String, Option<String>, Option<String>);   // (url, ref, subpath)
  ```
  `source_location` is what makes `Path` and `Git` sources uniform: a `Path` entry resolves against the marketplace's *own* repo, so both end as "clone this url at this ref, then descend into this subpath".

- [ ] **Step 1: Write the failing tests**

Create `support/src/plugin/checkout.rs`:

```rust
//! Materialising a plugin source into a checkout on disk.
//!
//! Every clone — whether it backs a marketplace or a plugin — lands in
//! `<data_dir>/sources/<key>`, keyed by `(url, ref)`. A marketplace entry that
//! points at a path inside its own repo therefore shares the marketplace's
//! clone instead of duplicating it.

use super::{PluginSource, source_key};
use std::path::{Path, PathBuf};

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

    fn fixture_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
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
    fn a_path_source_resolves_against_the_marketplace_repo() {
        let (url, git_ref, sub) = source_location(
            &PluginSource::Path("./plugins/alpha".into()),
            "https://x/market.git",
            Some("v1"),
        );
        assert_eq!(url, "https://x/market.git");
        assert_eq!(git_ref.as_deref(), Some("v1"));
        assert_eq!(sub.as_deref(), Some("./plugins/alpha"));
    }

    #[test]
    fn a_git_source_resolves_against_its_own_repo_and_ignores_the_marketplace_ref() {
        let (url, git_ref, sub) = source_location(
            &PluginSource::Git {
                url: "https://x/other.git".into(),
                path: Some("plugins/p".into()),
                git_ref: Some("v2".into()),
            },
            "https://x/market.git",
            Some("v1"),
        );
        assert_eq!(url, "https://x/other.git");
        assert_eq!(git_ref.as_deref(), Some("v2"));
        assert_eq!(sub.as_deref(), Some("plugins/p"));
    }

    /// An unpinned external source must not silently inherit the marketplace's
    /// ref — that would check out a branch that need not exist in the other repo.
    #[test]
    fn an_unpinned_git_source_stays_unpinned() {
        let (_, git_ref, _) = source_location(
            &PluginSource::Git {
                url: "https://x/other.git".into(),
                path: None,
                git_ref: None,
            },
            "https://x/market.git",
            Some("v1"),
        );
        assert!(git_ref.is_none());
    }

    #[test]
    fn ensure_checkout_clones_once_and_is_idempotent() {
        let src = TempDir::new().unwrap();
        fixture_repo(src.path());
        let home = TempDir::new().unwrap();
        let sources = home.path().join("sources");
        let url = format!("file://{}", src.path().display());

        let a = ensure_checkout(&sources, &url, None).unwrap();
        assert!(a.dir.join("skills/x/SKILL.md").is_file());
        let mtime = a.dir.metadata().unwrap().modified().unwrap();

        let b = ensure_checkout(&sources, &url, None).unwrap();
        assert_eq!(a.key, b.key, "same (url, ref) must reuse the checkout");
        assert_eq!(b.dir.metadata().unwrap().modified().unwrap(), mtime);
        assert_eq!(std::fs::read_dir(&sources).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_clone_leaves_nothing_behind() {
        let home = TempDir::new().unwrap();
        let sources = home.path().join("sources");
        let err = ensure_checkout(&sources, "file:///definitely/not/a/repo", None).unwrap_err();
        assert!(err.contains("git clone failed"), "err: {err}");
        assert_eq!(
            std::fs::read_dir(&sources).map(Iterator::count).unwrap_or(0),
            0,
            "a half-clone must not survive to trip up the next attempt"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-support --features git checkout`
Expected: FAIL — `ensure_checkout` / `source_location` not found.

- [ ] **Step 3: Implement**

Insert above the `#[cfg(test)]` block:

```rust
/// A materialised checkout: where it is, and the key naming it under `sources/`.
pub struct Checkout {
    pub dir: PathBuf,
    pub key: String,
}

/// Ensure a checkout of `(url, git_ref)` exists under `sources_dir`, cloning
/// only when it is absent. A failed clone removes its own partial directory so
/// the next attempt does not mistake it for a usable checkout.
pub fn ensure_checkout(
    sources_dir: &Path,
    url: &str,
    git_ref: Option<&str>,
) -> Result<Checkout, String> {
    std::fs::create_dir_all(sources_dir).map_err(|e| e.to_string())?;
    let key = source_key(url, git_ref);
    let dir = sources_dir.join(&key);
    if !dir.exists() {
        crate::git::clone(url, git_ref, &dir).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&dir);
        })?;
    }
    Ok(Checkout { dir, key })
}

/// Where a marketplace entry's plugin actually comes from, as
/// `(url, ref, subpath)`.
///
/// A `Path` entry resolves against the marketplace's own repo, so both source
/// kinds collapse to the same "clone this, descend into that" shape. A `Git`
/// entry keeps its own ref — inheriting the marketplace's would check out a
/// branch that need not exist in the other repository.
pub fn source_location(
    source: &PluginSource,
    marketplace_url: &str,
    marketplace_ref: Option<&str>,
) -> (String, Option<String>, Option<String>) {
    match source {
        PluginSource::Path(p) => (
            marketplace_url.to_string(),
            marketplace_ref.map(str::to_string),
            Some(p.clone()),
        ),
        PluginSource::Git { url, path, git_ref } => {
            (url.clone(), git_ref.clone(), path.clone())
        }
    }
}
```

Add `pub mod checkout;` to `support/src/plugin/mod.rs`, gated with `#[cfg(feature = "git")]`, and re-export `checkout::{Checkout, ensure_checkout, source_location}` under the same gate.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-support --features git`
Expected: PASS (all crate tests plus 5 new)

- [ ] **Step 5: Commit**

```bash
git add support/
git commit -m "feat(support): one checkout path for marketplace and plugin sources"
```

---

### Task 2: The marketplace registry

**Files:**
- Create: `cli/src/marketplace.rs`
- Modify: `cli/src/lib.rs`, `cli/src/plugins.rs` (`PluginPaths.marketplaces`)

**Interfaces:**
- Consumes: `ensure_checkout` (Task 1), `Marketplace::read`, `PluginPaths`.
- Produces:
  ```rust
  pub struct MarketplaceEntry { pub name: String, pub source: String,
      pub git_ref: Option<String>, pub sha: Option<String>,
      pub plugin_count: u32, pub source_key: String }
  pub fn add(paths: &PluginPaths, url: &str, name: Option<String>,
             git_ref: Option<String>, force: bool) -> Result<String, CliError>;
  pub fn list(paths: &PluginPaths) -> Vec<MarketplaceEntry>;
  pub fn show(paths: &PluginPaths, name: &str)
      -> Result<Vec<horsie_support::plugin::MarketplaceEntry>, CliError>;
  pub fn update(paths: &PluginPaths, name: &str) -> Result<(), CliError>;
  pub fn remove(paths: &PluginPaths, name: &str) -> Result<(), CliError>;
  pub fn resolve_entry(paths: &PluginPaths, marketplace: &str, plugin: &str)
      -> Result<(String, Option<String>, Option<String>, String), CliError>;
  ```
  `resolve_entry` returns `(url, ref, subpath, entry_name)` — everything `plugins::install` needs to materialise the plugin without knowing about marketplaces.

- [ ] **Step 1: Extend `PluginPaths`**

In `cli/src/plugins.rs`, add to `PluginPaths`:

```rust
    /// `<data_dir>/marketplaces` — the registry lockfile and nothing else; the
    /// clones themselves live in `sources`, like every other checkout.
    pub marketplaces: PathBuf,
```

- [ ] **Step 2: Write the failing tests**

Create `cli/src/marketplace.rs` with a `mod tests` covering: `add` records name/plugin_count and reuses the clone; adding twice without `--force` errors; `show` lists the declared plugins; `remove` drops the row but leaves a clone that an installed plugin still claims; `update` fast-forwards and refreshes `plugin_count`; `resolve_entry` errors with the available names for an unknown plugin. Build fixtures with a `marketplace.json` declaring two path-source plugins, committed into a `file://` repo (same helper shape as `cli/src/plugins.rs` tests).

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p horsie marketplace`
Expected: FAIL — module not found.

- [ ] **Step 4: Implement the registry**

Lockfile at `<marketplaces>/marketplaces.json`, same load/save shape as `plugins.json`. `add` calls `ensure_checkout`, reads the marketplace, warns on `skipped` entries, refuses a repo with no `marketplace.json`, and records `plugin_count`. `remove` drops the row and then garbage-collects the clone. `resolve_entry` reads the marketplace from its checkout and maps the named entry through `source_location`.

- [ ] **Step 5: Make GC aware of marketplace claims**

In `cli/src/plugins.rs`, `gc_clone` must not delete a checkout a marketplace still points at:

```rust
/// Delete a checkout once neither an installed plugin nor a registered
/// marketplace references it.
fn gc_clone(paths: &PluginPaths, key: &str) {
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
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p horsie marketplace`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add cli/
git commit -m "feat(cli): marketplace registry with add/list/show/update/remove"
```

---

### Task 3: `plugin install <plugin>@<marketplace>` and external sources

**Files:**
- Modify: `cli/src/plugins.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum InstallTarget { Url(String), FromMarketplace { plugin: String, marketplace: String } }
  impl InstallTarget { pub fn parse(arg: &str) -> InstallTarget; }
  pub fn install(paths: &PluginPaths, target: &InstallTarget, name: Option<String>,
                 git_ref: Option<String>, force: bool) -> Result<String, CliError>;
  ```

- [ ] **Step 1: Write the failing tests**

Add to `cli/src/plugins.rs` `mod tests`:

```rust
    /// SSH URLs contain `@`, so the marketplace form is recognised only by the
    /// strict shape the Agent Skills spec guarantees for names: lowercase
    /// alphanumerics and hyphens on both sides.
    #[test]
    fn install_target_parsing_never_mistakes_a_url_for_a_marketplace_ref() {
        assert!(matches!(
            InstallTarget::parse("impeccable@official"),
            InstallTarget::FromMarketplace { .. }
        ));
        for url in [
            "git@github.com:x/y.git",
            "https://github.com/o/r",
            "ssh://git@host/x.git",
            "file:///tmp/x",
            "https://user@host/x.git",
            "Impeccable@Official",
        ] {
            assert!(
                matches!(InstallTarget::parse(url), InstallTarget::Url(_)),
                "{url} must parse as a URL"
            );
        }
    }
```

plus an end-to-end test installing a path-source plugin out of a registered marketplace, and one installing an entry whose `source` is an external `file://` repo with a `path` subdir (proving the external forms now work).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie plugins`
Expected: FAIL — `InstallTarget` not found.

- [ ] **Step 3: Implement**

```rust
/// What `horsie plugin install <arg>` was pointed at.
pub enum InstallTarget {
    Url(String),
    FromMarketplace { plugin: String, marketplace: String },
}

impl InstallTarget {
    /// `<plugin>@<marketplace>` only when both sides match the name shape the
    /// Agent Skills spec guarantees — lowercase alphanumerics and hyphens. No
    /// git URL form can match it, so `git@github.com:x/y.git` stays a URL.
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
```

Rework `install` to resolve `target` into `(url, git_ref, subpath, fallback_name)` up front — for `Url` by cloning and consulting `marketplace.json` as today, for `FromMarketplace` via `marketplace::resolve_entry` — then run the existing checkout → inspect → symlink → lockfile path once. The `PluginSource::Git` arm of `resolve_plugin_root` stops being an error: a single-plugin marketplace with an external source now materialises through `ensure_checkout` like any other.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie plugins`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add cli/
git commit -m "feat(cli): install plugins from a registered marketplace, incl. external sources"
```

---

### Task 4: CLI wiring

**Files:**
- Modify: `cli/src/main.rs`

- [ ] **Step 1: Add the `Marketplace` subcommand**

```rust
    /// Manage marketplaces — repos that index plugins you can install by name.
    Marketplace {
        #[command(subcommand)]
        action: MarketplaceAction,
    },
```

with `Add { url, name, git_ref, force, config }`, `List { config }`, `Show { name, config }`, `Update { name, config }`, `Remove { name, config }`, mirroring `PluginAction`'s flag shapes.

- [ ] **Step 2: Extend `resolve_plugin_paths`**

```rust
        marketplaces: cfg.storage.data_dir.join("marketplaces"),
```

- [ ] **Step 3: Route `plugin install` through `InstallTarget`**

The positional argument is renamed from `url` to `target` in the help text (`<url> or <plugin>@<marketplace>`), parsed with `InstallTarget::parse`.

- [ ] **Step 4: Print tables consistent with `plugin list`**

`marketplace list` prints `NAME`, `PLUGINS`, `SOURCE`; `marketplace show` prints `NAME`, `VERSION`, `DESCRIPTION` truncated to a sensible width. A 276-entry marketplace is exactly why `show` exists — without it there is no way to learn the names `install` expects.

- [ ] **Step 5: Verify the surface**

Run: `cargo run -p horsie -- marketplace --help` and `cargo run -p horsie -- plugin install --help`
Expected: both list the new options.

- [ ] **Step 6: Commit**

```bash
git add cli/
git commit -m "feat(cli): horsie marketplace subcommand"
```

---

### Task 5: Docs, gates, live verification, PR

**Files:**
- Modify: `docs/guide/skills-and-plugins.md`

- [ ] **Step 1: Document the registry**

Add a "Marketplaces" section under the host-library docs: add a marketplace, list what it offers, install by name, and the note that removing a marketplace leaves plugins installed from it in place.

- [ ] **Step 2: Run the gates in order**

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

Expected: clean. Do not open a PR on red.

- [ ] **Step 3: Verify against real marketplaces**

```bash
cargo run -p horsie -- marketplace add https://github.com/pbakaus/impeccable --config <tmp>
cargo run -p horsie -- marketplace show impeccable --config <tmp>
cargo run -p horsie -- plugin install impeccable@impeccable --config <tmp>
```

Then add `https://github.com/anthropics/claude-plugins-public`, confirm `marketplace show` lists hundreds of entries, and install one whose `source` is external — that is the case PR1 could not reach.

- [ ] **Step 4: Push and open the PR**

Body: one long line per paragraph or bullet. Reference #105 Phase 0 PR2, note that marketplace clones share `sources/` with plugin checkouts, and that removing a marketplace deliberately leaves installed plugins alone.

---

## Self-Review

**Spec coverage:** `marketplace add/list/update/remove` (Task 2, plus `show`, which the spec did not name but a 276-entry marketplace requires); `plugin install <plugin>@<marketplace>` and the name-vs-SSH-URL rule (Task 3); external `Git` source forms with `ref`/`commit` pinning (Tasks 1 and 3); marketplaces lockfile and clone root (Task 2). The spec's separate `marketplaces/` sandbox grant is **not needed**: marketplace clones live in `sources/`, which PR1 already grants.

**Deferred to PR3:** the wire model, `marketplaces` table, HTTP routes, and both web pages.

**Type consistency:** `cli::marketplace::MarketplaceEntry` (a lockfile row) is distinct from `horsie_support::plugin::MarketplaceEntry` (a parsed manifest entry); `show` returns the latter and is always referred to by its full path. `ensure_checkout` returns `Checkout { dir, key }` and is consumed as such in Tasks 2 and 3. `source_location`'s tuple order `(url, ref, subpath)` matches `resolve_entry`'s first three elements.

**Placeholders:** Tasks 2 and 4 describe their tests and printing by contract rather than transcribing every line, since the fixtures and table shapes follow `cli/src/plugins.rs` verbatim; every new type, signature and non-obvious body is given in full.
