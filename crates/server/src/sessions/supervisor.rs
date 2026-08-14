//! The session registry: which sessions exist, which are loaded, and when a
//! loaded one goes cold.
//!
//! What is persisted here is a session's **existence and what it last said
//! about itself** — created, named, deleted, and its status. The session's own
//! journal is still the truth: the session folds a transition and then reports
//! it here, so this copy is a projection, never a source.
//!
//! It is persisted rather than cached for the reason the title is: a list has
//! to render it without loading the session, and loading is not free — it
//! re-attempts an interrupted provision, so a page of thirty cold runs must not
//! be thirty wake-ups. A cache made every row unknown after a restart, which
//! read as honest and was merely empty: the session's own journal is no more
//! current than this copy, because both only learn better when it loads.
//!
//! Nothing is loaded at boot. A session actor comes into being the first time a
//! command is addressed to it, and stops again once it has been idle for
//! [`SupervisorConfig::idle_timeout`].

use crate::sessions::addressing::{SessionRef, SessionShard, SupervisorInbox, SupervisorRef};
use crate::sessions::clock::{Clock, SystemClock};
use crate::sessions::session_actor::{
    AnswerError, AskAnswer, MessageAccepted, SessionCommand, SessionSnapshot, SessionUsageStats,
};
use crate::sessions::session_actor::{
    CoreCommand, ForkCommand, LifecycleCommand, ReadCommand, RunCommand, TurnCommand,
};
use crate::sessions::spec::{SessionId, SessionSpec, SessionStatus, status_kind, status_reason};
use crate::sessions::{SessionRevisions, UserMessageError};
use crate::users::{UserRegistry, UserServices, resolve};
use async_trait::async_trait;
use horsie_actor::{ActorContext, CommandEffect, EventSourcedActor, PersistenceId, ReplyTo};
use horsie_models::session::{
    ForkView, GlobalSessionEvent, GlobalSessionForksEvent, GlobalSessionStatusEvent,
    GlobalSessionTitleEvent,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use uuid::Uuid;

/// How long a loaded session may sit untouched before it is unloaded and its
/// runtime hibernated.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// How often the supervisor looks for sessions to unload.
const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(10);

/// Names this actor type in the journal. Changing it orphans every existing
/// supervisor log.
///
/// It happens to read the same as [`SupervisorShard::TYPE`], which names the
/// type in the cluster, and the two are deliberately not the same constant: one
/// is a durability contract and the other an address, and only one of them can
/// be changed.
const SUPERVISOR_KIND: &str = "session-supervisor";

/// How long a reader's poll parks before being answered with no news.
///
/// Any value is correct — expiring only costs a round trip — so this trades
/// idle chatter against how long a caller's slot in the reply table is held.
/// Half a minute is short enough that a departed reader is noticed promptly and
/// long enough that a quiet session costs two asks a minute.
const POLL_WINDOW: Duration = Duration::from_secs(30);

/// Knobs the idle policy reads. Separated so tests drive time explicitly.
///
/// One per deployment, cloned into every account's supervisor: how long a
/// session may sit idle is an operator's policy, not an account's.
#[derive(Clone)]
pub struct SupervisorConfig {
    pub clock: Arc<dyn Clock>,
    pub idle_timeout: Duration,
    /// `None` disables the background ticker — tests send `Tick` themselves.
    pub tick_interval: Option<Duration>,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            clock: Arc::new(SystemClock),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            tick_interval: Some(DEFAULT_TICK_INTERVAL),
        }
    }
}

/// Commands accepted by the [`SessionSupervisor`].
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize)]
pub enum SessionSupervisorCommand {
    /// Create a new session; replies with its generated id.
    Create {
        spec: SessionSpec,
        /// Unix epoch millis (supplied by the caller for deterministic tests).
        created_at: u64,
        reply: ReplyTo<SessionId>,
    },
    /// List every known session, each with the status it last reported.
    ///
    /// Loads nothing: the record is durable, so a cold session answers here as
    /// well as a live one. That is the whole reason the status is persisted.
    List {
        reply: ReplyTo<Vec<(SessionId, SessionRecord)>>,
    },
    /// Fetch one session's row and the state its actor recovered — its status,
    /// its usage and its agents. Loads the session: its journal is the truth,
    /// and the actor is the only thing that reads it.
    ///
    /// The whole session document in one ask. The supervisor owns the row; the
    /// actor owns everything that is *happening*, and answers all of it at
    /// once rather than a question at a time.
    Get {
        id: SessionId,
        reply: ReplyTo<Option<(SessionRecord, Option<SessionSnapshot>)>>,
    },
    /// Route a user message to one of the session's agents, loading the session
    /// if necessary. `agent_id` absent or `"main"` for the primary agent, else a
    /// subagent or workflow-step agent id.
    ///
    /// The reply lands once the *agent's* write is durable, so a caller holding
    /// an `Ok` holds a message that survives a crash.
    UserMessage {
        id: SessionId,
        agent_id: Option<String>,
        text: String,
        reply: ReplyTo<Result<MessageAccepted, UserMessageError>>,
    },
    /// Cancel one of a session's agents' turn in flight.
    Stop {
        id: SessionId,
        agent_id: String,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Delete one fork of a session. Never automatic — nothing prunes a fork,
    /// so this is the only way one goes.
    DeleteFork {
        id: SessionId,
        fork: Uuid,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Delete a session; the vendor decides its runtime's fate.
    Delete {
        id: SessionId,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Read forward from a cursor in one of a session's agent logs. `agent_id`
    /// selects the agent: absent or `"main"` for the primary agent, else a
    /// subagent id. The outer `None` means the session is unknown; an inner
    /// `None` means the agent is.
    ReadLog {
        id: SessionId,
        agent_id: Option<String>,
        after: Option<crate::agent_loop::Cursor>,
        reply: ReplyTo<Option<crate::agent_loop::ReadOutcome>>,
    },
    /// Read a window backwards from a cursor — scroll-back.
    PageLog {
        id: SessionId,
        agent_id: Option<String>,
        before: Option<u64>,
        max: usize,
        reply: ReplyTo<Option<crate::agent_loop::LogPage>>,
    },
    /// Read a session's aggregated usage.
    UsageStats {
        id: SessionId,
        reply: ReplyTo<Option<SessionUsageStats>>,
    },
    /// Read a session's workflow run (`None` when the session is unknown or
    /// is not a run).
    RunState {
        id: SessionId,
        reply: ReplyTo<Option<crate::sessions::workflow::WorkflowRunState>>,
    },
    /// Re-run one execution of a run's step.
    RetryStep {
        id: SessionId,
        index: u32,
        reply: ReplyTo<Option<Result<(), String>>>,
    },
    /// Answer every question one agent is parked on, at once.
    Answer {
        id: SessionId,
        agent_id: Option<String>,
        answers: Vec<AskAnswer>,
        reply: ReplyTo<Result<(), AnswerError>>,
    },
    /// Wait until one agent's revision differs from `after`, so a reader knows
    /// when to look again. `None` when the session or agent is unknown.
    ///
    /// A long poll, not a subscription. It answers with a *number*, never with
    /// a handle to one: a handle is a pointer into this process's memory, and a
    /// reader may be served by a different host from the one its session runs
    /// on. A number travels; a `watch::Receiver` cannot be encoded at all.
    ///
    /// Polling is also the honest shape once hosts are involved, because
    /// nothing about delivery between them is promised. A pushed notification
    /// that goes missing leaves a reader waiting forever; a poll that goes
    /// missing costs one window, and then the reader asks again.
    ///
    /// Answers straight away when the revision already differs, and otherwise
    /// waits until it moves or `POLL_WINDOW` passes — on expiry answering with
    /// the revision unchanged, which the reader reads as "still nothing".
    AwaitAgentRevision {
        id: SessionId,
        agent_id: Option<String>,
        /// The last revision this reader saw. `None` on a first ask, which
        /// answers immediately with wherever the agent currently is.
        after: Option<crate::sessions::Revision>,
        reply: ReplyTo<Option<crate::sessions::Revision>>,
    },
    /// Read one agent's document: what it is, what became of it, what it ran
    /// under, and its live values. `agent_id` absent or `"main"` for the
    /// primary agent — which, on a run, is the step in flight.
    AgentDetail {
        id: SessionId,
        agent_id: Option<String>,
        reply: ReplyTo<Option<crate::sessions::session_actor::AgentDetail>>,
    },
    /// Unload every session that has gone idle. Sent by the ticker, or by a
    /// test that has moved its clock.
    Tick,
    /// Tear down every loaded session for a clean shutdown.
    Shutdown { reply: ReplyTo<()> },
    /// Internal: a session actor reports its status changed.
    SessionStatusChanged {
        id: SessionId,
        status: SessionStatus,
    },
    /// Internal: a session actor requests a durable rename.
    RenameSession {
        id: SessionId,
        name: String,
        reply: ReplyTo<Result<(), horsie_actor::JournalError>>,
    },
    /// Internal: a session actor reports what forks it now holds.
    ///
    /// The whole roster rather than a delta, for the same reason
    /// [`Self::SessionStatusChanged`] sends the whole status: the session's own
    /// journal is the truth, this is a projection of it, and a projection built
    /// from deltas can drift where one built from the current value cannot.
    ForksChanged { id: SessionId, forks: Vec<ForkRow> },
    /// Internal: publish an already-journaled title to the global live feed.
    PublishSessionTitle { id: SessionId, name: String },
    /// Rename a session on someone's behalf rather than the agent's.
    ///
    /// Handled here rather than by asking the session actor, because renaming
    /// must not wake a session: loading one re-attempts an interrupted
    /// provision, and nobody expects typing a name to start a machine. A
    /// session that happens to be resident is told afterwards, so its own copy
    /// of the spec stays true.
    SetSessionTitle {
        id: SessionId,
        name: String,
        reply: ReplyTo<Result<String, RenameSessionError>>,
    },
    /// Merge-update one session's annotations. Err when the session is unknown.
    SetSessionAnnotations {
        id: SessionId,
        set: BTreeMap<String, String>,
        remove: Vec<String>,
        reply: ReplyTo<Result<(), String>>,
    },
}

/// Events recording which sessions exist. Status is deliberately absent — it
/// belongs to the session's own journal, and duplicating it here is what made
/// the two disagree after a crash.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionSupervisorEvent {
    SessionCreated {
        id: SessionId,
        spec: SessionSpec,
        created_at: u64,
    },
    SessionDeleted {
        id: SessionId,
    },
    SessionNamed {
        id: SessionId,
        name: String,
    },
    /// A session reached a new status. Journaled only when it differs from what
    /// is already recorded, so a session that loads and reports the status it
    /// recovered writes nothing.
    SessionStatusChanged {
        id: SessionId,
        status: SessionStatus,
    },
    /// A session's forks, as the list now holds them.
    SessionForksChanged {
        id: SessionId,
        forks: Vec<ForkRow>,
    },
    /// Merge-update of one session's annotations: `set` upserts, `remove` drops.
    SessionAnnotationsSet {
        id: SessionId,
        set: BTreeMap<String, String>,
        remove: Vec<String>,
    },
}

/// Why a rename was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenameSessionError {
    NotFound(String),
    /// Empty, multi-line, or over-long — the same rule the title tool applies.
    Invalid(String),
}

impl std::fmt::Display for RenameSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenameSessionError::NotFound(id) => write!(f, "no such session: {id}"),
            RenameSessionError::Invalid(reason) => write!(f, "{reason}"),
        }
    }
}

/// One fork under a session, as the session list holds it.
///
/// A projection of the session's own `ForkRecord`, not a second source of
/// truth — the same relationship `SessionRecord.status` has to the session's
/// journal, and durable for the same reason: `List` loads nothing, so what it
/// cannot read from the registry it cannot show at all.
///
/// `parent: None` means the session's main agent. Flattening `ForkParent` to an
/// `Option` here is what lets a client nest forks without learning the server's
/// own vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkRow {
    pub id: Uuid,
    pub parent: Option<Uuid>,
    pub title: Option<String>,
    pub status: crate::sessions::session_actor::AgentStatus,
    pub created_at_ms: u64,
    /// The moment of this fork's last status change — the end of its last turn
    /// once it is idle again. Zero before it has moved at all.
    #[serde(default)]
    pub last_activity_ms: u64,
}

impl ForkRow {
    /// This row as a client reads it.
    ///
    /// On the row rather than in the HTTP layer because the global feed
    /// publishes the same shape, and the list and the stream that updates it
    /// must not be able to describe a fork differently.
    #[must_use]
    pub fn to_view(&self) -> ForkView {
        ForkView {
            id: self.id.to_string(),
            parent: self.parent.map(|p| p.to_string()),
            title: self.title.clone(),
            status: self.status.as_wire().to_string(),
            created_at_ms: self.created_at_ms,
            last_activity_ms: self.last_activity_ms,
        }
    }
}

/// One registry row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub spec: SessionSpec,
    pub created_at: u64,
    /// User-set key-value metadata: a `tag.<name>` key per tag, plus any
    /// future provenance keys. Field-level default so pre-annotations
    /// journal rows load with an empty map.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    /// What this session last reported it was doing.
    ///
    /// Durable, not a cache. `Running` and `Provisioning` can go stale under a
    /// crash — they go stale identically in the session's own journal, which
    /// also only learns better when the session loads and repairs, so this copy
    /// is never less accurate than the truth it projects.
    #[serde(default)]
    pub status: SessionStatus,
    /// This session's forks, so the sidebar can nest them without loading the
    /// session. `#[serde(default)]` so pre-fork journal rows load with none.
    #[serde(default)]
    pub forks: Vec<ForkRow>,
}

/// Persisted supervisor state — which sessions exist, nothing more.
///
/// `#[serde(default)]` on the container: snapshotted state is a durability
/// contract. Add optional fields; never rename or repurpose one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSupervisorState {
    pub sessions: BTreeMap<SessionId, SessionRecord>,
}

pub struct SessionSupervisor {
    /// The account this supervisor is the session list of. Its only job is to
    /// key the persistence id — every scoped *store* binds its own copy.
    user: crate::auth::UserId,
    /// Where this account's bundle is resolved from, and the reason a recipe
    /// can stay synchronous: building one is async, so it happens at recovery
    /// rather than at construction.
    users: Weak<UserRegistry>,
    /// This account's bundle. `None` means exactly one thing — recovery has not
    /// run — and [`Self::services`] is what turns that into a fact the rest of
    /// the file does not have to restate.
    services: Option<Arc<UserServices>>,
    config: SupervisorConfig,
    /// When each loaded session was last spoken to, which is the whole of the
    /// idle policy. A session is in here for exactly as long as this supervisor
    /// believes it is loaded, so it also answers "what is there to sweep".
    last_activity: BTreeMap<SessionId, Instant>,
}

impl SessionSupervisor {
    pub fn new(
        user: crate::auth::UserId,
        users: Weak<UserRegistry>,
        config: SupervisorConfig,
    ) -> Self {
        Self {
            user,
            users,
            services: None,
            config,
            last_activity: BTreeMap::new(),
        }
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
            .expect("a supervisor handles no command before recovery has resolved its account")
    }

    fn revisions(&self) -> &Arc<SessionRevisions> {
        &self.services().revisions
    }

    /// The session `id`, and a note that it has just been spoken to.
    ///
    /// A name rather than a handle: an unloaded session is reactivated by the
    /// first command that arrives for it, so there is nothing to spawn here and
    /// nothing to invalidate when one goes cold. `None` only when the registry
    /// has no such session — which is what keeps a stranger's id from
    /// materialising an actor.
    fn session(
        &mut self,
        ctx: &ActorContext<SupervisorInbox>,
        state: &SessionSupervisorState,
        id: &SessionId,
    ) -> Option<SessionRef> {
        state
            .sessions
            .contains_key(id)
            .then(|| self.reach(ctx, id))?
    }

    /// The session `id` whether or not the registry knows it yet, and a note
    /// that it has just been spoken to.
    ///
    /// Creating a session needs this: the `SessionCreated` event is still being
    /// persisted when the session is first told what it is.
    fn reach(&mut self, ctx: &ActorContext<SupervisorInbox>, id: &SessionId) -> Option<SessionRef> {
        let session = match Uuid::parse_str(id) {
            Ok(uuid) => uuid,
            Err(e) => {
                tracing::error!(session_id = %id, error = %e, "unparseable session id");
                return None;
            }
        };
        self.last_activity
            .insert(id.clone(), self.config.clock.now());
        Some(SessionRef::new(
            ctx.shard_actor_of::<SessionShard>(),
            self.user.clone(),
            session,
        ))
    }

    /// The session `id` only if this supervisor believes it is loaded, and
    /// *without* counting as having spoken to it.
    ///
    /// For everything that must reach a live session but must never wake a cold
    /// one — a rename, a shutdown, an idle sweep. A reference cannot tell the
    /// difference by itself, because reaching through one is exactly what brings
    /// the actor back; `last_activity` is the only record of which sessions this
    /// supervisor has loaded.
    fn resident(&self, ctx: &ActorContext<SupervisorInbox>, id: &SessionId) -> Option<SessionRef> {
        let session = self
            .last_activity
            .contains_key(id)
            .then(|| Uuid::parse_str(id).ok())??;
        Some(SessionRef::new(
            ctx.shard_actor_of::<SessionShard>(),
            self.user.clone(),
            session,
        ))
    }

    fn publish(&self, id: &str, status: &SessionStatus) {
        let _ = self
            .services()
            .global_events
            .send(GlobalSessionEvent::StatusChanged(
                GlobalSessionStatusEvent {
                    session_id: id.to_string(),
                    status: status_kind(status),
                    reason: status_reason(status),
                },
            ));
    }

    /// Tell every open session list that a session's forks moved.
    ///
    /// Called beside the write, exactly as [`Self::publish`] is, and gated by
    /// the same idempotence check: a session reports its whole roster after
    /// every persisted batch, so publishing unconditionally would push a frame
    /// per batch for the life of every forked session.
    fn publish_forks(&self, id: &str, forks: &[ForkRow]) {
        let _ = self
            .services()
            .global_events
            .send(GlobalSessionEvent::ForksChanged(GlobalSessionForksEvent {
                session_id: id.to_string(),
                forks: forks.iter().map(ForkRow::to_view).collect(),
            }));
    }

    fn publish_title(&self, id: &str, name: &str) {
        let _ = self
            .services()
            .global_events
            .send(GlobalSessionEvent::TitleChanged(GlobalSessionTitleEvent {
                session_id: id.to_string(),
                name: name.to_string(),
            }));
    }

    /// Stop counting `id` as loaded, and let go of its revision channels unless
    /// a reader is still holding on.
    fn forget(&mut self, id: &SessionId) {
        self.last_activity.remove(id);
        self.revisions().release(id);
    }

    /// Unload every session that has been idle past the timeout.
    ///
    /// Runs inline on this mailbox, which is what makes it race-free: every
    /// command to a session goes through here, so nothing can reach one between
    /// it agreeing to unload and this supervisor forgetting it was loaded.
    async fn offload_idle(
        &mut self,
        ctx: &ActorContext<SupervisorInbox>,
        state: &SessionSupervisorState,
    ) {
        let now = self.config.clock.now();
        let timeout = self.config.idle_timeout;
        let candidates: Vec<SessionId> = self
            .last_activity
            .iter()
            .filter(|(id, last)| {
                // A running session is never a candidate: a long tool call must
                // not be unloaded out from under itself. Nor is one still
                // provisioning — unloading would strand the create's reply and
                // cost the next load a whole second attempt.
                if matches!(
                    state.sessions.get(*id).map(|rec| &rec.status),
                    Some(&SessionStatus::Running | &SessionStatus::Provisioning)
                ) {
                    return false;
                }
                now.duration_since(**last) >= timeout
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in candidates {
            // Deliberately not `session()`: reaching for one would count as
            // having spoken to it, and asking one that is not loaded would wake
            // it only to put it back to sleep.
            let Some(session) = self.resident(ctx, &id) else {
                continue;
            };
            match session
                .ask(|reply| SessionCommand::Lifecycle(LifecycleCommand::PrepareOffload { reply }))
                .await
            {
                Ok(true) => {
                    tracing::debug!(session = %id, "session idle; unloaded");
                    self.forget(&id);
                }
                // Refused: a run started while we were deciding. Restart its clock.
                Ok(false) => {
                    self.last_activity.insert(id, now);
                }
                // Nothing answered, so there is nothing loaded to unload.
                Err(_) => self.forget(&id),
            }
        }
    }
}

#[async_trait]
impl EventSourcedActor for SessionSupervisor {
    type Command = SupervisorInbox;
    type Event = SessionSupervisorEvent;
    type State = SessionSupervisorState;

    /// One supervisor per account, and the account is the instance id.
    ///
    /// Its event-sourced state *is* that account's session list, so this is
    /// what keeps two accounts' lists apart — there is no filter anywhere and
    /// nothing to forget to apply.
    fn persistence_id(&self) -> PersistenceId {
        PersistenceId::new(SUPERVISOR_KIND, self.user.as_str())
    }

    fn initial_state() -> SessionSupervisorState {
        SessionSupervisorState::default()
    }

    fn apply_event(
        mut state: SessionSupervisorState,
        event: SessionSupervisorEvent,
    ) -> SessionSupervisorState {
        match event {
            SessionSupervisorEvent::SessionCreated {
                id,
                spec,
                created_at,
            } => {
                state.sessions.insert(
                    id,
                    SessionRecord {
                        spec,
                        created_at,
                        annotations: BTreeMap::new(),
                        // Not a guess: a session's runtime is built before it
                        // can run anything, and creating one is the first thing
                        // it is asked to do.
                        status: SessionStatus::Provisioning,
                        forks: Vec::new(),
                    },
                );
            }
            SessionSupervisorEvent::SessionDeleted { id } => {
                state.sessions.remove(&id);
            }
            SessionSupervisorEvent::SessionNamed { id, name } => {
                if let Some(rec) = state.sessions.get_mut(&id) {
                    rec.spec.name = Some(name);
                }
            }
            SessionSupervisorEvent::SessionForksChanged { id, forks } => {
                if let Some(rec) = state.sessions.get_mut(&id) {
                    rec.forks = forks;
                }
            }
            SessionSupervisorEvent::SessionStatusChanged { id, status } => {
                if let Some(rec) = state.sessions.get_mut(&id) {
                    rec.status = status;
                }
            }
            SessionSupervisorEvent::SessionAnnotationsSet { id, set, remove } => {
                if let Some(rec) = state.sessions.get_mut(&id) {
                    for key in &remove {
                        rec.annotations.remove(key);
                    }
                    rec.annotations.extend(set);
                }
            }
        }
        state
    }

    /// Every command arrives addressed to an account, and this is the one
    /// place that reads the address: the shard already routed by it, so what is
    /// left below is the command it was wrapped around.
    async fn handle_command(
        &mut self,
        state: &SessionSupervisorState,
        cmd: SupervisorInbox,
        ctx: &mut ActorContext<SupervisorInbox>,
    ) -> CommandEffect<SessionSupervisorEvent> {
        match cmd.cmd {
            SessionSupervisorCommand::Create {
                spec,
                created_at,
                reply,
            } => {
                let id = Uuid::new_v4().to_string();
                // The session owns its runtime's whole life, so creating one is
                // the first thing it is asked to do rather than something done
                // to it. Two things follow, and both are the point.
                //
                // `Provision` is enqueued here, before the reply — so it is
                // ahead of the first message in the same mailbox, and the wait
                // is ordinary actor sequencing rather than a gate beside the
                // runtime manager. And the attempt is journaled by the session,
                // so a process that dies mid-create leaves a session that knows
                // to finish it, which no in-memory gate could.
                if let Some(session) = self.reach(ctx, &id) {
                    // What this session is, ahead of everything else it will
                    // ever be told. Nothing else can have addressed a uuid
                    // generated on this line, so this command is what brings the
                    // actor into being and is therefore first in its mailbox —
                    // which is what an empty journal needs, since a session
                    // recovers nothing and waits to be told what it is.
                    let _ = session
                        .tell(SessionCommand::Core(CoreCommand::RecordSpec {
                            spec: Box::new(spec.clone()),
                        }))
                        .await;
                    let _ = session
                        .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
                        .await;
                }
                let _ = reply.send(id.clone());
                // Not a guess: a fresh session is provisioning, and says so
                // until its vendor confirms the runtime. Recorded by the fold
                // of `SessionCreated`, which is why nothing is inserted here.
                self.publish(&id, &SessionStatus::Provisioning);
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionCreated {
                    id,
                    spec,
                    created_at,
                }])
            }
            SessionSupervisorCommand::List { reply } => {
                let sessions = state
                    .sessions
                    .iter()
                    .map(|(id, rec)| (id.clone(), rec.clone()))
                    .collect();
                let _ = reply.send(sessions);
                CommandEffect::none()
            }
            SessionSupervisorCommand::Get { id, reply } => {
                let Some(record) = state.sessions.get(&id).cloned() else {
                    let _ = reply.send(None);
                    return CommandEffect::none();
                };
                match self.session(ctx, state, &id) {
                    Some(session) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = session
                            .tell(SessionCommand::Read(ReadCommand::Snapshot {
                                reply: ReplyTo::from_sender(tx),
                            }))
                            .await;
                        tokio::spawn(async move {
                            let _ = reply.send(Some((record, rx.await.ok())));
                        });
                    }
                    None => {
                        let _ = reply.send(Some((record, None)));
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::UserMessage {
                id,
                agent_id,
                text,
                reply,
            } => {
                match self.session(ctx, state, &id) {
                    None => {
                        let _ = reply.send(Err(UserMessageError::NotFound));
                    }
                    Some(session) => {
                        let _ = session
                            .tell(SessionCommand::Turn(TurnCommand::UserMessage {
                                agent_id,
                                text,
                                reply,
                            }))
                            .await;
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::DeleteFork { id, fork, reply } => {
                match self.session(ctx, state, &id) {
                    None => {
                        let _ = reply.send(Err(format!("no such session: {id}")));
                    }
                    // Routed, not decided: which forks exist is the session's
                    // own state, and the supervisor's copy is a projection.
                    Some(session) => {
                        let _ = session
                            .tell(SessionCommand::Fork(ForkCommand::Delete {
                                id: fork,
                                reply,
                            }))
                            .await;
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::Stop {
                id,
                agent_id,
                reply,
            } => {
                match self.session(ctx, state, &id) {
                    None => {
                        let _ = reply.send(Err(format!("no such session: {id}")));
                    }
                    Some(session) => {
                        let (tx, rx) = oneshot::channel();
                        if session
                            .tell(SessionCommand::Turn(TurnCommand::Stop {
                                agent_id,
                                reply: ReplyTo::from_sender(tx),
                            }))
                            .await
                            .is_err()
                        {
                            let _ = reply.send(Err("session unavailable".to_string()));
                        } else {
                            // The session's own answer, forwarded rather than
                            // flattened to `Ok`: only it can tell an id that
                            // names no agent from one that had nothing to stop.
                            tokio::spawn(async move {
                                let _ = reply
                                    .send(rx.await.unwrap_or_else(|_| {
                                        Err("session unavailable".to_string())
                                    }));
                            });
                        }
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::Delete { id, reply } => {
                if !state.sessions.contains_key(&id) {
                    let _ = reply.send(Err(format!("no such session: {id}")));
                    return CommandEffect::none();
                }
                // Loading it first is deliberate: the session actor is what
                // knows how to cancel a run and tell the vendor.
                if let Some(session) = self.session(ctx, state, &id) {
                    let (tx, rx) = oneshot::channel();
                    if session
                        .tell(SessionCommand::Lifecycle(LifecycleCommand::Delete {
                            reply: ReplyTo::from_sender(tx),
                        }))
                        .await
                        .is_ok()
                    {
                        let _ = rx.await;
                    }
                }
                self.forget(&id);
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionDeleted { id }])
            }
            SessionSupervisorCommand::ReadLog {
                id,
                agent_id,
                after,
                reply,
            } => {
                match self.session(ctx, state, &id) {
                    Some(session) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = session
                            .tell(SessionCommand::Read(ReadCommand::ReadLog {
                                agent_id,
                                after,
                                reply: ReplyTo::from_sender(tx),
                            }))
                            .await;
                        tokio::spawn(async move {
                            let _ = reply.send(rx.await.ok().flatten());
                        });
                    }
                    None => {
                        let _ = reply.send(None);
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::PageLog {
                id,
                agent_id,
                before,
                max,
                reply,
            } => {
                match self.session(ctx, state, &id) {
                    Some(session) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = session
                            .tell(SessionCommand::Read(ReadCommand::PageLog {
                                agent_id,
                                before,
                                max,
                                reply: ReplyTo::from_sender(tx),
                            }))
                            .await;
                        tokio::spawn(async move {
                            let _ = reply.send(rx.await.ok().flatten());
                        });
                    }
                    None => {
                        let _ = reply.send(None);
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::UsageStats { id, reply } => {
                match self.session(ctx, state, &id) {
                    Some(session) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = session
                            .tell(SessionCommand::Read(ReadCommand::UsageStats {
                                reply: ReplyTo::from_sender(tx),
                            }))
                            .await;
                        tokio::spawn(async move {
                            let _ = reply.send(rx.await.ok());
                        });
                    }
                    None => {
                        let _ = reply.send(None);
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::RunState { id, reply } => {
                match self.session(ctx, state, &id) {
                    Some(session) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = session
                            .tell(SessionCommand::Run(RunCommand::State {
                                reply: ReplyTo::from_sender(tx),
                            }))
                            .await;
                        tokio::spawn(async move {
                            let _ = reply.send(rx.await.ok().flatten());
                        });
                    }
                    None => {
                        let _ = reply.send(None);
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::RetryStep { id, index, reply } => {
                match self.session(ctx, state, &id) {
                    Some(session) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = session
                            .tell(SessionCommand::Run(RunCommand::RetryStep {
                                index,
                                reply: ReplyTo::from_sender(tx),
                            }))
                            .await;
                        tokio::spawn(async move {
                            let _ = reply.send(rx.await.ok());
                        });
                    }
                    None => {
                        let _ = reply.send(None);
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::Answer {
                id,
                agent_id,
                answers,
                reply,
            } => {
                match self.session(ctx, state, &id) {
                    None => {
                        let _ = reply.send(Err(AnswerError::NothingPending));
                    }
                    Some(session) => {
                        let _ = session
                            .tell(SessionCommand::Turn(TurnCommand::Answer {
                                agent_id,
                                answers,
                                reply,
                            }))
                            .await;
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::AwaitAgentRevision {
                id,
                agent_id,
                after,
                reply,
            } => {
                // Answered here, not by the actor, and deliberately without
                // loading: waiting on a session is no reason to wake one. The
                // reader's first read is what loads it, and after that it only
                // looks again when the position moves — which can only happen
                // if something else already woke the session.
                if !state.sessions.contains_key(&id) {
                    let _ = reply.send(None);
                    return CommandEffect::none();
                }
                let wire_id = agent_id.as_deref().unwrap_or("main");
                let revisions = self.revisions().of(&id);
                revisions.touch();
                let mut rx = revisions.for_agent(wire_id).subscribe();
                let current = *rx.borrow_and_update();
                if after != Some(current) {
                    let _ = reply.send(Some(current));
                    return CommandEffect::none();
                }
                // Nothing new yet, so wait — off this mailbox, which has to stay
                // free to serve every other session while one reader waits.
                tokio::spawn(async move {
                    let moved = tokio::time::timeout(POLL_WINDOW, rx.changed()).await;
                    // A closed channel is not the end of the stream: the
                    // registry outlives the session precisely so a reader can
                    // sit through an offload. Answer with what we last saw and
                    // let the reader ask again.
                    let _ = reply.send(Some(match moved {
                        Ok(Ok(())) => *rx.borrow(),
                        Ok(Err(_)) | Err(_) => current,
                    }));
                });
                CommandEffect::none()
            }
            SessionSupervisorCommand::AgentDetail {
                id,
                agent_id,
                reply,
            } => {
                match self.session(ctx, state, &id) {
                    Some(session) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = session
                            .tell(SessionCommand::Read(ReadCommand::Agent {
                                agent_id,
                                reply: ReplyTo::from_sender(tx),
                            }))
                            .await;
                        tokio::spawn(async move {
                            let _ = reply.send(rx.await.ok().flatten());
                        });
                    }
                    None => {
                        let _ = reply.send(None);
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::Tick => {
                self.offload_idle(ctx, state).await;
                CommandEffect::none()
            }
            SessionSupervisorCommand::Shutdown { reply } => {
                let ids: Vec<SessionId> = self.last_activity.keys().cloned().collect();
                for id in ids {
                    if let Some(session) = self.resident(ctx, &id) {
                        let _ = session
                            .ask(|reply| {
                                SessionCommand::Lifecycle(LifecycleCommand::PrepareOffload {
                                    reply,
                                })
                            })
                            .await;
                    }
                    self.forget(&id);
                }
                let _ = reply.send(());
                CommandEffect::none()
            }
            SessionSupervisorCommand::SessionStatusChanged { id, status } => {
                self.publish(&id, &status);
                // Idempotent on purpose: a session reports after every persisted
                // batch and once more at load, so only a real transition is
                // worth a write. Without this a busy session would journal a row
                // here per batch, and every page-open would journal one more.
                match state.sessions.get(&id) {
                    Some(rec) if rec.status != status => {
                        CommandEffect::persist(vec![SessionSupervisorEvent::SessionStatusChanged {
                            id,
                            status,
                        }])
                    }
                    _ => CommandEffect::none(),
                }
            }
            SessionSupervisorCommand::ForksChanged { id, forks } => {
                // Idempotent, exactly as `SessionStatusChanged` is: a session
                // reports after every persisted batch and once more at load, so
                // only a real change is worth a write. Without this, a busy
                // session with one fork would journal a row here per batch.
                match state.sessions.get(&id) {
                    Some(rec) if rec.forks != forks => {
                        self.publish_forks(&id, &forks);
                        CommandEffect::persist(vec![SessionSupervisorEvent::SessionForksChanged {
                            id,
                            forks,
                        }])
                    }
                    _ => CommandEffect::none(),
                }
            }
            SessionSupervisorCommand::RenameSession { id, name, reply } => {
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionNamed { id, name }])
                    .and_ack(reply)
            }
            SessionSupervisorCommand::SetSessionTitle { id, name, reply } => {
                let title = match crate::sessions::title_tool::normalize_session_title(&name) {
                    Ok(title) => title,
                    Err(e) => {
                        let _ = reply.send(Err(RenameSessionError::Invalid(e.to_string())));
                        return CommandEffect::none();
                    }
                };
                if !state.sessions.contains_key(&id) {
                    let _ = reply.send(Err(RenameSessionError::NotFound(id)));
                    return CommandEffect::none();
                }
                // The resident actor's own copy of the spec is what decides
                // whether a first message still gets to title the session, so a
                // rename it never heard about would be overwritten by one. Only
                // a resident one, because renaming must not wake a session:
                // loading one re-attempts an interrupted provision, and nobody
                // expects typing a name to start a machine.
                if let Some(session) = self.resident(ctx, &id) {
                    let _ = session
                        .tell(SessionCommand::Core(CoreCommand::TitleSet {
                            name: title.clone(),
                        }))
                        .await;
                }
                self.publish_title(&id, &title);
                let _ = reply.send(Ok(title.clone()));
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionNamed {
                    id,
                    name: title,
                }])
            }
            SessionSupervisorCommand::PublishSessionTitle { id, name } => {
                // A rename superseded while its publish was queued must not
                // broadcast a stale title.
                let current = state
                    .sessions
                    .get(&id)
                    .and_then(|rec| rec.spec.name.as_deref());
                if current == Some(name.as_str()) {
                    self.publish_title(&id, &name);
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::SetSessionAnnotations {
                id,
                set,
                remove,
                reply,
            } => {
                if !state.sessions.contains_key(&id) {
                    let _ = reply.send(Err(format!("no such session: {id}")));
                    return CommandEffect::none();
                }
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionAnnotationsSet {
                    id,
                    set,
                    remove,
                }])
            }
        }
    }

    /// Recovery rebuilds the registry, resolves this account's bundle, and
    /// stops there. No session actor is loaded, no journal but this one is
    /// read, and no vendor is called — a restart costs one journal replay
    /// however many sessions exist.
    ///
    /// The bundle is resolved *here* because a shard recipe is synchronous and
    /// building one is not. This runs before the first command, so everything
    /// below may take it for granted.
    async fn on_recovery_complete(
        &mut self,
        _state: &SessionSupervisorState,
        ctx: &mut ActorContext<SupervisorInbox>,
    ) {
        self.services = resolve(&self.users, &self.user).await;

        if let Some(interval) = self.config.tick_interval {
            // This instance's own mailbox, not the shard reference: a tick sent
            // through the latter would rebuild the supervisor it was ticking,
            // so a stopped one would come back and start a second ticker.
            let me = SupervisorRef::new(ctx.self_ref(), self.user.clone());
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.tick().await; // the first tick fires immediately
                loop {
                    ticker.tick().await;
                    if me.tell(SessionSupervisorCommand::Tick).await.is_err() {
                        break;
                    }
                }
            });
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
mod tests {
    use super::*;
    use crate::runtime_vendor::fake::FakeRuntimeVendor;
    use crate::sessions::addressing::SupervisorShard;
    use crate::sessions::clock::TestClock;
    use crate::sessions::session_actor::SessionActor;
    use crate::sessions::session_actor::SessionDomainEvent;
    use crate::sessions::spec::AgentSettings;
    use horsie_actor::Journal;

    fn spec_fixture() -> SessionSpec {
        SessionSpec {
            name: Some("test".into()),
            agent: AgentSettings {
                instructions: None,
                model: "mock".into(),
                allowed_tools: None,
                use_plugins: None,
                max_iterations: None,
                max_retries: 0,
                mcp_servers: vec![],
                memory_spaces: vec![],
                thinking_effort: None,
                max_concurrent_subagents: None,
                auto_compact: None,
            },
            workspaces: vec![],
            provision: vec![],
            vendor: "mock".into(),
            plugins: vec![],
            origin: crate::sessions::spec::SessionOrigin::User,
            workflow: None,
            environment: None,
            env_vars: vec![],
        }
    }

    /// One account's deployment, on a fake runtime vendor and a clock the test
    /// moves by hand.
    ///
    /// A whole deployment rather than a supervisor, because a supervisor is
    /// built by a shard recipe now: it is handed an account id and resolves
    /// everything else, so there has to be an account for it to resolve.
    struct Fixture {
        agent: FakeRuntimeVendor,
        clock: Arc<TestClock>,
        node: crate::testing::Deployment,
    }

    impl Fixture {
        async fn supervisor(&self) -> SupervisorRef {
            self.node.supervisor().await
        }

        fn journal(&self) -> Arc<dyn Journal> {
            self.node.journal.clone()
        }

        /// Move every session past the idle timeout and sweep.
        async fn go_idle(&self, sup: &SupervisorRef) {
            self.clock.advance(Duration::from_secs(181));
            sup.tell(SessionSupervisorCommand::Tick).await.unwrap();
        }
    }

    async fn fixture() -> Fixture {
        let agent = FakeRuntimeVendor::builder("mock")
            .serve_in_process()
            .await
            .expect("fake agent");
        let clock: Arc<TestClock> = Arc::new(TestClock::new());
        let node = crate::testing::Deployment::new(
            crate::sessions::session_actor::testing::fake_deps(&agent, None),
            manual_config(&clock),
        )
        .await;
        Fixture { agent, clock, node }
    }

    fn manual_config(clock: &Arc<TestClock>) -> SupervisorConfig {
        SupervisorConfig {
            clock: clock.clone(),
            idle_timeout: Duration::from_secs(180),
            // No background ticker: the test decides when time passes and when
            // the sweep runs, so nothing here is a race.
            tick_interval: None,
        }
    }

    async fn create(sup: &SupervisorRef) -> SessionId {
        sup.ask(|reply| SessionSupervisorCommand::Create {
            spec: spec_fixture(),
            created_at: 1,
            reply,
        })
        .await
        .unwrap()
    }

    async fn await_signal(agent: &FakeRuntimeVendor, signal: &str) -> bool {
        for _ in 0..100 {
            if agent.signals().iter().any(|s| s == signal) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[test]
    fn created_then_named_then_deleted_folds() {
        let s = SessionSupervisor::apply_event(
            SessionSupervisorState::default(),
            SessionSupervisorEvent::SessionCreated {
                id: "s1".into(),
                spec: spec_fixture(),
                created_at: 7,
            },
        );
        assert_eq!(s.sessions.get("s1").unwrap().created_at, 7);
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionNamed {
                id: "s1".into(),
                name: "Latest".into(),
            },
        );
        assert_eq!(
            s.sessions.get("s1").unwrap().spec.name.as_deref(),
            Some("Latest")
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionDeleted { id: "s1".into() },
        );
        assert!(s.sessions.is_empty());
    }

    fn created_session(s: SessionSupervisorState, id: &str) -> SessionSupervisorState {
        SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionCreated {
                id: id.into(),
                spec: spec_fixture(),
                created_at: 1,
            },
        )
    }

    #[test]
    fn annotations_set_and_removed_fold() {
        let s = created_session(SessionSupervisorState::default(), "s1");
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::from([("tag.web".to_string(), String::new())]),
                remove: vec![],
            },
        );
        assert_eq!(
            s.sessions.get("s1").unwrap().annotations.get("tag.web"),
            Some(&String::new())
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::new(),
                remove: vec!["tag.web".to_string()],
            },
        );
        assert!(s.sessions.get("s1").unwrap().annotations.is_empty());
    }

    #[test]
    fn session_delete_drops_its_annotations() {
        let s = created_session(SessionSupervisorState::default(), "s1");
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::from([("tag.web".to_string(), String::new())]),
                remove: vec![],
            },
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionDeleted { id: "s1".into() },
        );
        assert!(s.sessions.is_empty());
    }

    /// Groups were deleted outright rather than migrated, which splits the
    /// upgrade in two — and only one half is safe.
    ///
    /// A *snapshot* taken while groups existed still loads: serde ignores the
    /// field the struct no longer knows, so the sessions in it survive. A
    /// surviving `Group*` *event* does not. `recover_state` decodes every
    /// replayed event with `serde_json::from_slice` and turns a failure into a
    /// hard `JournalError::Serialization`, so a single un-compacted group event
    /// stops the supervisor loading at all — and the supervisor is the session
    /// list. Hence the upgrade note: clear supervisor state, or snapshot past
    /// those events, before deploying this.
    #[test]
    fn a_snapshot_that_still_names_groups_loads_without_them() {
        let snapshot = serde_json::json!({
            "sessions": {},
            "groups": { "web": { "created_at": 1 } },
        });
        let state: SessionSupervisorState =
            serde_json::from_value(snapshot).expect("a snapshot from before tags must still load");
        assert!(state.sessions.is_empty());

        let event = serde_json::json!({ "GroupCreated": { "name": "web", "created_at": 1 } });
        assert!(serde_json::from_value::<SessionSupervisorEvent>(event).is_err());
    }

    #[tokio::test]
    async fn boot_loads_nothing() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        assert!(await_signal(&f.agent, &format!("create:{id}")).await);
        // Wait for the session to have finished provisioning and said so, so
        // the restart below has a status to lose.
        wait_for_status(&sup, &id, &SessionStatus::Idle).await;
        sup.ask(|reply| SessionSupervisorCommand::Shutdown { reply })
            .await
            .unwrap();
        let before = f.agent.signals();

        // Second incarnation on the same journal: the registry comes back, but
        // nothing is loaded and no vendor is touched.
        f.node.restart().await;
        let rows = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        let (_, rec) = rows
            .into_iter()
            .find(|(row_id, _)| row_id == &id)
            .expect("the session still exists");
        assert_eq!(
            rec.status,
            SessionStatus::Idle,
            "the row answers from the registry, without loading the session"
        );
        assert_eq!(
            f.agent.signals(),
            before,
            "recovery must not call the vendor"
        );
    }

    /// A session's status outlives the process that produced it.
    ///
    /// It used to be a cache of loaded sessions, so every row rendered unknown
    /// after a restart — and a workflow's list of past runs is a list of
    /// sessions that are by definition cold, so every one of them was a dash.
    #[tokio::test]
    async fn a_status_survives_a_restart_without_loading_the_session() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        assert!(await_signal(&f.agent, &format!("create:{id}")).await);
        let _ = sup
            .tell(SessionSupervisorCommand::SessionStatusChanged {
                id: id.clone(),
                status: SessionStatus::Failed {
                    reason: "the provider said no".into(),
                },
            })
            .await;
        sup.ask(|reply| SessionSupervisorCommand::Shutdown { reply })
            .await
            .unwrap();
        let before = f.agent.signals();

        f.node.restart().await;
        let rows = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        let (_, rec) = rows
            .into_iter()
            .find(|(row_id, _)| row_id == &id)
            .expect("the session still exists");
        assert_eq!(
            rec.status,
            SessionStatus::Failed {
                reason: "the provider said no".into()
            },
            "the last status the session reported is the one the registry keeps"
        );
        assert_eq!(
            f.agent.signals(),
            before,
            "listing must not wake a session, which would re-attempt its provision"
        );
    }

    /// Re-reporting an unchanged status journals nothing: a session reports
    /// after every persisted batch and once more at load, so without this a
    /// busy session would write a registry row per batch.
    #[tokio::test]
    async fn re_reporting_the_same_status_journals_nothing() {
        let f = fixture().await;
        let journal = f.journal();
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        assert!(await_signal(&f.agent, &format!("create:{id}")).await);
        let _ = sup
            .tell(SessionSupervisorCommand::SessionStatusChanged {
                id: id.clone(),
                status: SessionStatus::Idle,
            })
            .await;
        // Round-trip so the write above is folded before the count is taken.
        let _ = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        let pid = PersistenceId::new(
            "session-supervisor",
            crate::auth::UserId::bootstrap().as_str(),
        );
        let before = journal_len(&journal, &pid).await;

        for _ in 0..3 {
            let _ = sup
                .tell(SessionSupervisorCommand::SessionStatusChanged {
                    id: id.clone(),
                    status: SessionStatus::Idle,
                })
                .await;
        }
        let _ = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert_eq!(
            journal_len(&journal, &pid).await,
            before,
            "a status that has not changed is not news"
        );
    }

    /// Poll the registry until `id` reads `want` (2s cap).
    async fn wait_for_status(sup: &SupervisorRef, id: &SessionId, want: &SessionStatus) {
        for _ in 0..200 {
            let rows = sup
                .ask(|reply| SessionSupervisorCommand::List { reply })
                .await
                .unwrap();
            if rows
                .iter()
                .any(|(row_id, rec)| row_id == id && &rec.status == want)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("session {id} never reached {want:?}");
    }

    async fn journal_len(journal: &Arc<dyn Journal>, pid: &PersistenceId) -> usize {
        use futures_util::StreamExt;
        #[expect(
            clippy::disallowed_methods,
            reason = "test-only inspection of a journal whose actor is running"
        )]
        let stream = journal.replay(pid, 0).await;
        stream.count().await
    }

    #[tokio::test]
    async fn any_command_loads_the_session_without_acquiring_a_runtime() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        assert!(await_signal(&f.agent, &format!("create:{id}")).await);

        let before = f.agent.signals();
        let stats = sup
            .ask(|reply| SessionSupervisorCommand::UsageStats {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap();
        assert!(stats.is_some(), "the session must load on demand");
        assert_eq!(
            f.agent.signals(),
            before,
            "loading a session to read it must not touch its runtime"
        );
    }

    /// Poll the list until the session shows at least one fork.
    async fn wait_for_forks(sup: &SupervisorRef, id: &SessionId) {
        for _ in 0..200 {
            let rows = sup
                .ask(|reply| SessionSupervisorCommand::List { reply })
                .await
                .unwrap();
            if rows
                .iter()
                .any(|(row_id, rec)| row_id == id && !rec.forks.is_empty())
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("session {id} never listed a fork");
    }

    /// The sidebar is built from the durable registry — `List` is documented
    /// "Loads nothing". Deriving fork rows from session state instead would
    /// wake every session that has ever been forked, every time the app opens.
    #[tokio::test]
    async fn forks_are_listed_without_loading_the_session() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        let fork = Uuid::from_bytes([7; 16]);

        sup.tell(SessionSupervisorCommand::ForksChanged {
            id: id.clone(),
            forks: vec![ForkRow {
                id: fork,
                parent: None,
                title: Some("Other migration".into()),
                status: crate::sessions::session_actor::AgentStatus::Idle,
                created_at_ms: 5,
                last_activity_ms: 5,
            }],
        })
        .await
        .unwrap();
        wait_for_forks(&sup, &id).await;

        // A second supervisor over the same journal, which has loaded nothing.
        let cold = f.supervisor().await;
        let listed = cold
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        let (_, rec) = listed.iter().find(|(s, _)| *s == id).expect("the session");
        assert_eq!(rec.forks.len(), 1);
        assert_eq!(rec.forks[0].id, fork);
        assert_eq!(rec.forks[0].title.as_deref(), Some("Other migration"));
    }

    /// Durable is not the same as live.
    ///
    /// The registry held fork rows correctly and `List` answered them, but
    /// nothing was broadcast — so a fork reached the sidebar only when
    /// something *else* forced a refetch of the session list. Status and title
    /// publish beside their own write; forks did not, and the omission is
    /// invisible from the durable side, which is why the test asserts on the
    /// stream rather than on `List`.
    #[tokio::test]
    async fn a_fork_change_is_broadcast_for_the_session_list() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let mut grx = f.node.services().await.global_events.subscribe();
        let id = create(&sup).await;
        let fork = Uuid::from_bytes([9; 16]);

        sup.tell(SessionSupervisorCommand::ForksChanged {
            id: id.clone(),
            forks: vec![ForkRow {
                id: fork,
                parent: None,
                title: Some("The other direction".into()),
                status: crate::sessions::session_actor::AgentStatus::Provisioning,
                created_at_ms: 5,
                last_activity_ms: 5,
            }],
        })
        .await
        .unwrap();

        loop {
            let frame = tokio::time::timeout(Duration::from_secs(2), grx.recv())
                .await
                .expect("a forks frame reaches the session list")
                .unwrap();
            if let GlobalSessionEvent::ForksChanged(event) = frame {
                assert_eq!(event.session_id, id);
                assert_eq!(event.forks.len(), 1);
                assert_eq!(event.forks[0].id, fork.to_string());
                assert_eq!(event.forks[0].title.as_deref(), Some("The other direction"));
                assert_eq!(
                    event.forks[0].status, "provisioning",
                    "the frame speaks the wire vocabulary a client already reads"
                );
                break;
            }
        }
    }

    /// The publish rides the same idempotence guard as the write: a session
    /// reports its whole roster after every persisted batch, so a fork that has
    /// not changed must not wake every open sidebar.
    #[tokio::test]
    async fn re_reporting_the_same_forks_broadcasts_nothing() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let mut grx = f.node.services().await.global_events.subscribe();
        let id = create(&sup).await;
        let rows = vec![ForkRow {
            id: Uuid::from_bytes([9; 16]),
            parent: None,
            title: None,
            status: crate::sessions::session_actor::AgentStatus::Idle,
            created_at_ms: 5,
            last_activity_ms: 5,
        }];

        sup.tell(SessionSupervisorCommand::ForksChanged {
            id: id.clone(),
            forks: rows.clone(),
        })
        .await
        .unwrap();
        wait_for_forks(&sup, &id).await;
        // Drain everything the first report produced.
        while let Ok(Ok(frame)) = tokio::time::timeout(Duration::from_millis(200), grx.recv()).await
        {
            if matches!(frame, GlobalSessionEvent::ForksChanged(_)) {
                break;
            }
        }

        sup.tell(SessionSupervisorCommand::ForksChanged {
            id: id.clone(),
            forks: rows,
        })
        .await
        .unwrap();
        // Round-trip something else so the repeat has definitely been handled.
        let _ = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();

        while let Ok(Ok(frame)) = tokio::time::timeout(Duration::from_millis(200), grx.recv()).await
        {
            assert!(
                !matches!(frame, GlobalSessionEvent::ForksChanged(_)),
                "an unchanged roster must not broadcast"
            );
        }
    }

    /// A session reports its whole roster after every persisted batch. Writing
    /// a row for each would journal one per batch for the life of the session.
    #[tokio::test]
    async fn re_reporting_the_same_forks_journals_nothing() {
        let f = fixture().await;
        let journal = f.journal();
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        let rows = vec![ForkRow {
            id: Uuid::from_bytes([7; 16]),
            parent: None,
            title: None,
            status: crate::sessions::session_actor::AgentStatus::Idle,
            created_at_ms: 5,
            last_activity_ms: 5,
        }];

        sup.tell(SessionSupervisorCommand::ForksChanged {
            id: id.clone(),
            forks: rows.clone(),
        })
        .await
        .unwrap();
        wait_for_forks(&sup, &id).await;
        let pid = PersistenceId::new(SUPERVISOR_KIND, sup.account().as_str());
        let after_first = journal.last_seq(&pid).await.unwrap();

        sup.tell(SessionSupervisorCommand::ForksChanged {
            id: id.clone(),
            forks: rows,
        })
        .await
        .unwrap();
        // Round-trip something else so the repeat has definitely been handled.
        let _ = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert_eq!(
            journal.last_seq(&pid).await.unwrap(),
            after_first,
            "an unchanged roster is not worth a write"
        );
    }

    #[tokio::test]
    async fn an_unloaded_session_reports_the_status_in_its_journal() {
        // The supervisor's status map is a cache. Reading a session must ask the
        // actor, which recovers the truth from its journal — otherwise a session
        // that parked on a question and then went cold reports nothing, and the
        // UI has no way to know the question is still answerable.
        let f = fixture().await;
        let journal = f.journal();
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        // Creating a session loads it — it has a runtime to build — so unload it
        // first. Seeding the journal behind a live actor would prove nothing
        // about where a *reload* reads its status from.
        assert!(
            await_signal(&f.agent, &format!("create:{id}")).await,
            "the create has to finish before the session can go idle"
        );
        f.go_idle(&sup).await;
        assert!(
            await_signal(&f.agent, &format!("hibernate:{id}")).await,
            "the session must actually unload for this test to mean anything"
        );

        // The session asked a question in an earlier incarnation. `Create` left
        // a status in the cache, so a cache read would answer with the wrong
        // thing.
        // Appended where the log actually ends: this session has already run, so
        // its log is not empty and a writer claiming otherwise is exactly what
        // the fence rejects.
        let pid = SessionActor::persistence_id_for(Uuid::parse_str(&id).unwrap());
        let at = journal.last_seq(&pid).await.unwrap();
        journal
            .persist(
                &pid,
                &[serde_json::to_vec(&SessionDomainEvent::AskRecorded { at_ms: 0 }).unwrap()],
                at,
            )
            .await
            .unwrap();

        let row = sup
            .ask(|reply| SessionSupervisorCommand::Get {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap();
        let (_, snapshot) = row.expect("the session still exists");
        let snapshot = snapshot.expect("a known session answers with its state");
        assert_eq!(
            snapshot.status,
            SessionStatus::AwaitingInput,
            "a session parked on a question reports it, whatever the questions were"
        );
    }

    /// Ask whether an agent has moved, the way the SSE handler does.
    async fn poll(
        sup: &SupervisorRef,
        id: &SessionId,
        after: Option<crate::sessions::Revision>,
    ) -> Option<crate::sessions::Revision> {
        sup.ask(|reply| SessionSupervisorCommand::AwaitAgentRevision {
            id: id.clone(),
            agent_id: None,
            after,
            reply,
        })
        .await
        .unwrap()
    }

    /// A reader waiting on an agent's position must survive an idle offload.
    ///
    /// Not a nicety — it is what stops offload from being a loop. Disconnect a
    /// browser and it reconnects; a reconnect reads, a read loads the session,
    /// and a loaded session goes idle and offloads again. Keeping the channel
    /// on the supervisor rather than the actor means the reader simply waits.
    ///
    /// This is the same property the old session-frame channel had, restored
    /// per-agent now that one log carries everything a client reads.
    #[tokio::test]
    async fn a_reader_survives_an_offload_and_hears_the_reload() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        let start = poll(&sup, &id, None)
            .await
            .expect("the main agent is watchable");
        // Asking must not load the session: waiting on one is no reason to wake
        // it, and an ask that loaded would restart the idle clock on every
        // reconnect.
        let _ = sup
            .ask(|reply| SessionSupervisorCommand::UsageStats {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap();

        // Move the agent off its starting revision first. A session that never
        // moved sits at the same zero a discarded registry would be rebuilt at,
        // which would let this test pass without the registry surviving at all.
        let _ = sup
            .ask(|reply| SessionSupervisorCommand::UserMessage {
                id: id.clone(),
                agent_id: None,
                text: "first".into(),
                reply,
            })
            .await
            .unwrap();
        let seen = tokio::time::timeout(Duration::from_secs(5), poll(&sup, &id, Some(start)))
            .await
            .expect("the agent moves once it has something to answer")
            .expect("the main agent is watchable");
        assert_ne!(seen, start, "the agent must have actually moved");

        f.go_idle(&sup).await;
        assert!(
            await_signal(&f.agent, &format!("hibernate:{id}")).await,
            "the session must actually unload for this test to mean anything"
        );

        // The reader is between polls when the offload lands, holding no
        // receiver at all. Its next ask must still find the session watchable
        // and its revision where it left it — an offload is not news.
        let mut waiting = Box::pin(poll(&sup, &id, Some(seen)));
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut waiting)
                .await
                .is_err(),
            "an offload must not look to a reader like the agent moved"
        );

        // And that same wait hears the session's next move.
        let _ = sup
            .ask(|reply| SessionSupervisorCommand::UserMessage {
                id: id.clone(),
                agent_id: None,
                text: "second".into(),
                reply,
            })
            .await
            .unwrap();
        let moved = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the reloaded session must publish into the same channel")
            .expect("the agent is still watchable");
        assert_ne!(moved, seen, "the reloaded session moved the agent on");
    }

    /// A host that never saw the request creating an account can still build
    /// that account's supervisor, from the id alone.
    ///
    /// This is what a shard recipe does after a failover, so the test resolves
    /// one the same way rather than constructing it: a recipe that needed
    /// anything the original caller had would compile and then fail in exactly
    /// the situation clustering exists for.
    #[tokio::test]
    async fn a_supervisor_can_be_built_from_its_account_id_alone() {
        let f = fixture().await;
        let account = crate::auth::UserId::new("some-account");
        let reference = || {
            SupervisorRef::new(
                f.node
                    .users
                    .shared()
                    .system
                    .shard_actor_of::<SupervisorShard>(),
                account.clone(),
            )
        };

        // Nothing has built this account: the first command is what does, from
        // the id in the command and the recipe this node registered.
        let id = create(&reference()).await;

        // A second, independently resolved reference names the same actor
        // rather than a second one racing it for the same log.
        let sessions = reference()
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            vec![id],
            "the second resolution must be the same supervisor, not a fresh one"
        );
    }

    /// The two halves of the long poll: answer at once when there is news, and
    /// hold when there is not.
    #[tokio::test]
    async fn a_poll_answers_at_once_only_when_the_agent_moved() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let id = create(&sup).await;

        // A first ask carries no revision, so there is nothing it could be
        // waiting for. It says where the agent is and returns.
        let seen = tokio::time::timeout(Duration::from_secs(5), poll(&sup, &id, None))
            .await
            .expect("a first ask never waits")
            .expect("the main agent is watchable");

        // Asking again from that same revision is a wait, and waiting is the
        // whole point: an ask that returned here would spin the reader.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), poll(&sup, &id, Some(seen)))
                .await
                .is_err(),
            "a reader that is up to date must wait, not be answered"
        );

        // A revision from a session that does not exist is not a wait either.
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(5),
                poll(&sup, &"nope".to_string(), None)
            )
            .await
            .expect("an unknown session is answered, not waited on"),
            None,
            "an unknown session ends the reader's stream"
        );
    }

    #[tokio::test]
    async fn an_idle_session_is_unloaded_and_hibernated() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        sup.ask(|reply| SessionSupervisorCommand::UsageStats {
            id: id.clone(),
            reply,
        })
        .await
        .unwrap();

        // Not idle yet: a sweep now must leave it alone.
        sup.tell(SessionSupervisorCommand::Tick).await.unwrap();
        let _ = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert!(
            !f.agent.signals().contains(&format!("hibernate:{id}")),
            "a session inside its idle window must not be unloaded"
        );

        f.go_idle(&sup).await;
        let _ = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert!(
            await_signal(&f.agent, &format!("hibernate:{id}")).await,
            "going cold must tell the vendor: {:?}",
            f.agent.signals()
        );
    }

    #[tokio::test]
    async fn a_reloaded_session_never_creates_a_second_runtime() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        assert!(await_signal(&f.agent, &format!("create:{id}")).await);

        for _ in 0..3 {
            // A cheap call that does not load the session, so the loop tests
            // reload behaviour rather than its own side effects.
            let _ = sup
                .ask(|reply| SessionSupervisorCommand::List { reply })
                .await
                .unwrap();
            f.go_idle(&sup).await;
            let _ = sup
                .ask(|reply| SessionSupervisorCommand::List { reply })
                .await
                .unwrap();
        }

        let creates = f
            .agent
            .signals()
            .iter()
            .filter(|s| s.starts_with("create:"))
            .count();
        assert_eq!(
            creates, 1,
            "a runtime is provisioned once per session, ever"
        );
    }

    #[tokio::test]
    async fn unknown_session_routes_to_not_found() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let res = sup
            .ask(|reply| SessionSupervisorCommand::UserMessage {
                id: "missing".into(),
                agent_id: None,
                text: "hi".into(),
                reply,
            })
            .await
            .unwrap();
        assert!(matches!(res, Err(UserMessageError::NotFound)));
    }

    #[tokio::test]
    async fn rename_is_durable_and_publishes_the_current_title() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let mut grx = f.node.services().await.global_events.subscribe();
        let id = sup
            .ask(|reply| SessionSupervisorCommand::Create {
                spec: SessionSpec {
                    name: None,
                    ..spec_fixture()
                },
                created_at: 1,
                reply,
            })
            .await
            .unwrap();
        sup.ask(|reply| SessionSupervisorCommand::RenameSession {
            id: id.clone(),
            name: "Investigate login failure".into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap();
        sup.tell(SessionSupervisorCommand::PublishSessionTitle {
            id: id.clone(),
            name: "Investigate login failure".into(),
        })
        .await
        .unwrap();

        loop {
            let frame = tokio::time::timeout(Duration::from_secs(2), grx.recv())
                .await
                .unwrap()
                .unwrap();
            if let GlobalSessionEvent::TitleChanged(event) = frame {
                assert_eq!(event.session_id, id);
                assert_eq!(event.name, "Investigate login failure");
                break;
            }
        }
    }

    /// A person renaming a session, as opposed to the agent titling one.
    ///
    /// The two used to be the same writer, so a session the model never titled
    /// kept its first message as its name for good. This one holds the title
    /// tool's rule, refuses an id it does not know, and — crucially — does not
    /// load the session: waking one re-attempts an interrupted provision, and
    /// nobody expects typing a name to start a machine.
    #[tokio::test]
    async fn a_person_can_rename_a_session_without_waking_it() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let mut grx = f.node.services().await.global_events.subscribe();
        let id = sup
            .ask(|reply| SessionSupervisorCommand::Create {
                spec: SessionSpec {
                    name: None,
                    ..spec_fixture()
                },
                created_at: 1,
                reply,
            })
            .await
            .unwrap();

        let named = sup
            .ask(|reply| SessionSupervisorCommand::SetSessionTitle {
                id: id.clone(),
                name: "  Investigate login failure  ".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            named, "Investigate login failure",
            "trimmed like the tool's"
        );

        let listed = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert_eq!(
            listed
                .iter()
                .find(|(sid, ..)| *sid == id)
                .and_then(|(_, rec)| rec.spec.name.as_deref()),
            Some("Investigate login failure"),
            "the rename is durable, not just broadcast"
        );

        loop {
            let frame = tokio::time::timeout(Duration::from_secs(2), grx.recv())
                .await
                .unwrap()
                .unwrap();
            if let GlobalSessionEvent::TitleChanged(event) = frame {
                assert_eq!(event.name, "Investigate login failure");
                break;
            }
        }

        assert_eq!(
            sup.ask(|reply| SessionSupervisorCommand::SetSessionTitle {
                id: id.clone(),
                name: "one\ntwo".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap_err(),
            RenameSessionError::Invalid("session title must be a single line".into()),
        );
        assert_eq!(
            sup.ask(|reply| SessionSupervisorCommand::SetSessionTitle {
                id: "missing".into(),
                name: "anything".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap_err(),
            RenameSessionError::NotFound("missing".into()),
        );
    }

    /// The same rule against a session that has actually gone cold.
    ///
    /// A reference is a name now, and reaching through one is precisely what
    /// brings a session back — so "do not wake it" cannot be a property of the
    /// reference and has to be the supervisor declining to use one. Counted in
    /// hosted actors rather than in vendor calls, because a reload only calls
    /// the vendor when it has a provision to finish, and the point here is that
    /// nothing loaded at all.
    #[tokio::test]
    async fn renaming_a_cold_session_leaves_it_cold() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        assert!(await_signal(&f.agent, &format!("create:{id}")).await);
        wait_for_status(&sup, &id, &SessionStatus::Idle).await;
        f.go_idle(&sup).await;
        assert!(
            await_signal(&f.agent, &format!("hibernate:{id}")).await,
            "the session must actually unload for this test to mean anything"
        );
        let cold = f.node.users.shared().system.hosted();

        sup.ask(|reply| SessionSupervisorCommand::SetSessionTitle {
            id: id.clone(),
            name: "Renamed while cold".into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            f.node.users.shared().system.hosted(),
            cold,
            "renaming must not bring the session back"
        );
        let listed = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert_eq!(
            listed
                .iter()
                .find(|(sid, ..)| *sid == id)
                .and_then(|(_, rec)| rec.spec.name.as_deref()),
            Some("Renamed while cold"),
            "and the rename still takes"
        );
    }

    #[tokio::test]
    async fn a_tag_rides_the_session_list_and_comes_off_again() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let id = create(&sup).await;
        sup.ask(|reply| SessionSupervisorCommand::SetSessionAnnotations {
            id: id.clone(),
            set: BTreeMap::from([("tag.web".to_string(), String::new())]),
            remove: vec![],
            reply,
        })
        .await
        .unwrap()
        .unwrap();

        let sessions = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert_eq!(
            sessions[0].1.annotations.get("tag.web"),
            Some(&String::new())
        );

        // No registry to clean up: dropping the key from the last session
        // carrying it is the whole of deleting a tag.
        sup.ask(|reply| SessionSupervisorCommand::SetSessionAnnotations {
            id: id.clone(),
            set: BTreeMap::new(),
            remove: vec!["tag.web".to_string()],
            reply,
        })
        .await
        .unwrap()
        .unwrap();
        let sessions = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert!(sessions[0].1.annotations.is_empty());
    }

    #[tokio::test]
    async fn set_annotations_on_unknown_session_errors() {
        let f = fixture().await;
        let sup = f.supervisor().await;
        let err = sup
            .ask(|reply| SessionSupervisorCommand::SetSessionAnnotations {
                id: "nope".into(),
                set: BTreeMap::new(),
                remove: vec![],
                reply,
            })
            .await
            .unwrap();
        assert!(err.is_err());
    }
}
