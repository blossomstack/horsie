use crate::spec::{JobId, JobSpec, SupervisorDeps};
use crate::supervisor_actor::SupervisorCommand;
use actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId, spawn_root};
use agentcore::{AgentEvent, EventSink};
use async_trait::async_trait;
use executor::{
    ConnectedRuntimeRegistry, InMemExecutorTransport, ProcessRuntimeProvider, RuntimeEndpoint,
    RuntimeListenerServer, SandboxPolicy, serve_runtime_connections,
};
use executor_client::ExecutorClient;
use models::daemon::{JobEventFrame, JobStatus};
use models::executor::RuntimeConfig;
use runtime_client::RuntimeClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use workflow::{
    DefaultToolboxFactory, WorkflowActor, WorkflowCommand, WorkflowNotification,
    WorkflowRuntimeContext,
};

/// Capacity of a job's live log broadcast. Slow subscribers see `lagged` drops.
const LOG_BROADCAST_CAPACITY: usize = 256;
/// Capacity of the workflow→job notification channel.
const NOTIFY_CHANNEL_CAPACITY: usize = 256;

/// How to (re)start a job's workflow when launching it.
pub enum Kickoff {
    /// Fresh submit — start at the workflow's start agent with the job input.
    Start,
    /// Resume a suspended/awaiting workflow with a user message.
    Resume(String),
    /// Recover after a daemon restart — spawn the workflow and let its
    /// `on_recovery_complete` re-drive the interrupted turn (no command sent).
    Recover,
}

/// Inputs to [`JobRuntime::launch`].
pub struct LaunchParams {
    pub job_id: JobId,
    pub spec: JobSpec,
    pub kickoff: Kickoff,
    /// Sink the workflow pushes status transitions into.
    pub events: mpsc::Sender<WorkflowNotification>,
    /// Broadcast the job's render-friendly log frames flow into.
    pub logs: broadcast::Sender<JobEventFrame>,
}

/// A launched workflow plus the means to tear down its resources.
pub struct LaunchedJob {
    pub workflow: ActorRef<WorkflowCommand>,
    pub shutdown: Arc<dyn JobShutdown>,
}

/// Releases the OS resources a launched job acquired (runtime child, listener).
#[async_trait]
pub trait JobShutdown: Send + Sync {
    async fn shutdown(&self);
}

/// Provisions the execution environment for a job and spawns its workflow.
/// Abstracted so the supervisor can be driven in tests without a real sandbox.
#[async_trait]
pub trait JobRuntime: Send + Sync + 'static {
    async fn launch(&self, params: LaunchParams) -> Result<LaunchedJob, String>;
}

// ── Production runtime: the executor + sandboxed child assembly ──────────────

/// The production [`JobRuntime`] — assembles the in-process executor and a
/// sandboxed `october-runtime` child per job (the logic that used to live in
/// `cli::run::drive`).
pub struct ProcessJobRuntime {
    deps: SupervisorDeps,
}

impl ProcessJobRuntime {
    pub fn new(deps: SupervisorDeps) -> Self {
        Self { deps }
    }
}

/// Ephemeral unix socket for one executor assembly, kept short (sockaddr_un caps
/// the path at ~108 bytes) and unique per call so concurrent jobs never collide.
fn socket_path() -> Result<PathBuf, String> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let path = std::env::temp_dir()
        .join(format!("october-{}", &token[..12]))
        .join("rt.sock");
    let max = if cfg!(target_os = "macos") { 103 } else { 107 };
    if path.as_os_str().len() > max {
        return Err(format!(
            "unix socket path too long ({} bytes, max {max}): {}",
            path.as_os_str().len(),
            path.display()
        ));
    }
    Ok(path)
}

#[async_trait]
impl JobRuntime for ProcessJobRuntime {
    async fn launch(&self, params: LaunchParams) -> Result<LaunchedJob, String> {
        let LaunchParams {
            job_id,
            spec,
            kickoff,
            events,
            logs,
        } = params;

        // Runtime listener (unix) + connected registry; the accept loop registers
        // the direct transport for each runtime that connects.
        let connected = Arc::new(ConnectedRuntimeRegistry::new());
        let sock = socket_path()?;
        let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Unix(sock.clone()))
            .await
            .map_err(|e| e.to_string())?;
        let cancel = CancellationToken::new();
        serve_runtime_connections(listener, connected.clone(), cancel.clone());

        // Persist the resolved capability spec into the job dir so the runtime loads
        // a single source of truth (`jobs/<id>/capabilities.json`).
        let jdir = self.deps.state_dir.join("jobs").join(&job_id);
        std::fs::create_dir_all(&jdir).map_err(|e| e.to_string())?;
        let caps_path = jdir.join("capabilities.json");
        std::fs::write(
            &caps_path,
            serde_json::to_vec_pretty(&spec.capabilities).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

        let provider = ProcessRuntimeProvider::new(
            self.deps.runtime_bin.clone(),
            RuntimeEndpoint::Unix(sock),
            connected.clone(),
        )
        .with_sandbox(SandboxPolicy {
            capabilities_file: caps_path,
        });
        let client =
            ExecutorClient::new(InMemExecutorTransport::new(Arc::new(provider), connected));
        client
            .create_runtime(
                &job_id,
                RuntimeConfig {
                    working_dir: spec.workdir.to_string_lossy().into_owned(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        let rt_transport = client
            .runtime_transport(&job_id)
            .await
            .map_err(|e| e.to_string())?;
        let runtime_client = RuntimeClient::from_arc(rt_transport);

        let ctx = WorkflowRuntimeContext {
            provider_registry: self.deps.provider_registry.clone(),
            toolbox_factory: Arc::new(DefaultToolboxFactory),
            runtime_client,
            event_sink: Arc::new(BroadcastSink {
                job_id: job_id.clone(),
                tx: logs,
            }),
            workflow_events: events,
        };
        let wf = spawn_root(
            WorkflowActor::new(job_id.clone(), spec.workflow.clone(), ctx),
            self.deps.journal.clone(),
        );
        send_kickoff(&wf, kickoff, spec.input).await?;

        Ok(LaunchedJob {
            workflow: wf,
            shutdown: Arc::new(ProcessShutdown {
                client,
                cancel,
                runtime_id: job_id,
            }),
        })
    }
}

/// Send the appropriate first command to a freshly-spawned workflow actor.
/// `Recover` sends nothing — the recovered actor self-continues via its
/// `on_recovery_complete`.
async fn send_kickoff(
    wf: &ActorRef<WorkflowCommand>,
    kickoff: Kickoff,
    input: String,
) -> Result<(), String> {
    match kickoff {
        Kickoff::Start => wf
            .tell(WorkflowCommand::Start { input })
            .await
            .map_err(|e| e.to_string()),
        Kickoff::Resume(message) => wf
            .tell(WorkflowCommand::Resume { message })
            .await
            .map_err(|e| e.to_string()),
        Kickoff::Recover => Ok(()),
    }
}

struct ProcessShutdown {
    client: ExecutorClient,
    cancel: CancellationToken,
    runtime_id: String,
}

#[async_trait]
impl JobShutdown for ProcessShutdown {
    async fn shutdown(&self) {
        let _ = self.client.destroy_runtime(&self.runtime_id).await;
        self.cancel.cancel();
    }
}

/// Publishes render-friendly log frames to the job's broadcast channel. A live
/// `october job logs --follow` subscriber tails these; the journal stays the
/// durable record.
struct BroadcastSink {
    job_id: String,
    tx: broadcast::Sender<JobEventFrame>,
}

impl EventSink for BroadcastSink {
    fn emit(&self, event: AgentEvent) {
        if let Some(text) = render_event(&event) {
            let _ = self.tx.send(JobEventFrame {
                job_id: self.job_id.clone(),
                text,
            });
        }
    }
}

/// Render an agent event to a terminal-friendly log line, or `None` for events
/// not surfaced in logs. Mirrors the CLI's `TerminalSink`.
pub fn render_event(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::TextChunk(e) => Some(e.text.clone()),
        AgentEvent::ToolCallStart(e) => Some(format!("\n· tool {} [{}]\n", e.name, e.tool_call_id)),
        AgentEvent::ToolComplete(e) => Some(format!(
            "· tool {} → {}\n",
            e.tool_call_id,
            if e.is_error { "error" } else { "ok" }
        )),
        AgentEvent::RunComplete(e) => Some(format!(
            "\n· run complete ({} iterations, {}/{} tokens)\n",
            e.iterations, e.usage.input_tokens, e.usage.output_tokens
        )),
        AgentEvent::InputMessage(_)
        | AgentEvent::MessageStart(_)
        | AgentEvent::MessageStop(_)
        | AgentEvent::MessageComplete(_)
        | AgentEvent::ThinkingChunk(_)
        | AgentEvent::ToolCallInputDelta(_)
        | AgentEvent::ToolCallInputDone(_)
        | AgentEvent::ToolExecuting(_) => None,
    }
}

// ── The JobActor ────────────────────────────────────────────────────────────

/// Commands accepted by a [`JobActor`].
pub enum JobCommand {
    /// Begin executing the job (fresh submit).
    Start,
    /// Resume a suspended/awaiting job with a user message.
    Resume { message: String },
    /// Cancel the in-flight run (→ Suspended).
    Stop,
    /// Hand back a live log subscriber for `october job logs`.
    Subscribe {
        reply: oneshot::Sender<broadcast::Receiver<JobEventFrame>>,
    },
    /// Tear down OS resources (kill the runtime child) for a clean daemon shutdown,
    /// then stop. No terminal event is persisted, so the job stays in its current
    /// status and auto-resumes on the next daemon start. `reply` acks completion.
    Shutdown { reply: oneshot::Sender<()> },
    /// Internal: the workflow reported a terminal/await transition.
    WorkflowEvent(WorkflowNotification),
}

/// Events recording a job's lifecycle. Persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobDomainEvent {
    JobStarted,
    JobConcluded { output: Value },
    JobSuspended,
    JobAwaitingInput,
    JobFailed { error: String },
}

/// Persisted job state — purely a function of the event log. `status` is `None`
/// until the job's first event, distinguishing a freshly-spawned actor still
/// awaiting `Start` (do nothing on recovery) from one recovered mid-run (which
/// `on_recovery_complete` must re-drive).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobState {
    pub status: Option<JobStatus>,
}

/// Manages one job's lifecycle and resources: it owns the per-job log broadcast,
/// the live workflow handle, and the teardown hook for the sandboxed runtime. The
/// workflow orchestration itself stays in [`WorkflowActor`].
pub struct JobActor {
    job_id: JobId,
    spec: JobSpec,
    runtime: Arc<dyn JobRuntime>,
    parent: ActorRef<SupervisorCommand>,
    logs: broadcast::Sender<JobEventFrame>,
    workflow: Option<ActorRef<WorkflowCommand>>,
    shutdown: Option<Arc<dyn JobShutdown>>,
}

impl JobActor {
    pub fn new(
        job_id: JobId,
        spec: JobSpec,
        runtime: Arc<dyn JobRuntime>,
        parent: ActorRef<SupervisorCommand>,
    ) -> Self {
        let (logs, _) = broadcast::channel(LOG_BROADCAST_CAPACITY);
        Self {
            job_id,
            spec,
            runtime,
            parent,
            logs,
            workflow: None,
            shutdown: None,
        }
    }

    pub fn persistence_id_for(job_id: &str) -> PersistenceId {
        PersistenceId::new("job", job_id.to_string())
    }

    async fn report(&self, status: JobStatus) {
        let _ = self
            .parent
            .tell(SupervisorCommand::JobStatusChanged {
                job_id: self.job_id.clone(),
                status,
            })
            .await;
    }

    /// Launch (or relaunch) the workflow, wiring a forwarder that turns workflow
    /// notifications into [`JobCommand::WorkflowEvent`] back to this actor.
    async fn launch_workflow(
        &mut self,
        kickoff: Kickoff,
        ctx: &ActorContext<Self>,
    ) -> Result<(), String> {
        let (tx, mut rx) = mpsc::channel(NOTIFY_CHANNEL_CAPACITY);
        let self_ref = ctx.self_ref();
        tokio::spawn(async move {
            while let Some(n) = rx.recv().await {
                if self_ref.tell(JobCommand::WorkflowEvent(n)).await.is_err() {
                    break;
                }
            }
        });
        let params = LaunchParams {
            job_id: self.job_id.clone(),
            spec: self.spec.clone(),
            kickoff,
            events: tx,
            logs: self.logs.clone(),
        };
        let launched = self.runtime.launch(params).await?;
        self.workflow = Some(launched.workflow);
        self.shutdown = Some(launched.shutdown);
        Ok(())
    }

    /// Release the runtime resources of a finished/failed job.
    async fn teardown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.shutdown().await;
        }
        self.workflow = None;
    }

    async fn on_workflow_event(
        &mut self,
        notification: WorkflowNotification,
    ) -> CommandEffect<JobDomainEvent> {
        match notification {
            WorkflowNotification::Finished { output } => {
                self.teardown().await;
                self.report(JobStatus::Finished).await;
                CommandEffect::PersistAndStop(vec![JobDomainEvent::JobConcluded { output }])
            }
            WorkflowNotification::Failed { error } => {
                self.teardown().await;
                self.report(JobStatus::Failed).await;
                CommandEffect::PersistAndStop(vec![JobDomainEvent::JobFailed { error }])
            }
            // Suspended / awaiting keep the workflow + runtime alive so a live
            // `resume` re-drives the existing actor (no duplicate spawn).
            WorkflowNotification::Suspended => {
                self.report(JobStatus::Suspended).await;
                CommandEffect::Persist(vec![JobDomainEvent::JobSuspended])
            }
            WorkflowNotification::AwaitingUserInput { .. } => {
                self.report(JobStatus::AwaitingUserInput).await;
                CommandEffect::Persist(vec![JobDomainEvent::JobAwaitingInput])
            }
        }
    }
}

#[async_trait]
impl EventSourcedActor for JobActor {
    type Command = JobCommand;
    type Event = JobDomainEvent;
    type State = JobState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(&self.job_id)
    }

    fn initial_state() -> JobState {
        JobState::default()
    }

    fn apply_event(mut state: JobState, event: JobDomainEvent) -> JobState {
        state.status = Some(match event {
            JobDomainEvent::JobStarted => JobStatus::Running,
            JobDomainEvent::JobConcluded { .. } => JobStatus::Finished,
            JobDomainEvent::JobSuspended => JobStatus::Suspended,
            JobDomainEvent::JobAwaitingInput => JobStatus::AwaitingUserInput,
            JobDomainEvent::JobFailed { .. } => JobStatus::Failed,
        });
        state
    }

    async fn handle_command(
        &mut self,
        _state: &JobState,
        cmd: JobCommand,
        ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<JobDomainEvent> {
        match cmd {
            JobCommand::Start => match self.launch_workflow(Kickoff::Start, ctx).await {
                // Submit already recorded Running at the supervisor, so don't re-report.
                Ok(()) => CommandEffect::Persist(vec![JobDomainEvent::JobStarted]),
                Err(error) => {
                    self.report(JobStatus::Failed).await;
                    CommandEffect::PersistAndStop(vec![JobDomainEvent::JobFailed { error }])
                }
            },
            JobCommand::Resume { message } => {
                if let Some(wf) = &self.workflow {
                    let _ = wf.tell(WorkflowCommand::Resume { message }).await;
                    CommandEffect::None
                } else {
                    // Post-restart: no live workflow, so launch one fresh; it recovers
                    // its journal then handles the resume.
                    match self.launch_workflow(Kickoff::Resume(message), ctx).await {
                        Ok(()) => CommandEffect::None,
                        Err(error) => {
                            self.report(JobStatus::Failed).await;
                            CommandEffect::PersistAndStop(vec![JobDomainEvent::JobFailed { error }])
                        }
                    }
                }
            }
            JobCommand::Stop => {
                if let Some(wf) = &self.workflow {
                    let _ = wf.tell(WorkflowCommand::Cancel).await;
                }
                // The workflow's Suspended notification persists JobSuspended.
                CommandEffect::None
            }
            JobCommand::Subscribe { reply } => {
                let _ = reply.send(self.logs.subscribe());
                CommandEffect::None
            }
            JobCommand::Shutdown { reply } => {
                self.teardown().await;
                let _ = reply.send(());
                CommandEffect::Stop
            }
            JobCommand::WorkflowEvent(n) => self.on_workflow_event(n).await,
        }
    }

    /// After recovery, re-drive a job that was `Running` when the process died.
    /// A `None` status means the actor was just spawned for a fresh submit and is
    /// still awaiting `Start` — do nothing (else we'd double-launch). Paused jobs
    /// stay dormant (no sandbox child) until an explicit `Resume`.
    async fn on_recovery_complete(&mut self, state: &JobState, ctx: &mut ActorContext<Self>) {
        match state.status {
            Some(JobStatus::Running) => {
                if let Err(e) = self.launch_workflow(Kickoff::Recover, ctx).await {
                    tracing::error!(job_id = %self.job_id, error = %e, "failed to recover job");
                }
            }
            None
            | Some(JobStatus::Suspended)
            | Some(JobStatus::AwaitingUserInput)
            | Some(JobStatus::Finished)
            | Some(JobStatus::Failed) => {}
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    #[test]
    fn apply_event_sets_status() {
        let s = JobActor::apply_event(JobState::default(), JobDomainEvent::JobSuspended);
        assert_eq!(s.status, Some(JobStatus::Suspended));
        let s = JobActor::apply_event(s, JobDomainEvent::JobAwaitingInput);
        assert_eq!(s.status, Some(JobStatus::AwaitingUserInput));
        let s = JobActor::apply_event(
            s,
            JobDomainEvent::JobConcluded {
                output: Value::Null,
            },
        );
        assert_eq!(s.status, Some(JobStatus::Finished));
    }

    #[test]
    fn default_state_has_no_status() {
        assert_eq!(JobState::default().status, None);
    }

    #[test]
    fn failed_event_is_terminal_status() {
        let s = JobActor::apply_event(
            JobState::default(),
            JobDomainEvent::JobFailed {
                error: "boom".into(),
            },
        );
        assert_eq!(s.status, Some(JobStatus::Failed));
    }

    #[test]
    fn render_event_filters_non_log_events() {
        use models::events::MessageStartEvent;
        assert!(
            render_event(&AgentEvent::MessageStart(MessageStartEvent {
                message_id: "m".into(),
                role: models::agent::Role::Assistant,
            }))
            .is_none()
        );
    }
}
