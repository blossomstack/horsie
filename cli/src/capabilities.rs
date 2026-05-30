//! The built-in default sandbox capability spec, owned by the CLI. The CLI resolves
//! the effective spec (custom file → config → this default) and persists it into the
//! run dir; the runtime is a pure executor that loads whatever file it is handed.
//!
//! The defaults are the minimum a typical toolchain needs to even start a process:
//! read access to the loader/libraries, executables, and system config, plus the
//! standard device nodes; the working dir read-write; network blocked. They ship as
//! per-OS JSON so they are reviewable and fully replaceable via `--capabilities`.

use crate::error::CliError;
use models::capabilities::CapabilitySpec;

/// The shipped default capability file for the current platform, embedded at compile
/// time. `None` on platforms with no default (the sandbox is unsupported there).
fn builtin_default_json() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        Some(include_str!("capabilities/default.linux.json"))
    }
    #[cfg(target_os = "macos")]
    {
        Some(include_str!("capabilities/default.macos.json"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// The platform built-in default spec, used when no capability file is configured.
/// Returns `Err` on platforms with no shipped default (fail-closed).
pub fn builtin_default() -> Result<CapabilitySpec, CliError> {
    let raw = builtin_default_json().ok_or_else(|| {
        CliError::Config("no built-in capability spec for this platform".to_string())
    })?;
    serde_json::from_str(raw)
        .map_err(|e| CliError::Config(format!("built-in capability spec parse error: {e}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use models::capabilities::{Access, Grant, NetworkPolicy, WorkingDirGrant};

    // The legacy hard-coded sets, kept here as the regression oracle for the shipped
    // default files. If a default drifts from the original sandbox behavior, the
    // `builtin_default_matches_legacy_*` checks below fail.
    fn legacy_system_read_paths() -> Vec<&'static str> {
        #[cfg(target_os = "linux")]
        {
            vec![
                "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt", "/proc",
            ]
        }
        #[cfg(target_os = "macos")]
        {
            vec![
                "/usr",
                "/bin",
                "/sbin",
                "/System",
                "/Library",
                "/etc",
                "/private/etc",
                "/opt",
                "/var",
                "/private/var",
                "/dev/fd",
            ]
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Vec::new()
        }
    }

    fn legacy_device_files() -> &'static [(&'static str, bool)] {
        &[
            ("/dev/null", true),
            ("/dev/zero", false),
            ("/dev/urandom", false),
            ("/dev/random", false),
            ("/dev/tty", true),
        ]
    }

    #[test]
    fn builtin_default_blocks_network_and_grants_working_dir_rw() {
        let spec = builtin_default().expect("builtin default for this platform");
        assert_eq!(spec.network, NetworkPolicy::Block);
        assert!(
            spec.grants.contains(&Grant::WorkingDir(WorkingDirGrant {
                access: Access::ReadWrite,
            })),
            "built-in default must grant the working dir read-write"
        );
    }

    #[test]
    fn builtin_default_matches_legacy_hardcoded_sets() {
        let spec = builtin_default().unwrap();
        for p in legacy_system_read_paths() {
            let present = spec
                .grants
                .iter()
                .any(|g| matches!(g, Grant::Dir(d) if d.path == p && d.access == Access::Read));
            assert!(present, "built-in default missing read dir grant for {p}");
        }
        for (dev, writable) in legacy_device_files() {
            let access = if *writable {
                Access::ReadWrite
            } else {
                Access::Read
            };
            let present = spec
                .grants
                .iter()
                .any(|g| matches!(g, Grant::File(f) if f.path == *dev && f.access == access));
            assert!(present, "built-in default missing file grant for {dev}");
        }
    }

    #[test]
    fn builtin_default_round_trips_through_the_fluorite_wire_format() {
        // The shipped JSON must parse, and re-serializing then re-parsing must be
        // stable — guards the hand-written files against drift from the schema.
        let spec = builtin_default().unwrap();
        let json = serde_json::to_string(&spec).unwrap();
        let reparsed: CapabilitySpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, reparsed);
    }
}
