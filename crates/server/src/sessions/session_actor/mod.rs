//! The session actor: one journal, one sandbox, and a tree of runners.
//!
//! The actor is the only thing here that *acts*. The [`runner`] module owns
//! every decision — what an agent's turn ending means, what starts at a
//! boundary, what a crash left to repair — as pure functions over the folded
//! [`SessionState`](runner::state::SessionState). This file performs them:
//! it spawns agents from the [`AgentRole`](runner::role::AgentRole) a runner
//! resolves, journals the events a runner decides, folds what it journaled,
//! and drains the boundary each batch creates.
//!
//! Commands are grouped by domain — [`commands`] for the conversation surface,
//! [`spawn`] for delegated work, [`forking`] for branches, [`running`] for
//! workflow runs, [`lifecycle`] for the sandbox, [`reads`], [`hooks`] and
//! [`core`] for the rest — over the vocabulary in [`types`]. [`context`] is
//! not one of them: it assembles a turn on the *agent's* task rather than on
//! this mailbox, which is what keeps a thirty-second toolbox build from
//! blocking a cancel.

use horsie_actor::ReplyTo;
mod commands;
mod context;
mod core;
mod forking;
mod hooks;
mod lifecycle;
mod reads;
pub(crate) mod runner;
mod running;
mod spawn;
mod types;

pub use runner::event::{RecordedEnd, RunnerArgs, RunnerEvent, SessionEvent};
pub use runner::ids::{AgentId, RunnerId};
pub use runner::state::SessionState;
pub use types::*;

use hooks::StopHookParent;
use runner::action::{Repair, RunnerAction};
use runner::role::AgentRole;
use runner::state::RunnerState;
use runner::{MainAgentRunner, Runner, RunnerBehavior};

use crate::agent_loop::{
    AgentActor, AgentCommand, AgentOutcome, AgentParams, AgentRunDef, AgentRuntimeContext, Incoming,
};
use crate::sessions::{
    addressing::{SessionEntityId, SessionInbox, SessionRef, SupervisorRef},
    spec::{ServerDeps, SessionKind, SessionSpec, SessionStatus},
    supervisor::{ForkRow, SessionSupervisorCommand},
};
use crate::users::{UserRegistry, UserServices, resolve};
use async_trait::async_trait;
use context::SessionContextProvider;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_models::now_ms;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// The wire spelling of a session's primary agent, and the journal key its
/// transcript is published under.
pub const MAIN_AGENT_ID: &str = "main";

/// How long a cancel waits for the run to actually finish before giving up.
/// Cancellation is prompt (milliseconds); this is a backstop so a wedged run
/// can never hold the mailbox — and with it the Stop button — hostage.
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// One resident agent: its mailbox, and the provider that assembles its turns.
///
/// The two travel together because cancelling needs both — the mailbox to stop
/// the loop, and the provider to reach the runtime client the run in flight
/// already acquired.
#[derive(Clone)]
struct ResidentAgent {
    actor: ActorRef<AgentCommand>,
    provider: Arc<SessionContextProvider>,
}

/// The agent actors a session hosts, in one flat map keyed by id — the main
/// agent, when the session has one, under the session's own id.
///
/// Residency is a cache, never identity: what an id *is* is answered by the
/// runner tree in the persisted state.
struct SessionAgents {
    /// The session's id, under which its main agent is keyed.
    session: Uuid,
    agents: HashMap<Uuid, ResidentAgent>,
}

impl SessionAgents {
    fn new(session: Uuid) -> Self {
        Self {
            session,
            agents: HashMap::new(),
        }
    }

    /// The session's primary agent, for the kinds that have one.
    fn main(&self) -> Option<&ResidentAgent> {
        self.agents.get(&self.session)
    }

    fn get(&self, agent: AgentId) -> Option<&ResidentAgent> {
        self.agents.get(&agent.0)
    }

    fn insert(&mut self, agent: AgentId, resident: ResidentAgent) {
        self.agents.insert(agent.0, resident);
    }

    /// Forget one agent, handing it back so the caller can stop it. Only a
    /// fork's delete uses this: every other agent lives as long as the session
    /// is loaded.
    fn remove(&mut self, agent: AgentId) -> Option<ResidentAgent> {
        self.agents.remove(&agent.0)
    }

    /// Every agent, emptying the set. Used when the session unloads.
    fn drain_all(&mut self) -> Vec<ResidentAgent> {
        self.agents.drain().map(|(_, a)| a).collect()
    }
}

pub struct SessionActor {
    id: Uuid,
    /// Whose session this is. Not in the persistence id — a session's log is
    /// keyed by its uuid alone — but a recipe is handed it, because resolving
    /// the account's wiring is the one thing a session cannot do from its own
    /// id.
    account: crate::auth::UserId,
    /// What this session is. `None` until its own log says, or until the
    /// `RecordSpec` that brought this actor into being is handled.
    spec: Option<SessionSpec>,
    /// Where this account's bundle is resolved from. A shard recipe is
    /// synchronous, so nothing below can be handed in at construction.
    users: Weak<UserRegistry>,
    /// This account's bundle, resolved at recovery. See [`Self::services`].
    services: Option<Arc<UserServices>>,
    /// This session's supervisor, given at construction. A *name* with a warm
    /// cache rather than a handle to one mailbox.
    supervisor: SupervisorRef,
    /// The agent actors this session hosts, resident for as long as this actor
    /// is loaded. `None` means exactly one thing — this session does not yet
    /// know what it is.
    agents: Option<SessionAgents>,
    /// The last status this actor told the supervisor, so an unchanged one is
    /// not re-sent. `None` until it has reported once, which is why a freshly
    /// loaded session always reports.
    last_reported: Option<SessionStatus>,
    /// The same, for the fork roster.
    last_reported_forks: Vec<ForkRow>,
}

impl SessionActor {
    pub fn new(
        entity: SessionEntityId,
        supervisor: SupervisorRef,
        users: Weak<UserRegistry>,
    ) -> Self {
        Self {
            id: entity.session,
            account: entity.account,
            spec: None,
            users,
            services: None,
            supervisor,
            agents: None,
            last_reported: None,
            last_reported_forks: Vec::new(),
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

    /// The session's own agent id — its main agent's, and its root runner's.
    fn self_agent(&self) -> AgentId {
        AgentId(self.id)
    }

    /// Expects rather than handles: recovery resolves it, and recovery
    /// finishes before the first command is handled, so a `None` here is a
    /// broken actor lifecycle rather than a case with an answer.
    #[expect(
        clippy::expect_used,
        reason = "recovery runs before any command, so this cannot be None"
    )]
    fn services(&self) -> &Arc<UserServices> {
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
    /// on: nothing reads a spec before the command that records it, because
    /// that command is what created this actor.
    #[expect(
        clippy::expect_used,
        reason = "a session is told what it is before anything else can reach it"
    )]
    pub(super) fn spec(&self) -> &SessionSpec {
        self.spec
            .as_ref()
            .expect("a session is told what it is before anything else can reach it")
    }

    /// The same, for the two renames that keep the resident copy in step with
    /// what has just been journaled.
    #[expect(
        clippy::expect_used,
        reason = "a session is told what it is before anything else can reach it"
    )]
    pub(super) fn spec_mut(&mut self) -> &mut SessionSpec {
        self.spec
            .as_mut()
            .expect("a session is told what it is before anything else can reach it")
    }

    /// This session's own mailbox, as the thing that reaches it.
    pub(super) fn me(&self, ctx: &ActorContext<SessionInbox>) -> SessionRef {
        SessionRef::new(ctx.self_ref(), self.account.clone(), self.id, None)
    }

    /// Take up a spec, start the agents it calls for, and put right whatever
    /// the state it is handed says was interrupted.
    ///
    /// Two callers, and the pair is the whole of how a session learns what it
    /// is: recovery, from what its log already says, and `RecordSpec`, for a
    /// session whose log is empty because it was created a moment ago.
    pub(super) async fn adopt(
        &mut self,
        spec: SessionSpec,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) {
        self.spec = Some(spec);
        // The roster exists from the moment the session knows what it is.
        self.agents = Some(SessionAgents::new(self.id));
        let me = self.me(ctx);
        if state.root().is_none() {
            // The log holds no root runner yet: journal one, down the same
            // self-send path a repair takes, so creation and recovery are one
            // code path. The main agent below does not wait for it — its role
            // reads only the spec.
            let _ = me.tell(SessionCommand::Core(CoreCommand::CreateRoot)).await;
        }
        if matches!(self.spec().kind, SessionKind::Agent { .. }) {
            self.spawn_resident_main(ctx, state);
        }
        // Each runner repairs itself, discovered by iteration rather than a
        // hand-maintained list. A self-send rather than direct work, because
        // neither caller may write here — recovery must not persist at all —
        // so anything that needs to journal arrives as an ordinary command.
        for cmd in repairs_to_commands(runner::load_repairs(state)) {
            let _ = me.tell(cmd).await;
        }
        // Loading is not a transition, but it is the first moment anyone can
        // learn this status: a page already watching hears nothing otherwise.
        self.report_status(state).await;
    }

    /// Tell the supervisor what forks this session now holds, so the session
    /// list can nest them without loading it. The whole roster every time; the
    /// supervisor drops a report that changed nothing.
    async fn report_forks(&mut self, state: &SessionState) {
        let forks: Vec<ForkRow> = state
            .runners
            .iter()
            .filter_map(|(id, record)| {
                let RunnerState::Fork(f) = &record.state else {
                    return None;
                };
                Some(ForkRow {
                    id: id.0,
                    // A fork of a fork nests under it; one rooted on the main
                    // agent sits at the top.
                    parent: record
                        .parent
                        .filter(|p| {
                            matches!(
                                state.record(RunnerId::of_agent(*p)).map(|r| &r.state),
                                Some(RunnerState::Fork(_))
                            )
                        })
                        .map(|p| p.0),
                    title: f.title.clone(),
                    status: f.agent_status(),
                    created_at_ms: record.created_at_ms,
                    last_activity_ms: f.last_activity_ms,
                })
            })
            .collect();
        if forks.is_empty() && self.last_reported_forks.is_empty() {
            return;
        }
        if forks == self.last_reported_forks {
            return;
        }
        self.last_reported_forks = forks.clone();
        let _ = self
            .supervisor
            .tell(SessionSupervisorCommand::ForksChanged {
                id: self.id.to_string(),
                forks,
            })
            .await;
    }

    /// Tell the supervisor the status this session's journal just folded.
    ///
    /// Read off the folded state rather than announced at each transition, and
    /// called only where the state has settled — after a persisted batch, and
    /// once at load. So what the supervisor records is by construction what
    /// the session journaled.
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

    /// Spawn one of this session's agents from the role its runner resolved,
    /// and register it.
    ///
    /// The single spawner for every kind, and kind-free: everything that
    /// differs — journal identity, settings, toolbox layers, prompt suffix,
    /// hook shape — arrives precomputed on the [`AgentRole`]. Cheap and
    /// runtime-free: the provider, toolbox and system prompt are resolved per
    /// run, on the run's own task, so this costs nothing but a mailbox.
    fn spawn_for_role(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        role: AgentRole,
    ) -> Option<ResidentAgent> {
        // Taken from the account's registry rather than owned here, because
        // the channels have to outlive this actor: unloading an idle session
        // must leave a reader waiting rather than disconnecting it.
        let revisions = self.services().revisions.of(&self.id.to_string());
        // `publishing` rather than `for_agent`: this node is the one running
        // the agent, so every move of its counter has to reach whichever node
        // is answering readers of it.
        let revision = revisions.publishing(&role.name);
        let agent = role.agent;
        let name = role.name.clone();
        let journal_id = role.journal;
        let provider = Arc::new(SessionContextProvider {
            runtimes: self.deps().runtimes.provider(
                self.id.to_string(),
                // The provision this run speaks to. A session that has never
                // provisioned has none, and the empty string is what the
                // acquisition below will fail on rather than silently
                // addressing some other sandbox.
                state
                    .provisioning
                    .provisioned_at_ms
                    .map(|at| at.to_string())
                    .unwrap_or_default(),
                // A create is still outstanding. The journal is the only thing
                // that knows, and it has to say so.
                matches!(
                    state.provisioning.phase,
                    runner::state::ProvisionPhase::InFlight
                ),
                self.spec().vendor.clone(),
                self.spec().clone(),
            ),
            registry: self.deps().provider_registry.clone(),
            mcp: self.deps().mcp.clone(),
            memory: self.deps().memory.clone(),
            services: Some(self.services().clone()),
            session_id: self.id,
            role: role.clone(),
            session: self.me(ctx),
            plugins: self.spec().plugins.clone(),
            plugin_library: self.deps().plugins.clone(),
            last_client: Mutex::new(None),
        });
        let mut params = AgentParams::from_def(&AgentRunDef {
            system_prompt: None,
            max_iterations: role.settings.max_iterations,
            max_retries: Some(role.settings.max_retries),
            allowed_tools: role.settings.allowed_tools.clone(),
        });
        params.interactive = true;
        // A step is the only agent that owes a structured result, and the only
        // one for which a turn ending with plain text is not an answer.
        params.requires_result = role.requires_result();
        params.thinking_effort = role
            .settings
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let agent_ctx = AgentRuntimeContext {
            context_provider: provider.clone(),
            revision,
            parent: StopHookParent::wrap(self.me(ctx), provider.clone()),
            journal_id,
            // Computed from the state this spawn was decided against, never
            // remembered: an agent built after the runtime landed starts
            // ready, and one built before it starts waiting.
            ready: Self::runnable(state),
        };
        // A child of this session, named by the id it journals under — `main`
        // for the primary agent, the agent id for everything else. Created
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
        if let Some(agents) = self.agents.as_mut() {
            agents.insert(agent, resident.clone());
        }
        Some(resident)
    }

    /// The session's primary agent, spawned once at load. Its role reads only
    /// the spec, so it needs no runner record — which is what lets a freshly
    /// created session accept a message before its root's `Created` has
    /// landed.
    fn spawn_resident_main(&mut self, ctx: &ActorContext<SessionInbox>, state: &SessionState) {
        let runner = MainAgentRunner {
            id: RunnerId(self.id),
        };
        let Some(role) = runner.role(self.spec(), state, self.self_agent()) else {
            return;
        };
        self.spawn_for_role(ctx, state, role);
    }

    /// Resolve an agent selector to its actor: `None`/`"main"` for the primary
    /// agent, else an agent's uuid. A cold agent — one the persisted state
    /// knows about with no actor since this session loaded — is spawned on
    /// demand, so reading a finished agent works exactly like reading a live
    /// one.
    ///
    /// A run has no primary agent, so an unaddressed selector means the step
    /// in flight there.
    fn resolve_agent(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        agent_id: Option<&str>,
    ) -> Option<(AgentId, ActorRef<AgentCommand>)> {
        let agent = self.resolve_selector(state, agent_id)?;
        let actor = self.reach(agent, state, ctx)?;
        Some((agent, actor))
    }

    /// Which agent a selector names, without spawning anything.
    fn resolve_selector(&self, state: &SessionState, agent_id: Option<&str>) -> Option<AgentId> {
        match agent_id {
            None | Some(MAIN_AGENT_ID) => match &self.spec().kind {
                SessionKind::Agent { .. } => Some(self.self_agent()),
                // At most one step runs at a time, and the definition chose
                // it, so there is nothing else an unaddressed request on a run
                // could mean.
                SessionKind::Workflow { .. } => {
                    let (_, record) = state.root()?;
                    match &record.state {
                        RunnerState::Workflow(w) => w.run.current_agent().map(AgentId),
                        RunnerState::Main(_) | RunnerState::Sub(_) | RunnerState::Fork(_) => None,
                    }
                }
            },
            Some(raw) => {
                let id = AgentId(Uuid::parse_str(raw).ok()?);
                state.owner_of(id).map(|_| id)
            }
        }
    }

    /// The mailbox of one of this session's agents, spawning a cold one on
    /// demand. `None` when nothing under that id exists.
    fn reach(
        &mut self,
        agent: AgentId,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Option<ActorRef<AgentCommand>> {
        if let Some(resident) = self.agents.as_ref().and_then(|a| a.get(agent)) {
            return Some(resident.actor.clone());
        }
        // The role comes off the owning runner's record, not from the caller:
        // a cold agent woken to answer a read must run as what it was spawned
        // as. The main agent has no record before the root's `Created` lands,
        // so it resolves from the spec alone.
        let role = match Runner::owner_of(agent, state) {
            Some(runner) => runner.role(self.spec(), state, agent)?,
            None if agent == self.self_agent()
                && matches!(self.spec().kind, SessionKind::Agent { .. }) =>
            {
                MainAgentRunner {
                    id: RunnerId(self.id),
                }
                .role(self.spec(), state, agent)?
            }
            None => return None,
        };
        self.spawn_for_role(ctx, state, role).map(|r| r.actor)
    }

    /// Cancel one agent's run and wait for it to actually be over.
    ///
    /// Two halves, in this order. The sandbox is told to abandon what it is
    /// running first, using the client that agent's own `provide()` cached;
    /// then the agent's loop is stopped. Waiting matters: the caller is about
    /// to record a turn boundary, and a run still winding down can still
    /// append to the agent journal.
    pub(super) async fn cancel_agent(&mut self, agent: AgentId) {
        let Some(resident) = self.agents.as_ref().and_then(|a| a.get(agent)).cloned() else {
            return;
        };
        if let Some(client) = resident.provider.cached_client() {
            client.cancel_in_flight().await;
        }
        let (tx, rx) = oneshot::channel();
        let _ = resident
            .actor
            .tell(AgentCommand::Cancel {
                ack: Some(ReplyTo::from_sender(tx)),
            })
            .await;
        if tokio::time::timeout(CANCEL_TIMEOUT, rx).await.is_err() {
            tracing::warn!(
                session = %self.id,
                "cancelled run did not finish within {CANCEL_TIMEOUT:?}; proceeding"
            );
        }
    }

    /// Cancel whatever this session is running: the step in flight for a run,
    /// the main agent otherwise.
    pub(super) async fn cancel_in_flight(&mut self, state: &SessionState) {
        let target = match state.root() {
            Some((id, record)) => match &record.state {
                RunnerState::Workflow(w) => w.run.current_agent().map(AgentId),
                RunnerState::Main(_) => Some(AgentId(id.0)),
                RunnerState::Sub(_) | RunnerState::Fork(_) => None,
            },
            None => None,
        };
        if let Some(agent) = target {
            self.cancel_agent(agent).await;
        }
    }

    /// Carry out one runner decision and return the events that record it.
    async fn perform(
        &mut self,
        action: RunnerAction,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        match action {
            RunnerAction::Deliver { to, child, part } => {
                self.deliver(to, child, part, state, ctx).await
            }
            RunnerAction::StartStep { run, start } => self.start_step(run, start, state, ctx).await,
            RunnerAction::FinishRun { run, output } => vec![SessionEvent::Runner {
                id: run,
                at_ms: now_ms(),
                event: RunnerEvent::RunFinished { output },
            }],
            RunnerAction::FailRun { run, error } => vec![SessionEvent::Runner {
                id: run,
                at_ms: now_ms(),
                event: RunnerEvent::RunFailed { error },
            }],
        }
    }

    /// Put a finished child runner's result in the queue of the agent owed it,
    /// and record that it has been sent.
    ///
    /// Tell-then-persist: a crash between the enqueue and this write leaves
    /// the result still owed, so the next boundary re-delivers it. Delivery is
    /// at-least-once in that window (the parent may see a result twice), never
    /// lost — `Created`'s stricter persist-then-spawn is the deliberate
    /// exception, because an untracked agent is worse than a duplicate.
    ///
    /// Skipped, not failed, when the agent cannot be reached: the result stays
    /// owed and the next boundary tries again.
    async fn deliver(
        &mut self,
        to: AgentId,
        child: RunnerId,
        part: horsie_models::agent::SubAgentResultPart,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        let Some(agent) = self.reach(to, state, ctx) else {
            return Vec::new();
        };
        if agent
            .tell(AgentCommand::Enqueue {
                item: Incoming::SubAgent {
                    id: child.to_string(),
                    part: Box::new(part),
                },
                ack: None,
            })
            .await
            .is_err()
        {
            return Vec::new();
        }
        vec![SessionEvent::Runner {
            id: child,
            at_ms: now_ms(),
            event: RunnerEvent::Reported,
        }]
    }

    /// Everything every runner wants started at this turn boundary, performed
    /// in order, each seeing the state the previous one produced.
    ///
    /// Every turn boundary routes through here — without that, a result owed
    /// to a subagent parent strands the moment no further outcome can arrive.
    pub(super) async fn flush_then_drain(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        let mut next = state.clone();
        for action in runner::boundary_actions(&next) {
            let produced = self.perform(action, &next, ctx).await;
            for e in &produced {
                next.apply(e);
            }
            events.extend(produced);
        }
        events
    }

    /// Persist `events`, having first let the boundary they create start
    /// whatever became startable.
    ///
    /// The fold is local and one step early — the same fold the runtime will
    /// apply when it persists — because the drain has to see the state these
    /// events produce, not the one they were decided against.
    pub(super) async fn persist_and_advance(
        &mut self,
        state: &SessionState,
        mut events: Vec<SessionEvent>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        let mut next = state.clone();
        for e in &events {
            next.apply(e);
        }
        events.extend(self.flush_then_drain(&next, ctx).await);
        CommandEffect::persist(events)
    }

    /// Route one agent's outcome to the runner that owns it.
    ///
    /// The one command routed by *identity* rather than by variant: the same
    /// `Concluded` means "the turn is over", "this step's output picks the
    /// next step", or "tell the parent its child is done", depending only on
    /// which agent sent it.
    async fn on_agent_outcome(
        &mut self,
        state: &SessionState,
        outcome: AgentOutcome,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        let (who, end) = match TurnEnd::split(outcome) {
            Ok(pair) => pair,
            // Usage is banked for every agent alike, and always: the tokens
            // were spent whatever became of the turn that spent them. The main
            // agent banks under a fixed name because its journal is keyed by
            // the session id; every other agent banks under its own.
            Err((agent, NotAnEnd::Usage(usage_total))) => {
                let agent_id = match agent == self.id {
                    true => MAIN_AGENT_ID.to_string(),
                    false => agent.to_string(),
                };
                return CommandEffect::persist(vec![SessionEvent::UsageRecorded {
                    at_ms: now_ms(),
                    agent_id,
                    usage_total,
                }]);
            }
            // One of this session's agents drained its queue into a turn.
            // Recorded, not decided: the agent owns its own queue. What the
            // beginning means — the conversation running, a terminal node
            // woken — is the owning runner's fold's business.
            Err((agent, NotAnEnd::Started)) => {
                let agent = AgentId(agent);
                if state.owner_of(agent).is_none() {
                    return CommandEffect::none();
                }
                return CommandEffect::persist(vec![SessionEvent::TurnBegan {
                    at_ms: now_ms(),
                    agent,
                }]);
            }
            // A summary taken for somebody else. Not this agent's turn ending
            // — it may still be running — so it is answered here and the
            // routing below never sees it.
            Err((_, NotAnEnd::ForkSummary { forks, result })) => {
                return self.on_summarised(state, forks, result, ctx).await;
            }
        };
        let agent = AgentId(who);
        let Some(owner) = Runner::owner_of(agent, state) else {
            tracing::warn!(session = %self.id, agent = %agent, "outcome from an unknown agent; ignored");
            return CommandEffect::none();
        };
        let decision = owner.on_outcome(state, agent, end, now_ms());
        if decision.events.is_empty() {
            return CommandEffect::none();
        }
        if decision.advance {
            self.persist_and_advance(state, decision.events, ctx).await
        } else {
            CommandEffect::persist(decision.events)
        }
    }

    /// Whether this session's agents may start a turn at all: it has a
    /// runtime, and it is not terminal. The whole of what an agent's own drain
    /// gate cannot answer for itself.
    fn runnable(state: &SessionState) -> bool {
        runner::boundary_open(state)
    }

    /// Stop every agent this session hosts. Used when the session unloads.
    pub(super) async fn stop_agents(&mut self) {
        let Some(mut agents) = self.agents.take() else {
            return;
        };
        for agent in agents.drain_all() {
            // Cancel first: a stopped mailbox makes the run task's next
            // persist fail, but an in-flight tool call would run to completion
            // first.
            let _ = agent.actor.tell(AgentCommand::Cancel { ack: None }).await;
            let _ = agent.actor.tell(AgentCommand::Shutdown).await;
        }
    }
}

/// The commands that carry out a set of load repairs. Collapsed: several
/// interrupted subagents are one `Reconcile`, several seeding forks one
/// `ReseedInterrupted`, and every pending run is served by one `Advance`.
fn repairs_to_commands(repairs: Vec<Repair>) -> Vec<SessionCommand> {
    let mut commands = Vec::new();
    let (mut provision, mut reconcile, mut reseed, mut advance) = (false, false, false, false);
    for repair in repairs {
        match repair {
            Repair::Provision if !provision => {
                provision = true;
                commands.push(SessionCommand::Lifecycle(LifecycleCommand::Provision));
            }
            Repair::FailInterruptedSub { .. } if !reconcile => {
                reconcile = true;
                commands.push(SessionCommand::SubAgent(SubAgentCommand::Reconcile));
            }
            Repair::SuspendInterruptedRun { id } => {
                commands.push(SessionCommand::Run(RunCommand::ReconcileInterrupted {
                    run: id,
                }));
            }
            Repair::AdvanceRun { .. } if !advance => {
                advance = true;
                commands.push(SessionCommand::Run(RunCommand::Advance));
            }
            Repair::ReseedFork { .. } if !reseed => {
                reseed = true;
                commands.push(SessionCommand::Fork(ForkCommand::ReseedInterrupted));
            }
            Repair::Provision
            | Repair::FailInterruptedSub { .. }
            | Repair::AdvanceRun { .. }
            | Repair::ReseedFork { .. } => {}
        }
    }
    commands
}

#[async_trait]
impl EventSourcedActor for SessionActor {
    type Command = SessionInbox;
    type Event = SessionEvent;
    type State = SessionState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.id)
    }

    fn initial_state() -> SessionState {
        SessionState::default()
    }

    fn apply_event(mut state: SessionState, event: SessionEvent) -> SessionState {
        state.apply(&event);
        state
    }

    /// Write what just became durable into the agents' own transcripts, so a
    /// reader sees a lifecycle entry where it happened rather than having to
    /// infer it from the session's status. And report the status the batch
    /// left behind — here, because here the write is already durable: the
    /// supervisor's copy can lag the journal, never lead it.
    async fn on_events_persisted(&mut self, events: &[SessionEvent], state: &SessionState) {
        self.record_lifecycle(events, state).await;
        self.report_forks(state).await;
        self.report_status(state).await;
    }

    /// Every command arrives addressed to a session, and this is the one place
    /// that reads the address: the shard already routed by it, so what is left
    /// below is the command it was wrapped around.
    async fn handle_command(
        &mut self,
        state: &SessionState,
        cmd: SessionInbox,
        ctx: &mut ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd.cmd {
            SessionCommand::Lifecycle(c) => self.handle_lifecycle(state, c, ctx).await,
            SessionCommand::Turn(c) => self.handle_turn(state, c, ctx).await,
            SessionCommand::Run(c) => self.handle_run(state, c, ctx).await,
            SessionCommand::SubAgent(c) => self.handle_sub_agent(state, c, ctx).await,
            SessionCommand::Fork(c) => self.handle_fork(state, c, ctx).await,
            SessionCommand::Read(c) => self.handle_read(state, c, ctx).await,
            SessionCommand::Hooks(c) => self.handle_hooks(state, c, ctx).await,
            SessionCommand::Core(c) => self.handle_core(state, c, ctx).await,
            // The one command routed by identity rather than by variant: which
            // agent sent the outcome decides which runner answers it.
            SessionCommand::AgentOutcome(outcome) => {
                self.on_agent_outcome(state, outcome, ctx).await
            }
        }
    }

    /// Loading a session resolves its account, spawns its agent and repairs
    /// what the process died inside. It calls no vendor, starts no run, and
    /// drains nothing — an interrupted assistant turn is over, and queued user
    /// messages wait for the next turn the user starts.
    async fn on_recovery_complete(
        &mut self,
        state: &SessionState,
        ctx: &mut ActorContext<SessionInbox>,
    ) {
        self.services = resolve(&self.users, &self.account).await;

        // The journal is the truth about this session, and a session with
        // nothing in it has not been created yet: the `RecordSpec` that
        // brought this actor into being is next in this mailbox, and adopting
        // it is what starts the agents below.
        let Some(spec) = state.spec.clone() else {
            return;
        };
        self.adopt(spec, state, ctx).await;
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
pub(crate) mod testing;
