//! One interactive session: the session state machine and the owner of
//! its agents.
//!
//! Three things are deliberately *not* here. The session does not know how a
//! runtime is provisioned, resumed or released — that is
//! [`RuntimeManager`](crate::runtime_manager::RuntimeManager), and no vendor
//! call ever runs on this mailbox. It does not decide when it is loaded or
//! unloaded — that is the supervisor. And it never resumes a turn by itself:
//! an interrupted assistant turn is over, while accepted user input is a
//! promise kept at the next turn boundary.
//!
//! What is left here is the actor itself: the roster of agents it hosts, the
//! spawner that builds one, the dispatch that hands each command to the
//! component that owns it, and the turn boundary where the components'
//! decisions are performed in order. Everything else lives beside it.
//!
//! One component per slice of the session — [`lifecycle`] the sandbox,
//! [`turns`] the session, [`run`] the workflow runs (the session's own and
//! every one its agents invoke), [`subagent`] the tree of delegated work,
//! [`reads`] the questions that wake nothing, [`hooks`] what plugins did,
//! [`core`] the session's own bookkeeping — over the vocabulary in [`types`]
//! and the state in [`crate::sessions::run_forest`], to the shape in
//! [`component`]. [`context`] is not one of them: it
//! assembles a turn on the *agent's* task rather than on this mailbox, which is
//! what keeps a thirty-second toolbox build from blocking a cancel.

use horsie_actor::ReplyTo;
mod component;
mod context;
mod core;
mod hooks;
mod lifecycle;
mod reads;
mod run;
mod sub_session;
mod subagent;
mod turns;
mod types;

pub use types::*;

use component::Component;
use core::SessionCore;
use hooks::{HookRouting, StopHookParent};
use lifecycle::RuntimeLifecycle;
use reads::Reads;
use run::WorkflowRuns;
use sub_session::SubSessions;
use subagent::SubAgents;
use turns::Turns;

use crate::agent_loop::{
    AgentActor, AgentCommand, AgentOutcome, AgentParams, AgentRunDef, AgentRuntimeContext, Incoming,
};
use crate::agent_loop::{
    CoreCommand as AgentCoreCommand, QueueCommand as AgentQueueCommand,
    RunCommand as AgentRunCommand,
};
use crate::projects::{ProjectRegistry, ProjectServices, resolve};
use crate::sessions::{
    addressing::{SessionEntityId, SessionInbox, SessionRef, SupervisorRef},
    orchestrator::{AgentAction, Delivery},
    run_forest::RunState,
    spec::{AgentSettings, ServerDeps, SessionKind, SessionSpec, SessionStatus},
    supervisor::{SessionSupervisorCommand, SubSessionRow},
};
use async_trait::async_trait;
use context::{SessionAgentKind, SessionContextProvider};
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_models::now_ms;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// The path segment and usage key naming a session's primary agent, as opposed
/// to a subagent's uuid. One spelling, shared by every agent-scoped route and
/// by the actor that resolves them.
pub const MAIN_AGENT_ID: &str = "main";

/// How long a cancel waits for the run to actually finish before giving up.
/// Cancellation is prompt (milliseconds); this is a backstop so a wedged run
/// can never hold the mailbox — and with it the Stop button — hostage.
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// One resident agent: its mailbox, and the provider that assembles its turns.
///
/// The two travel together because cancelling needs both — the mailbox to stop
/// the loop, and the provider to reach the runtime client the run in flight
/// already acquired. Keeping the provider on the actor instead, as a single
/// field, meant only the main agent had one, so cancelling a workflow step
/// silently skipped the sandbox half.
#[derive(Clone)]
struct ResidentAgent {
    actor: ActorRef<AgentCommand>,
    provider: Arc<SessionContextProvider>,
}

/// The agent actors a session hosts, one flat roster keyed by agent id — the
/// main agent under the session's own id, everything else under its uuid. What
/// an id *is* lives in the forest; residency is just residency.
struct SessionAgents {
    /// The main agent's key — the session id — so `AgentKey::Main` resolves
    /// without the actor in scope.
    main: Uuid,
    live: HashMap<Uuid, ResidentAgent>,
}

impl SessionAgents {
    fn new(main: Uuid) -> Self {
        Self {
            main,
            live: HashMap::new(),
        }
    }

    /// The session's primary agent, for the kinds that have one.
    fn main(&self) -> Option<&ResidentAgent> {
        self.live.get(&self.main)
    }

    fn sub(&self, id: Uuid) -> Option<&ResidentAgent> {
        self.live.get(&id)
    }

    /// The agent registered under `key`, if it is still resident.
    fn get(&self, key: AgentKey) -> Option<&ResidentAgent> {
        match key {
            AgentKey::Main => self.main(),
            AgentKey::Sub(id) | AgentKey::Step(id) | AgentKey::SubSession(id) => self.live.get(&id),
        }
    }

    fn insert(&mut self, id: Uuid, agent: ResidentAgent) {
        self.live.insert(id, agent);
    }

    /// Every resident agent with the id it is registered under — which is the
    /// id the forest resolves a runtime for.
    fn iter(&self) -> impl Iterator<Item = (Uuid, &ResidentAgent)> {
        self.live.iter().map(|(id, agent)| (*id, agent))
    }

    /// Forget one agent, handing it back so the caller can stop it. Only a sub
    /// session's delete uses this: every other agent lives as long as the
    /// session is loaded, and nothing else removes one on request.
    fn remove(&mut self, id: Uuid) -> Option<ResidentAgent> {
        self.live.remove(&id)
    }

    /// Every agent, emptying the set. Used when the session unloads.
    fn drain_all(&mut self) -> Vec<ResidentAgent> {
        self.live.drain().map(|(_, a)| a).collect()
    }
}

/// Everything that differs between the three kinds of agent a session spawns.
///
/// The rest — the runtime provider, the plugin library, the MCP and memory
/// services, the session's own mailbox — is identical for all three and lives
/// on the actor, which is why one spawner can serve them all.
struct AgentPlan {
    kind: SessionAgentKind,
    /// Whose settings this agent runs under: the session's, or a step's own
    /// preset. This is also where its model and thinking effort come from.
    settings: crate::sessions::spec::AgentSettings,
    /// What a step promises to return, and whether it may ask. Default for
    /// every other kind of agent.
    step_result: crate::sessions::session_actor::context::StepResultDef,
    /// The plugin-declared agent type a typed subagent runs as.
    agent_type: Option<String>,
    /// Where a sub session came from, for the section of its system prompt
    /// that says so. `None` for every other kind of agent.
    origin: Option<crate::sessions::session_actor::context::SubSessionOrigin>,
    /// The run to resolve this agent's runtime through, when the forest cannot
    /// yet resolve it by the agent's own id.
    ///
    /// Only a step needs it, and only because it is spawned in the same breath
    /// that journals the entry naming it. Asking the run instead is exact
    /// rather than approximate: a step never chooses a runtime of its own, so
    /// its run's answer *is* its answer.
    runtime_via: Option<crate::sessions::run_forest::RunId>,
}

pub struct SessionActor {
    id: Uuid,
    /// Whose session this is. Not in the persistence id — a session's log is
    /// keyed by its uuid alone — but a recipe is handed it, because resolving
    /// the account's wiring is the one thing a session cannot do from its own
    /// id.
    account: crate::projects::ProjectId,
    /// What this session is. `None` until its own log says, or until the
    /// `Create` that brought this actor into being is handled.
    ///
    /// It is *not* safe to assume that command comes first: addressing a
    /// session is what materialises its actor, so anything holding this id can
    /// arrive ahead of it. `handle_command` refuses everything else while this
    /// is `None`, which is what makes the readers below sound.
    spec: Option<SessionSpec>,
    /// Where this account's bundle is resolved from. A shard recipe is
    /// synchronous, so nothing below can be handed in at construction.
    projects: Weak<ProjectRegistry>,
    /// This account's bundle, resolved at recovery. See [`Self::services`].
    services: Option<Arc<ProjectServices>>,
    /// This session's supervisor, given at construction.
    ///
    /// A *name* with a warm cache rather than a handle to one mailbox, so a
    /// supervisor that stops and comes back is reached through the same
    /// reference and this session is told nothing. That is what makes handing
    /// it down cost nothing — and a session built on a host that never saw the
    /// request creating it is still handed one, because the recipe resolves
    /// the reference for the whole supervisor type rather than for an
    /// instance.
    supervisor: SupervisorRef,
    /// The agent actors this session hosts, resident for as long as this actor
    /// is loaded.
    agents: SessionAgents,
    /// The last status this actor told the supervisor, so an unchanged one is
    /// not re-sent. `None` until it has reported once, which is why a freshly
    /// loaded session always reports.
    last_reported: Option<SessionStatus>,
    /// The same, for the sub session roster. Empty until it has reported once
    /// — which costs nothing, because a session with no sub sessions reports
    /// none either way.
    last_reported_sub_sessions: Vec<SubSessionRow>,
    /// The agent-run rows this actor last wrote to the index, so a batch that
    /// changed nothing writes nothing.
    ///
    /// The whole point of holding it: an agent's log grows on every turn but
    /// its *row* — who it is, what preset, whether it is over — changes twice
    /// in its life. Comparing against this is what turns "write the roster on
    /// every persisted batch" into those two writes.
    last_indexed_runs: Vec<crate::agent_runs::AgentRunRow>,
    /// Which of this session's agents were parked on questions the last time
    /// the projection looked, so leaving that state can be *noticed*.
    ///
    /// The transition is the whole signal. An inbox row is written when the
    /// agent parks and has to be settled when it stops being parked, and only
    /// a before-and-after can say which agents those are — a snapshot of who is
    /// awaiting input now cannot distinguish "still parked" from "was never
    /// parked".
    last_awaiting_input: Vec<String>,
    /// The last index write this actor spawned, so the next one can queue
    /// behind it.
    ///
    /// A chain rather than a channel: there is at most one of these in flight
    /// at a time and two per agent run in total, so a consumer task with a
    /// mailbox would be machinery for a queue that is almost always empty.
    indexing: Option<tokio::task::JoinHandle<()>>,
}

/// How `agent` reaches its sandbox, as `state` resolves it right now.
///
/// The three answers are kept distinct all the way to the agent: a runtime it
/// can address, a deliberate absence of one, or an answer the session has not
/// given yet. Collapsing the last two is what let an agent spawned during a
/// create run its first turn with no tools at all.
pub(in crate::sessions::session_actor) fn runtime_binding(
    deps: &ServerDeps,
    state: &SessionState,
    session: Uuid,
    agent: Uuid,
) -> crate::sessions::session_actor::context::AgentRuntimeBinding {
    runtime_binding_for(deps, state, session, state.forest.runtime_of_agent(agent))
}

/// The same, from a choice already resolved by some other walk.
pub(in crate::sessions::session_actor) fn runtime_binding_for(
    deps: &ServerDeps,
    state: &SessionState,
    session: Uuid,
    choice: crate::sessions::run_forest::RuntimeChoice,
) -> crate::sessions::session_actor::context::AgentRuntimeBinding {
    use crate::sessions::session_actor::context::AgentRuntimeBinding;
    match state.runtime_of_choice(choice) {
        AgentRuntime::On(runtime, rec) => AgentRuntimeBinding::On(Box::new(
            deps.runtimes.provider(
                session.to_string(),
                runtime.to_string(),
                // The provision this run speaks to. One that has never provisioned
                // has none, and the empty string is what the acquisition will fail
                // on rather than silently addressing some other sandbox.
                rec.provisioning
                    .at_ms()
                    .map(|at| at.to_string())
                    .unwrap_or_default(),
                // A create is still outstanding. The journal is the only thing
                // that knows, and it has to say so: a substrate that has not
                // reported the object yet is indistinguishable from one with
                // nothing there, and the difference is between waiting for a
                // runtime and declaring it gone.
                matches!(rec.provisioning, ProvisioningState::InFlight { .. }),
                rec.env.vendor.clone(),
                rec.env.clone(),
            ),
        )),
        AgentRuntime::Without => AgentRuntimeBinding::Without,
        AgentRuntime::Pending => AgentRuntimeBinding::Pending,
    }
}

impl SessionActor {
    pub fn new(
        entity: SessionEntityId,
        supervisor: SupervisorRef,
        projects: Weak<ProjectRegistry>,
    ) -> Self {
        Self {
            id: entity.session,
            account: entity.project,
            spec: None,
            projects,
            services: None,
            supervisor,
            agents: SessionAgents::new(entity.session),
            last_reported: None,
            last_reported_sub_sessions: Vec::new(),
            last_indexed_runs: Vec::new(),
            last_awaiting_input: Vec::new(),
            indexing: None,
        }
    }

    /// The journal identity of a session: kind `"session"`, id = the uuid.
    ///
    /// The account is deliberately absent. A session's log was found by this
    /// key before anything was addressed by an account, and putting one in the
    /// key now would orphan every log ever written.
    pub fn persistence_id_for(session_id: Uuid) -> PersistenceId {
        PersistenceId::new("session", session_id.to_string())
    }

    /// This account's bundle.
    ///
    /// Expects rather than handles: recovery resolves it, and recovery finishes
    /// before the first command is handled, so a `None` here is a broken actor
    /// lifecycle rather than a case with an answer.
    #[expect(
        clippy::expect_used,
        reason = "recovery runs before any command, so this cannot be None"
    )]
    fn services(&self) -> &Arc<ProjectServices> {
        self.services
            .as_ref()
            .expect("a session handles no command before recovery has resolved its account")
    }

    /// What this session runs on.
    pub(super) fn deps(&self) -> &ServerDeps {
        &self.services().deps
    }

    /// What this session is.
    ///
    /// Expects for the same reason [`Self::services`] does, one step further
    /// on: `handle_command` refuses every command but the one that records a
    /// spec while there is none, so nothing below it can reach this unset.
    #[expect(
        clippy::expect_used,
        reason = "a session is told what it is before anything else can reach it"
    )]
    pub(super) fn spec(&self) -> &SessionSpec {
        self.spec
            .as_ref()
            .expect("a session is told what it is before anything else can reach it")
    }

    /// This session's own mailbox, as the thing that reaches it.
    pub(super) fn me(&self, ctx: &ActorContext<SessionInbox>) -> SessionRef {
        SessionRef::new(ctx.self_ref(), self.account.clone(), self.id, None)
    }

    /// Answer a command that reached this session before it was told what it
    /// is, without touching any state that is not there yet.
    ///
    /// Answered rather than dropped wherever the command has a reply: "no such
    /// session" is the truthful answer — nothing has created this one — and it
    /// is the answer the caller can act on. A dropped reply says the same thing
    /// far less clearly, and a panic said it by killing the actor and whatever
    /// else was still arriving for it.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "refusing is the safe default, so a command added later should \
                  fall here rather than have to be classified"
    )]
    fn refuse_until_told(&self, cmd: SessionCommand) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            SessionCommand::Turn(TurnCommand::UserMessage { reply, .. }) => {
                let _ = reply.send(Err(crate::sessions::UserMessageError::NotFound));
            }
            SessionCommand::Turn(TurnCommand::Stop { reply, .. }) => {
                let _ = reply.send(Err("no such session".to_string()));
            }
            _other => {
                // The rest carry no reply this layer can build, so the caller
                // learns the same thing from a closed channel. Logged because
                // reaching a session that does not exist is worth seeing.
                tracing::warn!(
                    session = %self.id,
                    "a command reached a session before it was told what it is"
                );
            }
        }
        CommandEffect::none()
    }

    /// Take up a spec, start the agents it calls for, and put right whatever
    /// the state it is handed says was interrupted.
    ///
    /// Two callers, and the pair is the whole of how a session learns what it
    /// is: recovery, from what its log already says, and `Create`, for a
    /// session whose log is empty because it was created a moment ago. Both go
    /// through here so that a run started for the first time and one resumed
    /// after a restart take exactly the same path.
    pub(super) async fn adopt(
        &mut self,
        spec: SessionSpec,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) {
        self.spec = Some(spec);
        match &self.spec().kind {
            SessionKind::Workflow { .. } => {
                // A run has no main agent. Step actors, like subagent actors,
                // stay cold: they spawn on demand for a history read, a retry,
                // or the next step a boundary picks.
            }
            SessionKind::Agent { .. } => self.spawn_main_agent(ctx, state),
        }
        // Each component repairs itself. A self-send rather than direct work,
        // because neither caller may write here — recovery must not persist at
        // all, and `Create` is already returning an effect of its own — so
        // anything that needs to journal arrives as an ordinary command, down
        // the same path a live one would take.
        let repairs: Vec<SessionCommand> = [
            RuntimeLifecycle::on_load(state),
            SubAgents::on_load(state),
            WorkflowRuns::on_load(state),
            SubSessions::on_load(state),
            Turns::on_load(state),
        ]
        .into_iter()
        .flatten()
        .collect();
        let me = self.me(ctx);
        for cmd in repairs {
            let _ = me.tell(cmd).await;
        }
        // Loading is not a transition, but it is the first moment anyone can
        // learn this status: a page already watching hears nothing otherwise.
        //
        // Unconditional, repairs or not. This used to be skipped whenever a
        // repair was queued, on the grounds that the repair reports the status
        // it lands on — but `SubAgentCommand::Reconcile` persists an event and
        // reports nothing, so a session whose only repair was an interrupted
        // subagent loaded and said nothing at all. A repair that does persist
        // reports again from `on_events_persisted`, with the state it landed
        // on, and a report that changes nothing is dropped by the supervisor.
        self.report_status(state).await;
    }

    /// Tell the supervisor the status this session's journal just folded.
    ///
    /// Read off the folded state rather than announced at each transition, and
    /// called only where the state has settled — after a persisted batch, and
    /// once at load. So what the supervisor records is by construction what the
    /// session journaled, and the two cannot drift apart by a missed call site.
    ///
    /// That drift was the shape of the old code: thirteen `report(LITERAL)`
    /// calls, each one duplicating the status the event on the very next line
    /// was about to fold.
    /// Only on a change, because this is called after *every* persisted batch
    /// and most batches move nothing: a tool result, a subagent's outcome, a
    /// usage row. The supervisor drops an unchanged report anyway, but it is
    /// still a message on the mailbox that also serves every read.
    ///
    /// `None` at load, so a freshly recovered session always reports once —
    /// which is the moment anyone can first learn its status. Tell the
    /// supervisor what sub sessions this session now holds, so the session
    /// list can nest them without loading it.
    ///
    /// The whole roster every time, and the supervisor drops a report that
    /// changed nothing. A projection built from the current value cannot drift
    /// the way one built from deltas can — and `List` is documented to load
    /// nothing, so a sidebar that could not read this from the registry could
    /// not show sub sessions at all without waking every session that has one.
    async fn report_sub_sessions(&mut self, state: &SessionState) {
        if !state.forest.has_sub_sessions() && self.last_reported_sub_sessions.is_empty() {
            return;
        }
        let sub_sessions: Vec<SubSessionRow> = state
            .forest
            .sub_sessions()
            .map(|(id, rec)| SubSessionRow {
                id,
                parent: state
                    .forest
                    .owner_of_agent(id)
                    .and_then(|(_, e)| e.parent)
                    .filter(|pid| *pid != self.id),
                title: rec.title.clone(),
                status: rec.status,
                created_at_ms: state
                    .forest
                    .owner_of_agent(id)
                    .map(|(_, e)| e.created_at_ms)
                    .unwrap_or_default(),
                last_activity_ms: rec.last_activity_ms,
            })
            .collect();
        if sub_sessions == self.last_reported_sub_sessions {
            return;
        }
        self.last_reported_sub_sessions = sub_sessions.clone();
        let _ = self
            .supervisor
            .tell(SessionSupervisorCommand::SubSessionsChanged {
                id: self.id.to_string(),
                sub_sessions,
            })
            .await;
    }

    /// This session's rows for the agent-run index, derived from the roster.
    ///
    /// Pure, so the diffing below and the load-time reconcile agree by
    /// construction rather than by two people keeping them in step.
    fn agent_run_rows(&self, state: &SessionState) -> Vec<crate::agent_runs::AgentRunRow> {
        self.agent_roster(state)
            .into_iter()
            .map(|entry| crate::agent_runs::AgentRunRow {
                session_id: self.id.to_string(),
                agent_id: entry.id,
                preset: entry.preset,
                status: entry.status.as_wire().to_string(),
                started_at: i64::try_from(entry.started_at_ms).unwrap_or(i64::MAX),
                // Zero means "no end", which is what a `SubAgentView` carries
                // for a run still going *and* for a main agent that will never
                // have one. NULL is the honest column value for both.
                ended_at: (entry.ended_at_ms != 0)
                    .then(|| i64::try_from(entry.ended_at_ms).unwrap_or(i64::MAX)),
            })
            .collect()
    }

    /// Run an index write off this actor's task, behind whatever it last
    /// spawned.
    ///
    /// Off the mailbox because the index is a read model and the session is
    /// not: making a turn wait for a row to land trades the thing that matters
    /// for the thing that does not. Chained rather than free-running because
    /// the rows describe a moving state — "this run ended" arriving before
    /// "this run started" would leave the index claiming the opposite of what
    /// happened.
    fn spawn_index_write<F>(&mut self, write: F)
    where
        F: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let previous = self.indexing.take();
        let session = self.id;
        self.indexing = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            if let Err(e) = write.await {
                // Not fatal, and deliberately not retried here. A lost write
                // costs a reader one run's visibility until this session is
                // next loaded, where the reconcile puts it back.
                tracing::warn!(error = %e, %session, "agent-run index write failed");
            }
        }));
    }

    /// Write what changed about this session's agent runs into the index.
    ///
    /// Only the difference, which is what keeps this to two writes per run: an
    /// agent's row is written when it first appears and again when it reaches a
    /// terminal state, and a session that runs for an hour without gaining or
    /// finishing one writes nothing at all.
    ///
    /// Removals are not handled here — an agent leaving a roster is not an
    /// event this can see the far side of, and inferring a deletion from a
    /// shorter list would delete every row whenever a reload started the
    /// comparison from empty. `reconcile_agent_runs` at load is what makes the
    /// index agree again.
    fn index_agent_runs(&mut self, state: &SessionState) {
        let rows = self.agent_run_rows(state);
        if rows == self.last_indexed_runs {
            return;
        }
        let changed: Vec<crate::agent_runs::AgentRunRow> = rows
            .iter()
            .filter(|row| !self.last_indexed_runs.contains(row))
            .cloned()
            .collect();
        // Updated here rather than after the write lands: this is what *this
        // actor has decided to write*, and the spawned chain preserves that
        // order. Waiting for the write would mean holding the mailbox, which
        // is the whole thing being avoided.
        self.last_indexed_runs = rows;
        if changed.is_empty() {
            return;
        }
        let Some(services) = self.services.clone() else {
            return;
        };
        self.spawn_index_write(async move { services.agent_runs.record(&changed).await });
    }

    /// Make the index agree with this session's own state, wholesale.
    ///
    /// Runs once at load. Covers both directions the incremental write cannot:
    /// a row lost to a crash between the persist and the write, and a row for
    /// an agent this session no longer hosts.
    fn reconcile_agent_runs(&mut self, state: &SessionState) {
        let rows = self.agent_run_rows(state);
        let Some(services) = self.services.clone() else {
            return;
        };
        self.last_indexed_runs = rows.clone();
        let id = self.id.to_string();
        self.spawn_index_write(async move { services.agent_runs.reconcile(&id, &rows).await });
    }

    /// Drop this session's rows from the index, because the session is going.
    ///
    /// The one index write that *is* awaited. The actor stops immediately
    /// after, so a spawned task would be racing its own session's disappearance
    /// — and an entry outliving its transcript is the failure this prevents.
    /// Drop this session's claim on its artifacts, and delete any that nothing
    /// else still references.
    ///
    /// Artifacts are content-addressed, so the same image can be attached in
    /// several sessions and one row of bytes serves them all. That is why this
    /// releases a *use* rather than deleting outright: the bytes go only when
    /// the last session holding them does. Without it, a hosted deployment
    /// keeps every image of every deleted session for ever.
    async fn release_artifacts(&mut self) {
        let Some(services) = self.services.clone() else {
            return;
        };
        if let Err(e) = services
            .artifacts
            .release_session(&services.project, &self.id.to_string())
            .await
        {
            // Logged rather than propagated: the session is going away either
            // way, and a failure here leaks storage rather than corrupting
            // anything.
            tracing::warn!(session = %self.id, error = %e, "could not release artifacts");
        }
    }

    async fn forget_agent_runs(&mut self) {
        self.last_indexed_runs.clear();
        // Drain what is queued first, or a `record` still in flight lands after
        // the delete and resurrects the rows it was about to remove.
        if let Some(previous) = self.indexing.take() {
            let _ = previous.await;
        }
        let Some(services) = self.services.clone() else {
            return;
        };
        if let Err(e) = services
            .agent_runs
            .forget_session(&self.id.to_string())
            .await
        {
            tracing::warn!(error = %e, session = %self.id, "failed to drop agent runs");
        }
    }

    /// Put an agent's newly-parked questions in the person's inbox.
    ///
    /// Off the mailbox and behind whatever else this actor queued, like every
    /// other index write: the inbox is a read model and the session is not, so
    /// making a park wait for a row to land trades the thing that matters for
    /// the thing that does not. Idempotent at the store, so a replay writes
    /// nothing twice.
    fn index_inbox_asks(&mut self, agent: uuid::Uuid, asks: &[crate::agent_loop::AskedQuestion]) {
        let agent_id = match agent == self.id {
            true => MAIN_AGENT_ID.to_string(),
            false => agent.to_string(),
        };
        let rows = ask_rows(&self.id.to_string(), &agent_id, asks);
        if rows.is_empty() {
            return;
        }
        let Some(services) = self.services.clone() else {
            return;
        };
        self.spawn_index_write(async move {
            services
                .user_inbox
                .record_asks(&rows, crate::user_inbox::now_ms_i64())
                .await
        });
    }

    /// The agents this session currently has parked on questions.
    ///
    /// Addressed the way a route addresses them — `"main"` or a uuid — because
    /// that is what an inbox row holds, and a set compared in one vocabulary
    /// and written in another agrees with itself only by luck.
    fn awaiting_input_agents(&self, state: &SessionState) -> Vec<String> {
        let mut awaiting: Vec<String> = self
            .agent_roster(state)
            .into_iter()
            .filter(|entry| entry.status == AgentStatus::AwaitingInput)
            .map(|entry| entry.id)
            .collect();
        // A workflow step is not in that list even when it is the thing that
        // asked. `apply_asked` parks the *run* for a step — the step stays
        // `Running`, because it is still the current one and the answer resumes
        // it — so the roster, which reports each agent's own status, is right
        // and silent about it. The agent holding the question has to be read
        // off the run instead.
        if state.status() == SessionStatus::AwaitingInput
            && let Some(step) = state.forest.current_root_step_agent()
        {
            let step = step.to_string();
            if !awaiting.contains(&step) {
                awaiting.push(step);
            }
        }
        awaiting
    }

    /// Settle the inbox rows of every agent that has just stopped waiting.
    ///
    /// The counterpart to the row written when the agent parked. It closes and
    /// never names an outcome, because the outcome is not a fact this end
    /// holds: what the session can see is that the agent moved on, which is
    /// exactly `Closed` — "settled, reason unknown". The one path that *does*
    /// know is the answer handler, and its mark is allowed to land either side
    /// of this one (see `settle_agent_asks`).
    ///
    /// This is what catches the case nothing else names: a person who typed a
    /// new message instead of answering. The agent is resumed with a "not
    /// answered" result for every parked call and carries on; without this the
    /// inbox would still be offering to answer a question that no longer holds
    /// anything.
    fn settle_departed_asks(&mut self, state: &SessionState) {
        let awaiting = self.awaiting_input_agents(state);
        if awaiting == self.last_awaiting_input {
            return;
        }
        let departed: Vec<String> = self
            .last_awaiting_input
            .iter()
            .filter(|id| !awaiting.contains(id))
            .cloned()
            .collect();
        self.last_awaiting_input = awaiting;
        if departed.is_empty() {
            return;
        }
        let Some(services) = self.services.clone() else {
            return;
        };
        let session = self.id.to_string();
        self.spawn_index_write(async move {
            for agent in departed {
                services
                    .user_inbox
                    .settle_agent_asks(
                        &session,
                        &agent,
                        &[],
                        horsie_models::inbox::InboxState::Closed,
                        crate::user_inbox::now_ms_i64(),
                    )
                    .await?;
            }
            Ok(())
        });
    }

    /// Make the inbox agree with this session's own state.
    ///
    /// Runs once at load, and closes what the incremental writes could not: a
    /// row still `Open` against an agent that stopped waiting while nothing was
    /// watching.
    ///
    /// Derived from the roster alone — no agent is asked anything. Reading a
    /// parked agent's questions would mean resolving it, and resolving a
    /// subagent or a sub session *spawns* it, so a reconcile that did the
    /// thorough thing would wake every parked agent this session hosts on every
    /// single load. A session that cannot go quiet never offloads, and that is
    /// a much larger fault than the one it would be fixing.
    fn reconcile_inbox(&mut self, state: &SessionState) {
        let awaiting = self.awaiting_input_agents(state);
        self.last_awaiting_input = awaiting.clone();
        let Some(services) = self.services.clone() else {
            return;
        };
        let session = self.id.to_string();
        self.spawn_index_write(async move {
            services
                .user_inbox
                .reconcile_session(&session, &awaiting, crate::user_inbox::now_ms_i64())
                .await
        });
    }

    /// Drop this session's inbox messages, because the session is going.
    ///
    /// Awaited, like `forget_agent_runs` and for the same reason: the actor
    /// stops immediately after, and a message offering to answer a question in
    /// a session that no longer exists is worse than no message.
    async fn forget_inbox(&mut self) {
        self.last_awaiting_input.clear();
        if let Some(previous) = self.indexing.take() {
            let _ = previous.await;
        }
        let Some(services) = self.services.clone() else {
            return;
        };
        if let Err(e) = services
            .user_inbox
            .forget_session(&self.id.to_string())
            .await
        {
            tracing::warn!(error = %e, session = %self.id, "failed to drop inbox messages");
        }
    }

    async fn report_status(&mut self, state: &SessionState) {
        let status = state.status();
        if self.last_reported.as_ref() == Some(&status) {
            return;
        }
        self.last_reported = Some(status.clone());
        let _ = self
            .supervisor
            .tell(SessionSupervisorCommand::SessionStatusChanged {
                id: self.id.to_string(),
                status,
            })
            .await;
    }

    /// Re-point every resident agent at the runtime this state resolves for
    /// it.
    ///
    /// Called whenever a runtime record changes, because an agent outlives the
    /// answer: the main agent is spawned at load, when its session has not yet
    /// asked for a runtime, and a sub session's agent may exist before the
    /// runtime it asked for is built. Re-resolving rather than patching the
    /// one that changed keeps this correct for a re-provision too, where the
    /// incarnation moves under an agent that is already resident.
    fn repoint_agent_runtimes(&self, state: &SessionState) {
        for (id, resident) in self.agents.iter() {
            let binding = runtime_binding(self.deps(), state, self.id, id);
            *resident
                .provider
                .runtimes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = binding;
        }
    }

    /// Spawn one of this session's agents and register it.
    ///
    /// The single spawner for all three kinds. Cheap and runtime-free: the
    /// provider, toolbox and system prompt are resolved per run, on the run's
    /// own task, so this costs nothing but a mailbox.
    ///
    /// Main is registered as the session's primary; a subagent and a step both
    /// register under their own id, which is also the id they journal under.
    fn spawn_agent(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        plan: AgentPlan,
    ) -> Option<ResidentAgent> {
        // Taken from the account's registry rather than owned here, because the
        // channels have to outlive this actor: unloading an idle session must
        // leave a reader waiting rather than disconnecting it.
        let revisions = self.services().revisions.of(&self.id.to_string());
        // A subagent and a step journal under their own id; the main agent
        // journals under the session's, because its transcript *is* the
        // session's. The revision channel follows the same split.
        //
        // `publishing` rather than `for_agent`: this node is the one running
        // the agent, so every move of its counter has to reach whichever node
        // is answering readers of it — routinely a different one, since a
        // session and its supervisor are placed independently.
        let (journal_id, revision) = match plan.kind {
            SessionAgentKind::Main => (self.id, revisions.publishing(MAIN_AGENT_ID)),
            SessionAgentKind::Sub(id)
            | SessionAgentKind::Step(id)
            | SessionAgentKind::SubSession(id) => (id, revisions.publishing(&id.to_string())),
        };
        // Its name under this session, and the id it is addressed by.
        let name = match plan.kind {
            SessionAgentKind::Main => MAIN_AGENT_ID.to_string(),
            SessionAgentKind::Sub(id)
            | SessionAgentKind::Step(id)
            | SessionAgentKind::SubSession(id) => id.to_string(),
        };
        let key = plan.kind.agent_key();
        // Which runtime *this agent* runs on, resolved through the forest: its
        // own session's, or the sub session's that branched it, or none at all.
        // Read here rather than per acquisition so every call in one run
        // addresses the same sandbox even if the session re-provisions beneath
        // it — the same reason the incarnation is bound once.
        //
        // The main agent is spawned at load, before its session has asked for
        // a runtime, so this is routinely `Pending`. It stays behind a lock
        // for exactly that reason: `runtime_bindings` re-points it when the
        // create lands.
        let choice = self.runtime_choice_for(state, &plan);
        let runtimes = runtime_binding_for(self.deps(), state, self.id, choice);
        let provider = Arc::new(SessionContextProvider {
            runtimes: Mutex::new(runtimes),
            registry: self.deps().provider_registry.clone(),
            mcp: self.deps().mcp.clone(),
            memory: self.deps().memory.clone(),
            services: Some(self.services().clone()),
            step_result: plan.step_result.clone(),
            session_id: self.id,
            kind: plan.kind,
            agent_type: plan.agent_type,
            origin: plan.origin,
            unattended: self.spec().is_unattended(),
            session: self.me(ctx),
            plugins: self.spec().plugins.clone(),
            plugin_library: self.deps().plugins.clone(),
            last_client: Mutex::new(None),
            settings: plan.settings.clone(),
        });
        let mut params = AgentParams::from_def(&AgentRunDef {
            system_prompt: None,
            max_iterations: plan.settings.max_iterations,
            max_retries: Some(plan.settings.max_retries),
            allowed_tools: plan.settings.allowed_tools.clone(),
        });
        params.interactive = true;
        // A step is the only agent that owes a structured result, and the only
        // one for which a turn ending with plain text is not an answer.
        params.requires_result = matches!(plan.kind, SessionAgentKind::Step(_));
        // A subagent's conclusion is a report its parent consumes once, so it
        // may not conclude over delegated work still in flight — it parks, and
        // reports when its whole subtree is done. A session's text is an
        // answer to a person, not a report, so main and sub sessions keep
        // concluding per turn.
        params.park_on_outstanding_work = matches!(plan.kind, SessionAgentKind::Sub(_));
        params.thinking_effort = plan
            .settings
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let agent_ctx = AgentRuntimeContext {
            // Gated on the model, once, inside `artifact_source`: a source that
            // resolves nothing is how a text-only model is served, so no
            // provider below this holds a vision flag or can forget to check
            // one.
            artifacts: self.deps().artifact_source(&plan.settings.model),
            context_provider: provider.clone(),
            revision,
            parent: StopHookParent::wrap(self.me(ctx), key, provider.clone()),
            journal_id,
            // Computed from the state this spawn was decided against, never
            // remembered: an agent built after the runtime landed starts ready,
            // and one built before it starts waiting. Changes reach it as the
            // `Runtime` records it is sent anyway.
            ready: RuntimeLifecycle::ready_on(state.runtime_of_choice(choice))
                && state.fatal.is_none(),
        };
        // A child of this session, named by the id it journals under — `main`
        // for the primary agent, the node id for a subagent or a step. Created
        // rather than spawned anonymously so it has a path under this
        // session's, which is what makes it stop with the session and makes
        // two callers racing to reach one agent get one actor over its
        // journal.
        let actor = match ctx.actor_of(&name, ctx.persistent(AgentActor::new(agent_ctx, params))) {
            Ok(actor) => actor,
            Err(e) => {
                tracing::error!(session = %self.id, agent = %name, error = %e, "could not start the agent");
                return None;
            }
        };
        let resident = ResidentAgent { actor, provider };
        let id = match plan.kind {
            SessionAgentKind::Main => self.id,
            SessionAgentKind::Sub(id)
            | SessionAgentKind::Step(id)
            | SessionAgentKind::SubSession(id) => id,
        };
        self.agents.insert(id, resident.clone());
        Some(resident)
    }

    /// The session's primary agent, spawned once at load.
    fn spawn_main_agent(&mut self, ctx: &ActorContext<SessionInbox>, state: &SessionState) {
        let Some(settings) = self.spec().agent_settings() else {
            // Only an agent session has a main agent; `adopt` gates this call
            // on the kind.
            return;
        };
        self.spawn_agent(
            ctx,
            state,
            AgentPlan {
                kind: SessionAgentKind::Main,
                settings: settings.clone(),
                step_result: Default::default(),
                agent_type: None,
                origin: None,
                runtime_via: None,
            },
        );
    }

    /// Resolve an agent selector to its actor: `None`/`"main"` for the primary
    /// agent, else the id of a step or a subagent. A cold node — one the
    /// persisted state knows about with no actor since this session loaded — is
    /// spawned on demand, so reading a finished agent works exactly like
    /// reading a live one.
    ///
    /// A run has no primary agent, so an unaddressed selector means the step in
    /// flight there. Without that, everything a caller can leave unaddressed —
    /// an answer above all — resolved to nothing on a run and silently did
    /// nothing.
    fn resolve_agent(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        agent_id: Option<&str>,
    ) -> Option<(AgentKey, ActorRef<AgentCommand>)> {
        match agent_id {
            None | Some(MAIN_AGENT_ID) => {
                match state.forest.current_root_step_agent() {
                    // At most one of the root run's steps runs at a time, and
                    // the definition chose it, so there is nothing else an
                    // unaddressed request on a run could mean.
                    Some(step) => self.resolve_step(state, ctx, step),
                    None => self.agent().map(|actor| (AgentKey::Main, actor)),
                }
            }
            Some(raw) => {
                let id = Uuid::parse_str(raw).ok()?;
                let key = self.agent_key_of(state, id)?;
                let actor = match key {
                    AgentKey::Step(_) => return self.resolve_step(state, ctx, id),
                    AgentKey::SubSession(_) => self.spawn_sub_session_actor(ctx, state, id)?,
                    AgentKey::Sub(_) => {
                        if let Some(agent) = self.agents.sub(id) {
                            return Some((key, agent.actor.clone()));
                        }
                        // The type comes off the record, not from the caller: a
                        // cold node woken to answer a read must run as what it
                        // was spawned as.
                        let agent_type = state.forest.sub(id)?.agent_type.clone();
                        self.spawn_sub_agent_actor(ctx, state, id, agent_type)?
                    }
                    AgentKey::Main => self.agent()?,
                };
                Some((key, actor))
            }
        }
    }

    /// What kind of agent `id` names here, answered by the forest in one
    /// lookup: the entry that hosts an agent says what it is.
    pub(super) fn agent_key_of(&self, state: &SessionState, id: Uuid) -> Option<AgentKey> {
        let (_, entry) = state.forest.owner_of_agent(id)?;
        Some(match &entry.state {
            RunState::Main(_) => AgentKey::Main,
            RunState::Sub(_) => AgentKey::Sub(id),
            RunState::Workflow(_) => AgentKey::Step(id),
            RunState::SubSession(_) => AgentKey::SubSession(id),
        })
    }

    /// One step agent, spawned if it is not resident. `None` when the id names
    /// no execution in any run's log.
    ///
    /// The log, not the roster, is what identifies a step. Spawning on demand
    /// is what keeps a finished run's step transcripts readable — the roster is
    /// empty after a reload, and every agent-scoped read comes through here.
    fn resolve_step(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        id: Uuid,
    ) -> Option<(AgentKey, ActorRef<AgentCommand>)> {
        let (run, index) = state.forest.step_of_agent(id)?;
        if let Some(agent) = self.agents.sub(id) {
            return Some((AgentKey::Step(id), agent.actor.clone()));
        }
        let step = state.forest.workflow(run)?.run.get(index)?.step.clone();
        Some((
            AgentKey::Step(id),
            self.spawn_step_agent(ctx, state, run, id, &step)?,
        ))
    }

    fn agent(&self) -> Option<ActorRef<AgentCommand>> {
        self.agents.main().map(|a| a.actor.clone())
    }

    /// The settings an agent runs under, resolved from where it sits in the
    /// forest: the main agent and sub sessions use the agent session's
    /// settings, a step uses its own run's preset, and a subagent inherits
    /// from the nearest ancestor that owns settings — the step, sub session or
    /// main agent it ultimately runs under. `None` when the key names no agent
    /// here.
    pub(super) fn effective_settings<'a>(
        &'a self,
        state: &'a SessionState,
        key: AgentKey,
    ) -> Option<&'a AgentSettings> {
        match key {
            AgentKey::Main | AgentKey::SubSession(_) => self.spec().agent_settings(),
            AgentKey::Step(id) => {
                let (run, index) = state.forest.step_of_agent(id)?;
                let w = state.forest.workflow(run)?;
                let name = &w.run.get(index)?.step;
                w.graph.step(name).map(|step| &step.settings)
            }
            AgentKey::Sub(id) => {
                // Walk up to the nearest non-subagent ancestor, bounded like
                // every other walk over recovered data.
                let mut at = id;
                for _ in 0..=state.forest.depth_of_agent(id).unwrap_or(0) {
                    let (_, entry) = state.forest.owner_of_agent(at)?;
                    match &entry.state {
                        RunState::Sub(_) => at = entry.parent?,
                        RunState::Main(_) | RunState::SubSession(_) => {
                            return self.spec().agent_settings();
                        }
                        RunState::Workflow(_) => {
                            return self.effective_settings(state, AgentKey::Step(at));
                        }
                    }
                }
                // Deeper than its own depth: the chain is broken.
                let (_, entry) = state.forest.owner_of_agent(at)?;
                match &entry.state {
                    RunState::Sub(_) => None,
                    RunState::Main(_) | RunState::SubSession(_) => self.spec().agent_settings(),
                    RunState::Workflow(_) => self.effective_settings(state, AgentKey::Step(at)),
                }
            }
        }
    }

    /// The settings a spawn or an invocation by `caller` runs under: the
    /// caller's own, so a step's spawns inherit the step's settings and its
    /// cap.
    pub(super) fn effective_settings_of_agent<'a>(
        &'a self,
        state: &'a SessionState,
        caller: Uuid,
    ) -> Option<&'a AgentSettings> {
        let key = self.agent_key_of(state, caller)?;
        self.effective_settings(state, key)
    }

    /// Cancel one agent's run and wait for it to actually be over.
    ///
    /// Two halves, in this order. The sandbox is told to abandon what it is
    /// running first, using the client that agent's own `provide()` cached —
    /// asking the manager for a fresh one would round-trip the vendor on this
    /// mailbox, and a vendor mid-tool-call cannot answer a lifecycle request
    /// until the call it is relaying resolves. Then the agent's loop is
    /// stopped.
    ///
    /// Waiting matters: the caller is about to record a turn boundary, and a
    /// run still winding down can still append to the agent journal.
    async fn cancel_agent(&mut self, key: AgentKey) {
        let Some(agent) = self.agents.get(key).cloned() else {
            return;
        };
        if let Some(client) = agent.provider.cached_client() {
            client.cancel_in_flight().await;
        }
        let (tx, rx) = oneshot::channel();
        let _ = agent
            .actor
            .tell(AgentCommand::Run(AgentRunCommand::Cancel {
                ack: Some(ReplyTo::from_sender(tx)),
            }))
            .await;
        if tokio::time::timeout(CANCEL_TIMEOUT, rx).await.is_err() {
            tracing::warn!(
                session = %self.id,
                "cancelled run did not finish within {CANCEL_TIMEOUT:?}; proceeding"
            );
        }
    }

    /// Cancel whatever this session is running: every step in flight — the
    /// root run's and any invoked run's — and the main agent's turn. A run
    /// used to be skipped here entirely, so deleting one mid-step left its
    /// sandbox call running.
    async fn cancel_in_flight(&mut self, state: &SessionState) {
        let steps = state.forest.in_flight_steps();
        if steps.is_empty() {
            self.cancel_agent(AgentKey::Main).await;
            return;
        }
        for (_, _, agent) in steps {
            self.cancel_agent(AgentKey::Step(agent)).await;
        }
    }

    /// Carry out one orchestrator decision and return the events that record
    /// it.
    ///
    /// No turn ever begins here any more: an agent owns its queue and decides
    /// when that queue becomes a turn. What is left is delivery — putting a
    /// finished child's result where the agent that asked for it will find it.
    async fn perform(
        &mut self,
        action: AgentAction,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionDomainEvent> {
        match action {
            AgentAction::Deliver(delivery) => self.deliver(delivery, state, ctx).await,
            AgentAction::StartStep(step) => self.start_step(step, state, ctx).await,
            AgentAction::Finish { run, output } => vec![SessionDomainEvent::RunFinished {
                at_ms: now_ms(),
                run: run.0,
                output,
            }],
            AgentAction::Fail { run, error } => vec![SessionDomainEvent::RunFailed {
                at_ms: now_ms(),
                run: run.0,
                error,
            }],
        }
    }

    /// Put a finished subagent's result in the queue of the agent that is owed
    /// it, and record that it has been sent.
    ///
    /// Tell-then-persist: a crash between the enqueue and this write leaves the
    /// result still owed, so the next boundary re-delivers it. Delivery is
    /// at-least-once in that window (the parent may see a result twice), never
    /// lost — `spawn_agent`'s stricter persist-then-spawn is the deliberate
    /// exception, because an untracked agent is worse than a duplicate.
    ///
    /// Skipped, not failed, when the agent cannot be reached: the result stays
    /// owed and the next boundary tries again.
    async fn deliver(
        &mut self,
        delivery: Delivery,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionDomainEvent> {
        let Delivery { to, child, part } = delivery;
        let Some(agent) = self.reach(to, state, ctx) else {
            return Vec::new();
        };
        if agent
            .tell(AgentCommand::Queue(AgentQueueCommand::Enqueue {
                item: Incoming::SubAgent {
                    id: child.0.to_string(),
                    part: Box::new(part),
                },
                ack: None,
            }))
            .await
            .is_err()
        {
            return Vec::new();
        }
        // What was sent decides what records the send: a subagent's report
        // marks its node, a run's report marks its run entry.
        let recorded = match state.forest.entry(child).map(|e| &e.state) {
            Some(RunState::Workflow(_)) => SessionDomainEvent::RunNotified {
                at_ms: now_ms(),
                run: child.0,
            },
            Some(RunState::Sub(_) | RunState::Main(_) | RunState::SubSession(_)) | None => {
                SessionDomainEvent::SubAgentNotified {
                    at_ms: now_ms(),
                    id: child.0,
                }
            }
        };
        vec![recorded]
    }

    /// The mailbox of one of this session's agents, spawning a cold subagent's
    /// actor on demand. `None` when nothing under that key exists.
    fn reach(
        &mut self,
        key: AgentKey,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Option<ActorRef<AgentCommand>> {
        if let Some(agent) = self.agents.get(key) {
            return Some(agent.actor.clone());
        }
        match key {
            // A cold node reached for the first time since load. The type comes
            // off the record, not from the caller: a node woken to receive a
            // result must run as what it was spawned as.
            AgentKey::Sub(id) => {
                let agent_type = state.forest.sub(id)?.agent_type.clone();
                self.spawn_sub_agent_actor(ctx, state, id, agent_type)
            }
            // A step's actor spawns on demand from the run log, the same way a
            // cold subagent's does: a boundary can owe a result to a step whose
            // actor has since been unloaded.
            AgentKey::Step(id) => self.resolve_step(state, ctx, id).map(|(_, actor)| actor),
            // A cold sub session, woken to be read or messaged. Nothing comes
            // off a record here the way a subagent's type does: a sub session
            // runs under the session's own settings, like the session it
            // branched from.
            AgentKey::SubSession(id) => state
                .forest
                .sub_session(id)
                .is_some()
                .then(|| self.spawn_sub_session_actor(ctx, state, id))?,
            // Spawned at load, so it is either resident or this session is a
            // run and has none.
            AgentKey::Main => None,
        }
    }

    /// Everything the orchestrator wants started at this turn boundary,
    /// performed in order, each seeing the state the previous one produced.
    ///
    /// Every turn boundary routes through here — without that, a result owed to
    /// a subagent parent strands the moment no further subagent outcome can
    /// arrive (every node terminal), since an outcome was previously the only
    /// flush trigger.
    /// Whether any component has work in flight, so the session must not
    /// unload. This is what keeps a forty-minute tool call from being unloaded
    /// out from under itself.
    fn busy(&self, state: &SessionState) -> bool {
        RuntimeLifecycle::busy(state)
            || Turns::busy(state)
            || WorkflowRuns::busy(state)
            || SubAgents::busy(state)
            || SubSessions::busy(state)
    }

    /// Everything every component wants started, given the state as it now is.
    ///
    /// A concatenation, not a negotiation: each component returns only work it
    /// owns, so there is nothing to reconcile. Subagent wakes go first — a
    /// parent waiting on its children is work already in flight, and the next
    /// turn or step can wait a boundary.
    fn next_actions(&self, state: &SessionState) -> Vec<AgentAction> {
        // Nothing the *session* drives starts before its own runtime exists.
        // A sub session's turn is gated separately, by the `ready` flag its
        // agent is built with, because it may be waiting on a different one.
        //
        // Walked from the root *entry* rather than from an agent with the
        // session's id: a workflow session has no main agent, so there is no
        // such agent to ask, and asking anyway answered "unresolved" forever.
        if !RuntimeLifecycle::ready_on(self.session_runtime(state)) {
            return Vec::new();
        }
        [
            SubAgents::actions(state),
            Turns::actions(state),
            WorkflowRuns::actions(state),
        ]
        .concat()
    }

    async fn flush_then_drain(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionDomainEvent> {
        // To a fixpoint, not one pass: an action can make the next one
        // startable within the same boundary. A step concluding finishes its
        // run, and the finish is what makes the run's report owed — stopping
        // after one round would strand that report until the next boundary.
        // Every action extinguishes its own trigger when folded (a delivery
        // marks notified, a started step is in flight, a finished run is
        // terminal), so the rounds run dry; the cap is a backstop for a fold
        // that breaks that promise, not a budget.
        let mut events = Vec::new();
        let mut next = state.clone();
        for _ in 0..8 {
            let mut produced_any = false;
            for action in self.next_actions(&next) {
                let produced = self.perform(action, &next, ctx).await;
                produced_any = produced_any || !produced.is_empty();
                for e in &produced {
                    next = Self::apply_event(next, e.clone());
                }
                events.extend(produced);
            }
            if !produced_any {
                break;
            }
        }
        events
    }

    /// Persist `events`, having first let the boundary they create start
    /// whatever became startable.
    ///
    /// The fold is local and one step early — the same fold the runtime will
    /// apply when it persists — because the drain has to see the state these
    /// events produce, not the one they were decided against. Every turn
    /// boundary ends here, which is what stops a result owed to a subagent
    /// parent stranding when no further outcome can arrive.
    async fn persist_and_advance(
        &mut self,
        state: &SessionState,
        mut events: Vec<SessionDomainEvent>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        let next = events
            .iter()
            .cloned()
            .fold(state.clone(), Self::apply_event);
        events.extend(self.flush_then_drain(&next, ctx).await);
        CommandEffect::persist(events)
    }

    /// Route one agent's outcome to the component that owns what it means.
    ///
    /// The one command routed by *identity* rather than by variant: the same
    /// `Concluded` means "the turn is over", "this step's output picks the next
    /// step", or "tell the parent its child is done", depending only on which
    /// agent sent it. Answering the two non-ending reports first is what lets
    /// each of those three read the outcome as a turn that ended, rather than
    /// re-answering variants that mean the same thing to all of them.
    async fn on_agent_outcome(
        &mut self,
        state: &SessionState,
        outcome: AgentOutcome,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        // Peeked before the split, which deliberately discards the questions:
        // `TurnEnd` is the session's smaller vocabulary and the questions belong
        // to the agent that asked them. This is the one moment the session sees
        // their text, and the inbox needs it — so it is taken here rather than
        // by widening a type whose whole point is to be narrow.
        let asked = if let AgentOutcome::Asked { agent, asks } = &outcome {
            Some((*agent, asks.clone()))
        } else {
            None
        };
        let (who, end) = match TurnEnd::split(outcome) {
            Ok(pair) => pair,
            Err(boxed) => match *boxed {
                (
                    agent,
                    NotAnEnd::Usage {
                        usage_total,
                        context_tokens,
                        efficiency,
                    },
                ) => {
                    let agent_id = match agent == self.id {
                        true => MAIN_AGENT_ID.to_string(),
                        false => agent.to_string(),
                    };
                    return CommandEffect::persist(vec![SessionDomainEvent::UsageRecorded {
                        at_ms: now_ms(),
                        agent_id,
                        usage_total,
                        context_tokens,
                        efficiency,
                    }]);
                }
                (agent, NotAnEnd::Started) => {
                    return self.on_agent_started(state, agent).await;
                }
                (
                    _,
                    NotAnEnd::SeedSummary {
                        sub_sessions,
                        result,
                    },
                ) => {
                    return SubSessions::handle(
                        self,
                        state,
                        SubSessionCommand::Summarised {
                            sub_sessions,
                            result,
                        },
                        ctx,
                    )
                    .await;
                }
            },
        };
        // One lookup: the entry that hosts the agent says what its outcome
        // means. No ordering between registries to get right, because there is
        // one registry.
        let key = self.agent_key_of(state, who);
        if let Some((agent, asks)) = asked {
            // Everything but a subagent. A subagent has no user to ask — its
            // `Asked` is turned into a failure below — so a row for one would
            // be an inbox entry offering to unblock something that is already
            // finished, and nothing would ever settle it.
            if !matches!(key, Some(AgentKey::Sub(_))) {
                self.index_inbox_asks(agent, &asks);
            }
        }
        match key {
            Some(AgentKey::Main) => self.on_main_outcome(state, end, ctx).await,
            Some(AgentKey::Step(_)) => match state.forest.step_of_agent(who) {
                Some((run, index)) => self.on_step_outcome(state, run, index, who, end, ctx).await,
                None => CommandEffect::none(),
            },
            Some(AgentKey::SubSession(_)) => {
                self.on_sub_session_outcome(state, who, end, ctx).await
            }
            Some(AgentKey::Sub(_)) => self.on_sub_agent_outcome(state, who, end, ctx).await,
            None => {
                tracing::warn!(agent = %who, "outcome from an agent nothing hosts; ignored");
                CommandEffect::none()
            }
        }
    }

    /// One of this session's agents drained its queue into a turn.
    ///
    /// The session used to know this because it was the thing that started the
    /// turn. It is told now, and what it records depends only on which agent it
    /// was: the session's own status for the main agent, a tree node going back
    /// to work for a subagent. A step announces itself through `StepStarted`
    /// when the run picks it, so there is nothing to add here.
    async fn on_agent_started(
        &mut self,
        state: &SessionState,
        who: Uuid,
    ) -> CommandEffect<SessionDomainEvent> {
        match self.agent_key_of(state, who) {
            Some(AgentKey::Main) => CommandEffect::persist(vec![SessionDomainEvent::TurnBegan {
                at_ms: now_ms(),
                agent: who,
            }]),
            Some(AgentKey::Sub(_)) => {
                CommandEffect::persist(vec![SessionDomainEvent::SubAgentRunning {
                    at_ms: now_ms(),
                    id: who,
                }])
            }
            // A sub session's own status, and only its own: the session's
            // belongs to the main agent, and a sub session answering a
            // question is not the session working.
            Some(AgentKey::SubSession(_)) => {
                CommandEffect::persist(vec![SessionDomainEvent::SubSessionStatusChanged {
                    at_ms: now_ms(),
                    id: who,
                    status: AgentStatus::Running,
                }])
            }
            // A step announces itself through `StepStarted` when the run picks
            // it, so there is nothing to add here.
            Some(AgentKey::Step(_)) | None => CommandEffect::none(),
        }
    }

    /// Whether `agent` may start a turn at all: the runtime *it* runs on is
    /// there, and the session is not terminal. The whole of what an agent's own
    /// drain gate cannot answer for itself.
    ///
    /// Per agent since a session may own several runtimes: a sub session
    /// waiting on a machine of its own must not hold back the session it
    /// branched from, and an agent with no runtime at all is never waiting.
    /// Which runtime the session itself drives work on.
    ///
    /// Walked from the root entry, so it answers for a workflow session — whose
    /// root is a run, not a main agent — as readily as for an agent session.
    fn session_runtime<'a>(&self, state: &'a SessionState) -> AgentRuntime<'a> {
        state.runtime_of_choice(
            state
                .forest
                .runtime_of_run(crate::sessions::run_forest::RunId(self.id)),
        )
    }

    /// Which runtime the agent in `plan` will run on.
    ///
    /// A step is resolved through its run rather than through itself, because
    /// the event that puts it in the forest persists *after* this call — and a
    /// step never chooses a runtime of its own, so its run's answer is exact
    /// rather than a stand-in.
    fn runtime_choice_for(
        &self,
        state: &SessionState,
        plan: &AgentPlan,
    ) -> crate::sessions::run_forest::RuntimeChoice {
        let agent = plan.kind.agent_id(self.id);
        match plan.runtime_via {
            Some(run) if !state.forest.is_known_agent(agent) => state.forest.runtime_of_run(run),
            _ => state.forest.runtime_of_agent(agent),
        }
    }

    /// Stop every agent this session hosts. Used when the session unloads.
    async fn stop_agents(&mut self) {
        for agent in self.agents.drain_all() {
            // Cancel first: a stopped mailbox makes the run task's next persist
            // fail, but an in-flight tool call would run to completion first.
            let _ = agent
                .actor
                .tell(AgentCommand::Run(AgentRunCommand::Cancel { ack: None }))
                .await;
            let _ = agent
                .actor
                .tell(AgentCommand::Core(AgentCoreCommand::Shutdown))
                .await;
        }
    }
}

#[async_trait]
impl EventSourcedActor for SessionActor {
    type Command = SessionInbox;
    type Event = SessionDomainEvent;
    type State = SessionState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.id)
    }

    fn initial_state() -> SessionState {
        SessionState::default()
    }

    fn apply_event(mut state: SessionState, event: SessionDomainEvent) -> SessionState {
        match event {
            SessionDomainEvent::RuntimeRequested { .. }
            | SessionDomainEvent::ProvisioningStarted { .. }
            | SessionDomainEvent::ProvisioningProgress { .. }
            | SessionDomainEvent::ProvisioningSucceeded { .. }
            | SessionDomainEvent::ProvisioningFailed { .. } => {
                RuntimeLifecycle::apply(&mut state, &event)
            }
            SessionDomainEvent::TurnBegan { .. }
            | SessionDomainEvent::AskRecorded { .. }
            | SessionDomainEvent::TurnEnded { .. }
            | SessionDomainEvent::TurnFailed { .. }
            | SessionDomainEvent::TurnStopped { .. }
            | SessionDomainEvent::TurnInterrupted { .. }
            | SessionDomainEvent::SessionFailed { .. } => Turns::apply(&mut state, &event),
            SessionDomainEvent::RunCreated { .. }
            | SessionDomainEvent::StepStarted { .. }
            | SessionDomainEvent::StepConcluded { .. }
            | SessionDomainEvent::StepFailed { .. }
            | SessionDomainEvent::StepCancelled { .. }
            | SessionDomainEvent::RunFinished { .. }
            | SessionDomainEvent::RunFailed { .. }
            | SessionDomainEvent::RunNotified { .. } => WorkflowRuns::apply(&mut state, &event),
            SessionDomainEvent::SubAgentSpawned { .. }
            | SessionDomainEvent::SubAgentRunning { .. }
            | SessionDomainEvent::SubAgentCompleted { .. }
            | SessionDomainEvent::SubAgentFailed { .. }
            | SessionDomainEvent::SubAgentNotified { .. } => SubAgents::apply(&mut state, &event),
            SessionDomainEvent::SubSessionCreated { .. }
            | SessionDomainEvent::SubSessionSeeded { .. }
            | SessionDomainEvent::SubSessionStatusChanged { .. }
            | SessionDomainEvent::SubSessionTurnEnded { .. } => {
                SubSessions::apply(&mut state, &event)
            }
            SessionDomainEvent::UsageRecorded { .. }
            | SessionDomainEvent::AgentDeleted { .. }
            | SessionDomainEvent::SpecRecorded { .. }
            | SessionDomainEvent::Renamed { .. } => SessionCore::apply(&mut state, &event),
        }
        state
    }

    /// Write what just became durable into the agents' own transcripts, so a
    /// reader sees a lifecycle entry where it happened rather than having to
    /// infer it from the session's status.
    ///
    /// And report the status the batch left behind. Here rather than at each
    /// transition because here the write is already durable: the supervisor's
    /// copy can lag the journal, never lead it.
    async fn on_events_persisted(&mut self, events: &[SessionDomainEvent], state: &SessionState) {
        self.record_lifecycle(events, state).await;
        self.report_sub_sessions(state).await;
        self.report_status(state).await;
        self.index_agent_runs(state);
        self.settle_departed_asks(state);
    }

    /// Every command arrives addressed to a session, and this is the one place
    /// that reads the address: the shard already routed by it, so what is left
    /// below is the command it was wrapped around.
    async fn handle_command(
        &mut self,
        state: &SessionState,
        cmd: SessionInbox,
        ctx: &mut ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        // A session that has not been told what it is cannot serve anything:
        // every handler below reads its spec, and reading a missing one used to
        // take the actor down mid-create.
        //
        // Reachable whenever something addresses a session that does not exist
        // — a stale id from a client, and in a cluster a read that races the
        // `Create` making one, because addressing a session is what
        // materialises its actor. `Create` is the one command that answers
        // this state, since it is the one that ends it.
        if self.spec.is_none()
            && !matches!(cmd.cmd, SessionCommand::Core(CoreCommand::Create { .. }))
        {
            return self.refuse_until_told(cmd.cmd);
        }
        match cmd.cmd {
            SessionCommand::Lifecycle(c) => RuntimeLifecycle::handle(self, state, c, ctx).await,
            SessionCommand::Turn(c) => Turns::handle(self, state, c, ctx).await,
            SessionCommand::Run(c) => WorkflowRuns::handle(self, state, c, ctx).await,
            SessionCommand::SubAgent(c) => SubAgents::handle(self, state, c, ctx).await,
            SessionCommand::SubSession(c) => SubSessions::handle(self, state, c, ctx).await,
            SessionCommand::Read(c) => Reads::handle(self, state, c, ctx).await,
            SessionCommand::Hooks(c) => HookRouting::handle(self, state, c, ctx).await,
            SessionCommand::Core(c) => SessionCore::handle(self, state, c, ctx).await,
            // The one command routed by identity rather than by variant: which
            // agent sent the outcome decides which component answers it.
            SessionCommand::AgentOutcome(outcome) => {
                self.on_agent_outcome(state, outcome, ctx).await
            }
        }
    }

    /// Loading a session resolves its account, spawns its agent and repairs a
    /// turn the process died in. It calls no vendor, starts no run, and drains
    /// nothing — an interrupted assistant turn is over, and queued user
    /// messages wait for the next turn the user starts.
    ///
    /// The account is resolved *here* because a shard recipe is synchronous and
    /// building a bundle is not. This runs before the first command, so
    /// everything else may take it for granted.
    async fn on_recovery_complete(
        &mut self,
        state: &SessionState,
        ctx: &mut ActorContext<SessionInbox>,
    ) {
        self.services = resolve(&self.projects, &self.account).await;

        // The journal is the truth about this session, and a session with
        // nothing in it has not been created yet: the `Create` that brought
        // this actor into being carries the spec, and adopting it is what
        // starts the agents below. Writing it from here instead would race
        // that command, and a rename arriving first would have nothing to
        // rename.
        //
        // Leaving `spec` as `None` is safe rather than merely tolerated —
        // `handle_command` answers everything but `Create` while it is.
        let Some(spec) = state.spec.clone() else {
            return;
        };
        self.adopt(spec, state, ctx).await;
        // After `adopt`, which is what puts the spec on this actor — and so
        // what lets `agent_roster` resolve a preset at all. Reconciling before
        // it would file every agent this session hosts as ad-hoc.
        self.reconcile_agent_runs(state);
        self.reconcile_inbox(state);
    }
}

/// One inbox row per answerable question.
///
/// Questions with no `tool_call_id` are dropped rather than stored: that is a
/// pre-#62 journal, where the call id was never recorded, and an inbox row is
/// an offer to answer. Without the id there is nothing to send an answer to, so
/// the row could only ever be a button that fails.
fn ask_rows(
    session_id: &str,
    agent_id: &str,
    asks: &[crate::agent_loop::AskedQuestion],
) -> Vec<crate::user_inbox::AskRow> {
    asks.iter()
        .filter_map(|ask| {
            Some(crate::user_inbox::AskRow {
                session_id: session_id.to_string(),
                agent_id: agent_id.to_string(),
                question: ask.question.clone(),
                choices: ask.choices.clone(),
                multiple: ask.multiple,
                tool_call_id: ask.tool_call_id.clone()?,
            })
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
pub(crate) mod testing;
