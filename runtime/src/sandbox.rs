//! nono capability sandbox for the runtime child (Landlock on Linux, Seatbelt on
//! macOS). Behind the crate's `sandbox` feature.
//!
//! The capability set is declarative: the runtime loads a [`CapabilitySpec`]
//! (`models::capabilities`) from the `--sandbox-caps` file and translates it into a
//! `nono::CapabilitySet`. The spec is the single source of truth — the caller (the
//! CLI) resolves custom-or-default and writes the concrete file, so the runtime
//! carries no hidden fallback. This module owns only the spec → nono translation.

use models::capabilities::{Access, CapabilitySpec, Grant, NetworkPolicy};
use std::path::Path;

/// Load `caps_file` and enter the sandbox. Fail-closed: an unsupported platform, an
/// unreadable/invalid file, or any nono error returns `Err`, and the caller exits
/// non-zero before connecting or running any tool. There is no bypass.
///
/// The executor `socket_path` grant is injected here — it is an operational
/// requirement of the runtime↔executor IPC, not user-facing policy.
pub fn apply(
    working_dir: &Path,
    socket_path: Option<&Path>,
    caps_file: &Path,
) -> Result<(), String> {
    use nono::{CapabilitySet, Sandbox, UnixSocketMode};

    let info = Sandbox::support_info();
    if !info.is_supported {
        return Err(format!(
            "nono sandbox unsupported on {}: {}",
            info.platform, info.details
        ));
    }

    let spec = CapabilitySpec::load(caps_file)?;

    let mut caps = CapabilitySet::new();
    for grant in &spec.grants {
        match grant {
            // `allow_path` is directory-only; skip a path that is not a directory on
            // this host (defaults list paths that may be absent on a given system).
            Grant::Dir(g) => {
                let path = Path::new(&g.path);
                if path.is_dir() {
                    caps = caps
                        .allow_path(path, access_mode(&g.access))
                        .map_err(|e| e.to_string())?;
                }
            }
            // Single-file grant (e.g. a device node); on Linux nono adds the
            // device-ioctl rule automatically. Skip if the file is absent.
            Grant::File(g) => {
                let path = Path::new(&g.path);
                if path.exists() {
                    caps = caps
                        .allow_file(path, access_mode(&g.access))
                        .map_err(|e| e.to_string())?;
                }
            }
            // Resolved to the actual runtime working directory.
            Grant::WorkingDir(g) => {
                caps = caps
                    .allow_path(working_dir, access_mode(&g.access))
                    .map_err(|e| e.to_string())?;
            }
        }
    }

    if let Some(sock) = socket_path {
        caps = caps
            .allow_unix_socket(sock, UnixSocketMode::Connect)
            .map_err(|e| e.to_string())?;
    }

    match spec.network {
        NetworkPolicy::Block => caps = caps.block_network(),
        NetworkPolicy::Allow => {}
    }

    // `apply` returns `Result<SeccompNetFallback>` on Linux and `Result<()>` on
    // macOS. Bind the Linux payload (it is `#[must_use]`); on other platforms the
    // unit result is discarded as a statement (binding it would trip `let_unit_value`).
    #[cfg(target_os = "linux")]
    let _net_fallback = Sandbox::apply(&caps).map_err(|e| e.to_string())?;
    #[cfg(not(target_os = "linux"))]
    Sandbox::apply(&caps).map_err(|e| e.to_string())?;
    Ok(())
}

/// Translate the declarative [`Access`] into nono's `AccessMode`.
fn access_mode(access: &Access) -> nono::AccessMode {
    match access {
        Access::Read => nono::AccessMode::Read,
        Access::ReadWrite => nono::AccessMode::ReadWrite,
    }
}
