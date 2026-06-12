//! Real-sandbox end-to-end test: drive the production `ProcessJobRuntime` through
//! the `SupervisorActor`, spawning a genuine nono-sandboxed `horsie-runtime`
//! child, with `mock-llm` behind the provider. This is the coverage that the old
//! `cli::run`-based `cli_e2e.rs` provided before the daemon refactor — it proves
//! the executor/runtime/capability assembly actually works, which the supervisor's
//! `TestRuntime`-based tests deliberately bypass.
//!
//! Gated on sandbox support (probed by running the built binary), so it exercises
//! confinement where the kernel supports it (ubuntu/Landlock, macOS/Seatbelt) and
//! skips elsewhere — never failing CI for kernel reasons.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use actor::{ActorRef, FileJournal, Journal, spawn_root};
use cli::capabilities::{builtin_default, resolve_user_paths};
use cli::config::{HorsieConfig, build_registry};
use mock_llm::MockLlmServer;
use models::daemon::{JobStatus, JobSummary};
use models::workflow::{WorkflowAgentDef, WorkflowDefinition};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use supervisor::{JobSpec, ProcessJobRuntime, SupervisorActor, SupervisorCommand, SupervisorDeps};
use tempfile::TempDir;
use tokio::sync::oneshot;

const CONCLUDE: &str = workflow::CONCLUDE_TOOL;

// ── sandbox probe harness (ported from the former cli_e2e.rs) ────────────────

fn locate_runtime_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HORSIE_RUNTIME_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?; // .../target/<profile>/deps
    if let Some(profile) = dir.parent() {
        let cand = profile.join("horsie-runtime");
        if cand.exists() {
            return Some(cand);
        }
    }
    let cand = dir.join("horsie-runtime");
    cand.exists().then_some(cand)
}

#[derive(Debug, PartialEq)]
enum SandboxProbe {
    Supported,
    Unsupported,
    Incompatible,
}

fn classify_probe(exit_code: Option<i32>) -> SandboxProbe {
    match exit_code {
        Some(3) => SandboxProbe::Unsupported,
        Some(2) => SandboxProbe::Incompatible,
        _ => SandboxProbe::Supported,
    }
}

fn probe_sandbox(bin: &Path) -> SandboxProbe {
    let Ok(spec) = builtin_default() else {
        return SandboxProbe::Unsupported;
    };
    let tmp = TempDir::new().unwrap();
    let caps = tmp.path().join("caps.json");
    std::fs::write(&caps, serde_json::to_vec(&spec).unwrap()).unwrap();
    match std::process::Command::new(bin)
        .arg("--endpoint")
        .arg("ws://127.0.0.1:1")
        .arg("--runtime-id")
        .arg("probe")
        .arg("--workspace")
        .arg(format!("probe={}", tmp.path().display()))
        .arg("--sandbox-caps")
        .arg(&caps)
        .output()
    {
        Ok(o) => classify_probe(o.status.code()),
        Err(_) => SandboxProbe::Unsupported,
    }
}

/// `Some(bin)` if the sandbox can be exercised here, else `None` (skip). Panics
/// with a rebuild hint if the binary is stale.
fn runtime_or_skip(test: &str) -> Option<PathBuf> {
    let Some(bin) = locate_runtime_bin() else {
        eprintln!("skipping {test}: horsie-runtime binary not found");
        return None;
    };
    match probe_sandbox(&bin) {
        SandboxProbe::Supported => Some(bin),
        SandboxProbe::Unsupported => {
            eprintln!("skipping {test}: nono sandbox unsupported on this platform");
            None
        }
        SandboxProbe::Incompatible => panic!(
            "horsie-runtime at {} does not understand the current CLI flags — it is a \
             stale build. Rebuild it with `cargo build -p runtime` (or run the suite via \
             `cargo test --workspace`, which rebuilds it).",
            bin.display()
        ),
    }
}

// ── supervisor harness ───────────────────────────────────────────────────────

fn config_with_mock(root: &Path, mock_url: &str) -> HorsieConfig {
    let cfg = json!({
        "providers": { "local": { "type": "anthropic", "base_url": mock_url } },
        "models": { "m": { "provider": "local", "model_id": "test-model" } },
        "storage": { "state_dir": root.join("state"), "data_dir": root.join("data") }
    });
    serde_json::from_value(cfg).unwrap()
}

/// A single-agent workflow with the given tool allowlist.
fn bash_workflow(tools: &[&str]) -> WorkflowDefinition {
    WorkflowDefinition {
        default_use_plugins: None,
        start: "solo".into(),
        agents: vec![WorkflowAgentDef {
            use_plugins: None,
            name: "solo".into(),
            system_prompt: None,
            model: "m".into(),
            output_schema: Some(json!({
                "type": "object",
                "properties": { "answer": { "type": "string" } }
            })),
            allow_ask_user: false,
            allow_timers: None,
            transitions: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: Some(tools.iter().map(|s| s.to_string()).collect()),
        }],
    }
}

fn boot(
    root: &Path,
    cfg: &HorsieConfig,
    bin: PathBuf,
) -> (Arc<dyn Journal>, ActorRef<SupervisorCommand>) {
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(root.join("state")));
    let deps = SupervisorDeps {
        provider_registry: build_registry(cfg).unwrap(),
        runtime_bin: bin,
        state_dir: root.join("state"),
        journal: journal.clone(),
        // No hackamore: these tests assert the hackamore-less spawn path stays unchanged.
        hackamore: None,
    };
    let sup = spawn_root(
        SupervisorActor::new(Arc::new(ProcessJobRuntime::new(deps))),
        journal.clone(),
    );
    (journal, sup)
}

fn job_spec(def: WorkflowDefinition, workdir: &Path) -> JobSpec {
    JobSpec {
        plugins_dir: None,
        hook_path: Vec::new(),
        hackamore_policy: None,
        workflow: def,
        workflow_name: "wf".into(),
        workspaces: vec![models::Workspace {
            name: "ws".into(),
            path: workdir.to_path_buf(),
        }],
        input: "go".into(),
        // The resolved built-in default sandbox spec — workdir read-write, system
        // paths read-only — exactly what the daemon would apply.
        capabilities: resolve_user_paths(builtin_default().unwrap()),
    }
}

/// A job spanning several named workspace roots.
fn job_spec_multi(def: WorkflowDefinition, workspaces: &[(&str, &Path)]) -> JobSpec {
    JobSpec {
        plugins_dir: None,
        hook_path: Vec::new(),
        hackamore_policy: None,
        workflow: def,
        workflow_name: "wf".into(),
        workspaces: workspaces
            .iter()
            .map(|(n, p)| models::Workspace {
                name: (*n).into(),
                path: p.to_path_buf(),
            })
            .collect(),
        input: "go".into(),
        capabilities: resolve_user_paths(builtin_default().unwrap()),
    }
}

async fn submit(sup: &ActorRef<SupervisorCommand>, spec: JobSpec) -> String {
    let (tx, rx) = oneshot::channel();
    sup.tell(SupervisorCommand::Submit {
        spec,
        submitted_at: 0,
        reply: tx,
    })
    .await
    .unwrap();
    rx.await.unwrap()
}

async fn list(sup: &ActorRef<SupervisorCommand>) -> Vec<JobSummary> {
    let (tx, rx) = oneshot::channel();
    sup.tell(SupervisorCommand::List { reply: tx })
        .await
        .unwrap();
    rx.await.unwrap()
}

/// Poll until the job reaches a terminal status; returns it. Generous timeout — a
/// real runtime child must spawn, connect, and run bash.
async fn wait_terminal(sup: &ActorRef<SupervisorCommand>, job_id: &str) -> JobStatus {
    for _ in 0..600 {
        if let Some(j) = list(sup).await.into_iter().find(|j| j.job_id == job_id)
            && matches!(j.status, JobStatus::Finished | JobStatus::Failed)
        {
            return j.status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("job {job_id} never reached a terminal status");
}

// ── tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn sandboxed_job_writes_inside_workdir() {
    let Some(bin) = runtime_or_skip("sandboxed_job_writes_inside_workdir") else {
        return;
    };
    let mock = MockLlmServer::builder()
        .tool_call("bash", json!({ "command": "echo hi > inside.txt" }))
        .tool_call(CONCLUDE, json!({ "answer": "done" }))
        .build()
        .await;
    let dir = TempDir::new().unwrap();
    let workdir = TempDir::new().unwrap();
    let cfg = config_with_mock(dir.path(), &mock.url());
    let (_journal, sup) = boot(dir.path(), &cfg, bin);

    let id = submit(&sup, job_spec(bash_workflow(&["bash"]), workdir.path())).await;
    let status = wait_terminal(&sup, &id).await;
    assert_eq!(status, JobStatus::Finished, "job should finish");

    let inside = workdir.path().join("inside.txt");
    assert!(
        inside.exists(),
        "the tool should have written inside the workdir"
    );
    assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "hi");
}

#[tokio::test]
async fn sandboxed_job_cannot_write_outside_workdir() {
    let Some(bin) = runtime_or_skip("sandboxed_job_cannot_write_outside_workdir") else {
        return;
    };
    let outside_dir = TempDir::new().unwrap();
    let outside = outside_dir.path().join("escaped.txt");
    let command = format!("echo pwned > {}", outside.display());

    let mock = MockLlmServer::builder()
        .tool_call("bash", json!({ "command": command }))
        .tool_call(CONCLUDE, json!({ "answer": "tried" }))
        .build()
        .await;
    let dir = TempDir::new().unwrap();
    let workdir = TempDir::new().unwrap();
    let cfg = config_with_mock(dir.path(), &mock.url());
    let (_journal, sup) = boot(dir.path(), &cfg, bin);

    let id = submit(&sup, job_spec(bash_workflow(&["bash"]), workdir.path())).await;
    // The job finishes regardless (the failed write is the tool's problem, not the
    // workflow's); what matters is the sandbox denied the escape.
    wait_terminal(&sup, &id).await;
    assert!(
        !outside.exists(),
        "sandbox must deny writes outside the workdir, but {} was created",
        outside.display()
    );
}

#[tokio::test]
async fn sandboxed_agent_loads_workspace_skill() {
    // A skill file in the workspace must be discovered by the runtime's real scan and
    // loadable by the agent: the sandboxed `horsie-runtime` child scans the workspace
    // (`runtime::scan::exec`), the agent lists it (`inspect_workspace`) and loads its
    // body (`skill`) over the real transport — the whole scan→agent path, end to end.
    let Some(bin) = runtime_or_skip("sandboxed_agent_loads_workspace_skill") else {
        return;
    };
    let mock = MockLlmServer::builder()
        .tool_call("inspect_workspace", json!({}))
        .tool_call("skill", json!({ "name": "doc-helper" }))
        .tool_call(CONCLUDE, json!({ "answer": "done" }))
        .build()
        .await;
    let dir = TempDir::new().unwrap();
    let workdir = TempDir::new().unwrap();

    // A real skill on disk for the runtime's glob (`.claude/skills/*/SKILL.md`) to find.
    let skill_dir = workdir.path().join(".claude/skills/doc-helper");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: doc-helper\ndescription: Summarize project docs\n---\nStep 1: read the docs.",
    )
    .unwrap();

    let cfg = config_with_mock(dir.path(), &mock.url());
    let (journal, sup) = boot(dir.path(), &cfg, bin);

    // `skill` / `inspect_workspace` are always available regardless of the tool allowlist.
    let id = submit(&sup, job_spec(bash_workflow(&[]), workdir.path())).await;
    assert_eq!(wait_terminal(&sup, &id).await, JobStatus::Finished);

    // Replay the durable journal: the tool results the agent saw must carry the
    // scanned skill — its listing (name + description) and its loaded body.
    let frames = supervisor::render_history(&journal, &id).await;
    let text: String = frames.into_iter().map(|f| f.text).collect();
    assert!(
        text.contains("doc-helper") && text.contains("Summarize project docs"),
        "list_skills should surface the scanned skill; history:\n{text}"
    );
    assert!(
        text.contains("Step 1: read the docs."),
        "skill(doc-helper) should load the body; history:\n{text}"
    );
}

#[tokio::test]
async fn sandboxed_agent_across_two_workspaces() {
    // End to end with TWO co-equal workspaces: the runtime scans both, the agent
    // inspects them, loads a skill from a *named* workspace (getting that one's body,
    // not the other's), and writes into a *named* workspace (the file lands there and
    // nowhere else). Exercises registry-resolved scan + skill + write across roots.
    let Some(bin) = runtime_or_skip("sandboxed_agent_across_two_workspaces") else {
        return;
    };
    let mock = MockLlmServer::builder()
        .tool_call("inspect_workspace", json!({}))
        .tool_call(
            "skill",
            json!({ "workspace": "beta", "name": "beta-skill" }),
        )
        .tool_call(
            "write_file",
            json!({ "workspace": "alpha", "path": "out.txt", "content": "from-agent" }),
        )
        .tool_call(CONCLUDE, json!({ "answer": "done" }))
        .build()
        .await;
    let dir = TempDir::new().unwrap();
    let alpha = TempDir::new().unwrap();
    let beta = TempDir::new().unwrap();

    // A distinct skill in each workspace; same-shaped files, different names/bodies.
    for (root, name, body) in [
        (alpha.path(), "alpha-skill", "Alpha body."),
        (beta.path(), "beta-skill", "Beta body."),
    ] {
        let skill_dir = root.join(format!(".claude/skills/{name}"));
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} helper\n---\n{body}"),
        )
        .unwrap();
    }

    let cfg = config_with_mock(dir.path(), &mock.url());
    let (journal, sup) = boot(dir.path(), &cfg, bin);

    let spec = job_spec_multi(
        bash_workflow(&["write_file"]),
        &[("alpha", alpha.path()), ("beta", beta.path())],
    );
    let id = submit(&sup, spec).await;
    assert_eq!(wait_terminal(&sup, &id).await, JobStatus::Finished);

    // The write targeted `alpha` by name → it lands in alpha, never in beta.
    assert_eq!(
        std::fs::read_to_string(alpha.path().join("out.txt")).unwrap(),
        "from-agent",
        "write_file(workspace=alpha) must land in the alpha root"
    );
    assert!(
        !beta.path().join("out.txt").exists(),
        "the alpha-scoped write must not appear in beta"
    );

    // Journal: inspect surfaced both workspaces and their skills; the skill call loaded
    // beta's body specifically (not alpha's).
    let frames = supervisor::render_history(&journal, &id).await;
    let text: String = frames.into_iter().map(|f| f.text).collect();
    assert!(
        text.contains("## alpha") && text.contains("## beta"),
        "inspect_workspace should list both workspaces; history:\n{text}"
    );
    assert!(
        text.contains("alpha-skill") && text.contains("beta-skill"),
        "inspect_workspace should list each workspace's skills; history:\n{text}"
    );
    assert!(
        text.contains("Beta body."),
        "skill(workspace=beta) should load beta's body; history:\n{text}"
    );
    assert!(
        !text.contains("Alpha body."),
        "only beta's skill body was requested; alpha's must not appear; history:\n{text}"
    );
}
