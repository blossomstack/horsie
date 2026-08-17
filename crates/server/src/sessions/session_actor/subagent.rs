//! The tree of delegated work.
//!
//! Enforces depth and concurrency, persists a spawn *before* the child actor
//! exists — a crash between the two replays as a node recovery can fail, which
//! is strictly better than an untracked agent — records terminal results, and
//! reconciles nodes a dead process left running.
//!
//! **Never asks what kind of session it is in.** Every query it makes spans the
//! whole forest, which is what makes a workflow step's subagents work through
//! the identical code path a conversation's use. The previous shape put the tree
//! inside the session's mode and every read silently answered empty for a run.

use super::component::{ActionCx, Component};
use super::context::SessionAgentKind;
use super::{
    AgentAction, AgentKey, AgentPlan, CommandEffect, SessionActor, SessionCommand,
    SessionDomainEvent, SessionState, SubAgentCommand, TurnEnd,
};
use crate::agent_loop::{AgentCommand, Incoming};
use crate::sessions::addressing::SessionInbox;
use crate::sessions::subagents::{
    INTERRUPTED_ERROR, MAX_SUBAGENT_DEPTH, SubAgentParent, TreeOwner,
};
use horsie_actor::ActorContext;
use horsie_actor::ActorRef;
use horsie_actor::ReplyTo;
use horsie_models::now_ms;
use tokio::sync::oneshot;
use uuid::Uuid;

/// SubAgents.
pub(super) struct SubAgents;

impl SubAgents {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: SubAgentCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            SubAgentCommand::Spawn {
                caller,
                label,
                task,
                agent_type,
                reply,
                ..
            } => {
                let owner = state.subagents.owner_for(caller, state.root_owner());
                let Some(parent_depth) = owner
                    .and_then(|owner| state.subagents.tree(owner))
                    .map_or_else(
                        // An empty forest still has a Main at depth 0: the very
                        // first spawn of a session has no tree to look in yet.
                        || matches!(caller, SubAgentParent::Main).then_some(0),
                        |tree| tree.depth_of(caller),
                    )
                else {
                    let _ = reply.send(Err("caller is not a known agent".to_string()));
                    return CommandEffect::none();
                };
                if parent_depth >= MAX_SUBAGENT_DEPTH {
                    let _ = reply.send(Err(format!(
                        "max subagent depth {MAX_SUBAGENT_DEPTH} reached"
                    )));
                    return CommandEffect::none();
                }
                // The cap is the *caller's* settings' cap: a workflow step's
                // spawns are counted against the step's preset, never against a
                // session-wide value that nothing in a run owns.
                let Some(settings) = actor.effective_settings_for_parent(state, caller) else {
                    let _ = reply.send(Err("caller is not a known agent".to_string()));
                    return CommandEffect::none();
                };
                let max = settings.max_subagents();
                if state.subagents.active_count() >= max {
                    let _ = reply.send(Err(format!("{max} subagents already active")));
                    return CommandEffect::none();
                }
                // Persist first, spawn second: a crash between the two replays
                // as a Running node with no actor, which recovery reconciles
                // to failed — never an untracked agent.
                let id = Uuid::new_v4();
                let spawned = SessionDomainEvent::SubAgentSpawned {
                    at_ms: now_ms(),
                    id,
                    parent: caller,
                    label: label.clone(),
                    task: task.clone(),
                    depth: parent_depth + 1,
                    agent_type: agent_type.clone(),
                };
                let (tx, rx) = oneshot::channel();
                let self_ref = actor.me(ctx);
                tokio::spawn(async move {
                    let persisted = rx.await.unwrap_or_else(|_| {
                        Err(horsie_actor::JournalError::Backend(
                            "spawn ack channel closed".to_string(),
                        ))
                    });
                    let _ = self_ref
                        .tell(SessionCommand::SubAgent(SubAgentCommand::FinishSpawn {
                            id,
                            task,
                            agent_type,
                            reply,
                            persisted,
                        }))
                        .await;
                });
                CommandEffect::persist(vec![spawned]).and_ack(ReplyTo::from_sender(tx))
            }
            SubAgentCommand::FinishSpawn {
                id,
                task,
                agent_type,
                reply,
                persisted,
            } => {
                if let Err(e) = persisted {
                    let _ = reply.send(Err(format!("persist subagent: {e}")));
                    return CommandEffect::none();
                }
                let Some(agent) = actor.spawn_sub_agent_actor(ctx, state, id, agent_type) else {
                    let _ = reply.send(Err("could not start the subagent".to_string()));
                    return CommandEffect::none();
                };
                // The task is the first thing in this agent's queue, which it
                // drains at once — there is nothing else in it and nothing in
                // flight. Queued rather than run directly so a subagent has one
                // way in, whatever is addressed to it.
                let _ = agent
                    .tell(AgentCommand::Enqueue {
                        item: Incoming::User {
                            id: format!("task:{id}"),
                            text: task,
                        },
                        ack: None,
                    })
                    .await;
                let _ = reply.send(Ok(id));
                CommandEffect::none()
            }
            SubAgentCommand::Status {
                caller, id, reply, ..
            } => {
                // Visibility is answered within the caller's own tree: a step
                // and a conversation each see their own, and neither learns the
                // other exists.
                let tree = state
                    .subagents
                    .owner_for(caller, state.root_owner())
                    .and_then(|owner| state.subagents.tree(owner));
                let rendered = match id {
                    Some(id) if tree.is_some_and(|t| t.visible_to(caller, &id)) => tree
                        .and_then(|t| t.render_node(&id))
                        .ok_or_else(|| format!("no such subagent: {id}")),
                    // Out-of-subtree and unknown ids are indistinguishable —
                    // neither confirms the node exists.
                    Some(id) => Err(format!("no such subagent: {id}")),
                    None => Ok(tree
                        .map(|t| t.render_subtree(caller))
                        .unwrap_or_else(|| "No subagents.\n".to_string())),
                };
                let _ = reply.send(rendered);
                CommandEffect::none()
            }
            SubAgentCommand::Reconcile => {
                let interrupted = state.subagents.interrupted();
                if interrupted.is_empty() {
                    return CommandEffect::none();
                }
                CommandEffect::persist(
                    interrupted
                        .into_iter()
                        .map(|id| SessionDomainEvent::SubAgentFailed {
                            at_ms: now_ms(),
                            id,
                            error: INTERRUPTED_ERROR.to_string(),
                        })
                        .collect(),
                )
            }
        }
    }
}

/// Handlers that belong to this component but act on the actor's own
/// fields — the roster, the supervisor link, the spawn helpers. An inherent
/// `impl` in a child module sees them, so moving the code needed no plumbing.
impl SessionActor {
    /// A subagent's outcome: record it in the tree, then deliver every result
    /// owed to idle parents — wakes for subagent parents, a turn (via
    /// `drain`) when the main agent is owed and idle.
    pub(super) async fn on_sub_agent_outcome(
        &mut self,
        state: &SessionState,
        id: Uuid,
        end: TurnEnd,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        if state.subagents.node(id).is_none() {
            tracing::warn!(subagent = %id, "outcome from an unknown subagent; ignored");
            return CommandEffect::none();
        }
        let terminal = match end {
            TurnEnd::Concluded { output } => SessionDomainEvent::SubAgentCompleted {
                at_ms: now_ms(),
                id,
                output: output
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| output.to_string()),
            },
            TurnEnd::Failed { error, .. } => SessionDomainEvent::SubAgentFailed {
                at_ms: now_ms(),
                id,
                error,
            },
            // Defensive: a subagent has no ask or timer tools, so neither
            // outcome should ever occur.
            TurnEnd::Asked => SessionDomainEvent::SubAgentFailed {
                at_ms: now_ms(),
                id,
                error: "subagent asked the user; not supported".to_string(),
            },
            TurnEnd::Parked => SessionDomainEvent::SubAgentFailed {
                at_ms: now_ms(),
                id,
                error: "subagent parked; timers are not supported in sessions".to_string(),
            },
            // A subagent's interruption is repaired from the forest at *session*
            // load, by `SubAgents::on_load`, which is also where the parent is
            // owed the failure. This report cannot arrive first: a subagent
            // actor stays cold and spawns on demand, so its own recovery runs
            // long after the node was reconciled, and acting on it would fail
            // the same node a second time.
            TurnEnd::Interrupted => return CommandEffect::none(),
        };
        self.persist_and_advance(state, vec![terminal], ctx).await
    }
    /// Spawn a resident subagent actor — journal replay only; the caller
    /// decides whether a run starts (spawn) or not (recovery).
    /// Spawn one subagent's actor. `agent_type` names a plugin-declared agent to
    /// run as, and travels no further than the provider: the *definition* is
    /// resolved from the library scan when the subagent runs, so an agent whose
    /// plugin was removed in between fails loudly rather than running with a
    /// prompt nobody can point at.
    pub(super) fn spawn_sub_agent_actor(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
        agent_type: Option<String>,
    ) -> Option<ActorRef<AgentCommand>> {
        // Derived from the node's stored parent: a cold node woken to run must
        // run under the same settings its tree root ran under — a workflow
        // step's spawns under the step's preset, a conversation's under the
        // main agent's — never a fabricated session-wide value.
        let settings = self.effective_settings(state, AgentKey::Sub(id)).cloned()?;
        // A worker owes a report: it can delegate further, but it cannot ask,
        // name the session or branch it.
        let equipment = crate::sessions::runners::assemble(
            crate::sessions::runners::RunnerKind::SubAgent,
            &crate::sessions::runners::Assembly {
                settings: &settings,
                agent: crate::sessions::runners::AgentId(id),
                // This worker's own depth, off the node the tree already holds
                // — the number the depth gate is answered from when it spawns a
                // worker of its own.
                depth: state.subagents.node(id).map_or(0, |rec| rec.depth),
                unattended: self.spec().is_unattended(),
                fork: None,
                agent_type: agent_type.clone(),
            },
        );
        self.spawn_agent(
            ctx,
            state,
            AgentPlan {
                kind: SessionAgentKind::Sub(id),
                settings,
                equipment,
                agent_type,
            },
        )
        .map(|resident| resident.actor)
    }
}

impl Component for SubAgents {
    /// Deliver every result a child owes its parent. Reads the forest, so it
    /// works in a run exactly as in a conversation — and it never asks which it
    /// is in.
    fn actions(_cx: &ActionCx<'_>, state: &SessionState) -> Vec<AgentAction> {
        crate::sessions::orchestrator::owed_deliveries(state)
    }

    /// Nodes left `Running` by a dead process. Their runs are over; the parents
    /// are owed the failure like any other terminal result.
    fn on_load(_cx: &ActionCx<'_>, state: &SessionState) -> Option<SessionCommand> {
        (!state.subagents.interrupted().is_empty())
            .then_some(SessionCommand::SubAgent(SubAgentCommand::Reconcile))
    }

    /// This is the invariant that keeps a forty-minute tool call from being
    /// unloaded out from under itself.
    fn busy(state: &SessionState) -> bool {
        state.subagents.has_active()
    }

    /// The forest. The owner is resolved from the state as it stands *before*
    /// the event, which is the step in flight for a run and `Main` otherwise.
    ///
    /// Pure, and an associated function rather than a method: replay runs with
    /// no instance in scope, which is what makes a recovered session and a live
    /// one follow the same path.
    // The fallthrough is unreachable by construction: `SessionActor::apply_event`
    // matches every variant explicitly and routes each to exactly one component,
    // so a newly added event fails to compile *there* — which is where it should
    // be classified — rather than silently reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut SessionState, event: &SessionDomainEvent) {
        match event.clone() {
            SessionDomainEvent::SubAgentSpawned {
                id,
                parent,
                label,
                task,
                depth,
                at_ms,
                agent_type,
            } => {
                // The owner is resolved against the state as it stands *before*
                // this event: the step in flight for a run, Main otherwise.
                let owner = state
                    .subagents
                    .owner_for(parent, state.root_owner())
                    .unwrap_or(TreeOwner::Main);
                state
                    .subagents
                    .tree_mut(owner)
                    .apply_spawned(id, parent, label, task, depth, at_ms, agent_type);
            }
            SessionDomainEvent::SubAgentRunning { id, at_ms } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state.subagents.tree_mut(owner).apply_running(id, at_ms);
                }
            }
            SessionDomainEvent::SubAgentCompleted { id, output, at_ms } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state
                        .subagents
                        .tree_mut(owner)
                        .apply_completed(id, output, at_ms);
                }
            }
            SessionDomainEvent::SubAgentFailed { id, error, at_ms } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state
                        .subagents
                        .tree_mut(owner)
                        .apply_failed(id, error, at_ms);
                }
            }
            SessionDomainEvent::SubAgentNotified { id, .. } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state.subagents.tree_mut(owner).apply_notified(id);
                }
            }
            other => unreachable!("SubAgents was handed {other:?}"),
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
    //! The forest: what a spawn records, what an outcome delivers, and what a
    //! snapshot written under an older shape still loads as.
    use super::super::testing::*;
    use super::super::*;
    use super::*;
    use crate::sessions::session_actor::testing::seed_session;

    use std::sync::Arc;
    use uuid::Uuid;

    /// A session whose only repair is an interrupted subagent still tells its
    /// supervisor what it recovered as.
    ///
    /// Recovery skips its own report whenever a repair is queued, on the
    /// grounds that the repair reports the status it lands on. `Reconcile` is
    /// the repair that does not: it persists `SubAgentFailed` and nothing
    /// reports. So this session used to load and say nothing at all, leaving
    /// its row blank until something unrelated moved it.
    #[tokio::test]
    async fn a_subagent_only_repair_still_reports_a_status() {
        // The provider hangs, so the subagent is genuinely still `Running` when
        // the session goes away — which is what a fold reads as interrupted,
        // and what `SubAgents::on_load` queues a `Reconcile` for.
        let gate = BlockingProvider::new();
        let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
        let sub = spawn_sub(&session, "worker", "dig").await;
        wait_for_state(&journal, id, "the subagent to be running", |s| {
            s.subagents.interrupted().contains(&sub)
        })
        .await;
        drop(session);

        let before = f.list_revision().await;
        f.node.restart().await;
        let _revived = f.start(id, actor_spec_fixture()).await;
        assert!(
            wait_for_report(&f, before).await,
            "a loaded session must report a status, repairs or not"
        );
    }

    #[test]
    fn subagent_events_fold_into_the_tree() {
        use crate::sessions::subagents::{SubAgentParent, SubAgentStatus};
        let id = Uuid::new_v4();
        let s = fold(vec![SessionDomainEvent::SubAgentSpawned {
            at_ms: 0,
            id,
            parent: SubAgentParent::Main,
            label: "research".into(),
            task: "look into it".into(),
            depth: 1,
            agent_type: None,
        }]);
        assert_eq!(s.subagents.active_count(), 1);

        let s = SessionActor::apply_event(
            s,
            SessionDomainEvent::SubAgentCompleted {
                at_ms: 0,
                id,
                output: "answer".into(),
            },
        );
        let rec = s.subagents.node(id).unwrap();
        assert_eq!(rec.status, SubAgentStatus::Completed);
        assert!(!rec.notified);

        let s = SessionActor::apply_event(s, SessionDomainEvent::SubAgentNotified { at_ms: 0, id });
        assert!(s.subagents.node(id).unwrap().notified);
    }

    #[test]
    fn a_running_then_failed_subagent_reads_as_interrupted_then_terminal() {
        use crate::sessions::subagents::SubAgentParent;
        let id = Uuid::new_v4();
        let s = fold(vec![SessionDomainEvent::SubAgentSpawned {
            at_ms: 0,
            id,
            parent: SubAgentParent::Main,
            label: "w".into(),
            task: "t".into(),
            depth: 1,
            agent_type: None,
        }]);
        assert_eq!(s.subagents.interrupted(), vec![id]);
        let s = SessionActor::apply_event(
            s,
            SessionDomainEvent::SubAgentFailed {
                at_ms: 0,
                id,
                error: "interrupted by restart".into(),
            },
        );
        assert!(s.subagents.interrupted().is_empty());
    }

    #[tokio::test]
    async fn spawn_records_a_running_subagent_in_the_tree() {
        // Completion routing lands with outcome handling (next task); here the
        // spawn itself is what must be durable and attributed.
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        let sub = spawn_sub(&session, "research", "dig into it").await;
        wait_for_tree(&journal, id, |t| {
            t.node(sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
        })
        .await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        let rec = &state.subagents.node(sub).unwrap();
        assert_eq!(rec.depth, 1);
        assert_eq!(rec.parent, crate::sessions::subagents::SubAgentParent::Main);
        assert_eq!(rec.label, "research");
        assert_eq!(rec.task, "dig into it");
    }

    #[tokio::test]
    async fn spawn_beyond_depth_four_is_rejected() {
        // A hanging provider keeps every spawned node Running, so the chain
        // builds deterministically: Main → d1 → d2 → d3 → d4, and d4's spawn
        // is refused.
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        let mut parent = crate::sessions::subagents::SubAgentParent::Main;
        for _ in 0..4 {
            let id_child = session
                .ask(|reply| {
                    SessionCommand::SubAgent(SubAgentCommand::Spawn {
                        caller: parent,
                        agent: crate::sessions::runners::ids::AgentId::new_v4(),
                        label: "w".into(),
                        task: "t".into(),
                        agent_type: None,
                        reply,
                    })
                })
                .await
                .unwrap()
                .unwrap();
            wait_for_tree(&journal, id, |t| t.has_active()).await;
            parent = crate::sessions::subagents::SubAgentParent::SubAgent(id_child);
        }
        let res = session
            .ask(|reply| {
                SessionCommand::SubAgent(SubAgentCommand::Spawn {
                    caller: parent,
                    agent: crate::sessions::runners::ids::AgentId::new_v4(),
                    label: "x".into(),
                    task: "y".into(),
                    agent_type: None,
                    reply,
                })
            })
            .await
            .unwrap();
        assert_eq!(res.unwrap_err(), "max subagent depth 4 reached");
    }

    #[tokio::test]
    async fn spawn_beyond_the_concurrency_cap_is_rejected() {
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        for _ in 0..8 {
            let _ = spawn_sub(&session, "w", "t").await;
        }
        wait_for_tree(&journal, id, |t| t.active_count() == 8).await;
        let res = session
            .ask(|reply| {
                SessionCommand::SubAgent(SubAgentCommand::Spawn {
                    caller: crate::sessions::subagents::SubAgentParent::Main,
                    agent: crate::sessions::runners::ids::AgentId::new_v4(),
                    label: "x".into(),
                    task: "y".into(),
                    agent_type: None,
                    reply,
                })
            })
            .await
            .unwrap();
        assert_eq!(res.unwrap_err(), "8 subagents already active");
    }

    #[tokio::test]
    async fn spawn_from_an_unknown_caller_is_rejected() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let res = session
            .ask(|reply| {
                SessionCommand::SubAgent(SubAgentCommand::Spawn {
                    caller: crate::sessions::subagents::SubAgentParent::SubAgent(Uuid::new_v4()),
                    agent: crate::sessions::runners::ids::AgentId::new_v4(),
                    label: "x".into(),
                    task: "y".into(),
                    agent_type: None,
                    reply,
                })
            })
            .await
            .unwrap();
        assert_eq!(res.unwrap_err(), "caller is not a known agent");
    }

    #[tokio::test]
    async fn a_completed_subagent_notifies_an_idle_main_agent() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, "research", "dig").await;
        // Owed, then delivered: the tree's notified flag flips exactly once.
        wait_for_tree(&journal, id, |t| t.node(sub).is_some_and(|r| r.notified)).await;
        // …and then wait for the main agent to have *taken* it. The flag says
        // the result was handed over — one message into main's mailbox — never
        // that main has appended it, so reading the history the moment the flag
        // flips is a race. It only ever passed because nothing else was
        // competing for the scheduler at that instant.
        let texts = wait_for_subagent_text(&session, |texts| {
            texts.iter().any(|t| {
                t.contains("[subagent \"research\" completed]") && t.contains("sub answer")
            })
        })
        .await;
        assert!(
            texts.iter().any(
                |t| t.contains("[subagent \"research\" completed]") && t.contains("sub answer")
            ),
            "the main agent must be told the result: {texts:?}"
        );
        // The result is a part of its own, not text merged into the user's
        // message: that separation is what lets a client render it as agent
        // work instead of as something the person typed.
        assert!(
            user_texts(&main_history(&session).await)
                .iter()
                .all(|t| !t.contains("[subagent ")),
            "a result must never land in the user text"
        );
    }

    /// The child's own log, not the parent's card. A subagent page folds the
    /// same `TurnBegan`/`TurnEnded` pair every other agent's does, and the
    /// terminal entry used to reach only the parent — so a finished subagent's
    /// own page read `RUNNING` for ever while the forest beside it said
    /// `Completed`.
    #[tokio::test]
    async fn a_completed_subagent_closes_the_turn_in_its_own_log() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, "research", "dig").await;
        wait_for_tree(&journal, id, |t| t.node(sub).is_some_and(|r| r.notified)).await;

        let outcomes = turn_outcomes(&agent_history(&session, Some(sub.to_string())).await);
        assert!(
            matches!(
                outcomes.as_slice(),
                [horsie_agentcore::TurnOutcome::Ended(_)]
            ),
            "a subagent's one turn ends in its own log: {outcomes:?}"
        );
    }

    /// Stop, addressed to a subagent.
    ///
    /// The child is cancelled *and* the parent is told, because the parent is
    /// blocked on a `spawn_agent` result: stopping the child quietly would leave
    /// it waiting for one that can never come. Reported as a failed child, which
    /// is the shape crash recovery already delivers for the same situation.
    #[tokio::test]
    async fn stopping_a_subagent_cancels_it_and_tells_the_parent() {
        let provider = BlockingProvider::new();
        let (_f, session, id, journal) =
            spawn_session_with_provider(provider.clone() as Arc<dyn horsie_agentcore::LlmProvider>)
                .await;
        let sub = spawn_sub(&session, "research", "dig").await;
        wait_for_tree(&journal, id, |t| {
            t.node(sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
        })
        .await;

        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::Stop {
                    agent_id: sub.to_string(),
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("a working subagent is stoppable");

        // Owed and delivered: a stopped child still owes its parent an answer.
        wait_for_tree(&journal, id, |t| t.node(sub).is_some_and(|r| r.notified)).await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        let rec = state.subagents.node(sub).unwrap();
        assert_eq!(
            rec.status,
            crate::sessions::subagents::SubAgentStatus::Failed
        );
        let texts = subagent_texts(&main_history(&session).await);
        assert!(
            texts
                .iter()
                .any(|t| t.contains("[subagent \"research\" failed]")),
            "the parent must hear that its child was stopped: {texts:?}"
        );
        provider.release();
    }

    /// A subagent that failed says so where a reader opening it will look.
    #[tokio::test]
    async fn a_failed_subagents_own_log_carries_the_error() {
        let provider = FailOnNeedleProvider {
            needle: "doomed task".to_string(),
        };
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(provider)).await;
        let sub = spawn_sub(&session, "risky", "doomed task").await;
        wait_for_tree(&journal, id, |t| t.node(sub).is_some_and(|r| r.notified)).await;

        let outcomes = turn_outcomes(&agent_history(&session, Some(sub.to_string())).await);
        match outcomes.as_slice() {
            [horsie_agentcore::TurnOutcome::Failed(f)] => {
                assert!(f.error.contains("bad key"), "{:?}", f.error);
            }
            other => panic!("a failed subagent's turn ends as failed: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failed_subagent_reports_the_error_to_its_parent() {
        let provider = FailOnNeedleProvider {
            needle: "doomed task".to_string(),
        };
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(provider)).await;
        let sub = spawn_sub(&session, "risky", "doomed task").await;
        wait_for_tree(&journal, id, |t| t.node(sub).is_some_and(|r| r.notified)).await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        let rec = &state.subagents.node(sub).unwrap();
        assert_eq!(
            rec.status,
            crate::sessions::subagents::SubAgentStatus::Failed
        );
        assert!(rec.error.as_deref().unwrap().contains("bad key"));
        // Polled, not read once. `SubAgentNotified` is journaled as soon as the
        // parent's *mailbox* accepts the report — it is a `tell` — so the flag
        // means "handed over", not "recorded". The parent appends to its own
        // history a scheduling hop later, and reading immediately races that.
        let mut texts = Vec::new();
        for _ in 0..300 {
            texts = subagent_texts(&main_history(&session).await);
            if texts
                .iter()
                .any(|t| t.contains("[subagent \"risky\" failed]"))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            texts
                .iter()
                .any(|t| t.contains("[subagent \"risky\" failed]")),
            "the parent must hear the failure: {texts:?}"
        );
    }

    #[tokio::test]
    async fn a_stranded_grandchild_result_flushes_at_the_next_turn_boundary() {
        use crate::sessions::subagents::{SubAgentParent, SubAgentStatus};
        // Fold a crashed-session state straight into the journal: P completed
        // and its parent was told; P's child C died mid-run and was reconciled
        // to failed. Every node is terminal, so no subagent outcome will ever
        // arrive again — C's result is owed to P forever unless a turn
        // boundary delivers it.
        let p = Uuid::new_v4();
        let c = Uuid::new_v4();
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let events = [
            SessionDomainEvent::SubAgentSpawned {
                at_ms: 0,
                id: p,
                parent: SubAgentParent::Main,
                label: "parent".into(),
                task: "parent task".into(),
                depth: 1,
                agent_type: None,
            },
            SessionDomainEvent::SubAgentCompleted {
                at_ms: 0,
                id: p,
                output: "parent first answer".into(),
            },
            SessionDomainEvent::SubAgentNotified { at_ms: 0, id: p },
            SessionDomainEvent::SubAgentSpawned {
                at_ms: 0,
                id: c,
                parent: SubAgentParent::SubAgent(p),
                label: "child".into(),
                task: "child task".into(),
                depth: 2,
                agent_type: None,
            },
            SessionDomainEvent::SubAgentFailed {
                at_ms: 0,
                id: c,
                error: crate::sessions::subagents::INTERRUPTED_ERROR.into(),
            },
        ];

        // Loading must start no runs: C stays owed until someone acts.
        let session2 = seed_session(&_f, id, actor_spec_fixture(), &events).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(!&state.subagents.node(c).unwrap().notified);
        assert_eq!(
            state.subagents.node(p).unwrap().status,
            SubAgentStatus::Completed
        );

        // The next turn boundary wakes P with C's failure; P concludes again
        // and its new output is owed to the main agent.
        session2
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: None,
                    text: "hi".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap();
        // P's re-completion and its notification to the main agent persist in
        // one effect, so don't wait on a `!notified` window — C delivered and
        // P re-concluded are the durable facts.
        wait_for_tree(&journal, id, |t| {
            t.node(c).is_some_and(|r| r.notified)
                && t.node(p).is_some_and(|r| {
                    r.status == SubAgentStatus::Completed
                        && r.output.as_deref() == Some("sub answer")
                })
        })
        .await;
        let page = session2
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::PageLog {
                    agent_id: Some(p.to_string()),
                    before: None,
                    max: 20,
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("P's transcript");
        let texts = subagent_texts(&page);
        assert!(
            texts
                .iter()
                .any(|t| t.contains("[subagent \"child\" failed]")
                    && t.contains("interrupted by restart")),
            "P must be woken with C's result: {texts:?}"
        );
        let _ = session;
    }

    #[tokio::test]
    async fn recovery_respawns_subagents_and_fails_interrupted_ones() {
        // First incarnation: a hanging provider keeps the subagent mid-run.
        let gate = BlockingProvider::new();
        let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
        let sub = spawn_sub(&session, "w", "t").await;
        wait_for_tree(&journal, id, |t| {
            t.node(sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
        })
        .await;
        // Simulate process death: the last ref drops, the journal lives on.
        drop(session);

        // Second incarnation on the same journal.
        f.node.restart().await;
        let session2 = f.start(id, actor_spec_fixture()).await;
        wait_for_tree(&journal, id, |t| {
            t.node(sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Failed)
        })
        .await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.subagents.node(sub).unwrap().error.as_deref(),
            Some(crate::sessions::subagents::INTERRUPTED_ERROR)
        );
        // The transcript stays pageable: the resident actor answers history.
        let page = session2
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::PageLog {
                    agent_id: Some(sub.to_string()),
                    before: None,
                    max: 10,
                    reply,
                })
            })
            .await
            .unwrap();
        assert!(page.is_some(), "a reloaded subagent must answer history");
        gate.release();
    }

    /// The defect this change exists to close. A subagent spawned by a workflow
    /// step used to have its completion dropped — `on_sub_agent_outcome` looked the
    /// node up in the conversation's tree, which a run does not have.
    #[tokio::test]
    async fn a_workflow_steps_subagent_completion_is_recorded() {
        let (_f, session, id, journal) = a_run_with_a_step_in_flight().await;
        let sub = spawn_sub(&session, "helper", "dig").await;

        // The spawn lands in the step's tree, not the conversation's.
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        let step_agent = state.run.as_ref().unwrap().steps[0].agent;
        assert_eq!(
            state.subagents.owner_of(sub),
            Some(TreeOwner::Step(step_agent)),
            "a step's spawn belongs to that step's tree"
        );

        // And its completion is journaled rather than dropped.
        wait_for_tree(&journal, id, |forest| {
            forest
                .node(sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Completed)
        })
        .await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.subagents.node(sub).unwrap().output.as_deref(),
            Some("sub answer")
        );
    }

    /// The aggregates a run used to answer as though it had no subagents at all.
    #[tokio::test]
    async fn a_runs_subagents_count_toward_the_session_wide_aggregates() {
        // Blocks every call, so both the step and its subagent stay `Running` for
        // as long as this test looks at them.
        let provider = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_run_with_provider(provider).await;
        wait_for_run(&journal, id, |r| r.current().is_some()).await;
        let sub = spawn_sub(&session, "slow", "work").await;
        wait_for_tree(&journal, id, |f| f.node(sub).is_some()).await;

        // While it runs, the session is busy. This is what stops the supervisor
        // unloading a run out from under a step's subagent — `has_active` answered
        // false for every run before the forest.
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(
            state.subagents.has_active(),
            "a run's subagent is active work"
        );
        assert_eq!(state.subagents.active_count(), 1);
        assert_eq!(state.subagents.interrupted(), vec![sub]);

        // And the API reports it: the roster spans every tree, so a run's step
        // agents and the subagents beneath them arrive in one list.
        let snapshot = session
            .ask(|reply| SessionCommand::Read(ReadCommand::Snapshot { reply }))
            .await
            .unwrap();
        let ids: Vec<&str> = snapshot.agents.iter().map(|a| a.id.as_str()).collect();
        assert!(
            ids.contains(&sub.to_string().as_str()),
            "a run's subagents must reach the API: {ids:?}"
        );
    }

    /// A nested subagent's result reaches its parent inside a run. Delivery used to
    /// live only in `InteractiveOrchestrator`, so it never ran for a workflow;
    /// `wake_owed_parents` now reads the forest and the run driver calls it.
    #[tokio::test]
    async fn a_nested_subagents_result_wakes_its_parent_inside_a_run() {
        let (_f, session, id, journal) = a_run_with_a_step_in_flight().await;
        let parent = spawn_sub(&session, "lead", "delegate").await;
        wait_for_tree(&journal, id, |f| {
            f.node(parent)
                .is_some_and(|r| r.status != crate::sessions::subagents::SubAgentStatus::Running)
        })
        .await;

        let child = session
            .ask(|reply| {
                SessionCommand::SubAgent(SubAgentCommand::Spawn {
                    caller: crate::sessions::subagents::SubAgentParent::SubAgent(parent),
                    agent: crate::sessions::runners::ids::AgentId::new_v4(),
                    label: "helper".into(),
                    task: "dig".into(),
                    agent_type: None,
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap();

        // The child's result is delivered to its parent — `notified` flips only
        // when the parent has actually been resumed with it.
        wait_for_tree(&journal, id, |f| f.node(child).is_some_and(|r| r.notified)).await;
    }

    /// The new shape round-trips.
    #[test]
    fn the_new_state_shape_round_trips() {
        let mut state = SessionState::default();
        let id = Uuid::new_v4();
        state.subagents.tree_mut(TreeOwner::Main).apply_spawned(
            id,
            SubAgentParent::Main,
            "x".into(),
            "t".into(),
            1,
            100,
            None,
        );
        let json = serde_json::to_value(&state).unwrap();
        let back: SessionState = serde_json::from_value(json).unwrap();
        assert_eq!(back.subagents.node(id).unwrap().label, "x");
    }
}
