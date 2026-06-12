//! nono capability sandbox for the runtime child (Landlock on Linux, Seatbelt on
//! macOS). Behind the crate's `sandbox` feature.
//!
//! The capability set is declarative: the runtime loads a [`CapabilitySpec`]
//! (`horsie_models::capabilities`) from the `--sandbox-caps` file and translates it into a
//! `nono::CapabilitySet`. The spec is the single source of truth — the caller (the
//! CLI) resolves custom-or-default and writes the concrete file, so the runtime
//! carries no hidden fallback. This module owns only the spec → nono translation.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use horsie_models::capabilities::{Access, CapabilitySpec, Grant, NetworkPolicy};
use nono::{CapabilitySet, UnixSocketMode};
use std::path::Path;

/// Load `caps_file` and enter the sandbox. Fail-closed: an unsupported platform, an
/// unreadable/invalid file, or any nono error returns `Err`, and the caller exits
/// non-zero before connecting or running any tool. There is no bypass.
///
/// The executor `socket_path` grant is injected here — it is an operational
/// requirement of the runtime↔executor IPC, not user-facing policy.
pub fn apply(
    working_dirs: &[std::path::PathBuf],
    socket_path: Option<&Path>,
    caps_file: &Path,
) -> Result<(), String> {
    use nono::Sandbox;

    let info = Sandbox::support_info();
    if !info.is_supported {
        return Err(format!(
            "nono sandbox unsupported on {}: {}",
            info.platform, info.details
        ));
    }

    let spec = CapabilitySpec::load(caps_file)?;
    let caps = build_capability_set(&spec, working_dirs, socket_path)?;

    // `apply` returns `Result<SeccompNetFallback>` on Linux and `Result<()>` on
    // macOS. Bind the Linux payload (it is `#[must_use]`); on other platforms the
    // unit result is discarded as a statement (binding it would trip `let_unit_value`).
    #[cfg(target_os = "linux")]
    let _net_fallback = Sandbox::apply(&caps).map_err(|e| e.to_string())?;
    #[cfg(not(target_os = "linux"))]
    Sandbox::apply(&caps).map_err(|e| e.to_string())?;
    Ok(())
}

/// Pure spec → `nono::CapabilitySet` translation; no sandbox is entered. Split from
/// [`apply`] so the mapping (grants, network mode, IPC socket) is unit-testable.
fn build_capability_set(
    spec: &CapabilitySpec,
    working_dirs: &[std::path::PathBuf],
    socket_path: Option<&Path>,
) -> Result<CapabilitySet, String> {
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
            // Resolved to every runtime workspace root — one grant covers them all.
            Grant::WorkingDir(g) => {
                for dir in working_dirs {
                    caps = caps
                        .allow_path(dir, access_mode(&g.access))
                        .map_err(|e| e.to_string())?;
                }
            }
        }
    }

    // The executor IPC socket is an AF_UNIX grant — a capability layer separate from
    // the TCP network mode in nono, so it stays effective under `Block` and
    // `ProxyOnly` alike (nono emits the unix-socket allow after the network deny).
    if let Some(sock) = socket_path {
        caps = caps
            .allow_unix_socket(sock, UnixSocketMode::Connect)
            .map_err(|e| e.to_string())?;
    }

    match &spec.network {
        NetworkPolicy::Block(_) => caps = caps.block_network(),
        NetworkPolicy::Allow(_) => {}
        // Outbound TCP confined to localhost:<port> (the policy proxy); everything
        // else is kernel-blocked. The schema carries `u32` (fluorite has no `u16`),
        // so an out-of-range port is rejected here, fail-closed.
        NetworkPolicy::ProxyOnly(p) => {
            let port = u16::try_from(p.port)
                .map_err(|_| format!("proxy-only port {} out of range (max 65535)", p.port))?;
            caps = caps.proxy_only(port);
        }
    }

    // Raw platform rules verbatim (macOS Seatbelt; ignored on Linux). nono validates
    // each and emits them after the structured grants, so a trailing `(deny ...)` wins.
    for rule in spec.unsafe_seatbelt_rules.iter().flatten() {
        caps = caps.platform_rule(rule).map_err(|e| e.to_string())?;
    }

    // Diagnostics: when `HORSIE_SANDBOX_DEBUG_DENY` is set, emit `(debug deny)` so the
    // kernel logs every sandbox denial (visible via `log show --predicate 'eventMessage
    // CONTAINS "deny("'`). Off by default — it is noisy and only needed when hunting a
    // confinement failure. Read here (before sandbox apply) in the runtime's own env.
    if std::env::var_os("HORSIE_SANDBOX_DEBUG_DENY").is_some() {
        caps.set_seatbelt_debug_deny(true);
    }

    Ok(caps)
}

/// Translate the declarative [`Access`] into nono's `AccessMode`.
fn access_mode(access: &Access) -> nono::AccessMode {
    match access {
        Access::Read => nono::AccessMode::Read,
        Access::ReadWrite => nono::AccessMode::ReadWrite,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horsie_models::capabilities::{AllowNetwork, BlockNetwork, ProxyOnlyNetwork};
    use nono::NetworkMode;

    fn spec(network: NetworkPolicy) -> CapabilitySpec {
        CapabilitySpec {
            network,
            grants: vec![],
            unsafe_seatbelt_rules: None,
        }
    }

    #[test]
    fn block_policy_blocks_all_network() {
        let caps = build_capability_set(&spec(NetworkPolicy::Block(BlockNetwork {})), &[], None)
            .expect("build");
        assert_eq!(*caps.network_mode(), NetworkMode::Blocked);
    }

    #[test]
    fn allow_policy_leaves_network_open() {
        let caps = build_capability_set(&spec(NetworkPolicy::Allow(AllowNetwork {})), &[], None)
            .expect("build");
        assert_eq!(*caps.network_mode(), NetworkMode::AllowAll);
    }

    #[test]
    fn proxy_only_policy_maps_to_nono_proxy_only_for_that_port() {
        let caps = build_capability_set(
            &spec(NetworkPolicy::ProxyOnly(ProxyOnlyNetwork { port: 18080 })),
            &[],
            None,
        )
        .expect("build");
        assert_eq!(
            *caps.network_mode(),
            NetworkMode::ProxyOnly {
                port: 18080,
                bind_ports: vec![],
            }
        );
    }

    #[test]
    fn proxy_only_port_above_u16_is_rejected() {
        let err = build_capability_set(
            &spec(NetworkPolicy::ProxyOnly(ProxyOnlyNetwork { port: 70000 })),
            &[],
            None,
        )
        .expect_err("port 70000 must be rejected");
        assert!(err.contains("70000"), "error should name the port: {err}");
    }

    #[test]
    fn executor_ipc_unix_socket_grant_survives_proxy_only_mode() {
        // Unix sockets are a separate nono capability layer from the TCP network
        // mode: the IPC socket grant must hold even when egress is proxy-only.
        // nono canonicalizes the grant path, so it must exist on disk.
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("horsie-executor.sock");
        std::fs::write(&sock, b"").expect("create socket placeholder");
        let caps = build_capability_set(
            &spec(NetworkPolicy::ProxyOnly(ProxyOnlyNetwork { port: 18080 })),
            &[],
            Some(&sock),
        )
        .expect("build");
        // `covers` matches on the canonicalized grant path (e.g. /var → /private/var
        // on macOS), so query with the canonical form.
        let sock_canonical = sock.canonicalize().expect("canonicalize");
        assert!(
            caps.unix_socket_allowed(&sock_canonical, nono::UnixSocketOp::Connect),
            "executor IPC socket must stay connectable under ProxyOnly"
        );
        assert_eq!(
            *caps.network_mode(),
            NetworkMode::ProxyOnly {
                port: 18080,
                bind_ports: vec![],
            }
        );
    }
}
