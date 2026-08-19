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
