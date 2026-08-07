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
//! Two of its neighbours live alongside it rather than inside it: [`context`]
//! assembles one turn (runtime handle, toolbox, system prompt) on the agent's
//! own task, and [`hooks`] is the pair of sinks a plugin's hooks report
//! themselves through.

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

use crate::sessions::{
    ask_tool::ASK_USER_TOOL,
    orchestrator::AgentAction,
    spec::{PendingAsk, ServerDeps, SessionSpec, SessionStatus},
    supervisor::SessionSupervisorCommand,
};
use async_trait::async_trait;
use context::{SessionAgentKind, SessionContextProvider, session_run_def};
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_models::now_ms;
use horsie_workflow::{AgentActor, AgentCommand, AgentOutcome, AgentParams, AgentRuntimeContext};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// The agent id a session's primary agent reports usage under.
const MAIN_AGENT_ID: &str = "main";

/// How long a cancel waits for the run to actually finish before giving up.
/// Cancellation is prompt (milliseconds); this is a backstop so a wedged run
/// can never hold the mailbox — and with it the Stop button — hostage.
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The agent actors a session hosts.
///
/// An enum rather than an `Option` plus a map: a session's topology is decided
/// at creation and never changes, and a workflow run has no main agent at all.
enum SessionAgents {
    Interactive {
        main: ActorRef<AgentCommand>,
        subs: HashMap<Uuid, ActorRef<AgentCommand>>,
    },
    /// A workflow run: step agents and their subagents, all keyed by id. There
    /// is no main agent — the definition, not a person, decides who runs.
    Workflow {
        live: HashMap<Uuid, ActorRef<AgentCommand>>,
    },
}

impl SessionAgents {
    fn interactive(main: ActorRef<AgentCommand>) -> Self {
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
    fn main(&self) -> Option<&ActorRef<AgentCommand>> {
        match self {
            Self::Interactive { main, .. } => Some(main),
            Self::Workflow { .. } => None,
        }
    }

    fn sub(&self, id: Uuid) -> Option<&ActorRef<AgentCommand>> {
        match self {
            Self::Interactive { subs, .. } => subs.get(&id),
            Self::Workflow { live } => live.get(&id),
        }
    }

    /// The agent registered under `key`, if it is still resident.
    fn get(&self, key: AgentKey) -> Option<&ActorRef<AgentCommand>> {
        match key {
            AgentKey::Main => self.main(),
            AgentKey::Sub(id) | AgentKey::Step(id) => self.sub(id),
        }
    }

    fn insert_sub(&mut self, id: Uuid, agent: ActorRef<AgentCommand>) {
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
    fn drain_all(&mut self) -> Vec<ActorRef<AgentCommand>> {
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
    /// The step being started, for the instant between the orchestrator naming
    /// it and the `StepStarted` event landing in the log. Only
    /// `perform_run_action` sets it, and it clears it in the same call.
    pending_step: Option<(u32, String)>,
    /// The main agent's context provider, kept so [`Self::cancel_run`] can
    /// reach the runtime client the run already acquired instead of asking
    /// the manager for a fresh one.
    context_provider: Option<Arc<SessionContextProvider>>,
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
            pending_step: None,
            context_provider: None,
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

    /// Spawn the resident agent. Cheap and runtime-free: the provider, toolbox
    /// and system prompt are resolved per run, on the run's own task.
    fn spawn_main_agent(&mut self, ctx: &ActorContext<Self>) {
        let context_provider = Arc::new(SessionContextProvider {
            runtimes: self
                .deps
                .runtimes
                .provider(self.id.to_string(), self.spec.vendor.clone()),
            registry: self.deps.provider_registry.clone(),
            mcp: self.deps.mcp.clone(),
            memory: self.deps.memory.clone(),
            settings: self.spec.agent.clone(),
            step_output_schema: None,
            session_id: self.id,
            kind: SessionAgentKind::Main,
            agent_type: None,
            unattended: self.spec.is_unattended(),
            session: ctx.self_ref(),
            plugins: self.spec.plugins.clone(),
            plugin_library: self.deps.plugins.clone(),
            last_client: Mutex::new(None),
        });
        self.context_provider = Some(context_provider.clone());
        let mut params = AgentParams::from_def(&session_run_def(&self.spec.agent));
        params.interactive = true;
        // Only when the tool exists: an unattended session is not offered
        // `ask_user`, and naming a handoff tool the toolbox does not carry
        // would leave the loop watching for a call that can never come.
        if !self.spec.is_unattended() {
            params.optional_handoff_tool = Some(ASK_USER_TOOL.to_string());
        }
        params.thinking_effort = self
            .spec
            .agent
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let agent_ctx = AgentRuntimeContext {
            context_provider: context_provider.clone(),
            position: self.positions.for_agent(MAIN_AGENT_ID),
            parent: StopHookParent::wrap(ctx.self_ref(), AgentKey::Main, context_provider.clone()),
            session_id: self.id,
        };
        self.agents = Some(SessionAgents::interactive(
            ctx.spawn(AgentActor::new(agent_ctx, params)),
        ));
    }

    /// Resolve an agent selector to its resident actor: `None`/`"main"` for the
    /// primary agent, else a subagent id. A cold node — one in the persisted
    /// tree with no actor since this session loaded — is spawned on demand, so
    /// reading a finished subagent works exactly like reading a live one.
    fn resolve_agent(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<Self>,
        agent_id: Option<&str>,
    ) -> Option<(AgentKey, ActorRef<AgentCommand>)> {
        match agent_id {
            None | Some("main") => self
                .agents
                .as_ref()
                .and_then(SessionAgents::main)
                .cloned()
                .map(|a| (AgentKey::Main, a)),
            Some(raw) => {
                let id = Uuid::parse_str(raw).ok()?;
                if let Some(agent) = self.agents.as_ref().and_then(|a| a.sub(id)) {
                    return Some((AgentKey::Sub(id), agent.clone()));
                }
                // The type comes off the record, not from the caller: a cold
                // node woken to answer a read must run as what it was spawned as.
                let agent_type = state.subagents.node(id)?.agent_type.clone();
                Some((
                    AgentKey::Sub(id),
                    self.spawn_sub_agent_actor(ctx, id, agent_type),
                ))
            }
        }
    }

    fn agent(&self) -> Option<&ActorRef<AgentCommand>> {
        self.agents.as_ref().and_then(SessionAgents::main)
    }

    /// Carry out one orchestrator decision: resume the agent it names, report
    /// the status it implies, and return the events that record it.
    ///
    /// The single place a turn ever begins. Reached only at turn boundaries (a
    /// message arriving while idle, a turn ending, a stop) — never on load,
    /// which is what keeps opening a session free of side effects.
    async fn perform(
        &mut self,
        action: AgentAction,
        state: &SessionState,
        ctx: &ActorContext<Self>,
    ) -> Vec<SessionDomainEvent> {
        let AgentAction::StartTurn {
            who,
            input,
            consumed,
            answered,
            notified,
            mark_running,
        } = action
        else {
            return self.perform_run_action(action, ctx).await;
        };
        match who {
            // A subagent parent waking to consume its children's results. It
            // is skipped, not failed, when its actor cannot be reached: the
            // results stay owed and the next boundary retries.
            // A step is resumed only by `perform_run_action`; the turn path
            // never names one.
            AgentKey::Step(_) => Vec::new(),
            AgentKey::Sub(id) => {
                let agent = match self.agents.as_ref().and_then(|a| a.sub(id)) {
                    Some(agent) => agent.clone(),
                    // A cold node woken for the first time since load: spawn
                    // its resident actor on demand (see `on_recovery_complete`).
                    None => match state.subagents.node(id) {
                        Some(rec) => {
                            let agent_type = rec.agent_type.clone();
                            self.spawn_sub_agent_actor(ctx, id, agent_type)
                        }
                        None => return Vec::new(),
                    },
                };
                if agent
                    .tell(AgentCommand::Resume {
                        results: input.results,
                        message: input.message,
                        subagent_results: input.subagent_results,
                    })
                    .await
                    .is_err()
                {
                    return Vec::new();
                }
                let mut events = Vec::new();
                if let Some(parent) = mark_running {
                    events.push(SessionDomainEvent::SubAgentRunning {
                        at_ms: now_ms(),
                        id: parent,
                    });
                }
                events.extend(notified.into_iter().map(|id| {
                    SessionDomainEvent::SubAgentNotified {
                        at_ms: now_ms(),
                        id,
                    }
                }));
                events
            }
            AgentKey::Main => {
                if let Some(agent) = self.agent() {
                    let _ = agent
                        .tell(AgentCommand::Resume {
                            results: input.results,
                            message: input.message,
                            subagent_results: input.subagent_results,
                        })
                        .await;
                }
                self.report(SessionStatus::Running).await;
                // Tell-then-persist, like the user messages this turn also
                // carries: a crash between the agent's `Run` and this write
                // leaves the result owed, so the next turn re-delivers it.
                // Delivery is at-least-once in that window (the parent may see
                // a result twice), never lost — `spawn_agent`'s stricter
                // persist-then-spawn is the deliberate exception, because an
                // untracked agent is worse than a duplicate.
                let mut events = vec![SessionDomainEvent::TurnBegan {
                    at_ms: now_ms(),
                    consumed,
                    answering: None,
                    answered,
                }];
                events.extend(notified.into_iter().map(|id| {
                    SessionDomainEvent::SubAgentNotified {
                        at_ms: now_ms(),
                        id,
                    }
                }));
                events
            }
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
        ctx: &ActorContext<Self>,
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

    async fn on_agent_outcome(
        &mut self,
        state: &SessionState,
        outcome: AgentOutcome,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        let outcome_session = match &outcome {
            AgentOutcome::Concluded { session_id, .. }
            | AgentOutcome::Asked { session_id, .. }
            | AgentOutcome::Parked { session_id }
            | AgentOutcome::Failed { session_id, .. }
            | AgentOutcome::UsageRecorded { session_id, .. } => *session_id,
        };
        // In a run, an outcome is a step's or one of a step's subagents'.
        if let Some(run) = state.run.as_ref() {
            if let Some(index) = run.index_of_agent(outcome_session) {
                return self.on_step_outcome(state, index, outcome, ctx).await;
            }
            return self
                .on_sub_agent_outcome(state, outcome_session, outcome, ctx)
                .await;
        }
        if outcome_session != self.id {
            return self
                .on_sub_agent_outcome(state, outcome_session, outcome, ctx)
                .await;
        }
        // Usage is always recorded: the tokens were spent whatever became of
        // the turn that spent them.
        if let AgentOutcome::UsageRecorded { usage_total, .. } = outcome {
            return CommandEffect::persist(vec![SessionDomainEvent::UsageRecorded {
                at_ms: now_ms(),
                agent_id: MAIN_AGENT_ID.to_string(),
                usage_total,
            }]);
        }
        let (mut events, drained) = match outcome {
            AgentOutcome::UsageRecorded { .. } => unreachable!("handled above"),
            AgentOutcome::Concluded { .. } => {
                self.report(SessionStatus::Idle).await;
                (
                    vec![SessionDomainEvent::TurnEnded { at_ms: now_ms() }],
                    true,
                )
            }
            AgentOutcome::Asked { asks, .. } => {
                self.report(SessionStatus::AwaitingInput {
                    asks: asks
                        .iter()
                        .map(|a| PendingAsk {
                            tool_call_id: a.tool_call_id.clone(),
                            question: a.question.clone(),
                        })
                        .collect(),
                })
                .await;
                (
                    asks.into_iter()
                        .map(|a| SessionDomainEvent::AskRecorded {
                            at_ms: now_ms(),
                            tool_call_id: a.tool_call_id,
                            question: a.question,
                        })
                        .collect::<Vec<_>>(),
                    // An ask is a turn boundary too: a message queued while the
                    // agent was working becomes the answer.
                    true,
                )
            }
            AgentOutcome::Failed {
                error, terminal, ..
            } => {
                // A runtime that a live vendor cannot produce is the one
                // terminal failure: re-provisioning would silently rebuild a
                // workspace the user believes they still have. Everything else
                // — provider errors, tool errors, a vendor that is merely
                // offline — is a failed turn they can retry.
                if terminal {
                    self.report(SessionStatus::Unrecoverable {
                        reason: error.clone(),
                    })
                    .await;
                    (
                        vec![SessionDomainEvent::SessionFailed {
                            at_ms: now_ms(),
                            reason: error,
                        }],
                        false,
                    )
                } else {
                    self.report(SessionStatus::Failed {
                        reason: error.clone(),
                    })
                    .await;
                    // Deliberately no drain: a stuck cause (expired key, dead
                    // vendor) would otherwise turn three queued messages into
                    // three back-to-back failures. The next message drains them.
                    (
                        vec![SessionDomainEvent::TurnFailed {
                            at_ms: now_ms(),
                            error,
                        }],
                        false,
                    )
                }
            }
            AgentOutcome::Parked { .. } => {
                let error = "agent parked; timers are not supported in sessions".to_string();
                self.report(SessionStatus::Failed {
                    reason: error.clone(),
                })
                .await;
                (
                    vec![SessionDomainEvent::TurnFailed {
                        at_ms: now_ms(),
                        error,
                    }],
                    false,
                )
            }
        };
        if drained {
            let mut next = state.clone();
            for e in &events {
                next = Self::apply_event(next, e.clone());
            }
            events.extend(self.flush_then_drain(&next, ctx).await);
        }
        CommandEffect::persist(events)
    }

    /// Cancel the run in flight, if any, and wait for it to actually be over.
    ///
    /// Waiting matters: the caller is about to record a turn boundary, and a
    /// run that is still winding down can still append to the agent journal.
    async fn cancel_run(&mut self) {
        let Some(agent) = self.agent().cloned() else {
            return;
        };
        // Tell the sandbox to abandon what it is running first, so the wait
        // below is over an already-cancelled call rather than a live one. Uses
        // the client the run itself already acquired in `provide()` — asking
        // the manager for a fresh one would round-trip the vendor on this very
        // mailbox, and a vendor mid-tool-call cannot answer a lifecycle
        // request until the tool call it is relaying resolves.
        if let Some(client) = self
            .context_provider
            .as_ref()
            .and_then(|cp| cp.cached_client())
        {
            client.cancel_in_flight().await;
        }
        let (tx, rx) = oneshot::channel();
        let _ = agent.tell(AgentCommand::Cancel { ack: Some(tx) }).await;
        if tokio::time::timeout(CANCEL_TIMEOUT, rx).await.is_err() {
            tracing::warn!(
                session = %self.id,
                "cancelled run did not finish within {CANCEL_TIMEOUT:?}; proceeding"
            );
        }
    }

    /// Stop every agent this session hosts. Used when the session unloads.
    async fn stop_agents(&mut self) {
        let Some(mut agents) = self.agents.take() else {
            return;
        };
        for agent in agents.drain_all() {
            // Cancel first: a stopped mailbox makes the run task's next persist
            // fail, but an in-flight tool call would run to completion first.
            let _ = agent.tell(AgentCommand::Cancel { ack: None }).await;
            let _ = agent.tell(AgentCommand::Shutdown).await;
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
            SessionDomainEvent::MessageQueued { .. }
            | SessionDomainEvent::TurnBegan { .. }
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
    async fn on_events_persisted(&mut self, events: &[SessionDomainEvent], _state: &SessionState) {
        self.record_lifecycle(events).await;
    }

    async fn handle_command(
        &mut self,
        state: &SessionState,
        cmd: SessionCommand,
        ctx: &mut ActorContext<Self>,
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
    async fn on_recovery_complete(&mut self, state: &SessionState, ctx: &mut ActorContext<Self>) {
        if self.spec.workflow.is_some() {
            // A run has no main agent. Step actors, like subagent actors, stay
            // cold: they spawn on demand for a history read, a retry, or the
            // next step a boundary picks.
            self.agents = Some(SessionAgents::workflow());
        } else {
            self.spawn_main_agent(ctx);
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
