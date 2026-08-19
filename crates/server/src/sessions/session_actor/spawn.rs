//! Delegated work: spawning subagents, their status tool, and crash repair.
//!
//! Persist-then-spawn throughout: the `Created` event is durable before the
//! child actor exists, so a crash between the two replays as a running node
//! that recovery reconciles to failed — never an untracked agent.

use super::runner::event::{RecordedEnd, RunnerArgs, RunnerEvent};
use super::runner::state::{RunnerState, SubPhase};
use super::runner::{INTERRUPTED_ERROR, MAX_RUNNER_DEPTH, Runner, RunnerBehavior, deliver};
use super::{
    AgentId, CommandEffect, RunnerId, SessionActor, SessionCommand, SessionEvent, SessionState,
    SubAgentCommand,
};
use crate::agent_loop::{AgentCommand, Incoming};
use crate::sessions::addressing::SessionInbox;
use horsie_actor::{ActorContext, ReplyTo};
use horsie_models::now_ms;
use tokio::sync::oneshot;
use uuid::Uuid;

impl SessionActor {
    pub(super) async fn handle_sub_agent(
        &mut self,
        state: &SessionState,
        cmd: SubAgentCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd {
            SubAgentCommand::Spawn {
                caller,
                label,
                task,
                agent_type,
                reply,
            } => {
                // The caller's own runner is what the spawn hangs off: its
                // depth bounds the tree, and its settings carry the cap and
                // what the child inherits.
                let Some(owner) = state.owner_of(caller) else {
                    let _ = reply.send(Err("caller is not a known agent".to_string()));
                    return CommandEffect::none();
                };
                let depth = state.depth_of(owner);
                if depth >= MAX_RUNNER_DEPTH {
                    return refuse(
                        reply,
                        format!("max subagent depth {MAX_RUNNER_DEPTH} reached"),
                    );
                }
                // The cap is the *caller's* settings' cap: a workflow step's
                // spawns are counted against the step's preset, never against
                // a session-wide value that nothing in a run owns.
                let settings = Runner::of(owner, state)
                    .and_then(|runner| runner.role(self.spec(), state, caller))
                    .map(|role| role.settings);
                let Some(settings) = settings else {
                    let _ = reply.send(Err("caller is not a known agent".to_string()));
                    return CommandEffect::none();
                };
                let max = settings.max_subagents();
                if active_subs(state) >= max {
                    return refuse(reply, format!("{max} subagents already active"));
                }
                // Persist first, spawn second — see the module doc. The
                // caller's effective settings are snapshotted into the child's
                // own record, so a cold node needs no resolution walk to wake.
                let id = AgentId(Uuid::new_v4());
                let created = SessionEvent::Runner {
                    id: RunnerId::of_agent(id),
                    at_ms: now_ms(),
                    event: RunnerEvent::Created {
                        parent: Some(caller),
                        args: Box::new(RunnerArgs::Sub {
                            label,
                            task: task.clone(),
                            agent_type,
                            settings,
                        }),
                    },
                };
                let (tx, rx) = oneshot::channel();
                let self_ref = self.me(ctx);
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
                            reply,
                            persisted,
                        }))
                        .await;
                });
                CommandEffect::persist(vec![created]).and_ack(ReplyTo::from_sender(tx))
            }
            SubAgentCommand::FinishSpawn {
                id,
                task,
                reply,
                persisted,
            } => {
                if let Err(e) = persisted {
                    let _ = reply.send(Err(format!("persist subagent: {e}")));
                    return CommandEffect::none();
                }
                let Some(agent) = self.reach(id, state, ctx) else {
                    let _ = reply.send(Err("could not start the subagent".to_string()));
                    return CommandEffect::none();
                };
                // The task is the first thing in this agent's queue, which it
                // drains at once. Queued rather than run directly so a
                // subagent has one way in, whatever is addressed to it.
                let _ = agent
                    .tell(AgentCommand::Enqueue {
                        item: Incoming::User {
                            id: format!("task:{id}"),
                            text: task,
                        },
                        ack: None,
                    })
                    .await;
                let _ = reply.send(Ok(id.0));
                CommandEffect::none()
            }
            SubAgentCommand::Status { caller, id, reply } => {
                let _ = reply.send(render_status(state, caller, id));
                CommandEffect::none()
            }
            SubAgentCommand::Reconcile => {
                let interrupted: Vec<AgentId> = state
                    .runners
                    .iter()
                    .filter_map(|(id, record)| match &record.state {
                        RunnerState::Sub(s) if matches!(s.phase, SubPhase::Running { .. }) => {
                            Some(AgentId(id.0))
                        }
                        RunnerState::Sub(_)
                        | RunnerState::Main(_)
                        | RunnerState::Fork(_)
                        | RunnerState::Workflow(_) => None,
                    })
                    .collect();
                if interrupted.is_empty() {
                    return CommandEffect::none();
                }
                let events = interrupted
                    .into_iter()
                    .map(|agent| SessionEvent::TurnEnded {
                        at_ms: now_ms(),
                        agent,
                        end: RecordedEnd::Failed {
                            error: INTERRUPTED_ERROR.to_string(),
                        },
                    })
                    .collect();
                // Through the boundary, so the parents owed these failures
                // hear them now rather than at the next thing a person does.
                self.persist_and_advance(state, events, ctx).await
            }
        }
    }
}

fn refuse(reply: ReplyTo<Result<Uuid, String>>, why: String) -> CommandEffect<SessionEvent> {
    let _ = reply.send(Err(why));
    CommandEffect::none()
}

/// Subagents mid-run anywhere in the session — the concurrency limit's
/// measure.
fn active_subs(state: &SessionState) -> u32 {
    state
        .runners
        .values()
        .filter(|record| {
            matches!(
                &record.state,
                RunnerState::Sub(s) if matches!(s.phase, SubPhase::Running { .. })
            )
        })
        .count() as u32
}

/// Whether `id` is `caller` itself or sits somewhere under it — the
/// `subagent_status` visibility rule: an agent sees itself and its own
/// descendants, never siblings. Out-of-subtree and unknown ids are
/// indistinguishable, so neither confirms a node exists.
fn descends_from(state: &SessionState, id: AgentId, caller: AgentId) -> bool {
    let mut current = id;
    let limit = state.runners.len() + 1;
    for _ in 0..limit {
        if current == caller {
            return true;
        }
        match state
            .record(RunnerId::of_agent(current))
            .and_then(|r| r.parent)
        {
            Some(parent) => current = parent,
            None => return false,
        }
    }
    false
}

/// One node's detail, or the caller's subtree, for the `subagent_status` tool.
fn render_status(
    state: &SessionState,
    caller: AgentId,
    id: Option<Uuid>,
) -> Result<String, String> {
    match id {
        Some(raw) => {
            let id = AgentId(raw);
            let visible = descends_from(state, id, caller);
            let node = visible
                .then(
                    || match state.record(RunnerId::of_agent(id)).map(|r| &r.state) {
                        Some(RunnerState::Sub(s)) => Some(s),
                        _ => None,
                    },
                )
                .flatten();
            let Some(node) = node else {
                return Err(format!("no such subagent: {raw}"));
            };
            let (status, output, error) = match &node.phase {
                SubPhase::Running { .. } => ("running", None, None),
                SubPhase::Done { result: Ok(o), .. } => ("completed", Some(o.as_str()), None),
                SubPhase::Done { result: Err(e), .. } => ("failed", None, Some(e.as_str())),
            };
            let depth = state.depth_of(RunnerId::of_agent(id));
            let mut out = format!(
                "subagent \"{}\" ({raw}) — {status}, depth {depth}",
                node.label
            );
            if let Some(output) = output {
                out.push_str(&format!(
                    "\n\noutput:\n{}",
                    deliver::truncate_result(output)
                ));
            }
            if let Some(error) = error {
                out.push_str(&format!("\n\nerror:\n{}", deliver::truncate_result(error)));
            }
            Ok(out)
        }
        None => {
            let base = state.depth_of(RunnerId::of_agent(caller));
            let mut out = String::new();
            for (id, record) in &state.runners {
                let RunnerState::Sub(node) = &record.state else {
                    continue;
                };
                let agent = AgentId(id.0);
                // The caller's descendants — the caller itself is not its own
                // subagent.
                if agent == caller || !descends_from(state, agent, caller) {
                    continue;
                }
                let status = match &node.phase {
                    SubPhase::Running { .. } => "running",
                    SubPhase::Done { result: Ok(_), .. } => "completed",
                    SubPhase::Done { result: Err(_), .. } => "failed",
                };
                let depth = state.depth_of(*id);
                let indent = "  ".repeat(depth.saturating_sub(base).saturating_sub(1) as usize);
                out.push_str(&format!(
                    "{indent}- \"{}\" ({}) [{status}]\n",
                    node.label, id.0
                ));
            }
            if out.is_empty() {
                out.push_str("No subagents.\n");
            }
            Ok(out)
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
    //! Delegated work through the actor: what a spawn records, what an outcome
    //! delivers, and what recovery repairs.
    use super::super::testing::*;
    use super::super::*;
    use crate::sessions::session_actor::testing::seed_session;

    use std::sync::Arc;
    use uuid::Uuid;

    /// A session whose only repair is an interrupted subagent still tells its
    /// supervisor what it recovered as.
    #[tokio::test]
    async fn a_subagent_only_repair_still_reports_a_status() {
        // The provider hangs, so the subagent is genuinely still running when
        // the session goes away — which is what a fold reads as interrupted,
        // and what the sub runner's repair reconciles.
        let gate = BlockingProvider::new();
        let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
        let sub = spawn_sub(&session, id, "worker", "dig").await;
        wait_for_state(&journal, id, "the subagent to be running", |s| {
            interrupted_subs(s).contains(&sub)
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

    #[tokio::test]
    async fn spawn_records_a_running_subagent_in_the_tree() {
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        let sub = spawn_sub(&session, id, "research", "dig into it").await;
        wait_for_tree(&journal, id, |s| sub_running(s, sub)).await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        let node = sub_of(&state, sub).unwrap();
        assert_eq!(state.depth_of(RunnerId(sub)), 1);
        assert_eq!(
            state.record(RunnerId(sub)).unwrap().parent,
            Some(AgentId(id)),
            "a top-level spawn hangs off the main agent"
        );
        assert_eq!(node.label, "research");
        assert_eq!(node.task, "dig into it");
    }

    #[tokio::test]
    async fn spawn_beyond_depth_four_is_rejected() {
        // A hanging provider keeps every spawned node running, so the chain
        // builds deterministically: main → d1 → d2 → d3 → d4, and d4's spawn
        // is refused.
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        let mut parent = AgentId(id);
        for _ in 0..4 {
            let child = session
                .ask(|reply| {
                    SessionCommand::SubAgent(SubAgentCommand::Spawn {
                        caller: parent,
                        label: "w".into(),
                        task: "t".into(),
                        agent_type: None,
                        reply,
                    })
                })
                .await
                .unwrap()
                .unwrap();
            wait_for_tree(&journal, id, any_sub_active).await;
            parent = AgentId(child);
        }
        let res = session
            .ask(|reply| {
                SessionCommand::SubAgent(SubAgentCommand::Spawn {
                    caller: parent,
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
            let _ = spawn_sub(&session, id, "w", "t").await;
        }
        wait_for_tree(&journal, id, |s| interrupted_subs(s).len() == 8).await;
        let res = session
            .ask(|reply| {
                SessionCommand::SubAgent(SubAgentCommand::Spawn {
                    caller: AgentId(id),
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
                    caller: AgentId(Uuid::new_v4()),
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
        let sub = spawn_sub(&session, id, "research", "dig").await;
        // Owed, then delivered: the runner's notified flag flips exactly once.
        wait_for_tree(&journal, id, |s| sub_notified(s, sub)).await;
        // …and then wait for the main agent to have *taken* it. The flag says
        // the result was handed over — one message into main's mailbox — never
        // that main has appended it.
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
        // message.
        assert!(
            user_texts(&main_history(&session).await)
                .iter()
                .all(|t| !t.contains("[subagent ")),
            "a result must never land in the user text"
        );
    }

    /// The child's own log, not the parent's card: a subagent page folds the
    /// same `TurnBegan`/`TurnEnded` pair every other agent's does.
    #[tokio::test]
    async fn a_completed_subagent_closes_the_turn_in_its_own_log() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, id, "research", "dig").await;
        wait_for_tree(&journal, id, |s| sub_notified(s, sub)).await;

        let outcomes = turn_outcomes(&agent_history(&session, Some(sub.to_string())).await);
        assert!(
            matches!(
                outcomes.as_slice(),
                [horsie_agentcore::TurnOutcome::Ended(_)]
            ),
            "a subagent's one turn ends in its own log: {outcomes:?}"
        );
    }

    /// Stop, addressed to a subagent: the child is cancelled *and* the parent
    /// is told, because the parent is blocked on a `spawn_agent` result.
    #[tokio::test]
    async fn stopping_a_subagent_cancels_it_and_tells_the_parent() {
        let provider = BlockingProvider::new();
        let (_f, session, id, journal) =
            spawn_session_with_provider(provider.clone() as Arc<dyn horsie_agentcore::LlmProvider>)
                .await;
        let sub = spawn_sub(&session, id, "research", "dig").await;
        wait_for_tree(&journal, id, |s| sub_running(s, sub)).await;

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
        wait_for_tree(&journal, id, |s| sub_notified(s, sub)).await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(matches!(sub_result(&state, sub), Some(Err(_))));
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
        let sub = spawn_sub(&session, id, "risky", "doomed task").await;
        wait_for_tree(&journal, id, |s| sub_notified(s, sub)).await;

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
        let sub = spawn_sub(&session, id, "risky", "doomed task").await;
        wait_for_tree(&journal, id, |s| sub_notified(s, sub)).await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        match sub_result(&state, sub) {
            Some(Err(error)) => assert!(error.contains("bad key"), "{error}"),
            other => panic!("expected a failure, got {other:?}"),
        }
        // Polled, not read once: the flag means "handed over", and the parent
        // appends to its own history a scheduling hop later.
        let texts = wait_for_subagent_text(&session, |texts| {
            texts
                .iter()
                .any(|t| t.contains("[subagent \"risky\" failed]"))
        })
        .await;
        assert!(
            texts
                .iter()
                .any(|t| t.contains("[subagent \"risky\" failed]")),
            "the parent must hear the failure: {texts:?}"
        );
    }

    #[tokio::test]
    async fn a_stranded_grandchild_result_flushes_at_the_next_turn_boundary() {
        // Fold a crashed-session state straight into the journal: P completed
        // and its parent was told; P's child C died mid-run and was reconciled
        // to failed. Every node is terminal, so no subagent outcome will ever
        // arrive again — C's result is owed to P forever unless a turn
        // boundary delivers it.
        let p = Uuid::new_v4();
        let c = Uuid::new_v4();
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub_args = |label: &str, task: &str| {
            Box::new(RunnerArgs::Sub {
                label: label.into(),
                task: task.into(),
                agent_type: None,
                settings: agent_settings_fixture(),
            })
        };
        let events = [
            SessionEvent::Runner {
                id: RunnerId(p),
                at_ms: 0,
                event: RunnerEvent::Created {
                    parent: Some(AgentId(id)),
                    args: sub_args("parent", "parent task"),
                },
            },
            SessionEvent::TurnEnded {
                at_ms: 1,
                agent: AgentId(p),
                end: RecordedEnd::Concluded {
                    output: "parent first answer".into(),
                },
            },
            SessionEvent::Runner {
                id: RunnerId(p),
                at_ms: 2,
                event: RunnerEvent::Reported,
            },
            SessionEvent::Runner {
                id: RunnerId(c),
                at_ms: 3,
                event: RunnerEvent::Created {
                    parent: Some(AgentId(p)),
                    args: sub_args("child", "child task"),
                },
            },
            SessionEvent::TurnEnded {
                at_ms: 4,
                agent: AgentId(c),
                end: RecordedEnd::Failed {
                    error: runner::INTERRUPTED_ERROR.into(),
                },
            },
        ];

        // Loading must start no runs: C stays owed until someone acts.
        let session2 = seed_session(&_f, id, actor_spec_fixture(), &events).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(!sub_notified(&state, c));
        assert!(matches!(sub_result(&state, p), Some(Ok(_))));

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
        wait_for_tree(&journal, id, |s| {
            sub_notified(s, c) && sub_result(s, p) == Some(Ok("sub answer".into()))
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
        let sub = spawn_sub(&session, id, "w", "t").await;
        wait_for_tree(&journal, id, |s| sub_running(s, sub)).await;
        // Simulate process death: the last ref drops, the journal lives on.
        drop(session);

        // Second incarnation on the same journal.
        f.node.restart().await;
        let session2 = f.start(id, actor_spec_fixture()).await;
        wait_for_tree(&journal, id, |s| matches!(sub_result(s, sub), Some(Err(_)))).await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            sub_result(&state, sub),
            Some(Err(runner::INTERRUPTED_ERROR.to_string()))
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

    /// A subagent spawned by a workflow step hangs off that step's agent, and
    /// its completion is journaled rather than dropped.
    #[tokio::test]
    async fn a_workflow_steps_subagent_completion_is_recorded() {
        let (_f, session, id, journal) = a_run_with_a_step_in_flight().await;
        let step_agent = current_step_agent(&journal, id).await;
        let sub = spawn_sub(&session, step_agent, "helper", "dig").await;

        // The spawn hangs off the step's agent, not a conversation's.
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.record(RunnerId(sub)).unwrap().parent,
            Some(AgentId(step_agent)),
            "a step's spawn belongs to that step"
        );

        wait_for_tree(&journal, id, |s| {
            sub_result(s, sub) == Some(Ok("sub answer".into()))
        })
        .await;
    }

    /// The aggregates a run used to answer as though it had no subagents at all.
    #[tokio::test]
    async fn a_runs_subagents_count_toward_the_session_wide_aggregates() {
        let provider = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_run_with_provider(provider).await;
        wait_for_run(&journal, id, |r| r.current().is_some()).await;
        let step_agent = current_step_agent(&journal, id).await;
        let sub = spawn_sub(&session, step_agent, "slow", "work").await;
        wait_for_tree(&journal, id, |s| sub_of(s, sub).is_some()).await;

        // While it runs, the session is busy. This is what stops the
        // supervisor unloading a run out from under a step's subagent.
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(any_sub_active(&state), "a run's subagent is active work");
        assert!(runner::session_busy(&state));
        assert_eq!(interrupted_subs(&state), vec![sub]);

        // And the API reports it: the roster spans every runner, so a run's
        // step agents and the subagents beneath them arrive in one list.
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

    /// A nested subagent's result reaches its parent inside a run.
    #[tokio::test]
    async fn a_nested_subagents_result_wakes_its_parent_inside_a_run() {
        let (_f, session, id, journal) = a_run_with_a_step_in_flight().await;
        let step_agent = current_step_agent(&journal, id).await;
        let parent = spawn_sub(&session, step_agent, "lead", "delegate").await;
        wait_for_tree(&journal, id, |s| !sub_running(s, parent)).await;

        let child = session
            .ask(|reply| {
                SessionCommand::SubAgent(SubAgentCommand::Spawn {
                    caller: AgentId(parent),
                    label: "helper".into(),
                    task: "dig".into(),
                    agent_type: None,
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap();

        // The child's result is delivered to its parent — `notified` flips
        // only when the parent has actually been resumed with it.
        wait_for_tree(&journal, id, |s| sub_notified(s, child)).await;
    }

    /// The new state shape round-trips through serde, records and all.
    #[test]
    fn the_new_state_shape_round_trips() {
        let main = Uuid::new_v4();
        let sub = Uuid::new_v4();
        let state = fold(vec![
            SessionEvent::Runner {
                id: RunnerId(main),
                at_ms: 0,
                event: RunnerEvent::Created {
                    parent: None,
                    args: Box::new(RunnerArgs::Main),
                },
            },
            SessionEvent::Runner {
                id: RunnerId(sub),
                at_ms: 1,
                event: RunnerEvent::Created {
                    parent: Some(AgentId(main)),
                    args: Box::new(RunnerArgs::Sub {
                        label: "x".into(),
                        task: "t".into(),
                        agent_type: None,
                        settings: agent_settings_fixture(),
                    }),
                },
            },
        ]);
        let json = serde_json::to_value(&state).unwrap();
        let back: SessionState = serde_json::from_value(json).unwrap();
        assert_eq!(sub_of(&back, sub).unwrap().label, "x");
        assert_eq!(back, state);
    }
}
