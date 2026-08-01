//! The session registry: one event-sourced supervisor owning a
//! [`SessionActor`] child per live session. The registry is rebuilt by
//! replaying this actor's own journal — never by scanning disk.

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
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

/// Commands accepted by the [`SessionSupervisor`].
// `Create` inherently carries the full `SessionSpec`; the size gap to the small
// control variants is by design for a one-shot create command.
#[allow(clippy::large_enum_variant)]
pub enum SessionSupervisorCommand {
    /// Create a new session; replies with its generated id. The child begins
    /// provisioning immediately.
    Create {
        spec: SessionSpec,
        /// Unix epoch millis (supplied by the caller for deterministic tests).
        created_at: u64,
        reply: oneshot::Sender<SessionId>,
    },
    /// List all known sessions, every state.
    List {
        reply: oneshot::Sender<Vec<(SessionId, SessionRecord)>>,
    },
    /// Fetch one session's registry record.
    Get {
        id: SessionId,
        reply: oneshot::Sender<Option<SessionRecord>>,
    },
    /// Route a user message to the session (the child replies directly).
    UserMessage {
        id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<(), UserMessageError>>,
    },
    /// Stop a session (runtime stopped, preserved).
    Stop {
        id: SessionId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Delete a session (the vendor decides the runtime's fate).
    Delete {
        id: SessionId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Hand back a live frame subscriber for a session, or `None` if unknown.
    Subscribe {
        id: SessionId,
        reply: oneshot::Sender<Option<broadcast::Receiver<SessionFrame>>>,
    },
    /// Read a window of a session's conversation history, or `None` if unknown.
    History {
        id: SessionId,
        query: horsie_workflow::HistoryQuery,
        reply: oneshot::Sender<Option<horsie_workflow::AgentHistoryPage>>,
    },
    /// Read a session's aggregated usage, or `None` if unknown.
    UsageStats {
        id: SessionId,
        reply: oneshot::Sender<Option<SessionUsageStats>>,
    },
    /// Tear down every live session's OS resources for a clean shutdown.
    Shutdown { reply: oneshot::Sender<()> },
    /// Internal: a session actor reports its status changed.
    SessionStatusChanged {
        id: SessionId,
        status: SessionStatus,
    },
    /// Internal: a session actor requests a durable rename. Replies only after
    /// the SessionNamed event is journaled.
    RenameSession {
        id: SessionId,
        name: String,
        reply: oneshot::Sender<Result<(), horsie_actor::JournalError>>,
    },
    /// Internal: publish an already-journaled title to the global live feed.
    PublishSessionTitle { id: SessionId, name: String },
}

/// Events recording the session registry. Persisted.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionSupervisorEvent {
    SessionCreated {
        id: SessionId,
        spec: SessionSpec,
        created_at: u64,
    },
    SessionStatusChanged {
        id: SessionId,
        status: SessionStatus,
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
    pub status: SessionStatus,
    pub created_at: u64,
}

/// Persisted supervisor state — the session registry, purely a function of events.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionSupervisorState {
    pub sessions: BTreeMap<SessionId, SessionRecord>,
}

pub struct SessionSupervisor {
    deps: ServerDeps,
    global_tx: broadcast::Sender<GlobalSessionEvent>,
    children: BTreeMap<SessionId, ActorRef<SessionCommand>>,
}

impl SessionSupervisor {
    pub fn new(deps: ServerDeps, global_tx: broadcast::Sender<GlobalSessionEvent>) -> Self {
        Self {
            deps,
            global_tx,
            children: BTreeMap::new(),
        }
    }

    fn spawn_session(&mut self, ctx: &ActorContext<Self>, id: Uuid, spec: SessionSpec) {
        let child = ctx.spawn(SessionActor::new(
            id,
            spec,
            self.deps.clone(),
            ctx.self_ref(),
        ));
        self.children.insert(id.to_string(), child);
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

    /// A child's mailbox closed outside the normal lifecycle — its own journal
    /// recovery failed and the actor shut down. Mark the session visibly
    /// RecoveryFailed so one corrupt session never takes the server down.
    fn dead_child_effect(&mut self, id: SessionId) -> CommandEffect<SessionSupervisorEvent> {
        self.children.remove(&id);
        let status = SessionStatus::RecoveryFailed {
            reason: "session unavailable: journal recovery failed".to_string(),
        };
        self.publish(&id, &status);
        CommandEffect::persist(vec![SessionSupervisorEvent::SessionStatusChanged {
            id,
            status,
        }])
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
                        status: SessionStatus::Provisioning,
                        created_at,
                    },
                );
            }
            SessionSupervisorEvent::SessionStatusChanged { id, status } => {
                if let Some(rec) = state.sessions.get_mut(&id) {
                    rec.status = status;
                }
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
                let id = Uuid::new_v4();
                self.spawn_session(ctx, id, spec.clone());
                if let Some(child) = self.children.get(&id.to_string()) {
                    let _ = child.tell(SessionCommand::Provision).await;
                }
                let id = id.to_string();
                let _ = reply.send(id.clone());
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
                let _ = reply.send(state.sessions.get(&id).cloned());
                CommandEffect::none()
            }
            SessionSupervisorCommand::UserMessage { id, text, reply } => {
                match self.children.get(&id) {
                    None => {
                        let _ = reply.send(Err(UserMessageError::NotFound));
                        CommandEffect::none()
                    }
                    Some(child) => {
                        // The child replies directly; a failed tell means its
                        // mailbox closed (journal recovery failure) — the
                        // caller's reply was dropped with it (the HTTP ask
                        // surfaces the closed channel), and the session is
                        // marked visibly RecoveryFailed here.
                        if child
                            .tell(SessionCommand::UserMessage { text, reply })
                            .await
                            .is_err()
                        {
                            return self.dead_child_effect(id);
                        }
                        CommandEffect::none()
                    }
                }
            }
            SessionSupervisorCommand::Stop { id, reply } => match self.children.get(&id) {
                None => {
                    let _ = reply.send(Err(format!("no such session: {id}")));
                    CommandEffect::none()
                }
                Some(child) => {
                    let (tx, rx) = oneshot::channel();
                    if child
                        .tell(SessionCommand::Stop { reply: tx })
                        .await
                        .is_err()
                    {
                        let _ = reply.send(Err("session unavailable".to_string()));
                        return self.dead_child_effect(id);
                    }
                    // Forward the ack off the mailbox.
                    tokio::spawn(async move {
                        let _ = rx.await;
                        let _ = reply.send(Ok(()));
                    });
                    CommandEffect::none()
                }
            },
            SessionSupervisorCommand::Delete { id, reply } => {
                if !state.sessions.contains_key(&id) {
                    let _ = reply.send(Err(format!("no such session: {id}")));
                    return CommandEffect::none();
                }
                if let Some(child) = self.children.remove(&id) {
                    let (tx, rx) = oneshot::channel();
                    if child
                        .tell(SessionCommand::Delete { reply: tx })
                        .await
                        .is_ok()
                    {
                        let _ = rx.await;
                    }
                }
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionDeleted { id }])
            }
            SessionSupervisorCommand::Subscribe { id, reply } => {
                match self.children.get(&id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child.tell(SessionCommand::Subscribe { reply: tx }).await;
                        // Forward the child's receiver once it answers, off the mailbox.
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
                match self.children.get(&id) {
                    Some(child) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = child
                            .tell(SessionCommand::History { query, reply: tx })
                            .await;
                        // The child answers off its mailbox (a transient reader may
                        // recover a journal); forward the page when it lands.
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
                match self.children.get(&id) {
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
            SessionSupervisorCommand::Shutdown { reply } => {
                let mut acks = Vec::new();
                for child in self.children.values() {
                    let (tx, rx) = oneshot::channel();
                    if child
                        .tell(SessionCommand::Shutdown { reply: tx })
                        .await
                        .is_ok()
                    {
                        acks.push(rx);
                    }
                }
                self.children.clear();
                for ack in acks {
                    let _ = ack.await;
                }
                let _ = reply.send(());
                CommandEffect::none()
            }
            SessionSupervisorCommand::SessionStatusChanged { id, status } => {
                self.publish(&id, &status);
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionStatusChanged {
                    id,
                    status,
                }])
            }
            SessionSupervisorCommand::RenameSession { id, name, reply } => {
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionNamed { id, name }])
                    .and_ack(reply)
            }
            SessionSupervisorCommand::PublishSessionTitle { id, name } => {
                // A rename command that was superseded while its publish request
                // was queued must not broadcast a stale title.
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

    /// After recovery, re-spawn a [`SessionActor`] for every session in the
    /// registry (deleted ones are already gone from state). No vendor calls
    /// happen here — children reconcile themselves and wake lazily.
    async fn on_recovery_complete(
        &mut self,
        state: &SessionSupervisorState,
        ctx: &mut ActorContext<Self>,
    ) {
        let to_spawn: Vec<(SessionId, SessionSpec)> = state
            .sessions
            .iter()
            .map(|(id, rec)| (id.clone(), rec.spec.clone()))
            .collect();
        for (id, spec) in to_spawn {
            match Uuid::parse_str(&id) {
                Ok(uuid) => self.spawn_session(ctx, uuid, spec),
                Err(e) => {
                    tracing::error!(session_id = %id, error = %e, "unparseable session id; skipping");
                }
            }
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
    use crate::sessions::spec::AgentSettings;
    use crate::vendor::VendorLink;
    use crate::vendor::fake_agent::FakeVendorAgent;
    use horsie_actor::{InMemoryJournal, Journal, spawn_root};
    use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};
    use horsie_models::session::SessionStatusKind;
    use std::collections::HashMap;
    use std::sync::Arc;

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
            capabilities: CapabilitySpec {
                network: NetworkPolicy::Block(BlockNetwork {}),
                grants: vec![],
                unsafe_seatbelt_rules: None,
            },
            vendor: "mock".into(),
            plugins: vec![],
        }
    }

    async fn test_deps(tmp: &tempfile::TempDir) -> ServerDeps {
        let mut vendors: HashMap<String, Arc<VendorLink>> = HashMap::new();
        let mock_agent = FakeVendorAgent::builder("mock")
            .serve_in_process()
            .await
            .expect("fake agent");
        vendors.insert("mock".into(), mock_agent.link());
        ServerDeps {
            provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
            vendors: Arc::new(std::sync::RwLock::new(vendors)),
            state_dir: tmp.path().to_path_buf(),
            github_tokens: None,
            mcp: None,
            plugins: None,
            memory: None,
        }
    }

    #[test]
    fn created_then_status_then_deleted_folds() {
        let s = SessionSupervisor::apply_event(
            SessionSupervisorState::default(),
            SessionSupervisorEvent::SessionCreated {
                id: "s1".into(),
                spec: spec_fixture(),
                created_at: 7,
            },
        );
        assert_eq!(
            s.sessions.get("s1").unwrap().status,
            SessionStatus::Provisioning
        );
        assert_eq!(s.sessions.get("s1").unwrap().created_at, 7);
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionStatusChanged {
                id: "s1".into(),
                status: SessionStatus::Idle,
            },
        );
        assert_eq!(s.sessions.get("s1").unwrap().status, SessionStatus::Idle);
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionDeleted { id: "s1".into() },
        );
        assert!(s.sessions.is_empty());
    }

    #[test]
    fn session_named_event_folds_the_latest_title() {
        let state = SessionSupervisor::apply_event(
            SessionSupervisorState::default(),
            SessionSupervisorEvent::SessionCreated {
                id: "s1".into(),
                spec: SessionSpec {
                    name: None,
                    ..spec_fixture()
                },
                created_at: 7,
            },
        );
        let state = SessionSupervisor::apply_event(
            state,
            SessionSupervisorEvent::SessionNamed {
                id: "s1".into(),
                name: "First title".into(),
            },
        );
        let state = SessionSupervisor::apply_event(
            state,
            SessionSupervisorEvent::SessionNamed {
                id: "s1".into(),
                name: "Latest title".into(),
            },
        );
        assert_eq!(
            state.sessions.get("s1").unwrap().spec.name.as_deref(),
            Some("Latest title")
        );
    }

    #[tokio::test]
    async fn rename_session_replies_after_journaling_the_title() {
        let tmp = tempfile::tempdir().unwrap();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let (gtx, _grx) = broadcast::channel(16);
        let sup = spawn_root(SessionSupervisor::new(test_deps(&tmp).await, gtx), journal);

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

        let persisted = sup
            .ask(|reply| SessionSupervisorCommand::RenameSession {
                id: id.clone(),
                name: "Investigate login failure".into(),
                reply,
            })
            .await
            .unwrap();
        assert!(persisted.is_ok());

        let rec = sup
            .ask(|reply| SessionSupervisorCommand::Get {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.spec.name.as_deref(), Some("Investigate login failure"));
    }

    #[tokio::test]
    async fn publish_session_title_emits_a_dedicated_title_event() {
        let tmp = tempfile::tempdir().unwrap();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let (gtx, mut grx) = broadcast::channel(16);
        let sup = spawn_root(SessionSupervisor::new(test_deps(&tmp).await, gtx), journal);

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
            let frame = tokio::time::timeout(std::time::Duration::from_secs(2), grx.recv())
                .await
                .unwrap()
                .unwrap();
            match frame {
                horsie_models::session::GlobalSessionEvent::TitleChanged(event) => {
                    assert_eq!(event.session_id, id);
                    assert_eq!(event.name, "Investigate login failure");
                    break;
                }
                horsie_models::session::GlobalSessionEvent::StatusChanged(_) => {}
            }
        }
    }

    #[tokio::test]
    async fn create_list_get_delete_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let (gtx, mut grx) = broadcast::channel(16);
        let sup = spawn_root(SessionSupervisor::new(test_deps(&tmp).await, gtx), journal);

        let id = sup
            .ask(|reply| SessionSupervisorCommand::Create {
                spec: spec_fixture(),
                created_at: 1,
                reply,
            })
            .await
            .unwrap();

        let list = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, id);

        let rec = sup
            .ask(|reply| SessionSupervisorCommand::Get {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap();
        assert!(rec.is_some());

        // Creation published a Provisioning frame on the global stream.
        match grx.recv().await.unwrap() {
            horsie_models::session::GlobalSessionEvent::StatusChanged(frame) => {
                assert_eq!(frame.session_id, id);
                assert_eq!(frame.status, SessionStatusKind::Provisioning);
            }
            horsie_models::session::GlobalSessionEvent::TitleChanged(_) => {
                panic!("creation must not publish a title frame")
            }
        }

        let res = sup
            .ask(|reply| SessionSupervisorCommand::Delete {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap();
        assert!(res.is_ok());
        let list = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn unknown_session_routes_to_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let (gtx, _) = broadcast::channel(16);
        let sup = spawn_root(SessionSupervisor::new(test_deps(&tmp).await, gtx), journal);
        let res = sup
            .ask(|reply| SessionSupervisorCommand::UserMessage {
                id: "missing".into(),
                text: "hi".into(),
                reply,
            })
            .await
            .unwrap();
        assert!(matches!(res, Err(UserMessageError::NotFound)));
        let sub = sup
            .ask(|reply| SessionSupervisorCommand::Subscribe {
                id: "missing".into(),
                reply,
            })
            .await
            .unwrap();
        assert!(sub.is_none());
    }

    #[tokio::test]
    async fn registry_recovers_across_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let (gtx, _) = broadcast::channel(16);
        let sup = spawn_root(
            SessionSupervisor::new(test_deps(&tmp).await, gtx.clone()),
            journal.clone(),
        );
        let id = sup
            .ask(|reply| SessionSupervisorCommand::Create {
                spec: spec_fixture(),
                created_at: 9,
                reply,
            })
            .await
            .unwrap();
        sup.ask(|reply| SessionSupervisorCommand::Shutdown { reply })
            .await
            .unwrap();

        // Second incarnation on the same journal recovers the registry and
        // re-spawns the child (routable, no NotFound).
        let sup2 = spawn_root(SessionSupervisor::new(test_deps(&tmp).await, gtx), journal);
        let rec = sup2
            .ask(|reply| SessionSupervisorCommand::Get {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap();
        assert!(rec.is_some());
        let sub = sup2
            .ask(|reply| SessionSupervisorCommand::Subscribe { id, reply })
            .await
            .unwrap();
        assert!(sub.is_some());
    }
}
