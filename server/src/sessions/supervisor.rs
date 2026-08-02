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
use crate::sessions::session_actor::{SessionActor, SessionCommand, SessionUsageStats};
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
    /// Fetch one session's row, plus its status if it happens to be loaded.
    Get {
        id: SessionId,
        reply: oneshot::Sender<Option<(SessionRecord, Option<SessionStatus>)>>,
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
    /// Read a window of a session's conversation history.
    History {
        id: SessionId,
        query: horsie_workflow::HistoryQuery,
        reply: oneshot::Sender<Option<horsie_workflow::AgentHistoryPage>>,
    },
    /// Read a session's aggregated usage.
    UsageStats {
        id: SessionId,
        reply: oneshot::Sender<Option<SessionUsageStats>>,
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
}

/// One registry row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub spec: SessionSpec,
    pub created_at: u64,
}

/// Persisted supervisor state — which sessions exist, nothing more.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionSupervisorState {
    pub sessions: BTreeMap<SessionId, SessionRecord>,
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
        let child = ctx.spawn(SessionActor::new(
            uuid,
            record.spec.clone(),
            self.deps.clone(),
            ctx.self_ref(),
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

    fn forget(&mut self, id: &SessionId) {
        self.children.remove(id);
        self.status.remove(id);
        self.last_activity.remove(id);
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
                state
                    .sessions
                    .insert(id, SessionRecord { spec, created_at });
            }
            SessionSupervisorEvent::SessionDeleted { id } => {
                state.sessions.remove(&id);
            }
            SessionSupervisorEvent::SessionNamed { id, name } => {
                if let Some(rec) = state.sessions.get_mut(&id) {
                    rec.spec.name = Some(name);
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
                let row = state
                    .sessions
                    .get(&id)
                    .cloned()
                    .map(|rec| (rec, self.status.get(&id).cloned()));
                let _ = reply.send(row);
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
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionDeleted { id }])
            }
            SessionSupervisorCommand::Subscribe { id, reply } => {
                match self.ensure_loaded(ctx, state, &id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child.tell(SessionCommand::Subscribe { reply: tx }).await;
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
            SessionSupervisorCommand::History { id, query, reply } => {
                match self.ensure_loaded(ctx, state, &id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child
                            .tell(SessionCommand::History { query, reply: tx })
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
            },
            workspaces: vec![],
            provision: vec![],
            vendor: "mock".into(),
            plugins: vec![],
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
        let row = sup2
            .ask(|reply| SessionSupervisorCommand::Get {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap();
        let (_, status) = row.expect("the session still exists");
        assert!(
            status.is_none(),
            "an unloaded session has no status to report, and must not be loaded to find one"
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
        let sub = sup
            .ask(|reply| SessionSupervisorCommand::Subscribe {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap();
        assert!(sub.is_some(), "the session must load on demand");
        assert_eq!(
            f.agent.signals(),
            before,
            "loading a session to read it must not touch its runtime"
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
        sup.ask(|reply| SessionSupervisorCommand::Subscribe {
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
}
