//! What the shipped baseline actually does to a real process.
//!
//! The unit tests next to the spec check its *shape* — that `/` is granted
//! read, that the temp dir is granted read-write. That is not the same claim as
//! "a confined process can read a toolchain and cannot write to `$HOME`", and
//! the gap between the two is where #193 lived: the old baseline looked
//! reasonable as a list of prefixes and was unusable in practice.
//!
//! So this asks the kernel. The parent writes the real baseline to a caps file
//! and re-execs this test binary as a probe; the probe enters the sandbox and
//! reports which operations the kernel allowed. Nothing is mocked — the spec
//! under test is the one `horsie connect` ships.
#![cfg(feature = "sandbox")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// Set on the re-exec'd child, and the only thing that makes the probe run.
const PROBE_ENV: &str = "HORSIE_SANDBOX_PROBE_CAPS";
/// The workspace the probe is told to treat as its working dir.
const PROBE_WORKDIR_ENV: &str = "HORSIE_SANDBOX_PROBE_WORKDIR";

/// Emitted when the host cannot confine at all (no Landlock, unsupported
/// platform). The parent tolerates it rather than failing: an unenforceable
/// sandbox is a property of the machine, not a regression in the spec.
const SKIP: &str = "probe-skip";

#[test]
fn the_shipped_baseline_reads_the_whole_filesystem_and_writes_only_where_it_should() {
    // The probe re-runs this binary, so guard against a probe re-execing itself.
    if std::env::var_os(PROBE_ENV).is_some() {
        return;
    }

    let workspace = tempfile::tempdir().expect("workspace");
    let caps_dir = tempfile::tempdir().expect("caps dir");
    let caps_file = caps_dir.path().join("capabilities.json");
    let spec = horsie_runtime_host::baseline_capabilities().expect("baseline must parse");
    std::fs::write(
        &caps_file,
        serde_json::to_vec_pretty(&spec).expect("serialize baseline"),
    )
    .expect("write caps file");

    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "sandbox_probe", "--nocapture", "--ignored"])
        .env(PROBE_ENV, &caps_file)
        .env(PROBE_WORKDIR_ENV, workspace.path())
        .output()
        .expect("run probe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains(SKIP) {
        eprintln!("sandbox unsupported on this host; probe skipped");
        return;
    }
    assert!(
        output.status.success(),
        "probe failed.\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The probe. `#[ignore]` so it only runs when the parent names it explicitly —
/// a bare `cargo test` must never enter a sandbox in the shared test process.
#[test]
#[ignore = "re-exec'd by the parent test; entering a sandbox is irreversible"]
fn sandbox_probe() {
    let Some(caps_file) = std::env::var_os(PROBE_ENV) else {
        return;
    };
    let workdir = PathBuf::from(std::env::var_os(PROBE_WORKDIR_ENV).expect("probe workdir"));

    let workdirs = std::slice::from_ref(&workdir);
    if let Err(e) = horsie_runtime::sandbox::apply(workdirs, None, Path::new(&caps_file)) {
        println!("{SKIP}: {e}");
        return;
    }

    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));

    // The point of #193: a toolchain lives under $HOME, and the old baseline
    // granted nothing there. Read something outside every prefix that baseline
    // listed. `$HOME` itself is enough — it is the root of everything missing.
    assert!(
        std::fs::read_dir(&home).is_ok(),
        "reading $HOME must work: toolchains live under it"
    );

    // Writes stay fenced. If this ever passes, the sandbox has stopped being a
    // sandbox — read-all was only ever acceptable paired with a write fence.
    assert!(
        std::fs::write(home.join(".horsie-sandbox-probe"), b"x").is_err(),
        "writing to $HOME must be denied"
    );
    assert!(
        std::fs::write("/horsie-sandbox-probe", b"x").is_err(),
        "writing to / must be denied"
    );

    // The two places writes are supposed to land.
    std::fs::write(workdir.join("probe"), b"x").expect("the workspace must be writable");
    std::fs::write(std::env::temp_dir().join("horsie-sandbox-probe"), b"x")
        .expect("TMPDIR must be writable: git and cc create cache files there");
}
