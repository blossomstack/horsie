//! The October daemon: a long-lived process that supervises parallel jobs and
//! serves a unix control socket. The CLI's `run`/`job` subcommands are thin
//! clients (see [`crate::client`]).

pub mod protocol;

use crate::capabilities;
use crate::config::{OctoberConfig, build_registry};
use crate::error::CliError;
use actor::{ActorRef, FileJournal, Journal, spawn_root};
use models::capabilities::CapabilitySpec;
use models::daemon::{
    AckResponse, DaemonRequest, DaemonResponse, EndResponse, ErrorResponse, JobListResponse,
    StatusInfo, SubmittedResponse,
};
use protocol::{read_frame, write_frame};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use supervisor::{JobSpec, ProcessJobRuntime, SupervisorActor, SupervisorCommand, SupervisorDeps};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, oneshot};

/// The control socket path under the state root.
pub fn socket_path(root: &Path) -> PathBuf {
    root.join("daemon.sock")
}

/// Locate the sibling `october-runtime` binary next to this executable — the
/// default when the config sets no explicit `runtime.bin`.
fn default_runtime_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("october-runtime")))
        .unwrap_or_else(|| PathBuf::from("october-runtime"))
}

fn pid_path(root: &Path) -> PathBuf {
    root.join("daemon.pid")
}

/// Whether a live daemon is accepting on `sock`. A successful connect means an
/// active accept loop; connection-refused or absent means a stale/absent socket.
async fn daemon_alive(sock: &Path) -> bool {
    UnixStream::connect(sock).await.is_ok()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Process-wide state shared with every connection handler.
struct Daemon {
    supervisor: ActorRef<SupervisorCommand>,
    default_caps: CapabilitySpec,
    /// Shared journal, used to replay a job's history for `logs`.
    journal: Arc<dyn Journal>,
    started: Instant,
    shutdown: Arc<Notify>,
}

/// Run the daemon in the foreground until a `Shutdown` request arrives. Builds the
/// shared dependencies, spawns the [`SupervisorActor`] (whose `on_recovery_complete`
/// auto-resumes interrupted jobs), binds the socket, and serves connections.
pub async fn serve(cfg: OctoberConfig) -> Result<(), CliError> {
    // Two distinct roots: ephemeral runtime state (socket, pidfile, log, per-job
    // capability files) lives under `state_dir`; the durable journal under `data_dir`.
    let state_dir = cfg.storage.state_dir.clone();
    let data_dir = cfg.storage.data_dir.clone();
    // The runtime binary: an explicit `runtime.bin` from config, else the sibling
    // `october-runtime` next to this executable.
    let runtime_bin = cfg.runtime.bin.clone().unwrap_or_else(default_runtime_bin);
    std::fs::create_dir_all(&state_dir).map_err(|e| CliError::Io(e.to_string()))?;
    std::fs::create_dir_all(&data_dir).map_err(|e| CliError::Io(e.to_string()))?;

    // Refuse to start if a daemon is already listening on the socket (a successful
    // connect means a live accept loop). Only a stale socket file — connect refused
    // or absent — is safe to unlink and rebind, which the bind path does below.
    if daemon_alive(&socket_path(&state_dir)).await {
        return Err(CliError::Executor(format!(
            "a daemon is already running for {}",
            state_dir.display()
        )));
    }

    let registry = build_registry(&cfg)?;
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(data_dir.clone()));
    let deps = SupervisorDeps {
        provider_registry: registry,
        runtime_bin,
        state_dir: state_dir.clone(),
        journal: journal.clone(),
    };
    let runtime = Arc::new(ProcessJobRuntime::new(deps));
    let supervisor = spawn_root(SupervisorActor::new(runtime), journal.clone());

    // Resolve the default capability spec once: `sandbox.capabilities_file` else the
    // platform built-in, with `~`/`$HOME` expanded. Submits without an explicit spec
    // use this.
    let default_caps = match &cfg.sandbox.capabilities_file {
        Some(path) => CapabilitySpec::load(path).map_err(CliError::Config)?,
        None => capabilities::builtin_default()?,
    };
    let default_caps = capabilities::resolve_user_paths(default_caps);

    let sock = socket_path(&state_dir);
    // Remove a stale socket so bind() succeeds after an unclean shutdown.
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).map_err(|e| CliError::Executor(e.to_string()))?;
    std::fs::write(pid_path(&state_dir), std::process::id().to_string())
        .map_err(|e| CliError::Io(e.to_string()))?;
    println!("october daemon listening on {}", sock.display());

    let daemon = Arc::new(Daemon {
        supervisor,
        default_caps,
        journal,
        started: Instant::now(),
        shutdown: Arc::new(Notify::new()),
    });

    loop {
        tokio::select! {
            _ = daemon.shutdown.notified() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let d = daemon.clone();
                        tokio::spawn(async move { handle_conn(stream, d).await });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                    }
                }
            }
        }
    }

    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(pid_path(&state_dir));
    Ok(())
}

async fn handle_conn(stream: UnixStream, daemon: Arc<Daemon>) {
    let (mut rd, mut wr) = stream.into_split();
    let req: DaemonRequest = match read_frame(&mut rd).await {
        Ok(Some(r)) => r,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read request");
            return;
        }
    };

    let result = match req {
        DaemonRequest::Submit(s) => {
            let caps = capabilities::resolve_user_paths(
                s.capabilities
                    .unwrap_or_else(|| daemon.default_caps.clone()),
            );
            let spec = JobSpec {
                workflow: s.workflow,
                workflow_name: s.workflow_name,
                workdir: PathBuf::from(s.workdir),
                input: s.input,
                capabilities: caps,
            };
            let (tx, rx) = oneshot::channel();
            let _ = daemon
                .supervisor
                .tell(SupervisorCommand::Submit {
                    spec,
                    submitted_at: now_millis(),
                    reply: tx,
                })
                .await;
            match rx.await {
                Ok(job_id) => write_frame(
                    &mut wr,
                    &DaemonResponse::Submitted(SubmittedResponse { job_id }),
                )
                .await
                .is_ok(),
                Err(_) => write_err(&mut wr, "submit failed").await,
            }
        }
        DaemonRequest::List(_) => {
            let jobs = list_jobs(&daemon).await;
            write_frame(&mut wr, &DaemonResponse::JobList(JobListResponse { jobs }))
                .await
                .is_ok()
        }
        DaemonRequest::Status(_) => {
            let jobs = list_jobs(&daemon).await;
            let mut info = StatusInfo {
                pid: std::process::id(),
                uptime_secs: daemon.started.elapsed().as_secs(),
                running: 0,
                suspended: 0,
                finished: 0,
                failed: 0,
            };
            for j in &jobs {
                use models::daemon::JobStatus::{
                    AwaitingUserInput, Failed, Finished, Running, Suspended,
                };
                match j.status {
                    Running => info.running += 1,
                    Suspended | AwaitingUserInput => info.suspended += 1,
                    Finished => info.finished += 1,
                    Failed => info.failed += 1,
                }
            }
            write_frame(&mut wr, &DaemonResponse::Status(info))
                .await
                .is_ok()
        }
        DaemonRequest::Stop(s) => {
            let _ = daemon
                .supervisor
                .tell(SupervisorCommand::Stop { job_id: s.job_id })
                .await;
            write_ack(&mut wr).await
        }
        DaemonRequest::Resume(s) => {
            let _ = daemon
                .supervisor
                .tell(SupervisorCommand::Resume {
                    job_id: s.job_id,
                    message: s.message,
                })
                .await;
            write_ack(&mut wr).await
        }
        DaemonRequest::Remove(s) => {
            let (tx, rx) = oneshot::channel();
            let _ = daemon
                .supervisor
                .tell(SupervisorCommand::Remove {
                    job_id: s.job_id,
                    reply: tx,
                })
                .await;
            match rx.await {
                Ok(Ok(())) => write_ack(&mut wr).await,
                Ok(Err(msg)) => write_err(&mut wr, &msg).await,
                Err(_) => write_err(&mut wr, "remove failed").await,
            }
        }
        DaemonRequest::Logs(s) => stream_logs(&mut wr, &daemon, s.job_id, s.follow).await,
        DaemonRequest::Shutdown(s) => {
            if s.drain {
                drain_running(&daemon).await;
            }
            // Tear down every live job's runtime child before exiting so no
            // october-runtime process is orphaned. Jobs keep their persisted status
            // and auto-resume on the next start.
            let (tx, rx) = oneshot::channel();
            let _ = daemon
                .supervisor
                .tell(SupervisorCommand::Shutdown { reply: tx })
                .await;
            let _ = rx.await;
            let ok = write_ack(&mut wr).await;
            daemon.shutdown.notify_one();
            ok
        }
    };
    if !result {
        tracing::debug!("connection closed before response fully written");
    }
}

/// Block until no job is `Running`. Suspended / awaiting-input jobs don't hold
/// active compute and would otherwise wait on a human, so they don't delay a drain;
/// they keep their status and auto-resume next start.
async fn drain_running(daemon: &Daemon) {
    use models::daemon::JobStatus;
    loop {
        let running = list_jobs(daemon)
            .await
            .iter()
            .filter(|j| matches!(j.status, JobStatus::Running))
            .count();
        if running == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

async fn list_jobs(daemon: &Daemon) -> Vec<models::daemon::JobSummary> {
    let (tx, rx) = oneshot::channel();
    if daemon
        .supervisor
        .tell(SupervisorCommand::List { reply: tx })
        .await
        .is_err()
    {
        return Vec::new();
    }
    rx.await.unwrap_or_default()
}

/// Stream a job's logs: first the history replayed from its durable journals
/// (so this works for finished jobs too), then — with `follow` and a still-live
/// job — the live tail until the job ends. A small gap between history and the live
/// subscription is possible; acceptable for a log view.
async fn stream_logs<W>(wr: &mut W, daemon: &Daemon, job_id: String, follow: bool) -> bool
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    for frame in supervisor::render_history(&daemon.journal, &job_id).await {
        if write_frame(wr, &DaemonResponse::LogFrame(frame))
            .await
            .is_err()
        {
            return false;
        }
    }

    if follow {
        let (tx, rx) = oneshot::channel();
        if daemon
            .supervisor
            .tell(SupervisorCommand::Subscribe { job_id, reply: tx })
            .await
            .is_ok()
            && let Some(mut sub) = rx.await.ok().flatten()
        {
            loop {
                match sub.recv().await {
                    Ok(frame) => {
                        if write_frame(wr, &DaemonResponse::LogFrame(frame))
                            .await
                            .is_err()
                        {
                            return false;
                        }
                    }
                    // Lagged: skip dropped frames and keep going. Closed: job ended.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    write_frame(wr, &DaemonResponse::End(EndResponse {}))
        .await
        .is_ok()
}

async fn write_ack<W>(wr: &mut W) -> bool
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    write_frame(wr, &DaemonResponse::Ack(AckResponse {}))
        .await
        .is_ok()
}

async fn write_err<W>(wr: &mut W, message: &str) -> bool
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    write_frame(
        wr,
        &DaemonResponse::Error(ErrorResponse {
            message: message.to_string(),
        }),
    )
    .await
    .is_ok()
}
