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

struct PluginInfo {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    skill_count: u32,
    has_hooks: bool,
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

    let info = inspect_plugin_dir(&dest)?;
    if info.skill_count == 0 {
        return Err("not a plugin bundle: no SKILL.md found".to_string());
    }
    let name = info.name.clone().unwrap_or_else(|| repo_basename(url));
    let version = info.version.clone().or_else(|| git_head_sha(&dest));
    let zip_bytes = zip_dir(&dest)?;
    let hash = sha256_hex(&zip_bytes);
    Ok(Ingested {
        name,
        version,
        description: info.description,
        skill_count: info.skill_count,
        has_hooks: info.has_hooks,
        zip_bytes,
        hash,
    })
}

/// Read `.claude-plugin/plugin.json` (if any), count `<skills-loc>/*/SKILL.md`,
/// and detect a `SessionStart` hook. Honors a manifest `skills` override
/// (string or array), matching the shared-plugins discovery convention.
fn inspect_plugin_dir(root: &Path) -> Result<PluginInfo, String> {
    let manifest_path = root.join(".claude-plugin").join("plugin.json");
    let manifest: Option<serde_json::Value> = if manifest_path.is_file() {
        let txt = std::fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?;
        Some(serde_json::from_str(&txt).map_err(|e| format!("plugin.json: {e}"))?)
    } else {
        None
    };
    let field = |k: &str| {
        manifest
            .as_ref()
            .and_then(|m| m.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let locations: Vec<String> = match manifest.as_ref().and_then(|m| m.get("skills")) {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => vec!["skills".to_string()],
    };
    let mut skill_count = 0u32;
    for loc in &locations {
        if let Ok(entries) = std::fs::read_dir(root.join(loc)) {
            for entry in entries.flatten() {
                if entry.path().join("SKILL.md").is_file() {
                    skill_count += 1;
                }
            }
        }
    }
    let has_hooks = std::fs::read_to_string(root.join("hooks").join("hooks.json"))
        .map(|c| c.contains("SessionStart"))
        .unwrap_or(false);
    Ok(PluginInfo {
        name: field("name"),
        version: field("version"),
        description: field("description"),
        skill_count,
        has_hooks,
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
    format!("{:x}", hasher.finalize())
}

fn git_head_sha(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
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
        let info = inspect_plugin_dir(tmp.path()).unwrap();
        assert_eq!(info.name.as_deref(), Some("demo"));
        assert_eq!(info.skill_count, 2);
        assert!(info.has_hooks);
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
