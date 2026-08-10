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
    ask_tool::ASK_USER_TOOL,
    orchestrator::{AgentAction, Delivery},
    spec::{ServerDeps, SessionSpec, SessionStatus},
    supervisor::SessionSupervisorCommand,
    workflow::WorkflowRunState,
};
use async_trait::async_trait;
use context::{SessionAgentKind, SessionContextProvider};
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_models::now_ms;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
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
            AgentKey::Sub(id) | AgentKey::Step(id) => self.sub(id),
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
    /// A step's declared output schema, which becomes its `conclude` tool's
    /// input schema. `None` for every other kind.
    step_output_schema: Option<Value>,
    /// The plugin-declared agent type a typed subagent runs as.
    agent_type: Option<String>,
    /// A tool whose call ends the turn without ending the run. Only the main
    /// agent has one, and only when someone is there to answer it.
    handoff_tool: Option<String>,
}

pub struct SessionActor {
    id: Uuid,
    spec: SessionSpec,
    deps: ServerDeps,
    parent: ActorRef<SessionSupervisorCommand>,
    /// The agent actors this session hosts, resident for as long as this actor
    /// is loaded. `None` means exactly one thing — recovery has not finished —
    /// which is why the topology inside is a value rather than a second
    /// `Option`: a session's shape is decided at creation and never changes.
    agents: Option<SessionAgents>,
    /// The supervisor's per-agent position channels for this session.
    ///
    /// Cloned in rather than created here: it has to outlive this actor, so
    /// that unloading an idle session leaves a reader waiting rather than
    /// disconnecting it.
    positions: crate::sessions::Positions,
}

impl SessionActor {
    pub fn new(
        id: Uuid,
        spec: SessionSpec,
        deps: ServerDeps,
        parent: ActorRef<SessionSupervisorCommand>,
        positions: crate::sessions::Positions,
    ) -> Self {
        Self {
            id,
            spec,
            deps,
            parent,
            agents: None,
            positions,
        }
    }

    /// The journal identity of a session: kind `"session"`, id = the uuid.
    pub fn persistence_id_for(session_id: Uuid) -> PersistenceId {
        PersistenceId::new("session", session_id.to_string())
    }

    /// Report a status transition to the supervisor's cache and the live stream.
    async fn report(&self, status: SessionStatus) {
        let _ = self
            .parent
            .tell(SessionSupervisorCommand::SessionStatusChanged {
                id: self.id.to_string(),
                status,
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
        ctx: &ActorContext<SessionCommand>,
        state: &SessionState,
        plan: AgentPlan,
    ) -> ResidentAgent {
        // A subagent and a step journal under their own id; the main agent
        // journals under the session's, because its transcript *is* the
        // session's. The position channel follows the same split.
        let (journal_id, position) = match plan.kind {
            SessionAgentKind::Main => (self.id, self.positions.for_agent(MAIN_AGENT_ID)),
            SessionAgentKind::Sub(id) | SessionAgentKind::Step(id) => {
                (id, self.positions.for_agent(&id.to_string()))
            }
        };
        let key = plan.kind.agent_key();
        let provider = Arc::new(SessionContextProvider {
            runtimes: self.deps.runtimes.provider(
                self.id.to_string(),
                self.spec.vendor.clone(),
                self.spec.clone(),
            ),
            registry: self.deps.provider_registry.clone(),
            mcp: self.deps.mcp.clone(),
            memory: self.deps.memory.clone(),
            step_output_schema: plan.step_output_schema.clone(),
            session_id: self.id,
            kind: plan.kind,
            agent_type: plan.agent_type,
            unattended: self.spec.is_unattended(),
            session: ctx.self_ref(),
            plugins: self.spec.plugins.clone(),
            plugin_library: self.deps.plugins.clone(),
            last_client: Mutex::new(None),
            settings: plan.settings.clone(),
        });
        let mut params = AgentParams::from_def(&AgentRunDef {
            system_prompt: None,
            // The schema is what makes `conclude` typed, and typed output is
            // what a transition condition reads.
            output_schema: plan.step_output_schema.clone(),
            // Asking rides on `conclude`, so only a step that already has one
            // can ask. A step that declares no output ends its turn with plain
            // text, and that text is its output — forcing a terminal tool on it
            // would fail the run the moment the model simply answered.
            allow_ask_user: plan.step_output_schema.is_some() && !self.spec.is_unattended(),
            allow_timers: None,
            max_iterations: plan.settings.max_iterations,
            max_retries: Some(plan.settings.max_retries),
            allowed_tools: plan.settings.allowed_tools.clone(),
        });
        params.interactive = true;
        // Named only when the tool exists: naming a handoff tool the toolbox
        // does not carry would leave the loop watching for a call that can never
        // come. A step deliberately has none — its terminal tool is `conclude`,
        // and naming `ask_user` beside it would stop the loop treating
        // `conclude` as terminal.
        params.optional_handoff_tool = plan.handoff_tool;
        params.thinking_effort = plan
            .settings
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let agent_ctx = AgentRuntimeContext {
            context_provider: provider.clone(),
            position,
            parent: StopHookParent::wrap(ctx.self_ref(), key, provider.clone()),
            session_id: journal_id,
            // Computed from the state this spawn was decided against, never
            // remembered: an agent built after the runtime landed starts ready,
            // and one built before it starts waiting. Changes reach it as the
            // `Runtime` records it is sent anyway.
            ready: Self::runnable(state),
        };
        let resident = ResidentAgent {
            actor: ctx.spawn_persistent(AgentActor::new(agent_ctx, params)),
            provider,
        };
        match plan.kind {
            SessionAgentKind::Main => {
                self.agents = Some(SessionAgents::interactive(resident.clone()));
            }
            SessionAgentKind::Sub(id) | SessionAgentKind::Step(id) => {
                if let Some(agents) = self.agents.as_mut() {
                    agents.insert_sub(id, resident.clone());
                }
            }
        }
        resident
    }

    /// The session's primary agent, spawned once at load.
    fn spawn_main_agent(&mut self, ctx: &ActorContext<SessionCommand>, state: &SessionState) {
        self.spawn_agent(
            ctx,
            state,
            AgentPlan {
                kind: SessionAgentKind::Main,
                settings: self.spec.agent.clone(),
                step_output_schema: None,
                agent_type: None,
                // An unattended session is not offered `ask_user`: nobody is
                // there to answer, so a question would park the run forever.
                handoff_tool: (!self.spec.is_unattended()).then(|| ASK_USER_TOOL.to_string()),
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
        ctx: &ActorContext<SessionCommand>,
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
                if let Some(agent) = self.agents.as_ref().and_then(|a| a.sub(id)) {
                    return Some((AgentKey::Sub(id), agent.actor.clone()));
                }
                // The type comes off the record, not from the caller: a cold
                // node woken to answer a read must run as what it was spawned as.
                let agent_type = state.subagents.node(id)?.agent_type.clone();
                Some((
                    AgentKey::Sub(id),
                    self.spawn_sub_agent_actor(ctx, state, id, agent_type),
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
        ctx: &ActorContext<SessionCommand>,
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
        ctx: &ActorContext<SessionCommand>,
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
        ctx: &ActorContext<SessionCommand>,
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
        ctx: &ActorContext<SessionCommand>,
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
                Some(self.spawn_sub_agent_actor(ctx, state, id, agent_type))
            }
            // A step's actor spawns on demand from the run log, the same way a
            // cold subagent's does: a boundary can owe a result to a step whose
            // actor has since been unloaded.
            AgentKey::Step(id) => self.resolve_step(state, ctx, id).map(|(_, actor)| actor),
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
            spec: &self.spec,
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
        ctx: &ActorContext<SessionCommand>,
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
        ctx: &ActorContext<SessionCommand>,
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
        ctx: &ActorContext<SessionCommand>,
    ) -> CommandEffect<SessionDomainEvent> {
        let (who, end) = match TurnEnd::split(outcome) {
            Ok(pair) => pair,
            // Usage is banked for every agent alike, and always: the tokens
            // were spent whatever became of the turn that spent them. The main
            // agent banks under a fixed name because its journal is keyed by
            // the session id; every other agent banks under its own.
            Err((session_id, NotAnEnd::Usage(usage_total))) => {
                let agent_id = match session_id == self.id {
                    true => MAIN_AGENT_ID.to_string(),
                    false => session_id.to_string(),
                };
                return CommandEffect::persist(vec![SessionDomainEvent::UsageRecorded {
                    at_ms: now_ms(),
                    agent_id,
                    usage_total,
                }]);
            }
            Err((session_id, NotAnEnd::Started)) => {
                return self.on_agent_started(state, session_id).await;
            }
        };
        // In a run, an outcome is a step's or one of a step's subagents'.
        if let Some(run) = state.run.as_ref() {
            return match run.index_of_agent(who) {
                Some(index) => self.on_step_outcome(state, index, end, ctx).await,
                None => self.on_sub_agent_outcome(state, who, end, ctx).await,
            };
        }
        match who == self.id {
            true => self.on_main_outcome(state, end, ctx).await,
            false => self.on_sub_agent_outcome(state, who, end, ctx).await,
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
        if who == self.id {
            self.report(SessionStatus::Running).await;
            return CommandEffect::persist(vec![SessionDomainEvent::TurnBegan { at_ms: now_ms() }]);
        }
        if state.subagents.node(who).is_some() {
            return CommandEffect::persist(vec![SessionDomainEvent::SubAgentRunning {
                at_ms: now_ms(),
                id: who,
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
    type Command = SessionCommand;
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
            SessionDomainEvent::UsageRecorded { .. } => SessionCore::apply(&mut state, &event),
        }
        state
    }

    /// Write what just became durable into the agents' own transcripts, so a
    /// reader sees a lifecycle entry where it happened rather than having to
    /// infer it from the session's status.
    async fn on_events_persisted(&mut self, events: &[SessionDomainEvent], state: &SessionState) {
        self.record_lifecycle(events, state).await;
    }

    async fn handle_command(
        &mut self,
        state: &SessionState,
        cmd: SessionCommand,
        ctx: &mut ActorContext<SessionCommand>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            SessionCommand::Lifecycle(c) => RuntimeLifecycle::handle(self, state, c, ctx).await,
            SessionCommand::Turn(c) => Turns::handle(self, state, c, ctx).await,
            SessionCommand::Run(c) => WorkflowRun::handle(self, state, c, ctx).await,
            SessionCommand::SubAgent(c) => SubAgents::handle(self, state, c, ctx).await,
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

    /// Loading a session spawns its agent and repairs a turn the process died
    /// in. It calls no vendor, starts no run, and drains nothing — an
    /// interrupted assistant turn is over, and queued user messages wait for
    /// the next turn the user starts.
    async fn on_recovery_complete(
        &mut self,
        state: &SessionState,
        ctx: &mut ActorContext<SessionCommand>,
    ) {
        if self.spec.workflow.is_some() {
            // A run has no main agent. Step actors, like subagent actors, stay
            // cold: they spawn on demand for a history read, a retry, or the
            // next step a boundary picks.
            self.agents = Some(SessionAgents::workflow());
        } else {
            self.spawn_main_agent(ctx, state);
        }
        // Each component repairs itself. A self-send rather than direct work,
        // because recovery must not persist and this runs before the first live
        // command — so anything that needs to journal arrives as an ordinary
        // command, down the same path a live one would take.
        let cx = component::ActionCx {
            id: self.id,
            spec: &self.spec,
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
        let repairing = !repairs.is_empty();
        for cmd in repairs {
            let _ = ctx.self_ref().tell(cmd).await;
        }
        // Loading is not a transition, but it is the first moment anyone can
        // learn this status: the supervisor's cache is empty until a session
        // reports, and a page already watching hears nothing otherwise.
        //
        // Skipped when something is being repaired — that command reports the
        // status it lands on, and announcing the pre-repair one first would
        // show a state the session is already leaving.
        if !repairing {
            self.report(state.status.clone()).await;
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
mod testing;
