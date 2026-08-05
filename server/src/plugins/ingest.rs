//! Git ingestion: clone a bundle repo, inspect it (skills + hooks), pack a
//! deterministic zip, and hash it. Installation is a trusted admin action, so
//! the clone runs `git` on the host (not sandboxed). Deterministic zipping
//! (sorted entries, fixed mtime) makes re-clones of an unchanged tree hash
//! identically, so `update` is a no-op when nothing changed.

use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Result of a successful ingest — everything needed to persist a bundle.
pub struct Ingested {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub skill_count: u32,
    pub has_hooks: bool,
    pub zip_bytes: Vec<u8>,
    pub hash: String,
}

/// Clone `url` (optionally at `git_ref`), validate it is a plugin, and pack it.
/// Synchronous (shells `git`, walks the fs); callers run it on a blocking task.
pub fn ingest_git(url: &str, git_ref: Option<&str>) -> Result<Ingested, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("source_url is required".to_string());
    }
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let dest = tmp.path().join("repo");
    let mut cmd = std::process::Command::new("git");
    cmd.args(["clone", "--depth", "1"]);
    if let Some(r) = git_ref.map(str::trim).filter(|r| !r.is_empty()) {
        cmd.args(["--branch", r]);
    }
    cmd.arg(url).arg(&dest);
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

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
            std::fs::write(d.join("SKILL.md"), format!("---\nname: {s}\n---\nbody")).unwrap();
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

    #[test]
    fn inspect_reads_manifest_and_counts_skills() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin_tree(tmp.path());
        let root = horsie_support::plugin::PluginRoot::inspect(tmp.path()).unwrap();
        assert_eq!(root.name("fallback"), "demo");
        assert_eq!(root.skill_dirs.len(), 2);
        assert!(root.is_installable());
    }

    /// `has_hooks` used to be a substring match for `"SessionStart"` in the raw
    /// manifest, so it reported `false` for every plugin whose hooks are
    /// `PreToolUse`-only — wrong for a field the UI renders as a generic badge.
    #[test]
    fn has_hooks_covers_non_session_start_events() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("src");
        std::fs::create_dir_all(repo.join("skills/a")).unwrap();
        std::fs::write(repo.join("skills/a/SKILL.md"), "---\nname: a\n---\nb").unwrap();
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

        let ing = ingest_git(&format!("file://{}", repo.display()), None).unwrap();
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
        std::fs::write(s.join("SKILL.md"), "---\nname: impeccable\n---\nb").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);

        let ing = ingest_git(&format!("file://{}", repo.display()), None).unwrap();
        assert_eq!(ing.name, "impeccable");
        assert_eq!(ing.skill_count, 1);
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
        let err = ingest_git(&format!("file://{}", repo.display()), None)
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
        let ing = ingest_git(&url, None).unwrap();
        assert_eq!(ing.name, "demo");
        assert_eq!(ing.skill_count, 2);
        assert!(ing.has_hooks);
        assert!(!ing.hash.is_empty());
        assert!(ing.version.is_some());
    }
}
