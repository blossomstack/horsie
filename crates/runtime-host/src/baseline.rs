//! The vendor-authored baseline capability spec. The vendor owns the machine —
//! the workspaces, the plugin library, the local resources — so it owns the
//! sandbox policy too; nothing about confinement crosses the wire from the
//! server. One spec serves every OS: the runtime skips `Dir`/`File` grants
//! whose paths are absent on the host and ignores Seatbelt rules off macOS.
//!
//! **The whole filesystem is readable; writes are fenced to the workspaces and
//! the temp dirs.** An earlier baseline enumerated the system prefixes it
//! thought a build needed and granted nothing under `$HOME`, which put every
//! developer toolchain out of reach — `cargo`, `rustc` and `npx` all live under
//! `~/.cargo`, `~/.rustup`, `~/.local/bin` or `~/.nvm`. Enumerating those roots
//! instead would be a losing game against every language and version manager,
//! so the read side is simply `/`. The write fence is what the sandbox is
//! actually for, and it is the part an agent cannot argue its way past.
//!
//! The network is allowed: a local vendor's tools (git, cargo, curl) need
//! egress, and the runtime fetches the session's plugin bundles from the
//! server over HTTP *inside* the sandbox. Note what that costs: with reads open
//! and egress open, the sandbox limits damage, not disclosure — a prompt
//! injection can still read `~/.ssh` and POST it somewhere. Making this a
//! confidentiality boundary needs egress confined to a policy proxy, tracked
//! separately; it is not a property this spec claims today.
//!
//! The keychain denies below are load-bearing and not decoration. nono treats
//! *any* directory grant that covers a keychain DB as the operator asking for
//! keychain access, and drops its five `mach-lookup` denies when it sees one —
//! and every path is under `/`. Without these rules, widening the read grant
//! would hand a sandboxed agent live `secd`/`keychaind` IPC, which decrypts
//! every saved credential on demand. That is a different thing from reading a
//! file the read grant already exposes, and nothing here asked for it.
//!
//! What makes the denies effective is that nono emits platform rules *after*
//! the blanket `(allow mach-lookup)` in its profile, and Seatbelt takes the
//! last matching rule. Order within this list does not matter — the four denies
//! and the `SecurityServer` allow name disjoint daemons. That allow stays
//! because Secure Transport needs it to validate TLS certs.

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
    use horsie_models::capabilities::{
        Access, Grant, NetworkPolicy, TempDirGrant, WorkingDirGrant,
    };

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

    /// The read side is the root, not a list of prefixes. A regression here is
    /// the whole of #193: enumerated prefixes always miss somebody's toolchain.
    #[test]
    fn baseline_grants_the_whole_filesystem_readable() {
        let spec = baseline_capabilities().unwrap();
        assert!(
            spec.grants
                .iter()
                .any(|g| matches!(g, Grant::Dir(d) if d.path == "/" && d.access == Access::Read)),
            "baseline must grant `/` read so toolchains under $HOME are reachable"
        );
    }

    /// The other half of #193: `TMPDIR` was inherited through the env allowlist
    /// but landed under a read-only `/var`, so every `git` and `cc` invocation
    /// failed to create a cache file.
    #[test]
    fn baseline_grants_temp_dirs_read_write() {
        let spec = baseline_capabilities().unwrap();
        assert!(
            spec.grants.contains(&Grant::TempDir(TempDirGrant {
                access: Access::ReadWrite,
            })),
            "baseline must grant the inherited TMPDIR read-write"
        );
        let tmp = spec.grants.iter().any(
            |g| matches!(g, Grant::Dir(d) if d.path == "/tmp" && d.access == Access::ReadWrite),
        );
        assert!(tmp, "baseline missing read-write dir grant for /tmp");
    }

    /// Read-all must not have quietly become write-all. Only the workspaces,
    /// the temp dirs and a few device nodes may be writable.
    #[test]
    fn baseline_fences_writes_to_workspaces_and_temp() {
        let spec = baseline_capabilities().unwrap();
        for grant in &spec.grants {
            let Grant::Dir(d) = grant else { continue };
            assert!(
                d.access == Access::Read || d.path == "/tmp",
                "unexpected writable dir grant in the baseline: {}",
                d.path
            );
        }
    }

    /// nono drops its keychain `mach-lookup` denies as soon as one directory
    /// grant covers a keychain DB — and the `/` read grant covers every path.
    /// These rules put the denies back; losing them would give a sandboxed
    /// agent live keychain decryption, which no part of #193 asked for.
    #[test]
    fn baseline_denies_keychain_daemons_that_the_root_read_grant_would_unlock() {
        let spec = baseline_capabilities().unwrap();
        let rules = spec.unsafe_seatbelt_rules.unwrap_or_default();
        for daemon in [
            "com.apple.securityd",
            "com.apple.security.keychaind",
            "com.apple.secd",
            "com.apple.security.agent",
        ] {
            let deny = format!(r#"(deny mach-lookup (global-name "{daemon}"))"#);
            assert!(
                rules.contains(&deny),
                "baseline must deny keychain daemon {daemon}; the `/` read grant \
                 makes nono think keychain access was requested"
            );
        }
    }
}
