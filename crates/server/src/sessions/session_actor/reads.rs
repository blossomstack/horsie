//! Answering questions without waking anything.
//!
//! Every read is served from the resident actor's own memory, or forwarded to
//! the agent that owns the transcript. None of them touches the journal, so
//! opening a session to look at it costs no sandbox — which is what lets a
//! browser poll a session that is otherwise idle.
//!
//! No events and no state: this component only ever answers.

use super::{
    AgentDetail, AgentEntry, AgentKey, AgentKind, AgentStatus, AgentUsageEntry, CommandEffect,
    MAIN_AGENT_ID, ReadCommand, SessionActor, SessionDomainEvent, SessionSnapshot, SessionState,
    SessionUsageStats, SubSessionEntry,
};
use crate::agent_loop::AgentCommand;
use crate::agent_loop::AgentUsageSnapshot;
use crate::agent_loop::ReadCommand as AgentReadCommand;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::run_forest::{RunId, SubAgentRun, SubAgentStatus, SubSessionRun, WorkflowRun};
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
            ReadCommand::RuntimeCheckouts { runtime, reply } => {
                let urls = state.runtimes.get(&runtime).map(|rec| {
                    rec.env
                        .provision
                        .iter()
                        .filter(|step| step.uses == "git_checkout")
                        .filter_map(|step| {
                            step.with
                                .iter()
                                .find(|(key, _)| key == "url")
                                .map(|(_, url)| url.clone())
                        })
                        .collect()
                });
                let _ = reply.send(urls);
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
                    sub_sessions: actor.sub_session_roster(state),
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
fn main_entry(
    session: Uuid,
    state: &SessionState,
    status: &SessionStatus,
    model: Option<String>,
) -> AgentEntry {
    AgentEntry {
        id: MAIN_AGENT_ID.to_string(),
        parent: None,
        // The session's name *is* this agent's title — one fact, read from the
        // one place that owns it.
        title: state.forest.main_title().map(str::to_string),
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
        kind: AgentKind::Main,
        // A main agent is not briefed and does not conclude: it is talked to,
        // turn by turn, and its transcript is the whole of what it was asked
        // and what it answered.
        input: None,
        output: None,
        // Its subtree is the session, so this row's subtree total and the
        // session's total are the same number by construction.
        stats: state.agent_stats(Some(session), MAIN_AGENT_ID),
        model,
        started_at_ms: 0,
        ended_at_ms: 0,
        // Only a step belongs to a run.
        run: None,
        workflow: None,
    }
}

/// One execution of a workflow step. A step reached twice has two of these, and
/// each is its own agent.
fn step_entry(
    run_id: RunId,
    run: &WorkflowRun,
    execution: &StepRun,
    state: &SessionState,
    model: Option<String>,
) -> AgentEntry {
    AgentEntry {
        id: execution.agent.to_string(),
        // The agent that *invoked the run* — which is not "what spawned this
        // step", because nothing did: the definition chose it. `None` for the
        // session's own run, which nobody invited and which is the session.
        //
        // It used to be `None` for every step, and that is only true of a root
        // run: a run an agent started with `invoke_workflow` belongs to that
        // agent, and dropped here its executions arrived rootless and drew as
        // work the session had done directly.
        //
        // Filtered to rows the client actually holds, exactly as `sub_entry`
        // filters its own: the main agent is on the roster under a well-known
        // id rather than its uuid, and the schema already says an absent
        // parent means "rooted on this session's primary agent".
        parent: state
            .forest
            .entry(run_id)
            .and_then(|e| e.parent)
            .filter(|pid| state.forest.is_hosted_agent(*pid)),
        title: Some(execution.step.clone()),
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
        kind: AgentKind::Step,
        // A step's brief is its definition's, which the run graph holds; what
        // it produced is its own.
        input: None,
        output: execution
            .output
            .as_ref()
            .map(crate::sessions::workflow::output_as_input),
        stats: state.agent_stats(Some(execution.agent), &execution.agent.to_string()),
        model,
        started_at_ms: execution.started_at_ms,
        ended_at_ms: execution.ended_at_ms.unwrap_or(0),
        // Which run, and which definition it came from. One flat list of steps
        // cannot say either, and a session may host several runs at once.
        run: Some(run_id.0),
        workflow: Some(run.workflow.clone()),
    }
}

/// One sub session of a session.
///
/// Its status is read straight off the record rather than mapped from
/// anything: a sub session *is* a session, so `AgentStatus` is already the
/// vocabulary its record is kept in. `label` carries the title it gave itself,
/// which is `None` until it does — a client shows what it was branched from
/// instead.
fn sub_session_entry(
    id: Uuid,
    state: &SessionState,
    rec: &SubSessionRun,
    model: Option<String>,
) -> SubSessionEntry {
    let forest = &state.forest;
    let (created_at_ms, parent) = forest
        .owner_of_agent(id)
        .map(|(_, e)| (e.created_at_ms, e.parent))
        .unwrap_or((0, None));
    SubSessionEntry {
        id,
        // Rooted on the session's main agent, which is not a sub session, so
        // only a sub session parent is reported.
        parent: parent.filter(|pid| forest.sub_session(*pid).is_some()),
        title: rec.title.clone(),
        status: rec.status,
        created_at_ms,
        last_activity_ms: rec.last_activity_ms,
        input: rec.message.clone(),
        stats: state.agent_stats(Some(id), &id.to_string()),
        model,
    }
}

/// The same sub session, in the shape an agent-scoped read answers with.
///
/// A sub session is not a subagent, and this is the one place the two shapes
/// meet: `read_agent` answers for every agent a session hosts, and a sub
/// session is one of them. What it cannot honestly fill in — an end stamp, a
/// result — stays absent rather than being invented.
fn sub_session_as_agent(entry: &SubSessionEntry) -> AgentEntry {
    AgentEntry {
        id: entry.id.to_string(),
        parent: entry.parent,
        title: Some(entry.title.clone()),
        depth: 0,
        agent_type: None,
        status: entry.status,
        error: None,
        // Stamped by the roster loop, like every other entry's.
        preset: None,
        kind: AgentKind::SubSession,
        input: Some(entry.input.clone()),
        // A session is talked to; it owes nobody a result.
        output: None,
        stats: entry.stats,
        model: entry.model.clone(),
        started_at_ms: entry.created_at_ms,
        // A session is never *done*, so it has no end.
        ended_at_ms: 0,
        // Only a step belongs to a run.
        run: None,
        workflow: None,
    }
}

/// One subagent of the forest.
fn sub_entry(
    id: Uuid,
    state: &SessionState,
    rec: &SubAgentRun,
    model: Option<String>,
) -> AgentEntry {
    let forest = &state.forest;
    AgentEntry {
        id: id.to_string(),
        // Reported whenever the spawner is a row the client holds — another
        // subagent, a workflow step, or the sub session that spawned it —
        // because the session graph draws both rosters as one lineage and
        // anything it cannot hang this one off is drawn on the main agent
        // instead.
        //
        // A step's used to be dropped here, on the reasoning that a run's
        // shape is its workflow graph's. That is true of the *step*, and says
        // nothing about what a step delegated to: the subagent a coding step
        // spawned came out beside the step, as though the session itself had
        // spawned it.
        parent: forest
            .owner_of_agent(id)
            .and_then(|(_, e)| e.parent)
            .filter(|pid| forest.is_hosted_agent(*pid)),
        title: Some(rec.title.clone()),
        depth: forest.depth_of_agent(id).unwrap_or(0),
        agent_type: rec.agent_type.clone(),
        preset: None,
        status: match rec.status {
            SubAgentStatus::Running => AgentStatus::Running,
            SubAgentStatus::Completed => AgentStatus::Completed,
            SubAgentStatus::Failed => AgentStatus::Failed,
        },
        error: rec.error.clone(),
        kind: AgentKind::Sub,
        input: Some(rec.task.clone()),
        output: rec.output.clone(),
        stats: state.agent_stats(Some(id), &id.to_string()),
        model,
        started_at_ms: rec.started_at_ms,
        ended_at_ms: rec.ended_at_ms,
        // Only a step belongs to a run.
        run: None,
        workflow: None,
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
            false => vec![main_entry(
                self.id,
                state,
                &state.status(),
                self.model_of(state, MAIN_AGENT_ID),
            )],
        };
        // Every run's executions — the session's own and any invoked one's —
        // then every subagent. Sub sessions are their own roster, for the
        // reason `SubSessionEntry` gives.
        agents.extend(
            state
                .forest
                .workflows()
                .flat_map(|(id, w)| w.run.steps.iter().map(move |execution| (id, w, execution)))
                .map(|(id, w, execution)| {
                    let model = self.model_of(state, &execution.agent.to_string());
                    step_entry(id, w, execution, state, model)
                }),
        );
        agents.extend(state.forest.sub_ids().into_iter().filter_map(|id| {
            let model = self.model_of(state, &id.to_string());
            state
                .forest
                .sub(id)
                .map(|rec| sub_entry(id, state, rec, model))
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

    /// The settings one agent runs under, by the id the roster speaks. `None`
    /// for an id this session does not host.
    ///
    /// Read-only, unlike `resolve_agent`: this answers about an agent's
    /// configuration and must not spawn a cold one to do it — the roster asks
    /// for every agent at once, and a session with a finished run would then
    /// wake every step it ever executed just to render a list.
    fn settings_of<'a>(
        &'a self,
        state: &'a SessionState,
        agent_id: &str,
    ) -> Option<&'a crate::sessions::spec::AgentSettings> {
        let key = match agent_id {
            MAIN_AGENT_ID => AgentKey::Main,
            other => self.agent_key_of(state, Uuid::parse_str(other).ok()?)?,
        };
        self.effective_settings(state, key)
    }

    /// The saved preset an agent's settings came from. `None` for an agent
    /// configured inline.
    pub(super) fn preset_of(&self, state: &SessionState, agent_id: &str) -> Option<String> {
        self.settings_of(state, agent_id)
            .and_then(|s| s.source.preset())
            .map(str::to_string)
    }

    /// The model it runs, as a name the HTTP layer can look a context window
    /// up by — which this actor cannot, not knowing which models are
    /// configured.
    fn model_of(&self, state: &SessionState, agent_id: &str) -> Option<String> {
        self.settings_of(state, agent_id).map(|s| s.model.clone())
    }

    /// Every sub session branched out of this one, with the numbers only this
    /// actor holds.
    pub(super) fn sub_session_roster(&self, state: &SessionState) -> Vec<SubSessionEntry> {
        state
            .forest
            .sub_session_ids()
            .into_iter()
            .filter_map(|id| {
                let model = self.model_of(state, &id.to_string());
                state
                    .forest
                    .sub_session(id)
                    .map(|rec| sub_session_entry(id, state, rec, model))
            })
            .collect()
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
        // The run as well as the execution: a step's roster entry names both,
        // and this read builds the same entry the roster does.
        let execution = match key {
            AgentKey::Step(id) => state.forest.step_of_agent(id).and_then(|(run, index)| {
                let w = state.forest.workflow(run)?;
                Some((run, w, w.run.get(index)?))
            }),
            AgentKey::Main | AgentKey::Sub(_) | AgentKey::SubSession(_) => None,
        };
        let node = match key {
            AgentKey::Sub(id) => state.forest.sub(id),
            AgentKey::Main | AgentKey::Step(_) | AgentKey::SubSession(_) => None,
        };
        let model = self.effective_settings(state, key).map(|s| s.model.clone());
        let mut entry = match key {
            AgentKey::Main => main_entry(self.id, state, &state.status(), model),
            AgentKey::Step(_) => {
                let (run_id, run, step) = execution?;
                step_entry(run_id, run, step, state, model)
            }
            AgentKey::Sub(id) => sub_entry(id, state, node?, model),
            AgentKey::SubSession(id) => sub_session_as_agent(&sub_session_entry(
                id,
                state,
                state.forest.sub_session(id)?,
                model,
            )),
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
                (None, Some((_, _, step))) => step
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
            context_tokens: 0,
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
            let entry = main_entry(Uuid::nil(), &SessionState::default(), &status, None);
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
        assert_eq!(
            agents[0].title, None,
            "an untitled session's main agent has no title yet"
        );
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
                        title: "the other migration".into(),
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
                    title: "audit".into(),
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

    /// Every kind of spawner, against every kind of work it can spawn, and the
    /// parent the roster reports for each.
    ///
    /// One assertion over the whole matrix rather than a test per pair,
    /// because the bug was never in one pair — it was in a *list of kinds*
    /// written out twice, and a list is blind to the kind nobody thought of.
    /// Both `step` rows below came back `None` before `is_hosted_agent`
    /// replaced those lists: a subagent a coding step spawned, and a workflow
    /// a step invoked, each drew on the main agent as though the session had
    /// started it directly.
    ///
    /// `None` is not a hole here — the schema reads an absent parent as
    /// "rooted on this session's primary agent", and the main agent is on the
    /// roster under a well-known id rather than its uuid. So the three `None`
    /// rows are the three things the main agent itself started.
    #[test]
    fn the_roster_reports_a_parent_for_every_spawner_that_is_a_row_a_client_holds() {
        use crate::sessions::run_forest::{RunForest, RuntimeChoice, SeedMode};
        use crate::sessions::workflow::WorkflowRunSpec;
        use std::sync::Arc;

        let uid = |n: u8| Uuid::from_bytes([n; 16]);
        let (session, sub, sub_session, step) = (uid(1), uid(2), uid(3), uid(4));
        let graph = |name: &str| {
            Arc::new(WorkflowRunSpec {
                workflow: name.into(),
                start: "s".into(),
                steps: vec![],
                input: "go".into(),
                max_steps: 10,
            })
        };
        let mut f = RunForest::default();
        f.apply_root_agent(session, 0, RuntimeChoice::Pending);
        f.apply_sub_spawned(sub, session, "sub".into(), "t".into(), None, 10);
        f.apply_sub_session_created(
            sub_session,
            session,
            0,
            SeedMode::Fresh,
            "m".into(),
            "sub session".into(),
            10,
            RuntimeChoice::Inherit,
        );
        // The step exists only as a run's execution, so its run comes first.
        // Every spawner then invokes a run and spawns a subagent, and the two
        // that may branch a sub session do.
        for (run, parent, agent, label) in [
            (uid(10), session, step, "run by main"),
            (uid(11), sub, uid(61), "run by sub"),
            (uid(12), sub_session, uid(62), "run by sub session"),
            (uid(13), step, uid(63), "run by step"),
        ] {
            f.apply_run_created(RunId(run), parent, label.into(), graph(label), 20);
            f.apply_step_started(
                RunId(run),
                "s".into(),
                agent,
                1,
                None,
                None,
                "in".into(),
                21,
            );
        }
        for (n, parent, label) in [
            (30u8, session, "subagent of main"),
            (31, sub, "subagent of sub"),
            (32, sub_session, "subagent of sub session"),
            (33, step, "subagent of step"),
        ] {
            f.apply_sub_spawned(uid(n), parent, label.into(), "t".into(), None, 30);
        }
        // Only a session branches: `branchable` refuses a subagent's or a
        // step's parent outright, so those two pairs cannot exist to be drawn.
        f.apply_sub_session_created(
            uid(40),
            sub_session,
            0,
            SeedMode::Fresh,
            "m".into(),
            "sub session of sub session".into(),
            30,
            RuntimeChoice::Inherit,
        );
        let state = SessionState {
            forest: f,
            ..Default::default()
        };

        let named = |id: Option<Uuid>| match id {
            None => "rooted on main",
            Some(x) if x == sub => "sub",
            Some(x) if x == sub_session => "sub session",
            Some(x) if x == step => "step",
            Some(_) => "unexpected",
        };
        let mut got: Vec<(String, &str)> = Vec::new();
        for id in state.forest.sub_ids() {
            let rec = state.forest.sub(id).cloned().expect("a subagent");
            got.push((
                rec.title.clone(),
                named(sub_entry(id, &state, &rec, None).parent),
            ));
        }
        for (run_id, w) in state
            .forest
            .workflows()
            .map(|(i, w)| (i, w.clone()))
            .collect::<Vec<_>>()
        {
            for execution in &w.run.steps {
                let entry = step_entry(run_id, &w, execution, &state, None);
                got.push((format!("step of {}", w.workflow), named(entry.parent)));
            }
        }
        for id in state.forest.sub_session_ids() {
            let rec = state
                .forest
                .sub_session(id)
                .cloned()
                .expect("a sub session");
            let entry = sub_session_entry(id, &state, &rec, None);
            got.push((rec.title.clone(), named(entry.parent)));
        }
        got.sort();
        assert_eq!(
            got,
            vec![
                ("step of run by main".to_string(), "rooted on main"),
                ("step of run by step".to_string(), "step"),
                ("step of run by sub".to_string(), "sub"),
                ("step of run by sub session".to_string(), "sub session"),
                ("sub".to_string(), "rooted on main"),
                ("sub session".to_string(), "rooted on main"),
                ("sub session of sub session".to_string(), "sub session"),
                ("subagent of main".to_string(), "rooted on main"),
                ("subagent of step".to_string(), "step"),
                ("subagent of sub".to_string(), "sub"),
                ("subagent of sub session".to_string(), "sub session"),
            ]
        );
    }

    /// A subagent a workflow step spawned names that step as its parent.
    ///
    /// It used to name nobody. The roster reported a parent only when it was
    /// another subagent or a sub session, on the reasoning that a run's shape
    /// is its workflow graph's — true of the *step*, and silent about what the
    /// step delegated to. The agent a coding step spawned therefore reached
    /// the client rootless and drew on the main agent, beside the step that
    /// spawned it rather than under it.
    #[tokio::test]
    async fn a_subagent_of_a_workflow_step_names_the_step_as_its_parent() {
        let (_f, session, id, journal) = spawn_run_with_provider(BlockingProvider::new()).await;
        let run = wait_for_run(&journal, id, |r| r.current().is_some()).await;
        let step_agent = run.steps[0].agent;

        let sub = session
            .ask(|reply| {
                SessionCommand::SubAgent(SubAgentCommand::Spawn {
                    caller: step_agent,
                    title: "toolchain".into(),
                    task: "install the toolchain".into(),
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
        assert_eq!(entry.parent, Some(step_agent));
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
        assert_eq!(agents[0].title.as_deref(), Some(run.steps[0].step.as_str()));
        assert_eq!(agents[0].status, AgentStatus::Running);
    }

    /// Which run an execution belongs to, and what that run was started from.
    ///
    /// The roster lists every execution of every run a session hosts — its own
    /// and any an agent invoked — and it used to list them flat and
    /// parentless, on the reasoning that the definition chose each step so no
    /// agent is its parent. True of the step, and not enough: a client drawing
    /// the run as the sequence it is has nothing to group by, and two runs in
    /// one session are indistinguishable from one run with twice the steps.
    #[tokio::test]
    async fn a_step_names_the_run_it_belongs_to_and_the_workflow_it_came_from() {
        let (_f, session, id, journal) = spawn_run_with_provider(BlockingProvider::new()).await;
        wait_for_run(&journal, id, |r| r.current().is_some()).await;

        let agents = roster(&session).await;
        let step = &agents[0];
        assert_eq!(step.kind, AgentKind::Step);
        assert!(step.run.is_some(), "a step names its run: {step:?}");
        assert!(
            step.workflow.is_some(),
            "a step names the workflow its run was started from: {step:?}"
        );
        // The session's *own* run: nobody invited it, so there is no inviting
        // agent to name. An invoked run's steps carry that agent instead.
        assert_eq!(step.parent, None);
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
                crate::runtime_manager::RuntimeAddress {
                    session: &id.to_string(),
                    runtime: &id.to_string(),
                    incarnation: "i1",
                },
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
                    title: "second".into(),
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
                    artifacts: Vec::new(),
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
                crate::runtime_manager::RuntimeAddress {
                    session: &id.to_string(),
                    runtime: &id.to_string(),
                    incarnation: "i1",
                },
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
                    artifacts: Vec::new(),
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
