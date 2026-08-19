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
//! [`types`]. [`context`] is not one of them: it assembles a turn on the
//! *agent's* task rather than on this mailbox, which is what keeps a
//! thirty-second toolbox build from blocking a cancel.
//!
//! What is *not* here any more is a component per kind of agent. A session
//! hosts runners; which runner owns an agent is `state.agents[&id]`, and the
//! same lookup answers what used to need three registries probed in a
//! load-bearing order.

use crate::runtime_manager::RuntimeError;
use crate::sessions::UserMessageError;
use crate::sessions::runners::action::Action;
use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::runners::loading::AgentRole;
use crate::sessions::runners::message::ChildOutcome;
use crate::sessions::runners::state::SessionState as RunnerSessionState;
use crate::sessions::runners::{Emit, Runner, RunnerEvent, SessionView};
use horsie_actor::ReplyTo;
pub(crate) mod context;
mod core;
/// Visible to the crate for [`hooks::SessionHookSink`] alone: the runtime
/// capability attaches it to the client it acquires, and the sink routes into
/// this actor's mailbox, so it cannot live anywhere else.
pub(crate) mod hooks;
mod reads;
mod types;

pub use types::*;

/// The session's state and its journal, both the runner tree's now. Re-exported
/// here because every sibling in this module reaches them through
/// `session_actor`, and a session's state is what this actor *is*.
pub use crate::sessions::runners::SessionState;
pub use crate::sessions::runners::state::SessionEvent;

use core::SessionCore;
use hooks::{HookRouting, StopHookParent};
use reads::Reads;

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
    sync::{Arc, Weak},
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// The path segment and usage key naming a session's primary agent, as opposed
/// to a subagent's uuid. One spelling, shared by every agent-scoped route and
/// by the actor that resolves them.
pub const MAIN_AGENT_ID: &str = "main";

/// What a typed branch asked for, if the line is one.
///
/// Pure and separate from acting on it, so the table of what counts as a fork
/// command is testable with no actor in sight — and so classification happens
/// before the reply changes hands.
#[must_use]
fn fork_command(text: &str) -> Option<(crate::sessions::runners::action::ForkMode, String)> {
    use crate::sessions::runners::action::ForkMode;
    let (builtin, args) = horsie_support::plugin::commands::parse_invocation(text, '/').and_then(
        |(name, args)| horsie_support::plugin::builtins::builtin(name).map(|b| (b, args)),
    )?;
    let mode = match builtin.name {
        "fork" => ForkMode::Copy,
        "summary-n-fork" => ForkMode::Summary,
        _ => return None,
    };
    Some((mode, args.trim().to_string()))
}

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

/// Everything that differs between the three kinds of agent a session spawns.
///
/// The rest — the runtime provider, the plugin library, the MCP and memory
/// services, the session's own mailbox — is identical for all three and lives on
/// What [`Action::StartAgent`] carries, kept together on the way to the
/// spawner.
///
/// The action's own fields, moved rather than spread across a parameter list:
/// the session *performs* what a runner decided, so re-deriving any of it here
/// would be a second answer to a question already settled.
struct StartAgent {
    agent: AgentId,
    equipment: crate::agent_loop::capabilities::Capabilities,
    settings: crate::sessions::spec::AgentSettings,
    agent_type: Option<String>,
    first: crate::sessions::runners::action::FirstInput,
}

/// the actor, which is why one spawner can serve them all.
struct AgentPlan {
    /// Who is being started. One id, in the runners' flat space.
    agent: AgentId,
    /// What it is, for the four things that are not identity: whether its
    /// runtime client is scoped, its prompt suffix, which lifecycle entry
    /// opens its log, and whether setup narrates.
    role: AgentRole,
    /// Whose settings this agent runs under: the session's, or a step's own
    /// preset. This is also where its model and thinking effort come from.
    settings: crate::sessions::spec::AgentSettings,
    /// What this agent can do, assembled by whoever planned it.
    ///
    /// Here rather than derived from the kind downstream, because the extras
    /// only the spawn site knows about — a step's typed `submit_result` and
    /// whether that step may ask — are part of the same list. It is the list a
    /// runner will hold and hand to each agent it starts.
    equipment: crate::agent_loop::capabilities::Capabilities,
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
    /// The agent actors this session hosts, resident while this actor is
    /// loaded.
    ///
    /// One flat map, because one flat id space. Which runner owns an agent is
    /// `state.agents[&id]`, and the topology the old enum encoded — a workflow
    /// run has no main agent — is now just which runner is the root.
    agents: HashMap<AgentId, ResidentAgent>,
    /// Runners this actor has already emitted a `RunnerCreated` for, but whose
    /// persist may not have folded yet.
    ///
    /// The folded state is the real record and survives a reload; this covers
    /// only the window between answering the caller and the journal landing.
    /// A capability re-asking after a crash names the child it already
    /// journaled, so on a *reload* the state answers — but two asks inside one
    /// process can both see a state that has not folded the first yet, and that
    /// is what doubles the fleet.
    created: std::collections::HashSet<RunnerId>,
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
            agents: HashMap::new(),
            created: std::collections::HashSet::new(),
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
        // Nothing is spawned here and no component is asked to repair itself.
        // `Runner::actions` is pure and idempotent and is called at every
        // boundary, so creation and recovery take the same path: a runner that
        // needs its agent started asks for it whether that state arrived a
        // millisecond ago or from a journal replayed after a restart.
        //
        // The four `on_load` repairs this replaces existed because the old
        // shape had no such path — a `run()` that fired once needed a second
        // entry for recovery, and the suppression that implies is where the
        // bugs lived.
        //
        // One case `actions()` genuinely cannot see: an agent that was
        // `started` when the process died. Nothing in the fold distinguishes
        // it from one still working, because it *is* still marked working. The
        // agent itself closes that — it journals its own interruption at its
        // own recovery and reports it — so what the session owes is to bring
        // it back.
        //
        // A conversation's is brought back by the next message addressed to
        // it. A run's is not — nobody sends a run a message — so the session
        // says it instead, on the one boundary that is a load. See
        // [`Self::interrupted_at_load`].
        let _ = self
            .me(ctx)
            .tell(SessionCommand::Core(CoreCommand::Advance))
            .await;
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
        let forks = crate::sessions::runners::reads::fork_rows(state);
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
        let status = crate::sessions::runners::reads::session_status(state);
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
        // Every agent journals under its own id, including the root
        // conversation's — its transcript is no longer the session's. `main`
        // survives as a *revision channel name* only, because
        // `AwaitAgentRevision` keys its long poll on that exact string.
        let journal_id = plan.agent.as_uuid();
        let revision = match plan.role {
            AgentRole::Root => revisions.publishing(MAIN_AGENT_ID),
            AgentRole::Fork | AgentRole::Sub | AgentRole::Step => {
                revisions.publishing(&journal_id.to_string())
            }
        };
        // Its name under this session, and the id it is addressed by.
        let name = journal_id.to_string();
        // Built once, here, rather than per turn: it owns the cache of the
        // client this agent's last load acquired, which `cancel_agent` and the
        // `Stop` hooks both read.
        let loading = context::loading_for(
            plan.agent,
            plan.role,
            self.me(ctx),
            self.id,
            context::LoadingDeps {
                runtimes: self.deps().runtimes.provider(
                    self.id.to_string(),
                    // The provision this run speaks to. A session that has never
                    // provisioned has none, and the empty string is what the
                    // acquisition below will fail on rather than silently
                    // addressing some other sandbox.
                    state
                        .runners
                        .values()
                        .find_map(|r| match &r.state {
                            crate::sessions::runners::RunnerState::Runtime(rt) => {
                                rt.provisioned_at_ms
                            }
                            crate::sessions::runners::RunnerState::Conversation(_)
                            | crate::sessions::runners::RunnerState::SubAgent(_)
                            | crate::sessions::runners::RunnerState::Workflow(_) => None,
                        })
                        .map(|at| at.to_string())
                        .unwrap_or_default(),
                    // A create is still outstanding. The journal is the only thing
                    // that knows, and it has to say so: a substrate that has not
                    // reported the object yet is indistinguishable from one with
                    // nothing there, and the difference is between waiting for a
                    // runtime and declaring it gone.
                    matches!(
                        crate::sessions::runners::reads::session_status(state),
                        SessionStatus::Provisioning
                    ),
                    self.spec().vendor.clone(),
                    self.spec().clone(),
                ),
                registry: self.deps().provider_registry.clone(),
                mcp: self.deps().mcp.clone(),
                memory: self.deps().memory.clone(),
                services: Some(self.services().clone()),
                plugin_library: self.deps().plugins.clone(),
            },
        );
        // The same list twice, deliberately: the provider equips the agent from
        // it — the toolbox layers each capability pushes — and the agent itself
        // holds it, journals `Equipped` once, and advertises what its
        // capabilities answer for. Two copies of one decision, never two
        // decisions.
        let capabilities = plan.equipment.clone();
        let provider = Arc::new(SessionContextProvider {
            loading,
            equipment: plan.equipment,
            role: plan.role,
            agent: plan.agent,
            agent_type: plan.agent_type,
            plugins: self.spec().plugins.clone(),
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
        params.requires_result = matches!(plan.role, AgentRole::Step);
        params.capabilities = capabilities;
        params.thinking_effort = plan
            .settings
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let agent_ctx = AgentRuntimeContext {
            context_provider: provider.clone(),
            revision,
            parent: StopHookParent::wrap(self.me(ctx), plan.agent, provider.clone()),
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
        // One map, one key. Which runner this agent belongs to is the session
        // state's business, not this map's.
        self.agents.insert(plan.agent, resident.clone());
        Some(resident)
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
    async fn cancel_agent(&mut self, agent: AgentId) {
        let Some(agent) = self.agents.get(&agent).cloned() else {
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

    /// Cancel every agent this session has working.
    ///
    /// Every one, not just the root's: a run used to be skipped here entirely,
    /// so deleting one mid-step left its sandbox call running. With one flat
    /// map there is no kind to skip — whoever is resident is cancelled.
    async fn cancel_in_flight(&mut self, _state: &RunnerSessionState) {
        for agent in self.agents.keys().copied().collect::<Vec<_>>() {
            self.cancel_agent(agent).await;
        }
    }

    /// The runners a session is created with: its root, then its sandbox.
    ///
    /// **Order is load-bearing.** `SessionState::apply` makes the *first*
    /// runner created the root, so the conversation or the run must be
    /// journaled before the runtime — otherwise the sandbox becomes the
    /// session's status, and a session reads `Provisioning` for ever.
    ///
    /// `SessionKind` shrinks to exactly this one job: deciding which runner is
    /// the root.
    fn birth_runners(&self, spec: &SessionSpec) -> Vec<SessionEvent> {
        use crate::sessions::runners::action::{RunnerArgs, WorkflowSource};
        use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
        let at_ms = now_ms();
        let root = RunnerId::new_v4();
        // Minted once. A conversation's agent is named twice below — by the
        // args its runner is born from, and by the assembly its capabilities
        // are equipped against — and two uuids would equip the root agent's
        // fork and subagent capabilities in the name of an agent that does not
        // exist. `create_child` reads one id into both for the same reason.
        let main = AgentId::new_v4();
        let (kind, args) = match &spec.kind {
            SessionKind::Agent { .. } => (
                RunnerKind::Conversation,
                RunnerArgs::Conversation {
                    agent: main,
                    seed: None,
                    message: String::new(),
                    settings: Box::new(
                        spec.agent_settings()
                            .cloned()
                            .unwrap_or_else(crate::sessions::runners::empty_settings),
                    ),
                },
            ),
            SessionKind::Workflow { .. } => {
                let Some(run) = spec.workflow_run() else {
                    tracing::error!(session = %self.id, "a workflow session with no graph");
                    return Vec::new();
                };
                (
                    RunnerKind::Workflow,
                    RunnerArgs::Workflow {
                        source: WorkflowSource::Graph(run.clone()),
                        input: run.input.clone(),
                    },
                )
            }
        };
        let caps = crate::sessions::runners::assemble(
            kind,
            &crate::sessions::runners::Assembly {
                settings: &spec
                    .agent_settings()
                    .cloned()
                    .unwrap_or_else(crate::sessions::runners::empty_settings),
                agent: main,
                depth: 0,
                unattended: spec.is_unattended(),
                fork: None,
                agent_type: None,
            },
        );
        let Ok(state) = crate::sessions::runners::birth::born(args, caps, root) else {
            return Vec::new();
        };
        vec![
            SessionEvent::RunnerCreated {
                id: root,
                kind,
                parent: None,
                state: Box::new(state),
                at_ms,
            },
            SessionEvent::RunnerCreated {
                id: RunnerId::new_v4(),
                kind: RunnerKind::Runtime,
                parent: None,
                state: Box::new(crate::sessions::runners::birth::runtime_born()),
                at_ms,
            },
        ]
    }

    /// Carry out one runner's decision and return the events that record it.
    ///
    /// No turn ever begins here: an agent owns its queue and decides when that
    /// queue becomes a turn. What is left is the work only the session can do —
    /// starting an agent, creating a child, delivering a report, acquiring the
    /// sandbox.
    async fn perform(
        &mut self,
        runner: RunnerId,
        action: Action,
        state: &RunnerSessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        match action {
            Action::StartAgent {
                agent,
                equipment,
                settings,
                agent_type,
                first,
            } => match self
                .start_agent(
                    runner,
                    StartAgent {
                        agent,
                        equipment,
                        settings: *settings,
                        agent_type,
                        first,
                    },
                    state,
                    ctx,
                )
                .await
            {
                Some(_) => self.start_events(runner, agent, state),
                None => Vec::new(),
            },
            Action::CreateChild {
                id,
                kind,
                args,
                parent,
            } => self.create_child(id, kind, args, parent, state, ctx).await,
            Action::Deliver { to, from, part } => self.deliver(to, from, *part, state, ctx).await,
            Action::Cancel { agent } => {
                self.cancel_agent(agent).await;
                Vec::new()
            }
            Action::Provision => self.provision(runner, ctx).await,
            // Answered on the call that is waiting, never journaled: a refusal
            // is not something that happened to this session.
            Action::Reply { text } => {
                tracing::debug!(session = %self.id, %runner, text, "a runner answered a caller");
                Vec::new()
            }
        }
    }

    /// Start one agent for a runner, and register it.
    ///
    /// The single spawner. `AgentRole` decides the four things the old
    /// `AgentKey` decided besides identity — whether the runtime client is
    /// scoped, the prompt suffix, which lifecycle entry opens the log, and
    /// whether setup narrates.
    async fn start_agent(
        &mut self,
        runner: RunnerId,
        start: StartAgent,
        state: &RunnerSessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Option<SessionEvent> {
        let StartAgent {
            agent,
            equipment,
            settings,
            agent_type,
            first,
        } = start;
        let record = state.record(runner)?;
        let role = AgentRole::of(record.kind, runner == state.root);
        let resident = self.spawn_agent(
            ctx,
            state,
            AgentPlan {
                agent,
                role,
                settings,
                equipment,
                agent_type,
            },
        )?;
        if let crate::sessions::runners::action::FirstInput::Text(text) = first {
            let _ = resident
                .actor
                .tell(AgentCommand::Enqueue {
                    item: Incoming::User {
                        id: Uuid::new_v4().to_string(),
                        text,
                    },
                    ack: None,
                })
                .await;
        }
        Some(SessionEvent::AgentStarted { runner, agent })
    }

    /// The pair of events one started agent produces.
    ///
    /// Two, not one, and both are needed: `AgentStarted` is the session's — it
    /// is what makes `state.agents[&agent]` resolve — and the runner's own
    /// `Started` is what stops `actions()` asking again at the next boundary.
    fn start_events(
        &self,
        runner: RunnerId,
        agent: AgentId,
        state: &RunnerSessionState,
    ) -> Vec<SessionEvent> {
        let mut events = vec![SessionEvent::AgentStarted { runner, agent }];
        if let Some(record) = state.record(runner)
            && let Some(event) = record.state.started_event()
        {
            events.push(SessionEvent::Runner {
                id: runner,
                event: Box::new(event),
                at_ms: now_ms(),
            });
        }
        events
    }

    /// Create a child runner, durable before its agent exists.
    ///
    /// Persist-then-spawn: a crash between the two replays as no child at all,
    /// which is strictly better than an agent nothing tracks. The id is the
    /// capability's, not the session's, so the event it journaled and the
    /// action it returned name the same child.
    async fn create_child(
        &mut self,
        id: RunnerId,
        kind: crate::sessions::runners::ids::RunnerKind,
        args: crate::sessions::runners::action::RunnerArgs,
        parent: AgentId,
        state: &RunnerSessionState,
        _ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        use crate::sessions::runners::action::RunnerArgs;
        // The child's settings travel on the args, because the asker fixed
        // them: a worker inherits its caller's, a fork the conversation's. A
        // run carries none — each step resolves its own preset, which is what
        // lets step 1 run on a large model and step 2 on a small one.
        let (settings, agent, agent_type) = match &args {
            RunnerArgs::SubAgent {
                agent,
                settings,
                agent_type,
                ..
            } => ((**settings).clone(), *agent, agent_type.clone()),
            RunnerArgs::Conversation {
                agent, settings, ..
            } => ((**settings).clone(), *agent, None),
            // A run carries none: each step resolves its own preset. What it
            // is equipped with meanwhile is the asker's, which is what an
            // invoked run inherits until its first step overrides it.
            RunnerArgs::Workflow { .. } => {
                let Some(settings) = crate::sessions::runners::reads::settings_of(state, parent)
                else {
                    tracing::warn!(session = %self.id, %parent, "a run was asked for by an agent with no settings");
                    return Vec::new();
                };
                (settings.clone(), parent, None)
            }
        };
        let caps = crate::sessions::runners::assemble(
            kind,
            &crate::sessions::runners::Assembly {
                settings: &settings,
                agent,
                // The asking agent's depth plus one — the child's own, walked
                // from the runner that owns the agent that asked. Hardcoding
                // zero equipped every worker as if it were the main agent, so
                // the depth gate could never refuse anything and the tree
                // nested without limit.
                depth: state
                    .runner_of(parent)
                    .map_or(0, |runner| state.depth_of(runner) + 1),
                // A child of an unattended session is unattended too. Nobody
                // is watching either of them, and a question that parks for
                // ever parks just as hard one level down.
                unattended: self.spec().is_unattended(),
                fork: match &args {
                    // A fork names *itself*, not the session it branched from.
                    RunnerArgs::Conversation { seed: Some(_), .. } => Some(id),
                    RunnerArgs::Conversation { seed: None, .. }
                    | RunnerArgs::SubAgent { .. }
                    | RunnerArgs::Workflow { .. } => None,
                },
                agent_type,
            },
        );
        match crate::sessions::runners::birth::born(args, caps, id) {
            Ok(state) => vec![SessionEvent::RunnerCreated {
                id,
                kind,
                parent: Some(parent),
                state: Box::new(state),
                at_ms: now_ms(),
            }],
            Err(error) => {
                tracing::warn!(session = %self.id, runner = %id, error, "a child could not be created");
                Vec::new()
            }
        }
    }

    /// Acquire the sandbox this session runs in.
    ///
    /// Asked for by the runtime runner the moment it is `Pending`, which is
    /// what gives provisioning an answer at recovery: before, it was driven by
    /// a lifecycle command sent exactly once, so a session whose sandbox died
    /// between the ask and the answer sat `Pending` with nothing to restart it.
    ///
    /// The gate that used to live here — "only from the three states that mean
    /// no runtime has ever been confirmed" — is the runner's own `actions()`
    /// now, which asks only from `Pending` and a non-terminal `Failed`.
    async fn provision(
        &mut self,
        runner: RunnerId,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        use crate::sessions::runners::runtime;
        let runtimes = self.deps().runtimes.clone();
        let session = self.id.to_string();
        let vendor = self.spec().vendor.clone();
        let spec = self.spec().clone();
        let me = self.me(ctx);
        // Minted here and journaled below in the same breath, so the sandbox
        // this create starts and the entry that records it agree on one name.
        // Reading the clock twice would give the spawned task an identity the
        // journal never saw.
        let at_ms = now_ms();
        let incarnation = at_ms.to_string();
        // Off the mailbox: a real create runs for minutes, and this actor has
        // to keep answering reads, stops and deletes throughout. The status the
        // runner just folded is what holds the turn back meanwhile.
        tokio::spawn(async move {
            let (error, terminal, detail) = match runtimes
                .create(&session, &incarnation, &vendor, &spec)
                .await
            {
                Ok(detail) => (None, false, detail),
                // Exactly the split `get` makes: only a live vendor refusing to
                // produce the runtime is terminal. An offline vendor or a failed
                // token mint is a bad moment, not a dead session.
                Err(e @ RuntimeError::Gone(_)) => (Some(e.to_string()), true, None),
                Err(e @ (RuntimeError::Unavailable(_) | RuntimeError::Provision(_))) => {
                    (Some(e.to_string()), false, None)
                }
            };
            // Before the outcome, and separately from it: the vendor described
            // the runtime it accepted, and that sentence belongs to the wait
            // rather than to how the wait ended.
            if let Some(detail) = detail {
                let _ = me
                    .tell(SessionCommand::Core(CoreCommand::RuntimeEvent {
                        runner,
                        event: runtime::Event::Progress { detail },
                    }))
                    .await;
            }
            let event = match error {
                None => runtime::Event::Succeeded { at_ms },
                Some(error) => runtime::Event::Failed { error, terminal },
            };
            let _ = me
                .tell(SessionCommand::Core(CoreCommand::RuntimeEvent {
                    runner,
                    event,
                }))
                .await;
        });
        vec![SessionEvent::Runner {
            id: runner,
            event: Box::new(RunnerEvent::Runtime(runtime::Event::Started)),
            at_ms,
        }]
    }

    /// Put a finished child's report in the queue of the agent owed it.
    ///
    /// Tell-then-persist: a crash between the enqueue and the acknowledgement
    /// leaves the report still owed, so the next boundary re-delivers it.
    /// At-least-once in that window, never lost — `create_child`'s stricter
    /// persist-then-spawn is the deliberate exception, because an untracked
    /// agent is worse than a duplicate.
    ///
    /// Skipped, not failed, when the agent cannot be reached: the report stays
    /// owed and the next boundary tries again.
    async fn deliver(
        &mut self,
        to: AgentId,
        from: RunnerId,
        part: horsie_models::agent::SubAgentResultPart,
        state: &RunnerSessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        let Some(agent) = self.reach(to, state, ctx) else {
            return Vec::new();
        };
        let _ = agent
            .tell(AgentCommand::Enqueue {
                item: Incoming::SubAgent {
                    id: from.to_string(),
                    part: Box::new(part),
                },
                ack: None,
            })
            .await;
        // The acknowledgement is the *capability's*, not the session's: the
        // parent's `SubAgentCapability` folds `Reported` and drops the child
        // from `outstanding`. One fact, one writer.
        Vec::new()
    }

    /// The mailbox of one of this session's agents, spawning a cold one on
    /// demand. `None` when no runner owns that id.
    ///
    /// The three-registry probe this replaces answered "what kind of agent is
    /// this uuid" by trying the run log, then the fork roster, then the
    /// subagent forest — an order that was load-bearing, and getting it wrong
    /// made a fork of a fork read as a fork of a subagent.
    fn reach(
        &mut self,
        agent: AgentId,
        state: &RunnerSessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Option<ActorRef<AgentCommand>> {
        if let Some(resident) = self.agents.get(&agent) {
            return Some(resident.actor.clone());
        }
        let runner = state.runner_of(agent)?;
        let record = state.record(runner)?;
        let settings = record.state.settings(agent)?.clone();
        let role = AgentRole::of(record.kind, runner == state.root);
        let equipment = record.state.capabilities()?.clone();
        self.spawn_agent(
            ctx,
            state,
            AgentPlan {
                agent,
                role,
                settings,
                equipment,
                agent_type: None,
            },
        )
        .map(|r| r.actor)
    }
    /// What a runner may know about the session around it.
    ///
    /// Built per runner because `depth` is a walk up that runner's own parent
    /// chain; the other two are the session's and read the same for everybody.
    fn view(&self, state: &RunnerSessionState, runner: RunnerId) -> SessionView {
        SessionView {
            runtime_ready: state.runtime_ready(),
            depth: state.depth_of(runner),
            active_agents: state.active_agents(),
        }
    }

    /// Whether any runner has work in flight, so the session must not unload.
    /// This is what keeps a forty-minute tool call from being unloaded out from
    /// under itself.
    fn busy(&self, state: &RunnerSessionState) -> bool {
        state.runners.values().any(|r| r.state.busy())
    }

    /// Everything every runner wants started, given the state as it now is.
    ///
    /// A concatenation, not a negotiation: a runner returns only work it owns,
    /// so there is nothing to reconcile. The runtime gate that used to stand in
    /// front of this moved into `RunnerState::actions`, where the one runner
    /// exempt from it — the sandbox itself — says so rather than being special
    /// cased by its caller.
    fn next_actions(&self, state: &RunnerSessionState) -> Vec<(RunnerId, Action)> {
        state
            .runners
            .iter()
            .flat_map(|(id, rec)| {
                let view = self.view(state, *id);
                rec.state
                    .actions(&view)
                    .into_iter()
                    .map(move |action| (*id, action))
            })
            .collect()
    }

    /// Every runner that has reached a terminal status and has not been
    /// recorded as reaching one.
    ///
    /// Derived from the runner's own slice rather than written twice: the
    /// previous shape kept a fork's roster entry and the session's status as
    /// separate variables, and they disagreed. `Runner::finished` is the single
    /// answer and this is its only caller — without it no runner was ever
    /// terminal, so `owed` found nothing and **no worker's report and no run's
    /// output ever reached the agent that asked for it**.
    fn endings(&self, state: &RunnerSessionState) -> Vec<SessionEvent> {
        let at_ms = now_ms();
        state
            .runners
            .iter()
            .filter(|(_, rec)| !rec.status.is_terminal())
            .filter_map(|(id, rec)| {
                Runner::finished(&rec.state).map(|status| SessionEvent::RunnerEnded {
                    id: *id,
                    status,
                    at_ms,
                })
            })
            .collect()
    }

    /// Every finished child whose creator has not been told yet.
    ///
    /// The scan the two-batch delivery rests on: a runner that is terminal and
    /// has a parent offers its `outcome()` to that parent's capabilities, and
    /// the capability's own `outstanding` decides whether the report is still
    /// owed. So there is no `notified` flag to disagree with it — a crash
    /// between telling and persisting replays as a report still outstanding,
    /// and the next boundary finds it here again.
    ///
    /// `is_terminal`, not `== Done`: a worker that *failed* is owed a report
    /// too, and a `Done`-only scan would leave its parent waiting for ever.
    fn owed(&self, state: &RunnerSessionState) -> Vec<(RunnerId, AgentId, ChildOutcome)> {
        state
            .runners
            .iter()
            .filter(|(_, rec)| rec.status.is_terminal())
            .filter_map(|(id, rec)| {
                let parent = rec.parent?;
                let outcome = rec.state.outcome()?;
                Some((*id, parent, outcome))
            })
            .collect()
    }

    /// A runner's events, addressed and stamped.
    ///
    /// `at_ms` is read here — one of the few places it may be — because it is a
    /// fact about the journal entry rather than about what was decided. A
    /// decision is made once and folded any number of times, so a clock inside
    /// a fold would give a replay different timestamps from the live run.
    fn wrap(&self, runner: RunnerId, emit: Emit) -> Vec<SessionEvent> {
        let at_ms = now_ms();
        emit.events
            .into_iter()
            .map(|event| SessionEvent::Runner {
                id: runner,
                event: Box::new(event),
                at_ms,
            })
            .collect()
    }

    /// Everything startable at this boundary, performed in order, each seeing
    /// the state the previous one produced.
    ///
    /// Deliveries first: a parent waiting on its children is work already in
    /// flight, and a next turn can wait a boundary. This is also the re-drive
    /// point that makes delivery at-least-once — a report the last process told
    /// but never acknowledged is still `owed` here.
    async fn flush_then_drain(
        &mut self,
        state: &RunnerSessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        let mut next = state.clone();
        // Endings first: a runner that has reached one is what makes it a
        // candidate to report, and nothing else journals them.
        for e in self.endings(&next) {
            next.apply(&e);
            events.push(e);
        }
        for (child, parent, outcome) in self.owed(&next) {
            let produced = self
                .offer_to_parent(child, parent, &outcome, &next, ctx)
                .await;
            for e in &produced {
                next.apply(e);
            }
            events.extend(produced);
        }
        for (runner, action) in self.next_actions(&next) {
            let produced = self.perform(runner, action, &next, ctx).await;
            for e in &produced {
                next.apply(e);
            }
            events.extend(produced);
        }
        events
    }

    /// Hand a finished child's outcome to the agent that created it.
    ///
    /// Tell first, record afterwards. The agent owns the acknowledgement —
    /// its `sub_agent`/`workflow` state is the one place a child is
    /// outstanding, and the report and the ack are journaled there together —
    /// so a crash between the two halves here replays as a child still owed,
    /// and the offer is repeated. What the session records is only that it has
    /// offered, which is what stops it re-offering at every boundary for the
    /// life of the session: an offer reaches its parent by *waking* it, so an
    /// un-recorded hand-over would rehydrate a cold conversation for ever on
    /// behalf of a worker that finished days ago.
    ///
    /// Which capability the outcome belongs to is not decided here. It is
    /// offered to the agent, and the capability holding that child claims it —
    /// a child nobody is holding falls through, which is what stops two
    /// capabilities plausibly claiming one report.
    async fn offer_to_parent(
        &mut self,
        child: RunnerId,
        parent: AgentId,
        outcome: &ChildOutcome,
        state: &RunnerSessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        let Some(kind) = state.record(child).map(|record| record.kind) else {
            return Vec::new();
        };
        let event = match kind {
            RunnerKind::SubAgent => {
                RunnerEvent::SubAgent(crate::sessions::runners::subagent::Event::Reported)
            }
            RunnerKind::Workflow => {
                RunnerEvent::Workflow(crate::sessions::runners::workflow::Event::Reported)
            }
            // Neither reports: a conversation is never over, and the runtime
            // owns no agent. `owed` cannot reach either — both answer `None`
            // to `Runner::outcome` — so this is the arm that keeps that fact
            // checked by the compiler rather than asserted in a comment.
            RunnerKind::Conversation | RunnerKind::Runtime => return Vec::new(),
        };
        let Some(agent) = self.reach(parent, state, ctx) else {
            return Vec::new();
        };
        if agent
            .tell(AgentCommand::ChildMoved {
                msg: crate::sessions::runners::message::ChildMsg::Outcome {
                    child,
                    outcome: outcome.clone(),
                },
            })
            .await
            .is_err()
        {
            // Unreachable right now, so still owed. The next boundary tries
            // again rather than recording a hand-over that never happened.
            return Vec::new();
        }
        self.wrap(
            child,
            Emit {
                events: vec![event],
                actions: Vec::new(),
            },
        )
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
        state: &RunnerSessionState,
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

    /// Route one agent's outcome to the component that owns what it means.
    ///
    /// The one command routed by *identity* rather than by variant: the same
    /// `Concluded` means "the turn is over", "this step's output picks the next
    /// step", or "tell the parent its child is done", depending only on which
    /// agent sent it. Answering the two non-ending reports first is what lets
    /// each of those three read the outcome as a turn that ended, rather than
    /// re-answering variants that mean the same thing to all of them.
    /// Put a person's message in an agent's queue.
    ///
    /// The session's part is only to resolve the addressee — spawning a cold
    /// agent if need be — and to title an unnamed session from its first
    /// message. The message itself never touches session state: it is addressed
    /// to an agent, and that is where it is stored.
    ///
    /// `/fork` is deliberately not intercepted here any more. It is a
    /// `Command` its agent's `ForkCapability` claims, which is what lets the
    /// branch point be the *asking agent's* log sequence — a fact this actor
    /// cannot see.
    async fn on_user_message(
        &mut self,
        state: &RunnerSessionState,
        agent_id: Option<String>,
        text: String,
        reply: ReplyTo<Result<MessageAccepted, UserMessageError>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        if let SessionStatus::Unrecoverable { reason } =
            crate::sessions::runners::reads::session_status(state)
        {
            let _ = reply.send(Err(UserMessageError::Unrecoverable(reason)));
            return CommandEffect::none();
        }
        let Some(agent_id) = crate::sessions::runners::reads::resolve(state, agent_id.as_deref())
        else {
            // A run has no root conversation, so an unaddressed message there
            // reaches nobody. Naming a step is fine — that agent exists and can
            // be spoken to like any other.
            let _ = reply.send(Err(UserMessageError::NotFound));
            return CommandEffect::none();
        };
        let Some(agent) = self.reach(agent_id, state, ctx) else {
            let _ = reply.send(Err(UserMessageError::NotFound));
            return CommandEffect::none();
        };
        // A fork command never becomes a prompt. Recognised here, because this
        // is where every built-in is caught before the text can be treated as
        // one; decided *there*, by the capability, because the branch point is
        // the asking agent's own log sequence and this actor cannot see it.
        if let Some((mode, message)) = fork_command(text.trim()) {
            self.branch(agent, mode, message, reply);
            return self.persist_and_advance(state, Vec::new(), ctx).await;
        }
        // An unnamed session is titled from its first message, once. The rule is
        // `SessionCore`'s — a session's name is its own bookkeeping, not the
        // turn's — so this only says when to apply it.
        self.title_from_first_message(&text).await;

        let id = Uuid::new_v4().to_string();
        // A built-in is resolved here, before anything treats the text as a
        // prompt: `/compact` asks the server to do something and must never
        // reach `expand_invocation`, a template, or the model. Consulted ahead
        // of the plugin catalogue, so an installed bundle cannot take over a
        // control the product owns.
        let item = match horsie_support::plugin::commands::parse_invocation(text.trim(), '/')
            .and_then(|(name, args)| {
                horsie_support::plugin::builtins::builtin(name).map(|b| (b, args))
            }) {
            Some((builtin, args)) if builtin.name == "compact" => Incoming::Compact {
                id: id.clone(),
                instructions: (!args.trim().is_empty()).then(|| args.trim().to_string()),
            },
            // Every other built-in, present and future — `/fork` and
            // `/summary-n-fork` were answered above. Reaching here means the
            // table names something this match does not handle, which is a bug
            // rather than a message: sending it on as a prompt would show the
            // user's `/thing` to the model as if it were prose.
            Some((builtin, _)) => {
                tracing::error!(builtin = builtin.name, "unhandled builtin command");
                Incoming::User {
                    id: id.clone(),
                    text: text.clone(),
                }
            }
            None => Incoming::User {
                id: id.clone(),
                text: text.clone(),
            },
        };
        let (tx, rx) = oneshot::channel();
        let accepted = id.clone();
        tokio::spawn(async move {
            let answer = match rx.await {
                Ok(Ok(())) => Ok(MessageAccepted::queued(accepted)),
                // Never written, so it is not owed an answer, and the caller
                // must not be told it was accepted.
                Ok(Err(e)) => Err(UserMessageError::Rejected(format!("persist message: {e}"))),
                Err(_) => Err(UserMessageError::NotFound),
            };
            let _ = reply.send(answer);
        });
        if agent
            .tell(AgentCommand::Enqueue {
                item,
                ack: Some(ReplyTo::from_sender(tx)),
            })
            .await
            .is_err()
        {
            tracing::warn!(session = %self.id, "message could not reach the agent");
        }
        // A person acting is the boundary that flushes results owed to parents.
        // Those strand once every child is terminal — no further outcome will
        // arrive to trigger the flush — so the next thing the user does has to
        // be what delivers them.
        //
        // It is also what re-asks for a sandbox after a failed create: the
        // runtime runner is `Failed { terminal: false }`, and its `actions()`
        // asks again. No `Provision` command, and so no second path.
        self.persist_and_advance(state, Vec::new(), ctx).await
    }

    /// Put a typed branch to the agent that would take it.
    ///
    /// Off the mailbox, and answered from there: the agent journals the request
    /// before it says which conversation to open, and this actor must keep
    /// answering everything else meanwhile.
    ///
    /// The message id *is* the fork's agent. One `/fork` produces one
    /// conversation and no transcript entry, so there is no second id to hand
    /// back, and a client that follows the acknowledgement lands on the branch.
    fn branch(
        &self,
        agent: ActorRef<AgentCommand>,
        mode: crate::sessions::runners::action::ForkMode,
        message: String,
        reply: ReplyTo<Result<MessageAccepted, UserMessageError>>,
    ) {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let answer = match agent
                .tell(AgentCommand::ForkBranch {
                    mode,
                    message,
                    reply: ReplyTo::from_sender(tx),
                })
                .await
            {
                Ok(()) => match rx.await {
                    Ok(Ok(fork)) => Ok(MessageAccepted {
                        message_id: fork.clone(),
                        forked_agent: Some(fork),
                    }),
                    Ok(Err(why)) => Err(UserMessageError::Rejected(why)),
                    Err(_) => Err(UserMessageError::Rejected(
                        "the branch was never answered".to_string(),
                    )),
                },
                Err(e) => Err(UserMessageError::Rejected(format!("fork: {e}"))),
            };
            let _ = reply.send(answer);
        });
    }

    /// Every runner that was still working when the last process died.
    ///
    /// Sound because of *when* it is asked: `Advance` is sent by `adopt` and by
    /// nothing else, so this runs once per load, before any command — and at
    /// that moment no agent of this session is resident. A runner that believes
    /// it is working is therefore working in a process that no longer exists,
    /// and the runner that owns it says what that means: a conversation records
    /// an interrupted turn, a run cancels the execution and suspends itself.
    ///
    /// The alternative was to let each agent say so at its own recovery, which
    /// is what the agent does — but only once something wakes it, and nothing
    /// ever wakes a run's step. That run stayed `Running` on an execution no
    /// process was running: wedged, and unrecoverable except through a retry
    /// nobody was told to make.
    pub(super) fn interrupted_at_load(&self, state: &RunnerSessionState) -> Vec<SessionEvent> {
        state
            .runners
            .iter()
            .filter_map(|(id, rec)| {
                let lifecycle = rec.state.lifecycle()?;
                Runner::busy(&rec.state).then_some(())?;
                let agent = Runner::primary_agent(&rec.state)?;
                Some(
                    self.wrap(
                        *id,
                        lifecycle
                            .on_agent_ended(agent, &crate::sessions::runners::TurnEnd::Interrupted),
                    ),
                )
            })
            .flatten()
            .collect()
    }

    /// Cancel one agent's turn in flight.
    ///
    /// The gate is what matters: stopping something that was not working is
    /// nothing, never a failure. A boundary journaled over an agent that had
    /// already ended rewrites history — it moves an idle conversation backwards,
    /// or concludes a step the run has already routed past — and a stop is the
    /// easiest way to arrive twice, because a person can press it while the
    /// ending is in flight.
    async fn on_stop(
        &mut self,
        state: &RunnerSessionState,
        agent_id: &str,
        reply: ReplyTo<Result<(), String>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        let Some(agent) = crate::sessions::runners::reads::resolve(state, Some(agent_id)) else {
            let _ = reply.send(Err(format!("no such agent: {agent_id}")));
            return CommandEffect::none();
        };
        let Some(runner) = state.runner_of(agent) else {
            let _ = reply.send(Err(format!("no such agent: {agent_id}")));
            return CommandEffect::none();
        };
        let Some(record) = state.record(runner) else {
            let _ = reply.send(Err(format!("no such agent: {agent_id}")));
            return CommandEffect::none();
        };
        let Some(lifecycle) = record.state.lifecycle() else {
            let _ = reply.send(Ok(()));
            return CommandEffect::none();
        };
        self.cancel_agent(agent).await;
        let _ = reply.send(Ok(()));
        let emit = lifecycle.on_agent_stopped(agent);
        let events = self.wrap(runner, emit);
        self.persist_and_advance(state, events, ctx).await
    }

    /// Hand a person's answers to the agent that asked.
    ///
    /// Routed, never decided: the questions live in the asking agent's own
    /// journal, and the capability that recorded them is what validates the set.
    async fn on_answer(
        &mut self,
        state: &RunnerSessionState,
        agent_id: Option<&str>,
        answers: Vec<AskAnswer>,
        reply: ReplyTo<Result<(), AnswerError>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        let Some(agent) = crate::sessions::runners::reads::resolve(state, agent_id)
            .and_then(|id| self.reach(id, state, ctx))
        else {
            let _ = reply.send(Err(AnswerError::NothingPending));
            return CommandEffect::none();
        };
        if agent
            .tell(AgentCommand::Answer { answers, reply })
            .await
            .is_err()
        {
            tracing::warn!(session = %self.id, "answers could not reach the agent");
        }
        CommandEffect::none()
    }

    /// A person removed a runner. Nothing removes one on its own.
    async fn on_delete_runner(
        &mut self,
        state: &RunnerSessionState,
        agent: AgentId,
        reply: ReplyTo<Result<(), String>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        // Agent in, runner out. The two id spaces are separate and only one of
        // them is ever handed to a client, so treating the one it holds as the
        // other found nothing and answered "no such fork" for a fork sitting
        // right there in the list it came from.
        let Some(id) = state.runner_of(agent) else {
            let _ = reply.send(Err(format!("no such agent: {agent}")));
            return CommandEffect::none();
        };
        let Some(record) = state.record(id) else {
            let _ = reply.send(Err(format!("no such runner: {id}")));
            return CommandEffect::none();
        };
        // Its agents go with it, so they are stopped before the record that
        // named them is gone.
        for agent in record.state.rows().iter().filter_map(|r| r.id.parse().ok()) {
            self.cancel_agent(AgentId(agent)).await;
            if let Some(resident) = self.agents.remove(&AgentId(agent)) {
                let _ = resident.actor.tell(AgentCommand::Shutdown).await;
            }
        }
        let _ = reply.send(Ok(()));
        self.persist_and_advance(state, vec![SessionEvent::RunnerDeleted { id }], ctx)
            .await
    }

    /// A person asked a workflow step to run again.
    async fn on_retry_step(
        &mut self,
        state: &RunnerSessionState,
        index: usize,
        reply: ReplyTo<Result<(), String>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        let root = state.root;
        let Some(record) = state.record(root) else {
            let _ = reply.send(Err("this session is not a run".to_string()));
            return CommandEffect::none();
        };
        let crate::sessions::runners::RunnerState::Workflow(run) = &record.state else {
            let _ = reply.send(Err("this session is not a run".to_string()));
            return CommandEffect::none();
        };
        match run.retry(u32::try_from(index).unwrap_or(u32::MAX)) {
            Ok(emit) => {
                let _ = reply.send(Ok(()));
                let events = self.wrap(root, emit);
                self.persist_and_advance(state, events, ctx).await
            }
            Err(e) => {
                let _ = reply.send(Err(e));
                CommandEffect::none()
            }
        }
    }

    /// Hand one agent's ending to the runner that owns it.
    ///
    /// Still routed by *identity* rather than by variant — the same `Concluded`
    /// means "the turn is over", "this step's output picks the next step", or
    /// "tell the parent its child is done". What has gone is the *probe*: which
    /// of those it means used to be inferred by trying the run log, then the
    /// fork roster, then the subagent forest, in an order the code itself
    /// recorded as load-bearing. It is one lookup now.
    async fn on_agent_outcome(
        &mut self,
        state: &RunnerSessionState,
        outcome: AgentOutcome,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        let (who, end) = match TurnEnd::split(outcome) {
            Ok(pair) => pair,
            // Usage is banked for every agent alike, and always: the tokens
            // were spent whatever became of the turn that spent them. Keyed by
            // model rather than by agent, because the per-agent breakdown
            // belongs to the runner that owns it.
            Err((agent, NotAnEnd::Usage(usage_total))) => {
                let model = crate::sessions::runners::reads::settings_of(state, AgentId(agent))
                    .map_or_else(String::new, |s| s.model.clone());
                return CommandEffect::persist(vec![SessionEvent::UsageBanked {
                    model,
                    spent: usage_total,
                }]);
            }
            Err((agent, NotAnEnd::Started)) => {
                return self.on_agent_started(state, AgentId(agent)).await;
            }
            // A summary taken for somebody else. Not this agent's turn ending —
            // it may still be running — so the fork capability answers it and
            // the routing below never sees it.
            Err((agent, NotAnEnd::ForkSummary { forks, result })) => {
                return self
                    .on_fork_summary(state, AgentId(agent), forks, result)
                    .await;
            }
        };
        let who = AgentId(who);
        let Some(runner) = state.runner_of(who) else {
            tracing::warn!(session = %self.id, agent = %who, "an outcome from an agent no runner owns");
            return CommandEffect::none();
        };
        let Some(record) = state.record(runner) else {
            return CommandEffect::none();
        };
        let Some(lifecycle) = record.state.lifecycle() else {
            return CommandEffect::none();
        };
        let emit = lifecycle.on_agent_ended(who, &end);
        let events = self.wrap(runner, emit);
        self.persist_and_advance(state, events, ctx).await
    }

    /// A `/summary-n-fork` summary, offered to the capabilities of the agent it
    /// was taken from.
    ///
    /// The forks waiting on it are the fork capability's own business — it
    /// recorded them when it asked — so this is a plain offer rather than a
    /// command the session understands.
    async fn on_fork_summary(
        &mut self,
        state: &RunnerSessionState,
        agent: AgentId,
        forks: Vec<Uuid>,
        result: Result<String, String>,
    ) -> CommandEffect<SessionEvent> {
        let _ = state;
        // Not yet routed: `Msg` has no summary arm, so there is nothing for the
        // fork capability to claim. Loud rather than silent — a dropped summary
        // leaves every fork that queued into this turn waiting for a seed that
        // will never come, and a silent `none()` here would read as "handled".
        tracing::error!(
            session = %self.id,
            %agent,
            forks = forks.len(),
            ok = result.is_ok(),
            "a fork summary has nowhere to go: ForkCapability cannot yet be offered one"
        );
        CommandEffect::none()
    }

    /// One of this session's agents drained its queue into a turn.
    ///
    /// Handed to the runner that owns it, like every other lifecycle moment.
    /// The three-way match this replaces — the session's own status, a tree
    /// node going back to work, a fork's own status — was three ways of saying
    /// "whoever owns this agent should record that it started", and the owner
    /// is now a lookup.
    async fn on_agent_started(
        &mut self,
        state: &RunnerSessionState,
        who: AgentId,
    ) -> CommandEffect<SessionEvent> {
        let Some(runner) = state.runner_of(who) else {
            return CommandEffect::none();
        };
        let Some(record) = state.record(runner) else {
            return CommandEffect::none();
        };
        let Some(lifecycle) = record.state.lifecycle() else {
            return CommandEffect::none();
        };
        let events = self.wrap(runner, lifecycle.on_agent_started(who));
        CommandEffect::persist(events)
    }

    /// Whether this session's agents may start a turn at all: it has a runtime,
    /// and it is not terminal. The whole of what an agent's own drain gate
    /// cannot answer for itself.
    fn runnable(state: &RunnerSessionState) -> bool {
        state.runtime_ready()
            && !matches!(
                crate::sessions::runners::reads::session_status(state),
                SessionStatus::Unrecoverable { .. }
            )
    }

    /// Stop every agent this session hosts. Used when the session unloads.
    async fn stop_agents(&mut self) {
        for (_, agent) in std::mem::take(&mut self.agents) {
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
    type Event = SessionEvent;
    type State = RunnerSessionState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.id)
    }

    fn initial_state() -> RunnerSessionState {
        RunnerSessionState::default()
    }

    /// One writer, and it is the state's own.
    ///
    /// The twenty-arm match this replaces routed each event to the component
    /// that understood it. A runner's event is addressed to its runner, and
    /// everything else is the session's own — so there is nothing here to keep
    /// in step with a component list.
    fn apply_event(mut state: RunnerSessionState, event: SessionEvent) -> RunnerSessionState {
        state.apply(&event);
        state
    }

    /// Write what just became durable into the agents' own transcripts, so a
    /// reader sees a lifecycle entry where it happened rather than having to
    /// infer it from the session's status.
    ///
    /// And report the status the batch left behind. Here rather than at each
    /// transition because here the write is already durable: the supervisor's
    /// copy can lag the journal, never lead it.
    async fn on_events_persisted(&mut self, events: &[SessionEvent], state: &RunnerSessionState) {
        self.record_lifecycle(events, state).await;
        self.report_forks(state).await;
        self.report_status(state).await;
    }

    /// Every command arrives addressed to a session, and this is the one place
    /// that reads the address: the shard already routed by it, so what is left
    /// below is the command it was wrapped around.
    async fn handle_command(
        &mut self,
        state: &RunnerSessionState,
        cmd: SessionInbox,
        ctx: &mut ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd.cmd {
            SessionCommand::StartRunner {
                id,
                kind,
                args,
                parent,
                reply,
            } => {
                // Deduped on the capability-minted id, atomically with the
                // persist: a capability re-asking after a crash names the child
                // it already journaled, and a check at the sink would race.
                if state.record(id).is_some() || !self.created.insert(id) {
                    let _ = reply.send(Ok(()));
                    return CommandEffect::none();
                }
                let events = self.create_child(id, kind, *args, parent, state, ctx).await;
                if events.is_empty() {
                    let _ = reply.send(Err("the runner could not be created".to_string()));
                    return CommandEffect::none();
                }
                // Answered on the journal's own acknowledgement, not before it.
                // Creation persists first: the capability that asked is told
                // its child exists only once the log says so, because that
                // reply is what a person is handed to open — and a crash
                // before the write replays as no child at all, which is
                // strictly better than an id nothing tracks.
                let (tx, rx) = oneshot::channel();
                tokio::spawn(async move {
                    let _ = reply.send(match rx.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(e)) => Err(format!("persist the child: {e}")),
                        Err(_) => Err("the child was never written".to_string()),
                    });
                });
                self.persist_and_advance(state, events, ctx)
                    .await
                    .and_ack(horsie_actor::ReplyTo::from_sender(tx))
            }
            SessionCommand::UserMessage {
                agent_id,
                text,
                reply,
            } => {
                self.on_user_message(state, agent_id, text, reply, ctx)
                    .await
            }
            SessionCommand::Stop { agent_id, reply } => {
                self.on_stop(state, &agent_id, reply, ctx).await
            }
            SessionCommand::Answer {
                agent_id,
                answers,
                reply,
            } => {
                self.on_answer(state, agent_id.as_deref(), answers, reply, ctx)
                    .await
            }
            SessionCommand::DeleteRunner { agent, reply } => {
                self.on_delete_runner(state, agent, reply, ctx).await
            }
            SessionCommand::RetryStep { index, reply } => {
                self.on_retry_step(state, index, reply, ctx).await
            }
            SessionCommand::PrepareOffload { reply } => {
                // Work started while the supervisor was deciding: refuse, and
                // let the idle clock start again.
                if self.busy(state) {
                    let _ = reply.send(false);
                    return CommandEffect::none();
                }
                self.stop_agents().await;
                // Going cold releases the sandbox. Without it a session that
                // nobody has touched for hours keeps a machine running, which
                // is the whole reason the idle sweep exists.
                self.deps()
                    .runtimes
                    .hibernate(&self.id.to_string(), &self.spec().vendor)
                    .await;
                // Answered as this actor's last act: it writes nothing after
                // returning, so the supervisor can drop its reference the
                // moment it sees `true`.
                let _ = reply.send(true);
                CommandEffect::stop()
            }
            SessionCommand::Delete { reply } => {
                self.cancel_in_flight(state).await;
                self.stop_agents().await;
                // A deleted session's sandbox goes with it. Hibernating it
                // would leave a machine nothing will ever wake.
                self.deps()
                    .runtimes
                    .delete(&self.id.to_string(), &self.spec().vendor)
                    .await;
                let _ = reply.send(());
                CommandEffect::stop()
            }
            SessionCommand::Read(c) => Reads::handle(self, state, c, ctx).await,
            SessionCommand::Hooks(c) => HookRouting::handle(self, state, c, ctx).await,
            SessionCommand::Core(c) => SessionCore::handle(self, state, c, ctx).await,
            // The one command routed by identity rather than by variant: which
            // agent sent the outcome decides which runner answers it.
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
        state: &RunnerSessionState,
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
