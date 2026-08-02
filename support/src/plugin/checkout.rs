//! Materialising a plugin source into a checkout on disk.
//!
//! Every clone — whether it backs a marketplace or a plugin — lands in
//! `<data_dir>/sources/<key>`, keyed by `(url, ref)`. A marketplace entry that
//! points at a path inside its own repo therefore shares the marketplace's
//! clone instead of duplicating it.

use super::{PluginSource, source_key};
use std::path::{Path, PathBuf};

/// A materialised checkout: where it is, and the key naming it under `sources/`.
#[derive(Debug, Clone)]
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
        PluginSource::Git { url, path, git_ref } => (url.clone(), git_ref.clone(), path.clone()),
    }
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

        let b = ensure_checkout(&sources, &url, None).unwrap();
        assert_eq!(a.key, b.key, "same (url, ref) must reuse the checkout");
        assert_eq!(
            std::fs::read_dir(&sources).unwrap().count(),
            1,
            "a second call must not clone again"
        );
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
