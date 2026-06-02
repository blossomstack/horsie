use clap::{Parser, Subcommand};
use cli::client;
use cli::config::HorsieConfig;
use cli::daemon;
use cli::error::CliError;
use cli::validate::validate;
use models::capabilities::CapabilitySpec;
use models::daemon::SubmitRequest;
use models::workflow::WorkflowDefinition;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "horsie",
    version,
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
        /// Config path. Omit to use `$XDG_CONFIG_HOME/horsie/config.json`
        /// (else `~/.config/horsie/config.json`), or an empty config if absent.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Run and manage jobs on the running daemon.
    Job {
        #[command(subcommand)]
        action: JobAction,
    },
    /// Manage the shared plugin library (skills + SessionStart hooks for all jobs).
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
}

#[derive(Subcommand)]
enum PluginAction {
    /// Install a plugin by cloning its git repo into the shared library.
    Install {
        /// Git URL of the plugin repo (e.g. https://github.com/obra/superpowers).
        url: String,
        /// Install name (default: derived from the URL).
        #[arg(long)]
        name: Option<String>,
        /// Git ref/branch to check out.
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Reinstall over an existing plugin of the same name.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List installed plugins.
    List {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Update an installed plugin (git pull).
    Update {
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Remove an installed plugin.
    Remove {
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// Resolve the shared plugin library dir from config (`storage.plugins_dir`).
fn resolve_plugins_dir(config: Option<&Path>) -> Result<PathBuf, CliError> {
    Ok(HorsieConfig::resolve(config)?.storage.plugins_dir)
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon (auto-resumes interrupted jobs). Foreground by default;
    /// `--background` detaches it with output redirected to `<state>/daemon.log`.
    Start {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Run detached in the background instead of the foreground.
        #[arg(long)]
        background: bool,
    },
    /// Stop the running daemon. In-progress jobs stay Running and auto-resume on
    /// the next start.
    Stop {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Wait for running jobs to finish before the daemon exits.
        #[arg(long)]
        drain: bool,
    },
    /// Show daemon status: pid, uptime, and job counts by status.
    Status {
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum JobAction {
    /// Submit a workflow to the running daemon as a job and stream it.
    Run {
        #[arg(long)]
        workflow: PathBuf,
        /// Config path. Omit to use `$XDG_CONFIG_HOME/horsie/config.json`
        /// (else `~/.config/horsie/config.json`), or an empty config if absent.
        #[arg(long)]
        config: Option<PathBuf>,
        /// One or more workspace roots, comma-separated (e.g. `./api,./web,../shared`).
        /// A single value is the common case. Paths cannot contain commas.
        #[arg(long, value_delimiter = ',', required = true)]
        workdir: Vec<PathBuf>,
        #[arg(long)]
        input: String,
        /// Capability file fully replacing the runtime's built-in sandbox default.
        /// Overrides `sandbox.capabilities_file` in the config.
        #[arg(long)]
        capabilities: Option<PathBuf>,
        /// Submit and return the job id without streaming output.
        #[arg(long)]
        detach: bool,
    },
    /// List all jobs known to the daemon.
    List {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Show a job's workflow execution progress (per-agent, with timing).
    Status {
        job_id: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Stream a job's live output.
    Logs {
        job_id: String,
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Cancel a running job (it becomes resumable).
    Stop {
        job_id: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Resume a suspended or awaiting-input job with a message.
    Resume {
        job_id: String,
        #[arg(short = 'm', long)]
        message: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Remove a finished or failed job from the registry.
    Remove {
        job_id: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// Resolve the state dir (where the daemon control socket lives) from config:
/// `storage.state_dir`, defaulting to `$XDG_STATE_HOME/horsie`. Every client
/// command connects to the socket under this dir.
fn resolve_state_dir(config: Option<&Path>) -> Result<PathBuf, CliError> {
    Ok(HorsieConfig::resolve(config)?.storage.state_dir)
}

fn load_workflow(path: &Path) -> Result<WorkflowDefinition, CliError> {
    let text = std::fs::read_to_string(path).map_err(|e| CliError::Io(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| CliError::Config(e.to_string()))
}

fn do_validate(workflow: PathBuf, config: Option<PathBuf>) -> i32 {
    let cfg = match HorsieConfig::resolve(config.as_deref()) {
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

/// Build a `SubmitRequest` from `job run` arguments, validating the workflow
/// against the config and resolving an explicit `--capabilities` file (the daemon
/// applies its default when `capabilities` is `None`).
fn build_submit(
    workflow: PathBuf,
    config: Option<PathBuf>,
    workdirs: Vec<PathBuf>,
    input: String,
    capabilities: Option<PathBuf>,
) -> Result<SubmitRequest, CliError> {
    let cfg = HorsieConfig::resolve(config.as_deref())?;
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
        workdirs: workdirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        input,
        capabilities: caps,
        workflow_name,
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Compact duration: `45s`, `4m12s`, `1h03m`. Input is millis.
fn humanize(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Label for the single `Active` agent row, derived from the overall job status.
fn active_label(status: &models::daemon::JobStatus) -> &'static str {
    use models::daemon::JobStatus;
    match status {
        JobStatus::Running => "working",
        JobStatus::AwaitingUserInput => "awaiting input",
        JobStatus::Parked => "parked",
        JobStatus::Suspended => "suspended",
        JobStatus::Finished => "finished",
        JobStatus::Failed => "failed",
    }
}

/// Render a job's workflow progress as a header line plus one line per agent.
fn print_job_status(p: &models::daemon::JobProgress) {
    use models::daemon::AgentPhase;
    let now = now_ms();
    let overall = p.finished_at.unwrap_or(now).saturating_sub(p.submitted_at);
    println!(
        "job {} · workflow \"{}\" · {:?} · {}",
        p.job_id,
        p.workflow_name,
        p.status,
        humanize(overall)
    );
    println!();
    for a in &p.agents {
        let (marker, label) = match a.phase {
            AgentPhase::Done => ("✓", "finished"),
            AgentPhase::Pending => ("·", "pending"),
            AgentPhase::Active => ("▸", active_label(&p.status)),
        };
        let dur = match a.started_at {
            Some(start) => humanize(a.ended_at.unwrap_or(now).saturating_sub(start)),
            None => "—".to_string(),
        };
        println!("  {marker} {:<12} {:<16} {}", a.name, label, dur);
    }
}

async fn dispatch(command: Command) -> Result<i32, CliError> {
    match command {
        Command::Validate { workflow, config } => Ok(do_validate(workflow, config)),
        Command::Daemon { action } => match action {
            DaemonAction::Start { config, background } => {
                let cfg = HorsieConfig::resolve(config.as_deref())?;
                if background {
                    let state_dir = cfg.storage.state_dir.clone();
                    spawn_background_daemon(&state_dir, config.as_deref())?;
                    println!(
                        "daemon started in background ({}/daemon.log)",
                        state_dir.display()
                    );
                    Ok(0)
                } else {
                    daemon::serve(cfg).await?;
                    Ok(0)
                }
            }
            DaemonAction::Stop { config, drain } => {
                let root = resolve_state_dir(config.as_deref())?;
                client::shutdown(&root, drain).await?;
                println!("daemon stopped");
                Ok(0)
            }
            DaemonAction::Status { config } => {
                let root = resolve_state_dir(config.as_deref())?;
                let s = client::status(&root).await?;
                println!(
                    "pid {} · up {}s · running {} · parked {} · suspended {} · finished {} · failed {}",
                    s.pid, s.uptime_secs, s.running, s.parked, s.suspended, s.finished, s.failed
                );
                Ok(0)
            }
        },
        Command::Job { action } => match action {
            JobAction::Run {
                workflow,
                config,
                workdir,
                input,
                capabilities,
                detach,
            } => {
                let root = resolve_state_dir(config.as_deref())?;
                let req = build_submit(workflow, config, workdir, input, capabilities)?;
                if detach {
                    let job_id = client::submit(&root, req).await?;
                    println!("job {job_id}");
                    Ok(0)
                } else {
                    client::run_attached(&root, req).await
                }
            }
            JobAction::Status { job_id, config } => {
                let root = resolve_state_dir(config.as_deref())?;
                let p = client::job_status(&root, job_id).await?;
                print_job_status(&p);
                Ok(0)
            }
            JobAction::List { config } => {
                let root = resolve_state_dir(config.as_deref())?;
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
            } => {
                let root = resolve_state_dir(config.as_deref())?;
                client::logs(&root, job_id, follow).await?;
                Ok(0)
            }
            JobAction::Stop { job_id, config } => {
                let root = resolve_state_dir(config.as_deref())?;
                client::stop(&root, job_id).await?;
                println!("stopped");
                Ok(0)
            }
            JobAction::Resume {
                job_id,
                message,
                config,
            } => {
                let root = resolve_state_dir(config.as_deref())?;
                client::resume(&root, job_id, message).await?;
                println!("resumed");
                Ok(0)
            }
            JobAction::Remove { job_id, config } => {
                let root = resolve_state_dir(config.as_deref())?;
                client::remove(&root, job_id).await?;
                println!("removed");
                Ok(0)
            }
        },
        Command::Plugin { action } => match action {
            PluginAction::Install {
                url,
                name,
                git_ref,
                force,
                config,
            } => {
                let dir = resolve_plugins_dir(config.as_deref())?;
                let installed = cli::plugins::install(&dir, &url, name, git_ref, force)?;
                println!("installed plugin '{installed}' into {}", dir.display());
                Ok(0)
            }
            PluginAction::List { config } => {
                let dir = resolve_plugins_dir(config.as_deref())?;
                let plugins = cli::plugins::list(&dir);
                if plugins.is_empty() {
                    println!("no plugins installed");
                } else {
                    println!("{:<24} {:<10} SOURCE", "NAME", "VERSION");
                    for p in plugins {
                        println!(
                            "{:<24} {:<10} {}",
                            p.name,
                            p.version.as_deref().unwrap_or("-"),
                            p.source
                        );
                    }
                }
                Ok(0)
            }
            PluginAction::Update { name, config } => {
                let dir = resolve_plugins_dir(config.as_deref())?;
                cli::plugins::update(&dir, &name)?;
                println!("updated plugin '{name}'");
                Ok(0)
            }
            PluginAction::Remove { name, config } => {
                let dir = resolve_plugins_dir(config.as_deref())?;
                cli::plugins::remove(&dir, &name)?;
                println!("removed plugin '{name}'");
                Ok(0)
            }
        },
    }
}

/// Re-exec this binary as `horsie daemon start` (foreground) detached from the
/// terminal, with stdout/stderr redirected to `<state_dir>/daemon.log`, so the
/// parent returns immediately. The child re-resolves its state/data dirs from
/// `--config` (or the XDG defaults), deterministically landing on the same
/// `state_dir` the parent computed for the log path.
fn spawn_background_daemon(state_dir: &Path, config: Option<&Path>) -> Result<(), CliError> {
    use std::process::{Command, Stdio};
    std::fs::create_dir_all(state_dir).map_err(|e| CliError::Io(e.to_string()))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join("daemon.log"))
        .map_err(|e| CliError::Io(e.to_string()))?;
    let err_log = log.try_clone().map_err(|e| CliError::Io(e.to_string()))?;
    let exe = std::env::current_exe().map_err(|e| CliError::Io(e.to_string()))?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").arg("start");
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
