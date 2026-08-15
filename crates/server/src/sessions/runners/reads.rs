//! What a session looks like from outside: pure functions of [`SessionState`]
//! onto the shapes the HTTP API and the web UI already speak.
//!
//! Nothing here performs anything and nothing reads a clock, so every answer is
//! testable against a hand-built state with no actor, no runtime and no
//! journal — which is the whole reason this lands before the actor is swapped
//! over to it.
//!
//! # A method per fact, not a match per fact
//!
//! Several of these answers need something only one kind of runner holds: a
//! worker's label, a step's name, a conversation's last error. None of it is
//! reached with a `match` on [`RunnerState`](super::RunnerState). That match is
//! the per-kind dispatch this redesign exists to delete, and written here it
//! would simply grow back — one arm at a time, in the file every new read
//! touches.
//!
//! Instead each fact is a method on [`Runner`] that the runner holding it
//! answers and the rest leave defaulted. The split is worth stating: **a runner
//! answers what only it knows, and this module fills in what only the session
//! knows.** A worker says it is called "read the flake" and that it failed; the
//! session says which agent parented it and how deep it sits, because those are
//! facts about the shape of the session and not about the worker.
//!
//! # Where this deliberately differs from the shape it replaces
//!
//! - **`"main"` is a name, not an id.** It resolves to the root runner's agent —
//!   or, when the root is a run, to the step in flight. It is no longer the
//!   session's own uuid, and nothing here keys anything on that uuid.
//! - **Usage keys are uuids throughout.** The old map was keyed `"main" | uuid`;
//!   every agent now has a real id, so the special case is gone. See
//!   [`usage_stats`].
//! - **Depth stays per-tree**, which is decision 6 of the plan and is a
//!   deliberate hold rather than an oversight. See [`depth_of`].

use super::Runner;
use super::ids::{AgentId, RunnerId, RunnerKind};
use super::state::{RunnerRecord, SessionState};
use crate::agent_loop::{AgentUsageSnapshot, UsageTotal};
use crate::sessions::session_actor::{
    AgentEntry, AgentStatus, AgentUsageEntry, MAIN_AGENT_ID, SessionUsageStats,
};
use crate::sessions::spec::{AgentSettings, SessionSpec, SessionStatus, status_reason};
use crate::sessions::supervisor::ForkRow;
use crate::sessions::workflow::{WorkflowRunSpec, WorkflowRunState};
use std::collections::HashMap;
use std::sync::Arc;

// -- the session's status ---------------------------------------------------

/// This session's status.
///
/// Two sources, in this order:
///
/// 1. **The sandbox, which overrides everything.** Nothing can run without one,
///    so a session whose runtime is coming up is `Provisioning` whatever its
///    conversation last did, and one whose runtime can never be built is
///    `Unrecoverable` even though its conversation is sitting idle.
/// 2. **The root runner's own word for where it is.** Never an aggregate over
///    every runner: a subagent working in the background is not the session
///    working.
#[must_use]
pub fn session_status(s: &SessionState) -> SessionStatus {
    sandbox_standing(s)
        .or_else(|| s.record(s.root).and_then(|rec| rec.state.standing()))
        .unwrap_or_default()
}

/// What the sandbox makes of the session, when it makes anything of it.
///
/// `None` once it is up, which is the state that lets the root speak.
fn sandbox_standing(s: &SessionState) -> Option<SessionStatus> {
    s.runners
        .values()
        .find(|rec| rec.kind == RunnerKind::Runtime)
        .and_then(|rec| rec.state.standing())
}

/// A session's status as the status of the agent that *is* that session.
///
/// A conversation's root agent has no lifecycle of its own — it is the session
/// — so every one of the session's states has to project onto one of an
/// agent's, **with no catch-all arm**. A `_ =>` here is what once let a session
/// whose runtime never built report a `failed` status beside an `idle` agent.
fn main_status(status: &SessionStatus) -> AgentStatus {
    match status {
        SessionStatus::Provisioning => AgentStatus::Provisioning,
        SessionStatus::Running => AgentStatus::Running,
        SessionStatus::AwaitingInput => AgentStatus::AwaitingInput,
        // Every way a session can be broken, including the two that are *not*
        // `Failed`: a create that never built a runtime, and a session that can
        // never run again. Reported as anything else, they badge an idle agent
        // beside a document that says the session failed.
        SessionStatus::Failed { .. }
        | SessionStatus::ProvisioningFailed { .. }
        | SessionStatus::Unrecoverable { .. } => AgentStatus::Failed,
        // `Finished` is a run's, and a run has no root agent — but the match is
        // over the session's whole vocabulary, and an agent that is not working
        // is idle whichever way the session got there.
        SessionStatus::Idle | SessionStatus::Finished => AgentStatus::Idle,
    }
}

// -- the roster and one agent's entry ---------------------------------------

/// Every agent this session hosts, addressable at `/agents/:agent_id`.
///
/// The root's agents first, then everything else in runner order, so the shape
/// a client reads is stable and the session's own conversation — or its steps —
/// leads.
///
/// A fork's own agent is **not** here, and that is the shape this replaces: a
/// fork is a row in the session list, reachable at its own agent document, and
/// listing it here as well would have every client that nests forks render each
/// one twice. Its subagents are listed, exactly as they were.
#[must_use]
pub fn agent_roster(s: &SessionState) -> Vec<AgentEntry> {
    let main = resolve(s, None);
    let status = session_status(s);
    let order = std::iter::once(s.root).chain(s.runners.keys().copied().filter(|id| *id != s.root));
    let mut agents = Vec::new();
    for runner in order {
        let Some(rec) = s.record(runner) else {
            continue;
        };
        // A runner with a row in the session list is a conversation of its own,
        // and is read there rather than here.
        if rec.state.listing().is_some() {
            continue;
        }
        for row in rec.state.rows() {
            agents.push(entry(s, runner, rec, row, main, &status));
        }
    }
    agents
}

/// One agent's roster entry. `None` when this session hosts no such agent.
///
/// The same read [`agent_roster`] performs, so an agent's own document and its
/// row in the session's cannot disagree — they used to, and a concluded step
/// reported `running` for ever as a result.
#[must_use]
pub fn agent_entry(s: &SessionState, agent: AgentId) -> Option<AgentEntry> {
    let runner = s.runner_of(agent)?;
    let rec = s.record(runner)?;
    let id = agent.to_string();
    let row = rec.state.rows().into_iter().find(|row| row.id == id)?;
    Some(entry(
        s,
        runner,
        rec,
        row,
        resolve(s, None),
        &session_status(s),
    ))
}

/// The runner's row, filled in with what only the session knows: where the
/// agent sits, when its runner lived, and — for the one agent that *is* the
/// session — the session's own status.
fn entry(
    s: &SessionState,
    runner: RunnerId,
    rec: &RunnerRecord,
    mut row: AgentEntry,
    main: Option<AgentId>,
    status: &SessionStatus,
) -> AgentEntry {
    // The root runner's primary agent has no lifecycle of its own: it *is* the
    // session, so it reports the session's status rather than its runner's.
    // That is also what carries the sandbox down onto it — an agent whose
    // runtime is still being built reads `provisioning`, not `idle`.
    let is_main = runner == s.root && main.is_some_and(|agent| agent.to_string() == row.id);
    if is_main {
        row.status = main_status(status);
        row.error = status_reason(status);
    }
    // Zero at both ends is a runner saying it keeps no stamps of its own,
    // because its agent lived exactly as long as it did. Only a run answers
    // otherwise, and only the root is left at zero: the root is as old as the
    // session, whose `created_at` is on the same document.
    if row.started_at_ms == 0 && row.ended_at_ms == 0 && runner != s.root {
        row.started_at_ms = rec.created_at_ms;
        row.ended_at_ms = rec.ended_at_ms;
    }
    row.parent = parent_of(s, runner);
    row.depth = depth_of(s, runner, rec.kind);
    row
}

/// The agent that parented this runner, as a reader addresses it.
///
/// `None` when that agent belongs to the **root** runner, which is what both
/// `ForkParent::Main` and `SubAgentParent::Main` used to mean: rooted on
/// whatever this session's "main" is — the root conversation, or the step that
/// spawned it — and either way not a peer of the thing being described.
fn parent_of(s: &SessionState, runner: RunnerId) -> Option<uuid::Uuid> {
    let parent = s.record(runner)?.parent?;
    (s.runner_of(parent) != Some(s.root)).then(|| parent.as_uuid())
}

/// How deep this runner sits **inside its own tree**.
///
/// Decision 6 of the plan, and a deliberate hold. [`SessionState::depth_of`] is
/// session-tree depth, which is a defensible number and probably the better
/// one — but it is a wire change, and this is a refactor: a subagent under a
/// step of a *nested* run would read 2 where every client reads 1 today.
/// Changing it is a one-line follow-up with its own note in the API docs, not a
/// side effect of moving the read side.
///
/// So: hops up the parent chain while the runner above is of my own kind and is
/// not the root, started from the kind's own base. The root is where every tree
/// stops, which is what makes a fork of the session's conversation depth 0 while
/// a worker spawned by that same conversation is depth 1.
///
/// **The two conventions, and why they differ.** A subagent tree numbers from
/// 1: its root is the agent that spawned the worker, and that agent is not a
/// worker. A fork tree numbers from 0: its root is the session's own
/// conversation, which is a fork's peer in the same numbering — and a step is
/// chosen by its definition rather than spawned, so it roots its own tree at 0
/// for the same reason. The kind is the whole of what decides it, so it is
/// settled here rather than asked of each runner: a number a runner could
/// answer with is a number a runner could answer wrongly, and nothing about a
/// tree's numbering is a fact about the work in it.
///
/// Bounded by the number of runners rather than trusted to terminate: `parent`
/// is written once at creation so a cycle is impossible, but this walks data
/// recovered from a journal and the bound costs one comparison.
fn depth_of(s: &SessionState, runner: RunnerId, kind: RunnerKind) -> u32 {
    let mut depth = match kind {
        RunnerKind::SubAgent => 1,
        // The runtime owns no agents, so nothing asks it how deep they sit.
        RunnerKind::Conversation | RunnerKind::Workflow | RunnerKind::Runtime => 0,
    };
    let mut at = runner;
    for _ in 0..s.runners.len() {
        if at == s.root {
            break;
        }
        let Some(parent) = s.record(at).and_then(|rec| rec.parent) else {
            break;
        };
        let Some(up) = s.runner_of(parent) else {
            break;
        };
        if up == s.root || s.record(up).map(|rec| rec.kind) != Some(kind) {
            break;
        }
        depth += 1;
        at = up;
    }
    depth
}

// -- one agent's document ---------------------------------------------------

/// What an agent runs under: a step's own preset, a worker's settings as its
/// caller fixed them, a conversation's own.
///
/// Never the session's, which is the defect this shape closes: the session's
/// `AgentSettings` is the *first* step's, and the wrong answer for every other
/// agent in a run.
#[must_use]
pub fn settings_of(s: &SessionState, agent: AgentId) -> Option<&AgentSettings> {
    s.record(s.runner_of(agent)?)?.state.settings(agent)
}

/// What this agent was asked to do, and what it produced.
///
/// A conversation has neither: it is asked things one turn at a time, and what
/// it said is its transcript rather than a result.
#[must_use]
pub fn task_and_output(s: &SessionState, agent: AgentId) -> (Option<String>, Option<String>) {
    match s.runner_of(agent).and_then(|runner| s.record(runner)) {
        Some(rec) => rec.state.task_and_output(agent),
        None => (None, None),
    }
}

// -- the run ----------------------------------------------------------------

/// This session's run log, when the session *is* a run.
///
/// The root runner's, not any run the session happens to host: a workflow a
/// conversation invoked is that conversation's child, and `/workflow` asks
/// about the session.
#[must_use]
pub fn run_state(s: &SessionState) -> Option<WorkflowRunState> {
    s.record(s.root)?.state.run_log()
}

/// The graph this session's run was started from, snapshotted at creation.
///
/// Read from the runner rather than from the spec, which is what makes an
/// ad-hoc run — a graph with no definition row and no name — expressible at all.
#[must_use]
pub fn run_graph(s: &SessionState) -> Option<Arc<WorkflowRunSpec>> {
    s.record(s.root)?.state.run_graph()
}

// -- usage ------------------------------------------------------------------

/// This session's tokens: the total, and the split per agent.
///
/// The total is summed over [`SessionState::usage`], which is keyed by *model* —
/// a session-wide total that had to be added up from a per-agent map was a
/// per-agent fact wearing a session-shaped name. The split comes from each
/// runner's own bookkeeping: a conversation's and a worker's one total, a run's
/// per-step map.
///
/// **The keys change.** The old map was keyed `"main" | uuid`; every agent has a
/// real id now, so it is uuids throughout and nothing has to know which agent is
/// special. `main_agent` still answers for the session kinds that have a primary
/// conversation, and reads `None` on a run exactly as it did.
///
/// `main_agent`'s live half — the last turn's usage and the context size — is
/// not here and cannot be: those are the agent's own values, read by asking it,
/// and this function may not ask anything. The caller overlays them.
#[must_use]
pub fn usage_stats(s: &SessionState) -> SessionUsageStats {
    let mut agents: HashMap<String, UsageTotal> = HashMap::new();
    for rec in s.runners.values() {
        for (agent, spent) in rec.state.usage() {
            agents.insert(agent.to_string(), spent);
        }
    }
    let main = resolve(s, None).and_then(|agent| agents.get(&agent.to_string()).copied());
    SessionUsageStats {
        session_total: s
            .usage
            .values()
            .fold(UsageTotal::default(), |acc, spent| acc.combine(spent)),
        // A run has no primary conversation to report, and its spec says so —
        // the same source the shape this replaces read.
        main_agent: s
            .spec
            .as_ref()
            .and_then(SessionSpec::agent_settings)
            .map(|settings| AgentUsageEntry {
                model: settings.model.clone(),
                snapshot: AgentUsageSnapshot {
                    usage_total: main.unwrap_or_default(),
                    ..AgentUsageSnapshot::default()
                },
            }),
        agents,
    }
}

// -- forks ------------------------------------------------------------------

/// The forks this session holds, as the session list nests them.
///
/// Whole every time, so a copy built from the current value cannot drift the
/// way one built from deltas can.
#[must_use]
pub fn fork_rows(s: &SessionState) -> Vec<ForkRow> {
    s.runners
        .iter()
        .filter_map(|(id, rec)| {
            let mut row = rec.state.listing()?;
            // The two the fork cannot see: where it sits, and when the session
            // created it.
            row.parent = parent_of(s, *id);
            row.created_at_ms = rec.created_at_ms;
            Some(row)
        })
        .collect()
}

// -- addressing -------------------------------------------------------------

/// Resolve an agent selector to the agent it names.
///
/// `None`/`"main"` is the root runner's agent — or, when the root is a run, the
/// step in flight, because at most one step runs at a time and the definition
/// chose it, so there is nothing else an unaddressed request on a run could
/// mean. Without that, everything a caller can leave unaddressed — an answer
/// above all — resolved to nothing on a run and silently did nothing.
///
/// **`"main"` is a name, not an id.** It is no longer the session's own uuid: a
/// root conversation has a real agent id like everything else, and this is the
/// one place the name is turned into it.
///
/// A uuid resolves to that agent when this session hosts it, and to `None`
/// otherwise — one lookup, rather than the three-registry probe whose fixed
/// order made a fork of a fork read as a fork of a subagent.
#[must_use]
pub fn resolve(s: &SessionState, agent_id: Option<&str>) -> Option<AgentId> {
    match agent_id {
        None | Some(MAIN_AGENT_ID) => s.record(s.root)?.state.primary_agent(),
        Some(raw) => {
            let id = AgentId(uuid::Uuid::parse_str(raw).ok()?);
            s.runner_of(id).map(|_| id)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::runners::state::RunnerRecord;
    use crate::sessions::runners::{RunnerState, RunnerStatus, conversation, runtime, subagent};
    use crate::sessions::runners::{empty_settings, workflow};
    use crate::sessions::spec::{AgentSettings, SessionKind, SessionSpec};
    use crate::sessions::workflow::{
        StepStatus, TransitionSpec, WorkflowRunSpec, WorkflowRunStatus, WorkflowStepSpec,
        default_outcomes,
    };

    // -- fixtures -----------------------------------------------------------

    fn settings(model: &str) -> AgentSettings {
        AgentSettings {
            model: model.into(),
            ..empty_settings()
        }
    }

    /// Insert a runner with the slice a test built, and register its agents.
    /// The fold is the only writer in production; a test of the read side wants
    /// the state it reads, so it writes the record directly.
    fn put(
        s: &mut SessionState,
        kind: RunnerKind,
        parent: Option<AgentId>,
        state: RunnerState,
        agents: &[AgentId],
    ) -> RunnerId {
        let id = RunnerId::new_v4();
        if s.runners.is_empty() {
            s.root = id;
        }
        s.runners.insert(
            id,
            RunnerRecord {
                kind,
                parent,
                status: RunnerStatus::Running,
                state,
                created_at_ms: 100,
                ended_at_ms: 0,
            },
        );
        for agent in agents {
            s.agents.insert(*agent, id);
        }
        id
    }

    fn conversation(agent: AgentId) -> RunnerState {
        RunnerState::Conversation(conversation::State {
            agent,
            started: true,
            settings: settings("sonnet"),
            ..conversation::State::default()
        })
    }

    /// A session whose root is a conversation with a ready sandbox.
    fn session() -> (SessionState, AgentId) {
        let mut s = SessionState {
            spec: Some(spec(SessionKind::Agent {
                settings: settings("sonnet"),
            })),
            ..SessionState::default()
        };
        let main = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::Conversation,
            None,
            conversation(main),
            &[main],
        );
        put(&mut s, RunnerKind::Runtime, None, ready_runtime(), &[]);
        (s, main)
    }

    fn ready_runtime() -> RunnerState {
        let mut state = runtime::State::default();
        state.apply(
            &crate::sessions::runners::RunnerEvent::Runtime(runtime::Event::Succeeded { at_ms: 1 }),
            0,
        );
        RunnerState::Runtime(state)
    }

    fn phased_runtime(phase: runtime::Phase, detail: Option<&str>) -> RunnerState {
        RunnerState::Runtime(runtime::State {
            phase,
            detail: detail.map(str::to_string),
            provisioned_at_ms: None,
        })
    }

    fn worker(agent: AgentId, label: &str) -> subagent::State {
        subagent::State {
            agent,
            started: true,
            label: label.into(),
            task: "look at the last three runs".into(),
            settings: settings("sonnet"),
            ..subagent::State::default()
        }
    }

    fn spec(kind: SessionKind) -> SessionSpec {
        SessionSpec {
            kind,
            ..SessionSpec::for_vendor("mock")
        }
    }

    fn step(name: &str, model: &str, transitions: Vec<TransitionSpec>) -> WorkflowStepSpec {
        WorkflowStepSpec {
            name: name.into(),
            agent: "a".into(),
            prompt: format!("Do {name}."),
            outcomes: default_outcomes(),
            fields: Vec::new(),
            interactive: false,
            transitions,
            settings: settings(model),
        }
    }

    fn graph() -> WorkflowRunSpec {
        WorkflowRunSpec {
            workflow: "fix-bug".into(),
            start: "plan".into(),
            steps: vec![
                step(
                    "plan",
                    "gpt-5.6-terra",
                    vec![TransitionSpec {
                        to: "code".into(),
                        when: None,
                    }],
                ),
                step("code", "deepseek-v4-flash", Vec::new()),
            ],
            input: "the build is red".into(),
            max_steps: 100,
        }
    }

    /// A run whose first step has started, driven through the runner's own fold
    /// so the state under test is one the session could really have written.
    fn run_session() -> (SessionState, RunnerId, AgentId) {
        let mut s = SessionState {
            spec: Some(spec(SessionKind::Workflow {
                run: Arc::new(graph()),
            })),
            ..SessionState::default()
        };
        let run = RunnerId::new_v4();
        let mut state = workflow::State {
            run,
            graph: Arc::new(graph()),
            ..workflow::State::default()
        };
        let agent = workflow::step_agent_id(run, 0);
        state.apply(
            &crate::sessions::runners::RunnerEvent::Workflow(workflow::Event::StepStarted {
                index: 0,
                step: "plan".into(),
                agent,
                attempt: 1,
                from: None,
                via: None,
                input: "Do plan.".into(),
            }),
            500,
        );
        s.root = run;
        s.runners.insert(
            run,
            RunnerRecord {
                kind: RunnerKind::Workflow,
                parent: None,
                status: RunnerStatus::Running,
                state: RunnerState::Workflow(Box::new(state)),
                created_at_ms: 100,
                ended_at_ms: 0,
            },
        );
        s.agents.insert(agent, run);
        put(&mut s, RunnerKind::Runtime, None, ready_runtime(), &[]);
        (s, run, agent)
    }

    /// Fold one workflow event into the run this session is.
    fn advance(s: &mut SessionState, run: RunnerId, event: workflow::Event, at_ms: u64) {
        let rec = s.runners.get_mut(&run).expect("the run");
        Runner::apply(
            &mut rec.state,
            &crate::sessions::runners::RunnerEvent::Workflow(event),
            at_ms,
        );
    }

    fn turn(s: &mut SessionState, runner: RunnerId, event: conversation::Event, at_ms: u64) {
        let rec = s.runners.get_mut(&runner).expect("the conversation");
        Runner::apply(
            &mut rec.state,
            &crate::sessions::runners::RunnerEvent::Conversation(event),
            at_ms,
        );
    }

    fn of(agents: &[AgentEntry], agent: AgentId) -> &AgentEntry {
        agents
            .iter()
            .find(|a| a.id == agent.to_string())
            .unwrap_or_else(|| panic!("{agent} is not in the roster: {agents:?}"))
    }

    // -- the session's status -----------------------------------------------

    /// **The exhaustive table, with no catch-all arm.** A conversation's root
    /// agent has no lifecycle of its own, so every session status has to
    /// project onto one an agent can be in. A `_ =>` here is what let a session
    /// whose runtime never built report a `failed` status beside an `idle`
    /// agent, and it is why this asserts every variant rather than the
    /// interesting ones.
    #[test]
    fn every_session_status_is_a_state_its_main_agent_can_be_in() {
        for (status, expected) in [
            (SessionStatus::Provisioning, AgentStatus::Provisioning),
            (SessionStatus::Idle, AgentStatus::Idle),
            (SessionStatus::Running, AgentStatus::Running),
            (SessionStatus::AwaitingInput, AgentStatus::AwaitingInput),
            (SessionStatus::Finished, AgentStatus::Idle),
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
            assert_eq!(main_status(&status), expected, "{status:?}");
        }
    }

    /// The session's status is the root runner's own word for where it is, not
    /// a second variable beside it — and not an aggregate: a worker running in
    /// the background is not the session running.
    #[test]
    fn the_session_status_is_the_root_conversations_turn() {
        let (mut s, main) = session();
        let root = s.root;
        assert_eq!(session_status(&s), SessionStatus::Idle);

        let dig = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::SubAgent,
            Some(main),
            RunnerState::SubAgent(worker(dig, "dig")),
            &[dig],
        );
        assert_eq!(
            session_status(&s),
            SessionStatus::Idle,
            "a background worker is not the session working"
        );

        turn(&mut s, root, conversation::Event::TurnBegan, 1);
        assert_eq!(session_status(&s), SessionStatus::Running);
        turn(&mut s, root, conversation::Event::Asked, 2);
        assert_eq!(session_status(&s), SessionStatus::AwaitingInput);
    }

    /// A failed turn's reason has one source — the conversation's own
    /// `last_error`. It used to be carried into the event and dropped by the
    /// fold, so a person saw a session that had failed and nothing about why.
    #[test]
    fn a_failed_turn_carries_its_reason_into_the_session_status() {
        let (mut s, _main) = session();
        let root = s.root;
        turn(
            &mut s,
            root,
            conversation::Event::TurnFailed {
                error: "the model refused".into(),
            },
            1,
        );
        assert_eq!(
            session_status(&s),
            SessionStatus::Failed {
                reason: "the model refused".into()
            }
        );
    }

    /// **The sandbox overrides the root.** Nothing can run without one, so a
    /// conversation sitting idle under a runtime that is still coming up — or
    /// one that can never be built — must not read `idle`.
    #[test]
    fn the_sandbox_overrides_whatever_the_root_is_doing() {
        for (phase, expected) in [
            (runtime::Phase::Pending, SessionStatus::Provisioning),
            (runtime::Phase::Provisioning, SessionStatus::Provisioning),
            (
                runtime::Phase::Failed { terminal: false },
                SessionStatus::ProvisioningFailed {
                    reason: "no capacity".into(),
                },
            ),
            (
                runtime::Phase::Failed { terminal: true },
                SessionStatus::Unrecoverable {
                    reason: "no capacity".into(),
                },
            ),
        ] {
            let (mut s, main) = session();
            let sandbox = *s
                .runners
                .iter()
                .find(|(_, rec)| rec.kind == RunnerKind::Runtime)
                .map(|(id, _)| id)
                .expect("a sandbox");
            s.runners.get_mut(&sandbox).expect("a sandbox").state =
                phased_runtime(phase, Some("no capacity"));
            assert_eq!(session_status(&s), expected, "{phase:?}");
            // And it reaches the agent that *is* the session, which is the
            // whole point of the override.
            assert_eq!(
                agent_entry(&s, main).expect("the root agent").status,
                main_status(&expected),
                "{phase:?}"
            );
        }
    }

    /// A run that reached a terminal step is `Finished` — not `Idle`. A run
    /// that ran to completion and one that stopped part-way both rest, and
    /// telling them apart is the whole reason to look at a list of past runs.
    #[test]
    fn a_finished_run_is_the_sessions_status() {
        let (mut s, run, agent) = run_session();
        assert_eq!(session_status(&s), SessionStatus::Running);
        advance(
            &mut s,
            run,
            workflow::Event::StepConcluded {
                index: 0,
                output: serde_json::json!({"outcome": "success"}),
            },
            600,
        );
        advance(
            &mut s,
            run,
            workflow::Event::Finished {
                output: serde_json::json!({"outcome": "success"}),
            },
            700,
        );
        assert_eq!(session_status(&s), SessionStatus::Finished);
        assert_eq!(
            agent_entry(&s, agent).expect("the step").status,
            AgentStatus::Completed,
            "a finished run's step is what it concluded, not the session's rest"
        );
    }

    // -- the roster ---------------------------------------------------------

    /// A conversation lists the agent nothing spawned, so that every agent is
    /// reachable at one shape — and its workers beside it.
    #[test]
    fn a_conversation_lists_its_main_agent_and_its_subagents() {
        let (mut s, main) = session();
        let dig = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::SubAgent,
            Some(main),
            RunnerState::SubAgent(worker(dig, "research")),
            &[dig],
        );

        let agents = agent_roster(&s);
        assert_eq!(agents[0].id, main.to_string(), "the root leads: {agents:?}");
        assert_eq!(agents[0].label, None, "the root agent is not one of many");
        assert_eq!(of(&agents, dig).label.as_deref(), Some("research"));
        assert_eq!(agents.len(), 2);
    }

    /// A run has no main agent — it *is* its steps. Reporting one anyway meant
    /// a finished run answered with an agent that does not exist, permanently
    /// running, while the session's own status said `Idle` right beside it.
    #[test]
    fn a_run_lists_its_steps_and_no_main_agent() {
        let (s, _run, agent) = run_session();
        let agents = agent_roster(&s);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, agent.to_string());
        assert_eq!(agents[0].label.as_deref(), Some("plan"));
        assert_eq!(agents[0].status, AgentStatus::Running);
        assert!(
            agents.iter().all(|a| a.id != MAIN_AGENT_ID),
            "a run has no main agent: {agents:?}"
        );
    }

    /// What became of a step is the run log's answer, and a step that concluded
    /// says so. It used to be in no subagent tree and in nothing else an agent
    /// document read, so it fell through to a hardcoded `running` and reported
    /// it for ever — through reloads and cold tabs, long after the run ended.
    #[test]
    fn a_concluded_step_reports_that_it_concluded() {
        let (mut s, run, agent) = run_session();
        advance(
            &mut s,
            run,
            workflow::Event::StepConcluded {
                index: 0,
                output: serde_json::json!({"outcome": "p0", "note": "found it"}),
            },
            900,
        );
        let entry = agent_entry(&s, agent).expect("a step is an agent of its run");
        assert_eq!(entry.status, AgentStatus::Completed);
        assert_eq!(entry.started_at_ms, 500);
        assert_eq!(entry.ended_at_ms, 900, "a step that ended is stamped");
        let (task, output) = task_and_output(&s, agent);
        assert_eq!(task, None, "a step's brief is its definition's");
        assert!(
            output
                .expect("a concluded step reports what it concluded")
                .contains("found it")
        );
        // And the roster agrees, because it is the same read.
        assert_eq!(agent_roster(&s)[0].status, AgentStatus::Completed);
    }

    /// The defect this shape closes: opening a workflow step used to show the
    /// *start* step's settings, because the session carried the first step's
    /// preset as its own. Here `plan` runs terra and `code` runs flash.
    #[test]
    fn a_step_carries_its_own_settings_and_never_the_first_steps() {
        let (mut s, run, plan) = run_session();
        assert_eq!(
            settings_of(&s, plan).expect("the plan step").model,
            "gpt-5.6-terra"
        );
        advance(
            &mut s,
            run,
            workflow::Event::StepConcluded {
                index: 0,
                output: serde_json::json!({"outcome": "success"}),
            },
            600,
        );
        let code = workflow::step_agent_id(run, 1);
        advance(
            &mut s,
            run,
            workflow::Event::StepStarted {
                index: 1,
                step: "code".into(),
                agent: code,
                attempt: 1,
                from: Some(0),
                via: None,
                input: "Do code.".into(),
            },
            700,
        );
        s.agents.insert(code, run);
        assert_eq!(
            settings_of(&s, code).expect("the code step").model,
            "deepseek-v4-flash",
            "a step reads its own preset, not the run's first"
        );
        // And a worker spawned by that step runs under what its caller fixed,
        // which this reads off the worker rather than re-deriving.
        let helper = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::SubAgent,
            Some(code),
            RunnerState::SubAgent(subagent::State {
                settings: settings("deepseek-v4-flash"),
                ..worker(helper, "helper")
            }),
            &[helper],
        );
        assert_eq!(
            settings_of(&s, helper).expect("the helper").model,
            "deepseek-v4-flash"
        );
    }

    /// A worker's task and report are its own, and its status is read off the
    /// one field that also says whether a report is owed.
    #[test]
    fn a_worker_reports_its_task_its_label_and_what_it_produced() {
        let (mut s, main) = session();
        let dig = AgentId::new_v4();
        let runner = put(
            &mut s,
            RunnerKind::SubAgent,
            Some(main),
            RunnerState::SubAgent(worker(dig, "read the flake")),
            &[dig],
        );
        assert_eq!(
            agent_entry(&s, dig).expect("the worker").status,
            AgentStatus::Running
        );

        let rec = s.runners.get_mut(&runner).expect("the worker");
        rec.ended_at_ms = 4_000;
        Runner::apply(
            &mut rec.state,
            &crate::sessions::runners::RunnerEvent::SubAgent(subagent::Event::Concluded {
                output: "three flakes, all in setup".into(),
            }),
            0,
        );
        let entry = agent_entry(&s, dig).expect("the worker");
        assert_eq!(entry.status, AgentStatus::Completed);
        assert_eq!(entry.label.as_deref(), Some("read the flake"));
        assert_eq!(entry.started_at_ms, 100, "the record's own stamp");
        assert_eq!(entry.ended_at_ms, 4_000);
        assert_eq!(
            task_and_output(&s, dig),
            (
                Some("look at the last three runs".to_string()),
                Some("three flakes, all in setup".to_string())
            )
        );
    }

    // -- parent and depth ---------------------------------------------------

    /// **`parent` is `None` at the root.** Both `ForkParent::Main` and
    /// `SubAgentParent::Main` used to mean exactly this: rooted on whatever
    /// this session's "main" is, and either way not a peer of the thing being
    /// described. Anything else names the agent that created it.
    #[test]
    fn a_child_of_the_root_has_no_parent_and_anything_deeper_does() {
        let (mut s, main) = session();
        let first = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::SubAgent,
            Some(main),
            RunnerState::SubAgent(worker(first, "first")),
            &[first],
        );
        let second = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::SubAgent,
            Some(first),
            RunnerState::SubAgent(worker(second, "second")),
            &[second],
        );

        assert_eq!(agent_entry(&s, main).expect("the root").parent, None);
        assert_eq!(
            agent_entry(&s, first).expect("the first worker").parent,
            None,
            "a worker the root spawned is rooted, not parented"
        );
        assert_eq!(
            agent_entry(&s, second).expect("the second worker").parent,
            Some(first.as_uuid())
        );
    }

    /// **Depth stays per-tree** — decision 6, and a deliberate hold rather than
    /// an oversight. A worker tree starts at 1 and a fork tree at 0, which is
    /// what every client reads today; session-tree depth would renumber both.
    #[test]
    fn depth_is_counted_inside_each_tree_and_not_across_the_session() {
        let (mut s, main) = session();
        let first = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::SubAgent,
            Some(main),
            RunnerState::SubAgent(worker(first, "first")),
            &[first],
        );
        let second = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::SubAgent,
            Some(first),
            RunnerState::SubAgent(worker(second, "second")),
            &[second],
        );
        assert_eq!(agent_entry(&s, main).expect("the root").depth, 0);
        assert_eq!(agent_entry(&s, first).expect("first").depth, 1);
        assert_eq!(agent_entry(&s, second).expect("second").depth, 2);
    }

    /// The case the hold exists for: a worker under a step of a **nested** run.
    /// Session-tree depth would call it 2 — root conversation, run, worker —
    /// where the wire says 1, because its tree begins at the step that spawned
    /// it.
    #[test]
    fn a_worker_under_a_nested_runs_step_is_one_deep_not_two() {
        let (mut s, main) = session();
        let run = RunnerId::new_v4();
        let step = workflow::step_agent_id(run, 0);
        let mut state = workflow::State {
            run,
            graph: Arc::new(graph()),
            ..workflow::State::default()
        };
        state.apply(
            &crate::sessions::runners::RunnerEvent::Workflow(workflow::Event::StepStarted {
                index: 0,
                step: "plan".into(),
                agent: step,
                attempt: 1,
                from: None,
                via: None,
                input: "Do plan.".into(),
            }),
            500,
        );
        s.runners.insert(
            run,
            RunnerRecord {
                kind: RunnerKind::Workflow,
                parent: Some(main),
                status: RunnerStatus::Running,
                state: RunnerState::Workflow(Box::new(state)),
                created_at_ms: 100,
                ended_at_ms: 0,
            },
        );
        s.agents.insert(step, run);
        let helper = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::SubAgent,
            Some(step),
            RunnerState::SubAgent(worker(helper, "helper")),
            &[helper],
        );

        assert_eq!(
            agent_entry(&s, step).expect("the step").depth,
            0,
            "a step is chosen by the definition and roots its own tree"
        );
        assert_eq!(agent_entry(&s, helper).expect("the helper").depth, 1);
        assert_eq!(
            s.depth_of(s.runner_of(helper).expect("the helper's runner")),
            2,
            "session-tree depth disagrees, which is the whole reason this is \
             computed per tree"
        );
    }

    // -- forks --------------------------------------------------------------

    /// A conversation that carries a branch point, which is the whole of what
    /// makes it a fork.
    fn fork(agent: AgentId, source: AgentId, seeded: bool) -> conversation::State {
        conversation::State {
            agent,
            seed: Some(crate::sessions::runners::action::Branch {
                source,
                source_seq: 3,
                mode: crate::sessions::forks::ForkMode::Copy,
            }),
            seeded,
            started: seeded,
            title: Some("the flake".into()),
            settings: settings("sonnet"),
            ..conversation::State::default()
        }
    }

    /// **A fork is `Provisioning` until its branch point lands.** It is listed
    /// and addressable from the moment it is created — the reply to `/fork`
    /// names it — but it cannot run, and a client badges that wait.
    #[test]
    fn a_fork_is_provisioning_until_its_seed_lands() {
        let (mut s, main) = session();
        let branch = AgentId::new_v4();
        let runner = put(
            &mut s,
            RunnerKind::Conversation,
            Some(main),
            RunnerState::Conversation(fork(branch, main, false)),
            &[branch],
        );
        assert_eq!(
            agent_entry(&s, branch).expect("the fork").status,
            AgentStatus::Provisioning
        );

        turn(&mut s, runner, conversation::Event::Seeded, 900);
        assert_eq!(
            agent_entry(&s, branch).expect("the fork").status,
            AgentStatus::Idle
        );
    }

    /// A seed that failed is a failure, not a wait. Reported as `provisioning`
    /// it would sit in the list spinning for ever.
    #[test]
    fn a_fork_whose_seed_failed_reports_the_failure() {
        let (mut s, main) = session();
        let branch = AgentId::new_v4();
        let runner = put(
            &mut s,
            RunnerKind::Conversation,
            Some(main),
            RunnerState::Conversation(fork(branch, main, false)),
            &[branch],
        );
        turn(
            &mut s,
            runner,
            conversation::Event::SeedFailed {
                error: "the copy failed".into(),
            },
            900,
        );
        let entry = agent_entry(&s, branch).expect("the fork");
        assert_eq!(entry.status, AgentStatus::Failed);
        assert_eq!(entry.error.as_deref(), Some("the copy failed"));
    }

    /// The session list's rows: one per fork, nested by parent, ordered by
    /// their own last activity. A fork of the session's conversation has no
    /// parent; a fork of a fork names the one it came from.
    #[test]
    fn forks_are_rows_in_the_session_list_and_not_agents_of_the_session() {
        let (mut s, main) = session();
        let first = AgentId::new_v4();
        let one = put(
            &mut s,
            RunnerKind::Conversation,
            Some(main),
            RunnerState::Conversation(fork(first, main, true)),
            &[first],
        );
        turn(&mut s, one, conversation::Event::TurnEnded, 2_500);
        let second = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::Conversation,
            Some(first),
            RunnerState::Conversation(fork(second, first, true)),
            &[second],
        );

        let rows = fork_rows(&s);
        assert_eq!(rows.len(), 2);
        let row = rows
            .iter()
            .find(|r| r.id == first.as_uuid())
            .expect("the first fork");
        assert_eq!(row.parent, None, "a fork of the root is rooted");
        assert_eq!(row.title.as_deref(), Some("the flake"));
        assert_eq!(row.created_at_ms, 100);
        assert_eq!(row.last_activity_ms, 2_500);
        assert_eq!(row.status, AgentStatus::Idle);
        assert_eq!(
            rows.iter()
                .find(|r| r.id == second.as_uuid())
                .expect("the second fork")
                .parent,
            Some(first.as_uuid())
        );

        // And none of them is one of the session's agents: a client that nests
        // forks from these rows would otherwise draw each one twice.
        let agents = agent_roster(&s);
        assert!(
            agents.iter().all(|a| a.id != first.to_string()),
            "a fork was listed as an agent of its session: {agents:?}"
        );
        // Its own document still answers, which is how a person opens it.
        assert!(agent_entry(&s, first).is_some());
    }

    /// **A fork's row shows the name it gave itself.** That name lives in the
    /// title capability's own slice, because the capability owns the tool that
    /// set it — read it off the conversation's `title` field instead and every
    /// fork in the session list goes back to being nameless the moment this
    /// ships.
    #[test]
    fn a_fork_that_named_itself_is_listed_under_that_name() {
        use crate::sessions::runners::capabilities::{
            CapEvent, Capabilities, title::TitleCapability,
        };
        let (mut s, main) = session();
        let branch = AgentId::new_v4();
        let runner = put(
            &mut s,
            RunnerKind::Conversation,
            Some(main),
            RunnerState::Conversation(conversation::State {
                capabilities: Capabilities::new(vec![Box::new(TitleCapability::for_fork(
                    RunnerId::new_v4(),
                ))]),
                // What it was created with, which the name it chooses replaces.
                title: Some("fork of the flake".into()),
                ..fork(branch, main, true)
            }),
            &[branch],
        );
        assert_eq!(
            fork_rows(&s)[0].title.as_deref(),
            Some("fork of the flake"),
            "until it names itself, its row shows what it was branched as"
        );

        let rec = s.runners.get_mut(&runner).expect("the fork");
        Runner::apply(
            &mut rec.state,
            &crate::sessions::runners::RunnerEvent::Capability(CapEvent::Title(
                crate::sessions::runners::capabilities::title::Event::Set {
                    name: "the setup flake".into(),
                },
            )),
            0,
        );
        assert_eq!(fork_rows(&s)[0].title.as_deref(), Some("the setup flake"));
    }

    /// Depth inside the fork tree, which starts at 0 because its root is the
    /// session's own conversation.
    #[test]
    fn a_fork_of_a_fork_is_one_deep() {
        let (mut s, main) = session();
        let first = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::Conversation,
            Some(main),
            RunnerState::Conversation(fork(first, main, true)),
            &[first],
        );
        let second = AgentId::new_v4();
        put(
            &mut s,
            RunnerKind::Conversation,
            Some(first),
            RunnerState::Conversation(fork(second, first, true)),
            &[second],
        );
        assert_eq!(agent_entry(&s, first).expect("the first fork").depth, 0);
        assert_eq!(agent_entry(&s, second).expect("the second fork").depth, 1);
    }

    // -- addressing ---------------------------------------------------------

    /// **`"main"` is a name, not an id.** It resolves to the root runner's
    /// agent, which is a uuid like every other agent's — and no longer the
    /// session's own.
    #[test]
    fn main_names_the_root_conversations_agent() {
        let (s, main) = session();
        assert_eq!(resolve(&s, None), Some(main));
        assert_eq!(resolve(&s, Some(MAIN_AGENT_ID)), Some(main));
        assert_eq!(resolve(&s, Some(&main.to_string())), Some(main));
        assert_eq!(resolve(&s, Some(&AgentId::new_v4().to_string())), None);
        assert_eq!(resolve(&s, Some("not a uuid")), None);
    }

    /// On a run, an unaddressed request means the step in flight: at most one
    /// runs at a time and the definition chose it, so there is nothing else it
    /// could mean. Between steps there is nothing to address.
    #[test]
    fn main_on_a_run_is_the_step_in_flight() {
        let (mut s, run, agent) = run_session();
        assert_eq!(resolve(&s, None), Some(agent));
        assert_eq!(resolve(&s, Some(MAIN_AGENT_ID)), Some(agent));

        advance(
            &mut s,
            run,
            workflow::Event::StepConcluded {
                index: 0,
                output: serde_json::json!({"outcome": "success"}),
            },
            600,
        );
        assert_eq!(
            resolve(&s, None),
            None,
            "between steps there is no step in flight to address"
        );
        assert_eq!(
            resolve(&s, Some(&agent.to_string())),
            Some(agent),
            "the step itself is still addressable by id"
        );
    }

    // -- usage --------------------------------------------------------------

    fn spent(input: u64) -> UsageTotal {
        UsageTotal {
            input_tokens: input,
            ..UsageTotal::default()
        }
    }

    /// **The keys are uuids throughout.** The old map was keyed
    /// `"main" | uuid`; every agent has a real id now, so nothing has to know
    /// which one is special. The session's total is summed over the by-model
    /// map, not over this one.
    #[test]
    fn usage_is_recorded_per_agent_and_keyed_by_uuid() {
        let (mut s, main) = session();
        let dig = AgentId::new_v4();
        let runner = put(
            &mut s,
            RunnerKind::SubAgent,
            Some(main),
            RunnerState::SubAgent(worker(dig, "dig")),
            &[dig],
        );
        let root = s.root;
        Runner::apply(
            &mut s.runners.get_mut(&root).expect("the root").state,
            &crate::sessions::runners::RunnerEvent::Usage {
                agent: main,
                model: "sonnet".into(),
                spent: spent(10),
            },
            0,
        );
        Runner::apply(
            &mut s.runners.get_mut(&runner).expect("the worker").state,
            &crate::sessions::runners::RunnerEvent::Usage {
                agent: dig,
                model: "sonnet".into(),
                spent: spent(4),
            },
            0,
        );
        s.bank("sonnet".into(), &spent(14));

        let stats = usage_stats(&s);
        assert_eq!(stats.session_total.input_tokens, 14);
        assert_eq!(stats.agents[&main.to_string()].input_tokens, 10);
        assert_eq!(stats.agents[&dig.to_string()].input_tokens, 4);
        assert!(
            !stats.agents.contains_key(MAIN_AGENT_ID),
            "the primary agent is keyed by its own id now: {:?}",
            stats.agents.keys().collect::<Vec<_>>()
        );
        let entry = stats.main_agent.expect("a conversation has a main agent");
        assert_eq!(entry.model, "sonnet");
        assert_eq!(entry.snapshot.usage_total.input_tokens, 10);
    }

    /// A run has no main agent to report — the same answer the shape this
    /// replaces gave, and from the same source. Its per-step split is what the
    /// graph endpoint renders, and a run's total is a sum that cannot be taken
    /// apart again.
    #[test]
    fn a_run_reports_per_step_tokens_and_no_main_agent() {
        let (mut s, run, agent) = run_session();
        Runner::apply(
            &mut s.runners.get_mut(&run).expect("the run").state,
            &crate::sessions::runners::RunnerEvent::Usage {
                agent,
                model: "sonnet".into(),
                spent: spent(7),
            },
            0,
        );
        s.bank("sonnet".into(), &spent(7));

        let stats = usage_stats(&s);
        assert!(stats.main_agent.is_none());
        assert_eq!(stats.agents[&agent.to_string()].input_tokens, 7);
        assert_eq!(stats.session_total.input_tokens, 7);
    }

    // -- the run ------------------------------------------------------------

    /// The run log and the graph are the runner's own, which is what makes an
    /// ad-hoc run — no definition row, no name — expressible at all.
    #[test]
    fn the_run_reads_its_log_and_its_graph_off_the_root_runner() {
        let (mut s, run, _agent) = run_session();
        let state = run_state(&s).expect("this session is a run");
        assert_eq!(state.status, WorkflowRunStatus::Running);
        assert_eq!(state.steps.len(), 1);
        assert_eq!(state.steps[0].step, "plan");
        assert_eq!(state.steps[0].status, StepStatus::Running);
        assert_eq!(run_graph(&s).expect("this session is a run").start, "plan");

        advance(
            &mut s,
            run,
            workflow::Event::Failed {
                error: "step budget exhausted".into(),
            },
            900,
        );
        let state = run_state(&s).expect("this session is a run");
        assert_eq!(state.status, WorkflowRunStatus::Failed);
        assert_eq!(state.error.as_deref(), Some("step budget exhausted"));
        assert_eq!(
            session_status(&s),
            SessionStatus::Failed {
                reason: "step budget exhausted".into()
            }
        );
    }

    /// A conversation is not a run, and asking it for one answers nothing
    /// rather than an empty log that renders as a graph with no steps.
    #[test]
    fn a_conversation_has_no_run() {
        let (s, _main) = session();
        assert!(run_state(&s).is_none());
        assert!(run_graph(&s).is_none());
    }

    // -- nothing at all -----------------------------------------------------

    /// A state with no runners answers every question without panicking. It is
    /// a real state: a session journals its spec before its first runner, and a
    /// read can arrive in that window.
    #[test]
    fn an_empty_session_answers_everything() {
        let s = SessionState::default();
        assert_eq!(session_status(&s), SessionStatus::Idle);
        assert!(agent_roster(&s).is_empty());
        assert!(fork_rows(&s).is_empty());
        assert!(resolve(&s, None).is_none());
        assert!(agent_entry(&s, AgentId::new_v4()).is_none());
        assert!(settings_of(&s, AgentId::new_v4()).is_none());
        assert_eq!(task_and_output(&s, AgentId::new_v4()), (None, None));
        assert!(run_state(&s).is_none());
        assert_eq!(usage_stats(&s).session_total, UsageTotal::default());
    }
}
