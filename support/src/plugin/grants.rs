//! Sandbox grants for the shared plugin library.
//!
//! Installed plugins are symlinks into clones elsewhere on disk, and both
//! Landlock and Seatbelt resolve through symlinks — so the *targets* must be
//! granted too, not just the library root.

use horsie_models::capabilities::{Access, DirGrant, Grant};
use std::path::{Path, PathBuf};

/// Read-only `Dir` grants so a sandboxed runtime can read plugin skills and
/// resources and execute hooks. Empty when there is no library.
///
/// `extra_roots` carries the clone roots that the library's symlinks point into.
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
            g.iter()
                .all(|x| matches!(x, Grant::Dir(d) if d.access == Access::Read)),
            "plugin grants must be read-only"
        );
    }
}
