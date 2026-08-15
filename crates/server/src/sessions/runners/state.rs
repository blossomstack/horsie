//! What a session knows: which runners exist, how they nest, and the totals.
//!
//! Nothing here belongs to one agent. A runner's own slice lives in
//! [`RunnerRecord::state`] and the session never looks inside it — it hands it
//! back to the runner that owns it. Per-agent usage lives there too; what the
//! session keeps is an aggregate by model, because a session-wide total that
//! had to be summed from a per-agent map was a per-agent fact wearing a
//! session-shaped name.

use super::ids::{AgentId, RunnerId, RunnerKind, RunnerStatus};
use super::{RunnerEvent, RunnerState};
use crate::agent_loop::UsageTotal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One runner, as the session records it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerRecord {
    pub kind: RunnerKind,
    /// The agent that created me. `None` for the root and for the runtime.
    ///
    /// Provenance, not debt: whether a runner reports is decided by its kind,
    /// which is what lets a fork have a parent and owe it nothing.
    pub parent: Option<AgentId>,
    pub status: RunnerStatus,
    /// My slice, opaque to the session.
    pub state: RunnerState,
    pub created_at_ms: u64,
    /// Zero while this runner is still going.
    pub ended_at_ms: u64,
}

/// Purely structure and aggregates.
///
/// `#[serde(default)]` on the container because this is snapshotted, so it is
/// a durability contract: add optional fields, never rename or repurpose one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    /// What this session is. The session's journal is the truth about it, so
    /// the spec lives here and not only in the supervisor's list.
    pub spec: Option<crate::sessions::spec::SessionSpec>,
    /// The conversation or the run this session *is*. Its status is the
    /// session's status; everything else hangs off an agent inside it.
    pub root: RunnerId,
    /// Which runner owns each agent. Structure, not content — what an agent
    /// said lives in the agent's own journal.
    ///
    /// This one map replaces the three-registry probe that used to answer
    /// "what kind of agent is this id", and with it the ordering hazard.
    pub agents: BTreeMap<AgentId, RunnerId>,
    /// Tokens by model across everything this session has run.
    pub usage: BTreeMap<String, UsageTotal>,
    pub runners: BTreeMap<RunnerId, RunnerRecord>,
}

impl SessionState {
    /// The session's status: the root runner's, never an aggregate over all of
    /// them. A subagent working in the background is not the session working.
    #[must_use]
    pub fn status(&self) -> RunnerStatus {
        self.runners
            .get(&self.root)
            .map_or(RunnerStatus::Pending, |r| r.status)
    }

    #[must_use]
    pub fn runner_of(&self, agent: AgentId) -> Option<RunnerId> {
        self.agents.get(&agent).copied()
    }

    #[must_use]
    pub fn record(&self, runner: RunnerId) -> Option<&RunnerRecord> {
        self.runners.get(&runner)
    }

    /// The runners this agent created.
    ///
    /// The only direction `parent` is read in bulk — a single child's owner is
    /// a field lookup — and it is what a cancel cascades over.
    #[must_use]
    pub fn children_of(&self, agent: AgentId) -> Vec<RunnerId> {
        self.runners
            .iter()
            .filter(|(_, r)| r.parent == Some(agent))
            .map(|(id, _)| *id)
            .collect()
    }

    /// How deep this runner sits: the walk that replaces the subagent forest's
    /// per-tree depth bookkeeping, and the one budget a nested workflow needs.
    ///
    /// `parent` is written once at creation and never edited, so the chain
    /// cannot contain a cycle; the guard below is against a corrupted log
    /// rather than against a shape this code can produce.
    #[must_use]
    pub fn depth_of(&self, runner: RunnerId) -> u32 {
        let mut depth = 0;
        let mut at = runner;
        for _ in 0..self.runners.len() {
            let Some(parent) = self.runners.get(&at).and_then(|r| r.parent) else {
                return depth;
            };
            let Some(next) = self.runner_of(parent) else {
                return depth;
            };
            depth += 1;
            at = next;
        }
        depth
    }

    /// Bank tokens against the model that spent them.
    pub fn bank(&mut self, model: String, spent: &UsageTotal) {
        let entry = self.usage.entry(model).or_default();
        *entry = entry.combine(spent);
    }

    /// Every agent this session hosts, with the runner that owns it. The read
    /// side's one entry point, so no reader re-derives ownership.
    pub fn all_agents(&self) -> impl Iterator<Item = (AgentId, RunnerId)> + '_ {
        self.agents.iter().map(|(a, r)| (*a, *r))
    }
}

/// What a session records.
///
/// Five variants and no more: everything else a session used to journal was a
/// fact about one runner, and lives in that runner's own slice now. The old
/// vocabulary had thirty-odd variants because a fork's turn boundary, a step's
/// conclusion and a subagent's report each needed their own, and each had to be
/// routed to the one component that understood it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    /// What this session is. The first thing a session journals.
    SpecRecorded {
        spec: Box<crate::sessions::spec::SessionSpec>,
    },
    /// A runner came into being. Persisted *before* its agents exist, so a
    /// crash between the two replays as a runner with nothing started — which
    /// `Runner::actions` restarts — rather than as an agent nothing tracks.
    RunnerCreated {
        id: RunnerId,
        kind: RunnerKind,
        parent: Option<AgentId>,
        state: Box<RunnerState>,
        at_ms: u64,
    },
    /// A runner reached its end.
    RunnerEnded {
        id: RunnerId,
        status: RunnerStatus,
        at_ms: u64,
    },
    /// A runner started an agent, so the session can route to it. The one
    /// place `agents` grows.
    AgentStarted { runner: RunnerId, agent: AgentId },
    /// Something a runner recorded about itself, or one of its capabilities
    /// about itself. Routed by id and never inspected.
    ///
    /// `at_ms` is here rather than inside the runner's own event because it is
    /// a fact about the journal entry, not about what was decided: a decision
    /// is made once and folded any number of times, so a time read inside the
    /// fold would differ on every replay. Every runner that stamps a time —
    /// a step's start, a conversation's last activity — reads this one.
    Runner {
        id: RunnerId,
        event: Box<RunnerEvent>,
        at_ms: u64,
    },
    /// Tokens spent, banked against the model that spent them.
    UsageBanked { model: String, spent: UsageTotal },
}

impl SessionState {
    /// Fold one event.
    ///
    /// Pure, and the only writer. An event addressed to a runner this state
    /// does not know is ignored rather than a panic: a fold has to survive a
    /// log it does not fully understand, or one bad row takes the session with
    /// it.
    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::SpecRecorded { spec } => self.spec = Some((**spec).clone()),
            SessionEvent::RunnerCreated {
                id,
                kind,
                parent,
                state,
                at_ms,
            } => {
                // The first runner is the root: a session's shape is decided
                // when it is created, and the root is whichever runner the
                // spec called for.
                if self.runners.is_empty() {
                    self.root = *id;
                }
                self.runners.insert(
                    *id,
                    RunnerRecord {
                        kind: *kind,
                        parent: *parent,
                        status: RunnerStatus::Pending,
                        state: (**state).clone(),
                        created_at_ms: *at_ms,
                        ended_at_ms: 0,
                    },
                );
            }
            SessionEvent::RunnerEnded { id, status, at_ms } => {
                if let Some(rec) = self.runners.get_mut(id) {
                    rec.status = *status;
                    rec.ended_at_ms = *at_ms;
                }
            }
            SessionEvent::AgentStarted { runner, agent } => {
                if self.runners.contains_key(runner) {
                    self.agents.insert(*agent, *runner);
                    if let Some(rec) = self.runners.get_mut(runner) {
                        rec.status = RunnerStatus::Running;
                    }
                }
            }
            SessionEvent::Runner { id, event, at_ms } => {
                if let Some(rec) = self.runners.get_mut(id) {
                    super::Runner::apply(&mut rec.state, event, *at_ms);
                }
            }
            SessionEvent::UsageBanked { model, spent } => self.bank(model.clone(), spent),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
impl SessionState {
    /// Insert a bare record. Tests only — live code creates a runner by
    /// journaling `RunnerCreated`, never by reaching in here.
    pub(crate) fn insert_for_test(
        &mut self,
        kind: RunnerKind,
        parent: Option<AgentId>,
        status: RunnerStatus,
    ) -> RunnerId {
        let id = RunnerId::new_v4();
        self.runners.insert(
            id,
            RunnerRecord {
                kind,
                parent,
                status,
                state: RunnerState::empty_for(kind),
                created_at_ms: 0,
                ended_at_ms: 0,
            },
        );
        id
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn created(id: RunnerId, kind: RunnerKind, parent: Option<AgentId>) -> SessionEvent {
        SessionEvent::RunnerCreated {
            id,
            kind,
            parent,
            state: Box::new(RunnerState::empty_for(kind)),
            at_ms: 0,
        }
    }

    /// The session's status is the root runner's, not an aggregate: a
    /// background subagent must not make the session read Running.
    #[test]
    fn session_status_is_the_root_runners() {
        let mut s = SessionState::default();
        let root = s.insert_for_test(RunnerKind::Conversation, None, RunnerStatus::Pending);
        s.root = root;
        let agent = AgentId::new_v4();
        s.agents.insert(agent, root);
        let _busy = s.insert_for_test(RunnerKind::SubAgent, Some(agent), RunnerStatus::Running);
        assert_eq!(s.status(), RunnerStatus::Pending);
    }

    /// Nesting is recorded once, in `parent`. Depth is a walk up it, which is
    /// what replaces the forest's per-tree depth bookkeeping.
    #[test]
    fn depth_walks_up_the_parent_chain() {
        let mut s = SessionState::default();
        let root = s.insert_for_test(RunnerKind::Conversation, None, RunnerStatus::Running);
        s.root = root;
        let a0 = AgentId::new_v4();
        s.agents.insert(a0, root);
        let r1 = s.insert_for_test(RunnerKind::SubAgent, Some(a0), RunnerStatus::Running);
        let a1 = AgentId::new_v4();
        s.agents.insert(a1, r1);
        let r2 = s.insert_for_test(RunnerKind::SubAgent, Some(a1), RunnerStatus::Running);

        assert_eq!(s.depth_of(root), 0);
        assert_eq!(s.depth_of(r1), 1);
        assert_eq!(s.depth_of(r2), 2);
    }

    /// A workflow invoked by a subagent nests exactly as a subagent does —
    /// the depth walk does not care which kinds it passes through, which is
    /// the whole of what makes arbitrary nesting expressible.
    #[test]
    fn a_workflow_under_a_subagent_nests_like_anything_else() {
        let mut s = SessionState::default();
        let root = s.insert_for_test(RunnerKind::Conversation, None, RunnerStatus::Running);
        s.root = root;
        let main = AgentId::new_v4();
        s.agents.insert(main, root);
        let sub = s.insert_for_test(RunnerKind::SubAgent, Some(main), RunnerStatus::Running);
        let worker = AgentId::new_v4();
        s.agents.insert(worker, sub);
        let run = s.insert_for_test(RunnerKind::Workflow, Some(worker), RunnerStatus::Running);
        let step = AgentId::new_v4();
        s.agents.insert(step, run);
        let nested = s.insert_for_test(RunnerKind::SubAgent, Some(step), RunnerStatus::Running);

        assert_eq!(s.depth_of(run), 2);
        assert_eq!(s.depth_of(nested), 3);
    }

    #[test]
    fn children_of_an_agent_are_the_runners_it_parented() {
        let mut s = SessionState::default();
        let root = s.insert_for_test(RunnerKind::Conversation, None, RunnerStatus::Running);
        s.root = root;
        let a0 = AgentId::new_v4();
        s.agents.insert(a0, root);
        let r1 = s.insert_for_test(RunnerKind::SubAgent, Some(a0), RunnerStatus::Running);
        let r2 = s.insert_for_test(RunnerKind::Workflow, Some(a0), RunnerStatus::Running);
        let other = AgentId::new_v4();
        s.agents.insert(other, r1);
        let _elsewhere =
            s.insert_for_test(RunnerKind::SubAgent, Some(other), RunnerStatus::Running);

        let mut kids = s.children_of(a0);
        kids.sort();
        let mut want = vec![r1, r2];
        want.sort();
        assert_eq!(kids, want);
    }

    /// Usage is aggregated by model, not by agent: per-agent totals belong to
    /// the runner that owns the agent.
    #[test]
    fn usage_aggregates_by_model() {
        // `UsageTotal` has public counters and a `combine`; there is no
        // constructor helper, so build one here rather than adding a
        // production API only tests want.
        fn spent(input: u64, output: u64) -> UsageTotal {
            UsageTotal {
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            }
        }
        let mut s = SessionState::default();
        s.bank("sonnet".into(), &spent(10, 5));
        s.bank("sonnet".into(), &spent(1, 1));
        s.bank("opus".into(), &spent(2, 2));
        assert_eq!(s.usage.len(), 2);
        assert_eq!(s.usage["sonnet"].input_tokens, 11);
        assert_eq!(s.usage["opus"].output_tokens, 2);
    }

    /// Creation registers no agent: the runner's own first `StartAgent` does
    /// that. A crash between the two therefore replays as a runner with
    /// nothing started, which `actions()` restarts — the alternative, an agent
    /// nothing tracks, is the one this ordering exists to prevent.
    #[test]
    fn creating_a_runner_registers_no_agent() {
        let mut s = SessionState::default();
        let id = RunnerId::new_v4();
        s.apply(&created(id, RunnerKind::Conversation, None));
        assert_eq!(s.runners[&id].status, RunnerStatus::Pending);
        assert!(s.agents.is_empty());
        assert_eq!(s.root, id, "the first runner is the root");
    }

    /// The one place `agents` grows, and it moves the runner to Running.
    #[test]
    fn a_started_agent_is_routable_and_moves_the_runner() {
        let mut s = SessionState::default();
        let id = RunnerId::new_v4();
        let agent = AgentId::new_v4();
        s.apply(&created(id, RunnerKind::SubAgent, None));
        s.apply(&SessionEvent::AgentStarted { runner: id, agent });
        assert_eq!(s.runner_of(agent), Some(id));
        assert_eq!(s.runners[&id].status, RunnerStatus::Running);
    }

    /// A fold must survive a log it does not fully understand: one row naming
    /// a runner this state never saw must not take the whole session down.
    #[test]
    fn an_event_for_an_unknown_runner_is_ignored() {
        let mut s = SessionState::default();
        s.apply(&SessionEvent::AgentStarted {
            runner: RunnerId::new_v4(),
            agent: AgentId::new_v4(),
        });
        s.apply(&SessionEvent::RunnerEnded {
            id: RunnerId::new_v4(),
            status: RunnerStatus::Done,
            at_ms: 1,
        });
        assert!(s.agents.is_empty());
        assert!(s.runners.is_empty());
    }

    /// The recovery contract, and the reason nothing in a fold may reach for a
    /// clock or mint an id: replaying the same log twice has to land the same
    /// state, byte for byte, or a recovered session and a live one diverge.
    ///
    /// The `StepStarted` is the part that earns its place. A step's
    /// `started_at_ms` is a time a fold has to write, and the only honest way
    /// to write one is to read it off the entry — so the moment anybody reaches
    /// for a clock inside an `apply`, the two folds differ and this fails.
    #[test]
    fn folding_the_same_log_twice_lands_the_same_state() {
        let root = RunnerId::new_v4();
        let main = AgentId::new_v4();
        let child = RunnerId::new_v4();
        let run = RunnerId::new_v4();
        let log = vec![
            created(root, RunnerKind::Conversation, None),
            SessionEvent::AgentStarted {
                runner: root,
                agent: main,
            },
            created(child, RunnerKind::SubAgent, Some(main)),
            created(run, RunnerKind::Workflow, Some(main)),
            SessionEvent::Runner {
                id: run,
                event: Box::new(RunnerEvent::Workflow(
                    crate::sessions::runners::workflow::Event::StepStarted {
                        index: 0,
                        step: "triage".into(),
                        agent: AgentId::new_v4(),
                        attempt: 1,
                        from: None,
                        via: None,
                        input: "the build is red".into(),
                    },
                )),
                at_ms: 1_700_000_000_000,
            },
            SessionEvent::UsageBanked {
                model: "sonnet".into(),
                spent: UsageTotal {
                    input_tokens: 7,
                    output_tokens: 3,
                    ..Default::default()
                },
            },
            SessionEvent::RunnerEnded {
                id: child,
                status: RunnerStatus::Done,
                at_ms: 9,
            },
        ];

        let fold = |log: &[SessionEvent]| {
            let mut s = SessionState::default();
            for e in log {
                s.apply(e);
            }
            s
        };
        let once = fold(&log);
        let twice = fold(&log);
        assert_eq!(
            serde_json::to_string(&once).unwrap(),
            serde_json::to_string(&twice).unwrap()
        );

        // And the stamp is the entry's, not zero: a fold that ignored `at_ms`
        // would pass the equality above and still lose every time in the state.
        let RunnerState::Workflow(w) = &once.runners[&run].state else {
            panic!("the run is a workflow");
        };
        assert_eq!(w.steps[0].started_at_ms, 1_700_000_000_000);
    }

    /// The state is snapshotted, so a row missing a field must still load.
    #[test]
    fn an_empty_row_deserializes() {
        let s: SessionState = serde_json::from_str("{}").unwrap();
        assert!(s.runners.is_empty());
        assert!(s.agents.is_empty());
        assert_eq!(s.status(), RunnerStatus::Pending);
    }
}
