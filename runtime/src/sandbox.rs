//! nono capability sandbox for the runtime child (Landlock on Linux, Seatbelt on
//! macOS). Behind the crate's `sandbox` feature.

use std::path::{Path, PathBuf};

/// Per-platform read-only system paths a typical toolchain (`bash`, coreutils, git,
/// compilers) needs. Start minimal; expand from observed denials (nono surfaces
/// `DenialRecord` / `SandboxViolation` diagnostics).
fn system_read_paths() -> Vec<&'static str> {
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

/// Device files commonly used by the toolchain, with their access. These are
/// granted via `allow_file` (nono's `allow_path` is directory-only); on Linux nono
/// adds the device-ioctl rule automatically.
fn device_files() -> &'static [(&'static str, bool)] {
    // (path, writable)
    &[
        ("/dev/null", true),
        ("/dev/zero", false),
        ("/dev/urandom", false),
        ("/dev/random", false),
        ("/dev/tty", true),
    ]
}

/// Build the capability set and enter the sandbox. Fail-closed: an unsupported
/// platform or any error returns `Err`, and the caller exits non-zero before
/// connecting or running any tool. There is no bypass.
pub fn apply(
    working_dir: &Path,
    socket_path: Option<&Path>,
    extra_read: &[PathBuf],
) -> Result<(), String> {
    use nono::{AccessMode, CapabilitySet, Sandbox, UnixSocketMode};

    let info = Sandbox::support_info();
    if !info.is_supported {
        return Err(format!(
            "nono sandbox unsupported on {}: {}",
            info.platform, info.details
        ));
    }

    let mut caps = CapabilitySet::new();
    for p in system_read_paths() {
        if Path::new(p).is_dir() {
            caps = caps
                .allow_path(p, AccessMode::Read)
                .map_err(|e| e.to_string())?;
        }
    }
    for (dev, writable) in device_files() {
        if Path::new(dev).exists() {
            let mode = if *writable {
                AccessMode::ReadWrite
            } else {
                AccessMode::Read
            };
            caps = caps.allow_file(dev, mode).map_err(|e| e.to_string())?;
        }
    }
    caps = caps
        .allow_path(working_dir, AccessMode::ReadWrite)
        .map_err(|e| e.to_string())?;
    for p in extra_read {
        if p.is_dir() {
            caps = caps
                .allow_path(p, AccessMode::Read)
                .map_err(|e| e.to_string())?;
        } else if p.exists() {
            caps = caps
                .allow_file(p, AccessMode::Read)
                .map_err(|e| e.to_string())?;
        }
    }
    if let Some(sock) = socket_path {
        caps = caps
            .allow_unix_socket(sock, UnixSocketMode::Connect)
            .map_err(|e| e.to_string())?;
    }
    caps = caps.block_network();

    // `apply` returns `Result<SeccompNetFallback>` on Linux and `Result<()>` on
    // macOS. Bind the Linux payload (it is `#[must_use]`); on other platforms the
    // unit result is discarded as a statement (binding it would trip `let_unit_value`).
    #[cfg(target_os = "linux")]
    let _net_fallback = Sandbox::apply(&caps).map_err(|e| e.to_string())?;
    #[cfg(not(target_os = "linux"))]
    Sandbox::apply(&caps).map_err(|e| e.to_string())?;
    Ok(())
}
