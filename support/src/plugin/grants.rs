//! Sandbox grants for the bundles a runtime materializes for its session.

use horsie_models::capabilities::{Access, DirGrant, Grant};
use std::path::{Path, PathBuf};

/// Grants a sandboxed runtime needs to provision and read its session's
/// bundles: read-write on the directory it unpacks into — the sandbox is
/// applied before the fetch runs, so a read grant is not enough — and read on
/// the hook interpreter dirs.
pub fn session_plugin_grants(plugins_dir: Option<&Path>, hook_path: &[PathBuf]) -> Vec<Grant> {
    let mut out = Vec::new();
    if let Some(dir) = plugins_dir {
        out.push(Grant::Dir(DirGrant {
            path: dir.to_string_lossy().into_owned(),
            access: Access::ReadWrite,
        }));
    }
    out.extend(hook_path.iter().map(|p| {
        Grant::Dir(DirGrant {
            path: p.to_string_lossy().into_owned(),
            access: Access::Read,
        })
    }));
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

    #[test]
    fn the_plugins_dir_is_writable_because_the_runtime_unpacks_into_it() {
        let g = session_plugin_grants(Some(Path::new("/state/plugins/rt-1")), &[]);
        assert_eq!(g.len(), 1);
        assert!(
            matches!(&g[0], Grant::Dir(d)
                if d.path == "/state/plugins/rt-1" && d.access == Access::ReadWrite),
            "got {:?}",
            g[0]
        );
    }

    #[test]
    fn hook_dirs_are_read_only_and_granted_without_a_plugins_dir() {
        let g = session_plugin_grants(None, &[PathBuf::from("/opt/node/bin")]);
        assert!(
            matches!(&g[..], [Grant::Dir(d)]
                if d.path == "/opt/node/bin" && d.access == Access::Read),
            "got {g:?}"
        );
    }

    #[test]
    fn nothing_to_grant_yields_nothing() {
        assert!(session_plugin_grants(None, &[]).is_empty());
    }
}
