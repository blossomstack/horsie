use clap::{Parser, Subcommand};
use cli::client;
use cli::config::OctoberConfig;
use cli::daemon;
use cli::error::CliError;
use cli::validate::validate;
use models::capabilities::CapabilitySpec;
use models::daemon::SubmitRequest;
use models::workflow::WorkflowDefinition;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "october",
    about = "Run agent workflows in a nono-sandboxed runtime, supervised by a local daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate a workflow against a config; report all errors.
    Validate {
        #[arg(long)]
        workflow: PathBuf,
        /// Config path. Omit to use `$XDG_CONFIG_HOME/october/config.json`
        /// (else `~/.config/october/config.json`), or an empty config if absent.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Submit a workflow to the running daemon as a job and stream it.
    Run {
        #[arg(long)]
        workflow: PathBuf,
        /// Config path. Omit to use `$XDG_CONFIG_HOME/october/config.json`
        /// (else `~/.config/october/config.json`), or an empty config if absent.
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        workdir: PathBuf,
        #[arg(long)]
        input: String,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Capability file fully replacing the runtime's built-in sandbox default.
        /// Overrides `sandbox.capabilities_file` in the config.
        #[arg(long)]
        capabilities: Option<PathBuf>,
        /// Submit and return the job id without streaming output.
        #[arg(long)]
        detach: bool,
    },
    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Manage jobs on the running daemon.
    Job {
        #[command(subcommand)]
        action: JobAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon (auto-resumes interrupted jobs). Foreground by default;
    /// `--background` detaches it with output redirected to `<state>/daemon.log`.
    Start {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Run detached in the background instead of the foreground.
        #[arg(long)]
        background: bool,
    },
    /// Stop the running daemon. In-progress jobs stay Running and auto-resume on
    /// the next start.
    Stop {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Wait for running jobs to finish before the daemon exits.
        #[arg(long)]
        drain: bool,
    },
    /// Show daemon status: pid, uptime, and job counts by status.
    Status {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum JobAction {
    /// List all jobs known to the daemon.
    List {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Stream a job's live output.
    Logs {
        job_id: String,
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Cancel a running job (it becomes resumable).
    Stop {
        job_id: String,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Resume a suspended or awaiting-input job with a message.
    Resume {
        job_id: String,
        #[arg(short = 'm', long)]
        message: String,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Remove a finished or failed job from the registry.
    Remove {
        job_id: String,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

/// Locate the sibling `october-runtime` binary next to this executable.
fn runtime_binary_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("october-runtime")))
        .unwrap_or_else(|| PathBuf::from("october-runtime"))
}

/// Resolve the state root: `--state-dir` if given, else config `storage.root_dir`.
fn resolve_root(state_dir: Option<PathBuf>, config: Option<&Path>) -> Result<PathBuf, CliError> {
    match state_dir {
        Some(dir) => Ok(dir),
        None => Ok(OctoberConfig::resolve(config)?.storage.root_dir),
    }
}

fn load_workflow(path: &Path) -> Result<WorkflowDefinition, CliError> {
    let text = std::fs::read_to_string(path).map_err(|e| CliError::Io(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| CliError::Config(e.to_string()))
}

fn do_validate(workflow: PathBuf, config: Option<PathBuf>) -> i32 {
    let cfg = match OctoberConfig::resolve(config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let text = match std::fs::read_to_string(&workflow) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("failed to read workflow: {e}");
            return 2;
        }
    };
    let def: WorkflowDefinition = match serde_json::from_str(&text) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("workflow parse error: {e}");
            return 2;
        }
    };
    let errs = validate(&def, &cfg);
    if errs.is_empty() {
        println!("valid");
        0
    } else {
        for e in &errs {
            eprintln!("✗ {e}");
        }
        1
    }
}

/// Build a `SubmitRequest` from `run`/submit arguments, validating the workflow
/// against the config and resolving an explicit `--capabilities` file (the daemon
/// applies its default when `capabilities` is `None`).
fn build_submit(
    workflow: PathBuf,
    config: Option<PathBuf>,
    workdir: PathBuf,
    input: String,
    capabilities: Option<PathBuf>,
) -> Result<SubmitRequest, CliError> {
    let cfg = OctoberConfig::resolve(config.as_deref())?;
    let def = load_workflow(&workflow)?;
    let errs = validate(&def, &cfg);
    if !errs.is_empty() {
        return Err(CliError::Validation(errs.join("\n")));
    }
    let caps: Option<CapabilitySpec> = match capabilities {
        Some(path) => Some(CapabilitySpec::load(&path).map_err(CliError::Config)?),
        None => None,
    };
    let workflow_name = workflow
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workflow".to_string());
    Ok(SubmitRequest {
        workflow: def,
        workdir: workdir.to_string_lossy().into_owned(),
        input,
        capabilities: caps,
        workflow_name,
    })
}

async fn dispatch(command: Command) -> Result<i32, CliError> {
    match command {
        Command::Validate { workflow, config } => Ok(do_validate(workflow, config)),
        Command::Run {
            workflow,
            config,
            workdir,
            input,
            state_dir,
            capabilities,
            detach,
        } => {
            let root = resolve_root(state_dir, config.as_deref())?;
            let req = build_submit(workflow, config, workdir, input, capabilities)?;
            if detach {
                let job_id = client::submit(&root, req).await?;
                println!("job {job_id}");
                Ok(0)
            } else {
                client::run_attached(&root, req).await
            }
        }
        Command::Daemon { action } => match action {
            DaemonAction::Start {
                config,
                state_dir,
                background,
            } => {
                let cfg = OctoberConfig::resolve(config.as_deref())?;
                let root = match state_dir {
                    Some(dir) => dir,
                    None => cfg.storage.root_dir.clone(),
                };
                if background {
                    spawn_background_daemon(&root, config.as_deref())?;
                    println!(
                        "daemon started in background ({}/daemon.log)",
                        root.display()
                    );
                    Ok(0)
                } else {
                    daemon::serve(cfg, root, runtime_binary_path()).await?;
                    Ok(0)
                }
            }
            DaemonAction::Stop {
                config,
                state_dir,
                drain,
            } => {
                let root = resolve_root(state_dir, config.as_deref())?;
                client::shutdown(&root, drain).await?;
                println!("daemon stopped");
                Ok(0)
            }
            DaemonAction::Status { config, state_dir } => {
                let root = resolve_root(state_dir, config.as_deref())?;
                let s = client::status(&root).await?;
                println!(
                    "pid {} · up {}s · running {} · suspended {} · finished {} · failed {}",
                    s.pid, s.uptime_secs, s.running, s.suspended, s.finished, s.failed
                );
                Ok(0)
            }
        },
        Command::Job { action } => match action {
            JobAction::List { config, state_dir } => {
                let root = resolve_root(state_dir, config.as_deref())?;
                let jobs = client::list(&root).await?;
                if jobs.is_empty() {
                    println!("no jobs");
                } else {
                    println!("{:<38} {:<18} {:<12} WORKDIR", "JOB", "WORKFLOW", "STATUS");
                    for j in jobs {
                        println!(
                            "{:<38} {:<18} {:<12} {}",
                            j.job_id,
                            j.workflow_name,
                            format!("{:?}", j.status),
                            j.workdir
                        );
                    }
                }
                Ok(0)
            }
            JobAction::Logs {
                job_id,
                follow,
                config,
                state_dir,
            } => {
                let root = resolve_root(state_dir, config.as_deref())?;
                client::logs(&root, job_id, follow).await?;
                Ok(0)
            }
            JobAction::Stop {
                job_id,
                config,
                state_dir,
            } => {
                let root = resolve_root(state_dir, config.as_deref())?;
                client::stop(&root, job_id).await?;
                println!("stopped");
                Ok(0)
            }
            JobAction::Resume {
                job_id,
                message,
                config,
                state_dir,
            } => {
                let root = resolve_root(state_dir, config.as_deref())?;
                client::resume(&root, job_id, message).await?;
                println!("resumed");
                Ok(0)
            }
            JobAction::Remove {
                job_id,
                config,
                state_dir,
            } => {
                let root = resolve_root(state_dir, config.as_deref())?;
                client::remove(&root, job_id).await?;
                println!("removed");
                Ok(0)
            }
        },
    }
}

/// Re-exec this binary as `october daemon start` (foreground) detached from the
/// terminal, with stdout/stderr redirected to `<root>/daemon.log`, so the parent
/// returns immediately. Errors if a daemon is already running (the child's
/// liveness guard would fail, but we check here too for a clean message).
fn spawn_background_daemon(root: &Path, config: Option<&Path>) -> Result<(), CliError> {
    use std::process::{Command, Stdio};
    std::fs::create_dir_all(root).map_err(|e| CliError::Io(e.to_string()))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("daemon.log"))
        .map_err(|e| CliError::Io(e.to_string()))?;
    let err_log = log.try_clone().map_err(|e| CliError::Io(e.to_string()))?;
    let exe = std::env::current_exe().map_err(|e| CliError::Io(e.to_string()))?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").arg("start").arg("--state-dir").arg(root);
    if let Some(c) = config {
        cmd.arg("--config").arg(c);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log));
    cmd.spawn().map_err(|e| CliError::Executor(e.to_string()))?;
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match dispatch(cli.command).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    };
    std::process::exit(code);
}
