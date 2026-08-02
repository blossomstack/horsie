//! The vendor-authored baseline capability spec. The vendor owns the machine —
//! the workspaces, the plugin library, the local resources — so it owns the
//! sandbox policy too; nothing about confinement crosses the wire from the
//! server. One spec serves every OS: the runtime skips `Dir`/`File` grants
//! whose paths are absent on the host and ignores Seatbelt rules off macOS.
//!
//! The network is allowed: a local vendor's tools (git, cargo, curl) need
//! egress, and the runtime fetches the session's plugin bundles from the
//! server over HTTP *inside* the sandbox — a blocked default would break both
//! while adding little, since the filesystem is the actual boundary here.

use horsie_models::capabilities::CapabilitySpec;

const BASELINE_CAPABILITIES_JSON: &str = include_str!("baseline_capabilities.json");

/// The baseline spec, parsed from the embedded JSON. Returns `Err` instead of
/// panicking because workspace lints deny `expect` in production code.
pub fn baseline_capabilities() -> Result<CapabilitySpec, String> {
    serde_json::from_str(BASELINE_CAPABILITIES_JSON)
        .map_err(|e| format!("built-in baseline capability spec parse error: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::capabilities::{Access, Grant, NetworkPolicy, WorkingDirGrant};

    #[test]
    fn baseline_parses() {
        baseline_capabilities().expect("embedded baseline spec must parse");
    }

    #[test]
    fn baseline_allows_network_and_grants_working_dir_read_write() {
        let spec = baseline_capabilities().unwrap();
        assert!(
            matches!(spec.network, NetworkPolicy::Allow(_)),
            "a local vendor's tools and the runtime's bundle fetch need egress"
        );
        assert!(
            spec.grants.contains(&Grant::WorkingDir(WorkingDirGrant {
                access: Access::ReadWrite,
            })),
            "baseline must grant the working dir read-write"
        );
    }

    #[test]
    fn baseline_allows_macos_security_server_for_tls() {
        let spec = baseline_capabilities().unwrap();
        let rules = spec.unsafe_seatbelt_rules.unwrap_or_default();
        assert!(
            rules
                .iter()
                .any(|r| r == r#"(allow mach-lookup (global-name "com.apple.SecurityServer"))"#),
            "macOS Secure Transport needs SecurityServer to validate TLS certs"
        );
    }

    #[test]
    fn baseline_grants_system_toolchain_reads() {
        let spec = baseline_capabilities().unwrap();
        for path in ["/usr", "/bin", "/etc"] {
            let present = spec
                .grants
                .iter()
                .any(|g| matches!(g, Grant::Dir(d) if d.path == path && d.access == Access::Read));
            assert!(present, "baseline missing read dir grant for {path}");
        }
    }
}
