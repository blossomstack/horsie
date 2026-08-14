//! One interactive session: the conversational state machine and the owner of
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
//! component that owns it, and the turn boundary where the components' decisions
//! are performed in order. Everything else lives beside it.
//!
//! One component per slice of the session — [`lifecycle`] the sandbox,
//! [`turns`] the conversation, [`run`] the workflow graph, [`subagent`] the tree
//! of delegated work, [`reads`] the questions that wake nothing, [`hooks`] what
//! plugins did, [`core`] the session's own bookkeeping — over the vocabulary in
//! [`types`], to the shape in [`component`]. [`context`] is not one of them: it
//! assembles a turn on the *agent's* task rather than on this mailbox, which is
//! what keeps a thirty-second toolbox build from blocking a cancel.

use horsie_actor::ReplyTo;
mod component;
mod context;
mod core;
mod fork;
mod hooks;
mod lifecycle;
mod reads;
mod run;
mod subagent;
mod turns;
mod types;

pub use types::*;

use component::Component;
use core::SessionCore;
use fork::ForkedAgents;
use hooks::{HookRouting, StopHookParent};
use lifecycle::RuntimeLifecycle;
use reads::Reads;
use run::WorkflowRun;
use subagent::SubAgents;
use turns::Turns;

use crate::agent_loop::{
    AgentActor, AgentCommand, AgentOutcome, AgentParams, AgentRunDef, AgentRuntimeContext, Incoming,
};
use crate::sessions::{
    addressing::{SessionEntityId, SessionInbox, SessionRef, SupervisorRef},
    orchestrator::{AgentAction, Delivery},
    spec::{ServerDeps, SessionSpec, SessionStatus},
    supervisor::{ForkRow, SessionSupervisorCommand},
    workflow::WorkflowRunState,
};
use crate::users::{UserRegistry, UserServices, resolve};
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

/// The agent actors a session hosts.
///
/// An enum rather than an `Option` plus a map: a session's topology is decided
/// at creation and never changes, and a workflow run has no main agent at all.
enum SessionAgents {
    Interactive {
        main: ResidentAgent,
        subs: HashMap<Uuid, ResidentAgent>,
    },
    /// A workflow run: step agents and their subagents, all keyed by id. There
    /// is no main agent — the definition, not a person, decides who runs.
    Workflow { live: HashMap<Uuid, ResidentAgent> },
}

impl SessionAgents {
    fn interactive(main: ResidentAgent) -> Self {
        Self::Interactive {
            main,
            subs: HashMap::new(),
        }
    }

    fn workflow() -> Self {
        Self::Workflow {
            live: HashMap::new(),
        }
    }

    /// The session's primary agent, for the kinds that have one.
    fn main(&self) -> Option<&ResidentAgent> {
        match self {
            Self::Interactive { main, .. } => Some(main),
            Self::Workflow { .. } => None,
        }
    }

    fn sub(&self, id: Uuid) -> Option<&ResidentAgent> {
        match self {
            Self::Interactive { subs, .. } => subs.get(&id),
            Self::Workflow { live } => live.get(&id),
        }
    }

    /// The agent registered under `key`, if it is still resident.
    fn get(&self, key: AgentKey) -> Option<&ResidentAgent> {
        match key {
            AgentKey::Main => self.main(),
            AgentKey::Sub(id) | AgentKey::Step(id) | AgentKey::Fork(id) => self.sub(id),
        }
    }

    fn insert_sub(&mut self, id: Uuid, agent: ResidentAgent) {
        match self {
            Self::Interactive { subs, .. } => {
                subs.insert(id, agent);
            }
            Self::Workflow { live } => {
                live.insert(id, agent);
            }
        }
    }

    /// Forget one agent, handing it back so the caller can stop it. Only a
    /// fork's delete uses this: every other agent lives as long as the session
    /// is loaded, and nothing else removes one on request.
    fn remove_sub(&mut self, id: Uuid) -> Option<ResidentAgent> {
        match self {
            Self::Interactive { subs, .. } => subs.remove(&id),
            Self::Workflow { live } => live.remove(&id),
        }
    }

    /// Every agent, emptying the set. Used when the session unloads.
    fn drain_all(&mut self) -> Vec<ResidentAgent> {
        match self {
            Self::Interactive { main, subs } => {
                let mut out: Vec<_> = subs.drain().map(|(_, a)| a).collect();
                out.push(main.clone());
                out
            }
            Self::Workflow { live } => live.drain().map(|(_, a)| a).collect(),
        }
    }
}

/// Everything that differs between the three kinds of agent a session spawns.
///
/// The rest — the runtime provider, the plugin library, the MCP and memory
/// services, the session's own mailbox — is identical for all three and lives on
/// the actor, which is why one spawner can serve them all.
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
}

pub struct SessionActor {
    id: Uuid,
    /// Whose session this is. Not in the persistence id — a session's log is
    /// keyed by its uuid alone — but a recipe is handed it, because resolving
    /// the account's wiring is the one thing a session cannot do from its own
    /// id.
    account: crate::auth::UserId,
    /// What this session is. `None` until its own log says, or until the
    /// `RecordSpec` that brought this actor into being is handled — which is
    /// the first thing in the mailbox of a session that has no log yet.
    spec: Option<SessionSpec>,
    /// Where this account's bundle is resolved from. A shard recipe is
    /// synchronous, so nothing below can be handed in at construction.
    users: Weak<UserRegistry>,
    /// This account's bundle, resolved at recovery. See [`Self::services`].
    services: Option<Arc<UserServices>>,
    /// This session's supervisor, given at construction.
    ///
    /// A *name* with a warm cache rather than a handle to one mailbox, so a
    /// supervisor that stops and comes back is reached through the same
    /// reference and this session is told nothing. That is what makes handing it
    /// down cost nothing — and a session built on a host that never saw the
    /// request creating it is still handed one, because the recipe resolves the
    /// reference for the whole supervisor type rather than for an instance.
    supervisor: SupervisorRef,
    /// The agent actors this session hosts, resident for as long as this actor
    /// is loaded. `None` means exactly one thing — this session does not yet
    /// know what it is — which is why the topology inside is a value rather
    /// than a second `Option`: a session's shape is decided at creation and
    /// never changes.
    agents: Option<SessionAgents>,
    /// The last status this actor told the supervisor, so an unchanged one is
    /// not re-sent. `None` until it has reported once, which is why a freshly
    /// loaded session always reports.
    last_reported: Option<SessionStatus>,
    /// The same, for the fork roster. Empty until it has reported once — which
    /// costs nothing, because a session with no forks reports none either way.
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

    /// This account's bundle.
    ///
    /// Expects rather than handles: recovery resolves it, and recovery finishes
    /// before the first command is handled, so a `None` here is a broken actor
    /// lifecycle rather than a case with an answer.
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
        if self.spec().workflow.is_some() {
            // A run has no main agent. Step actors, like subagent actors, stay
            // cold: they spawn on demand for a history read, a retry, or the
            // next step a boundary picks.
            self.agents = Some(SessionAgents::workflow());
        } else {
            self.spawn_main_agent(ctx, state);
        }
        // Each component repairs itself. A self-send rather than direct work,
        // because neither caller may write here — recovery must not persist at
        // all, and `RecordSpec` is already returning an effect of its own — so
        // anything that needs to journal arrives as an ordinary command, down
        // the same path a live one would take.
        let cx = component::ActionCx {
            id: self.id,
            spec: self.spec(),
        };
        let repairs: Vec<SessionCommand> = [
            RuntimeLifecycle::on_load(&cx, state),
            SubAgents::on_load(&cx, state),
            WorkflowRun::on_load(&cx, state),
            Turns::on_load(&cx, state),
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
    /// which is the moment anyone can first learn its status.
    /// Tell the supervisor what forks this session now holds, so the session
    /// list can nest them without loading it.
    ///
    /// The whole roster every time, and the supervisor drops a report that
    /// changed nothing. A projection built from the current value cannot drift
    /// the way one built from deltas can — and `List` is documented to load
    /// nothing, so a sidebar that could not read this from the registry could
    /// not show forks at all without waking every session that has one.
    async fn report_forks(&mut self, state: &SessionState) {
        if state.forks.is_empty() && self.last_reported_forks.is_empty() {
            return;
        }
        let forks: Vec<ForkRow> = state
            .forks
            .iter()
            .map(|(id, rec)| ForkRow {
                id: *id,
                parent: match rec.parent {
                    crate::sessions::forks::ForkParent::Main => None,
                    crate::sessions::forks::ForkParent::Fork(pid) => Some(pid),
                },
                title: rec.title.clone(),
                status: rec.status,
                created_at_ms: rec.created_at_ms,
                last_activity_ms: rec.last_activity_ms,
            })
            .collect();
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

    async fn report_status(&mut self, state: &SessionState) {
        if self.last_reported.as_ref() == Some(&state.status) {
            return;
        }
        self.last_reported = Some(state.status.clone());
        let _ = self
            .supervisor
            .tell(SessionSupervisorCommand::SessionStatusChanged {
                id: self.id.to_string(),
                status: state.status.clone(),
            })
            .await;
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
        let (journal_id, revision) = match plan.kind {
            SessionAgentKind::Main => (self.id, revisions.for_agent(MAIN_AGENT_ID)),
            SessionAgentKind::Sub(id) | SessionAgentKind::Step(id) | SessionAgentKind::Fork(id) => {
                (id, revisions.for_agent(&id.to_string()))
            }
        };
        // Its name under this session, and the id it is addressed by.
        let name = match plan.kind {
            SessionAgentKind::Main => MAIN_AGENT_ID.to_string(),
            SessionAgentKind::Sub(id) | SessionAgentKind::Step(id) | SessionAgentKind::Fork(id) => {
                id.to_string()
            }
        };
        let key = plan.kind.agent_key();
        let provider = Arc::new(SessionContextProvider {
            runtimes: self.deps().runtimes.provider(
                self.id.to_string(),
                // The provision this run speaks to. A session that has never
                // provisioned has none, and the empty string is what the
                // acquisition below will fail on rather than silently
                // addressing some other sandbox.
                state
                    .provisioned_at_ms
                    .map(|at| at.to_string())
                    .unwrap_or_default(),
                // A create is still outstanding. The journal is the only thing
                // that knows, and it has to say so: a substrate that has not
                // reported the object yet is indistinguishable from one with
                // nothing there, and the difference is between waiting for a
                // runtime and declaring it gone.
                matches!(state.status, SessionStatus::Provisioning),
                self.spec().vendor.clone(),
                self.spec().clone(),
            ),
            registry: self.deps().provider_registry.clone(),
            mcp: self.deps().mcp.clone(),
            memory: self.deps().memory.clone(),
            services: Some(self.services().clone()),
            step_result: plan.step_result.clone(),
            session_id: self.id,
            kind: plan.kind,
            agent_type: plan.agent_type,
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
        params.thinking_effort = plan
            .settings
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let agent_ctx = AgentRuntimeContext {
            context_provider: provider.clone(),
            revision,
            parent: StopHookParent::wrap(self.me(ctx), key, provider.clone()),
            journal_id,
            // Computed from the state this spawn was decided against, never
            // remembered: an agent built after the runtime landed starts ready,
            // and one built before it starts waiting. Changes reach it as the
            // `Runtime` records it is sent anyway.
            ready: Self::runnable(state),
        };
        // A child of this session, named by the id it journals under — `main`
        // for the primary agent, the node id for a subagent or a step. Created
        // rather than spawned anonymously so it has a path under this session's,
        // which is what makes it stop with the session and makes two callers
        // racing to reach one agent get one actor over its journal.
        let actor = match ctx.actor_of(&name, ctx.persistent(AgentActor::new(agent_ctx, params))) {
            Ok(actor) => actor,
            Err(e) => {
                tracing::error!(session = %self.id, agent = %name, error = %e, "could not start the agent");
                return None;
            }
        };
        let resident = ResidentAgent { actor, provider };
        match plan.kind {
            SessionAgentKind::Main => {
                self.agents = Some(SessionAgents::interactive(resident.clone()));
            }
            SessionAgentKind::Sub(id) | SessionAgentKind::Step(id) | SessionAgentKind::Fork(id) => {
                if let Some(agents) = self.agents.as_mut() {
                    agents.insert_sub(id, resident.clone());
                }
            }
        }
        Some(resident)
    }

    /// The session's primary agent, spawned once at load.
    fn spawn_main_agent(&mut self, ctx: &ActorContext<SessionInbox>, state: &SessionState) {
        self.spawn_agent(
            ctx,
            state,
            AgentPlan {
                kind: SessionAgentKind::Main,
                settings: self.spec().agent.clone(),
                step_result: Default::default(),
                agent_type: None,
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
                match state.run.as_ref().and_then(WorkflowRunState::current_agent) {
                    // At most one step runs at a time, and the definition chose
                    // it, so there is nothing else an unaddressed request on a
                    // run could mean.
                    Some(step) => self.resolve_step(state, ctx, step),
                    None => self.agent().map(|actor| (AgentKey::Main, actor)),
                }
            }
            Some(raw) => {
                let id = Uuid::parse_str(raw).ok()?;
                if let Some(resolved) = self.resolve_step(state, ctx, id) {
                    return Some(resolved);
                }
                // Before the roster is consulted, because the roster cannot
                // say what *kind* of agent an id names — forks and subagents
                // share one map. Answering `Sub` for a fork made a fork of a
                // fork read as a fork of a subagent, and be refused.
                if state.forks.contains(id) {
                    return self
                        .spawn_fork_actor(ctx, state, id)
                        .map(|actor| (AgentKey::Fork(id), actor));
                }
                if let Some(agent) = self.agents.as_ref().and_then(|a| a.sub(id)) {
                    return Some((AgentKey::Sub(id), agent.actor.clone()));
                }
                // The type comes off the record, not from the caller: a cold
                // node woken to answer a read must run as what it was spawned as.
                let agent_type = state.subagents.node(id)?.agent_type.clone();
                Some((
                    AgentKey::Sub(id),
                    self.spawn_sub_agent_actor(ctx, state, id, agent_type)?,
                ))
            }
        }
    }

    /// One of a run's step agents, spawned if it is not resident. `None` when
    /// this session is not a run, or when the id names no execution in its log.
    ///
    /// The log, not the roster, is what identifies a step: a run's step
    /// subagents are registered in the same roster, so residency alone cannot
    /// tell the two apart. Spawning on demand is what keeps a finished run's
    /// step transcripts readable — the roster is empty after a reload, and
    /// every agent-scoped read comes through here.
    fn resolve_step(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        id: Uuid,
    ) -> Option<(AgentKey, ActorRef<AgentCommand>)> {
        let run = state.run.as_ref()?;
        let index = run.index_of_agent(id)?;
        if let Some(agent) = self.agents.as_ref().and_then(|a| a.sub(id)) {
            return Some((AgentKey::Step(id), agent.actor.clone()));
        }
        let step = run.get(index)?.step.clone();
        Some((
            AgentKey::Step(id),
            self.spawn_step_agent(ctx, state, id, &step)?,
        ))
    }

    fn agent(&self) -> Option<ActorRef<AgentCommand>> {
        self.agents
            .as_ref()
            .and_then(SessionAgents::main)
            .map(|a| a.actor.clone())
    }

    /// Cancel one agent's run and wait for it to actually be over.
    ///
    /// Two halves, in this order. The sandbox is told to abandon what it is
    /// running first, using the client that agent's own `provide()` cached —
    /// asking the manager for a fresh one would round-trip the vendor on this
    /// mailbox, and a vendor mid-tool-call cannot answer a lifecycle request
    /// until the call it is relaying resolves. Then the agent's loop is stopped.
    ///
    /// Waiting matters: the caller is about to record a turn boundary, and a run
    /// still winding down can still append to the agent journal.
    async fn cancel_agent(&mut self, key: AgentKey) {
        let Some(agent) = self.agents.as_ref().and_then(|a| a.get(key)).cloned() else {
            return;
        };
        if let Some(client) = agent.provider.cached_client() {
            client.cancel_in_flight().await;
        }
        let (tx, rx) = oneshot::channel();
        let _ = agent
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
    /// the main agent otherwise. A run used to be skipped here entirely, so
    /// deleting one mid-step left its sandbox call running.
    async fn cancel_in_flight(&mut self, state: &SessionState) {
        let step = state
            .run
            .as_ref()
            .and_then(|r| r.current().and_then(|i| r.get(i)))
            .map(|s| s.agent);
        match step {
            Some(agent) => self.cancel_agent(AgentKey::Step(agent)).await,
            None => self.cancel_agent(AgentKey::Main).await,
        }
    }

    /// Carry out one orchestrator decision and return the events that record it.
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
            AgentAction::Finish { output } => self.finish_run(output).await,
            AgentAction::Fail { error } => self.fail_run(error).await,
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
        vec![SessionDomainEvent::SubAgentNotified {
            at_ms: now_ms(),
            id: child,
        }]
    }

    /// The mailbox of one of this session's agents, spawning a cold subagent's
    /// actor on demand. `None` when nothing under that key exists.
    fn reach(
        &mut self,
        key: AgentKey,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Option<ActorRef<AgentCommand>> {
        if let Some(agent) = self.agents.as_ref().and_then(|a| a.get(key)) {
            return Some(agent.actor.clone());
        }
        match key {
            // A cold node reached for the first time since load. The type comes
            // off the record, not from the caller: a node woken to receive a
            // result must run as what it was spawned as.
            AgentKey::Sub(id) => {
                let agent_type = state.subagents.node(id)?.agent_type.clone();
                self.spawn_sub_agent_actor(ctx, state, id, agent_type)
            }
            // A step's actor spawns on demand from the run log, the same way a
            // cold subagent's does: a boundary can owe a result to a step whose
            // actor has since been unloaded.
            AgentKey::Step(id) => self.resolve_step(state, ctx, id).map(|(_, actor)| actor),
            // A cold fork, woken to be read or messaged. Nothing comes off a
            // record here the way a subagent's type does: a fork runs under
            // the session's own settings, like the conversation it branched
            // from.
            AgentKey::Fork(id) => state
                .forks
                .contains(id)
                .then(|| self.spawn_fork_actor(ctx, state, id))?,
            // Spawned at load, so it is either resident or this session is a run
            // and has none.
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
            || WorkflowRun::busy(state)
            || SubAgents::busy(state)
    }

    /// Everything every component wants started, given the state as it now is.
    ///
    /// A concatenation, not a negotiation: each component returns only work it
    /// owns, so there is nothing to reconcile. Subagent wakes go first — a
    /// parent waiting on its children is work already in flight, and the next
    /// turn or step can wait a boundary.
    fn next_actions(&self, state: &SessionState) -> Vec<AgentAction> {
        // Nothing starts before the runtime it would run on exists. One gate,
        // checked once, for every component.
        if !RuntimeLifecycle::ready(state) {
            return Vec::new();
        }
        let cx = component::ActionCx {
            id: self.id,
            spec: self.spec(),
        };
        [
            SubAgents::actions(&cx, state),
            Turns::actions(&cx, state),
            WorkflowRun::actions(&cx, state),
        ]
        .concat()
    }

    async fn flush_then_drain(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionDomainEvent> {
        let mut events = Vec::new();
        let mut next = state.clone();
        for action in self.next_actions(&next) {
            let produced = self.perform(action, &next, ctx).await;
            for e in &produced {
                next = Self::apply_event(next, e.clone());
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
                return CommandEffect::persist(vec![SessionDomainEvent::UsageRecorded {
                    at_ms: now_ms(),
                    agent_id,
                    usage_total,
                }]);
            }
            Err((agent, NotAnEnd::Started)) => {
                return self.on_agent_started(state, agent).await;
            }
            // A summary taken for somebody else. Not this agent's turn ending —
            // it may still be running — so it is answered here and the routing
            // below never sees it.
            Err((_, NotAnEnd::ForkSummary { forks, result })) => {
                return ForkedAgents::handle(
                    self,
                    state,
                    ForkCommand::Summarised { forks, result },
                    ctx,
                )
                .await;
            }
        };
        // In a run, an outcome is a step's or one of a step's subagents'.
        if let Some(run) = state.run.as_ref() {
            return match run.index_of_agent(who) {
                Some(index) => self.on_step_outcome(state, index, end, ctx).await,
                None => self.on_sub_agent_outcome(state, who, end, ctx).await,
            };
        }
        if who == self.id {
            return self.on_main_outcome(state, end, ctx).await;
        }
        // Before the subagent forest, because a fork is not in it: asked last,
        // every one of a fork's turns would be dropped as an outcome from an
        // agent nothing recognises.
        if state.forks.contains(who) {
            return self.on_fork_outcome(state, who, end, ctx).await;
        }
        self.on_sub_agent_outcome(state, who, end, ctx).await
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
        if who == self.id {
            return CommandEffect::persist(vec![SessionDomainEvent::TurnBegan { at_ms: now_ms() }]);
        }
        if state.subagents.node(who).is_some() {
            return CommandEffect::persist(vec![SessionDomainEvent::SubAgentRunning {
                at_ms: now_ms(),
                id: who,
            }]);
        }
        // A fork's own status, and only its own: the session's belongs to the
        // main agent, and a fork answering a question is not the session
        // working.
        if state.forks.contains(who) {
            return CommandEffect::persist(vec![SessionDomainEvent::ForkStatusChanged {
                at_ms: now_ms(),
                id: who,
                status: AgentStatus::Running,
            }]);
        }
        CommandEffect::none()
    }

    /// Whether this session's agents may start a turn at all: it has a runtime,
    /// and it is not terminal. The whole of what an agent's own drain gate
    /// cannot answer for itself.
    fn runnable(state: &SessionState) -> bool {
        RuntimeLifecycle::ready(state)
            && !matches!(state.status, SessionStatus::Unrecoverable { .. })
    }

    /// Stop every agent this session hosts. Used when the session unloads.
    async fn stop_agents(&mut self) {
        let Some(mut agents) = self.agents.take() else {
            return;
        };
        for agent in agents.drain_all() {
            // Cancel first: a stopped mailbox makes the run task's next persist
            // fail, but an in-flight tool call would run to completion first.
            let _ = agent.actor.tell(AgentCommand::Cancel { ack: None }).await;
            let _ = agent.actor.tell(AgentCommand::Shutdown).await;
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
            SessionDomainEvent::ProvisioningStarted { .. }
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
            SessionDomainEvent::StepStarted { .. }
            | SessionDomainEvent::StepConcluded { .. }
            | SessionDomainEvent::StepFailed { .. }
            | SessionDomainEvent::StepCancelled { .. }
            | SessionDomainEvent::RunFinished { .. }
            | SessionDomainEvent::RunFailed { .. } => WorkflowRun::apply(&mut state, &event),
            SessionDomainEvent::SubAgentSpawned { .. }
            | SessionDomainEvent::SubAgentRunning { .. }
            | SessionDomainEvent::SubAgentCompleted { .. }
            | SessionDomainEvent::SubAgentFailed { .. }
            | SessionDomainEvent::SubAgentNotified { .. } => SubAgents::apply(&mut state, &event),
            SessionDomainEvent::ForkCreated { .. }
            | SessionDomainEvent::ForkSeeded { .. }
            | SessionDomainEvent::ForkTitled { .. }
            | SessionDomainEvent::ForkStatusChanged { .. }
            | SessionDomainEvent::ForkTurnEnded { .. }
            | SessionDomainEvent::ForkDeleted { .. } => ForkedAgents::apply(&mut state, &event),
            SessionDomainEvent::UsageRecorded { .. }
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
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd.cmd {
            SessionCommand::Lifecycle(c) => RuntimeLifecycle::handle(self, state, c, ctx).await,
            SessionCommand::Turn(c) => Turns::handle(self, state, c, ctx).await,
            SessionCommand::Run(c) => WorkflowRun::handle(self, state, c, ctx).await,
            SessionCommand::SubAgent(c) => SubAgents::handle(self, state, c, ctx).await,
            SessionCommand::Fork(c) => ForkedAgents::handle(self, state, c, ctx).await,
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
        self.services = resolve(&self.users, &self.account).await;

        // The journal is the truth about this session, and a session with
        // nothing in it has not been created yet: the `RecordSpec` that brought
        // this actor into being is next in this mailbox, and adopting it is
        // what starts the agents below. Writing it from here instead would race
        // that command, and a rename arriving first would have nothing to
        // rename.
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
