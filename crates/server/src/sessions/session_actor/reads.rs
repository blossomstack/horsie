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
use crate::agent_loop::ReadCommand as AgentReadCommand;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::run_forest::{RunForest, SubAgentRun, SubAgentStatus, SubSessionRun};
use crate::sessions::spec::SessionStatus;
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
        ctx: &ActorContext<SessionInbox>,
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
                        .ask(|reply| AgentCommand::Read(AgentReadCommand::ReadLog { after, reply }))
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(out);
                CommandEffect::none()
            }
            ReadCommand::PageLog {
                agent_id,
                anchor,
                max,
                filter,
                reply,
            } => {
                let agent = actor.resolve_agent(state, ctx, agent_id.as_deref());
                let page = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| {
                            AgentCommand::Read(AgentReadCommand::PageLog {
                                anchor,
                                max,
                                filter,
                                reply,
                            })
                        })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(page);
                CommandEffect::none()
            }
            ReadCommand::SearchLog {
                agent_id,
                needle,
                max,
                filter,
                reply,
            } => {
                let agent = actor.resolve_agent(state, ctx, agent_id.as_deref());
                let hits = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| {
                            AgentCommand::Read(AgentReadCommand::SearchLog {
                                needle,
                                max,
                                filter,
                                reply,
                            })
                        })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(hits);
                CommandEffect::none()
            }
            ReadCommand::SeqOfId {
                agent_id,
                entry_id,
                reply,
            } => {
                let agent = actor.resolve_agent(state, ctx, agent_id.as_deref());
                let seq = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| {
                            AgentCommand::Read(AgentReadCommand::SeqOfId {
                                id: entry_id,
                                reply,
                            })
                        })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(seq);
                CommandEffect::none()
            }
            ReadCommand::Agent { agent_id, reply } => {
                let detail = actor.read_agent(state, ctx, agent_id.as_deref()).await;
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

/// A session's primary agent. It has no lifecycle of its own — it is the
/// session, so the session's status *is* its status.
fn main_entry(status: &SessionStatus) -> AgentEntry {
    AgentEntry {
        id: MAIN_AGENT_ID.to_string(),
        parent: None,
        label: None,
        depth: 0,
        agent_type: None,
        preset: None,
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
        preset: None,
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

/// One sub session of a session.
///
/// Its status is read straight off the record rather than mapped from
/// anything: a sub session *is* a session, so `AgentStatus` is already the
/// vocabulary its record is kept in. `label` carries the title it gave itself,
/// which is `None` until it does — a client shows what it was branched from
/// instead.
fn sub_session_entry(id: Uuid, forest: &RunForest, rec: &SubSessionRun) -> AgentEntry {
    let (created_at_ms, parent) = forest
        .owner_of_agent(id)
        .map(|(_, e)| (e.created_at_ms, e.parent))
        .unwrap_or((0, None));
    AgentEntry {
        id: id.to_string(),
        // Rooted on the session's main agent, which is not a sub session, so
        // only a sub session parent is reported.
        parent: parent.filter(|pid| forest.sub_session(*pid).is_some()),
        label: rec.title.clone(),
        depth: forest.depth_of_agent(id).unwrap_or(0),
        agent_type: None,
        preset: None,
        status: rec.status,
        error: None,
        started_at_ms: created_at_ms,
        // A session is never *done*, so it has no end.
        ended_at_ms: 0,
    }
}

/// One subagent of the forest.
fn sub_entry(id: Uuid, forest: &RunForest, rec: &SubAgentRun) -> AgentEntry {
    AgentEntry {
        id: id.to_string(),
        // Reported when the parent is another subagent or the sub session
        // that spawned it — both are rows the client holds, so both are things
        // it can hang this one off. A step's is not: a run's shape is its
        // workflow graph's, drawn from the run rather than from this roster.
        //
        // A sub session's used to be dropped here too, as "top-level to a
        // reader". It is not: the session graph draws both rosters as one
        // lineage, and without this the agent a sub session delegated to came
        // out beside it, hanging off the main agent, as though the main agent
        // had spawned it.
        parent: forest
            .owner_of_agent(id)
            .and_then(|(_, e)| e.parent)
            .filter(|pid| forest.sub(*pid).is_some() || forest.sub_session(*pid).is_some()),
        label: Some(rec.label.clone()),
        depth: forest.depth_of_agent(id).unwrap_or(0),
        agent_type: rec.agent_type.clone(),
        preset: None,
        status: match rec.status {
            SubAgentStatus::Running => AgentStatus::Running,
            SubAgentStatus::Completed => AgentStatus::Completed,
            SubAgentStatus::Failed => AgentStatus::Failed,
        },
        error: rec.error.clone(),
        started_at_ms: rec.started_at_ms,
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
    /// session's main agent takes its state from `state.status`, a run's
    /// step agents from `state.run`, and every subagent from `state.subagents`.
    /// Three pieces of state nothing above this actor holds together.
    ///
    /// A run has no main agent — it *is* its steps, and the definition rather
    /// than a person decides which one runs. The same fact
    /// [`SessionAgents::Workflow`](super::SessionAgents) records about the live
    /// actors, asked of the durable state instead.
    pub(super) fn agent_roster(&self, state: &SessionState) -> Vec<AgentEntry> {
        // Listed even though nothing spawned it, so that every agent is
        // reachable at one shape. A workflow session has no main agent — it
        // *is* its steps.
        let mut agents: Vec<AgentEntry> = match state.forest.root_is_workflow() {
            true => Vec::new(),
            false => vec![main_entry(&state.status())],
        };
        // Every run's executions — the session's own and any invoked one's —
        // then every subagent. Sub sessions are read through `read_agent`, as
        // before.
        agents.extend(
            state
                .forest
                .workflows()
                .flat_map(|(_, w)| w.run.steps.iter())
                .map(step_entry),
        );
        agents.extend(state.forest.sub_ids().into_iter().filter_map(|id| {
            state
                .forest
                .sub(id)
                .map(|rec| sub_entry(id, &state.forest, rec))
        }));
        // Stamped here, once, over whatever the per-kind builders produced.
        // Those builders each see one slice of the forest; only the actor can
        // resolve settings for an arbitrary key, and resolving it in four
        // places is four chances for main, step, subagent and sub session to
        // disagree about what they ran under.
        for entry in &mut agents {
            entry.preset = self.preset_of(state, &entry.id);
        }
        agents
    }

    /// The saved preset an agent's settings came from, by the id the roster
    /// speaks. `None` for an agent configured inline, and for an id this
    /// session does not host.
    ///
    /// Read-only, unlike `resolve_agent`: this answers about an agent's
    /// configuration and must not spawn a cold one to do it — the roster asks
    /// for every agent at once, and a session with a finished run would then
    /// wake every step it ever executed just to render a list.
    pub(super) fn preset_of(&self, state: &SessionState, agent_id: &str) -> Option<String> {
        let key = match agent_id {
            MAIN_AGENT_ID => AgentKey::Main,
            other => self.agent_key_of(state, Uuid::parse_str(other).ok()?)?,
        };
        self.effective_settings(state, key)
            .and_then(|s| s.source.preset())
            .map(str::to_string)
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
        ctx: &ActorContext<SessionInbox>,
        agent_id: Option<&str>,
    ) -> Option<AgentDetail> {
        let (key, agent) = self.resolve_agent(state, ctx, agent_id)?;
        let execution = match key {
            AgentKey::Step(id) => state
                .forest
                .step_of_agent(id)
                .and_then(|(run, index)| state.forest.workflow(run).and_then(|w| w.run.get(index))),
            AgentKey::Main | AgentKey::Sub(_) | AgentKey::SubSession(_) => None,
        };
        let node = match key {
            AgentKey::Sub(id) => state.forest.sub(id),
            AgentKey::Main | AgentKey::Step(_) | AgentKey::SubSession(_) => None,
        };
        let mut entry = match key {
            AgentKey::Main => main_entry(&state.status()),
            AgentKey::Step(_) => step_entry(execution?),
            AgentKey::Sub(id) => sub_entry(id, &state.forest, node?),
            AgentKey::SubSession(id) => {
                sub_session_entry(id, &state.forest, state.forest.sub_session(id)?)
            }
        };
        // From the settings just below, not from a second lookup: one agent
        // read must not be able to say it runs under one preset and was
        // configured by another.
        entry.preset = self
            .effective_settings(state, key)
            .and_then(|s| s.source.preset())
            .map(str::to_string);
        Some(AgentDetail {
            entry,
            // Resolved from the key, never from the session's own settings: a
            // step runs under its own preset, and a subagent under its tree's
            // root — the session's `AgentSettings` is the *first* step's, and
            // the wrong answer for any other agent.
            settings: self.effective_settings(state, key).cloned()?,
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
                .ask(|reply| AgentCommand::Read(AgentReadCommand::GetState { reply }))
                .await
                .ok()?,
        })
    }

    /// Aggregated usage. Totals come from this session's own durable record;
    /// only the live context size is asked of the agent.
    pub(super) async fn read_usage(&self, state: &SessionState) -> SessionUsageStats {
        let snapshot = match self.agent() {
            Some(agent) => agent
                .ask(|reply| AgentCommand::Read(AgentReadCommand::GetUsage { reply }))
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

    /// A session's main agent has no lifecycle of its own, so every one of
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

    /// A session lists the agent nothing spawned, so that every agent is
    /// reachable at one shape.
    #[tokio::test]
    async fn a_session_lists_its_main_agent_and_its_subagents() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, "research", "dig").await;
        wait_for_tree(&journal, id, |f| f.sub(sub).is_some()).await;

        let agents = roster(&session).await;
        assert_eq!(agents[0].id, MAIN_AGENT_ID);
        assert_eq!(agents[0].label, None, "the main agent is not one of many");
        assert!(
            agents.iter().any(|a| a.id == sub.to_string()),
            "a subagent is an agent of its session: {agents:?}"
        );
    }

    /// A subagent a sub session spawned names that sub session as its parent.
    ///
    /// It used to name nobody: the roster reported a parent only when the
    /// parent was another subagent, so an agent a sub session delegated to
    /// reached the client rooted on the main agent — drawn beside the sub
    /// session that spawned it, as though the session itself had.
    #[tokio::test]
    async fn a_subagent_of_a_sub_session_names_the_sub_session_as_its_parent() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub_session = session
            .ask(|reply| {
                SessionCommand::SubSession(
                    crate::sessions::session_actor::SubSessionCommand::Create {
                        parent: id,
                        seed: crate::sessions::run_forest::SeedMode::Fresh,
                        message: "try the other migration".into(),
                        // Nothing named, so it works where its parent works.
                        env: crate::sessions::session_actor::RequestedRuntime::Inherit,
                        reply,
                    },
                )
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_tree(&journal, id, |f| f.sub_session(sub_session).is_some()).await;

        let sub = session
            .ask(|reply| {
                SessionCommand::SubAgent(SubAgentCommand::Spawn {
                    caller: sub_session,
                    label: "audit".into(),
                    task: "audit the dependencies".into(),
                    agent_type: None,
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_tree(&journal, id, |f| f.sub(sub).is_some()).await;

        let agents = roster(&session).await;
        let entry = agents
            .iter()
            .find(|a| a.id == sub.to_string())
            .expect("the subagent is in the roster");
        assert_eq!(entry.parent, Some(sub_session));
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
        let provider = MockProvider::scripted(Script::of([Ok(concludes(
            serde_json::json!({"outcome": "p0"}),
        ))]));
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

    /// The defect this change exists to close: opening a workflow step used to
    /// show the *start* step's settings, because the session carried the first
    /// step's preset as its own `agent`. Here `plan` runs terra and `code` runs
    /// flash, and the code step's document — and the subagents it spawns —
    /// must report flash, not terra.
    #[tokio::test]
    async fn a_workflow_step_and_its_subagents_carry_the_steps_own_settings() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let mut spec = actor_spec_fixture();
        spec.kind = crate::sessions::spec::SessionKind::Workflow {
            run: Arc::new(two_model_run_spec_fixture("build the fix")),
        };
        f.deps
            .runtimes
            .create(
                &id.to_string(),
                "i1",
                "mock",
                &spec.runtime_env().expect("the fixture has a runtime"),
            )
            .await
            .expect("create");
        // The plan step concludes and routes to code; code stays in flight on
        // the blocking provider so it is the current agent while we read it.
        let plan_provider = MockProvider::scripted(Script::of([Ok(concludes(
            serde_json::json!({"outcome": "success"}),
        ))]));
        let code_gate = BlockingProvider::new();
        {
            let mut registry = f.deps.provider_registry.write().unwrap();
            registry.insert(
                "gpt-5.6-terra".to_string(),
                crate::sessions::spec::ModelEntry::provider_only(
                    plan_provider as Arc<dyn LlmProvider>,
                ),
            );
            registry.insert(
                "deepseek-v4-flash".to_string(),
                crate::sessions::spec::ModelEntry::provider_only(code_gate.clone()),
            );
        }
        let journal = f.journal();
        let session = f.start(id, spec).await;

        let run = wait_for_run(&journal, id, |r| {
            r.current()
                .is_some_and(|i| r.get(i).is_some_and(|s| s.step == "code"))
        })
        .await;
        let code_agent = run.current_agent().expect("the code step is in flight");

        // The step's own document reports flash — never the start step's terra.
        let detail = session
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::Agent {
                    agent_id: Some(code_agent.to_string()),
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("the code step is an agent of its run");
        assert_eq!(detail.settings.model, "deepseek-v4-flash");
        assert_eq!(
            detail.settings.thinking_effort.as_deref(),
            Some("high"),
            "the code step's own effort, not the planner's"
        );
        assert_eq!(
            detail.settings.memory_spaces,
            vec!["codebase".to_string()],
            "the code step's own memory spaces"
        );

        // A subagent spawned under the code step inherits its settings — the
        // model it runs, and the cap its spawn is counted against.
        let sub = spawn_sub(&session, "helper", "dig").await;
        wait_for_tree(&journal, id, |t| t.sub(sub).is_some()).await;
        let sub_detail = session
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::Agent {
                    agent_id: Some(sub.to_string()),
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("a subagent is an agent of its run");
        assert_eq!(sub_detail.settings.model, "deepseek-v4-flash");
        assert_eq!(sub_detail.settings.max_concurrent_subagents, Some(1));

        // And the concurrency cap is the code step's cap, not a session-wide
        // value: the step's budget of one is already spent.
        let res = session
            .ask(|reply| {
                SessionCommand::SubAgent(SubAgentCommand::Spawn {
                    caller: code_agent,
                    label: "second".into(),
                    task: "more".into(),
                    agent_type: None,
                    reply,
                })
            })
            .await
            .unwrap();
        assert_eq!(res.unwrap_err(), "1 subagents already active");
        code_gate.release();
    }

    /// This session's agents, read the way `GET /api/sessions/:id` reads them.
    async fn roster(session: &SessionRef) -> Vec<AgentEntry> {
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
        let counting = Arc::new(CountingJournal::new());
        let journal: Arc<dyn horsie_actor::Journal> = counting.clone();
        let agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock")
            .serve_in_process()
            .await
            .expect("fake agent");
        let f = fixture_on(journal.clone(), agent, None).await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(
                &id.to_string(),
                "i1",
                "mock",
                &actor_spec_fixture()
                    .runtime_env()
                    .expect("the fixture has a runtime"),
            )
            .await
            .expect("create");
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            crate::sessions::spec::ModelEntry::provider_only(
                Arc::new(EchoProvider) as Arc<dyn LlmProvider>
            ),
        );
        let session = f.start(id, actor_spec_fixture()).await;

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
