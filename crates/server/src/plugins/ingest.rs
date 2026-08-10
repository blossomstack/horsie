//! Git ingestion: clone a bundle repo, inspect it (skills + hooks), pack a
//! deterministic zip, and hash it. Installation is a trusted admin action, so
//! the clone runs `git` on the host (not sandboxed). Deterministic zipping
//! (sorted entries, fixed mtime) makes re-clones of an unchanged tree hash
//! identically, so `update` is a no-op when nothing changed.

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
        /// The index's name for this entry, used when the plugin's own manifest
        /// declares none. The two differ often enough to matter:
        /// `42crunch-api-security-testing` installs as `api-security-testing`,
        /// and a repo directory name is a worse guess than either.
        name_hint: Option<String>,
    },
}

impl IngestTarget {
    fn url(&self) -> &str {
        match self {
            IngestTarget::Url { url, .. } | IngestTarget::Resolved { url, .. } => url,
        }
    }

    /// Trimmed, and `None` rather than empty — an empty `--branch` is a clone
    /// failure rather than "the default branch".
    fn git_ref(&self) -> Option<&str> {
        match self {
            IngestTarget::Url { git_ref, .. } | IngestTarget::Resolved { git_ref, .. } => {
                git_ref.as_deref().map(str::trim).filter(|r| !r.is_empty())
            }
        }
    }
}

/// A packed bundle — everything needed to persist a `plugins` row.
///
/// `url`/`git_ref`/`subpath` are what was *actually* cloned and descended into,
/// which is not always what the caller asked for: a marketplace entry may name
/// another repo. Storing the resolved triple is what makes `update` re-clone
/// the same tree.
pub struct PluginBundle {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Everything the bundle offers: its commands, skills and agents. Derived
    /// here, once, so nothing downstream re-scans a checkout to find out.
    pub catalog: Vec<horsie_support::plugin::catalog::CatalogEntry>,
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
///
/// `Debug`, unlike [`PluginBundle`], which holds the zip bytes.
#[derive(Debug)]
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

    // Where inside the checkout the plugin sits, and what to call it when its
    // own manifest does not say. A one-entry index names its plugin, which is a
    // better fallback than the repo basename.
    let (subpath, fallback) = match target {
        IngestTarget::Resolved {
            subpath, name_hint, ..
        } => (
            subpath.clone(),
            name_hint.clone().unwrap_or_else(|| repo_basename(url)),
        ),
        IngestTarget::Url { .. } => match Marketplace::read(&dest)? {
            // Not a catalogue: the repo root is the plugin root, as before.
            None => (None, repo_basename(url)),
            Some(m) => match m.plugins.as_slice() {
                // An index that declares nothing is not a catalogue worth
                // recording; fall back to inspecting the repo itself, as the
                // CLI does.
                [] => (None, repo_basename(url)),
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
                            name_hint: Some(only.name.clone()),
                        });
                    }
                    (sub, only.name.clone())
                }
                _ => {
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
        &fallback,
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
    let git_ref = git_ref.map(str::trim).filter(|r| !r.is_empty());
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let dest = tmp.path().join("repo");
    horsie_support::git::clone(url, git_ref, &dest)?;
    let m = Marketplace::read(&dest)?.ok_or_else(|| {
        format!(
            "'{}' is not a marketplace: it has no .claude-plugin/marketplace.json",
            horsie_support::remote_url::redact_url_credentials(url)
        )
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
    let catalog = horsie_support::plugin::catalog::build(&root);
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
        catalog,
        has_hooks,
        unsupported_hooks,
        zip_bytes,
        hash,
        url: url.to_string(),
        git_ref: git_ref.map(str::to_string),
        subpath,
    })
}

/// Unpack an artifact back into a directory tree — [`zip_dir`] read backwards.
///
/// Only the catalogue backfill needs this: runtimes unpack their own copies.
/// It lives here anyway, beside the packing it inverts, so the two conventions
/// cannot drift apart in separate files.
///
/// An entry whose path escapes `into` is skipped rather than written. These
/// archives are ones this server packed itself, so that cannot happen today —
/// but a path-traversal guard that exists only where an attacker is expected is
/// a guard someone deletes.
pub(super) fn unpack_zip(bytes: &[u8], into: &Path) -> Result<(), String> {
    let mut zip =
        zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = file.enclosed_name() else {
            continue;
        };
        let out = into.join(rel);
        if file.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut w = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut file, &mut w).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Deterministically zip a directory tree, excluding `.git`.
fn zip_dir(root: &Path) -> Result<Vec<u8>, String> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644)
        .last_modified_time(zip::DateTime::default());
    for (rel, abs) in &files {
        let data = std::fs::read(abs).map_err(|e| e.to_string())?;
        zip.start_file(rel, opts).map_err(|e| e.to_string())?;
        zip.write_all(&data).map_err(|e| e.to_string())?;
    }
    let cursor = zip.finish().map_err(|e| e.to_string())?;
    Ok(cursor.into_inner())
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_name() == std::ffi::OsStr::new(".git") {
            continue;
        }
        let path = entry.path();
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            collect_files(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, path));
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn repo_basename(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("plugin")
        .trim_end_matches(".git")
        .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Build a minimal plugin tree at `root`.
    fn write_plugin_tree(root: &Path) {
        let cp = root.join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("plugin.json"),
            r#"{"name":"demo","version":"1.0.0","description":"a demo bundle"}"#,
        )
        .unwrap();
        for s in ["a", "b"] {
            let d = root.join("skills").join(s);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!("---\nname: {s}\ndescription: d\n---\nbody"),
            )
            .unwrap();
        }
        let h = root.join("hooks");
        std::fs::create_dir_all(&h).unwrap();
        std::fs::write(h.join("hooks.json"), r#"{"hooks":{"SessionStart":[]}}"#).unwrap();
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

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
        std::fs::write(
            s.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\nbody"),
        )
        .unwrap();
    }

    /// Commit whatever is at `root` and return a `file://` URL for it.
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

    fn zip_entry_names(bytes: &[u8]) -> Vec<String> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
        (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect()
    }

    #[test]
    fn inspect_reads_manifest_and_counts_skills() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin_tree(tmp.path());
        let root = horsie_support::plugin::PluginRoot::inspect(tmp.path()).unwrap();
        assert_eq!(root.name("fallback"), "demo");
        assert_eq!(root.skill_dirs.len(), 2);
        assert!(root.is_installable());
    }

    /// A bundle whose hooks horsie cannot fire still ingests — its skills work —
    /// but the events are named, so nothing installs to silence.
    #[test]
    fn unsupported_hook_events_are_reported_not_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("src");
        let root = root.as_path();
        std::fs::create_dir_all(root.join("skills/s")).unwrap();
        std::fs::write(
            root.join("skills/s/SKILL.md"),
            "---\nname: s\ndescription: d\n---\nb",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("hooks")).unwrap();
        std::fs::write(
            root.join("hooks/hooks.json"),
            r#"{"hooks":{
                 "PostToolUse":[{"hooks":[{"type":"command","command":"ok"}]}],
                 "WorktreeCreate":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        )
        .unwrap();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "init"]);

        let ing =
            expect_plugin(ingest_git(&url_target(&format!("file://{}", root.display()))).unwrap());
        assert!(ing.has_hooks);
        assert_eq!(
            ing.unsupported_hooks.len(),
            1,
            "{:?}",
            ing.unsupported_hooks
        );
        assert!(
            ing.unsupported_hooks[0].contains("WorktreeCreate"),
            "{:?}",
            ing.unsupported_hooks
        );
    }

    /// `has_hooks` used to be a substring match for `"SessionStart"` in the raw
    /// manifest, so it reported `false` for every plugin whose hooks are
    /// `PreToolUse`-only — wrong for a field the UI renders as a generic badge.
    #[test]
    fn has_hooks_covers_non_session_start_events() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("src");
        std::fs::create_dir_all(repo.join("skills/a")).unwrap();
        std::fs::write(
            repo.join("skills/a/SKILL.md"),
            "---\nname: a\ndescription: d\n---\nb",
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("hooks")).unwrap();
        std::fs::write(
            repo.join("hooks/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Edit","hooks":[]}]}}"#,
        )
        .unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);

        let ing =
            expect_plugin(ingest_git(&url_target(&format!("file://{}", repo.display()))).unwrap());
        assert!(ing.has_hooks, "PreToolUse-only hooks must count as hooks");
    }

    /// A repo whose skills live where the manifest says, not where convention
    /// says — the shape that used to be rejected outright.
    #[test]
    fn manifest_declared_skills_root_is_ingested() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("src");
        let cp = repo.join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("plugin.json"),
            r#"{"name":"impeccable","version":"4.0.4","skills":"./.claude/skills/"}"#,
        )
        .unwrap();
        let s = repo.join(".claude/skills/impeccable");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(
            s.join("SKILL.md"),
            "---\nname: impeccable\ndescription: d\n---\nb",
        )
        .unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);

        let ing =
            expect_plugin(ingest_git(&url_target(&format!("file://{}", repo.display()))).unwrap());
        assert_eq!(ing.name, "impeccable");
        assert_eq!(
            ing.catalog
                .iter()
                .filter(|e| e.kind == horsie_support::plugin::catalog::CatalogKind::Skill)
                .count(),
            1
        );
    }

    #[test]
    fn a_repo_with_no_skills_is_rejected_with_where_it_looked() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("src");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("README.md"), "hi").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);

        // `.err().unwrap()` rather than `.unwrap_err()`: `Ingested` holds the
        // zip bytes and deliberately isn't `Debug`.
        let err = ingest_git(&url_target(&format!("file://{}", repo.display())))
            .err()
            .unwrap();
        assert!(err.contains("SKILL.md"), "err: {err}");
        assert!(err.contains("skills"), "must name where it looked: {err}");
    }

    #[test]
    fn zip_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin_tree(tmp.path());
        let a = zip_dir(tmp.path()).unwrap();
        let b = zip_dir(tmp.path()).unwrap();
        assert_eq!(sha256_hex(&a), sha256_hex(&b));
        assert!(!a.is_empty());
    }

    #[test]
    fn ingest_git_clones_and_inspects_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("src");
        std::fs::create_dir_all(&repo).unwrap();
        write_plugin_tree(&repo);
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);

        let url = format!("file://{}", repo.display());
        let ing = expect_plugin(ingest_git(&url_target(&url)).unwrap());
        assert_eq!(ing.name, "demo");
        assert_eq!(
            ing.catalog
                .iter()
                .filter(|e| e.kind == horsie_support::plugin::catalog::CatalogKind::Skill)
                .count(),
            2
        );
        assert!(ing.has_hooks);
        assert!(!ing.hash.is_empty());
        assert!(ing.version.is_some());
    }

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
        std::fs::write(
            s.join("SKILL.md"),
            "---\nname: impeccable\ndescription: d\n---\nb",
        )
        .unwrap();
        // A file at the repo root that must NOT end up in the artifact.
        std::fs::write(repo.join("README.md"), "not part of the bundle").unwrap();
        let url = commit_repo(&repo);

        let b = expect_plugin(ingest_git(&url_target(&url)).unwrap());
        assert_eq!(b.name, "impeccable");
        assert_eq!(
            b.catalog
                .iter()
                .filter(|e| e.kind == horsie_support::plugin::catalog::CatalogKind::Skill)
                .count(),
            1
        );
        assert_eq!(b.subpath.as_deref(), Some("./plugin"));
        assert_eq!(
            b.url, url,
            "a path entry stays in the marketplace's own repo"
        );
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
                name_hint: None,
            })
            .unwrap(),
        );
        assert_eq!(
            b.catalog
                .iter()
                .filter(|e| e.kind == horsie_support::plugin::catalog::CatalogKind::Skill)
                .count(),
            1
        );
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
}
