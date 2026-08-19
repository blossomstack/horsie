//! Answering questions without waking anything.
//!
//! Every read is served from the resident actor's own memory, or forwarded to
//! the agent that owns the transcript. None of them touches the journal, so
//! opening a session to look at it costs no sandbox — which is what lets a
//! browser poll a session that is otherwise idle.

use super::runner::state::{
    ForkState, RunnerRecord, RunnerState, SessionState, SubPhase, SubState,
};
use super::runner::{Runner, RunnerBehavior};
use super::{
    AgentDetail, AgentEntry, AgentStatus, AgentUsageEntry, CommandEffect, MAIN_AGENT_ID,
    ReadCommand, RunnerId, SessionActor, SessionEvent, SessionSnapshot, SessionUsageStats,
};
use crate::agent_loop::AgentCommand;
use crate::agent_loop::AgentUsageSnapshot;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::spec::SessionStatus;
use crate::sessions::workflow::{StepRun, StepStatus};
use horsie_actor::ActorContext;

impl SessionActor {
    pub(super) async fn handle_read(
        &mut self,
        state: &SessionState,
        cmd: ReadCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd {
            ReadCommand::ReadLog {
                agent_id,
                after,
                reply,
            } => {
                // Read from the resident actor's in-memory state. No journal
                // access, no runtime — opening a session to read it stays free
                // of sandbox cost.
                let agent = self.resolve_agent(state, ctx, agent_id.as_deref());
                let out = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| AgentCommand::ReadLog { after, reply })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(out);
                CommandEffect::none()
            }
            ReadCommand::PageLog {
                agent_id,
                before,
                max,
                reply,
            } => {
                let agent = self.resolve_agent(state, ctx, agent_id.as_deref());
                let page = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| AgentCommand::PageLog { before, max, reply })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(page);
                CommandEffect::none()
            }
            ReadCommand::Agent { agent_id, reply } => {
                let detail = self.read_agent(state, ctx, agent_id.as_deref()).await;
                let _ = reply.send(detail);
                CommandEffect::none()
            }
            ReadCommand::Snapshot { reply } => {
                let _ = reply.send(SessionSnapshot {
                    status: state.status(),
                    // Banked totals only, so this asks no agent anything. The
                    // live context size is per-agent and never summed, and it
                    // is on the agent document rather than here.
                    usage_total: state.session_usage_total(),
                    agents: self.agent_roster(state),
                });
                CommandEffect::none()
            }
            ReadCommand::UsageStats { reply } => {
                let stats = self.read_usage(state).await;
                let _ = reply.send(stats);
                CommandEffect::none()
            }
        }
    }

    /// Every agent this session hosts, addressable at `/agents/:agent_id`.
    ///
    /// Only this actor can answer it: a conversation's main agent takes its
    /// state from the session, a run's step agents from its run log, and every
    /// subagent from its own runner record — held together by nothing above
    /// this actor. Forks are deliberately absent: the session list shows them
    /// through the fork roster the supervisor already holds.
    pub(super) fn agent_roster(&self, state: &SessionState) -> Vec<AgentEntry> {
        let mut agents: Vec<AgentEntry> = match self.spec().workflow_run() {
            // A run has no main agent — it *is* its steps, and the definition
            // rather than a person decides which one runs.
            Some(_) => Vec::new(),
            // Listed even though nothing spawned it, so that every agent is
            // reachable at one shape.
            None => vec![main_entry(&state.status())],
        };
        for record in state.runners.values() {
            if let RunnerState::Workflow(w) = &record.state {
                agents.extend(w.run.steps.iter().map(step_entry));
            }
        }
        agents.extend(
            state
                .runners
                .iter()
                .filter_map(|(id, record)| match &record.state {
                    RunnerState::Sub(node) => Some(sub_entry(state, *id, record, node)),
                    RunnerState::Main(_) | RunnerState::Fork(_) | RunnerState::Workflow(_) => None,
                }),
        );
        agents
    }

    /// One agent's document. `None` when this session hosts no such agent.
    ///
    /// Resolution spawns a cold agent, which is what makes a finished one
    /// readable; the owning runner is what says where the rest of the document
    /// comes from.
    pub(super) async fn read_agent(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        agent_id: Option<&str>,
    ) -> Option<AgentDetail> {
        let (agent, actor) = self.resolve_agent(state, ctx, agent_id)?;
        let owner = Runner::owner_of(agent, state);
        let entry = match &owner {
            None if agent == self.self_agent() => main_entry(&state.status()),
            None => return None,
            Some(runner) => {
                let id = runner.id();
                let record = state.record(id)?;
                match &record.state {
                    RunnerState::Main(_) => main_entry(&state.status()),
                    RunnerState::Sub(node) => sub_entry(state, id, record, node),
                    RunnerState::Fork(f) => fork_entry(state, id, record, f),
                    RunnerState::Workflow(w) => {
                        let index = w.run.index_of_agent(agent.0)?;
                        step_entry(w.run.get(index)?)
                    }
                }
            }
        };
        let settings = match &owner {
            Some(runner) => runner.role(self.spec(), state, agent)?.settings,
            None => self.spec().agent_settings()?.clone(),
        };
        let node =
            owner
                .as_ref()
                .and_then(|runner| match state.record(runner.id()).map(|r| &r.state) {
                    Some(RunnerState::Sub(node)) => Some(node),
                    _ => None,
                });
        let execution: Option<&StepRun> =
            owner
                .as_ref()
                .and_then(|runner| match state.record(runner.id()).map(|r| &r.state) {
                    Some(RunnerState::Workflow(w)) => {
                        w.run.index_of_agent(agent.0).and_then(|i| w.run.get(i))
                    }
                    _ => None,
                });
        Some(AgentDetail {
            entry,
            settings,
            task: node.map(|node| node.task.clone()),
            output: match (node, execution) {
                (Some(node), _) => match &node.phase {
                    SubPhase::Done { result: Ok(o), .. } => Some(o.clone()),
                    SubPhase::Done { result: Err(_), .. } | SubPhase::Running { .. } => None,
                },
                (None, Some(execution)) => execution
                    .output
                    .as_ref()
                    .map(crate::sessions::workflow::output_as_input),
                (None, None) => None,
            },
            state: actor
                .ask(|reply| AgentCommand::GetState { reply })
                .await
                .ok()?,
        })
    }

    /// Aggregated usage. Totals come from this session's own durable record;
    /// only the live context size is asked of the agent.
    pub(super) async fn read_usage(&self, state: &SessionState) -> SessionUsageStats {
        let snapshot = match self
            .agents
            .as_ref()
            .and_then(|a| a.main())
            .map(|r| r.actor.clone())
        {
            Some(agent) => agent
                .ask(|reply| AgentCommand::GetUsage { reply })
                .await
                .unwrap_or_default(),
            None => AgentUsageSnapshot::default(),
        };
        let main_usage_total = state
            .agent_usage
            .get(MAIN_AGENT_ID)
            .copied()
            .unwrap_or_default();
        SessionUsageStats {
            session_total: state.session_usage_total(),
            agents: state.agent_usage.clone(),
            // A run has no main agent to report; only an agent session's does.
            main_agent: self
                .spec()
                .agent_settings()
                .map(|settings| AgentUsageEntry {
                    model: settings.model.clone(),
                    snapshot: AgentUsageSnapshot {
                        usage_total: main_usage_total,
                        last_turn_usage: snapshot.last_turn_usage,
                        context_tokens: snapshot.context_tokens,
                    },
                }),
        }
    }
}

/// A conversation's primary agent. It has no lifecycle of its own — it is the
/// session, so the session's status *is* its status.
fn main_entry(status: &SessionStatus) -> AgentEntry {
    AgentEntry {
        id: MAIN_AGENT_ID.to_string(),
        parent: None,
        label: None,
        depth: 0,
        agent_type: None,
        status: match status {
            SessionStatus::Provisioning => AgentStatus::Provisioning,
            SessionStatus::Running => AgentStatus::Running,
            SessionStatus::AwaitingInput => AgentStatus::AwaitingInput,
            // Every way a session can be broken, including the two that are
            // *not* `Failed`: a create that never built a runtime, and a
            // session that can never run again.
            SessionStatus::Failed { .. }
            | SessionStatus::ProvisioningFailed { .. }
            | SessionStatus::Unrecoverable { .. } => AgentStatus::Failed,
            // `Finished` is a run's, and a run has no main agent — but the
            // match is over the session's whole vocabulary, and an agent that
            // is not working is idle whichever way the session got there.
            SessionStatus::Idle | SessionStatus::Finished => AgentStatus::Idle,
        },
        error: crate::sessions::spec::status_reason(status),
        started_at_ms: 0,
        ended_at_ms: 0,
    }
}

/// One execution of a workflow step. A step reached twice has two of these, and
/// each is its own agent.
fn step_entry(execution: &StepRun) -> AgentEntry {
    AgentEntry {
        id: execution.agent.to_string(),
        // The definition chose this step, so no agent is its parent.
        parent: None,
        label: Some(execution.step.clone()),
        depth: 0,
        agent_type: None,
        status: match execution.status {
            StepStatus::Running => AgentStatus::Running,
            StepStatus::Concluded => AgentStatus::Completed,
            StepStatus::Failed => AgentStatus::Failed,
            StepStatus::Cancelled => AgentStatus::Cancelled,
        },
        error: execution.error.clone(),
        started_at_ms: execution.started_at_ms,
        ended_at_ms: execution.ended_at_ms.unwrap_or(0),
    }
}

/// One node of delegated work.
fn sub_entry(
    state: &SessionState,
    id: RunnerId,
    record: &RunnerRecord,
    node: &SubState,
) -> AgentEntry {
    let (status, error, started, ended) = match &node.phase {
        SubPhase::Running { since_ms } => (AgentStatus::Running, None, *since_ms, 0),
        SubPhase::Done {
            result,
            started_ms,
            ended_ms,
            ..
        } => match result {
            Ok(_) => (AgentStatus::Completed, None, *started_ms, *ended_ms),
            Err(e) => (AgentStatus::Failed, Some(e.clone()), *started_ms, *ended_ms),
        },
    };
    AgentEntry {
        id: id.to_string(),
        // On the wire, a parent is another *subagent*: a node rooted directly
        // on a conversation or a step reports none, exactly as the old forest
        // did.
        parent: record
            .parent
            .filter(|p| {
                matches!(
                    state.record(RunnerId::of_agent(*p)).map(|r| &r.state),
                    Some(RunnerState::Sub(_))
                )
            })
            .map(|p| p.0),
        label: Some(node.label.clone()),
        depth: state.depth_of(id),
        agent_type: node.agent_type.clone(),
        status,
        error,
        started_at_ms: started,
        ended_at_ms: ended,
    }
}

/// One fork of a conversation. `label` carries the title it gave itself, which
/// is `None` until it does — a client shows what it was branched from instead.
fn fork_entry(
    state: &SessionState,
    id: RunnerId,
    record: &RunnerRecord,
    fork: &ForkState,
) -> AgentEntry {
    AgentEntry {
        id: id.to_string(),
        // Rooted on the session's main agent, which is not a fork, or on the
        // fork it branched from.
        parent: record
            .parent
            .filter(|p| {
                matches!(
                    state.record(RunnerId::of_agent(*p)).map(|r| &r.state),
                    Some(RunnerState::Fork(_))
                )
            })
            .map(|p| p.0),
        label: fork.title.clone(),
        depth: state.depth_of(id),
        agent_type: None,
        // Derived from the fork's own phases: a fork *is* a conversation, so
        // `AgentStatus` is already its vocabulary.
        status: fork.agent_status(),
        error: None,
        started_at_ms: record.created_at_ms,
        // A conversation is never *done*, so it has no end.
        ended_at_ms: 0,
    }
}
