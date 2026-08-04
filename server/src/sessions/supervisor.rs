//! The session registry: which sessions exist, which are loaded, and when a
//! loaded one goes cold.
//!
//! What is persisted here is **existence only** — created, named, deleted.
//! Status is not: the session's own journal is the truth for that, and this
//! actor keeps a cache filled in when a session loads and reports in. So after
//! a restart the list renders with every status unknown until someone opens a
//! session, which is the honest thing to show.
//!
//! Nothing is loaded at boot. A session actor is spawned the first time a
//! command is addressed to it, and dropped again once it has been idle for
//! [`SupervisorConfig::idle_timeout`].

use crate::sessions::clock::{Clock, SystemClock};
use crate::sessions::session_actor::{
    AnswerError, AskAnswer, FRAME_BROADCAST_CAPACITY, SessionActor, SessionCommand,
    SessionSnapshot, SessionUsageStats,
};
use crate::sessions::spec::{
    ServerDeps, SessionId, SessionSpec, SessionStatus, status_kind, status_reason,
};
use crate::sessions::{SessionFrame, UserMessageError};
use async_trait::async_trait;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_models::session::{
    GlobalSessionEvent, GlobalSessionStatusEvent, GlobalSessionTitleEvent,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

/// How long a loaded session may sit untouched before it is unloaded and its
/// runtime hibernated.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// How often the supervisor looks for sessions to unload.
const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(10);

/// Knobs the idle policy reads. Separated so tests drive time explicitly.
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
pub enum SessionSupervisorCommand {
    /// Create a new session; replies with its generated id.
    Create {
        spec: SessionSpec,
        /// Unix epoch millis (supplied by the caller for deterministic tests).
        created_at: u64,
        reply: oneshot::Sender<SessionId>,
    },
    /// List every known session. `status` is `None` for one that is not loaded.
    List {
        reply: oneshot::Sender<Vec<(SessionId, SessionRecord, Option<SessionStatus>)>>,
    },
    /// Fetch one session's row and the state its actor recovered. Loads the
    /// session: its journal is the truth, and the actor is the only thing that
    /// reads it.
    Get {
        id: SessionId,
        reply: oneshot::Sender<Option<(SessionRecord, Option<SessionSnapshot>)>>,
    },
    /// Route a user message to the session, loading it if necessary.
    UserMessage {
        id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<String, UserMessageError>>,
    },
    /// Cancel the session's turn in flight.
    Stop {
        id: SessionId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Delete a session; the vendor decides its runtime's fate.
    Delete {
        id: SessionId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Hand back a live frame subscriber, or `None` if the session is unknown.
    Subscribe {
        id: SessionId,
        reply: oneshot::Sender<Option<broadcast::Receiver<SessionFrame>>>,
    },
    /// Read a window of a session's conversation history. `agent_id` selects
    /// the agent: absent or `"main"` for the primary agent, else a subagent
    /// id. The outer `None` means the session is unknown; an inner `None`
    /// means the agent is.
    History {
        id: SessionId,
        agent_id: Option<String>,
        query: horsie_workflow::HistoryQuery,
        reply: oneshot::Sender<Option<horsie_workflow::AgentHistoryPage>>,
    },
    /// Read a session's aggregated usage.
    UsageStats {
        id: SessionId,
        reply: oneshot::Sender<Option<SessionUsageStats>>,
    },
    /// Read a session's workflow run (`None` when the session is unknown or
    /// is not a run).
    RunState {
        id: SessionId,
        reply: oneshot::Sender<Option<crate::sessions::workflow::WorkflowRunState>>,
    },
    /// Re-run one execution of a run's step.
    RetryStep {
        id: SessionId,
        index: u32,
        reply: oneshot::Sender<Option<Result<(), String>>>,
    },
    /// Read a session's subagent tree (`None` when the session is unknown).
    SubAgents {
        id: SessionId,
        reply: oneshot::Sender<Option<Vec<(Uuid, crate::sessions::subagents::SubAgentRecord)>>>,
    },
    /// Answer every pending ask of a session at once.
    Answer {
        id: SessionId,
        answers: Vec<AskAnswer>,
        reply: oneshot::Sender<Result<(), AnswerError>>,
    },
    /// Subscribe to one agent's live frames (`None` when the session or agent
    /// is unknown).
    SubscribeAgent {
        id: SessionId,
        agent_id: Option<String>,
        reply: oneshot::Sender<Option<broadcast::Receiver<crate::sessions::AgentFrame>>>,
    },
    /// Read one agent's current values, for its document.
    AgentState {
        id: SessionId,
        agent_id: Option<String>,
        reply: oneshot::Sender<Option<horsie_workflow::AgentStateView>>,
    },
    /// Unload every session that has gone idle. Sent by the ticker, or by a
    /// test that has moved its clock.
    Tick,
    /// Tear down every loaded session for a clean shutdown.
    Shutdown { reply: oneshot::Sender<()> },
    /// Internal: a session actor reports its status changed.
    SessionStatusChanged {
        id: SessionId,
        status: SessionStatus,
    },
    /// Internal: a session actor requests a durable rename.
    RenameSession {
        id: SessionId,
        name: String,
        reply: oneshot::Sender<Result<(), horsie_actor::JournalError>>,
    },
    /// Internal: publish an already-journaled title to the global live feed.
    PublishSessionTitle { id: SessionId, name: String },
    /// Register a group. `created_at` is unix epoch millis (caller-supplied for
    /// deterministic tests, like `Create`).
    CreateGroup {
        name: String,
        created_at: u64,
        reply: oneshot::Sender<Result<(), GroupError>>,
    },
    /// Rename a registered *or annotation-only* group; sessions follow.
    RenameGroup {
        old: String,
        new: String,
        reply: oneshot::Sender<Result<(), GroupError>>,
    },
    /// Delete a group and strip its annotation from every session.
    DeleteGroup {
        name: String,
        reply: oneshot::Sender<Result<(), GroupError>>,
    },
    /// The group registry, name-sorted.
    ListGroups {
        reply: oneshot::Sender<Vec<(String, GroupRecord)>>,
    },
    /// Merge-update one session's annotations. Err when the session is unknown.
    SetSessionAnnotations {
        id: SessionId,
        set: BTreeMap<String, String>,
        remove: Vec<String>,
        reply: oneshot::Sender<Result<(), String>>,
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
    GroupCreated {
        name: String,
        created_at: u64,
    },
    /// Renames the registry key and rewrites `group=<old>` annotations; both
    /// ride one event so the fixup is atomic with the rename.
    GroupRenamed {
        old: String,
        new: String,
    },
    /// Removes the registry key and strips `group=<name>` annotations.
    GroupDeleted {
        name: String,
    },
    /// Merge-update of one session's annotations: `set` upserts, `remove` drops.
    SessionAnnotationsSet {
        id: SessionId,
        set: BTreeMap<String, String>,
        remove: Vec<String>,
    },
}

/// Why a group command was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupError {
    /// Neither registered nor referenced by any session annotation.
    NotFound(String),
    /// The name is already taken (create, or rename target).
    NameTaken(String),
    /// Empty or over-long.
    Invalid(String),
}

impl std::fmt::Display for GroupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupError::NotFound(name) => write!(f, "no such group: {name}"),
            GroupError::NameTaken(name) => write!(f, "group already exists: {name}"),
            GroupError::Invalid(reason) => write!(f, "invalid group name: {reason}"),
        }
    }
}

impl std::error::Error for GroupError {}

const GROUP_NAME_MAX_LEN: usize = 128;

fn validate_group_name(name: &str) -> Result<(), GroupError> {
    if name.is_empty() {
        return Err(GroupError::Invalid("empty".into()));
    }
    if name.len() > GROUP_NAME_MAX_LEN {
        return Err(GroupError::Invalid(format!(
            "longer than {GROUP_NAME_MAX_LEN} characters"
        )));
    }
    Ok(())
}

/// Whether the group is registered or any session carries `group=<name>`.
fn group_exists(state: &SessionSupervisorState, name: &str) -> bool {
    state.groups.contains_key(name)
        || state
            .sessions
            .values()
            .any(|rec| rec.annotations.get("group").is_some_and(|g| g == name))
}

/// One registry row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub spec: SessionSpec,
    pub created_at: u64,
    /// User-set key-value metadata (group, future provenance keys). Field-level
    /// default so pre-annotations journal rows load with an empty map.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}

/// One registered group. Registration is optional metadata: a group can exist
/// with zero sessions, and an annotation can name a group that was never
/// registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRecord {
    pub created_at: u64,
}

/// Persisted supervisor state — which sessions exist, nothing more.
///
/// `#[serde(default)]` on the container: snapshotted state is a durability
/// contract. Add optional fields; never rename or repurpose one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSupervisorState {
    pub sessions: BTreeMap<SessionId, SessionRecord>,
    /// Registered groups, name-keyed.
    #[serde(default)]
    pub groups: BTreeMap<String, GroupRecord>,
}

pub struct SessionSupervisor {
    deps: ServerDeps,
    global_tx: broadcast::Sender<GlobalSessionEvent>,
    config: SupervisorConfig,
    children: BTreeMap<SessionId, ActorRef<SessionCommand>>,
    /// Last known status of each *loaded* session. Absent means "not loaded",
    /// which the API reports as unknown rather than guessing.
    status: BTreeMap<SessionId, SessionStatus>,
    last_activity: BTreeMap<SessionId, Instant>,
    /// One live-frame channel per session, owned here rather than by the actor
    /// so that unloading a session does not disconnect whoever is watching it.
    /// An entry outlives its actor only while something still holds a receiver.
    frames: BTreeMap<SessionId, broadcast::Sender<SessionFrame>>,
}

impl SessionSupervisor {
    pub fn new(deps: ServerDeps, global_tx: broadcast::Sender<GlobalSessionEvent>) -> Self {
        Self::with_config(deps, global_tx, SupervisorConfig::default())
    }

    pub fn with_config(
        deps: ServerDeps,
        global_tx: broadcast::Sender<GlobalSessionEvent>,
        config: SupervisorConfig,
    ) -> Self {
        Self {
            deps,
            global_tx,
            config,
            children: BTreeMap::new(),
            status: BTreeMap::new(),
            last_activity: BTreeMap::new(),
            frames: BTreeMap::new(),
        }
    }

    /// The live child for `id`, spawning it if this is the first command to
    /// reach it. Loading reads two journals and calls no vendor.
    fn ensure_loaded(
        &mut self,
        ctx: &ActorContext<Self>,
        state: &SessionSupervisorState,
        id: &SessionId,
    ) -> Option<ActorRef<SessionCommand>> {
        self.last_activity
            .insert(id.clone(), self.config.clock.now());
        if let Some(child) = self.children.get(id) {
            return Some(child.clone());
        }
        let record = state.sessions.get(id)?;
        let uuid = match Uuid::parse_str(id) {
            Ok(uuid) => uuid,
            Err(e) => {
                tracing::error!(session_id = %id, error = %e, "unparseable session id");
                return None;
            }
        };
        let frames = self.frames_for(id);
        let child = ctx.spawn(SessionActor::new(
            uuid,
            record.spec.clone(),
            self.deps.clone(),
            ctx.self_ref(),
            frames,
        ));
        self.children.insert(id.clone(), child.clone());
        Some(child)
    }

    fn publish(&self, id: &str, status: &SessionStatus) {
        let _ = self.global_tx.send(GlobalSessionEvent::StatusChanged(
            GlobalSessionStatusEvent {
                session_id: id.to_string(),
                status: status_kind(status),
                reason: status_reason(status),
            },
        ));
    }

    fn publish_title(&self, id: &str, name: &str) {
        let _ = self
            .global_tx
            .send(GlobalSessionEvent::TitleChanged(GlobalSessionTitleEvent {
                session_id: id.to_string(),
                name: name.to_string(),
            }));
    }

    /// This session's live-frame channel, created on first use.
    fn frames_for(&mut self, id: &SessionId) -> broadcast::Sender<SessionFrame> {
        self.frames
            .entry(id.clone())
            .or_insert_with(|| broadcast::channel(FRAME_BROADCAST_CAPACITY).0)
            .clone()
    }

    fn forget(&mut self, id: &SessionId) {
        self.children.remove(id);
        self.status.remove(id);
        self.last_activity.remove(id);
        // The channel outlives the actor while anyone is still watching: an
        // unloaded session has nothing to say until something reloads it, and
        // ending the stream would only make the client reconnect and reload it.
        if self
            .frames
            .get(id)
            .is_none_or(|tx| tx.receiver_count() == 0)
        {
            self.frames.remove(id);
        }
    }

    /// Unload every session that has been idle past the timeout.
    ///
    /// Runs inline on this mailbox, which is what makes it race-free: every
    /// command to a child goes through here, so nothing can reach a session
    /// between it agreeing to unload and its reference being dropped.
    async fn offload_idle(&mut self) {
        let now = self.config.clock.now();
        let timeout = self.config.idle_timeout;
        let candidates: Vec<SessionId> = self
            .children
            .keys()
            .filter(|id| {
                // A running session is never a candidate: a long tool call must
                // not be unloaded out from under itself.
                if self.status.get(*id) == Some(&SessionStatus::Running) {
                    return false;
                }
                self.last_activity
                    .get(*id)
                    .is_none_or(|last| now.duration_since(*last) >= timeout)
            })
            .cloned()
            .collect();

        for id in candidates {
            let Some(child) = self.children.get(&id).cloned() else {
                continue;
            };
            match child
                .ask(|reply| SessionCommand::PrepareOffload { reply })
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
                // Its mailbox is gone, so it is already unloaded.
                Err(_) => self.forget(&id),
            }
        }
    }
}

#[async_trait]
impl EventSourcedActor for SessionSupervisor {
    type Command = SessionSupervisorCommand;
    type Event = SessionSupervisorEvent;
    type State = SessionSupervisorState;

    fn persistence_id(&self) -> PersistenceId {
        PersistenceId::new("session-supervisor", "main")
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
            SessionSupervisorEvent::GroupCreated { name, created_at } => {
                state.groups.insert(name, GroupRecord { created_at });
            }
            SessionSupervisorEvent::GroupRenamed { old, new } => {
                if let Some(rec) = state.groups.remove(&old) {
                    state.groups.insert(new.clone(), rec);
                }
                for rec in state.sessions.values_mut() {
                    if rec.annotations.get("group") == Some(&old) {
                        rec.annotations.insert("group".to_string(), new.clone());
                    }
                }
            }
            SessionSupervisorEvent::GroupDeleted { name } => {
                state.groups.remove(&name);
                for rec in state.sessions.values_mut() {
                    if rec.annotations.get("group") == Some(&name) {
                        rec.annotations.remove("group");
                    }
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

    async fn handle_command(
        &mut self,
        state: &SessionSupervisorState,
        cmd: SessionSupervisorCommand,
        ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<SessionSupervisorEvent> {
        match cmd {
            SessionSupervisorCommand::Create {
                spec,
                created_at,
                reply,
            } => {
                let id = Uuid::new_v4().to_string();
                // The runtime is provisioned exactly here, exactly once in a
                // session's life — that single call site *is* the guarantee.
                // Detached, because a create legitimately runs for minutes and
                // the first turn's `get` is what waits for it.
                let runtimes = self.deps.runtimes.clone();
                let vendor = spec.vendor.clone();
                let spec_for_create = spec.clone();
                let create_id = id.clone();
                tokio::spawn(async move {
                    if let Err(e) = runtimes.create(&create_id, &vendor, &spec_for_create).await {
                        tracing::error!(session = %create_id, error = %e, "runtime create failed");
                    }
                });
                let _ = reply.send(id.clone());
                // A fresh session's status is not a guess, so seed the cache:
                // it is idle, with a runtime being provisioned behind it.
                self.status.insert(id.clone(), SessionStatus::Idle);
                self.publish(&id, &SessionStatus::Idle);
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
                    .map(|(id, rec)| (id.clone(), rec.clone(), self.status.get(id).cloned()))
                    .collect();
                let _ = reply.send(sessions);
                CommandEffect::none()
            }
            SessionSupervisorCommand::Get { id, reply } => {
                let Some(record) = state.sessions.get(&id).cloned() else {
                    let _ = reply.send(None);
                    return CommandEffect::none();
                };
                match self.ensure_loaded(ctx, state, &id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child.tell(SessionCommand::Snapshot { reply: tx }).await;
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
            SessionSupervisorCommand::UserMessage { id, text, reply } => {
                match self.ensure_loaded(ctx, state, &id) {
                    None => {
                        let _ = reply.send(Err(UserMessageError::NotFound));
                    }
                    Some(child) => {
                        let _ = child
                            .tell(SessionCommand::UserMessage { text, reply })
                            .await;
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::Stop { id, reply } => {
                match self.ensure_loaded(ctx, state, &id) {
                    None => {
                        let _ = reply.send(Err(format!("no such session: {id}")));
                    }
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        if child
                            .tell(SessionCommand::Stop { reply: tx })
                            .await
                            .is_err()
                        {
                            let _ = reply.send(Err("session unavailable".to_string()));
                        } else {
                            tokio::spawn(async move {
                                let _ = rx.await;
                                let _ = reply.send(Ok(()));
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
                if let Some(child) = self.ensure_loaded(ctx, state, &id) {
                    let (tx, rx) = oneshot::channel();
                    if child
                        .tell(SessionCommand::Delete { reply: tx })
                        .await
                        .is_ok()
                    {
                        let _ = rx.await;
                    }
                }
                self.forget(&id);
                // A deleted session has no stream to keep alive.
                self.frames.remove(&id);
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionDeleted { id }])
            }
            SessionSupervisorCommand::Subscribe { id, reply } => {
                // Answered here, not by the actor: a broadcast channel is
                // transport, not state the actor owns, and watching a session
                // is no reason to load one.
                let receiver = state
                    .sessions
                    .contains_key(&id)
                    .then(|| self.frames_for(&id).subscribe());
                let _ = reply.send(receiver);
                CommandEffect::none()
            }
            SessionSupervisorCommand::History {
                id,
                agent_id,
                query,
                reply,
            } => {
                match self.ensure_loaded(ctx, state, &id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child
                            .tell(SessionCommand::History {
                                agent_id,
                                query,
                                reply: tx,
                            })
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
                match self.ensure_loaded(ctx, state, &id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child.tell(SessionCommand::UsageStats { reply: tx }).await;
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
                match self.ensure_loaded(ctx, state, &id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child.tell(SessionCommand::RunState { reply: tx }).await;
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
                match self.ensure_loaded(ctx, state, &id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child
                            .tell(SessionCommand::RetryStep { index, reply: tx })
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
            SessionSupervisorCommand::SubAgents { id, reply } => {
                match self.ensure_loaded(ctx, state, &id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child.tell(SessionCommand::SubAgentTree { reply: tx }).await;
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
            SessionSupervisorCommand::Answer { id, answers, reply } => {
                match self.ensure_loaded(ctx, state, &id) {
                    None => {
                        let _ = reply.send(Err(AnswerError::NothingPending));
                    }
                    Some(child) => {
                        let _ = child.tell(SessionCommand::Answer { answers, reply }).await;
                    }
                }
                CommandEffect::none()
            }
            SessionSupervisorCommand::SubscribeAgent {
                id,
                agent_id,
                reply,
            } => {
                match self.ensure_loaded(ctx, state, &id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child
                            .tell(SessionCommand::SubscribeAgent {
                                agent_id,
                                reply: tx,
                            })
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
            SessionSupervisorCommand::AgentState {
                id,
                agent_id,
                reply,
            } => {
                match self.ensure_loaded(ctx, state, &id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child
                            .tell(SessionCommand::AgentState {
                                agent_id,
                                reply: tx,
                            })
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
                self.offload_idle().await;
                CommandEffect::none()
            }
            SessionSupervisorCommand::Shutdown { reply } => {
                let ids: Vec<SessionId> = self.children.keys().cloned().collect();
                for id in ids {
                    if let Some(child) = self.children.get(&id) {
                        let _ = child
                            .ask(|reply| SessionCommand::PrepareOffload { reply })
                            .await;
                    }
                    self.forget(&id);
                }
                let _ = reply.send(());
                CommandEffect::none()
            }
            SessionSupervisorCommand::SessionStatusChanged { id, status } => {
                self.publish(&id, &status);
                self.status.insert(id, status);
                CommandEffect::none()
            }
            SessionSupervisorCommand::RenameSession { id, name, reply } => {
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionNamed { id, name }])
                    .and_ack(reply)
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
            SessionSupervisorCommand::CreateGroup {
                name,
                created_at,
                reply,
            } => {
                if let Err(e) = validate_group_name(&name) {
                    let _ = reply.send(Err(e));
                    return CommandEffect::none();
                }
                if state.groups.contains_key(&name) {
                    let _ = reply.send(Err(GroupError::NameTaken(name)));
                    return CommandEffect::none();
                }
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::GroupCreated {
                    name,
                    created_at,
                }])
            }
            SessionSupervisorCommand::RenameGroup { old, new, reply } => {
                if let Err(e) = validate_group_name(&new) {
                    let _ = reply.send(Err(e));
                    return CommandEffect::none();
                }
                // The fold inserts `new` unconditionally; without this guard a
                // rename onto an existing group silently overwrites it.
                if state.groups.contains_key(&new) {
                    let _ = reply.send(Err(GroupError::NameTaken(new)));
                    return CommandEffect::none();
                }
                if !group_exists(state, &old) {
                    let _ = reply.send(Err(GroupError::NotFound(old)));
                    return CommandEffect::none();
                }
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::GroupRenamed { old, new }])
            }
            SessionSupervisorCommand::DeleteGroup { name, reply } => {
                if !group_exists(state, &name) {
                    let _ = reply.send(Err(GroupError::NotFound(name)));
                    return CommandEffect::none();
                }
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::GroupDeleted { name }])
            }
            SessionSupervisorCommand::ListGroups { reply } => {
                let _ = reply.send(
                    state
                        .groups
                        .iter()
                        .map(|(name, rec)| (name.clone(), rec.clone()))
                        .collect(),
                );
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

    /// Recovery rebuilds the registry and stops there. No session actor is
    /// spawned, no journal but this one is read, and no vendor is called — a
    /// restart costs one journal replay however many sessions exist.
    async fn on_recovery_complete(
        &mut self,
        _state: &SessionSupervisorState,
        ctx: &mut ActorContext<Self>,
    ) {
        if let Some(interval) = self.config.tick_interval {
            let me = ctx.self_ref();
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
    use crate::sessions::clock::TestClock;
    use crate::sessions::session_actor::SessionDomainEvent;
    use crate::sessions::spec::AgentSettings;
    use horsie_actor::{InMemoryJournal, Journal, spawn_root};
    use std::collections::HashMap;

    fn spec_fixture() -> SessionSpec {
        SessionSpec {
            name: Some("test".into()),
            agent: AgentSettings {
                model: "mock".into(),
                allowed_tools: None,
                use_plugins: None,
                max_iterations: None,
                max_retries: 0,
                mcp_servers: vec![],
                memory_spaces: vec![],
                thinking_effort: None,
                max_concurrent_subagents: None,
            },
            workspaces: vec![],
            provision: vec![],
            vendor: "mock".into(),
            plugins: vec![],
            origin: crate::sessions::spec::SessionOrigin::User,
            workflow: None,
        }
    }

    struct Fixture {
        deps: ServerDeps,
        agent: FakeRuntimeVendor,
        _tmp: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let agent = FakeRuntimeVendor::builder("mock")
            .serve_in_process()
            .await
            .expect("fake agent");
        let mut vendors = HashMap::new();
        vendors.insert("mock".to_string(), agent.link());
        let vendors = Arc::new(std::sync::RwLock::new(vendors));
        let deps = ServerDeps {
            runtimes: crate::runtime_manager::test_runtime_manager(&vendors, tmp.path()),
            provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
            vendors,
            state_dir: tmp.path().to_path_buf(),
            github_tokens: None,
            mcp: None,
            plugins: None,
            memory: None,
        };
        Fixture {
            deps,
            agent,
            _tmp: tmp,
        }
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

    async fn create(sup: &ActorRef<SessionSupervisorCommand>) -> SessionId {
        sup.ask(|reply| SessionSupervisorCommand::Create {
            spec: spec_fixture(),
            created_at: 1,
            reply,
        })
        .await
        .unwrap()
    }

    async fn spawn_supervisor(f: &Fixture) -> ActorRef<SessionSupervisorCommand> {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let clock: Arc<TestClock> = Arc::new(TestClock::new());
        let (gtx, _) = broadcast::channel(16);
        spawn_root(
            SessionSupervisor::with_config(f.deps.clone(), gtx, manual_config(&clock)),
            journal,
        )
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
                set: BTreeMap::from([("group".to_string(), "web".to_string())]),
                remove: vec![],
            },
        );
        assert_eq!(
            s.sessions.get("s1").unwrap().annotations.get("group"),
            Some(&"web".to_string())
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::new(),
                remove: vec!["group".to_string()],
            },
        );
        assert!(s.sessions.get("s1").unwrap().annotations.is_empty());
    }

    #[test]
    fn group_rename_rewrites_annotations() {
        let s = created_session(SessionSupervisorState::default(), "s1");
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::GroupCreated {
                name: "web".into(),
                created_at: 1,
            },
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::from([("group".to_string(), "web".to_string())]),
                remove: vec![],
            },
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::GroupRenamed {
                old: "web".into(),
                new: "frontend".into(),
            },
        );
        assert!(s.groups.contains_key("frontend"));
        assert!(!s.groups.contains_key("web"));
        assert_eq!(
            s.sessions.get("s1").unwrap().annotations.get("group"),
            Some(&"frontend".to_string())
        );
    }

    #[test]
    fn group_delete_strips_annotations() {
        let s = created_session(SessionSupervisorState::default(), "s1");
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::GroupCreated {
                name: "web".into(),
                created_at: 1,
            },
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::from([("group".to_string(), "web".to_string())]),
                remove: vec![],
            },
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::GroupDeleted { name: "web".into() },
        );
        assert!(s.groups.is_empty());
        assert!(s.sessions.get("s1").unwrap().annotations.is_empty());
    }

    #[test]
    fn session_delete_drops_its_annotations() {
        let s = created_session(SessionSupervisorState::default(), "s1");
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::from([("group".to_string(), "web".to_string())]),
                remove: vec![],
            },
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionDeleted { id: "s1".into() },
        );
        assert!(s.sessions.is_empty());
    }

    #[tokio::test]
    async fn boot_loads_nothing() {
        let f = fixture().await;
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let clock: Arc<TestClock> = Arc::new(TestClock::new());
        let (gtx, _) = broadcast::channel(16);

        let sup = spawn_root(
            SessionSupervisor::with_config(f.deps.clone(), gtx.clone(), manual_config(&clock)),
            journal.clone(),
        );
        let id = create(&sup).await;
        assert!(await_signal(&f.agent, &format!("create:{id}")).await);
        sup.ask(|reply| SessionSupervisorCommand::Shutdown { reply })
            .await
            .unwrap();
        let before = f.agent.signals();

        // Second incarnation on the same journal: the registry comes back, but
        // nothing is loaded and no vendor is touched.
        let sup2 = spawn_root(
            SessionSupervisor::with_config(f.deps.clone(), gtx, manual_config(&clock)),
            journal,
        );
        let rows = sup2
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        let (_, _, status) = rows
            .into_iter()
            .find(|(row_id, _, _)| row_id == &id)
            .expect("the session still exists");
        assert!(
            status.is_none(),
            "listing sessions must not load one to find its status"
        );
        assert_eq!(
            f.agent.signals(),
            before,
            "recovery must not call the vendor"
        );
    }

    #[tokio::test]
    async fn any_command_loads_the_session_without_acquiring_a_runtime() {
        let f = fixture().await;
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let clock: Arc<TestClock> = Arc::new(TestClock::new());
        let (gtx, _) = broadcast::channel(16);
        let sup = spawn_root(
            SessionSupervisor::with_config(f.deps.clone(), gtx, manual_config(&clock)),
            journal,
        );
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

    #[tokio::test]
    async fn an_unloaded_session_reports_the_status_in_its_journal() {
        // The supervisor's status map is a cache. Reading a session must ask the
        // actor, which recovers the truth from its journal — otherwise a session
        // that parked on a question and then went cold reports nothing, and the
        // UI has no way to know the question is still answerable.
        let f = fixture().await;
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let clock: Arc<TestClock> = Arc::new(TestClock::new());
        let (gtx, _) = broadcast::channel(16);
        let sup = spawn_root(
            SessionSupervisor::with_config(f.deps.clone(), gtx, manual_config(&clock)),
            journal.clone(),
        );
        let id = create(&sup).await;

        // The session asked a question in an earlier incarnation. `Create` left
        // `Idle` in the cache, so a cache read would answer with the wrong thing.
        let pid = SessionActor::persistence_id_for(Uuid::parse_str(&id).unwrap());
        journal
            .persist(
                &pid,
                &[serde_json::to_vec(&SessionDomainEvent::AskRecorded {
                    at_ms: 0,
                    tool_call_id: Some("call-1".into()),
                    question: "which shape?".into(),
                })
                .unwrap()],
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
        match snapshot.status {
            SessionStatus::AwaitingInput { asks } => {
                assert_eq!(asks.len(), 1);
                assert_eq!(asks[0].tool_call_id.as_deref(), Some("call-1"));
                assert_eq!(asks[0].question, "which shape?");
            }
            other => panic!("expected AwaitingInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_subscriber_survives_an_offload() {
        // The frame channel is transport, not the actor's state. Killing it at
        // offload ends every SSE stream on the session, the browser reconnects,
        // the reconnect loads the session again — a ~3 minute churn loop for as
        // long as a tab is open.
        let f = fixture().await;
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let clock: Arc<TestClock> = Arc::new(TestClock::new());
        let (gtx, _) = broadcast::channel(16);
        let sup = spawn_root(
            SessionSupervisor::with_config(f.deps.clone(), gtx, manual_config(&clock)),
            journal,
        );
        let id = create(&sup).await;
        let mut sub = sup
            .ask(|reply| SessionSupervisorCommand::Subscribe {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap()
            .expect("subscribed");
        // Subscribing does not load a session, so ask for something that does.
        let _ = sup
            .ask(|reply| SessionSupervisorCommand::UsageStats {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap();

        clock.advance(Duration::from_secs(181));
        sup.tell(SessionSupervisorCommand::Tick).await.unwrap();
        assert!(
            await_signal(&f.agent, &format!("hibernate:{id}")).await,
            "the session must actually unload for this test to mean anything"
        );

        assert!(
            !matches!(sub.try_recv(), Err(broadcast::error::TryRecvError::Closed)),
            "an offload must not close a live subscriber's stream"
        );

        // And the same receiver still sees the session's next frame.
        let _ = sup
            .ask(|reply| SessionSupervisorCommand::UserMessage {
                id: id.clone(),
                text: "hello".into(),
                reply,
            })
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("a frame must arrive within the timeout");
        assert!(
            frame.is_ok(),
            "the reloaded session publishes to the same channel"
        );
    }

    #[tokio::test]
    async fn an_idle_session_is_unloaded_and_hibernated() {
        let f = fixture().await;
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let clock: Arc<TestClock> = Arc::new(TestClock::new());
        let (gtx, _) = broadcast::channel(16);
        let sup = spawn_root(
            SessionSupervisor::with_config(f.deps.clone(), gtx, manual_config(&clock)),
            journal,
        );
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

        clock.advance(Duration::from_secs(181));
        sup.tell(SessionSupervisorCommand::Tick).await.unwrap();
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
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let clock: Arc<TestClock> = Arc::new(TestClock::new());
        let (gtx, _) = broadcast::channel(16);
        let sup = spawn_root(
            SessionSupervisor::with_config(f.deps.clone(), gtx, manual_config(&clock)),
            journal,
        );
        let id = create(&sup).await;
        assert!(await_signal(&f.agent, &format!("create:{id}")).await);

        for _ in 0..3 {
            sup.ask(|reply| SessionSupervisorCommand::Subscribe {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap();
            clock.advance(Duration::from_secs(181));
            sup.tell(SessionSupervisorCommand::Tick).await.unwrap();
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
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let clock: Arc<TestClock> = Arc::new(TestClock::new());
        let (gtx, _) = broadcast::channel(16);
        let sup = spawn_root(
            SessionSupervisor::with_config(f.deps, gtx, manual_config(&clock)),
            journal,
        );
        let res = sup
            .ask(|reply| SessionSupervisorCommand::UserMessage {
                id: "missing".into(),
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
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let clock: Arc<TestClock> = Arc::new(TestClock::new());
        let (gtx, mut grx) = broadcast::channel(16);
        let sup = spawn_root(
            SessionSupervisor::with_config(f.deps, gtx, manual_config(&clock)),
            journal,
        );
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

    #[tokio::test]
    async fn group_create_list_and_duplicate_conflict() {
        let f = fixture().await;
        let sup = spawn_supervisor(&f).await;

        sup.ask(|reply| SessionSupervisorCommand::CreateGroup {
            name: "web".into(),
            created_at: 1,
            reply,
        })
        .await
        .unwrap()
        .unwrap();

        let dup = sup
            .ask(|reply| SessionSupervisorCommand::CreateGroup {
                name: "web".into(),
                created_at: 2,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(dup, Err(GroupError::NameTaken("web".into())));

        let groups = sup
            .ask(|reply| SessionSupervisorCommand::ListGroups { reply })
            .await
            .unwrap();
        assert_eq!(
            groups,
            vec![("web".to_string(), GroupRecord { created_at: 1 })]
        );
    }

    #[tokio::test]
    async fn group_validation_rejects_empty_and_overlong_names() {
        let f = fixture().await;
        let sup = spawn_supervisor(&f).await;
        for bad in ["", &"x".repeat(129)] {
            let err = sup
                .ask(|reply| SessionSupervisorCommand::CreateGroup {
                    name: bad.to_string(),
                    created_at: 1,
                    reply,
                })
                .await
                .unwrap();
            assert!(matches!(err, Err(GroupError::Invalid(_))));
        }
    }

    #[tokio::test]
    async fn rename_unregistered_group_rewrites_annotations() {
        let f = fixture().await;
        let sup = spawn_supervisor(&f).await;
        let id = create(&sup).await;
        sup.ask(|reply| SessionSupervisorCommand::SetSessionAnnotations {
            id: id.clone(),
            set: BTreeMap::from([("group".to_string(), "web".to_string())]),
            remove: vec![],
            reply,
        })
        .await
        .unwrap()
        .unwrap();

        // "web" was never registered; the rename still fixes the annotation.
        sup.ask(|reply| SessionSupervisorCommand::RenameGroup {
            old: "web".into(),
            new: "frontend".into(),
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
            sessions[0].1.annotations.get("group"),
            Some(&"frontend".to_string())
        );
        assert!(
            sup.ask(|reply| SessionSupervisorCommand::ListGroups { reply })
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn unknown_group_rename_and_delete_are_not_found() {
        let f = fixture().await;
        let sup = spawn_supervisor(&f).await;
        let err = sup
            .ask(|reply| SessionSupervisorCommand::RenameGroup {
                old: "nope".into(),
                new: "x".into(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(err, Err(GroupError::NotFound("nope".into())));
        let err = sup
            .ask(|reply| SessionSupervisorCommand::DeleteGroup {
                name: "nope".into(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(err, Err(GroupError::NotFound("nope".into())));
    }

    #[tokio::test]
    async fn set_annotations_on_unknown_session_errors() {
        let f = fixture().await;
        let sup = spawn_supervisor(&f).await;
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
