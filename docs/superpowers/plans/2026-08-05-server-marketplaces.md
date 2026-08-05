# Server-side marketplaces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the server resolve a pasted git URL the way the CLI already does — bundle, or catalogue — and give the web UI one install box plus a marketplace picker.

**Architecture:** `server/src/plugins/ingest.rs` grows a target enum (`Url` vs `Resolved`) and returns `Ingested::{Plugin, Marketplace}`; a new `marketplaces` table caches each source's parsed index; `PluginService::install` returns an `InstallOutcome` union; Settings → Skills gains a Marketplaces section between the install box and the bundle list.

**Tech Stack:** Rust (axum, sqlx `Any`), fluorite IDL codegen (Rust + TypeScript), React + TanStack Query + Tailwind, Vitest, Playwright.

## Global Constraints

- **No backward compatibility.** Reshape wire types, DB rows and function signatures freely; there is no deployed data to migrate.
- **Illegal states unrepresentable.** No boolean flags standing in for a mode; use enums. The one deliberate exception is `PluginInstallInput`'s four optional fields, which the spec records as a decision.
- **fluorite traps** (all hit for real in #206):
  - fluorite **warns and exits 0** on a `.fl` parse failure — check the generated file actually changed, never trust "Code generation complete!".
  - fluorite **rejects `///` doc comments on union *variants***. Use `//` inside union bodies. `///` is fine on structs, on the union itself, and on struct fields.
  - `models/build.rs` needs a `touch` to pick up a **new** `.fl` file (not needed here — `plugins.fl` already exists).
  - Generated TS optionals are `?: T | undefined`, **never** `| null`.
- **clippy runs with `-D warnings` and `wildcard_enum_match_arm` denied.** No `_ =>` over any enum defined in this repo.
- **Verification cost:** iterate with `cargo test -p horsie-server --lib`, run the full workspace suite once before pushing, never twice in one command.
- **Worktree:** all work happens in `.horsie/worktrees/server-marketplaces` on branch `feat/server-marketplaces`.
- **Authorship:** no Claude/AI attribution in commits, PR body, or issues. Never pass `-c user.name` / `-c user.email` to git.

## File Structure

| File | Responsibility |
| --- | --- |
| `models/fluorite/plugins.fl` | wire types: `PluginInstallInput` (4 optionals), `PluginView` + `marketplace`, `MarketplacePluginView`, `MarketplaceView`, `InstallOutcome` union |
| `server/src/plugins/ingest.rs` | clone → classify → pack. `IngestTarget`, `PluginBundle`, `ParsedMarketplace`, `Ingested`, `ingest_git`, `read_marketplace` |
| `server/migrations/{sqlite,postgres}/0022_marketplaces.sql` | `marketplaces` table + three columns on `plugins` |
| `server/src/plugins/store.rs` | `PluginRow` gains `source_subpath`/`marketplace`/`marketplace_entry`; `installed_entries()` |
| `server/src/plugins/marketplace_store.rs` (new) | `MarketplaceRow` + `MarketplaceStore` — the cached index, JSON in / JSON out |
| `server/src/plugins/service.rs` | `install` → `InstallOutcome`, `update` re-resolves, marketplace list/refresh/remove |
| `server/src/http/marketplaces.rs` (new) | three routes |
| `server/src/http/plugins.rs` | `install` returns the union |
| `clients/web/src/api/client.ts` | `api.marketplaces` + install's new return type |
| `clients/web/src/hooks/usePlugins.ts` | `useMarketplaces`, `useRefreshMarketplace`, `useRemoveMarketplace` |
| `clients/web/src/pages/settings/skills/MarketplaceRow.tsx` (new) | one source: header, filter, entry list, install/refresh/remove |
| `clients/web/src/pages/settings/skills/BundleRow.tsx` (new) | moved out of `SkillsSettings.tsx` unchanged, plus the marketplace chip |
| `clients/web/src/pages/settings/SkillsSettings.tsx` | composition of three sections |
| `clients/web/e2e/{global-setup.ts,harness.ts,u-marketplace.spec.ts}` | a `file://` marketplace fixture and the end-to-end flow |

---

### Task 1: Ingest resolves through a marketplace index

The parity fix. `pbakaus/impeccable` — a repo whose `marketplace.json` declares one plugin at `./plugin` — currently fails to install from the server and succeeds from the CLI. After this task it succeeds from both.

**Files:**
- Modify: `server/src/plugins/ingest.rs` (whole file — types, `ingest_git`, tests)
- Modify: `server/src/plugins/service.rs:44-62,97-132,177-193` (call sites only, to keep the crate compiling)

**Interfaces:**
- Consumes: `horsie_support::plugin::{Marketplace, MarketplaceEntry, PluginRoot, PluginSource, join_declared, source_location}`, `horsie_support::git::head_sha`
- Produces:
  - `pub enum IngestTarget { Url { url: String, git_ref: Option<String> }, Resolved { url: String, git_ref: Option<String>, subpath: Option<String> } }`
  - `pub struct PluginBundle { name, version, description, skill_count, has_hooks, unsupported_hooks, zip_bytes, hash, url, git_ref, subpath }`
  - `pub struct ParsedMarketplace { name: String, url: String, git_ref: Option<String>, sha: Option<String>, entries: Vec<MarketplaceEntry>, skipped: Vec<String> }`
  - `pub enum Ingested { Plugin(PluginBundle), Marketplace(ParsedMarketplace) }`
  - `pub fn ingest_git(target: &IngestTarget) -> Result<Ingested, String>`
  - `pub fn read_marketplace(url: &str, git_ref: Option<&str>) -> Result<ParsedMarketplace, String>`

**Why an enum target rather than the spec's `subpath: Option<String>`:** the spec says `Ingested::Marketplace` is "never returned when `subpath` was given". A `Git` marketplace entry with no `path` resolves to `subpath: None`, so with a flat struct that promise would depend on a field being `None` for two different reasons. The enum makes it structural: only `IngestTarget::Url` can return `Marketplace`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `server/src/plugins/ingest.rs`. Reuse the existing `git()` helper. Add one helper beside it:

```rust
    /// Write `.claude-plugin/marketplace.json` at `root`.
    fn write_marketplace(root: &Path, json: &str) {
        let dir = root.join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marketplace.json"), json).unwrap();
    }

    /// Write a minimal skill-only plugin tree at `dir`.
    fn write_skill(dir: &Path, name: &str) {
        let s = dir.join("skills").join(name);
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("SKILL.md"), format!("---\nname: {name}\n---\nbody")).unwrap();
    }

    fn commit_repo(root: &Path) -> String {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "init"]);
        format!("file://{}", root.display())
    }

    fn url_target(url: &str) -> IngestTarget {
        IngestTarget::Url {
            url: url.to_string(),
            git_ref: None,
        }
    }

    /// `Ingested` holds zip bytes and deliberately isn't `Debug`, so unwrap by
    /// matching rather than with `unwrap`/`unwrap_err`.
    fn expect_plugin(ing: Ingested) -> PluginBundle {
        match ing {
            Ingested::Plugin(p) => p,
            Ingested::Marketplace(m) => panic!("expected a plugin, got marketplace {}", m.name),
        }
    }

    fn expect_marketplace(ing: Ingested) -> ParsedMarketplace {
        match ing {
            Ingested::Marketplace(m) => m,
            Ingested::Plugin(p) => panic!("expected a marketplace, got plugin {}", p.name),
        }
    }
```

The tests:

```rust
    /// THE REGRESSION TEST. `pbakaus/impeccable`'s shape: a marketplace index at
    /// the repo root declaring exactly one plugin at `./plugin`, whose manifest
    /// puts its skills somewhere non-default. This is what the web UI could not
    /// install and the CLI could.
    #[test]
    fn a_marketplace_with_one_entry_installs_that_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("src");
        std::fs::create_dir_all(&repo).unwrap();
        write_marketplace(
            &repo,
            r#"{"name":"impeccable","plugins":[
                 {"name":"impeccable","version":"4.0.4","source":"./plugin"}]}"#,
        );
        let plugin = repo.join("plugin");
        let cp = plugin.join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("plugin.json"),
            r#"{"name":"impeccable","version":"4.0.4","skills":"./.claude/skills/"}"#,
        )
        .unwrap();
        let s = plugin.join(".claude/skills/impeccable");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("SKILL.md"), "---\nname: impeccable\n---\nb").unwrap();
        // A file at the repo root that must NOT end up in the artifact.
        std::fs::write(repo.join("README.md"), "not part of the bundle").unwrap();
        let url = commit_repo(&repo);

        let b = expect_plugin(ingest_git(&url_target(&url)).unwrap());
        assert_eq!(b.name, "impeccable");
        assert_eq!(b.skill_count, 1);
        assert_eq!(b.subpath.as_deref(), Some("./plugin"));
        assert_eq!(b.url, url, "a path entry stays in the marketplace's own repo");
        // The zip is packed from the plugin root, not the repo root.
        let names = zip_entry_names(&b.zip_bytes);
        assert!(
            !names.iter().any(|n| n == "README.md"),
            "packed the repo root, not the plugin root: {names:?}"
        );
    }

    /// Several entries is not an install: it is a catalogue the caller has to
    /// pick from.
    #[test]
    fn a_marketplace_with_several_entries_is_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("src");
        std::fs::create_dir_all(&repo).unwrap();
        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","description":"the first","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        );
        write_skill(&repo.join("plugins/alpha"), "a");
        write_skill(&repo.join("plugins/beta"), "b");
        let url = commit_repo(&repo);

        let m = expect_marketplace(ingest_git(&url_target(&url)).unwrap());
        assert_eq!(m.name, "catalogue");
        assert_eq!(m.entries.len(), 2);
        assert_eq!(m.entries[0].description.as_deref(), Some("the first"));
        assert!(m.sha.is_some(), "the sha is what refresh compares against");
    }

    /// A malformed entry must not brick its siblings — the 276-entry case.
    #[test]
    fn a_malformed_entry_is_skipped_and_named() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("src");
        std::fs::create_dir_all(&repo).unwrap();
        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"broken"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        );
        write_skill(&repo.join("plugins/alpha"), "a");
        write_skill(&repo.join("plugins/beta"), "b");
        let url = commit_repo(&repo);

        let m = expect_marketplace(ingest_git(&url_target(&url)).unwrap());
        assert_eq!(m.entries.len(), 2);
        assert_eq!(m.skipped.len(), 1);
        assert!(m.skipped[0].contains("source"), "{:?}", m.skipped);
    }

    /// A resolved target clones exactly what it was told and never consults an
    /// index — the guarantee that lets a marketplace entry point at a repo that
    /// is itself a marketplace.
    #[test]
    fn a_resolved_target_ignores_the_repos_own_marketplace() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("src");
        std::fs::create_dir_all(&repo).unwrap();
        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        );
        write_skill(&repo.join("plugins/alpha"), "a");
        write_skill(&repo.join("plugins/beta"), "b");
        let url = commit_repo(&repo);

        let b = expect_plugin(
            ingest_git(&IngestTarget::Resolved {
                url: url.clone(),
                git_ref: None,
                subpath: Some("./plugins/beta".into()),
            })
            .unwrap(),
        );
        assert_eq!(b.skill_count, 1);
        assert_eq!(b.subpath.as_deref(), Some("./plugins/beta"));
    }

    /// A one-entry index whose entry points at ANOTHER repo: the second repo is
    /// cloned, and the bundle records that repo as its source so `update`
    /// re-clones the right thing.
    #[test]
    fn a_one_entry_index_can_point_at_another_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let other = tmp.path().join("other");
        write_skill(&other, "x");
        let other_url = commit_repo(&other);

        let repo = tmp.path().join("src");
        std::fs::create_dir_all(&repo).unwrap();
        write_marketplace(
            &repo,
            &format!(
                r#"{{"name":"m","plugins":[{{"name":"x","source":{{"source":"git","url":"{other_url}"}}}}]}}"#
            ),
        );
        let url = commit_repo(&repo);

        let b = expect_plugin(ingest_git(&url_target(&url)).unwrap());
        assert_eq!(b.name, "x");
        assert_eq!(b.url, other_url);
        assert!(b.subpath.is_none());
    }

    /// `read_marketplace` is refresh's entry point: it must refuse a repo that
    /// has no index rather than silently reporting an empty catalogue.
    #[test]
    fn read_marketplace_refuses_a_plain_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("src");
        write_skill(&repo, "a");
        let url = commit_repo(&repo);

        let err = read_marketplace(&url, None).unwrap_err();
        assert!(err.contains("marketplace.json"), "err: {err}");
    }
```

Plus the zip-inspection helper the first test needs, at module scope inside `mod tests`:

```rust
    fn zip_entry_names(bytes: &[u8]) -> Vec<String> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
        (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect()
    }
```

Also **update the existing tests** in that module, which call `ingest_git(&url, None)`: replace each with `expect_plugin(ingest_git(&url_target(&url)).unwrap())`, and in `a_repo_with_no_skills_is_rejected_with_where_it_looked` replace `.err().unwrap()` with `ingest_git(&url_target(&url)).err().unwrap()` (the error type is `String`, so `unwrap_err` still needs `Ingested: Debug` — keep `.err().unwrap()`).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-server --lib plugins::ingest`
Expected: FAIL — `cannot find type IngestTarget`, `cannot find function read_marketplace`.

- [ ] **Step 3: Rewrite the ingest types and flow**

Replace the header of `server/src/plugins/ingest.rs` (everything from the `use` block down to the end of `ingest_git`) with:

```rust
use horsie_support::plugin::{
    Marketplace, MarketplaceEntry, PluginRoot, join_declared, source_location,
};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// What to ingest, and how much freedom ingest has to reinterpret it.
///
/// The split is load-bearing: only a URL a person pasted may turn out to be a
/// catalogue. Anything already resolved — through an index, or from a bundle's
/// remembered source — is cloned and packed verbatim, so an entry pointing at a
/// repo that is itself a marketplace cannot send us round again.
pub enum IngestTarget {
    /// A URL with no interpretation yet: bundle repo, or marketplace.
    Url {
        url: String,
        git_ref: Option<String>,
    },
    /// Clone this, descend into `subpath`, pack what is there.
    Resolved {
        url: String,
        git_ref: Option<String>,
        subpath: Option<String>,
    },
}

impl IngestTarget {
    fn url(&self) -> &str {
        match self {
            IngestTarget::Url { url, .. } | IngestTarget::Resolved { url, .. } => url,
        }
    }

    fn git_ref(&self) -> Option<&str> {
        match self {
            IngestTarget::Url { git_ref, .. } | IngestTarget::Resolved { git_ref, .. } => {
                git_ref.as_deref()
            }
        }
    }
}

/// A packed bundle — everything needed to persist a `plugins` row.
///
/// `url`/`git_ref`/`subpath` are what was *actually* cloned and descended into,
/// which is not always what the caller asked for: a marketplace entry may name
/// another repo. Storing the resolved triple is what makes `update` re-clone the
/// same tree.
pub struct PluginBundle {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub skill_count: u32,
    pub has_hooks: bool,
    /// One sentence per hook event this bundle declares that horsie cannot fire.
    /// Surfaced rather than dropped: a bundle that ingests cleanly and then has
    /// a guard silently never run is the failure the event classification is
    /// there to prevent.
    pub unsupported_hooks: Vec<String>,
    pub zip_bytes: Vec<u8>,
    pub hash: String,
    pub url: String,
    pub git_ref: Option<String>,
    pub subpath: Option<String>,
}

/// A catalogue as read from a checkout — everything a `marketplaces` row stores.
pub struct ParsedMarketplace {
    /// The index's declared `name`, else the repo basename.
    pub name: String,
    pub url: String,
    pub git_ref: Option<String>,
    /// HEAD at the time it was read, so a refresh can report a no-op.
    pub sha: Option<String>,
    pub entries: Vec<MarketplaceEntry>,
    /// Human-readable reasons for entries that could not be understood.
    pub skipped: Vec<String>,
}

/// What a cloned repo turned out to be.
pub enum Ingested {
    Plugin(PluginBundle),
    /// Only reachable from [`IngestTarget::Url`].
    Marketplace(ParsedMarketplace),
}

/// Clone, classify, and pack. Synchronous (shells `git`, walks the fs); callers
/// run it on a blocking task.
pub fn ingest_git(target: &IngestTarget) -> Result<Ingested, String> {
    let url = target.url().trim();
    if url.is_empty() {
        return Err("source_url is required".to_string());
    }
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let dest = tmp.path().join("repo");
    horsie_support::git::clone(url, target.git_ref(), &dest)?;

    let subpath = match target {
        IngestTarget::Resolved { subpath, .. } => subpath.clone(),
        IngestTarget::Url { .. } => match Marketplace::read(&dest)? {
            // Not a catalogue: the repo root is the plugin root, as before.
            None => None,
            Some(m) => match m.plugins.as_slice() {
                // An index that declares nothing is not a catalogue worth
                // recording; fall back to inspecting the repo itself, as the
                // CLI does.
                [] => None,
                [only] => {
                    let (entry_url, entry_ref, sub) =
                        source_location(&only.source, url, target.git_ref());
                    if entry_url != url {
                        // The entry points elsewhere: clone that instead, and
                        // never read *its* index.
                        return ingest_git(&IngestTarget::Resolved {
                            url: entry_url,
                            git_ref: entry_ref,
                            subpath: sub,
                        });
                    }
                    sub
                }
                many => {
                    let _ = many;
                    return Ok(Ingested::Marketplace(ParsedMarketplace {
                        name: m.name.clone().unwrap_or_else(|| repo_basename(url)),
                        url: url.to_string(),
                        git_ref: target.git_ref().map(str::to_string),
                        sha: horsie_support::git::head_sha(&dest),
                        entries: m.plugins,
                        skipped: m.skipped,
                    }));
                }
            },
        },
    };

    let plugin_root = match subpath.as_deref() {
        Some(s) => join_declared(&dest, s),
        None => dest.clone(),
    };
    let bundle = pack(
        &dest,
        &plugin_root,
        url,
        target.git_ref(),
        subpath,
        &repo_basename(url),
    )?;
    Ok(Ingested::Plugin(bundle))
}

/// Clone `url` and parse its marketplace index. Used by refresh, which must not
/// silently accept a source that has stopped being a catalogue.
pub fn read_marketplace(url: &str, git_ref: Option<&str>) -> Result<ParsedMarketplace, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("source_url is required".to_string());
    }
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let dest = tmp.path().join("repo");
    horsie_support::git::clone(url, git_ref, &dest)?;
    let m = Marketplace::read(&dest)?.ok_or_else(|| {
        format!("'{url}' is not a marketplace: it has no .claude-plugin/marketplace.json")
    })?;
    Ok(ParsedMarketplace {
        name: m.name.clone().unwrap_or_else(|| repo_basename(url)),
        url: url.to_string(),
        git_ref: git_ref.map(str::to_string),
        sha: horsie_support::git::head_sha(&dest),
        entries: m.plugins,
        skipped: m.skipped,
    })
}

/// Inspect a plugin root and pack it. `checkout` is the clone's root — the sha
/// fallback for a version comes from there, not from the plugin subtree.
fn pack(
    checkout: &Path,
    plugin_root: &Path,
    url: &str,
    git_ref: Option<&str>,
    subpath: Option<String>,
    fallback_name: &str,
) -> Result<PluginBundle, String> {
    let root = PluginRoot::inspect(plugin_root)?;
    if !root.is_installable() {
        return Err(format!("not a plugin bundle: {}", root.rejection()));
    }
    let name = root.name(fallback_name);
    let version = root
        .version()
        .map(str::to_string)
        .or_else(|| horsie_support::git::head_sha(checkout));
    let description = root.description().map(str::to_string);
    let skill_count = u32::try_from(root.skill_dirs.len()).unwrap_or(u32::MAX);
    let has_hooks = plugin_root.join("hooks").join("hooks.json").is_file();
    let unsupported_hooks = horsie_support::plugin::hooks::read(plugin_root)
        .map(|h| {
            h.unsupported
                .iter()
                .map(|(name, why)| why.explain(name))
                .collect()
        })
        .unwrap_or_default();
    let zip_bytes = zip_dir(plugin_root)?;
    let hash = sha256_hex(&zip_bytes);
    Ok(PluginBundle {
        name,
        version,
        description,
        skill_count,
        has_hooks,
        unsupported_hooks,
        zip_bytes,
        hash,
        url: url.to_string(),
        git_ref: git_ref.map(str::to_string),
        subpath,
    })
}
```

Notes for the implementer:
- `zip_dir`, `collect_files`, `sha256_hex`, `repo_basename` stay exactly as they are.
- `.git` is excluded by `collect_files` already; a subpath root never contains one.
- The `many => { let _ = many; ... }` binding exists only so the arm reads as "several"; if clippy objects, match on `_entries` via a slice pattern guard instead — do **not** reach for a wildcard over a repo enum (none is matched here, so `_` on a slice is fine).

- [ ] **Step 4: Fix the call sites in `service.rs` so the crate compiles**

Minimal, temporary — Task 4 rewrites these properly. In `clone_and_pack`, change the body to build an `IngestTarget::Url` and unwrap the `Plugin` arm with an error for the other:

```rust
async fn clone_and_pack(url: String, git_ref: Option<String>) -> Result<PluginBundle, String> {
    let target = ingest::IngestTarget::Url { url, git_ref };
    let ingested = tokio::task::spawn_blocking(move || ingest::ingest_git(&target))
        .await
        .map_err(|e| e.to_string())??;
    match ingested {
        ingest::Ingested::Plugin(b) => {
            for reason in &b.unsupported_hooks {
                tracing::warn!(
                    plugin = b.name,
                    reason,
                    "plugin declares a hook horsie cannot run"
                );
            }
            Ok(b)
        }
        ingest::Ingested::Marketplace(m) => Err(format!(
            "'{}' is a marketplace listing {} plugins",
            m.url,
            m.entries.len()
        )),
    }
}
```

and change `persist`'s `ing: Ingested` parameter to `ing: PluginBundle`, plus the import to `use super::ingest::{self, PluginBundle};`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie-server --lib plugins::`
Expected: PASS, including the five new ingest tests.

- [ ] **Step 6: Commit**

```bash
git add server/src/plugins/ingest.rs server/src/plugins/service.rs
git commit -m "server: resolve a pasted URL through its marketplace index"
```

---

### Task 2: Wire types

**Files:**
- Modify: `models/fluorite/plugins.fl`
- Regenerate: `clients/ts/src/generated/`, `clients/web/src/generated/`

**Interfaces:**
- Produces (Rust, `horsie_models::plugins`): `PluginInstallInput`, `PluginView`, `MarketplacePluginView`, `MarketplaceView`, `InstallOutcome`
- Produces (TS): the same names, camelCased, with `InstallOutcome` as `{ outcome: "Installed"; value: PluginView } | { outcome: "Marketplace"; value: MarketplaceView }`

- [ ] **Step 1: Rewrite `models/fluorite/plugins.fl`**

```fluorite
/// Wire contracts for the DB-managed plugin-bundle library (skills + hooks).
package plugins;

/// A library entry as shown in the web UI (metadata only — never the bytes).
struct PluginView {
    /// Canonical bundle name (from plugin.json, else repo basename).
    name: String,
    description: Option<String>,
    /// Resolved version (manifest version, else the cloned commit sha).
    version: Option<String>,
    source_url: String,
    source_ref: Option<String>,
    /// Number of SKILL.md skills the bundle provides.
    skill_count: u32,
    /// Whether the bundle ships hooks horsie will run.
    has_hooks: bool,
    /// Pre-checked in the new-session bundle picker.
    enabled_default: bool,
    artifact_size: u64,
    /// The marketplace this bundle came from, when it came from one. Bundles
    /// installed from a plain git URL have none.
    marketplace: Option<String>,
}

/// One plugin a marketplace offers, as cached from its index.
struct MarketplacePluginView {
    name: String,
    description: Option<String>,
    version: Option<String>,
    /// True when a bundle installed from this entry is already in the library,
    /// so the picker can say "installed" instead of offering it again.
    installed: bool,
}

/// A registered marketplace and the catalogue it last offered.
struct MarketplaceView {
    /// The index's declared name, else the repo basename. Primary key.
    name: String,
    source_url: String,
    source_ref: Option<String>,
    plugin_count: u32,
    /// When the index was last read, epoch millis as a string.
    updated_at: String,
    plugins: Vec<MarketplacePluginView>,
    /// Entries the index declared that could not be parsed. Shown rather than
    /// dropped: a catalogue that quietly lost three plugins is a bug report
    /// nobody files.
    skipped: Vec<String>,
}

/// Install a bundle, or register the catalogue a URL turned out to be.
///
/// Exactly one of `source_url` and the `(marketplace, plugin_name)` pair must be
/// given; the other two fields are then absent. Four optional fields rather than
/// a union is a deliberate trade recorded in the design doc: this is an existing
/// wire type, and a union buys compile-time safety over one runtime check at the
/// cost of reshaping every call site.
struct PluginInstallInput {
    source_url: Option<String>,
    source_ref: Option<String>,
    marketplace: Option<String>,
    plugin_name: Option<String>,
}

/// What a pasted URL turned out to be.
#[type_tag = "outcome"]
union InstallOutcome {
    // A plain bundle repo, or a marketplace declaring exactly one plugin.
    Installed(PluginView),
    // Several plugins on offer: the source is recorded and its index cached,
    // and the caller picks from `MarketplaceView.plugins`.
    Marketplace(MarketplaceView),
}

/// Toggle whether a bundle is pre-selected for new sessions.
struct PluginDefaultInput {
    enabled_default: bool,
}
```

**Note the `//` comments inside the union body.** `///` there is a fluorite parse error, and fluorite exits 0 on parse failure — the damage shows up later as a missing type.

- [ ] **Step 2: Regenerate and verify the generation actually happened**

```bash
cargo build -p horsie-models
grep -c "InstallOutcome" target/debug/build/horsie-models-*/out/plugins/mod.rs 2>/dev/null || \
  grep -rn "InstallOutcome" $(find target -name 'mod.rs' -path '*plugins*' | head -1)
make ts-types
cd clients/web && bun install && bun run generate-types && cd ../..
```

Expected: `clients/web/src/generated/plugins/installOutcome.ts` and `marketplaceView.ts` exist. If they do not, fluorite silently failed to parse — re-read the `.fl` for a `///` inside the union.

- [ ] **Step 3: Commit**

```bash
git add models/fluorite/plugins.fl clients/ts/src/generated clients/web/src/generated
git commit -m "models: marketplace views and an install outcome union"
```

---

### Task 3: The `marketplaces` table and its store

**Files:**
- Create: `server/migrations/sqlite/0022_marketplaces.sql`, `server/migrations/postgres/0022_marketplaces.sql`
- Create: `server/src/plugins/marketplace_store.rs`
- Modify: `server/src/plugins/store.rs`, `server/src/plugins/mod.rs`
- Modify: `support/src/plugin/marketplace.rs` (derive `Serialize`/`Deserialize`)

**Interfaces:**
- Consumes: `crate::db::Db`, `crate::db::testing::db()`, `horsie_support::plugin::MarketplaceEntry`
- Produces:
  - `PluginRow` gains `pub source_subpath: Option<String>`, `pub marketplace: Option<String>`, `pub marketplace_entry: Option<String>`
  - `PluginStore::installed_entries(&self, marketplace: &str) -> Result<HashSet<String>, String>`
  - `pub struct MarketplaceRow { name, source_url, source_ref, sha, entries: Vec<MarketplaceEntry>, skipped: Vec<String>, created_at, updated_at }`
  - `MarketplaceStore::{new, list, get, upsert, delete}`

**Why `source_subpath` on `plugins`, which the spec's SQL omits:** the spec says a bundle "carries the `subpath` it was resolved from, so `update` re-resolves to the same tree". That only survives a restart if the row stores it. Adding the column is the spec's intent made durable.

- [ ] **Step 1: Write the migrations**

`server/migrations/sqlite/0022_marketplaces.sql`:

```sql
-- Registered marketplaces: a git source plus the catalogue it last offered.
--
-- `entries` is the PARSED index, cached. The official marketplace has ~276
-- entries and browsing it is a page render, so a git clone must not sit on that
-- path. The cost is that the cache is a snapshot: a plugin published since the
-- last read appears only after POST /api/marketplaces/:name/refresh.
--
-- Storing the parsed form rather than the raw file means a marketplace.json
-- schema change is absorbed at refresh time by the same parser the CLI uses,
-- rather than at read time by a second one.
CREATE TABLE marketplaces (
    name          TEXT PRIMARY KEY,   -- the index's own `name`, else repo basename
    source_url    TEXT NOT NULL,
    source_ref    TEXT,
    sha           TEXT,               -- HEAD when last read
    entries       TEXT NOT NULL,      -- JSON array of parsed entries
    skipped       TEXT NOT NULL,      -- JSON array of reasons
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

-- Provenance for a bundle installed through a marketplace. Nullable because a
-- bundle installed from a plain URL has none, which is every row today.
-- `source_subpath` is where inside the checkout the plugin root sat, so `update`
-- re-clones the same tree rather than the repo root.
ALTER TABLE plugins ADD COLUMN source_subpath TEXT;
ALTER TABLE plugins ADD COLUMN marketplace TEXT;
ALTER TABLE plugins ADD COLUMN marketplace_entry TEXT;
```

`server/migrations/postgres/0022_marketplaces.sql`: the same file with the header line changed to
`-- PostgreSQL mirror of migrations/sqlite/0022_marketplaces.sql.` and each `ALTER TABLE` written as three separate statements exactly as above (Postgres accepts them unchanged). No type differences — every column is TEXT.

`plugin_count` from the spec's SQL is **dropped**: it is `entries.len()`, and a denormalised count that can disagree with the column beside it is a bug waiting to happen. `MarketplaceView.plugin_count` is computed on read.

- [ ] **Step 2: Write the failing store test**

Append to `server/src/plugins/store.rs`'s test module:

```rust
    /// Deleting a marketplace must leave bundles installed from it alone —
    /// dropping a source is not dropping the software.
    #[tokio::test]
    async fn provenance_survives_and_lists_installed_entries() {
        let s = PluginStore::new(testing::db().await);
        let mut r = row("api-security-testing", "h1");
        r.marketplace = Some("official".into());
        // The index's name for an entry is not always the name it installs as.
        r.marketplace_entry = Some("42crunch-api-security-testing".into());
        r.source_subpath = Some("./plugins/api".into());
        s.upsert(&r).await.unwrap();
        s.upsert(&row("plain", "h2")).await.unwrap();

        let got = s.get("api-security-testing").await.unwrap().unwrap();
        assert_eq!(got.marketplace.as_deref(), Some("official"));
        assert_eq!(got.source_subpath.as_deref(), Some("./plugins/api"));

        let entries = s.installed_entries("official").await.unwrap();
        assert!(entries.contains("42crunch-api-security-testing"));
        assert_eq!(entries.len(), 1, "a plain install must not appear");
    }
```

and create `server/src/plugins/marketplace_store.rs` with its own test:

```rust
    #[tokio::test]
    async fn entries_round_trip_through_json() {
        let s = MarketplaceStore::new(testing::db().await);
        assert!(s.list().await.unwrap().is_empty());
        s.upsert(&fixture()).await.unwrap();
        let got = s.get("official").await.unwrap().unwrap();
        assert_eq!(got.entries.len(), 2);
        assert_eq!(got.entries[1].name, "beta");
        assert_eq!(
            got.entries[0].source,
            PluginSource::Path("./plugins/alpha".into()),
            "the source shape must survive the cache, not just the name"
        );
        assert_eq!(got.skipped, vec!["entry 2: missing 'source'".to_string()]);

        s.delete("official").await.unwrap();
        assert!(s.get("official").await.unwrap().is_none());
    }

    /// A cache written by an older parser must not brick the list endpoint.
    #[tokio::test]
    async fn an_unreadable_cache_reports_itself_instead_of_failing() {
        let db = testing::db().await;
        let s = MarketplaceStore::new(db.clone());
        s.upsert(&fixture()).await.unwrap();
        sqlx::query(&db.q("UPDATE marketplaces SET entries = ? WHERE name = ?"))
            .bind("{not json")
            .bind("official")
            .execute(db.pool())
            .await
            .unwrap();

        let got = s.get("official").await.unwrap().unwrap();
        assert!(got.entries.is_empty());
        assert!(
            got.skipped.iter().any(|s| s.contains("refresh")),
            "must tell the operator what to do: {:?}",
            got.skipped
        );
    }
```

with

```rust
    fn fixture() -> MarketplaceRow {
        MarketplaceRow {
            name: "official".into(),
            source_url: "https://example.com/market.git".into(),
            source_ref: None,
            sha: Some("abc123".into()),
            entries: vec![
                MarketplaceEntry {
                    name: "alpha".into(),
                    description: Some("the first".into()),
                    version: None,
                    source: PluginSource::Path("./plugins/alpha".into()),
                },
                MarketplaceEntry {
                    name: "beta".into(),
                    description: None,
                    version: Some("2.0".into()),
                    source: PluginSource::Git {
                        url: "https://example.com/beta.git".into(),
                        path: None,
                        git_ref: Some("v2".into()),
                    },
                },
            ],
            skipped: vec!["entry 2: missing 'source'".into()],
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p horsie-server --lib plugins::`
Expected: FAIL — `marketplace_store` is not a module; `PluginRow` has no field `marketplace`.

- [ ] **Step 4: Implement**

First, make the support types cacheable — in `support/src/plugin/marketplace.rs`, change the two derives:

```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PluginSource {
```

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MarketplaceEntry {
```

(`Deserialize` is already imported in that file; use the fully-qualified paths above so the existing `use serde::Deserialize;` is not shadowed.)

Then `server/src/plugins/marketplace_store.rs`:

```rust
//! Storage for registered marketplaces (`marketplaces` table), sharing the
//! config store's database. The parsed index is cached in `entries` as JSON:
//! browsing a 276-entry catalogue is a local read, and a refresh is what puts a
//! git clone back on the path.

use crate::db::Db;
use horsie_support::plugin::MarketplaceEntry;
use sqlx::Row;
use sqlx::any::AnyRow;

const COLS: &str = "name, source_url, source_ref, sha, entries, skipped, created_at, updated_at";

/// One row of the `marketplaces` table, with `entries`/`skipped` already parsed.
#[derive(Clone, Debug)]
pub struct MarketplaceRow {
    pub name: String,
    pub source_url: String,
    pub source_ref: Option<String>,
    pub sha: Option<String>,
    pub entries: Vec<MarketplaceEntry>,
    pub skipped: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct MarketplaceStore {
    db: Db,
}

impl MarketplaceStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub async fn list(&self) -> Result<Vec<MarketplaceRow>, String> {
        let sql = self
            .db
            .q(&format!("SELECT {COLS} FROM marketplaces ORDER BY name"));
        let rows = sqlx::query(&sql)
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_marketplace).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<MarketplaceRow>, String> {
        let sql = self
            .db
            .q(&format!("SELECT {COLS} FROM marketplaces WHERE name = ?"));
        let row = sqlx::query(&sql)
            .bind(name)
            .fetch_optional(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_marketplace).transpose()
    }

    pub async fn upsert(&self, row: &MarketplaceRow) -> Result<(), String> {
        let entries = serde_json::to_string(&row.entries).map_err(|e| e.to_string())?;
        let skipped = serde_json::to_string(&row.skipped).map_err(|e| e.to_string())?;
        let sql = self.db.q(
            "INSERT INTO marketplaces (name, source_url, source_ref, sha, entries, skipped, \
             created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(name) DO UPDATE SET source_url = excluded.source_url, \
             source_ref = excluded.source_ref, sha = excluded.sha, \
             entries = excluded.entries, skipped = excluded.skipped, \
             updated_at = excluded.updated_at",
        );
        sqlx::query(&sql)
            .bind(&row.name)
            .bind(&row.source_url)
            .bind(&row.source_ref)
            .bind(&row.sha)
            .bind(&entries)
            .bind(&skipped)
            .bind(&row.created_at)
            .bind(&row.updated_at)
            .execute(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn delete(&self, name: &str) -> Result<(), String> {
        let sql = self.db.q("DELETE FROM marketplaces WHERE name = ?");
        sqlx::query(&sql)
            .bind(name)
            .execute(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// A cache this parser cannot read is reported on the row rather than failing
/// the read: one stale row must not take the marketplace list down with it, and
/// the operator needs to be told the fix is a refresh.
fn row_to_marketplace(row: &AnyRow) -> Result<MarketplaceRow, String> {
    let get_s = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    let get_os = |c: &str| {
        row.try_get::<Option<String>, _>(c)
            .map_err(|e| e.to_string())
    };
    let raw_entries = get_s("entries")?;
    let raw_skipped = get_s("skipped")?;
    let mut skipped: Vec<String> = serde_json::from_str(&raw_skipped).unwrap_or_default();
    let entries = match serde_json::from_str::<Vec<MarketplaceEntry>>(&raw_entries) {
        Ok(e) => e,
        Err(e) => {
            skipped.push(format!(
                "the cached index could not be read ({e}) — refresh this marketplace"
            ));
            Vec::new()
        }
    };
    Ok(MarketplaceRow {
        name: get_s("name")?,
        source_url: get_s("source_url")?,
        source_ref: get_os("source_ref")?,
        sha: get_os("sha")?,
        entries,
        skipped,
        created_at: get_s("created_at")?,
        updated_at: get_s("updated_at")?,
    })
}
```

Test module header, matching its siblings:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::db::testing;
    use horsie_support::plugin::PluginSource;
    // … fixture() and the two tests above
}
```

Then `store.rs`: extend `COLS` to
`"name, source_kind, source_url, source_ref, source_subpath, version, description, skill_count, has_hooks, artifact_hash, artifact_size, enabled_default, marketplace, marketplace_entry, created_at, updated_at"`,
add the three fields to `PluginRow` and `row_to_plugin` (`source_subpath`, `marketplace`, `marketplace_entry` all via `get_os`), extend the `INSERT` column list, `VALUES` placeholders and `DO UPDATE SET` clause with the same three, add the three `.bind(...)` calls **in column order**, add the fields to the test `row()` helper as `None`, and add:

```rust
    /// Entry names of bundles installed from `marketplace`, so the picker can
    /// mark them rather than offering them again.
    pub async fn installed_entries(&self, marketplace: &str) -> Result<HashSet<String>, String> {
        let sql = self
            .db
            .q("SELECT marketplace_entry FROM plugins WHERE marketplace = ? AND marketplace_entry IS NOT NULL");
        let rows = sqlx::query(&sql)
            .bind(marketplace)
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows
            .iter()
            .filter_map(|r| r.try_get::<Option<String>, _>("marketplace_entry").ok().flatten())
            .collect())
    }
```

Finally `server/src/plugins/mod.rs`: add `mod marketplace_store;` and `pub use marketplace_store::{MarketplaceRow, MarketplaceStore};`.

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p horsie-server --lib plugins::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add server/migrations server/src/plugins support/src/plugin/marketplace.rs
git commit -m "server: cache parsed marketplace indexes and bundle provenance"
```

---

### Task 4: The service — one install, three marketplace operations

**Files:**
- Modify: `server/src/plugins/service.rs`
- Modify: `server/src/http/mod.rs` (wherever `PluginService::new` is constructed — search for it)

**Interfaces:**
- Consumes: Task 1's `IngestTarget`/`Ingested`/`PluginBundle`/`ParsedMarketplace`/`read_marketplace`, Task 2's `InstallOutcome`/`MarketplaceView`/`MarketplacePluginView`, Task 3's `MarketplaceStore`/`MarketplaceRow`/`PluginStore::installed_entries`
- Produces:
  - `PluginService::new(store: PluginStore, marketplaces: MarketplaceStore, artifacts: ArtifactStore, token_secret: Vec<u8>)`
  - `PluginService::install(&self, input: PluginInstallInput) -> Result<InstallOutcome, String>`
  - `PluginService::list_marketplaces(&self) -> Result<Vec<MarketplaceView>, String>`
  - `PluginService::refresh_marketplace(&self, name: &str) -> Result<MarketplaceView, String>`
  - `PluginService::remove_marketplace(&self, name: &str) -> Result<(), String>`

- [ ] **Step 1: Write the failing service tests**

Append to `service.rs`'s test module. Reuse the existing `git()` helper; add `write_skill`/`write_marketplace`/`commit_repo` copies as in Task 1 (a test helper duplicated across two modules is cheaper than a shared test crate here).

```rust
    /// The one box: a URL that is a catalogue records a source and returns it,
    /// rather than erroring or installing something arbitrary.
    #[tokio::test]
    async fn a_catalogue_url_registers_a_marketplace() {
        let (svc, tmp) = service().await;
        let repo = tmp.path().join("market");
        std::fs::create_dir_all(&repo).unwrap();
        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        );
        write_skill(&repo.join("plugins/alpha"), "a");
        write_skill(&repo.join("plugins/beta"), "b");
        let url = commit_repo(&repo);

        let out = svc.install(url_input(&url)).await.unwrap();
        let InstallOutcome::Marketplace(m) = out else {
            panic!("a two-entry catalogue must not install anything");
        };
        assert_eq!(m.name, "catalogue");
        assert_eq!(m.plugin_count, 2);
        assert!(m.plugins.iter().all(|p| !p.installed));
        assert!(svc.list_marketplaces().await.unwrap().len() == 1);
    }

    /// The second half of the one box: picking an entry installs it through the
    /// CACHED index — no second clone of the marketplace.
    #[tokio::test]
    async fn installing_from_a_marketplace_uses_the_cached_index() {
        let (svc, tmp) = service().await;
        let repo = tmp.path().join("market");
        std::fs::create_dir_all(&repo).unwrap();
        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        );
        write_skill(&repo.join("plugins/alpha"), "a");
        write_skill(&repo.join("plugins/beta"), "b");
        let url = commit_repo(&repo);
        svc.install(url_input(&url)).await.unwrap();

        let out = svc
            .install(PluginInstallInput {
                source_url: None,
                source_ref: None,
                marketplace: Some("catalogue".into()),
                plugin_name: Some("beta".into()),
            })
            .await
            .unwrap();
        let InstallOutcome::Installed(v) = out else {
            panic!("picking an entry must install it");
        };
        assert_eq!(v.skill_count, 1);
        assert_eq!(v.marketplace.as_deref(), Some("catalogue"));

        // The picker now knows not to offer it again.
        let m = &svc.list_marketplaces().await.unwrap()[0];
        let beta = m.plugins.iter().find(|p| p.name == "beta").unwrap();
        assert!(beta.installed);
    }

    /// An unknown entry names what is on offer, as the CLI does.
    #[tokio::test]
    async fn an_unknown_entry_names_the_alternatives() {
        let (svc, tmp) = service().await;
        let repo = tmp.path().join("market");
        std::fs::create_dir_all(&repo).unwrap();
        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        );
        write_skill(&repo.join("plugins/alpha"), "a");
        write_skill(&repo.join("plugins/beta"), "b");
        let url = commit_repo(&repo);
        svc.install(url_input(&url)).await.unwrap();

        let err = svc
            .install(PluginInstallInput {
                source_url: None,
                source_ref: None,
                marketplace: Some("catalogue".into()),
                plugin_name: Some("gamma".into()),
            })
            .await
            .unwrap_err();
        assert!(err.contains("alpha") && err.contains("beta"), "err: {err}");
    }

    /// Neither form given is a rejection, not a panic and not a clone of "".
    #[tokio::test]
    async fn an_empty_install_input_is_rejected() {
        let (svc, _tmp) = service().await;
        let err = svc
            .install(PluginInstallInput {
                source_url: None,
                source_ref: None,
                marketplace: None,
                plugin_name: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("source_url"), "err: {err}");
    }

    /// Removing a source is not removing the software.
    #[tokio::test]
    async fn removing_a_marketplace_leaves_its_bundles_installed() {
        let (svc, tmp) = service().await;
        let repo = tmp.path().join("market");
        std::fs::create_dir_all(&repo).unwrap();
        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        );
        write_skill(&repo.join("plugins/alpha"), "a");
        write_skill(&repo.join("plugins/beta"), "b");
        let url = commit_repo(&repo);
        svc.install(url_input(&url)).await.unwrap();
        svc.install(PluginInstallInput {
            source_url: None,
            source_ref: None,
            marketplace: Some("catalogue".into()),
            plugin_name: Some("alpha".into()),
        })
        .await
        .unwrap();

        svc.remove_marketplace("catalogue").await.unwrap();
        assert!(svc.list_marketplaces().await.unwrap().is_empty());
        assert_eq!(svc.list().await.unwrap().len(), 1, "the bundle stays");
    }

    /// Re-pasting a registered marketplace refreshes it rather than erroring:
    /// "add it again" and "refresh" are the same intent from the user's side.
    #[tokio::test]
    async fn re_pasting_a_registered_marketplace_refreshes_it() {
        let (svc, tmp) = service().await;
        let repo = tmp.path().join("market");
        std::fs::create_dir_all(&repo).unwrap();
        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        );
        write_skill(&repo.join("plugins/alpha"), "a");
        write_skill(&repo.join("plugins/beta"), "b");
        let url = commit_repo(&repo);
        svc.install(url_input(&url)).await.unwrap();

        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"},
                 {"name":"gamma","source":"./plugins/gamma"}]}"#,
        );
        write_skill(&repo.join("plugins/gamma"), "g");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "add gamma"]);

        let out = svc.install(url_input(&url)).await.unwrap();
        let InstallOutcome::Marketplace(m) = out else {
            panic!("still a catalogue");
        };
        assert_eq!(m.plugin_count, 3);
        assert_eq!(svc.list_marketplaces().await.unwrap().len(), 1, "not a second row");
    }

    /// Two different sources claiming one name is rejected, naming the incumbent
    /// — silently renaming one would break provenance already recorded on
    /// installed bundles.
    #[tokio::test]
    async fn a_name_collision_between_two_sources_is_rejected() {
        let (svc, tmp) = service().await;
        let mk = |dir: &str| {
            let repo = tmp.path().join(dir);
            std::fs::create_dir_all(&repo).unwrap();
            write_marketplace(
                &repo,
                r#"{"name":"catalogue","plugins":[
                     {"name":"alpha","source":"./plugins/alpha"},
                     {"name":"beta","source":"./plugins/beta"}]}"#,
            );
            write_skill(&repo.join("plugins/alpha"), "a");
            write_skill(&repo.join("plugins/beta"), "b");
            commit_repo(&repo)
        };
        let first = mk("one");
        svc.install(url_input(&first)).await.unwrap();
        let err = svc.install(url_input(&mk("two"))).await.unwrap_err();
        assert!(err.contains(&first), "must name the incumbent source: {err}");
    }
```

with the helper

```rust
    fn url_input(url: &str) -> PluginInstallInput {
        PluginInstallInput {
            source_url: Some(url.to_string()),
            source_ref: None,
            marketplace: None,
            plugin_name: None,
        }
    }
```

and update `service()` to build a `MarketplaceStore` off the same pool (`crate::db::testing::db()` returns a `Db`; clone it).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p horsie-server --lib plugins::service`
Expected: FAIL — `install` returns `PluginView`, `list_marketplaces` does not exist.

- [ ] **Step 3: Implement the service**

Replace `install`, `update`, `persist`, `clone_and_pack` and `row_to_view` in `server/src/plugins/service.rs`, and add the marketplace operations:

```rust
/// Where a bundle came from, for the `plugins` row. Both fields or neither: a
/// bundle either came through a catalogue or did not.
enum Provenance {
    Direct,
    FromMarketplace { name: String, entry: String },
}

impl PluginService {
    /// Install a bundle, or register the catalogue a URL turned out to be.
    pub async fn install(&self, input: PluginInstallInput) -> Result<InstallOutcome, String> {
        let url = input
            .source_url
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let pair = match (input.marketplace.as_ref(), input.plugin_name.as_ref()) {
            (Some(m), Some(p)) => Some((m.clone(), p.clone())),
            (None, None) => None,
            _ => {
                return Err(
                    "marketplace and plugin_name must be given together".to_string()
                );
            }
        };
        match (url, pair) {
            (Some(_), Some(_)) => Err(
                "give either source_url or (marketplace, plugin_name), not both".to_string(),
            ),
            (None, None) => Err(
                "source_url, or a (marketplace, plugin_name) pair, is required".to_string(),
            ),
            (Some(url), None) => self.install_url(url, input.source_ref).await,
            (None, Some((market, plugin))) => self.install_entry(&market, &plugin).await,
        }
    }

    /// A pasted URL: clone once, and let what is there decide the outcome.
    async fn install_url(
        &self,
        url: String,
        git_ref: Option<String>,
    ) -> Result<InstallOutcome, String> {
        let git_ref = git_ref.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let target = IngestTarget::Url {
            url,
            git_ref,
        };
        match blocking_ingest(target).await? {
            Ingested::Plugin(bundle) => Ok(InstallOutcome::Installed(
                self.persist(bundle, Provenance::Direct).await?,
            )),
            Ingested::Marketplace(parsed) => {
                Ok(InstallOutcome::Marketplace(self.record(parsed).await?))
            }
        }
    }

    /// A pick from a registered catalogue: resolved against the CACHED index, so
    /// browsing and installing do not each pay for a clone of the marketplace.
    async fn install_entry(&self, market: &str, plugin: &str) -> Result<InstallOutcome, String> {
        let row = self
            .marketplaces
            .get(market)
            .await?
            .ok_or_else(|| format!("no such marketplace '{market}'"))?;
        let entry = row.entries.iter().find(|e| e.name == plugin).ok_or_else(|| {
            let names: Vec<&str> = row.entries.iter().map(|e| e.name.as_str()).collect();
            format!(
                "marketplace '{market}' has no plugin '{plugin}'. Available: {}",
                names.join(", ")
            )
        })?;
        let (url, git_ref, subpath) =
            source_location(&entry.source, &row.source_url, row.source_ref.as_deref());
        let entry_name = entry.name.clone();
        let bundle = match blocking_ingest(IngestTarget::Resolved {
            url,
            git_ref,
            subpath,
        })
        .await?
        {
            Ingested::Plugin(b) => b,
            // Unreachable by construction: `Resolved` never classifies.
            Ingested::Marketplace(m) => {
                return Err(format!("'{}' resolved to a marketplace", m.url));
            }
        };
        let view = self
            .persist(
                bundle,
                Provenance::FromMarketplace {
                    name: market.to_string(),
                    entry: entry_name,
                },
            )
            .await?;
        Ok(InstallOutcome::Installed(view))
    }

    pub async fn list_marketplaces(&self) -> Result<Vec<MarketplaceView>, String> {
        let mut out = Vec::new();
        for row in self.marketplaces.list().await? {
            out.push(self.marketplace_view(row).await?);
        }
        Ok(out)
    }

    /// Re-clone and re-parse. Deliberately `read_marketplace` rather than
    /// `ingest_git`: a catalogue that has dropped to one entry is still a
    /// catalogue, and must not turn a refresh into an install.
    pub async fn refresh_marketplace(&self, name: &str) -> Result<MarketplaceView, String> {
        let row = self
            .marketplaces
            .get(name)
            .await?
            .ok_or_else(|| format!("no such marketplace '{name}'"))?;
        let url = row.source_url.clone();
        let git_ref = row.source_ref.clone();
        let parsed = tokio::task::spawn_blocking(move || {
            ingest::read_marketplace(&url, git_ref.as_deref())
        })
        .await
        .map_err(|e| e.to_string())??;
        // The row keeps the name it was registered under: it is the primary key
        // and installed bundles already record it as their provenance.
        let updated = MarketplaceRow {
            name: row.name.clone(),
            source_url: row.source_url.clone(),
            source_ref: row.source_ref.clone(),
            sha: parsed.sha,
            entries: parsed.entries,
            skipped: parsed.skipped,
            created_at: row.created_at.clone(),
            updated_at: now_string(),
        };
        self.marketplaces.upsert(&updated).await?;
        self.marketplace_view(updated).await
    }

    /// Drop the source. Bundles installed from it stay: dropping a source is not
    /// dropping the software (and matches `horsie marketplace remove`).
    pub async fn remove_marketplace(&self, name: &str) -> Result<(), String> {
        self.marketplaces.delete(name).await
    }

    /// Register a freshly-parsed catalogue, or refresh the row already holding
    /// its name.
    async fn record(&self, parsed: ParsedMarketplace) -> Result<MarketplaceView, String> {
        let existing = self.marketplaces.get(&parsed.name).await?;
        if let Some(prev) = &existing {
            if prev.source_url != parsed.url {
                return Err(format!(
                    "a marketplace named '{}' is already registered from {}",
                    parsed.name, prev.source_url
                ));
            }
        }
        let now = now_string();
        let row = MarketplaceRow {
            name: parsed.name,
            source_url: parsed.url,
            source_ref: parsed.git_ref,
            sha: parsed.sha,
            entries: parsed.entries,
            skipped: parsed.skipped,
            created_at: existing.map_or_else(|| now.clone(), |p| p.created_at),
            updated_at: now,
        };
        self.marketplaces.upsert(&row).await?;
        self.marketplace_view(row).await
    }

    async fn marketplace_view(&self, row: MarketplaceRow) -> Result<MarketplaceView, String> {
        let installed = self.store.installed_entries(&row.name).await?;
        Ok(MarketplaceView {
            plugin_count: u32::try_from(row.entries.len()).unwrap_or(u32::MAX),
            plugins: row
                .entries
                .iter()
                .map(|e| MarketplacePluginView {
                    name: e.name.clone(),
                    description: e.description.clone(),
                    version: e.version.clone(),
                    installed: installed.contains(&e.name),
                })
                .collect(),
            name: row.name,
            source_url: row.source_url,
            source_ref: row.source_ref,
            updated_at: row.updated_at,
            skipped: row.skipped,
        })
    }

    async fn persist(
        &self,
        bundle: PluginBundle,
        provenance: Provenance,
    ) -> Result<PluginView, String> {
        let existing = self.store.get(&bundle.name).await?;
        self.artifacts
            .write(&bundle.hash, &bundle.zip_bytes)
            .map_err(|e| e.to_string())?;
        let (marketplace, marketplace_entry) = match provenance {
            Provenance::Direct => (None, None),
            Provenance::FromMarketplace { name, entry } => (Some(name), Some(entry)),
        };
        let now = now_string();
        let row = PluginRow {
            name: bundle.name,
            source_kind: "git".to_string(),
            source_url: bundle.url,
            source_ref: bundle.git_ref,
            source_subpath: bundle.subpath,
            version: bundle.version,
            description: bundle.description,
            skill_count: bundle.skill_count,
            has_hooks: bundle.has_hooks,
            artifact_hash: bundle.hash,
            artifact_size: bundle.zip_bytes.len() as u64,
            enabled_default: existing.as_ref().is_some_and(|e| e.enabled_default),
            marketplace,
            marketplace_entry,
            created_at: existing
                .as_ref()
                .map(|e| e.created_at.clone())
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        };
        self.store.upsert(&row).await?;
        self.gc().await?;
        Ok(row_to_view(row))
    }
}

/// Run the blocking clone + pack off the async runtime, warning about hooks
/// horsie cannot fire — a bundle whose skills are fine installs anyway, and
/// saying nothing would leave a guard that silently never runs.
async fn blocking_ingest(target: IngestTarget) -> Result<Ingested, String> {
    let ingested = tokio::task::spawn_blocking(move || ingest::ingest_git(&target))
        .await
        .map_err(|e| e.to_string())??;
    if let Ingested::Plugin(b) = &ingested {
        for reason in &b.unsupported_hooks {
            tracing::warn!(
                plugin = b.name,
                reason,
                "plugin declares a hook horsie cannot run"
            );
        }
    }
    Ok(ingested)
}
```

`update` becomes:

```rust
    /// Re-clone a bundle. One installed through a marketplace re-resolves
    /// through the cached index first, so a catalogue that has moved or
    /// re-pinned an entry is followed.
    pub async fn update(&self, name: &str) -> Result<PluginView, String> {
        let existing = self
            .store
            .get(name)
            .await?
            .ok_or_else(|| format!("no such bundle '{name}'"))?;
        match (&existing.marketplace, &existing.marketplace_entry) {
            (Some(market), Some(entry)) => match self.install_entry(market, entry).await? {
                InstallOutcome::Installed(v) => Ok(v),
                InstallOutcome::Marketplace(m) => {
                    Err(format!("'{}' resolved to a marketplace", m.source_url))
                }
            },
            _ => {
                let target = IngestTarget::Resolved {
                    url: existing.source_url.clone(),
                    git_ref: existing.source_ref.clone(),
                    subpath: existing.source_subpath.clone(),
                };
                match blocking_ingest(target).await? {
                    Ingested::Plugin(b) => self.persist(b, Provenance::Direct).await,
                    Ingested::Marketplace(m) => {
                        Err(format!("'{}' resolved to a marketplace", m.url))
                    }
                }
            }
        }
    }
```

`row_to_view` gains `marketplace: row.marketplace`. `PluginService` gains a `marketplaces: MarketplaceStore` field and `new` takes it as the second argument; update the construction site in `server/src/http/mod.rs` (search `PluginService::new`).

Note `persist` now reads `existing` itself rather than taking it as an argument — `update` no longer needs to thread it through.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p horsie-server --lib plugins::`
Expected: PASS — seven new service tests plus the existing two.

- [ ] **Step 5: Commit**

```bash
git add server/src/plugins/service.rs server/src/http/mod.rs
git commit -m "server: one install path for URLs, catalogues and picks"
```

---

### Task 5: HTTP routes

**Files:**
- Create: `server/src/http/marketplaces.rs`
- Modify: `server/src/http/plugins.rs:17-28`, `server/src/http/mod.rs` (module list + routes + tests)

**Interfaces:**
- Consumes: Task 4's service methods
- Produces routes:
  - `GET /api/marketplaces` → `Vec<MarketplaceView>`
  - `POST /api/marketplaces/{name}/refresh` → `MarketplaceView`
  - `DELETE /api/marketplaces/{name}` → 204
  - `POST /api/plugins` → `InstallOutcome` (201)

There is deliberately **no `POST /api/marketplaces`**: adding a source happens by pasting its URL into the one box.

- [ ] **Step 1: Write the failing HTTP tests**

In `server/src/http/mod.rs`'s test module, beside the existing plugin tests:

```rust
    /// The one box: the same endpoint answers "installed it" and "here is a
    /// catalogue", so the client never has to classify a URL before sending it.
    #[tokio::test]
    async fn posting_a_catalogue_url_returns_a_marketplace_outcome() {
        let (app, tmp) = app_with_marketplace_fixture().await;
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/plugins",
                &serde_json::json!({ "sourceUrl": tmp.url }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let body: serde_json::Value = json_body(res).await;
        assert_eq!(body["outcome"], "Marketplace");
        assert_eq!(body["value"]["pluginCount"], 2);

        let res = app.oneshot(get("/api/marketplaces")).await.unwrap();
        let list: serde_json::Value = json_body(res).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
    }

    /// Neither input form is a 422 with a message, not a 500.
    #[tokio::test]
    async fn posting_an_empty_install_input_is_unprocessable() {
        let (app, _tmp) = app_with_marketplace_fixture().await;
        let res = app
            .oneshot(post_json("/api/plugins", &serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Removing a source is a 204 and leaves the bundle library alone.
    #[tokio::test]
    async fn deleting_a_marketplace_keeps_its_installed_bundles() {
        let (app, tmp) = app_with_marketplace_fixture().await;
        app.clone()
            .oneshot(post_json(
                "/api/plugins",
                &serde_json::json!({ "sourceUrl": tmp.url }),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(post_json(
                "/api/plugins",
                &serde_json::json!({ "marketplace": "catalogue", "pluginName": "alpha" }),
            ))
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(delete("/api/marketplaces/catalogue"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let res = app.oneshot(get("/api/plugins")).await.unwrap();
        let body: serde_json::Value = json_body(res).await;
        assert_eq!(body.as_array().unwrap().len(), 1);
    }
```

`app_with_marketplace_fixture()` builds the test app the existing plugin tests use plus a `file://` two-entry catalogue in a tempdir, returning `(app, Fixture { _tmp: TempDir, url: String })`. **Read the existing plugin tests in `server/src/http/mod.rs:1095-1150` first and reuse their app builder and their `get`/`post`/`delete`/`json_body` helpers verbatim** — do not invent new ones; if a helper there is named differently, use that name.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p horsie-server --lib http::`
Expected: FAIL — 404 on `/api/marketplaces`.

- [ ] **Step 3: Implement**

`server/src/http/marketplaces.rs`:

```rust
//! HTTP surface for registered marketplaces: list what is on offer, re-read a
//! catalogue, drop a source.
//!
//! There is deliberately no POST: a marketplace is registered by pasting its URL
//! into `POST /api/plugins`, which is the one box the whole design turns on. A
//! second way in would be a second thing to keep consistent.

use super::AppState;
use super::error::Api;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use horsie_models::plugins::MarketplaceView;

/// GET /api/marketplaces — every registered source and its cached catalogue.
pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<MarketplaceView>>, Api> {
    state
        .plugins
        .list_marketplaces()
        .await
        .map(Json)
        .map_err(Api::internal)
}

/// POST /api/marketplaces/:name/refresh — re-clone and re-parse the index.
pub async fn refresh(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<MarketplaceView>, Api> {
    state
        .plugins
        .refresh_marketplace(&name)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// DELETE /api/marketplaces/:name — drop the source. Bundles installed from it
/// stay installed.
pub async fn remove(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .plugins
        .remove_marketplace(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Api::unprocessable)
}
```

`server/src/http/plugins.rs` — `install` returns the union:

```rust
/// POST /api/plugins — install a bundle, or register the catalogue a URL turned
/// out to be. One box: the caller does not have to know which it pasted.
pub async fn install(
    State(state): State<AppState>,
    Json(input): Json<PluginInstallInput>,
) -> Result<(StatusCode, Json<InstallOutcome>), Api> {
    state
        .plugins
        .install(input)
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(Api::unprocessable)
}
```

(both arms create a row, so both are 201.)

`server/src/http/mod.rs`: add `mod marketplaces;` beside `mod plugins;` and the routes beside the plugin ones:

```rust
        .route("/api/marketplaces", get(marketplaces::list))
        .route("/api/marketplaces/{name}", delete(marketplaces::remove))
        .route(
            "/api/marketplaces/{name}/refresh",
            post(marketplaces::refresh),
        )
```

(`delete` may need adding to the `axum::routing` import list — check what is already imported; other routes use `.delete(...)` chained on `put(...)`.)

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p horsie-server --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/src/http
git commit -m "server: marketplace routes and a union install response"
```

---

### Task 6: The web UI — one box, three sections

**Files:**
- Modify: `clients/web/src/api/client.ts`, `clients/web/src/hooks/usePlugins.ts`
- Create: `clients/web/src/pages/settings/skills/MarketplaceRow.tsx`, `clients/web/src/pages/settings/skills/BundleRow.tsx`
- Modify: `clients/web/src/pages/settings/SkillsSettings.tsx`
- Create: `clients/web/src/pages/settings/skills/MarketplaceRow.test.tsx`

**Interfaces:**
- Consumes: `InstallOutcome`, `MarketplaceView`, `MarketplacePluginView`, `PluginView` from `../../api/types`
- Produces:
  - `api.marketplaces.{list,refresh,remove}`; `api.plugins.install` now `Promise<InstallOutcome>`
  - `useMarketplaces()`, `useRefreshMarketplace()`, `useRemoveMarketplace()`, `marketplacesKey`
  - `<MarketplaceRow marketplace expanded onToggle />`, `<BundleRow bundle />`

- [ ] **Step 1: API client and hooks**

In `clients/web/src/api/client.ts`, change the install signature to `Promise<InstallOutcome>` and add beside the `plugins` block:

```ts
  marketplaces: {
    /** Registered sources, each with its cached catalogue. */
    list: (): Promise<MarketplaceView[]> => request("/marketplaces"),

    /** Re-clone and re-parse a source's index; may take a few seconds. */
    refresh: (name: string): Promise<MarketplaceView> =>
      request(`/marketplaces/${encodeURIComponent(name)}/refresh`, {
        method: "POST",
      }),

    /** Drop a source. Bundles installed from it stay installed. */
    remove: (name: string): Promise<void> =>
      request(`/marketplaces/${encodeURIComponent(name)}`, { method: "DELETE" }),
  },
```

In `clients/web/src/hooks/usePlugins.ts`, add `export const marketplacesKey = ["marketplaces"] as const;`, three hooks mirroring the plugin ones, and make **every** plugin mutation invalidate both keys (installing an entry flips its `installed` flag in the catalogue; removing a bundle flips it back):

```ts
const invalidateBoth = (client: QueryClient) => {
  void client.invalidateQueries({ queryKey: pluginsKey });
  void client.invalidateQueries({ queryKey: marketplacesKey });
};
```

- [ ] **Step 2: Write the failing component test**

`clients/web/src/pages/settings/skills/MarketplaceRow.test.tsx`:

```tsx
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { MarketplaceView } from "../../../api/types";
import { MarketplaceRow } from "./MarketplaceRow";

afterEach(cleanup);

const install = vi.fn();
const refresh = vi.fn();
const remove = vi.fn();

vi.mock("../../../hooks/usePlugins", () => ({
  useInstallPlugin: () => ({ mutate: install, isPending: false }),
  useRefreshMarketplace: () => ({ mutate: refresh, isPending: false }),
  useRemoveMarketplace: () => ({ mutate: remove, isPending: false }),
}));

function market(over: Partial<MarketplaceView> = {}): MarketplaceView {
  return {
    name: "official",
    sourceUrl: "https://example.com/market.git",
    sourceRef: undefined,
    pluginCount: 3,
    updatedAt: "1",
    skipped: [],
    plugins: [
      { name: "alpha", description: "the first", version: "1.0", installed: false },
      { name: "beta", description: undefined, version: undefined, installed: true },
      { name: "gamma", description: undefined, version: undefined, installed: false },
    ],
    ...over,
  };
}

describe("MarketplaceRow", () => {
  it("offers uninstalled entries and marks the installed one", () => {
    render(<MarketplaceRow marketplace={market()} expanded onToggle={() => {}} />);
    const rows = screen.getAllByTestId("marketplace-entry");
    expect(rows).toHaveLength(3);
    expect(screen.getByTestId("entry-install-beta")).toBeDisabled();
    expect(screen.getByTestId("entry-install-alpha")).not.toBeDisabled();
  });

  it("installs an entry by (marketplace, name), never by URL", () => {
    render(<MarketplaceRow marketplace={market()} expanded onToggle={() => {}} />);
    fireEvent.click(screen.getByTestId("entry-install-alpha"));
    expect(install).toHaveBeenCalledWith({
      marketplace: "official",
      pluginName: "alpha",
    });
  });

  // The official catalogue has ~276 entries; the filter is why the list is
  // usable at all.
  it("narrows the list as you filter", () => {
    render(<MarketplaceRow marketplace={market()} expanded onToggle={() => {}} />);
    fireEvent.change(screen.getByTestId("marketplace-filter"), {
      target: { value: "gam" },
    });
    expect(screen.getAllByTestId("marketplace-entry")).toHaveLength(1);
  });

  // A catalogue that quietly lost three plugins is a bug report nobody files.
  it("names entries it could not parse", () => {
    render(
      <MarketplaceRow
        marketplace={market({ skipped: ["entry 4: missing 'source'"] })}
        expanded
        onToggle={() => {}}
      />,
    );
    expect(screen.getByTestId("marketplace-skipped").textContent).toContain(
      "missing 'source'",
    );
  });

  it("collapses its entries when not expanded", () => {
    render(<MarketplaceRow marketplace={market()} expanded={false} onToggle={() => {}} />);
    expect(screen.queryByTestId("marketplace-entry")).toBeNull();
  });
});
```

- [ ] **Step 3: Run to verify failure**

Run: `cd clients/web && bunx vitest run src/pages/settings/skills`
Expected: FAIL — cannot resolve `./MarketplaceRow`.

- [ ] **Step 4: Implement the components**

`MarketplaceRow.tsx` — a controlled disclosure (`expanded`/`onToggle` come from the parent so the install box can reveal the source it just registered):

```tsx
import { ChevronDown, ChevronRight, Download, Loader2, RotateCcw, Trash2 } from "lucide-react";
import { useState } from "react";
import type { MarketplaceView } from "../../../api/types";
import {
  useInstallPlugin,
  useRefreshMarketplace,
  useRemoveMarketplace,
} from "../../../hooks/usePlugins";

export function MarketplaceRow({
  marketplace,
  expanded,
  onToggle,
}: {
  marketplace: MarketplaceView;
  expanded: boolean;
  onToggle: () => void;
}) { /* header row, then when `expanded`: filter input, entry list, skipped list */ }
```

Requirements the tests pin, plus the ones they cannot:
- header: name, `{pluginCount} plugins`, source URL, refresh button, delete button. Delete confirms with `confirm(\`Remove marketplace "${name}"? Bundles installed from it stay installed.\`)` — the CLI's semantics, said out loud.
- entries: `data-testid="marketplace-entry"`; install button `data-testid={\`entry-install-${p.name}\`}`, `disabled={p.installed || install.isPending}`, label `Installed` when installed else `Install`.
- filter: `data-testid="marketplace-filter"`, case-insensitive substring over name **and** description.
- skipped: rendered only when non-empty, `data-testid="marketplace-skipped"`.
- follow the existing page's classes (`panel`, `key`, `key-icon`, `chip`, `text-faint`, `rounded-[var(--radius-control)]`); do not introduce new colour tokens.

`BundleRow.tsx` — move `BundleRow` and `Toggle` out of `SkillsSettings.tsx` unchanged, and add the provenance chip after the version chip:

```tsx
          {bundle.marketplace && (
            <span className="chip !py-0 text-[0.625rem]">{bundle.marketplace}</span>
          )}
```

`SkillsSettings.tsx` — becomes composition. The install handler branches on the outcome:

```tsx
  const [expanded, setExpanded] = useState<string | null>(null);

  const submitInstall = async () => {
    const url = sourceUrl.trim();
    if (!url) return;
    try {
      const outcome = await install.mutateAsync({
        sourceUrl: url,
        sourceRef: sourceRef.trim() || undefined,
      });
      setSourceUrl("");
      setSourceRef("");
      // A catalogue is not an error and not a dead end: its row appears below,
      // already open, so the next click is the one the person came to make.
      setExpanded(outcome.outcome === "Marketplace" ? outcome.value.name : null);
    } catch {
      /* surfaced from install.error below */
    }
  };
```

with the box's copy updated to say what it now accepts — label `Git URL`, helper text `A skill bundle, or a marketplace of them. horsie works out which.` — and a Marketplaces `<section className="panel p-4">` between the install box and the installed bundles, rendering `marketplaces?.map(m => <MarketplaceRow key={m.name} marketplace={m} expanded={expanded === m.name} onToggle={() => setExpanded(expanded === m.name ? null : m.name)} />)`, with an empty state of `No marketplaces added yet. Paste one above.` The section is hidden entirely when the list is empty **and** nothing is loading, so a server with no catalogues looks exactly as it does today.

- [ ] **Step 5: Run to verify pass**

Run: `cd clients/web && bunx vitest run && bun run build`
Expected: PASS + a clean production build (this is the typecheck gate).

- [ ] **Step 6: Commit**

```bash
git add clients/web/src
git commit -m "web: browse and install from a marketplace on the skills page"
```

---

### Task 7: End-to-end, full green, PR

**Files:**
- Modify: `clients/web/e2e/harness.ts` (`RuntimeInfo` gains `marketplaceUrl`), `clients/web/e2e/global-setup.ts`
- Create: `clients/web/e2e/u-marketplace.spec.ts`
- Modify: `docs/guide/skills-and-plugins.md`

**Interfaces:**
- Consumes: everything above
- Produces: `RuntimeInfo.marketplaceUrl: string` — a `file://` URL for a two-entry catalogue repo built in global-setup

- [ ] **Step 1: Build the fixture in global-setup**

Beside the existing `plugins-lib` fixture, add a real git repo so the server's `git clone` has something local to clone (the e2e suite never touches the network):

```ts
  // A local git marketplace so group U can exercise the real ingest path —
  // clone, parse the index, pick an entry — without the network.
  const marketDir = path.join(tmpDir, "market");
  for (const [entry, skill] of [
    ["e2e-alpha", "e2e-alpha-skill"],
    ["e2e-beta", "e2e-beta-skill"],
  ]) {
    const d = path.join(marketDir, "plugins", entry, "skills", skill);
    fs.mkdirSync(d, { recursive: true });
    fs.writeFileSync(
      path.join(d, "SKILL.md"),
      `---\nname: ${skill}\ndescription: E2E marketplace fixture skill\n---\nbody\n`,
    );
  }
  fs.mkdirSync(path.join(marketDir, ".claude-plugin"), { recursive: true });
  fs.writeFileSync(
    path.join(marketDir, ".claude-plugin", "marketplace.json"),
    JSON.stringify({
      name: "e2e-market",
      plugins: [
        { name: "e2e-alpha", description: "the first fixture plugin", source: "./plugins/e2e-alpha" },
        { name: "e2e-beta", description: "the second", source: "./plugins/e2e-beta" },
      ],
    }),
  );
  const gitIn = (args: string[]) =>
    execFileSync("git", ["-C", marketDir, ...args], { stdio: "ignore" });
  gitIn(["init", "-q"]);
  gitIn(["config", "user.email", "e2e@example.com"]);
  gitIn(["config", "user.name", "e2e"]);
  gitIn(["add", "-A"]);
  gitIn(["commit", "-qm", "init"]);
  const marketplaceUrl = `file://${marketDir}`;
```

(`import { execFileSync } from "node:child_process";` — check whether `spawn` is already imported from there and extend that import.)

Add `marketplaceUrl` to the `RuntimeInfo` interface in `harness.ts` with a doc comment, and to the object global-setup writes to `RUNTIME_FILE`. Expose it as a Playwright fixture in `fixtures.ts` alongside `appBase`, following exactly how `appBase` is derived from `readRuntimeInfo()`.

- [ ] **Step 2: Write the e2e spec**

`clients/web/e2e/u-marketplace.spec.ts`:

```ts
// Group U — the one box: paste a catalogue URL, get a source rather than a
// failed install, pick a plugin from it, and see it in the library.
//
// This is the flow the whole design exists to produce, and the only test that
// covers all three sections of the Skills page together. It runs against a real
// `file://` git marketplace, so it exercises clone → parse index → resolve entry
// → pack → persist, which is the path that was broken for every
// marketplace-shaped repo before this change.

import { test, expect } from "./fixtures";

test("U1: a catalogue URL registers a source, and an entry installs from it", async ({
  page,
  appBase,
  marketplaceUrl,
}) => {
  await page.goto(`${appBase}/settings/skills`);

  await page.getByLabel("Git URL").fill(marketplaceUrl);
  await page.getByRole("button", { name: "Install" }).click();

  // Not an install, and not an error: a source, already open.
  const row = page.getByTestId("marketplace-row").filter({ hasText: "e2e-market" });
  await expect(row).toBeVisible();
  await expect(row).toContainText("2 plugins");
  await expect(page.getByTestId("entry-install-e2e-alpha")).toBeVisible();

  await page.getByTestId("entry-install-e2e-beta").click();

  // The bundle lands in the library, carrying where it came from…
  const bundle = page.getByTestId("bundle-row").filter({ hasText: "e2e-beta" });
  await expect(bundle).toBeVisible();
  await expect(bundle).toContainText("e2e-market");
  // …and the catalogue stops offering it.
  await expect(page.getByTestId("entry-install-e2e-beta")).toBeDisabled();
});
```

This needs `data-testid="marketplace-row"` on `MarketplaceRow`'s outer element and `data-testid="bundle-row"` on `BundleRow`'s — add them in Task 6's components if not already there, and re-run vitest.

- [ ] **Step 3: Run the e2e suite**

Run:
```bash
cd clients/web && bun run build && cd ../..
cargo build --workspace
cd clients/web && TMPDIR=/tmp/he2e HORSIE_E2E_SKIP_BUILD=1 bunx playwright test u-marketplace
```
(`TMPDIR=/tmp/he2e` is not optional on macOS: the default `$TMPDIR` makes the harness's unix socket path exceed `sun_path`'s 104 characters and global setup dies. `mkdir -p /tmp/he2e` first.)
Expected: PASS.

- [ ] **Step 4: Correct the stale guide**

`docs/guide/skills-and-plugins.md` currently says only `SessionStart` hooks run and frames hooks as context injection. Four events run now, two of them change control flow, and installing by URL now accepts a catalogue. Update:
- the hooks paragraph: name the four wired events (`PreToolUse`, `PostToolUse`, `SessionStart`, `Stop`), and say that a `PreToolUse` hook can deny or rewrite any tool call and a `Stop` hook can keep a turn going — the existing trust warning understates this.
- the install section: one box, a URL may be a bundle or a marketplace, and removing a marketplace leaves its bundles installed.

- [ ] **Step 5: Full gate**

Run, in order, stopping at the first failure:
```bash
cargo fmt --all
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --workspace
make ts-types && git diff --exit-code clients/ts/src/generated
cd clients/web && bun run build && bunx vitest run
```
`cargo fmt` before clippy, not after: clippy reports formatting-sensitive lints that fmt then re-breaks.

- [ ] **Step 6: Commit, push, open the PR**

```bash
git add -A
git commit -m "e2e: install from a marketplace end to end"
git push -u origin feat/server-marketplaces
gh pr create --title "server: marketplaces, and one box that installs anything" --body "$(cat <<'EOF'
Closes Phase 0 PR3 of #105.

`server/src/plugins/ingest.rs` inspected the repo root and never read `marketplace.json`, so every marketplace-shaped repo failed to install from the web UI while installing fine from the CLI — including `pbakaus/impeccable`, the repo that motivated #105. Ingest now classifies what it cloned: `IngestTarget::Url` may resolve through an index, `IngestTarget::Resolved` clones exactly what it was told, and only the former can return a catalogue.

`POST /api/plugins` returns an `InstallOutcome` union rather than always a `PluginView`, so one input box handles both — paste a URL and the server works out whether it is a bundle or a catalogue. A catalogue is recorded in a new `marketplaces` table with its parsed index cached, and browsing it is a local read; refresh is a button, never a poll. Settings → Skills grows a Marketplaces section between the install box and the bundle list, and installed bundles carry a chip naming where they came from.

Design: `docs/superpowers/specs/2026-08-05-server-marketplaces-design.md`. Plan: `docs/superpowers/plans/2026-08-05-server-marketplaces.md`.
EOF
)"
```

Then watch CI to green: `gh pr checks --watch`. Seven checks are required and pending checks block a merge, so use `gh pr merge --auto` if merging.

---

## Self-review

**Spec coverage.** Every section of the design maps to a task: "One box" → Tasks 2, 4, 5, 6; "Ingest parity" → Task 1; "Data model" → Task 3; "API" → Task 5; "Web" → Task 6; "Error handling" → Task 4 (ambiguous entry, unresolvable source, re-paste, name collision) and Task 1 (neither bundle nor marketplace, via the existing `PluginRoot::rejection`); "Testing" → the per-task test steps plus Task 7.

**Two deliberate deviations from the spec, both strengthening it:**
1. `IngestTarget` is an enum, not a struct with `subpath: Option<String>`. The spec's "never returned when `subpath` was given" becomes structural rather than a comment; a `Git` entry with no `path` would otherwise have made the promise depend on `None` meaning two things.
2. `plugins` gains `source_subpath`, which the spec's SQL omits. Without it the spec's own claim — that a bundle "carries the `subpath` it was resolved from, so `update` re-resolves to the same tree" — does not survive a restart.

**One simplification:** the spec's `plugin_count` column is dropped; it is `entries.len()`, computed on read.

**Not covered, deliberately:** `sha` is stored but nothing compares against it yet — the spec's "so refresh can report a no-op" is a future affordance, and reporting it would need a UI element the spec does not describe.
