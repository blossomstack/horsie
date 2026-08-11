//! Answering questions without waking anything.
//!
//! Every read is served from the resident actor's own memory, or forwarded to
//! the agent that owns the transcript. None of them touches the journal, so
//! opening a session to look at it costs no sandbox — which is what lets a
//! browser poll a session that is otherwise idle.
//!
//! No events and no state: this component only ever answers.

use super::{
    AgentDetail, AgentEntry, AgentKey, AgentStatus, AgentUsageEntry, CommandEffect, MAIN_AGENT_ID,
    ReadCommand, SessionActor, SessionDomainEvent, SessionSnapshot, SessionState,
    SessionUsageStats,
};
use crate::agent_loop::AgentCommand;
use crate::agent_loop::AgentUsageSnapshot;
use crate::sessions::session_actor::SessionCommand;
use crate::sessions::spec::SessionStatus;
use crate::sessions::subagents::{SubAgentParent, SubAgentRecord, SubAgentStatus};
use crate::sessions::workflow::{StepRun, StepStatus};
use horsie_actor::ActorContext;
use uuid::Uuid;

/// Reads.
pub(super) struct Reads;

impl Reads {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: ReadCommand,
        ctx: &ActorContext<SessionCommand>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            ReadCommand::ReadLog {
                agent_id,
                after,
                reply,
            } => {
                // Read from the resident actor's in-memory state. No journal
                // access, no runtime — opening a session to read it stays free
                // of sandbox cost.
                let agent = actor.resolve_agent(state, ctx, agent_id.as_deref());
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
                let agent = actor.resolve_agent(state, ctx, agent_id.as_deref());
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
                let detail = actor.read_agent(state, ctx, agent_id.as_deref()).await;
                let _ = reply.send(detail);
                CommandEffect::none()
            }
            ReadCommand::Snapshot { reply } => {
                let _ = reply.send(SessionSnapshot {
                    status: state.status.clone(),
                    // Banked totals only, so this asks no agent anything. The
                    // live context size is per-agent and never summed, and it
                    // is on the agent document rather than here.
                    usage_total: state.session_usage_total(),
                    agents: actor.agent_roster(state),
                });
                CommandEffect::none()
            }
            ReadCommand::UsageStats { reply } => {
                let stats = actor.read_usage(state).await;
                let _ = reply.send(stats);
                CommandEffect::none()
            }
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
            // session that can never run again. Reported as anything else, they
            // badge an idle agent beside a document that says the session
            // failed.
            SessionStatus::Failed { .. }
            | SessionStatus::ProvisioningFailed { .. }
            | SessionStatus::Unrecoverable { .. } => AgentStatus::Failed,
            SessionStatus::Idle => AgentStatus::Idle,
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

/// One node of a subagent tree.
fn sub_entry(id: Uuid, rec: &SubAgentRecord) -> AgentEntry {
    AgentEntry {
        id: id.to_string(),
        parent: match rec.parent {
            // Rooted on whatever this session's "main" is — the main agent, or
            // the step that spawned it. Either way, not a subagent.
            SubAgentParent::Main => None,
            SubAgentParent::SubAgent(pid) => Some(pid),
        },
        label: Some(rec.label.clone()),
        depth: rec.depth,
        agent_type: rec.agent_type.clone(),
        status: match rec.status {
            SubAgentStatus::Running => AgentStatus::Running,
            SubAgentStatus::Completed => AgentStatus::Completed,
            SubAgentStatus::Failed => AgentStatus::Failed,
        },
        error: rec.error.clone(),
        started_at_ms: rec.spawned_at_ms,
        ended_at_ms: rec.ended_at_ms,
    }
}

/// Handlers that belong to this component but act on the actor's own
/// fields — the roster, the supervisor link, the spawn helpers. An inherent
/// `impl` in a child module sees them, so moving the code needed no plumbing.
impl SessionActor {
    /// Every agent this session hosts, addressable at `/agents/:agent_id`.
    ///
    /// Only this actor can answer it, which is the whole reason it is here: a
    /// conversation's main agent takes its state from `state.status`, a run's
    /// step agents from `state.run`, and every subagent from `state.subagents`.
    /// Three pieces of state nothing above this actor holds together.
    ///
    /// A run has no main agent — it *is* its steps, and the definition rather
    /// than a person decides which one runs. The same fact
    /// [`SessionAgents::Workflow`](super::SessionAgents) records about the live
    /// actors, asked of the durable state instead.
    pub(super) fn agent_roster(&self, state: &SessionState) -> Vec<AgentEntry> {
        let mut agents: Vec<AgentEntry> = match self.spec.workflow.is_some() {
            true => state
                .run
                .iter()
                .flat_map(|run| run.steps.iter())
                .map(step_entry)
                .collect(),
            // Listed even though nothing spawned it, so that every agent is
            // reachable at one shape.
            false => vec![main_entry(&state.status)],
        };
        agents.extend(
            state
                .subagents
                .ids()
                .into_iter()
                .filter_map(|id| state.subagents.node(id).map(|rec| sub_entry(id, rec))),
        );
        agents
    }

    /// One agent's document. `None` when this session hosts no such agent.
    ///
    /// Resolution is [`resolve_agent`](SessionActor::resolve_agent)'s, not a
    /// second copy of it: the key it answers with is what says whether this is
    /// the main agent, a step, or a subagent, and therefore where the rest of
    /// the document comes from. That is also what makes a cold agent readable —
    /// resolving one spawns it.
    pub(super) async fn read_agent(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionCommand>,
        agent_id: Option<&str>,
    ) -> Option<AgentDetail> {
        let (key, agent) = self.resolve_agent(state, ctx, agent_id)?;
        let execution = match key {
            AgentKey::Step(id) => state
                .run
                .as_ref()
                .and_then(|run| run.index_of_agent(id).and_then(|i| run.get(i))),
            AgentKey::Main | AgentKey::Sub(_) => None,
        };
        let node = match key {
            AgentKey::Sub(id) => state.subagents.node(id),
            AgentKey::Main | AgentKey::Step(_) => None,
        };
        let entry = match key {
            AgentKey::Main => main_entry(&state.status),
            AgentKey::Step(_) => step_entry(execution?),
            AgentKey::Sub(id) => sub_entry(id, node?),
        };
        Some(AgentDetail {
            entry,
            // A step runs under its own preset, so the session's model — which
            // is the *first* step's — is the wrong one for any other step.
            model: match execution.and_then(|e| self.step_spec(&e.step)) {
                Some(step) => step.settings.model.clone(),
                None => self.spec.agent.model.clone(),
            },
            task: node.map(|node| node.task.clone()),
            output: match (node, execution) {
                (Some(node), _) => node.output.clone(),
                (None, Some(execution)) => execution
                    .output
                    .as_ref()
                    .map(crate::sessions::workflow::output_as_input),
                (None, None) => None,
            },
            state: agent
                .ask(|reply| AgentCommand::GetState { reply })
                .await
                .ok()?,
        })
    }

    /// The definition of one of this run's steps.
    fn step_spec(&self, name: &str) -> Option<&crate::sessions::workflow::WorkflowStepSpec> {
        self.spec.workflow.as_ref()?.step(name)
    }

    /// Aggregated usage. Totals come from this session's own durable record;
    /// only the live context size is asked of the agent.
    pub(super) async fn read_usage(&self, state: &SessionState) -> SessionUsageStats {
        let snapshot = match self.agent() {
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
            main_agent: AgentUsageEntry {
                model: self.spec.agent.model.clone(),
                snapshot: AgentUsageSnapshot {
                    usage_total: main_usage_total,
                    last_turn_usage: snapshot.last_turn_usage,
                    context_tokens: snapshot.context_tokens,
                },
            },
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
    //! That a read answers from memory and never from the journal.
    use super::super::testing::*;
    use super::super::*;
    use super::*;

    use crate::agent_loop::UsageTotal;
    use horsie_agentcore::LlmProvider;

    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn usage_is_recorded_per_agent() {
        let s = fold(vec![SessionDomainEvent::UsageRecorded {
            at_ms: 0,
            agent_id: MAIN_AGENT_ID.to_string(),
            usage_total: UsageTotal {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: None,
                cache_read_tokens: None,
            },
        }]);
        assert_eq!(s.agent_usage.get(MAIN_AGENT_ID).unwrap().input_tokens, 10);
    }

    /// A conversation's main agent has no lifecycle of its own, so every one of
    /// its session's states has to project onto one of an agent's — with no
    /// catch-all arm. A `_ =>` here is what let a session whose runtime never
    /// built report a `failed` status beside an `idle` agent, and it is why
    /// this asserts every variant rather than the interesting ones.
    #[test]
    fn every_session_status_is_a_state_its_main_agent_can_be_in() {
        for (status, expected) in [
            (SessionStatus::Provisioning, AgentStatus::Provisioning),
            (SessionStatus::Idle, AgentStatus::Idle),
            (SessionStatus::Running, AgentStatus::Running),
            (SessionStatus::AwaitingInput, AgentStatus::AwaitingInput),
            (
                SessionStatus::Failed {
                    reason: "boom".into(),
                },
                AgentStatus::Failed,
            ),
            (
                SessionStatus::ProvisioningFailed {
                    reason: "no vendor".into(),
                },
                AgentStatus::Failed,
            ),
            (
                SessionStatus::Unrecoverable {
                    reason: "gone".into(),
                },
                AgentStatus::Failed,
            ),
        ] {
            let entry = main_entry(&status);
            assert_eq!(entry.status, expected, "{status:?}");
            assert_eq!(entry.id, MAIN_AGENT_ID);
            assert_eq!(
                entry.error,
                crate::sessions::spec::status_reason(&status),
                "an agent and its session must give the same reason: {status:?}"
            );
        }
    }

    /// A conversation lists the agent nothing spawned, so that every agent is
    /// reachable at one shape.
    #[tokio::test]
    async fn a_conversation_lists_its_main_agent_and_its_subagents() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, "research", "dig").await;
        wait_for_tree(&journal, id, |f| f.node(sub).is_some()).await;

        let agents = roster(&session).await;
        assert_eq!(agents[0].id, MAIN_AGENT_ID);
        assert_eq!(agents[0].label, None, "the main agent is not one of many");
        assert!(
            agents.iter().any(|a| a.id == sub.to_string()),
            "a subagent is an agent of its session: {agents:?}"
        );
    }

    /// A run has no main agent — it *is* its steps. Reporting one anyway meant
    /// a finished run answered with an agent that does not exist, permanently
    /// running, while the session's own status said `Idle` right beside it.
    #[tokio::test]
    async fn a_run_lists_its_steps_and_no_main_agent() {
        let (_f, session, id, journal) = spawn_run_with_provider(BlockingProvider::new()).await;
        let run = wait_for_run(&journal, id, |r| r.current().is_some()).await;

        let agents = roster(&session).await;
        assert!(
            agents.iter().all(|a| a.id != MAIN_AGENT_ID),
            "a run has no main agent: {agents:?}"
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, run.steps[0].agent.to_string());
        assert_eq!(agents[0].label.as_deref(), Some(run.steps[0].step.as_str()));
        assert_eq!(agents[0].status, AgentStatus::Running);
    }

    /// What became of a step is the run log's answer, and a step that concluded
    /// says so. It used to be in no subagent tree and in nothing else the agent
    /// document read, so it fell through to a hardcoded `running` and reported
    /// it for ever — through reloads and cold tabs, long after the run ended.
    #[tokio::test]
    async fn a_concluded_step_reports_that_it_concluded() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider =
            MockProvider::scripted(Script::of([Ok(concludes(serde_json::json!({"ok": true})))]));
        let (_f, session, id, journal) = spawn_run_with_provider(provider).await;
        let run = wait_for_run(&journal, id, |r| {
            r.steps.iter().any(|s| s.status == StepStatus::Concluded)
        })
        .await;
        let agent_id = run.steps[0].agent.to_string();

        let detail = session
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::Agent {
                    agent_id: Some(agent_id.clone()),
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("a step is an agent of its run");
        assert_eq!(detail.entry.id, agent_id);
        assert_eq!(detail.entry.status, AgentStatus::Completed);
        assert!(
            detail.entry.ended_at_ms > 0,
            "a step that ended is stamped with when"
        );
        assert!(
            detail.output.is_some(),
            "a concluded step reports what it concluded"
        );
        // And the roster agrees, because it is the same projection.
        assert_eq!(roster(&session).await[0].status, AgentStatus::Completed);
    }

    /// This session's agents, read the way `GET /api/sessions/:id` reads them.
    async fn roster(session: &ActorRef<SessionCommand>) -> Vec<AgentEntry> {
        session
            .ask(|reply| SessionCommand::Read(ReadCommand::Snapshot { reply }))
            .await
            .unwrap()
            .agents
    }

    /// The invariant the old two-vocabulary design could not even state, now
    /// nearly a tautology: reading forward and paging backwards return the same
    /// entries, in the same order, because there is one log and one writer.
    ///
    /// Worth keeping precisely because it used to be hard. Two projections of
    /// one append-only log could disagree when one of them was a broadcast a
    /// subscriber might have joined late; neither of these can.
    #[tokio::test]
    async fn reading_forward_and_paging_back_agree_on_the_log() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: None,
                    text: "go".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_journal_len(&journal, id, 2).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let streamed: Vec<u64> = session
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::ReadLog {
                    agent_id: None,
                    after: None,
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("main agent log")
            .entries
            .iter()
            .map(|e| e.seq)
            .collect();
        let stored: Vec<u64> = main_history(&session)
            .await
            .entries
            .iter()
            .map(|e| e.seq)
            .collect();
        assert!(!streamed.is_empty(), "the turn must produce entries");
        assert_eq!(streamed, stored);
        assert_eq!(
            streamed,
            (0..streamed.len() as u64).collect::<Vec<_>>(),
            "no gaps and no reordering"
        );
    }

    /// Reads and streams are served from actor state. The journal is touched
    /// only while an actor recovers — never to answer a query.
    #[tokio::test]
    async fn serving_reads_never_touches_the_journal() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "mock", &actor_spec_fixture())
            .await
            .expect("create");
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            crate::sessions::spec::ModelEntry::provider_only(
                Arc::new(EchoProvider) as Arc<dyn LlmProvider>
            ),
        );
        let counting = Arc::new(CountingJournal::new());
        let journal: Arc<dyn horsie_actor::Journal> = counting.clone();
        let session =
            horsie_actor::ActorSystem::new(journal.clone()).spawn_persistent(SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps.clone(),
                spawn_deaf_supervisor(),
                crate::sessions::Revisions::default(),
            ));

        // Drive one turn so both actors are loaded and have history.
        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: None,
                    text: "go".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_journal_len(&journal, id, 2).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Recovery is allowed to replay; everything after it is not.
        let after_recovery = counting.replays();
        assert!(
            after_recovery > 0,
            "the counter must actually observe recovery, or this test proves nothing"
        );

        let _ = main_history(&session).await;
        let _ = session
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::ReadLog {
                    agent_id: None,
                    after: Some(crate::agent_loop::Cursor {
                        entry_seq: 0,
                        delta_seq: 0,
                    }),
                    reply,
                })
            })
            .await
            .unwrap();
        let _ = session
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::Agent {
                    agent_id: None,
                    reply,
                })
            })
            .await
            .unwrap();
        let _ = session
            .ask(|reply| SessionCommand::Read(ReadCommand::Snapshot { reply }))
            .await
            .unwrap();

        assert_eq!(
            counting.replays(),
            after_recovery,
            "the session document and an agent's must both be served from memory"
        );
    }
}
