//! Every unit of work a session hosts, in one hierarchical forest.
//!
//! An entry is one *run* of something: the main conversation, a delegated
//! subagent task, a workflow run (the session's own, or one an agent invoked
//! mid-session), or a forked conversation. Each entry names the **agent it runs
//! under** — `parent` — and that one edge is the whole nesting model: a step
//! agent can spawn subagents, a subagent can invoke a workflow, a workflow's
//! step can spawn subagents, and every routing question is answered by the same
//! two lookups.
//!
//! Pure data, like the structures it replaces (`SubAgentForest`, `ForkRoster`,
//! the single `WorkflowRunState`): the session actor folds its journal through
//! these methods, so live operation and recovery follow one path. The
//! components keep their roles — turns, subagents, workflow runs, forks — and
//! each folds only entries of its own kind; the forest is where their state
//! lives, not who decides.
//!
//! Identity is deliberately plain: an agent-shaped entry (main, subagent,
//! fork) is keyed by its agent's uuid — main's is the session's — and a
//! workflow entry mints its own run id, hosting its step agents through the
//! `agents` index. There is no per-kind key enum; what an id *is* lives in the
//! entry it resolves to, where a `match` must be exhaustive.

use crate::sessions::session_actor::AgentStatus;
use crate::sessions::workflow::{WorkflowRunSpec, WorkflowRunState, render_result};
use horsie_models::agent::SubAgentResultPart;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

/// Deepest entry the forest may hold, counting every delegation edge — a
/// subagent spawn and a workflow invocation alike. An entry *at* this depth can
/// neither spawn nor invoke, which is what bounds a workflow that invokes
/// itself.
pub const MAX_DEPTH: u32 = 4;

/// Cap on concurrently-active subagents when the caller's settings name none.
pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: u32 = 8;

/// Cap on workflow runs that are live (not yet terminal) in one session.
pub const MAX_LIVE_RUNS: usize = 8;

/// Error recorded for delegated work that was mid-run when the process died.
pub const INTERRUPTED_ERROR: &str = "interrupted by restart";

/// Error recorded for delegated work someone stopped.
///
/// Its own wording rather than [`INTERRUPTED_ERROR`]'s, because this one
/// reaches a *model*: the parent reads it as the result of the child it is
/// waiting on, and "interrupted by restart" would have it reason about a crash
/// that never happened.
pub const STOPPED_ERROR: &str = "stopped before it finished";

/// Largest result (output or error) injected into a parent's context or
/// rendered by `subagent_status` — the same bound the runtime puts on a tool's
/// streamed output.
pub const MAX_RESULT_BYTES: usize = 50_000;

/// Cap a result for injection/rendering, marking the cut so the reader knows
/// the answer continues elsewhere (the full transcript is always in the
/// child's own history).
fn truncate_result(text: &str) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_RESULT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}…\n\n[truncated: {} bytes total]",
        &text[..end],
        text.len()
    )
}

/// One unit of work the session hosts. For main, a subagent or a fork this is
/// the agent's own uuid (main's is the session's); a workflow entry mints its
/// own, distinct from any agent's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RunId(pub Uuid);

/// One entry of the forest: whose agent it runs under, and what it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEntry {
    /// The agent this work runs *under*: what spawned or invoked it. `None`
    /// only for the root — the main conversation, or the session's own
    /// workflow run. This one field is the whole nesting model.
    pub parent: Option<Uuid>,
    pub created_at_ms: u64,
    pub state: RunState,
}

/// What an entry is. The kind lives here rather than in the key, so a routing
/// question is one map hit and adding a kind is one variant — with the
/// compiler holding every `match` to completeness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RunState {
    Main(MainRun),
    Sub(SubAgentRun),
    Workflow(WorkflowRun),
    Fork(ForkRun),
}

/// Where the main conversation's turn stands. The session's status used to be
/// this, stored beside everything else's and written by three components; now
/// it is the root entry's own fact and the status is a projection.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TurnPhase {
    #[default]
    Idle,
    Running,
    /// Parked on one or more questions. Carries none of them: the questions
    /// belong to the agent that asked and are answered through it.
    AwaitingInput,
    /// The last turn failed. Sticky so a client can badge it; fully
    /// recoverable — the next turn moves it back to `Running`.
    Failed {
        error: String,
    },
}

/// The main conversation.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MainRun {
    pub turn: TurnPhase,
}

/// Lifecycle of one subagent. `Completed`/`Failed` are turn-terminal, not
/// actor-terminal: a node with children may wake again to consume their
/// results and conclude a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubAgentStatus {
    Running,
    Completed,
    Failed,
}

/// One delegated task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubAgentRun {
    pub label: String,
    pub task: String,
    /// The plugin-declared agent type this runs as, if any. `None` is the
    /// general-purpose subagent.
    pub agent_type: Option<String>,
    pub status: SubAgentStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    /// Whether the parent was sent this entry's latest terminal result. Every
    /// completion resets it; every actual send re-marks it — that pair is what
    /// makes delivery exactly-once across offloads.
    pub notified: bool,
    /// When this cycle of work started and reached its terminal state. The
    /// span restarts when a terminal node wakes to consume child results, so
    /// the parent is told about the cycle, not the whole life of the node.
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
}

/// One workflow run: the session's own (parent `None`) or one an agent invoked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRun {
    /// The workflow this was started from — a name for display and for the
    /// report its invoker reads.
    pub workflow: String,
    /// The graph, snapshotted at creation. On the entry (journaled with the
    /// event that created it) so replay never reaches a store, and so every
    /// run — root or invoked — resolves its steps from its *own* definition.
    pub graph: Arc<WorkflowRunSpec>,
    /// The run's status and append-only step log — the existing shape,
    /// unchanged: it is already a replayable log with a lossless graph
    /// projection.
    pub run: WorkflowRunState,
    /// Whether the invoking agent was sent this run's terminal result. Inert
    /// for the root run, which has no parent to owe.
    pub notified: bool,
}

/// One forked conversation. Owes nobody a result — there is deliberately no
/// `notified` here, so the owed-delivery query cannot misread a fork.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkRun {
    /// The source agent's log seq this fork was taken at — the branch point.
    pub source_seq: u64,
    pub mode: ForkMode,
    /// What the fork was created to do. Durable here, not merely queued on the
    /// agent, because a fork abandoned mid-seed is re-seeded from this record.
    pub message: String,
    /// What the fork has named itself, once it has.
    pub title: Option<String>,
    /// `Provisioning` is the seeding window — the one state in which no turn
    /// has run, and the reason an interrupted seed is safe to re-attempt.
    pub status: AgentStatus,
    /// When this fork last did anything — the moment of its most recent status
    /// change.
    pub last_activity_ms: u64,
}

/// How a fork's history was seeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForkMode {
    /// `/fork` — the source's log, copied and scrubbed.
    Copy,
    /// `/summary-n-fork` — a summary of the source, produced out of band.
    Summary,
    /// `spawn_conversation` — no history at all. The agent that asked for the
    /// fork already knows the context and writes the brief itself, so there is
    /// nothing to carry and nothing to summarise.
    Fresh,
}

impl ForkMode {
    /// The wire spelling, and what a lifecycle entry carries.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Summary => "summary",
            Self::Fresh => "fresh",
        }
    }
}

/// One result a child owes the agent that asked for it: a finished subagent's
/// report, or a finished workflow run's.
#[derive(Debug, Clone, PartialEq)]
pub struct OwedDelivery {
    /// The entry whose result this is, so the send can be recorded against it.
    pub child: RunId,
    /// Whose queue it goes in.
    pub to: Uuid,
    pub part: SubAgentResultPart,
}

/// The forest: every entry, and which entry hosts every agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunForest {
    /// The entry with no parent. `None` until the session learns what it is.
    root: Option<RunId>,
    entries: BTreeMap<RunId, RunEntry>,
    /// Which entry hosts each agent: main and every sub/fork host themselves;
    /// a step agent is hosted by its run's entry.
    agents: BTreeMap<Uuid, RunId>,
}

impl RunForest {
    // ---------------------------------------------------------------- roots

    /// The session is a conversation: one main agent, keyed by the session id.
    pub fn apply_root_agent(&mut self, session: Uuid, at_ms: u64) {
        let id = RunId(session);
        self.entries.insert(
            id,
            RunEntry {
                parent: None,
                created_at_ms: at_ms,
                state: RunState::Main(MainRun::default()),
            },
        );
        self.agents.insert(session, id);
        self.root = Some(id);
    }

    /// The session is a workflow run: the root entry is the run itself, keyed
    /// by the session id so its step agent ids stay derivable from it.
    pub fn apply_root_workflow(
        &mut self,
        session: Uuid,
        workflow: String,
        graph: Arc<WorkflowRunSpec>,
        at_ms: u64,
    ) {
        let id = RunId(session);
        self.entries.insert(
            id,
            RunEntry {
                parent: None,
                created_at_ms: at_ms,
                state: RunState::Workflow(WorkflowRun {
                    workflow,
                    graph,
                    run: WorkflowRunState::default(),
                    notified: false,
                }),
            },
        );
        self.root = Some(id);
    }

    // -------------------------------------------------------------- lookups

    #[must_use]
    pub fn root_id(&self) -> Option<RunId> {
        self.root
    }

    #[must_use]
    pub fn root(&self) -> Option<&RunEntry> {
        self.root.and_then(|id| self.entries.get(&id))
    }

    /// The session's own workflow run, when the root is one.
    #[must_use]
    pub fn root_workflow(&self) -> Option<(RunId, &WorkflowRun)> {
        let id = self.root?;
        match self.entries.get(&id).map(|e| &e.state) {
            Some(RunState::Workflow(w)) => Some((id, w)),
            Some(RunState::Main(_) | RunState::Sub(_) | RunState::Fork(_)) | None => None,
        }
    }

    /// Whether this session *is* a workflow run.
    #[must_use]
    pub fn root_is_workflow(&self) -> bool {
        self.root_workflow().is_some()
    }

    /// The agent of the root run's step in flight — what an unaddressed
    /// request on a workflow session means, since a run has no main agent and
    /// at most one of its steps runs at a time.
    #[must_use]
    pub fn current_root_step_agent(&self) -> Option<Uuid> {
        self.root_workflow()
            .and_then(|(_, w)| w.run.current_agent())
    }

    /// Every step in flight, across every run: the executions a stop, a delete
    /// or crash recovery must account for.
    #[must_use]
    pub fn in_flight_steps(&self) -> Vec<(RunId, u32, Uuid)> {
        self.workflows()
            .filter_map(|(id, w)| {
                let index = w.run.current()?;
                let agent = w.run.get(index)?.agent;
                Some((id, index, agent))
            })
            .collect()
    }

    /// Whether any run has a step in flight.
    #[must_use]
    pub fn has_step_in_flight(&self) -> bool {
        self.workflows().any(|(_, w)| w.run.current().is_some())
    }

    #[must_use]
    pub fn entry(&self, id: RunId) -> Option<&RunEntry> {
        self.entries.get(&id)
    }

    /// The entry that hosts `agent` — its own for main, a subagent or a fork;
    /// its run's for a step agent.
    #[must_use]
    pub fn owner_of_agent(&self, agent: Uuid) -> Option<(RunId, &RunEntry)> {
        let id = *self.agents.get(&agent)?;
        self.entries.get(&id).map(|entry| (id, entry))
    }

    #[must_use]
    pub fn is_known_agent(&self, agent: Uuid) -> bool {
        self.agents.contains_key(&agent)
    }

    /// How deep `agent` sits, counting every delegation edge above it. Main is
    /// 0; so is a root run's step agent — a step is the run's own work, not
    /// delegated by it. `None` for an agent the forest does not know.
    #[must_use]
    pub fn depth_of_agent(&self, agent: Uuid) -> Option<u32> {
        let (id, entry) = self.owner_of_agent(agent)?;
        self.depth_of_entry(id, entry)
    }

    fn depth_of_entry(&self, id: RunId, entry: &RunEntry) -> Option<u32> {
        // Bounded by the forest's size: parent chains are built by appending,
        // so a cycle is impossible — but this walks recovered data, and a
        // bound costs one comparison per step.
        let mut depth = 0u32;
        let mut at = entry.parent;
        let mut hops = 0usize;
        let _ = id;
        while let Some(parent_agent) = at {
            hops += 1;
            if hops > self.entries.len() {
                tracing::warn!("a run entry's parent chain does not terminate; reporting it flat");
                return Some(0);
            }
            depth += 1;
            match self.owner_of_agent(parent_agent) {
                Some((_, parent_entry)) => at = parent_entry.parent,
                // A deleted ancestor (a removed fork) leaves the chain where
                // it stands: this is as deep as it can be said to be.
                None => break,
            }
        }
        Some(depth)
    }

    /// Whether `descendant` sits anywhere under `ancestor`'s agent — the
    /// visibility rule: an agent sees itself and its own subtree, never a
    /// sibling's.
    #[must_use]
    pub fn descends_from(&self, descendant: Uuid, ancestor: Uuid) -> bool {
        let mut at = Some(descendant);
        let mut hops = 0usize;
        while let Some(agent) = at {
            if agent == ancestor {
                return true;
            }
            hops += 1;
            if hops > self.entries.len().saturating_add(1) {
                return false;
            }
            at = match self.owner_of_agent(agent) {
                // A step agent's next ancestor is its run's inviter.
                Some((_, entry)) => entry.parent,
                None => None,
            };
        }
        false
    }

    // ------------------------------------------------------------- subagents

    pub fn apply_sub_spawned(
        &mut self,
        id: Uuid,
        parent: Uuid,
        label: String,
        task: String,
        agent_type: Option<String>,
        at_ms: u64,
    ) {
        self.entries.insert(
            RunId(id),
            RunEntry {
                parent: Some(parent),
                created_at_ms: at_ms,
                state: RunState::Sub(SubAgentRun {
                    label,
                    task,
                    agent_type,
                    status: SubAgentStatus::Running,
                    output: None,
                    error: None,
                    notified: false,
                    started_at_ms: at_ms,
                    ended_at_ms: 0,
                }),
            },
        );
        self.agents.insert(id, RunId(id));
    }

    /// A terminal subagent started another cycle (woken to consume child
    /// results). The span restarts with it.
    pub fn apply_sub_running(&mut self, id: Uuid, at_ms: u64) {
        if let Some(sub) = self.sub_mut(id) {
            sub.status = SubAgentStatus::Running;
            sub.started_at_ms = at_ms;
            sub.ended_at_ms = 0;
        }
    }

    pub fn apply_sub_completed(&mut self, id: Uuid, output: String, at_ms: u64) {
        if let Some(sub) = self.sub_mut(id) {
            sub.status = SubAgentStatus::Completed;
            sub.output = Some(output);
            sub.error = None;
            sub.notified = false;
            sub.ended_at_ms = at_ms;
        }
    }

    pub fn apply_sub_failed(&mut self, id: Uuid, error: String, at_ms: u64) {
        if let Some(sub) = self.sub_mut(id) {
            sub.status = SubAgentStatus::Failed;
            sub.error = Some(error);
            sub.notified = false;
            sub.ended_at_ms = at_ms;
        }
    }

    pub fn apply_sub_notified(&mut self, id: Uuid) {
        if let Some(sub) = self.sub_mut(id) {
            sub.notified = true;
        }
    }

    #[must_use]
    pub fn sub(&self, id: Uuid) -> Option<&SubAgentRun> {
        match self.entries.get(&RunId(id)).map(|e| &e.state) {
            Some(RunState::Sub(sub)) => Some(sub),
            Some(RunState::Main(_) | RunState::Workflow(_) | RunState::Fork(_)) | None => None,
        }
    }

    fn sub_mut(&mut self, id: Uuid) -> Option<&mut SubAgentRun> {
        match self.entries.get_mut(&RunId(id)).map(|e| &mut e.state) {
            Some(RunState::Sub(sub)) => Some(sub),
            Some(RunState::Main(_) | RunState::Workflow(_) | RunState::Fork(_)) | None => None,
        }
    }

    /// Every subagent id the forest holds.
    #[must_use]
    pub fn sub_ids(&self) -> Vec<Uuid> {
        self.entries
            .iter()
            .filter(|(_, e)| matches!(e.state, RunState::Sub(_)))
            .map(|(id, _)| id.0)
            .collect()
    }

    /// Subagents mid-run anywhere in the session — the concurrency limit's
    /// measure, and what decides an offload is unsafe.
    #[must_use]
    pub fn active_sub_count(&self) -> u32 {
        self.subs()
            .filter(|(_, s)| s.status == SubAgentStatus::Running)
            .count() as u32
    }

    #[must_use]
    pub fn has_active_subs(&self) -> bool {
        self.subs()
            .any(|(_, s)| s.status == SubAgentStatus::Running)
    }

    /// Subagents still `Running` — at recovery, ones the process died under.
    #[must_use]
    pub fn interrupted_subs(&self) -> Vec<Uuid> {
        self.subs()
            .filter(|(_, s)| s.status == SubAgentStatus::Running)
            .map(|(id, _)| id)
            .collect()
    }

    fn subs(&self) -> impl Iterator<Item = (Uuid, &SubAgentRun)> {
        self.entries.iter().filter_map(|(id, e)| match &e.state {
            RunState::Sub(sub) => Some((id.0, sub)),
            RunState::Main(_) | RunState::Workflow(_) | RunState::Fork(_) => None,
        })
    }

    // ---------------------------------------------------------------- turns

    /// The main conversation's turn began.
    pub fn apply_turn_began(&mut self, agent: Uuid) {
        if let Some(main) = self.main_mut(agent) {
            main.turn = TurnPhase::Running;
        }
    }

    /// The main conversation's turn ended, stopped, or was found interrupted.
    pub fn apply_turn_idle(&mut self, agent: Uuid) {
        if let Some(main) = self.main_mut(agent) {
            main.turn = TurnPhase::Idle;
        }
    }

    pub fn apply_turn_failed(&mut self, agent: Uuid, error: String) {
        if let Some(main) = self.main_mut(agent) {
            main.turn = TurnPhase::Failed { error };
        }
    }

    /// `agent` parked on questions. For the main conversation that is its turn
    /// phase; for a workflow's step it parks the run — the step stays running,
    /// and the answer resumes it.
    pub fn apply_asked(&mut self, agent: Uuid) {
        let Some(id) = self.agents.get(&agent).copied() else {
            return;
        };
        match self.entries.get_mut(&id).map(|e| &mut e.state) {
            Some(RunState::Main(main)) => main.turn = TurnPhase::AwaitingInput,
            Some(RunState::Workflow(run)) => run.run.apply_awaiting(),
            Some(RunState::Sub(_) | RunState::Fork(_)) | None => {}
        }
    }

    /// The main conversation's turn phase, when the root is one.
    #[must_use]
    pub fn main_turn(&self) -> Option<&TurnPhase> {
        match self.root().map(|e| &e.state) {
            Some(RunState::Main(main)) => Some(&main.turn),
            Some(RunState::Sub(_) | RunState::Workflow(_) | RunState::Fork(_)) | None => None,
        }
    }

    fn main_mut(&mut self, agent: Uuid) -> Option<&mut MainRun> {
        match self.entries.get_mut(&RunId(agent)).map(|e| &mut e.state) {
            Some(RunState::Main(main)) => Some(main),
            Some(RunState::Sub(_) | RunState::Workflow(_) | RunState::Fork(_)) | None => None,
        }
    }

    // ------------------------------------------------------- workflow runs

    /// An agent invoked a workflow mid-session.
    pub fn apply_run_created(
        &mut self,
        id: RunId,
        parent: Uuid,
        workflow: String,
        graph: Arc<WorkflowRunSpec>,
        at_ms: u64,
    ) {
        self.entries.insert(
            id,
            RunEntry {
                parent: Some(parent),
                created_at_ms: at_ms,
                state: RunState::Workflow(WorkflowRun {
                    workflow,
                    graph,
                    run: WorkflowRunState::default(),
                    notified: false,
                }),
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn apply_step_started(
        &mut self,
        run: RunId,
        step: String,
        agent: Uuid,
        attempt: u32,
        from: Option<u32>,
        via: Option<String>,
        input: String,
        at_ms: u64,
    ) {
        if let Some(w) = self.workflow_mut(run) {
            w.run
                .apply_started(step, agent, attempt, from, via, input, at_ms);
        }
        self.agents.insert(agent, run);
    }

    pub fn apply_step_concluded(&mut self, run: RunId, index: u32, output: Value, at_ms: u64) {
        if let Some(w) = self.workflow_mut(run) {
            w.run.apply_concluded(index, output, at_ms);
        }
    }

    pub fn apply_step_failed(&mut self, run: RunId, index: u32, error: String, at_ms: u64) {
        if let Some(w) = self.workflow_mut(run) {
            w.run.apply_step_failed(index, error.clone(), at_ms);
            w.run.apply_failed(error);
            w.notified = false;
        }
    }

    pub fn apply_step_cancelled(&mut self, run: RunId, index: u32, at_ms: u64) {
        if let Some(w) = self.workflow_mut(run) {
            w.run.apply_cancelled(index, at_ms);
        }
    }

    pub fn apply_run_finished(&mut self, run: RunId, output: Value) {
        if let Some(w) = self.workflow_mut(run) {
            w.run.apply_finished(output);
            w.notified = false;
        }
    }

    pub fn apply_run_failed(&mut self, run: RunId, error: String) {
        if let Some(w) = self.workflow_mut(run) {
            w.run.apply_failed(error);
            w.notified = false;
        }
    }

    pub fn apply_run_notified(&mut self, run: RunId) {
        if let Some(w) = self.workflow_mut(run) {
            w.notified = true;
        }
    }

    #[must_use]
    pub fn workflow(&self, id: RunId) -> Option<&WorkflowRun> {
        match self.entries.get(&id).map(|e| &e.state) {
            Some(RunState::Workflow(w)) => Some(w),
            Some(RunState::Main(_) | RunState::Sub(_) | RunState::Fork(_)) | None => None,
        }
    }

    fn workflow_mut(&mut self, id: RunId) -> Option<&mut WorkflowRun> {
        match self.entries.get_mut(&id).map(|e| &mut e.state) {
            Some(RunState::Workflow(w)) => Some(w),
            Some(RunState::Main(_) | RunState::Sub(_) | RunState::Fork(_)) | None => None,
        }
    }

    /// Every workflow run the forest holds, root first (map order otherwise).
    pub fn workflows(&self) -> impl Iterator<Item = (RunId, &WorkflowRun)> {
        self.entries.iter().filter_map(|(id, e)| match &e.state {
            RunState::Workflow(w) => Some((*id, w)),
            RunState::Main(_) | RunState::Sub(_) | RunState::Fork(_) => None,
        })
    }

    /// The run hosting `agent` as one of its step executions, with the index.
    #[must_use]
    pub fn step_of_agent(&self, agent: Uuid) -> Option<(RunId, u32)> {
        let (id, entry) = self.owner_of_agent(agent)?;
        match &entry.state {
            RunState::Workflow(w) => w.run.index_of_agent(agent).map(|index| (id, index)),
            RunState::Main(_) | RunState::Sub(_) | RunState::Fork(_) => None,
        }
    }

    /// Workflow runs not yet terminal — the live-run limit's measure.
    #[must_use]
    pub fn live_run_count(&self) -> usize {
        self.workflows()
            .filter(|(_, w)| !w.run.status.is_terminal())
            .count()
    }

    /// Every entry that runs under `of`, directly or through any chain of
    /// spawns and invocations — the closure a stop must take down with it.
    /// `of`'s own entry is not in it.
    #[must_use]
    pub fn descendant_entries(&self, of: Uuid) -> Vec<RunId> {
        self.entries
            .iter()
            .filter(|(id, entry)| {
                if id.0 == of {
                    return false;
                }
                match &entry.state {
                    // An agent-shaped entry is its own agent: walk from it.
                    RunState::Sub(_) | RunState::Fork(_) | RunState::Main(_) => {
                        self.descends_from(id.0, of)
                    }
                    // A run is reached through the agent that invoked it.
                    RunState::Workflow(_) => entry
                        .parent
                        .is_some_and(|p| p == of || self.descends_from(p, of)),
                }
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// One run's detail for the `workflow_status` tool: its phase and its step
    /// log, each execution with its status.
    #[must_use]
    pub fn render_run(&self, id: RunId) -> Option<String> {
        let w = self.workflow(id)?;
        let phase = match w.run.status {
            crate::sessions::workflow::WorkflowRunStatus::Pending => "pending",
            crate::sessions::workflow::WorkflowRunStatus::Running => "running",
            crate::sessions::workflow::WorkflowRunStatus::Suspended => "suspended",
            crate::sessions::workflow::WorkflowRunStatus::AwaitingInput => "awaiting input",
            crate::sessions::workflow::WorkflowRunStatus::Finished => "finished",
            crate::sessions::workflow::WorkflowRunStatus::Failed => "failed",
        };
        let mut out = format!("workflow \"{}\" ({}) — {phase}", w.workflow, id.0);
        for (index, step) in w.run.steps.iter().enumerate() {
            let status = match step.status {
                crate::sessions::workflow::StepStatus::Running => "running",
                crate::sessions::workflow::StepStatus::Concluded => "concluded",
                crate::sessions::workflow::StepStatus::Failed => "failed",
                crate::sessions::workflow::StepStatus::Cancelled => "cancelled",
            };
            out.push_str(&format!("\n{index}. \"{}\" [{status}]", step.step));
        }
        if let Some(error) = &w.run.error {
            out.push_str(&format!("\n\nerror:\n{}", truncate_result(error)));
        } else if let Some(output) = &w.run.output {
            out.push_str(&format!(
                "\n\noutput:\n{}",
                truncate_result(&render_result(output))
            ));
        }
        Some(out)
    }

    // ---------------------------------------------------------------- forks

    #[allow(clippy::too_many_arguments)]
    pub fn apply_fork_created(
        &mut self,
        id: Uuid,
        parent: Uuid,
        source_seq: u64,
        mode: ForkMode,
        message: String,
        at_ms: u64,
    ) {
        self.entries.insert(
            RunId(id),
            RunEntry {
                parent: Some(parent),
                created_at_ms: at_ms,
                state: RunState::Fork(ForkRun {
                    source_seq,
                    mode,
                    message,
                    title: None,
                    // Nothing may run until the seed lands — the same status a
                    // session uses while its runtime is built, and the reason
                    // a fork found in it at load is safe to re-seed: it is
                    // precisely the state in which no turn has run.
                    status: AgentStatus::Provisioning,
                    last_activity_ms: at_ms,
                }),
            },
        );
        self.agents.insert(id, RunId(id));
    }

    /// The seed is durable, so the fork may run.
    ///
    /// Only out of `Provisioning`: the seed carries the fork's first message,
    /// so the fork can report `Running` before the session records the seed —
    /// writing `Idle` regardless moved a working fork backwards.
    pub fn apply_fork_seeded(&mut self, id: Uuid) {
        if let Some(fork) = self
            .fork_mut(id)
            .filter(|fork| fork.status == AgentStatus::Provisioning)
        {
            fork.status = AgentStatus::Idle;
        }
    }

    pub fn apply_fork_titled(&mut self, id: Uuid, title: String) {
        if let Some(fork) = self.fork_mut(id) {
            fork.title = Some(title);
        }
    }

    pub fn apply_fork_status(&mut self, id: Uuid, status: AgentStatus, at_ms: u64) {
        if let Some(fork) = self.fork_mut(id) {
            fork.status = status;
            fork.last_activity_ms = at_ms;
        }
    }

    pub fn apply_fork_deleted(&mut self, id: Uuid) {
        if matches!(
            self.entries.get(&RunId(id)).map(|e| &e.state),
            Some(RunState::Fork(_))
        ) {
            self.entries.remove(&RunId(id));
            self.agents.remove(&id);
        }
    }

    #[must_use]
    pub fn fork(&self, id: Uuid) -> Option<&ForkRun> {
        match self.entries.get(&RunId(id)).map(|e| &e.state) {
            Some(RunState::Fork(fork)) => Some(fork),
            Some(RunState::Main(_) | RunState::Sub(_) | RunState::Workflow(_)) | None => None,
        }
    }

    fn fork_mut(&mut self, id: Uuid) -> Option<&mut ForkRun> {
        match self.entries.get_mut(&RunId(id)).map(|e| &mut e.state) {
            Some(RunState::Fork(fork)) => Some(fork),
            Some(RunState::Main(_) | RunState::Sub(_) | RunState::Workflow(_)) | None => None,
        }
    }

    pub fn forks(&self) -> impl Iterator<Item = (Uuid, &ForkRun)> {
        self.entries.iter().filter_map(|(id, e)| match &e.state {
            RunState::Fork(fork) => Some((id.0, fork)),
            RunState::Main(_) | RunState::Sub(_) | RunState::Workflow(_) => None,
        })
    }

    #[must_use]
    pub fn fork_ids(&self) -> Vec<Uuid> {
        self.forks().map(|(id, _)| id).collect()
    }

    #[must_use]
    pub fn has_forks(&self) -> bool {
        self.forks().next().is_some()
    }

    /// Forks whose seed never landed. Re-seeded at load: seeding is
    /// session-owned work with no journal of its own, so nothing else can
    /// finish one a dead process abandoned.
    #[must_use]
    pub fn seeding_forks(&self) -> Vec<Uuid> {
        self.forks()
            .filter(|(_, f)| matches!(f.status, AgentStatus::Provisioning))
            .map(|(id, _)| id)
            .collect()
    }

    #[must_use]
    pub fn has_seeding_forks(&self) -> bool {
        self.forks()
            .any(|(_, f)| matches!(f.status, AgentStatus::Provisioning))
    }

    // ------------------------------------------------------------- delivery

    /// Every result a child owes the agent that asked for it and has not been
    /// sent: finished subagents and finished invoked runs, under one rule —
    /// a parent, a terminal result, and `!notified`. Forks have no terminal
    /// report by construction, so they cannot appear here.
    #[must_use]
    pub fn owed(&self) -> Vec<OwedDelivery> {
        let mut out = Vec::new();
        for (id, entry) in &self.entries {
            let Some(parent) = entry.parent else {
                continue;
            };
            let part = match &entry.state {
                RunState::Sub(sub) => {
                    if sub.status == SubAgentStatus::Running || sub.notified {
                        continue;
                    }
                    sub_result_part(id.0, sub)
                }
                RunState::Workflow(w) => {
                    if !w.run.status.is_terminal() || w.notified {
                        continue;
                    }
                    run_result_part(*id, entry.created_at_ms, w)
                }
                RunState::Main(_) | RunState::Fork(_) => continue,
            };
            out.push(OwedDelivery {
                child: *id,
                to: parent,
                part,
            });
        }
        out
    }

    // ------------------------------------------------------------ rendering

    /// One subagent's detail for the `subagent_status` tool.
    #[must_use]
    pub fn render_sub(&self, id: Uuid) -> Option<String> {
        let sub = self.sub(id)?;
        let status = match sub.status {
            SubAgentStatus::Running => "running",
            SubAgentStatus::Completed => "completed",
            SubAgentStatus::Failed => "failed",
        };
        let depth = self.depth_of_agent(id).unwrap_or(0);
        let mut out = format!(
            "subagent \"{}\" ({id}) — {status}, depth {depth}",
            sub.label
        );
        if let Some(output) = &sub.output {
            out.push_str(&format!("\n\noutput:\n{}", truncate_result(output)));
        }
        if let Some(error) = &sub.error {
            out.push_str(&format!("\n\nerror:\n{}", truncate_result(error)));
        }
        Some(out)
    }

    /// The subagents under `from`, as an indented list — the caller's own
    /// descendants, itself excluded.
    #[must_use]
    pub fn render_sub_tree(&self, from: Uuid) -> String {
        let mut out = String::new();
        self.render_children(from, 0, &mut out);
        if out.is_empty() {
            out.push_str("No subagents.\n");
        }
        out
    }

    fn render_children(&self, of: Uuid, indent: usize, out: &mut String) {
        for (id, entry) in &self.entries {
            if entry.parent != Some(of) {
                continue;
            }
            match &entry.state {
                RunState::Sub(sub) => {
                    let status = match sub.status {
                        SubAgentStatus::Running => "running",
                        SubAgentStatus::Completed => "completed",
                        SubAgentStatus::Failed => "failed",
                    };
                    let pad = "  ".repeat(indent);
                    out.push_str(&format!("{pad}- \"{}\" ({}) [{status}]\n", sub.label, id.0));
                    self.render_children(id.0, indent + 1, out);
                }
                // A step's subagents hang off the step agent, which the walk
                // reaches through the run's own log.
                RunState::Workflow(w) => {
                    for step in &w.run.steps {
                        self.render_children(step.agent, indent, out);
                    }
                }
                RunState::Main(_) | RunState::Fork(_) => {}
            }
        }
    }
}

/// The structured part a parent is handed when a subagent reaches a terminal
/// state. Status decides which body the parent hears — a node that completed
/// once and failed on a later cycle still holds the earlier output, and the
/// stale success must not mask the failure.
fn sub_result_part(id: Uuid, sub: &SubAgentRun) -> SubAgentResultPart {
    let (status, body) = match sub.status {
        SubAgentStatus::Failed => ("failed", sub.error.as_deref().unwrap_or_default()),
        SubAgentStatus::Running | SubAgentStatus::Completed => {
            ("completed", sub.output.as_deref().unwrap_or_default())
        }
    };
    SubAgentResultPart {
        subagent_id: id.to_string(),
        label: sub.label.clone(),
        status: status.to_string(),
        text: truncate_result(body),
        spawned_at_ms: sub.started_at_ms,
        ended_at_ms: sub.ended_at_ms,
    }
}

/// The same part, for a finished workflow run: the invoking agent reads a
/// run's report exactly as it reads a subagent's, which is what keeps nested
/// runs off the wire protocol entirely.
fn run_result_part(id: RunId, created_at_ms: u64, w: &WorkflowRun) -> SubAgentResultPart {
    let failed = w.run.status == crate::sessions::workflow::WorkflowRunStatus::Failed;
    let body = match (&w.run.error, &w.run.output) {
        (Some(error), _) if failed => error.clone(),
        (_, Some(output)) => render_result(output),
        (Some(error), None) => error.clone(),
        (None, None) => String::new(),
    };
    let ended_at_ms = w
        .run
        .steps
        .iter()
        .filter_map(|s| s.ended_at_ms)
        .max()
        .unwrap_or(0);
    SubAgentResultPart {
        subagent_id: id.0.to_string(),
        label: format!("workflow {}", w.workflow),
        status: if failed { "failed" } else { "completed" }.to_string(),
        text: truncate_result(&body),
        spawned_at_ms: created_at_ms,
        ended_at_ms,
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

    fn uid(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn graph(workflow: &str) -> Arc<WorkflowRunSpec> {
        Arc::new(WorkflowRunSpec {
            workflow: workflow.into(),
            start: "plan".into(),
            steps: vec![],
            input: "go".into(),
            max_steps: 10,
        })
    }

    fn conversation() -> (RunForest, Uuid) {
        let mut f = RunForest::default();
        let session = uid(1);
        f.apply_root_agent(session, 100);
        (f, session)
    }

    #[test]
    fn a_conversation_roots_a_main_entry_hosting_the_session_agent() {
        let (f, session) = conversation();
        assert_eq!(f.root_id(), Some(RunId(session)));
        let (id, entry) = f.owner_of_agent(session).unwrap();
        assert_eq!(id, RunId(session));
        assert!(entry.parent.is_none());
        assert_eq!(f.main_turn(), Some(&TurnPhase::Idle));
        assert_eq!(f.depth_of_agent(session), Some(0));
    }

    #[test]
    fn turn_events_move_only_the_main_entry() {
        let (mut f, session) = conversation();
        f.apply_turn_began(session);
        assert_eq!(f.main_turn(), Some(&TurnPhase::Running));
        f.apply_asked(session);
        assert_eq!(f.main_turn(), Some(&TurnPhase::AwaitingInput));
        f.apply_turn_failed(session, "boom".into());
        assert_eq!(
            f.main_turn(),
            Some(&TurnPhase::Failed {
                error: "boom".into()
            })
        );
        f.apply_turn_idle(session);
        assert_eq!(f.main_turn(), Some(&TurnPhase::Idle));
    }

    #[test]
    fn a_spawned_sub_is_hosted_under_its_parent_at_the_next_depth() {
        let (mut f, session) = conversation();
        let sub = uid(2);
        f.apply_sub_spawned(sub, session, "research".into(), "dig".into(), None, 200);
        let (id, entry) = f.owner_of_agent(sub).unwrap();
        assert_eq!(id, RunId(sub));
        assert_eq!(entry.parent, Some(session));
        assert_eq!(f.depth_of_agent(sub), Some(1));
        assert_eq!(f.active_sub_count(), 1);
        assert!(f.has_active_subs());
        assert_eq!(f.interrupted_subs(), vec![sub]);
    }

    #[test]
    fn completed_then_notified_makes_a_sub_not_owed() {
        let (mut f, session) = conversation();
        let sub = uid(2);
        f.apply_sub_spawned(sub, session, "audit".into(), "t".into(), None, 100);
        f.apply_sub_completed(sub, "done".into(), 400);
        let owed = f.owed();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].to, session);
        assert_eq!(owed[0].child, RunId(sub));
        assert_eq!(owed[0].part.text, "done");
        assert_eq!(
            (owed[0].part.spawned_at_ms, owed[0].part.ended_at_ms),
            (100, 400)
        );
        f.apply_sub_notified(sub);
        assert!(f.owed().is_empty());
    }

    #[test]
    fn a_second_completion_re_owes_the_parent_with_its_own_span() {
        let (mut f, session) = conversation();
        let sub = uid(2);
        f.apply_sub_spawned(sub, session, "w".into(), "t".into(), None, 100);
        f.apply_sub_completed(sub, "first".into(), 400);
        f.apply_sub_notified(sub);
        f.apply_sub_running(sub, 5_000);
        f.apply_sub_completed(sub, "second".into(), 5_200);
        let owed = f.owed();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].part.text, "second");
        assert_eq!(
            (owed[0].part.spawned_at_ms, owed[0].part.ended_at_ms),
            (5_000, 5_200)
        );
    }

    #[test]
    fn a_later_failure_reports_the_failure_not_the_earlier_output() {
        let (mut f, session) = conversation();
        let sub = uid(2);
        f.apply_sub_spawned(sub, session, "w".into(), "t".into(), None, 100);
        f.apply_sub_completed(sub, "first pass".into(), 400);
        f.apply_sub_notified(sub);
        f.apply_sub_running(sub, 500);
        f.apply_sub_failed(sub, "second pass blew up".into(), 900);
        let owed = f.owed();
        assert_eq!(owed[0].part.status, "failed");
        assert_eq!(owed[0].part.text, "second pass blew up");
    }

    #[test]
    fn a_nested_sub_is_owed_to_the_sub_that_spawned_it() {
        let (mut f, session) = conversation();
        let lead = uid(2);
        let helper = uid(3);
        f.apply_sub_spawned(lead, session, "lead".into(), "t".into(), None, 100);
        f.apply_sub_completed(lead, "waiting".into(), 200);
        f.apply_sub_notified(lead);
        f.apply_sub_spawned(helper, lead, "helper".into(), "t".into(), None, 300);
        f.apply_sub_completed(helper, "kid done".into(), 600);
        let owed = f.owed();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].to, lead);
        assert_eq!(owed[0].part.text, "kid done");
        assert_eq!(f.depth_of_agent(helper), Some(2));
    }

    #[test]
    fn an_owed_part_caps_a_huge_result() {
        let (mut f, session) = conversation();
        let sub = uid(2);
        let huge = "x".repeat(MAX_RESULT_BYTES + 10_000);
        f.apply_sub_spawned(sub, session, "w".into(), "t".into(), None, 100);
        f.apply_sub_completed(sub, huge.clone(), 400);
        let text = &f.owed()[0].part.text;
        assert!(text.contains("[truncated:"), "{:.200}", text);
        assert!(text.len() < huge.len());
        assert!(text.len() <= MAX_RESULT_BYTES + 100);
    }

    #[test]
    fn visibility_is_the_descendant_closure_and_nothing_else() {
        let (mut f, session) = conversation();
        let parent = uid(2);
        let child = uid(3);
        let other = uid(4);
        f.apply_sub_spawned(parent, session, "p".into(), "t".into(), None, 100);
        f.apply_sub_spawned(child, parent, "c".into(), "t".into(), None, 100);
        f.apply_sub_spawned(other, session, "o".into(), "t".into(), None, 100);
        assert!(f.descends_from(child, session));
        assert!(f.descends_from(child, parent));
        assert!(f.descends_from(parent, parent));
        assert!(!f.descends_from(child, other));
        assert!(!f.descends_from(uid(9), session));
    }

    #[test]
    fn renders_a_sub_and_a_subtree() {
        let (mut f, session) = conversation();
        let parent = uid(2);
        let child = uid(3);
        f.apply_sub_spawned(parent, session, "lead".into(), "t".into(), None, 100);
        f.apply_sub_spawned(child, parent, "helper".into(), "t".into(), None, 100);
        f.apply_sub_failed(child, "boom".into(), 400);
        let node = f.render_sub(child).unwrap();
        assert!(node.contains("boom"), "{node}");
        assert!(node.contains("failed"), "{node}");
        assert!(node.contains("depth 2"), "{node}");
        let tree = f.render_sub_tree(session);
        assert!(tree.contains("lead"), "{tree}");
        assert!(
            tree.contains("  - \"helper\""),
            "a child is indented: {tree}"
        );
        assert!(f.render_sub_tree(child).contains("No subagents"));
    }

    // ------------------------------------------------------ workflow entries

    #[test]
    fn a_workflow_session_roots_a_run_keyed_by_the_session() {
        let mut f = RunForest::default();
        let session = uid(1);
        f.apply_root_workflow(session, "review".into(), graph("review"), 100);
        assert_eq!(f.root_id(), Some(RunId(session)));
        let (id, w) = f.workflows().next().unwrap();
        assert_eq!(id, RunId(session));
        assert_eq!(w.workflow, "review");
        assert_eq!(f.live_run_count(), 1);
    }

    #[test]
    fn a_step_agent_is_hosted_by_its_run() {
        let mut f = RunForest::default();
        let session = uid(1);
        f.apply_root_workflow(session, "review".into(), graph("review"), 100);
        let step_agent = uid(5);
        f.apply_step_started(
            RunId(session),
            "plan".into(),
            step_agent,
            1,
            None,
            None,
            "go".into(),
            200,
        );
        let (id, _) = f.owner_of_agent(step_agent).unwrap();
        assert_eq!(id, RunId(session));
        assert_eq!(f.step_of_agent(step_agent), Some((RunId(session), 0)));
        // A root run's step is the run's own work: depth 0, so its subagents
        // count from 1 exactly as a conversation's do.
        assert_eq!(f.depth_of_agent(step_agent), Some(0));
        let sub = uid(6);
        f.apply_sub_spawned(sub, step_agent, "helper".into(), "t".into(), None, 300);
        assert_eq!(f.depth_of_agent(sub), Some(1));
        assert!(f.descends_from(sub, step_agent));
    }

    #[test]
    fn an_invoked_run_is_owed_to_its_invoker_when_terminal() {
        let (mut f, session) = conversation();
        let run = RunId(uid(7));
        f.apply_run_created(run, session, "deploy".into(), graph("deploy"), 500);
        assert!(f.owed().is_empty(), "a live run owes nothing yet");
        f.apply_run_finished(run, serde_json::json!({"description": "shipped"}));
        let owed = f.owed();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].to, session);
        assert_eq!(owed[0].child, run);
        assert_eq!(owed[0].part.label, "workflow deploy");
        assert_eq!(owed[0].part.status, "completed");
        assert!(
            owed[0].part.text.contains("shipped"),
            "{}",
            owed[0].part.text
        );
        f.apply_run_notified(run);
        assert!(f.owed().is_empty());
    }

    #[test]
    fn a_failed_run_reports_failed_with_its_error() {
        let (mut f, session) = conversation();
        let run = RunId(uid(7));
        f.apply_run_created(run, session, "deploy".into(), graph("deploy"), 500);
        f.apply_run_failed(run, "no transition matched".into());
        let owed = f.owed();
        assert_eq!(owed[0].part.status, "failed");
        assert_eq!(owed[0].part.text, "no transition matched");
        let _ = session;
    }

    #[test]
    fn the_root_run_is_never_owed_to_anybody() {
        let mut f = RunForest::default();
        let session = uid(1);
        f.apply_root_workflow(session, "review".into(), graph("review"), 100);
        f.apply_run_finished(RunId(session), serde_json::json!({"ok": true}));
        assert!(f.owed().is_empty(), "no parent, nothing owed");
        assert_eq!(f.live_run_count(), 0);
    }

    #[test]
    fn an_invoked_runs_step_counts_depth_from_the_invoker() {
        let (mut f, session) = conversation();
        let sub = uid(2);
        f.apply_sub_spawned(sub, session, "lead".into(), "t".into(), None, 100);
        let run = RunId(uid(7));
        f.apply_run_created(run, sub, "deploy".into(), graph("deploy"), 500);
        let step_agent = uid(8);
        f.apply_step_started(
            run,
            "plan".into(),
            step_agent,
            1,
            None,
            None,
            "go".into(),
            600,
        );
        // session(0) -> sub(1) -> run entry(2) hosts its step at that depth.
        assert_eq!(f.depth_of_agent(step_agent), Some(2));
        assert!(f.descends_from(step_agent, sub));
        assert!(f.descends_from(step_agent, session));
    }

    // ------------------------------------------------------------- forks

    #[test]
    fn a_fork_lifecycle_folds_like_the_roster_did() {
        let (mut f, session) = conversation();
        let fork = uid(3);
        f.apply_fork_created(fork, session, 42, ForkMode::Copy, "go".into(), 1_000);
        let rec = f.fork(fork).unwrap();
        assert_eq!(rec.source_seq, 42);
        assert_eq!(rec.status, AgentStatus::Provisioning);
        assert!(f.has_seeding_forks());
        f.apply_fork_seeded(fork);
        assert_eq!(f.fork(fork).unwrap().status, AgentStatus::Idle);
        assert!(!f.has_seeding_forks());
        f.apply_fork_titled(fork, "Try the other migration".into());
        f.apply_fork_status(fork, AgentStatus::Running, 2_000);
        let rec = f.fork(fork).unwrap();
        assert_eq!(rec.title.as_deref(), Some("Try the other migration"));
        assert_eq!(rec.last_activity_ms, 2_000);
        f.apply_fork_deleted(fork);
        assert!(f.fork(fork).is_none());
        assert!(!f.is_known_agent(fork));
    }

    #[test]
    fn seeding_does_not_overwrite_a_fork_that_is_already_working() {
        let (mut f, session) = conversation();
        let fork = uid(3);
        f.apply_fork_created(fork, session, 0, ForkMode::Copy, "go".into(), 1_000);
        f.apply_fork_status(fork, AgentStatus::Running, 1_100);
        f.apply_fork_seeded(fork);
        assert_eq!(f.fork(fork).unwrap().status, AgentStatus::Running);
    }

    #[test]
    fn events_for_a_deleted_fork_are_ignored() {
        let (mut f, session) = conversation();
        let fork = uid(3);
        f.apply_fork_created(fork, session, 0, ForkMode::Copy, "go".into(), 1_000);
        f.apply_fork_deleted(fork);
        f.apply_fork_seeded(fork);
        f.apply_fork_titled(fork, "ghost".into());
        f.apply_fork_status(fork, AgentStatus::Running, 2_000);
        assert!(f.fork(fork).is_none());
    }

    #[test]
    fn a_fork_never_appears_in_owed_deliveries() {
        let (mut f, session) = conversation();
        let fork = uid(3);
        f.apply_fork_created(fork, session, 0, ForkMode::Copy, "go".into(), 1_000);
        f.apply_fork_status(fork, AgentStatus::Idle, 2_000);
        assert!(f.owed().is_empty());
    }

    #[test]
    fn deleting_a_parent_fork_leaves_its_child_and_bounds_the_depth_walk() {
        let (mut f, session) = conversation();
        let parent = uid(3);
        let child = uid(4);
        f.apply_fork_created(parent, session, 0, ForkMode::Copy, "go".into(), 1_000);
        f.apply_fork_created(child, parent, 0, ForkMode::Copy, "go".into(), 2_000);
        f.apply_fork_deleted(parent);
        assert!(
            f.fork(child).is_some(),
            "a child fork is its own conversation"
        );
        // The chain above it is gone, so this is as deep as it can be said to be.
        assert_eq!(f.depth_of_agent(child), Some(1));
    }

    #[test]
    fn the_forest_round_trips_through_serde() {
        let (mut f, session) = conversation();
        let sub = uid(2);
        f.apply_sub_spawned(sub, session, "x".into(), "t".into(), None, 100);
        let run = RunId(uid(7));
        f.apply_run_created(run, sub, "deploy".into(), graph("deploy"), 500);
        f.apply_fork_created(uid(3), session, 0, ForkMode::Summary, "go".into(), 600);
        let json = serde_json::to_value(&f).unwrap();
        let back: RunForest = serde_json::from_value(json).unwrap();
        assert_eq!(back, f);
    }
}
