# Plugin Manifest Resolver Implementation Plan (PR1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make plugins installable regardless of where the plugin root sits in a repo, by consolidating the three `.claude-plugin/plugin.json` parsers into one shared crate and teaching the CLI installer about marketplace-declared plugin roots.

**Architecture:** A new `horsie-support` crate owns manifest parsing, skill discovery, plugin-root inspection, marketplace parsing, and (behind a `git` feature) git helpers. `runtime`, `server` and `cli` are rewired onto it, deleting three divergent copies. The CLI install layout changes from "clone into `plugins_dir/<name>`" to "clone into `<data_dir>/sources/<key>`, symlink `plugins_dir/<name>` at the resolved plugin root", which supports subpath plugins while preserving `git pull`. Sandbox grants are extended to cover the symlink targets.

**Tech Stack:** Rust 1.96.0, serde/serde_json, sha2, tempfile, thiserror, tracing.

Spec: `docs/superpowers/specs/2026-08-02-plugin-marketplace-design.md`

## Global Constraints

- Protocol types are ONLY defined in `models/fluorite/*.fl` (codegen). Never hand-write protocol structs. **PR1 adds no wire types** — it touches no `.fl` file.
- Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm` (workspace lints).
- Test modules open with the standard opt-out:
  ```rust
  #[cfg(test)]
  #[allow(
      clippy::unwrap_used,
      clippy::expect_used,
      clippy::panic,
      clippy::wildcard_enum_match_arm
  )]
  mod tests {
  ```
- Unit tests live in-file under `#[cfg(test)] mod tests`, using `tempfile::TempDir`.
- New crate version is `0.1.6`, matching every other workspace crate (the `publish.yml` version-guard job compares the git tag against every crate version).
- Tests must not touch the network. Git tests clone from a `file://` fixture repo created in a `TempDir`.
- Avoid mutating process env (`std::env::set_var`) in tests — it is process-global and races with parallel tests.
- Pre-PR gates, in this order: `cargo fmt --all`, then `cargo clippy --all-targets --all-features -- -D warnings`, then `cargo test --workspace`. (fmt before clippy: clippy reports formatting-sensitive spans, and reformatting after clippy can reintroduce warnings.)

## File Structure

**Created:**
- `support/Cargo.toml` — new crate `horsie-support` v0.1.6, `git` feature (non-default).
- `support/src/lib.rs` — module declarations only; no items at the crate root.
- `support/src/plugin/mod.rs` — re-exports + `source_key`.
- `support/src/plugin/manifest.rs` — `PluginManifest`.
- `support/src/plugin/skills.rs` — skill location + enumeration.
- `support/src/plugin/layout.rs` — `PluginRoot::inspect`.
- `support/src/plugin/marketplace.rs` — `Marketplace`, `MarketplaceEntry`, `PluginSource`.
- `support/src/plugin/grants.rs` — sandbox `Dir` grants for the plugin library (moved out of `cli`).
- `support/src/git.rs` — clone/pull/head-sha, behind the `git` feature.

**Modified:**
- `Cargo.toml` — add `support` to workspace members.
- `runtime/src/plugins.rs` — delete `read_manifest`, `plugin_name`, `skills_locations`; `discover_skills` calls into `horsie-support`.
- `server/src/plugins/ingest.rs` — `inspect_plugin_dir` calls into `horsie-support`.
- `cli/src/plugins.rs` — sources+symlink layout, manifest-aware gate, marketplace resolution.
- `cli/src/capabilities.rs` — `with_plugin_grants` delegates to `horsie-support`.
- `cli/src/main.rs` — resolve `sources_dir`; pass it through install/update/remove.
- `cli/src/daemon/mod.rs` — pass `sources_dir` to `with_plugin_grants`.
- `runtime-vendor/src/vendor.rs` — inject host-library grants into the capability spec it writes.
- `docs/guide/skills-and-plugins.md` — document the new layout and marketplace-aware install.

---

### Task 1: Scaffold `horsie-support` with `plugin::manifest`

**Files:**
- Create: `support/Cargo.toml`, `support/src/lib.rs`, `support/src/plugin/mod.rs`, `support/src/plugin/manifest.rs`
- Modify: `Cargo.toml` (workspace members)

**Interfaces:**
- Produces:
  ```rust
  pub struct PluginManifest {
      pub name: Option<String>,
      pub version: Option<String>,
      pub description: Option<String>,
      pub skills: Vec<String>,   // empty => caller uses the default "skills"
  }
  impl PluginManifest {
      pub fn path(plugin_root: &Path) -> PathBuf;
      pub fn read(plugin_root: &Path) -> Result<Option<PluginManifest>, String>;
  }
  ```
  `read` returns `Ok(None)` when the file is absent and `Err` when it is present but malformed. Both callers need this split: the runtime ignores errors (best-effort discovery), the server surfaces them (install must fail loudly).

- [ ] **Step 1: Add the crate to the workspace**

In `Cargo.toml`, add `"support",` to `[workspace] members`, immediately after `"models",`.

- [ ] **Step 2: Write `support/Cargo.toml`**

```toml
[package]
name = "horsie-support"
license = "MIT OR Apache-2.0"
repository = "https://github.com/blossomstack/horsie"
description = "Shared host-side helpers for horsie: plugin manifests, skills, marketplaces"
version = "0.1.6"
edition = "2024"

[features]
default = []
# Git helpers shell out to `git`; only the CLI and server need them. The runtime
# ships into the sandbox and only reads already-materialised trees.
git = []

[dependencies]
horsie-models = { version = "0.1.6", path = "../models" }
serde      = { workspace = true }
serde_json = { workspace = true }
sha2       = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }

[lints]
workspace = true
```

- [ ] **Step 3: Write `support/src/lib.rs`**

```rust
//! Hand-written helpers shared by the horsie binaries — the counterpart to
//! `horsie-models`, which holds only generated wire types.
//!
//! Every item lives under a domain module; nothing is exported at the crate
//! root. A module that acquires its own heavy dependencies graduates into its
//! own crate.

#[cfg(feature = "git")]
pub mod git;
pub mod plugin;
```

- [ ] **Step 4: Write `support/src/plugin/mod.rs`**

```rust
//! Reading Claude Code plugin packaging: `.claude-plugin/plugin.json`,
//! `.claude-plugin/marketplace.json`, and the skills they point at.
//!
//! horsie reads this format and never writes it. See
//! `docs/superpowers/specs/2026-08-02-plugin-marketplace-design.md`.

pub mod grants;
pub mod layout;
pub mod manifest;
pub mod marketplace;
pub mod skills;

pub use layout::PluginRoot;
pub use manifest::PluginManifest;
pub use marketplace::{Marketplace, MarketplaceEntry, PluginSource};

use sha2::{Digest, Sha256};

/// Stable short key for a checkout of `(url, git_ref)`, used to name the shared
/// clone under `<data_dir>/sources/`. Keyed by source rather than by plugin name
/// so a marketplace declaring several plugins as paths into its own repo clones
/// once.
pub fn source_key(url: &str, git_ref: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.trim().as_bytes());
    hasher.update(b"\n");
    hasher.update(git_ref.unwrap_or("").as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}
```

Note: `grants`, `layout`, `marketplace` and `skills` are created in later tasks. To keep this task compiling on its own, add the four `pub mod` lines only as each module is created — for **this** task, `mod.rs` declares `pub mod manifest;` and `pub use manifest::PluginManifest;` plus `source_key`, and later tasks add their own line.

- [ ] **Step 5: Write the failing tests in `support/src/plugin/manifest.rs`**

```rust
//! `.claude-plugin/plugin.json`.
//!
//! Only the fields horsie uses today are modelled. Agents, commands, hooks and
//! `mcpServers` are added here — in one place — by later phases of #105.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginManifest {
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Skill roots relative to the plugin root. Empty means "not declared" —
    /// callers fall back to the conventional `skills/`.
    pub skills: Vec<String>,
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

    fn write_manifest(root: &Path, json: &str) {
        let dir = root.join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), json).unwrap();
    }

    #[test]
    fn absent_manifest_is_ok_none() {
        let dir = TempDir::new().unwrap();
        assert_eq!(PluginManifest::read(dir.path()).unwrap(), None);
    }

    #[test]
    fn reads_scalar_fields() {
        let dir = TempDir::new().unwrap();
        write_manifest(
            dir.path(),
            r#"{"name":"impeccable","version":"4.0.4","description":"d"}"#,
        );
        let m = PluginManifest::read(dir.path()).unwrap().unwrap();
        assert_eq!(m.name.as_deref(), Some("impeccable"));
        assert_eq!(m.version.as_deref(), Some("4.0.4"));
        assert_eq!(m.description.as_deref(), Some("d"));
        assert!(m.skills.is_empty());
    }

    #[test]
    fn skills_accepts_string_or_array() {
        let dir = TempDir::new().unwrap();
        write_manifest(dir.path(), r#"{"skills":"./.claude/skills/"}"#);
        assert_eq!(
            PluginManifest::read(dir.path()).unwrap().unwrap().skills,
            vec!["./.claude/skills/".to_string()]
        );

        let dir2 = TempDir::new().unwrap();
        write_manifest(dir2.path(), r#"{"skills":["a/skills","b/skills"]}"#);
        assert_eq!(
            PluginManifest::read(dir2.path()).unwrap().unwrap().skills,
            vec!["a/skills".to_string(), "b/skills".to_string()]
        );
    }

    #[test]
    fn malformed_manifest_is_err_not_none() {
        let dir = TempDir::new().unwrap();
        write_manifest(dir.path(), "{not json");
        let err = PluginManifest::read(dir.path()).unwrap_err();
        assert!(err.contains("plugin.json"), "error should name the file: {err}");
    }

    #[test]
    fn source_key_is_stable_and_ref_sensitive() {
        let a = super::super::source_key("https://x/y.git", None);
        let b = super::super::source_key("https://x/y.git", None);
        let c = super::super::source_key("https://x/y.git", Some("v2"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }
}
```

- [ ] **Step 6: Run the tests to verify they fail**

Run: `cargo test -p horsie-support`
Expected: FAIL — `PluginManifest::read` and `PluginManifest::path` are not defined.

- [ ] **Step 7: Implement `read`/`path`**

Insert above the `#[cfg(test)]` block in `support/src/plugin/manifest.rs`:

```rust
/// Raw wire shape. `skills` is a string or an array of strings.
#[derive(Deserialize)]
struct RawManifest {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    skills: Option<StringOrList>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    fn into_vec(self) -> Vec<String> {
        match self {
            StringOrList::One(s) => vec![s],
            StringOrList::Many(v) => v,
        }
    }
}

impl PluginManifest {
    /// `<plugin_root>/.claude-plugin/plugin.json`.
    pub fn path(plugin_root: &Path) -> PathBuf {
        plugin_root.join(".claude-plugin").join("plugin.json")
    }

    /// `Ok(None)` when absent; `Err` when present but unreadable or malformed.
    pub fn read(plugin_root: &Path) -> Result<Option<PluginManifest>, String> {
        let path = Self::path(plugin_root);
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let raw: RawManifest =
            serde_json::from_str(&text).map_err(|e| format!("plugin.json: {e}"))?;
        Ok(Some(PluginManifest {
            name: raw.name,
            version: raw.version,
            description: raw.description,
            skills: raw.skills.map(StringOrList::into_vec).unwrap_or_default(),
        }))
    }
}
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p horsie-support`
Expected: PASS (5 tests)

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock support/
git commit -m "feat(support): add horsie-support crate with plugin manifest parsing"
```

---

### Task 2: `plugin::skills` — manifest-aware skill discovery

**Files:**
- Create: `support/src/plugin/skills.rs`
- Modify: `support/src/plugin/mod.rs` (add `pub mod skills;`)

**Interfaces:**
- Consumes: `PluginManifest` (Task 1).
- Produces:
  ```rust
  pub fn skill_locations(plugin_root: &Path, manifest: Option<&PluginManifest>) -> Vec<PathBuf>;
  pub fn skill_dirs(plugin_root: &Path, manifest: Option<&PluginManifest>) -> Vec<PathBuf>;
  ```
  `skill_dirs` returns each directory containing a `SKILL.md`, sorted. Callers derive the file as `dir.join("SKILL.md")`. This one function replaces the runtime's glob, the server's read_dir count, and the CLI's `has_skills`.

- [ ] **Step 1: Write the failing tests**

Create `support/src/plugin/skills.rs`:

```rust
//! Locating a plugin's skills: the manifest `skills` field (string or array of
//! roots relative to the plugin root), else the conventional `skills/`. A skill
//! is a direct child directory of a root that contains a `SKILL.md`.

use super::PluginManifest;
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

    fn write_skill(root: &Path, rel: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: x\n---\nbody").unwrap();
    }

    #[test]
    fn defaults_to_skills_dir() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "skills/brainstorming");
        let dirs = skill_dirs(dir.path(), None);
        assert_eq!(dirs, vec![dir.path().join("skills/brainstorming")]);
    }

    #[test]
    fn manifest_override_replaces_the_default() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "custom/skills/x");
        write_skill(dir.path(), "skills/ignored");
        let m = PluginManifest {
            skills: vec!["custom/skills".into()],
            ..Default::default()
        };
        let dirs = skill_dirs(dir.path(), Some(&m));
        assert_eq!(dirs, vec![dir.path().join("custom/skills/x")]);
    }

    /// impeccable's shape: a `./`-prefixed, trailing-slash, dot-directory root.
    #[test]
    fn dot_prefixed_hidden_root_resolves() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), ".claude/skills/impeccable");
        let m = PluginManifest {
            skills: vec!["./.claude/skills/".into()],
            ..Default::default()
        };
        let dirs = skill_dirs(dir.path(), Some(&m));
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].ends_with("impeccable"));
        // The returned path must stay under the plugin root and must not be
        // canonicalised — callers strip_prefix it to build a relative id.
        assert!(dirs[0].strip_prefix(dir.path()).is_ok());
    }

    #[test]
    fn array_roots_are_all_scanned_and_sorted() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "b/skills/two");
        write_skill(dir.path(), "a/skills/one");
        let m = PluginManifest {
            skills: vec!["a/skills".into(), "b/skills".into()],
            ..Default::default()
        };
        let dirs = skill_dirs(dir.path(), Some(&m));
        assert_eq!(
            dirs,
            vec![
                dir.path().join("a/skills/one"),
                dir.path().join("b/skills/two"),
            ]
        );
    }

    #[test]
    fn missing_or_empty_roots_yield_nothing() {
        let dir = TempDir::new().unwrap();
        assert!(skill_dirs(dir.path(), None).is_empty());
        std::fs::create_dir_all(dir.path().join("skills/notaskill")).unwrap();
        assert!(skill_dirs(dir.path(), None).is_empty());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-support skills`
Expected: FAIL — `skill_dirs` not found.

- [ ] **Step 3: Implement**

Insert above the `#[cfg(test)]` block:

```rust
/// Skill roots for a plugin: the manifest override when declared, else `skills/`.
pub fn skill_locations(plugin_root: &Path, manifest: Option<&PluginManifest>) -> Vec<PathBuf> {
    match manifest.map(|m| m.skills.as_slice()) {
        Some(roots) if !roots.is_empty() => {
            roots.iter().map(|r| plugin_root.join(r)).collect()
        }
        _ => vec![plugin_root.join("skills")],
    }
}

/// Every directory under a skill root that contains a `SKILL.md`, sorted for
/// stable ordering. Paths are built by joining, never canonicalised, so callers
/// can `strip_prefix` a library root off them.
pub fn skill_dirs(plugin_root: &Path, manifest: Option<&PluginManifest>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in skill_locations(plugin_root, manifest) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if dir.join("SKILL.md").is_file() {
                out.push(dir);
            }
        }
    }
    out.sort();
    out
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-support skills`
Expected: PASS (5 tests)

- [ ] **Step 5: Commit**

```bash
git add support/src/plugin/
git commit -m "feat(support): manifest-aware skill discovery"
```

---

### Task 3: `plugin::layout` — plugin-root inspection

**Files:**
- Create: `support/src/plugin/layout.rs`
- Modify: `support/src/plugin/mod.rs` (add `pub mod layout;` and the `PluginRoot` re-export)

**Interfaces:**
- Consumes: `PluginManifest` (Task 1), `skill_dirs` (Task 2).
- Produces:
  ```rust
  pub struct PluginRoot {
      pub dir: PathBuf,
      pub manifest: Option<PluginManifest>,
      pub skill_dirs: Vec<PathBuf>,
  }
  impl PluginRoot {
      pub fn inspect(dir: &Path) -> Result<PluginRoot, String>;
      pub fn name(&self, fallback: &str) -> String;
      pub fn version(&self) -> Option<&str>;
      pub fn description(&self) -> Option<&str>;
      pub fn is_installable(&self) -> bool;
      pub fn rejection(&self) -> String;
  }
  ```
  `is_installable` is deliberately "has at least one skill", preserving today's behaviour. Phase 1 of #105 widens it here, once.

- [ ] **Step 1: Write the failing tests**

Create `support/src/plugin/layout.rs`:

```rust
//! Deciding whether a directory is an installable plugin, and describing it.

use super::{skills, PluginManifest};
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

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn plain_skills_dir_is_installable() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("skills/x/SKILL.md"), "---\nname: x\n---\n");
        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert!(root.is_installable());
        assert_eq!(root.name("fallback"), "fallback");
    }

    #[test]
    fn manifest_name_wins_over_fallback() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join(".claude-plugin/plugin.json"),
            r#"{"name":"fancy","version":"2.0","description":"d"}"#,
        );
        write(&dir.path().join("skills/x/SKILL.md"), "---\nname: x\n---\n");
        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert_eq!(root.name("fallback"), "fancy");
        assert_eq!(root.version(), Some("2.0"));
        assert_eq!(root.description(), Some("d"));
    }

    /// The impeccable case: manifest points skills outside the default location.
    #[test]
    fn manifest_skills_override_makes_it_installable() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join(".claude-plugin/plugin.json"),
            r#"{"name":"impeccable","skills":"./.claude/skills/"}"#,
        );
        write(
            &dir.path().join(".claude/skills/impeccable/SKILL.md"),
            "---\nname: impeccable\n---\n",
        );
        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert!(
            root.is_installable(),
            "manifest-declared skills root must count"
        );
        assert_eq!(root.skill_dirs.len(), 1);
    }

    #[test]
    fn no_skills_is_not_installable_and_says_where_it_looked() {
        let dir = TempDir::new().unwrap();
        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert!(!root.is_installable());
        let msg = root.rejection();
        assert!(msg.contains("skills"), "rejection should name the location: {msg}");
    }

    #[test]
    fn malformed_manifest_propagates() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join(".claude-plugin/plugin.json"), "{oops");
        assert!(PluginRoot::inspect(dir.path()).is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-support layout`
Expected: FAIL — `PluginRoot` not found.

- [ ] **Step 3: Implement**

Insert above the `#[cfg(test)]` block:

```rust
/// An inspected plugin directory.
pub struct PluginRoot {
    pub dir: PathBuf,
    pub manifest: Option<PluginManifest>,
    pub skill_dirs: Vec<PathBuf>,
}

impl PluginRoot {
    /// Read the manifest (if any) and enumerate skills. `Err` only when a
    /// manifest is present but malformed — an absent manifest is normal.
    pub fn inspect(dir: &Path) -> Result<PluginRoot, String> {
        let manifest = PluginManifest::read(dir)?;
        let skill_dirs = skills::skill_dirs(dir, manifest.as_ref());
        Ok(PluginRoot {
            dir: dir.to_path_buf(),
            manifest,
            skill_dirs,
        })
    }

    /// Manifest `name`, else `fallback` (normally the repo basename).
    pub fn name(&self, fallback: &str) -> String {
        self.manifest
            .as_ref()
            .and_then(|m| m.name.as_deref())
            .unwrap_or(fallback)
            .to_string()
    }

    pub fn version(&self) -> Option<&str> {
        self.manifest.as_ref().and_then(|m| m.version.as_deref())
    }

    pub fn description(&self) -> Option<&str> {
        self.manifest.as_ref().and_then(|m| m.description.as_deref())
    }

    /// Today: a plugin is installable when it provides at least one skill.
    /// Widening this to hooks/agents/commands is Phase 1 of #105 — and this is
    /// the single place it changes.
    pub fn is_installable(&self) -> bool {
        !self.skill_dirs.is_empty()
    }

    /// Why `is_installable` is false, naming every location that was searched
    /// so the user can see what the tool expected.
    pub fn rejection(&self) -> String {
        let looked = skills::skill_locations(&self.dir, self.manifest.as_ref())
            .iter()
            .map(|p| {
                p.strip_prefix(&self.dir)
                    .unwrap_or(p)
                    .display()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("no skills found: looked for */SKILL.md under {looked}")
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-support layout`
Expected: PASS (5 tests)

- [ ] **Step 5: Commit**

```bash
git add support/src/plugin/
git commit -m "feat(support): plugin root inspection"
```

---

### Task 4: `plugin::marketplace` — parse and normalise source forms

**Files:**
- Create: `support/src/plugin/marketplace.rs`
- Modify: `support/src/plugin/mod.rs` (add `pub mod marketplace;` and re-exports)

**Interfaces:**
- Produces:
  ```rust
  pub enum PluginSource {
      Path(String),
      Git { url: String, path: Option<String>, git_ref: Option<String> },
  }
  pub struct MarketplaceEntry {
      pub name: String,
      pub description: Option<String>,
      pub version: Option<String>,
      pub source: PluginSource,
  }
  pub struct Marketplace {
      pub name: Option<String>,
      pub plugins: Vec<MarketplaceEntry>,
      pub skipped: Vec<String>,
  }
  impl Marketplace {
      pub fn path(repo_root: &Path) -> PathBuf;
      pub fn read(repo_root: &Path) -> Result<Option<Marketplace>, String>;
      pub fn find(&self, name: &str) -> Option<&MarketplaceEntry>;
      pub fn names(&self) -> Vec<&str>;
  }
  ```

- [ ] **Step 1: Write the failing tests**

Create `support/src/plugin/marketplace.rs`:

```rust
//! `.claude-plugin/marketplace.json` — an index of plugins. Entries mostly point
//! *outward* at other repos: of the 276 entries in `claude-plugins-public`, 223
//! are external. Four `source` shapes occur in the wild; all normalise to
//! [`PluginSource`].
//!
//! A malformed entry is skipped rather than failing the whole marketplace: one
//! bad row must not brick a 276-entry index.

use serde::Deserialize;
use serde_json::Value;
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

    fn write_marketplace(root: &Path, json: &str) {
        let dir = root.join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marketplace.json"), json).unwrap();
    }

    #[test]
    fn absent_marketplace_is_ok_none() {
        let dir = TempDir::new().unwrap();
        assert!(Marketplace::read(dir.path()).unwrap().is_none());
    }

    #[test]
    fn relative_path_source() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"name":"impeccable","plugins":[
                 {"name":"impeccable","description":"d","version":"4.0.4","source":"./plugin"}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(m.name.as_deref(), Some("impeccable"));
        assert_eq!(m.plugins.len(), 1);
        let e = &m.plugins[0];
        assert_eq!(e.name, "impeccable");
        assert_eq!(e.version.as_deref(), Some("4.0.4"));
        assert_eq!(e.source, PluginSource::Path("./plugin".into()));
    }

    #[test]
    fn git_subdir_source() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"plugins":[{"name":"p","source":{"source":"git-subdir",
                 "url":"https://github.com/o/r.git","path":"plugins/p","ref":"v1.5.5",
                 "sha":"deadbeef"}}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(
            m.plugins[0].source,
            PluginSource::Git {
                url: "https://github.com/o/r.git".into(),
                path: Some("plugins/p".into()),
                git_ref: Some("v1.5.5".into()),
            }
        );
    }

    #[test]
    fn url_source_with_and_without_path() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"plugins":[
                 {"name":"a","source":{"source":"url","url":"https://x/a.git","sha":"s"}},
                 {"name":"b","source":{"source":"url","url":"https://x/b.git","path":"sub/b"}}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(
            m.plugins[0].source,
            PluginSource::Git {
                url: "https://x/a.git".into(),
                path: None,
                git_ref: None,
            }
        );
        assert_eq!(
            m.plugins[1].source,
            PluginSource::Git {
                url: "https://x/b.git".into(),
                path: Some("sub/b".into()),
                git_ref: None,
            }
        );
    }

    #[test]
    fn github_source_expands_to_a_url_and_pins_the_commit() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"plugins":[{"name":"p","source":{"source":"github",
                 "repo":"fullstorydev/fullstory-skills","commit":"1ec5865"}}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(
            m.plugins[0].source,
            PluginSource::Git {
                url: "https://github.com/fullstorydev/fullstory-skills.git".into(),
                path: None,
                git_ref: Some("1ec5865".into()),
            }
        );
    }

    #[test]
    fn malformed_entry_is_skipped_not_fatal() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"plugins":[
                 {"name":"good","source":"./a"},
                 {"name":"nosource"},
                 {"source":"./noname"},
                 {"name":"badkind","source":{"source":"carrier-pigeon"}}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(m.names(), vec!["good"]);
        assert_eq!(m.skipped.len(), 3, "skipped: {:?}", m.skipped);
    }

    #[test]
    fn find_and_names() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"plugins":[{"name":"a","source":"./a"},{"name":"b","source":"./b"}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(m.names(), vec!["a", "b"]);
        assert!(m.find("a").is_some());
        assert!(m.find("zzz").is_none());
    }

    #[test]
    fn malformed_json_is_err() {
        let dir = TempDir::new().unwrap();
        write_marketplace(dir.path(), "{nope");
        assert!(Marketplace::read(dir.path()).is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-support marketplace`
Expected: FAIL — `Marketplace` not found.

- [ ] **Step 3: Implement**

Insert above the `#[cfg(test)]` block:

```rust
/// Where a marketplace entry's plugin tree comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSource {
    /// A path inside the marketplace repo itself.
    Path(String),
    /// Another git repo, optionally a subdirectory of it, optionally pinned.
    Git {
        url: String,
        path: Option<String>,
        git_ref: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct MarketplaceEntry {
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub source: PluginSource,
}

#[derive(Debug, Clone)]
pub struct Marketplace {
    pub name: Option<String>,
    pub plugins: Vec<MarketplaceEntry>,
    /// Human-readable reasons for entries that could not be understood.
    pub skipped: Vec<String>,
}

#[derive(Deserialize)]
struct RawMarketplace {
    name: Option<String>,
    #[serde(default)]
    plugins: Vec<Value>,
}

impl Marketplace {
    /// `<repo_root>/.claude-plugin/marketplace.json`.
    pub fn path(repo_root: &Path) -> PathBuf {
        repo_root.join(".claude-plugin").join("marketplace.json")
    }

    /// `Ok(None)` when absent; `Err` when present but malformed at the top level.
    pub fn read(repo_root: &Path) -> Result<Option<Marketplace>, String> {
        let path = Self::path(repo_root);
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let raw: RawMarketplace =
            serde_json::from_str(&text).map_err(|e| format!("marketplace.json: {e}"))?;

        let mut plugins = Vec::new();
        let mut skipped = Vec::new();
        for (i, entry) in raw.plugins.iter().enumerate() {
            match parse_entry(entry) {
                Ok(e) => plugins.push(e),
                Err(why) => skipped.push(format!("entry {i}: {why}")),
            }
        }
        Ok(Some(Marketplace {
            name: raw.name,
            plugins,
            skipped,
        }))
    }

    pub fn find(&self, name: &str) -> Option<&MarketplaceEntry> {
        self.plugins.iter().find(|p| p.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.name.as_str()).collect()
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn parse_entry(v: &Value) -> Result<MarketplaceEntry, String> {
    let name = str_field(v, "name").ok_or("missing 'name'")?;
    let source = v.get("source").ok_or("missing 'source'")?;
    let source = parse_source(source)?;
    Ok(MarketplaceEntry {
        name,
        description: str_field(v, "description"),
        version: str_field(v, "version"),
        source,
    })
}

fn parse_source(v: &Value) -> Result<PluginSource, String> {
    if let Some(path) = v.as_str() {
        if path.is_empty() {
            return Err("empty path source".to_string());
        }
        return Ok(PluginSource::Path(path.to_string()));
    }
    let kind = str_field(v, "source").ok_or("source object missing 'source' kind")?;
    match kind.as_str() {
        "git-subdir" | "url" | "git" => {
            let url = str_field(v, "url").ok_or("git source missing 'url'")?;
            Ok(PluginSource::Git {
                url,
                path: str_field(v, "path"),
                git_ref: str_field(v, "ref"),
            })
        }
        "github" => {
            let repo = str_field(v, "repo").ok_or("github source missing 'repo'")?;
            Ok(PluginSource::Git {
                url: format!("https://github.com/{repo}.git"),
                path: str_field(v, "path"),
                // `commit` is github-source's pin; `ref` is accepted as a synonym.
                git_ref: str_field(v, "commit").or_else(|| str_field(v, "ref")),
            })
        }
        other => Err(format!("unsupported source kind '{other}'")),
    }
}
```

Note on `sha`: deliberately not read. It is an integrity digest over a packaging horsie does not reproduce, so honouring it would claim a verification we do not perform. `ref`/`commit` carry the pinning.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-support marketplace`
Expected: PASS (8 tests)

- [ ] **Step 5: Commit**

```bash
git add support/src/plugin/
git commit -m "feat(support): parse marketplace.json and normalise source forms"
```

---

### Task 5: `git` module behind the `git` feature

**Files:**
- Create: `support/src/git.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn clone(url: &str, git_ref: Option<&str>, dest: &Path) -> Result<(), String>;
  pub fn pull_ff_only(dir: &Path) -> Result<(), String>;
  pub fn head_sha(dir: &Path) -> Option<String>;
  ```

- [ ] **Step 1: Write the failing tests**

Create `support/src/git.rs`:

```rust
//! Thin wrappers over the `git` binary. Behind the `git` feature: only the CLI
//! and server clone: the runtime reads already-materialised trees.

use std::path::Path;
use std::process::Command;

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

    #[test]
    fn clone_failure_reports_stderr() {
        let dst = TempDir::new().unwrap();
        let err = clone(
            "file:///definitely/not/a/repo",
            None,
            &dst.path().join("c"),
        )
        .unwrap_err();
        assert!(err.contains("git clone failed"), "err: {err}");
    }

    #[test]
    fn head_sha_of_a_non_repo_is_none() {
        let dir = TempDir::new().unwrap();
        assert!(head_sha(dir.path()).is_none());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-support --features git git`
Expected: FAIL — `clone` not found.

- [ ] **Step 3: Implement**

Insert above the `#[cfg(test)]` block:

```rust
/// Shallow-clone `url` into `dest`, optionally at `git_ref`.
pub fn clone(url: &str, git_ref: Option<&str>, dest: &Path) -> Result<(), String> {
    let dest_str = dest.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["clone", "--depth", "1"];
    if let Some(r) = git_ref {
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

/// `git pull --ff-only` in an existing clone.
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
```

Note: a `--depth 1` clone cannot always fast-forward. `pull_ff_only` is used by `plugin update` on clones created here; if it fails, the caller reports the git error rather than silently re-cloning.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-support --features git`
Expected: PASS (all crate tests, including 4 git tests)

- [ ] **Step 5: Commit**

```bash
git add support/src/git.rs support/src/lib.rs
git commit -m "feat(support): git clone/pull helpers behind the git feature"
```

---

### Task 6: Rewire the runtime loader onto `horsie-support`

**Files:**
- Modify: `runtime/Cargo.toml` (add `horsie-support`)
- Modify: `runtime/src/plugins.rs` (delete `read_manifest`, `plugin_name`, `skills_locations`; rewrite `discover_skills`)

**Interfaces:**
- Consumes: `PluginRoot::inspect` (Task 3).
- Produces: no signature change. `discover_skills(plugins_dir) -> Vec<PluginSkill>` keeps its contract, including `rel_dir` being relative to `plugins_dir`.

- [ ] **Step 1: Add a symlink regression test**

The new CLI layout symlinks `plugins_dir/<name>` at a plugin root elsewhere on disk. Nothing in discovery may canonicalise, or `rel_dir` would leak the target. Add to `runtime/src/plugins.rs` `mod tests`:

```rust
    /// The CLI installs plugins as symlinks into a shared clone. Discovery must
    /// follow the link but keep `rel_dir` relative to the library root.
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
```

- [ ] **Step 2: Run it to verify it passes already**

Run: `cargo test -p horsie-runtime plugins::tests::discovers_skills_through_a_symlinked_plugin_dir`
Expected: PASS — this pins existing behaviour that the rewrite must not break. If it fails, stop: the layout assumption is wrong.

- [ ] **Step 3: Add the dependency**

In `runtime/Cargo.toml`, under `[dependencies]`, after the `horsie-models` line:

```toml
horsie-support = { version = "0.1.6", path = "../support" }
```

- [ ] **Step 4: Rewrite `discover_skills` and delete the duplicates**

In `runtime/src/plugins.rs`, delete `read_manifest`, `plugin_name` and `skills_locations` entirely, drop the now-unused `use serde_json::Value;` if nothing else needs it (`session_start_commands` still does — keep it), and replace `discover_skills` with:

```rust
/// Enumerate every installed plugin's skills. `rel_dir` is each skill's directory
/// relative to `plugins_dir` so the agent can read sibling resources via the
/// filesystem tools against `horsie_shared`.
pub fn discover_skills(plugins_dir: &Path) -> Vec<PluginSkill> {
    let mut out = Vec::new();
    for plugin_root in plugin_dirs(plugins_dir) {
        // Best-effort: a plugin with a malformed manifest contributes nothing
        // rather than failing the whole scan.
        let Ok(root) = horsie_support::plugin::PluginRoot::inspect(&plugin_root) else {
            tracing::warn!(plugin = %plugin_root.display(), "skipping plugin with unreadable manifest");
            continue;
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
```

- [ ] **Step 5: Run the runtime tests**

Run: `cargo test -p horsie-runtime plugins`
Expected: PASS — all pre-existing tests (`discovers_default_skills_dir`, `manifest_name_and_skills_override`, `skills_array_override`, `empty_or_missing_dir_is_empty`, hook tests) unmodified, plus the new symlink test. These passing unmodified is the evidence that removing the duplicate parser changed no behaviour.

- [ ] **Step 6: Commit**

```bash
git add runtime/Cargo.toml runtime/src/plugins.rs Cargo.lock
git commit -m "refactor(runtime): use horsie-support for plugin manifest and skills"
```

---

### Task 7: Rewire the server bundle ingest onto `horsie-support`

**Files:**
- Modify: `server/Cargo.toml` (add `horsie-support` with the `git` feature)
- Modify: `server/src/plugins/ingest.rs` (replace `inspect_plugin_dir`, `git_head_sha`)

**Interfaces:**
- Consumes: `PluginRoot::inspect` (Task 3), `git::head_sha` (Task 5).
- Produces: `ingest_git` keeps its signature and its `Ingested` fields.

- [ ] **Step 1: Add the dependency**

In `server/Cargo.toml`, under `[dependencies]`, after the `horsie-models` line:

```toml
horsie-support         = { path = "../support", features = ["git"] }
```

- [ ] **Step 2: Replace `inspect_plugin_dir` and `git_head_sha`**

In `server/src/plugins/ingest.rs`, delete `struct PluginInfo`, `fn inspect_plugin_dir`, and `fn git_head_sha`. In `ingest_git`, replace the block from `let info = inspect_plugin_dir(&dest)?;` through the `Ok(Ingested { … })` with:

```rust
    let root = horsie_support::plugin::PluginRoot::inspect(&dest)?;
    if !root.is_installable() {
        return Err(format!("not a plugin bundle: {}", root.rejection()));
    }
    let name = root.name(&repo_basename(url));
    let version = root
        .version()
        .map(str::to_string)
        .or_else(|| horsie_support::git::head_sha(&dest));
    let description = root.description().map(str::to_string);
    let skill_count = u32::try_from(root.skill_dirs.len()).unwrap_or(u32::MAX);
    let has_hooks = dest.join("hooks").join("hooks.json").is_file();
    let zip_bytes = zip_dir(&dest)?;
    let hash = sha256_hex(&zip_bytes);
    Ok(Ingested {
        name,
        version,
        description,
        skill_count,
        has_hooks,
        zip_bytes,
        hash,
    })
```

Behaviour note: `has_hooks` changes from "the file contains the substring `SessionStart`" to "a `hooks/hooks.json` exists". The old check reported `false` for every plugin whose hooks are `PreToolUse`-only, which is wrong for a field the UI renders as a generic "hooks" badge. Adjust the existing `inspect_*` unit tests in this file that assert on `has_hooks` to match, and keep a test proving a plugin with a `PreToolUse`-only manifest now reports `true`.

- [ ] **Step 3: Run the server plugin tests**

Run: `cargo test -p horsie-server plugins`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add server/Cargo.toml server/src/plugins/ingest.rs Cargo.lock
git commit -m "refactor(server): use horsie-support for bundle inspection"
```

---

### Task 8: CLI — sources + symlink layout, manifest-aware gate

**Files:**
- Modify: `cli/Cargo.toml` (add `horsie-support` with `git`)
- Modify: `cli/src/plugins.rs` (`install`, `update`, `remove`, delete `has_skills`/`name_from_url` duplication of git)
- Modify: `cli/src/main.rs` (`resolve_plugins_dir` → also resolve `sources_dir`)

**Interfaces:**
- Consumes: `PluginRoot` (Task 3), `source_key` (Task 1), `git` (Task 5).
- Produces:
  ```rust
  pub struct PluginPaths { pub plugins: PathBuf, pub sources: PathBuf }
  pub fn install(paths: &PluginPaths, url: &str, name: Option<String>,
                 git_ref: Option<String>, force: bool) -> Result<String, CliError>;
  pub fn update(paths: &PluginPaths, name: &str) -> Result<(), CliError>;
  pub fn remove(paths: &PluginPaths, name: &str) -> Result<(), CliError>;
  ```
  `PluginEntry` gains `source_key: Option<String>` so `update`/`remove` can find the backing clone.

- [ ] **Step 1: Write the failing tests**

Add to `cli/src/plugins.rs` `mod tests` (reuse the `fixture_repo` helper pattern from Task 5, defined locally here):

```rust
    fn git_run(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A repo whose plugin root is a subdirectory declared by a marketplace,
    /// with a manifest pointing skills outside the default location — the
    /// impeccable shape.
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
        git_run(dir, &["init", "-q", "-b", "main"]);
        git_run(dir, &["config", "user.email", "t@example.com"]);
        git_run(dir, &["config", "user.name", "t"]);
        git_run(dir, &["add", "-A"]);
        git_run(dir, &["commit", "-qm", "init"]);
    }

    fn paths(root: &Path) -> PluginPaths {
        PluginPaths {
            plugins: root.join("plugins"),
            sources: root.join("sources"),
        }
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
            &format!("file://{}", src.path().display()),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(name, "impeccable");

        let link = p.plugins.join("impeccable");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(link.join("skills/impeccable/SKILL.md").is_file());

        // The lockfile records the manifest name, version and the backing clone.
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
        let url = format!("file://{}", src.path().display());
        install(&p, &url, None, None, false).unwrap();

        update(&p, "impeccable").unwrap();
        assert!(p.plugins.join("impeccable/skills/impeccable/SKILL.md").is_file());

        remove(&p, "impeccable").unwrap();
        assert!(!p.plugins.join("impeccable").exists());
        assert!(list(&p).is_empty());
        // The shared clone is garbage-collected once nothing points at it.
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
        git_run(src.path(), &["init", "-q", "-b", "main"]);
        git_run(src.path(), &["config", "user.email", "t@example.com"]);
        git_run(src.path(), &["config", "user.name", "t"]);
        git_run(src.path(), &["add", "-A"]);
        git_run(src.path(), &["commit", "-qm", "init"]);

        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        let err = install(
            &p,
            &format!("file://{}", src.path().display()),
            None,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("SKILL.md"), "err: {err}");
        assert!(err.contains("skills"), "error must name where it looked: {err}");
        // A rejected install leaves nothing behind.
        assert!(!p.plugins.join("s").exists());
    }

    #[test]
    fn duplicate_install_needs_force() {
        let src = TempDir::new().unwrap();
        impeccable_fixture(src.path());
        let home = TempDir::new().unwrap();
        let p = paths(home.path());
        let url = format!("file://{}", src.path().display());
        install(&p, &url, None, None, false).unwrap();
        assert!(install(&p, &url, None, None, false).is_err());
        install(&p, &url, None, None, true).unwrap();
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie plugins`
Expected: FAIL — `PluginPaths` not found.

- [ ] **Step 3: Add the dependency**

In `cli/Cargo.toml`, under `[dependencies]`, after the `horsie-models` line:

```toml
horsie-support = { version = "0.1.6", path = "../support", features = ["git"] }
```

- [ ] **Step 4: Implement the new layout**

In `cli/src/plugins.rs`:

1. Add to `PluginEntry`: `#[serde(default)] pub source_key: Option<String>,`
2. Add:

```rust
/// The two directories the plugin library spans: the symlink farm the runtime
/// reads, and the clones those links point into.
#[derive(Debug, Clone)]
pub struct PluginPaths {
    /// `storage.plugins_dir` — one symlink per installed plugin.
    pub plugins: PathBuf,
    /// `<data_dir>/sources` — one clone per `(url, ref)`, shared by every
    /// plugin resolved out of it.
    pub sources: PathBuf,
}
```

3. Replace `install` with:

```rust
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
        horsie_support::git::clone(url, git_ref.as_deref(), &clone_dir)
            .map_err(CliError::Executor)?;
    }

    let (root_dir, entry_name) = resolve_plugin_root(&clone_dir, url)?;
    let root = horsie_support::plugin::PluginRoot::inspect(&root_dir)
        .map_err(CliError::Config)?;
    if !root.is_installable() {
        gc_clone(paths, &key);
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
        sha: horsie_support::git::head_sha(&link),
        source_key: Some(key),
    });
    save_lock(&paths.plugins, &lock)?;
    Ok(install_name)
}

/// The plugin root inside a clone: the marketplace-declared entry when the repo
/// is a marketplace, else the repo root. Returns the entry name when a
/// marketplace named it.
fn resolve_plugin_root(
    clone_dir: &Path,
    url: &str,
) -> Result<(PathBuf, Option<String>), CliError> {
    let market = horsie_support::plugin::Marketplace::read(clone_dir)
        .map_err(CliError::Config)?;
    let Some(market) = market else {
        return Ok((clone_dir.to_path_buf(), None));
    };
    for why in &market.skipped {
        tracing::warn!(marketplace = %url, "skipping unreadable marketplace {why}");
    }
    match market.plugins.as_slice() {
        [only] => match &only.source {
            horsie_support::plugin::PluginSource::Path(p) => {
                Ok((clone_dir.join(p), Some(only.name.clone())))
            }
            // External sources need the marketplace registry; PR2 adds it.
            horsie_support::plugin::PluginSource::Git { url: u, .. } => {
                Err(CliError::Config(format!(
                    "'{}' is published from another repo ({u}); install it from there directly",
                    only.name
                )))
            }
        },
        [] => Ok((clone_dir.to_path_buf(), None)),
        many => Err(CliError::Config(format!(
            "'{url}' is a marketplace listing {} plugins; \
             install one directly by its own repo URL. Available: {}",
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

/// Remove an installed plugin's library entry, whether it is a symlink (new
/// layout) or a real directory (installed before this change).
fn remove_link(link: &Path) -> Result<(), CliError> {
    let meta = match link.symlink_metadata() {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    let r = if meta.file_type().is_symlink() {
        std::fs::remove_file(link)
    } else {
        std::fs::remove_dir_all(link)
    };
    r.map_err(|e| CliError::Io(e.to_string()))
}

/// Delete a clone once no lockfile entry references it.
fn gc_clone(paths: &PluginPaths, key: &str) {
    let still_used = load_lock(&paths.plugins)
        .plugins
        .iter()
        .any(|p| p.source_key.as_deref() == Some(key));
    if !still_used {
        let _ = std::fs::remove_dir_all(paths.sources.join(key));
    }
}
```

4. Replace `update`:

```rust
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
    if let Some(e) = lock.plugins.iter_mut().find(|p| p.name == name) {
        e.sha = horsie_support::git::head_sha(&link);
        if let Some(v) = root.version() {
            e.version = Some(v.to_string());
        }
    }
    save_lock(&paths.plugins, &lock)
}
```

5. Replace `remove`:

```rust
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
        gc_clone(paths, &k);
    }
    Ok(())
}
```

6. Update `list` to take `&PluginPaths` (`load_lock(&paths.plugins)`), and delete `has_skills` and its test.

- [ ] **Step 5: Update the call sites in `cli/src/main.rs`**

Replace `resolve_plugins_dir` with:

```rust
/// Resolve the plugin library paths from config: the symlink farm
/// (`storage.plugins_dir`) and the shared clones (`<data_dir>/sources`).
fn resolve_plugin_paths(config: Option<&Path>) -> Result<horsie::plugins::PluginPaths, CliError> {
    let cfg = HorsieConfig::resolve(config)?;
    Ok(horsie::plugins::PluginPaths {
        plugins: cfg.storage.plugins_dir,
        sources: cfg.storage.data_dir.join("sources"),
    })
}
```

and update the four `PluginAction` arms to build `let paths = resolve_plugin_paths(config.as_deref())?;` and pass `&paths`, printing `paths.plugins.display()` in the install message.

- [ ] **Step 6: Run the CLI tests**

Run: `cargo test -p horsie plugins`
Expected: PASS (4 new tests plus the retained lockfile/`library_for_runtime` tests)

- [ ] **Step 7: Commit**

```bash
git add cli/ Cargo.lock
git commit -m "feat(cli): resolve plugin roots via manifest and marketplace, install as symlinks"
```

---

### Task 9: Sandbox grants for the symlink targets

**Files:**
- Create: `support/src/plugin/grants.rs`
- Modify: `support/src/plugin/mod.rs` (add `pub mod grants;`)
- Modify: `cli/src/capabilities.rs` (`with_plugin_grants` delegates)
- Modify: `cli/src/daemon/mod.rs:188` (pass the sources dir)
- Modify: `runtime-vendor/Cargo.toml`, `runtime-vendor/src/vendor.rs` (inject host-library grants)

**Interfaces:**
- Produces:
  ```rust
  pub fn plugin_library_grants(
      plugins_dir: Option<&Path>,
      extra_roots: &[PathBuf],
      hook_path: &[PathBuf],
  ) -> Vec<Grant>;
  ```
  Returns read-only `Dir` grants. `extra_roots` carries the clone roots that symlinks point into.

- [ ] **Step 1: Write the failing test**

Create `support/src/plugin/grants.rs`:

```rust
//! Sandbox grants for the shared plugin library.
//!
//! Installed plugins are symlinks into clones elsewhere on disk, and both
//! Landlock and Seatbelt resolve through symlinks — so the *targets* must be
//! granted too, not just the library root.

use horsie_models::capabilities::{Access, DirGrant, Grant};
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

    fn paths(grants: &[Grant]) -> Vec<String> {
        grants
            .iter()
            .filter_map(|g| match g {
                Grant::Dir(d) => Some(d.path.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn no_library_yields_no_grants() {
        assert!(plugin_library_grants(None, &[PathBuf::from("/s")], &[]).is_empty());
    }

    #[test]
    fn grants_library_sources_and_hook_dirs() {
        let g = plugin_library_grants(
            Some(Path::new("/d/plugins")),
            &[PathBuf::from("/d/sources")],
            &[PathBuf::from("/opt/node/bin")],
        );
        assert_eq!(paths(&g), vec!["/d/plugins", "/d/sources", "/opt/node/bin"]);
        assert!(
            g.iter().all(|x| matches!(x, Grant::Dir(d) if d.access == Access::Read)),
            "plugin grants must be read-only"
        );
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-support grants`
Expected: FAIL — `plugin_library_grants` not found.

- [ ] **Step 3: Implement**

Insert above the `#[cfg(test)]` block:

```rust
/// Read-only `Dir` grants so a sandboxed runtime can read plugin skills and
/// resources and execute hooks. Empty when there is no library.
pub fn plugin_library_grants(
    plugins_dir: Option<&Path>,
    extra_roots: &[PathBuf],
    hook_path: &[PathBuf],
) -> Vec<Grant> {
    let Some(dir) = plugins_dir else {
        return Vec::new();
    };
    let read = |p: &Path| {
        Grant::Dir(DirGrant {
            path: p.to_string_lossy().into_owned(),
            access: Access::Read,
        })
    };
    let mut out = vec![read(dir)];
    out.extend(extra_roots.iter().map(|p| read(p)));
    out.extend(hook_path.iter().map(|p| read(p)));
    out
}
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -p horsie-support grants`
Expected: PASS (2 tests)

- [ ] **Step 5: Delegate from the CLI**

In `cli/src/capabilities.rs`, replace the body of `with_plugin_grants` and widen its signature:

```rust
/// Append read-only `Dir` grants for the shared plugin library, the clone roots
/// its symlinks point into, and the hook interpreter dirs. A no-op when
/// `plugins_dir` is `None`.
pub fn with_plugin_grants(
    mut spec: CapabilitySpec,
    plugins_dir: Option<&Path>,
    sources_dir: Option<&Path>,
    hook_path: &[PathBuf],
) -> CapabilitySpec {
    let extra: Vec<PathBuf> = sources_dir.into_iter().map(Path::to_path_buf).collect();
    spec.grants.extend(horsie_support::plugin::grants::plugin_library_grants(
        plugins_dir,
        &extra,
        hook_path,
    ));
    spec
}
```

Remove the now-unused `Access`, `DirGrant`, `Grant` imports if nothing else in the file uses them, and update the existing `with_plugin_grants` unit tests to pass the extra argument.

- [ ] **Step 6: Update the daemon call site**

In `cli/src/daemon/mod.rs`, thread the sources dir through to line ~188 so the call becomes `capabilities::with_plugin_grants(spec, plugins_dir.as_deref(), Some(&sources_dir), &hook_path)`, where `sources_dir` comes from the same config as `plugins_dir` (`cfg.storage.data_dir.join("sources")`) alongside the existing `library_for_runtime` call at line 128.

- [ ] **Step 7: Fix the `connect` path**

`horsie connect` never applied plugin grants at all: `runtime-vendor/src/vendor.rs:495` writes the server's capability spec verbatim, while `host_library` only populates `RuntimeConfig.plugins_dir` (`vendor.rs:531`). A sandboxed local runtime is therefore told to read a directory it has no capability for.

In `runtime-vendor/Cargo.toml` add:

```toml
horsie-support = { version = "0.1.6", path = "../support" }
```

In `runtime-vendor/src/vendor.rs`, add a `host_sources: Option<PathBuf>` field beside `host_library` (defaulting to `None`, set by a widened `with_host_library(dir, sources, hook_path)`), and in the `(Some(spec), true)` arm of the `caps_file` match, augment the spec before writing:

```rust
            (Some(spec), true) => {
                let mut spec = spec.clone();
                spec.grants.extend(horsie_support::plugin::grants::plugin_library_grants(
                    self.host_library.as_deref(),
                    &self.host_sources.iter().cloned().collect::<Vec<_>>(),
                    &self.hook_path,
                ));
                let path = self.caps_path(runtime_id);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("create runtime state dir: {e}"))?;
                }
                let bytes = serde_json::to_vec_pretty(&spec)
                    .map_err(|e| format!("encode capability spec: {e}"))?;
                std::fs::write(&path, bytes).map_err(|e| format!("write capability file: {e}"))?;
                Some(path)
            }
```

Update `cli/src/connect.rs:214` and `cli/src/main.rs:609` to pass the sources dir into `with_host_library` / `PluginLibrary`.

- [ ] **Step 8: Add a regression test for the connect path**

In `runtime-vendor/src/vendor.rs` `mod tests`, add a test that builds a `RuntimeVendor` with `with_host_library`, drives the caps-file write for a sandboxed create, reads the written JSON back, and asserts it contains `Dir` grants for both the library and the sources dir. Expected before the fix: FAIL (no plugin grants present).

- [ ] **Step 9: Run the affected tests**

Run: `cargo test -p horsie-support -p horsie -p horsie-runtime-vendor`
Expected: PASS

- [ ] **Step 10: Commit**

```bash
git add support/ cli/ runtime-vendor/ Cargo.lock
git commit -m "fix(sandbox): grant the plugin library and its clone roots, including on the connect path"
```

---

### Task 10: Docs, full gates, and PR

**Files:**
- Modify: `docs/guide/skills-and-plugins.md`

- [ ] **Step 1: Update the guide**

In the "Skills on your own machine (host library)" section, after the `horsie plugin install <git-url>` bullet, add:

```markdown
horsie reads the repository's plugin packaging to find the skills: a
`.claude-plugin/plugin.json` `skills` field when present (otherwise the
conventional `skills/` directory), and a `.claude-plugin/marketplace.json` when
the repository publishes its plugin from a subdirectory. Installed plugins are
symlinks into a shared clone under `<data-dir>/sources`, so `horsie plugin
update` is a fast-forward pull rather than a re-clone.

A repository whose marketplace lists several plugins cannot be installed by URL
alone — install the plugin from its own repository instead.
```

- [ ] **Step 2: Run the full gates in order**

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

Expected: clean. Fix anything that fails before proceeding — do not open a PR on red.

- [ ] **Step 3: Verify against the real repository that motivated this**

```bash
cargo run -p horsie -- plugin install https://github.com/pbakaus/impeccable
cargo run -p horsie -- plugin list
```

Expected: installs as `impeccable` at version `4.0.4`; `plugin list` shows it. Then `cargo run -p horsie -- plugin remove impeccable` and confirm both the symlink and the clone are gone.

- [ ] **Step 4: Commit and push**

```bash
git add docs/guide/skills-and-plugins.md
git commit -m "docs: manifest-aware plugin install and the shared-clone layout"
git push -u origin feat/plugin-manifest-resolver
```

- [ ] **Step 5: Open the PR**

Body: one long line per paragraph or bullet (GitHub renders newlines as literal breaks). State what changed and why, reference issue #105 Phase 0 and the spec path, note the `has_hooks` semantic change, and note that `horsie-support` needs a one-time crates.io trusted-publishing configuration before the next `v*` tag.

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
| --- | --- |
| `horsie-support` crate, module layout, `git` feature | 1, 5 |
| Manifest model | 1 |
| Marketplace model, four source forms, `sha` ignored | 4 |
| Resolution, ambiguous-repo error, skipped entries | 4, 8 |
| On-disk layout, `<key>` hashing, GC | 1 (`source_key`), 8 |
| Symlinks preserve `git pull` and `current_sha` | 8 |
| Sandbox grants incl. the `connect` bug | 9 |
| CLI surface (install/update/remove) | 8 |
| Error handling | 3 (`rejection`), 4, 8 |
| Testing incl. impeccable regression fixture | 2, 3, 6, 8 |
| Consequences: publishing chore | 10 (PR body) |

Deferred to PR2/PR3 by design and **not** covered here: `horsie marketplace add/list/update/remove`, `plugin install <plugin>@<marketplace>` and its name-vs-SSH-URL disambiguation rule, external `Git` source resolution (Task 8 errors with a pointer instead), the marketplaces lockfile and clone root, the server/web surface. The `marketplaces/` sandbox grant arrives with the marketplaces directory in PR2.

**Type consistency:** `PluginRoot::inspect` → `Result<PluginRoot, String>` is consumed as such in Tasks 6, 7, 8. `skill_dirs` is a field on `PluginRoot` and a free function in `skills`; both are used with the right form. `PluginPaths` is introduced in Task 8 and used in its own tests and `main.rs` only. `plugin_library_grants` has one signature, used identically in `cli` and `runtime-vendor`. `source_key` is defined in Task 1 and used in Task 8.

**Placeholders:** none — every step carries the code or the exact command it needs.
