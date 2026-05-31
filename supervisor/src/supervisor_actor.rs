use crate::job_actor::{JobActor, JobCommand, JobRuntime};
use crate::spec::{JobId, JobSpec};
use actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use async_trait::async_trait;
use models::daemon::{JobEventFrame, JobStatus, JobSummary};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::{broadcast, oneshot};

/// Commands accepted by the [`SupervisorActor`].
pub enum SupervisorCommand {
    /// Submit a new job; replies with its generated id.
    Submit {
        spec: JobSpec,
        /// Unix epoch millis (supplied by the caller for deterministic tests).
        submitted_at: u64,
        reply: oneshot::Sender<JobId>,
    },
    /// List all known jobs.
    List {
        reply: oneshot::Sender<Vec<JobSummary>>,
    },
    /// Cancel a running job (→ Suspended).
    Stop { job_id: JobId },
    /// Resume a suspended/awaiting job with a message.
    Resume { job_id: JobId, message: String },
    /// Remove a terminal (finished/failed) job from the registry. Errors if the
    /// job is still active or unknown.
    Remove {
        job_id: JobId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Hand back a live log subscriber for a job, or `None` if it has no live actor.
    Subscribe {
        job_id: JobId,
        reply: oneshot::Sender<Option<broadcast::Receiver<JobEventFrame>>>,
    },
    /// Tear down every live job's OS resources for a clean daemon shutdown; `reply`
    /// acks once all runtime children are gone. Jobs keep their persisted status so
    /// they auto-resume next start.
    Shutdown { reply: oneshot::Sender<()> },
    /// Internal: a job actor reports its status changed.
    JobStatusChanged { job_id: JobId, status: JobStatus },
}

/// Events recording the job registry. Persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SupervisorEvent {
    JobSubmitted {
        id: JobId,
        spec: JobSpec,
        submitted_at: u64,
    },
    JobStatusChanged {
        id: JobId,
        status: JobStatus,
    },
    JobRemoved {
        id: JobId,
    },
}

/// One registry row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub spec: JobSpec,
    pub status: JobStatus,
    pub submitted_at: u64,
}

/// Persisted supervisor state — the job registry, purely a function of events.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SupervisorState {
    pub jobs: BTreeMap<JobId, JobRecord>,
}

fn is_terminal(status: &JobStatus) -> bool {
    matches!(status, JobStatus::Finished | JobStatus::Failed)
}

fn summary(id: &str, rec: &JobRecord) -> JobSummary {
    JobSummary {
        job_id: id.to_string(),
        workflow_name: rec.spec.workflow_name.clone(),
        status: rec.status.clone(),
        submitted_at: rec.submitted_at,
        workdir: rec.spec.workdir.to_string_lossy().into_owned(),
    }
}

/// Owns the registry of all jobs and one [`JobActor`] child per live job. The
/// registry is rebuilt by replaying this actor's own journal — never by scanning
/// disk (the `Journal` trait has no list API).
pub struct SupervisorActor {
    runtime: Arc<dyn JobRuntime>,
    children: BTreeMap<JobId, ActorRef<JobCommand>>,
}

impl SupervisorActor {
    pub fn new(runtime: Arc<dyn JobRuntime>) -> Self {
        Self {
            runtime,
            children: BTreeMap::new(),
        }
    }

    fn spawn_job(&mut self, ctx: &ActorContext<Self>, job_id: JobId, spec: JobSpec) {
        let child = ctx.spawn(JobActor::new(
            job_id.clone(),
            spec,
            self.runtime.clone(),
            ctx.self_ref(),
        ));
        self.children.insert(job_id, child);
    }
}

#[async_trait]
impl EventSourcedActor for SupervisorActor {
    type Command = SupervisorCommand;
    type Event = SupervisorEvent;
    type State = SupervisorState;

    fn persistence_id(&self) -> PersistenceId {
        PersistenceId::new("supervisor", "main")
    }

    fn initial_state() -> SupervisorState {
        SupervisorState::default()
    }

    fn apply_event(mut state: SupervisorState, event: SupervisorEvent) -> SupervisorState {
        match event {
            SupervisorEvent::JobSubmitted {
                id,
                spec,
                submitted_at,
            } => {
                state.jobs.insert(
                    id,
                    JobRecord {
                        spec,
                        status: JobStatus::Running,
                        submitted_at,
                    },
                );
            }
            SupervisorEvent::JobStatusChanged { id, status } => {
                if let Some(rec) = state.jobs.get_mut(&id) {
                    rec.status = status;
                }
            }
            SupervisorEvent::JobRemoved { id } => {
                state.jobs.remove(&id);
            }
        }
        state
    }

    async fn handle_command(
        &mut self,
        state: &SupervisorState,
        cmd: SupervisorCommand,
        ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<SupervisorEvent> {
        match cmd {
            SupervisorCommand::Submit {
                spec,
                submitted_at,
                reply,
            } => {
                let id = uuid::Uuid::new_v4().to_string();
                self.spawn_job(ctx, id.clone(), spec.clone());
                if let Some(child) = self.children.get(&id) {
                    let _ = child.tell(JobCommand::Start).await;
                }
                let _ = reply.send(id.clone());
                CommandEffect::Persist(vec![SupervisorEvent::JobSubmitted {
                    id,
                    spec,
                    submitted_at,
                }])
            }
            SupervisorCommand::List { reply } => {
                let jobs = state
                    .jobs
                    .iter()
                    .map(|(id, rec)| summary(id, rec))
                    .collect();
                let _ = reply.send(jobs);
                CommandEffect::None
            }
            SupervisorCommand::Stop { job_id } => {
                if let Some(child) = self.children.get(&job_id) {
                    let _ = child.tell(JobCommand::Stop).await;
                }
                CommandEffect::None
            }
            SupervisorCommand::Resume { job_id, message } => {
                if let Some(child) = self.children.get(&job_id) {
                    let _ = child.tell(JobCommand::Resume { message }).await;
                }
                CommandEffect::None
            }
            SupervisorCommand::Remove { job_id, reply } => match state.jobs.get(&job_id) {
                None => {
                    let _ = reply.send(Err(format!("no such job: {job_id}")));
                    CommandEffect::None
                }
                Some(rec) if !is_terminal(&rec.status) => {
                    let _ = reply.send(Err(format!(
                        "job {job_id} is {:?}; stop it before removing",
                        rec.status
                    )));
                    CommandEffect::None
                }
                Some(_) => {
                    // Terminal jobs have no live child (PersistAndStop), but drop any
                    // stale ref defensively.
                    self.children.remove(&job_id);
                    let _ = reply.send(Ok(()));
                    CommandEffect::Persist(vec![SupervisorEvent::JobRemoved { id: job_id }])
                }
            },
            SupervisorCommand::Shutdown { reply } => {
                // Ask every live job to release its runtime child, then await all acks
                // so no october-runtime process is orphaned when the daemon exits.
                let mut acks = Vec::new();
                for child in self.children.values() {
                    let (tx, rx) = oneshot::channel();
                    if child.tell(JobCommand::Shutdown { reply: tx }).await.is_ok() {
                        acks.push(rx);
                    }
                }
                self.children.clear();
                for ack in acks {
                    let _ = ack.await;
                }
                let _ = reply.send(());
                CommandEffect::None
            }
            SupervisorCommand::Subscribe { job_id, reply } => {
                match self.children.get(&job_id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child.tell(JobCommand::Subscribe { reply: tx }).await;
                        // Forward the child's receiver once it answers, off the mailbox.
                        tokio::spawn(async move {
                            let _ = reply.send(rx.await.ok());
                        });
                    }
                    None => {
                        let _ = reply.send(None);
                    }
                }
                CommandEffect::None
            }
            SupervisorCommand::JobStatusChanged { job_id, status } => {
                CommandEffect::Persist(vec![SupervisorEvent::JobStatusChanged {
                    id: job_id,
                    status,
                }])
            }
        }
    }

    /// After recovery, re-spawn a [`JobActor`] for every non-terminal job so it can
    /// auto-resume (Running) or accept a `resume` (Suspended/AwaitingUserInput).
    async fn on_recovery_complete(
        &mut self,
        state: &SupervisorState,
        ctx: &mut ActorContext<Self>,
    ) {
        let to_spawn: Vec<(JobId, JobSpec)> = state
            .jobs
            .iter()
            .filter(|(_, rec)| !is_terminal(&rec.status))
            .map(|(id, rec)| (id.clone(), rec.spec.clone()))
            .collect();
        for (id, spec) in to_spawn {
            self.spawn_job(ctx, id, spec);
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
    use models::capabilities::{CapabilitySpec, NetworkPolicy};
    use models::workflow::WorkflowDefinition;
    use std::path::PathBuf;

    fn spec() -> JobSpec {
        JobSpec {
            workflow: WorkflowDefinition {
                start: "a".into(),
                agents: vec![],
            },
            workflow_name: "wf".into(),
            workdir: PathBuf::from("/tmp"),
            input: "go".into(),
            capabilities: CapabilitySpec {
                network: NetworkPolicy::Block,
                grants: vec![],
            },
        }
    }

    #[test]
    fn submit_then_status_updates_registry() {
        let s = SupervisorActor::apply_event(
            SupervisorState::default(),
            SupervisorEvent::JobSubmitted {
                id: "j1".into(),
                spec: spec(),
                submitted_at: 7,
            },
        );
        assert_eq!(s.jobs.len(), 1);
        assert_eq!(s.jobs.get("j1").unwrap().status, JobStatus::Running);
        assert_eq!(s.jobs.get("j1").unwrap().submitted_at, 7);

        let s = SupervisorActor::apply_event(
            s,
            SupervisorEvent::JobStatusChanged {
                id: "j1".into(),
                status: JobStatus::Finished,
            },
        );
        assert_eq!(s.jobs.get("j1").unwrap().status, JobStatus::Finished);
    }

    #[test]
    fn removed_job_drops_from_registry() {
        let s = SupervisorActor::apply_event(
            SupervisorState::default(),
            SupervisorEvent::JobSubmitted {
                id: "j1".into(),
                spec: spec(),
                submitted_at: 0,
            },
        );
        let s = SupervisorActor::apply_event(s, SupervisorEvent::JobRemoved { id: "j1".into() });
        assert!(s.jobs.is_empty());
    }

    #[test]
    fn summary_projects_record_fields() {
        let rec = JobRecord {
            spec: spec(),
            status: JobStatus::AwaitingUserInput,
            submitted_at: 42,
        };
        let sm = summary("j9", &rec);
        assert_eq!(sm.job_id, "j9");
        assert_eq!(sm.workflow_name, "wf");
        assert_eq!(sm.status, JobStatus::AwaitingUserInput);
        assert_eq!(sm.submitted_at, 42);
        assert_eq!(sm.workdir, "/tmp");
    }
}
