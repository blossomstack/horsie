//! End-to-end CLI tests: drive `cli::run` / `cli::resume` against a real
//! nono-sandboxed `october-runtime` child, with `mock-llm` behind the provider.
//!
//! All sandbox-spawning tests are gated on runtime sandbox support (probed by
//! running the built binary), so they exercise confinement where the kernel
//! supports it (ubuntu/Landlock, macOS/Seatbelt) and skip elsewhere — never
//! failing CI for kernel reasons.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use cli::capabilities::builtin_default;
use cli::run::{EXIT_AWAIT, ResumeParams, RunParams, resume, run};
use mock_llm::MockLlmServer;
use models::capabilities::{Access, DirGrant, Grant};
use serde_json::json;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const CONCLUDE: &str = workflow::CONCLUDE_TOOL;

// ── harness ──────────────────────────────────────────────────────────────────

/// Find the built `october-runtime` binary: env override → sibling of the test
/// exe's profile dir → adjacent to the test exe.
fn locate_runtime_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OCTOBER_RUNTIME_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?; // .../target/<profile>/deps
    if let Some(profile) = dir.parent() {
        let cand = profile.join("october-runtime");
        if cand.exists() {
            return Some(cand);
        }
    }
    let cand = dir.join("october-runtime");
    cand.exists().then_some(cand)
}

/// Probe sandbox support by applying the sandbox in a throwaway runtime process
/// pointed at an unreachable `ws://` endpoint: exit 3 = apply failed (unsupported
/// kernel or `sandbox` feature off); anything else = apply succeeded. Sandboxing is
/// now driven by `--sandbox-caps`, so the probe writes the built-in default spec and
/// points the runtime at it.
fn sandbox_supported(bin: &Path) -> bool {
    let Ok(spec) = builtin_default() else {
        return false; // no built-in default for this platform → unsupported
    };
    let tmp = TempDir::new().unwrap();
    let caps = tmp.path().join("caps.json");
    std::fs::write(&caps, serde_json::to_vec(&spec).unwrap()).unwrap();
    match std::process::Command::new(bin)
        .arg("--endpoint")
        .arg("ws://127.0.0.1:1")
        .arg("--runtime-id")
        .arg("probe")
        .arg("--working-dir")
        .arg(tmp.path())
        .arg("--sandbox-caps")
        .arg(&caps)
        .output()
    {
        Ok(o) => o.status.code() != Some(3),
        Err(_) => false,
    }
}

/// `Some(bin)` if the sandbox can be exercised here, else `None` (skip).
fn runtime_or_skip(test: &str) -> Option<PathBuf> {
    match locate_runtime_bin() {
        Some(bin) if sandbox_supported(&bin) => Some(bin),
        Some(_) => {
            eprintln!("skipping {test}: nono sandbox unsupported on this platform");
            None
        }
        None => {
            eprintln!("skipping {test}: october-runtime binary not found");
            None
        }
    }
}

fn write_config(dir: &Path, mock_url: &str) -> PathBuf {
    let cfg = json!({
        // No api_key_env → a no-auth Anthropic client pointed at the mock server.
        "providers": { "local": { "type": "anthropic", "base_url": mock_url } },
        "models": { "m": { "provider": "local", "model_id": "test-model" } },
        "storage": { "root_dir": dir.join("state") }
    });
    let path = dir.join("october.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
    path
}

/// A single-agent workflow JSON file. `tools` is the allowlist; `ask` toggles
/// allow_ask_user.
fn write_workflow(dir: &Path, tools: &[&str], ask: bool) -> PathBuf {
    // fluorite-generated WorkflowAgentDef serializes with camelCase keys.
    let def = json!({
        "start": "solo",
        "agents": [ {
            "name": "solo",
            "systemPrompt": null,
            "model": "m",
            "outputSchema": { "type": "object", "properties": { "answer": { "type": "string" } } },
            "allowAskUser": ask,
            "transitions": null,
            "maxIterations": null,
            "maxRetries": null,
            "allowedTools": tools,
        } ]
    });
    let path = dir.join("workflow.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&def).unwrap()).unwrap();
    path
}

fn state_dir(root: &Path) -> PathBuf {
    root.join("state")
}

/// The single run directory under `<state>/runs/` (there is exactly one per test).
/// The workflow run id = the `runs/<id>` directory that has a `manifest.json`
/// (agent sessions also journal under `runs/<session_id>/`, but only the workflow
/// run writes a manifest).
fn workflow_run_id(state: &Path) -> String {
    let runs = state.join("runs");
    for e in std::fs::read_dir(&runs).unwrap() {
        let e = e.unwrap();
        if e.path().join("manifest.json").exists() {
            return e.file_name().to_string_lossy().into_owned();
        }
    }
    panic!("no run dir with a manifest under {}", runs.display());
}

// ── tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_orchestration_finishes() {
    let Some(bin) = runtime_or_skip("run_orchestration_finishes") else {
        return;
    };
    let mock = MockLlmServer::builder()
        .tool_call(CONCLUDE, json!({ "answer": "42" }))
        .build()
        .await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &mock.url());
    let workflow = write_workflow(dir.path(), &[], false);
    let workdir = TempDir::new().unwrap();

    let code = run(RunParams {
        workflow_path: workflow,
        config_path: config,
        workdir: workdir.path().to_path_buf(),
        input: "go".into(),
        state_dir: Some(state_dir(dir.path())),
        runtime_bin: bin,
        capabilities_path: None,
    })
    .await
    .expect("run failed");

    assert_eq!(code, 0, "expected the workflow to finish with exit 0");
}

#[tokio::test]
async fn suspend_then_resume_roundtrips() {
    let Some(bin) = runtime_or_skip("suspend_then_resume_roundtrips") else {
        return;
    };
    let mock = MockLlmServer::builder()
        .tool_call(
            CONCLUDE,
            json!({ "kind": "ask", "question": "what colour?" }),
        )
        .tool_call(
            CONCLUDE,
            json!({ "kind": "submit", "output": { "answer": "blue" } }),
        )
        .build()
        .await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &mock.url());
    let workflow = write_workflow(dir.path(), &[], true);
    let workdir = TempDir::new().unwrap();
    let state = state_dir(dir.path());

    let code = run(RunParams {
        workflow_path: workflow,
        config_path: config.clone(),
        workdir: workdir.path().to_path_buf(),
        input: "pick a colour".into(),
        state_dir: Some(state.clone()),
        runtime_bin: bin.clone(),
        capabilities_path: None,
    })
    .await
    .expect("run failed");
    assert_eq!(code, EXIT_AWAIT, "expected run to pause awaiting input");

    // Manifest round-trips and carries no secrets.
    let run_id = workflow_run_id(&state);
    let manifest_path = state.join("runs").join(&run_id).join("manifest.json");
    assert!(manifest_path.exists(), "manifest not written");
    let manifest = std::fs::read_to_string(&manifest_path).unwrap();
    assert!(!manifest.to_lowercase().contains("api_key"));
    assert!(manifest.contains("\"workdir\""));

    let code = resume(ResumeParams {
        run_id,
        config_path: config,
        state_dir: Some(state),
        message: "blue".into(),
        runtime_bin: bin,
    })
    .await
    .expect("resume failed");
    assert_eq!(code, 0, "expected resume to finish with exit 0");
}

#[tokio::test]
async fn failing_workflow_exits_without_hanging() {
    // When an agent can't complete (here: the model never calls the handoff tool, a
    // non-recoverable failure; recoverable failures suspend instead), the CLI must
    // EXIT — never block forever on a terminal notification that won't come. The
    // timeout turns a hang regression into a fast failure instead of a stuck CI job.
    let Some(bin) = runtime_or_skip("failing_workflow_exits_without_hanging") else {
        return;
    };
    let mock = MockLlmServer::builder().error(500, "boom").build().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &mock.url());
    let workflow = write_workflow(dir.path(), &[], false);
    let workdir = TempDir::new().unwrap();

    let code = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        run(RunParams {
            workflow_path: workflow,
            config_path: config,
            workdir: workdir.path().to_path_buf(),
            input: "go".into(),
            state_dir: Some(state_dir(dir.path())),
            runtime_bin: bin,
            capabilities_path: None,
        }),
    )
    .await
    .expect("run hung on a non-terminating workflow")
    .expect("run returned an error");
    // Failed → 1, Suspended (recoverable) → EXIT_AWAIT; either way it must not be 0
    // and must not hang.
    assert_ne!(
        code, 0,
        "a failing workflow must exit non-zero, not succeed"
    );
}

#[tokio::test]
async fn bash_writes_inside_workdir() {
    let Some(bin) = runtime_or_skip("bash_writes_inside_workdir") else {
        return;
    };
    let mock = MockLlmServer::builder()
        .tool_call("bash", json!({ "command": "echo hi > inside.txt" }))
        .tool_call(CONCLUDE, json!({ "answer": "done" }))
        .build()
        .await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &mock.url());
    let workflow = write_workflow(dir.path(), &["bash"], false);
    let workdir = TempDir::new().unwrap();

    let code = run(RunParams {
        workflow_path: workflow,
        config_path: config,
        workdir: workdir.path().to_path_buf(),
        input: "write a file".into(),
        state_dir: Some(state_dir(dir.path())),
        runtime_bin: bin,
        capabilities_path: None,
    })
    .await
    .expect("run failed");
    assert_eq!(code, 0);

    let inside = workdir.path().join("inside.txt");
    assert!(
        inside.exists(),
        "tool should have written inside the workdir"
    );
    assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "hi");
}

#[tokio::test]
async fn bash_cannot_write_outside_workdir() {
    let Some(bin) = runtime_or_skip("bash_cannot_write_outside_workdir") else {
        return;
    };
    // A target OUTSIDE the workdir (and not in any granted path).
    let outside_dir = TempDir::new().unwrap();
    let outside = outside_dir.path().join("escaped.txt");
    let command = format!("echo pwned > {}", outside.display());

    let mock = MockLlmServer::builder()
        .tool_call("bash", json!({ "command": command }))
        .tool_call(CONCLUDE, json!({ "answer": "tried" }))
        .build()
        .await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &mock.url());
    let workflow = write_workflow(dir.path(), &["bash"], false);
    let workdir = TempDir::new().unwrap();

    let code = run(RunParams {
        workflow_path: workflow,
        config_path: config,
        workdir: workdir.path().to_path_buf(),
        input: "try to escape".into(),
        state_dir: Some(state_dir(dir.path())),
        runtime_bin: bin,
        capabilities_path: None,
    })
    .await
    .expect("run failed");
    assert_eq!(code, 0);

    assert!(
        !outside.exists(),
        "sandbox must deny writes outside the workdir, but {} was created",
        outside.display()
    );
}

#[tokio::test]
async fn capability_file_grants_write_outside_workdir() {
    // The inverse of `bash_cannot_write_outside_workdir`: a custom capability file
    // that adds a read-write grant for an otherwise-denied directory lets the tool
    // write there — proving the file is a full, effective override of the default.
    let Some(bin) = runtime_or_skip("capability_file_grants_write_outside_workdir") else {
        return;
    };
    let outside_dir = TempDir::new().unwrap();
    let outside = outside_dir.path().join("granted.txt");
    let command = format!("echo ok > {}", outside.display());

    let mock = MockLlmServer::builder()
        .tool_call("bash", json!({ "command": command }))
        .tool_call(CONCLUDE, json!({ "answer": "wrote" }))
        .build()
        .await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &mock.url());
    let workflow = write_workflow(dir.path(), &["bash"], false);
    let workdir = TempDir::new().unwrap();

    // Full-override spec = built-in default + a read-write grant for the outside dir.
    let mut spec = builtin_default().expect("builtin default");
    spec.grants.push(Grant::Dir(DirGrant {
        path: outside_dir.path().to_string_lossy().into_owned(),
        access: Access::ReadWrite,
    }));
    let caps = dir.path().join("caps.json");
    std::fs::write(&caps, serde_json::to_vec_pretty(&spec).unwrap()).unwrap();

    let code = run(RunParams {
        workflow_path: workflow,
        config_path: config,
        workdir: workdir.path().to_path_buf(),
        input: "write outside".into(),
        state_dir: Some(state_dir(dir.path())),
        runtime_bin: bin,
        capabilities_path: Some(caps),
    })
    .await
    .expect("run failed");
    assert_eq!(code, 0);

    assert!(
        outside.exists(),
        "the capability file granted read-write to {}, so the tool should have written it",
        outside.display()
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap().trim(), "ok");
}
