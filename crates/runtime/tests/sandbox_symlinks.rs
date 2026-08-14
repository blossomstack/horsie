//! Can a confined runtime read a plugin through a symlink?
//!
//! The per-agent plugin layout gives each agent a directory of links into one
//! content-addressed store, so two agents selecting the same bundle cost one
//! fetch:
//!
//! ```text
//! <plugins_dir>/store/<hash>/…            ← the real files
//! <plugins_dir>/agents/<agent>/<name> ->  ../../store/<hash>
//! ```
//!
//! Both halves live under the one directory the vendor granted, so on paper the
//! kernel resolves the link's target inside the grant and the read succeeds. On
//! paper is not good enough for a confinement boundary: Landlock evaluates the
//! *resolved* path and Seatbelt has its own rules, and a layout that turns out
//! to be unreadable under confinement has to be discovered here rather than as
//! an agent that silently has no skills.
//!
//! Asks the kernel, using the re-exec probe pattern from `sandbox_baseline.rs`:
//! the parent builds the tree and a caps file, the child enters the sandbox and
//! reports what it was allowed.

#![cfg(feature = "sandbox")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

const PROBE_ENV: &str = "HORSIE_SYMLINK_PROBE_CAPS";
const PROBE_ROOT_ENV: &str = "HORSIE_SYMLINK_PROBE_ROOT";

/// Emitted when the host cannot confine at all. Tolerated: an unenforceable
/// sandbox is a property of the machine, not a regression in the layout.
const SKIP: &str = "probe-skip";

/// Printed by the probe once it has entered the sandbox and read through the
/// link. A positive marker, because the parent tolerates a skip — without one,
/// "the test passed" and "the host cannot confine, so nothing was checked" look
/// identical, and this test exists precisely to answer a question about the
/// kernel.
const CONFIRMED: &str = "probe-confirmed";

/// What a bundle's `SKILL.md` says, so the assertion is a real read of real
/// bytes through the link rather than a stat.
const SKILL: &str = "---\nname: linked\ndescription: reached through a link\n---\nbody\n";

#[test]
fn a_confined_runtime_reads_a_plugin_through_the_per_agent_symlink() {
    if std::env::var_os(PROBE_ENV).is_some() {
        return;
    }

    let plugins = tempfile::tempdir().expect("plugins dir");
    build_tree(plugins.path());

    // Granted exactly as a vendor grants it: the one plugins directory, by path.
    // The store and the per-agent trees are both inside it, which is the whole
    // reason the layout is shaped this way — the runtime has no write grant on
    // the parent and could not create a sibling.
    let caps_dir = tempfile::tempdir().expect("caps dir");
    let caps_file = caps_dir.path().join("capabilities.json");
    let spec = horsie_runtime_host::baseline_capabilities().expect("baseline must parse");
    std::fs::write(
        &caps_file,
        serde_json::to_vec_pretty(&spec).expect("serialize baseline"),
    )
    .expect("write caps file");

    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "symlink_probe", "--nocapture", "--ignored"])
        .env(PROBE_ENV, &caps_file)
        .env(PROBE_ROOT_ENV, plugins.path())
        .output()
        .expect("run probe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains(SKIP) {
        eprintln!(
            "sandbox unsupported on this host; the symlink layout was NOT verified \
             against the kernel here"
        );
        return;
    }
    assert!(
        stdout.contains(CONFIRMED),
        "the probe neither confirmed nor skipped, so nothing was actually \
         checked.\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("symlink layout confirmed against the kernel on this host");
    assert!(
        output.status.success(),
        "a confined runtime could not read a plugin through its per-agent link, \
         so the store-and-link layout is not usable under the sandbox.\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The layout under test, built by the parent (which is not confined).
fn build_tree(root: &Path) {
    let store = root.join("store/deadbeef/skills/linked");
    std::fs::create_dir_all(&store).expect("store dir");
    std::fs::write(store.join("SKILL.md"), SKILL).expect("skill file");

    let agent_dir = root.join("agents/agent-1");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    // Relative, so the tree survives being moved or bind-mounted somewhere else
    // — a vendor resolves the plugins root itself and it is not the same path in
    // a container as on a laptop.
    #[cfg(unix)]
    std::os::unix::fs::symlink("../../store/deadbeef", agent_dir.join("a-bundle"))
        .expect("link the bundle into the agent's tree");
}

#[test]
#[ignore = "re-exec'd by the parent test; entering a sandbox is irreversible"]
fn symlink_probe() {
    let Some(caps_file) = std::env::var_os(PROBE_ENV) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(PROBE_ROOT_ENV).expect("probe root"));

    let workdirs = std::slice::from_ref(&root);
    if let Err(e) = horsie_runtime::sandbox::apply(workdirs, None, Path::new(&caps_file)) {
        println!("{SKIP}: {e}");
        return;
    }

    let through_link = root.join("agents/agent-1/a-bundle/skills/linked/SKILL.md");
    let read = std::fs::read_to_string(&through_link);
    assert!(
        read.is_ok(),
        "reading {} through the per-agent link was denied: {:?}",
        through_link.display(),
        read.err()
    );
    assert_eq!(
        read.expect("checked above"),
        SKILL,
        "the bytes through the link must be the store's own"
    );

    // The scanner enumerates the agent's directory and keeps entries that are
    // directories — `Path::is_dir()`, which resolves *through* a link. If that
    // resolution is denied, discovery finds nothing and the agent comes up with
    // no skills at all, silently.
    let listed: Vec<PathBuf> = std::fs::read_dir(root.join("agents/agent-1"))
        .expect("the agent's own directory must be listable")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(
        listed.len(),
        1,
        "the linked bundle must look like a directory to the scanner, got {listed:?}"
    );

    println!("{CONFIRMED}");
}
