//! The default capability spec the server hands a session whose creation
//! request supplied none. `horsie connect` sandboxes its runtimes by default,
//! so this spec is enforced: it must confine a runtime without breaking it —
//! the working dir read-write, the system toolchain read-only, network
//! blocked. One spec serves every vendor OS: the runtime skips `Dir`/`File`
//! grants whose paths are absent on the host and ignores Seatbelt rules off
//! macOS, so the union of the per-OS defaults is safe everywhere.

use horsie_models::capabilities::CapabilitySpec;

const DEFAULT_CAPABILITIES_JSON: &str = include_str!("default_capabilities.json");

/// The built-in default spec, parsed from the embedded JSON. Returns `Err`
/// instead of panicking because workspace lints deny `expect` in production
/// code; a corrupt embedded file fails server startup, loudly.
pub fn default_capabilities() -> Result<CapabilitySpec, String> {
    serde_json::from_str(DEFAULT_CAPABILITIES_JSON)
        .map_err(|e| format!("built-in default capability spec parse error: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::capabilities::{Access, Grant, NetworkPolicy, WorkingDirGrant};

    #[test]
    fn default_spec_parses() {
        default_capabilities().expect("embedded default spec must parse");
    }

    #[test]
    fn default_spec_blocks_network_and_grants_working_dir_read_write() {
        let spec = default_capabilities().unwrap();
        assert!(
            matches!(spec.network, NetworkPolicy::Block(_)),
            "default must block network egress"
        );
        assert!(
            spec.grants.contains(&Grant::WorkingDir(WorkingDirGrant {
                access: Access::ReadWrite,
            })),
            "default must grant the working dir read-write"
        );
    }

    #[test]
    fn default_spec_allows_macos_security_server_for_tls() {
        let spec = default_capabilities().unwrap();
        let rules = spec.unsafe_seatbelt_rules.unwrap_or_default();
        assert!(
            rules.iter()
                .any(|r| r == r#"(allow mach-lookup (global-name "com.apple.SecurityServer"))"#),
            "macOS Secure Transport needs SecurityServer to validate TLS certs"
        );
    }

    #[test]
    fn default_spec_grants_system_toolchain_reads() {
        let spec = default_capabilities().unwrap();
        for path in ["/usr", "/bin", "/etc"] {
            let present = spec.grants.iter().any(
                |g| matches!(g, Grant::Dir(d) if d.path == path && d.access == Access::Read),
            );
            assert!(present, "default missing read dir grant for {path}");
        }
    }
}
