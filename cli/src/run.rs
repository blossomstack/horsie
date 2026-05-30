use crate::config::{OctoberConfig, build_registry};
use crate::error::CliError;
use crate::terminal_sink::TerminalSink;
use crate::validate::validate;
use actor::{FileJournal, Journal, spawn_root};
use executor::{
    ConnectedRuntimeRegistry, InMemExecutorTransport, ProcessRuntimeProvider, RuntimeEndpoint,
    RuntimeListenerServer, SandboxPolicy, serve_runtime_connections,
};
use executor_client::ExecutorClient;
use models::capabilities::CapabilitySpec;
use models::executor::RuntimeConfig;
use models::workflow::WorkflowDefinition;
use runtime_client::RuntimeClient;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workflow::{
    DefaultToolboxFactory, WorkflowActor, WorkflowCommand, WorkflowNotification,
    WorkflowRuntimeContext,
};

/// Exit code returned when a run pauses awaiting user input.
pub const EXIT_AWAIT: i32 = 10;

/// Persisted run metadata (no secrets): the resolved workflow + workdir, enough to
/// rebuild the runtime context on `resume`.
#[derive(Serialize, Deserialize)]
struct Manifest {
    workflow: WorkflowDefinition,
    workdir: PathBuf,
}

pub struct RunParams {
    pub workflow_path: PathBuf,
    pub config_path: PathBuf,
    pub workdir: PathBuf,
    pub input: String,
    pub state_dir: Option<PathBuf>,
    pub runtime_bin: PathBuf,
    /// `--capabilities`: overrides `sandbox.capabilities_file` from the config.
    pub capabilities_path: Option<PathBuf>,
}

pub struct ResumeParams {
    pub run_id: String,
    pub config_path: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub message: String,
    pub runtime_bin: PathBuf,
}

fn load_workflow(path: &Path) -> Result<WorkflowDefinition, CliError> {
    let text = std::fs::read_to_string(path).map_err(|e| CliError::Io(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| CliError::Config(e.to_string()))
}

fn run_dir(root: &Path, run_id: &str) -> PathBuf {
    root.join("runs").join(run_id)
}

/// Ephemeral unix socket for one executor assembly, kept short (sockaddr_un caps
/// the path at ~108 bytes) and outside any agent workdir. Unique per call — the
/// socket is never persisted, so `run` and `resume` must not collide (a stale
/// listener's `Drop` would otherwise unlink a live socket).
fn socket_path() -> Result<PathBuf, CliError> {
    let token = Uuid::new_v4().simple().to_string();
    let path = std::env::temp_dir()
        .join(format!("october-{}", &token[..12]))
        .join("rt.sock");
    // sockaddr_un.sun_path is 104 bytes on macOS and 108 on Linux (incl. NUL), so
    // the usable max is 103 / 107. Use the tighter platform limit.
    let max = if cfg!(target_os = "macos") { 103 } else { 107 };
    if path.as_os_str().len() > max {
        return Err(CliError::Executor(format!(
            "unix socket path too long ({} bytes, max {max}): {}",
            path.as_os_str().len(),
            path.display()
        )));
    }
    Ok(path)
}

/// How to launch the sandboxed runtime child for one `drive` call: which binary, and
/// an optional `--capabilities` override of the configured/default capability spec.
struct RuntimeLaunch {
    binary: PathBuf,
    capabilities_override: Option<PathBuf>,
}

/// Assemble the in-process sandboxed executor, spawn the workflow actor, and drive
/// the two-plane control loop until a terminal/await transition.
async fn drive(
    def: WorkflowDefinition,
    cfg: OctoberConfig,
    workdir: PathBuf,
    run_id: String,
    root_dir: PathBuf,
    launch: RuntimeLaunch,
    kickoff: WorkflowCommand,
) -> Result<i32, CliError> {
    let registry = build_registry(&cfg)?;

    // Runtime listener (unix) + connected registry; the accept loop registers the
    // direct transport for each runtime that connects.
    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    let sock = socket_path()?;
    let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Unix(sock.clone()))
        .await
        .map_err(|e| CliError::Executor(e.to_string()))?;
    let cancel = CancellationToken::new();
    serve_runtime_connections(listener, connected.clone(), cancel.clone());

    // Resolve the effective capability spec — `--capabilities` flag wins over
    // `sandbox.capabilities_file`, and absent both we use the runtime's platform
    // built-in default. Persist the concrete spec into the run dir so the runtime
    // loads a single source of truth and `runs/<id>/capabilities.json` records
    // exactly what was applied. Resolution happens here (not in the runtime) so the
    // runtime carries no hidden fallback.
    let caps_source = launch
        .capabilities_override
        .or_else(|| cfg.sandbox.capabilities_file.clone());
    let spec = match &caps_source {
        Some(path) => CapabilitySpec::load(path).map_err(CliError::Config)?,
        None => crate::capabilities::builtin_default()?,
    };
    let rdir = run_dir(&root_dir, &run_id);
    std::fs::create_dir_all(&rdir).map_err(|e| CliError::Io(e.to_string()))?;
    let caps_path = rdir.join("capabilities.json");
    std::fs::write(
        &caps_path,
        serde_json::to_vec_pretty(&spec).map_err(|e| CliError::Io(e.to_string()))?,
    )
    .map_err(|e| CliError::Io(e.to_string()))?;

    // Lifecycle: in-process executor + sandboxed runtime provider.
    let provider = ProcessRuntimeProvider::new(
        launch.binary,
        RuntimeEndpoint::Unix(sock),
        connected.clone(),
    )
    .with_sandbox(SandboxPolicy {
        capabilities_file: caps_path,
    });
    let client = ExecutorClient::new(InMemExecutorTransport::new(Arc::new(provider), connected));

    client
        .create_runtime(
            &run_id,
            RuntimeConfig {
                working_dir: workdir.to_string_lossy().into_owned(),
            },
        )
        .await
        .map_err(|e| CliError::Executor(e.to_string()))?;
    let rt_transport = client
        .runtime_transport(&run_id)
        .await
        .map_err(|e| CliError::Executor(e.to_string()))?;
    let runtime_client = RuntimeClient::from_arc(rt_transport);

    // Persist the manifest (no secrets) so `resume` can rebuild. `rdir` was created
    // above when writing the capability spec.
    let manifest = Manifest {
        workflow: def.clone(),
        workdir: workdir.clone(),
    };
    std::fs::write(
        rdir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).map_err(|e| CliError::Io(e.to_string()))?,
    )
    .map_err(|e| CliError::Io(e.to_string()))?;

    // Two planes: TerminalSink streams AgentEvents; this channel carries control flow.
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let ctx = WorkflowRuntimeContext {
        provider_registry: registry,
        toolbox_factory: Arc::new(DefaultToolboxFactory),
        runtime_client,
        event_sink: Arc::new(TerminalSink),
        workflow_events: tx,
    };
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(root_dir));
    let wf = spawn_root(WorkflowActor::new(run_id.clone(), def, ctx), journal);
    wf.tell(kickoff)
        .await
        .map_err(|e| CliError::Executor(e.to_string()))?;

    // Every `WorkflowNotification` is terminal for the CLI (there are no progress
    // notifications), so a single recv suffices — no loop. A dropped sender (None)
    // means the actor stopped without a transition; exit rather than hang.
    let exit = match rx.recv().await {
        Some(WorkflowNotification::AwaitingUserInput { question }) => {
            println!("\n⏸ awaiting input (run {run_id}):\n{question}");
            EXIT_AWAIT
        }
        Some(WorkflowNotification::Finished { output }) => {
            println!(
                "\n{}",
                serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())
            );
            0
        }
        Some(WorkflowNotification::Failed { error }) => {
            eprintln!("\n✗ failed: {error}");
            1
        }
        // Suspended (cancel or a recoverable failure) pauses the run; the CLI can't
        // resume in-process, so exit with the await code and let the user
        // `october resume` rather than block on a transition that will not come.
        Some(WorkflowNotification::Suspended) => {
            println!("\n⏸ suspended (run {run_id}): resume with `october resume --run {run_id}`");
            EXIT_AWAIT
        }
        None => 1,
    };
    let _ = client.destroy_runtime(&run_id).await;
    cancel.cancel();
    Ok(exit)
}

pub async fn run(p: RunParams) -> Result<i32, CliError> {
    let cfg = OctoberConfig::load(&p.config_path)?;
    let def = load_workflow(&p.workflow_path)?;
    let errs = validate(&def, &cfg);
    if !errs.is_empty() {
        return Err(CliError::Validation(errs.join("\n")));
    }
    let root_dir = p.state_dir.unwrap_or_else(|| cfg.storage.root_dir.clone());
    let run_id = Uuid::new_v4().to_string();
    println!("run {run_id}");
    drive(
        def,
        cfg,
        p.workdir,
        run_id,
        root_dir,
        RuntimeLaunch {
            binary: p.runtime_bin,
            capabilities_override: p.capabilities_path,
        },
        WorkflowCommand::Start { input: p.input },
    )
    .await
}

pub async fn resume(p: ResumeParams) -> Result<i32, CliError> {
    let cfg = OctoberConfig::load(&p.config_path)?;
    let root_dir = p.state_dir.unwrap_or_else(|| cfg.storage.root_dir.clone());
    let manifest_path = run_dir(&root_dir, &p.run_id).join("manifest.json");
    let manifest: Manifest = serde_json::from_slice(
        &std::fs::read(&manifest_path).map_err(|e| CliError::Io(e.to_string()))?,
    )
    .map_err(|e| CliError::Config(e.to_string()))?;
    drive(
        manifest.workflow,
        cfg,
        manifest.workdir,
        p.run_id,
        root_dir,
        RuntimeLaunch {
            binary: p.runtime_bin,
            // Resume reuses the config's capabilities_file; there is no resume-time flag.
            capabilities_override: None,
        },
        WorkflowCommand::Resume { message: p.message },
    )
    .await
}
