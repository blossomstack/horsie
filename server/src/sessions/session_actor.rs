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

use crate::runtime_manager::{RuntimeClientProvider, RuntimeError};
use crate::sessions::ask_tool::{ASK_USER_TOOL, AskUserToolbox};
use crate::sessions::events::{QuietEventSink, SessionEventSink};
use crate::sessions::spawn_tool::SubAgentToolbox;
use crate::sessions::spec::{AgentSettings, ServerDeps, SessionSpec, SessionStatus};
use crate::sessions::subagents::{
    INTERRUPTED_ERROR, MAX_SUBAGENT_DEPTH, SubAgentParent, SubAgentTree,
};
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::title_tool::{SessionTitleToolbox, normalize_session_title};
use crate::sessions::{SessionFrame, UserMessageError};
use async_trait::async_trait;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_agentcore::{LlmProvider, Toolbox};
use horsie_runtime_client::RuntimeClient;
use horsie_workflow::{
    AgentActor, AgentCommand, AgentHistoryPage, AgentOutcome, AgentOutcomeSink, AgentParams,
    AgentRunDef, AgentRuntimeContext, AgentUsageSnapshot, ContextError, ContextProvider, Contexts,
    DefaultToolboxFactory, HistoryQuery, SharedContext, ToolboxFactory, UsageTotal,
    compose_system_prompt, scan_workspace,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

/// Capacity of a session's live frame broadcast. Slow subscribers see `lagged`
/// drops and catch up from the journal.
const FRAME_BROADCAST_CAPACITY: usize = 256;

/// The agent id a session's primary agent reports usage under.
const MAIN_AGENT_ID: &str = "main";

/// How long a cancel waits for the run to actually finish before giving up.
/// Cancellation is prompt (milliseconds); this is a backstop so a wedged run
/// can never hold the mailbox — and with it the Stop button — hostage.
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Separator between messages merged into one turn. Blank line, because the
/// model is reading them as one message and paragraph breaks are what a human
/// would have typed.
const MERGE_SEPARATOR: &str = "\n\n";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn emit_progress(frames: &broadcast::Sender<SessionFrame>, stage: &str, detail: Option<String>) {
    let _ = frames.send(SessionFrame::Progression {
        stage: stage.to_string(),
        detail,
        at_ms: now_ms(),
    });
}

/// The baseline system prompt given to every session agent.
const SESSION_AGENT_PROMPT: &str = include_str!("system_prompt.md");

/// Commands accepted by a [`SessionActor`].
pub enum SessionCommand {
    /// A user message. Always accepted: it is queued durably and answered by
    /// the next turn, so there is no rejection path and no `409`.
    UserMessage {
        text: String,
        reply: oneshot::Sender<Result<String, UserMessageError>>,
    },
    /// Cancel the turn in flight. Queued messages are *not* discarded — stop
    /// means "not this turn", not "throw away what I asked for".
    Stop { reply: oneshot::Sender<()> },
    /// Delete: cancel, tell the vendor, and stop.
    Delete { reply: oneshot::Sender<()> },
    /// Hand back a live frame subscriber for the SSE stream.
    Subscribe {
        reply: oneshot::Sender<broadcast::Receiver<SessionFrame>>,
    },
    /// Read a window of conversation history from one of the session's
    /// agents: `agent_id` absent or `"main"` for the primary agent, otherwise
    /// a subagent id. `None` answers "no such agent".
    History {
        agent_id: Option<String>,
        query: HistoryQuery,
        reply: oneshot::Sender<Option<AgentHistoryPage>>,
    },
    /// Read this session's aggregated usage.
    UsageStats {
        reply: oneshot::Sender<SessionUsageStats>,
    },
    /// The supervisor wants to unload this session. Answers `false` if a run
    /// started in the meantime, in which case nothing has changed and the idle
    /// clock simply restarts.
    PrepareOffload { reply: oneshot::Sender<bool> },
    /// Internal: an agent reported its terminal outcome.
    AgentOutcome(AgentOutcome),
    /// Internal: post-recovery reconciliation of a turn the process died in.
    ReconcileInterrupted,
    /// Set the session title from the built-in title tool.
    SetSessionTitle {
        title: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// The `spawn_agent` tool: start a subagent under `caller`.
    SpawnSubAgent {
        caller: SubAgentParent,
        label: String,
        task: String,
        reply: oneshot::Sender<Result<Uuid, String>>,
    },
    /// The `subagent_status` tool: one node, or the caller's whole subtree.
    SubAgentStatus {
        caller: SubAgentParent,
        id: Option<Uuid>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Read the whole subagent tree (backs `GET /api/sessions/:id/subagents`).
    SubAgentTree {
        reply: oneshot::Sender<Vec<(Uuid, crate::sessions::subagents::SubAgentRecord)>>,
    },
    /// Internal: post-recovery reconciliation of subagents the process died
    /// under (tree nodes still `Running`). Their runs are over; the parents
    /// are owed the failure like any other terminal result.
    ReconcileSubAgents,
    /// Internal: the spawn's `SubAgentSpawned` write came back — only now
    /// does the child actor exist (persist-then-spawn). A failed write spawns
    /// nothing and the tool gets the error.
    FinishSpawnSubAgent {
        id: Uuid,
        caller: SubAgentParent,
        label: String,
        task: String,
        reply: oneshot::Sender<Result<Uuid, String>>,
        persisted: Result<(), horsie_actor::JournalError>,
    },
}

/// Events recording a session's lifecycle. Persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionDomainEvent {
    /// A user message was accepted. Durable *before* anything is done with it,
    /// so an accepted message survives a crash and is still owed an answer.
    MessageQueued {
        id: String,
        text: String,
        at_ms: u64,
    },
    /// A turn started, consuming these queued messages — and, if the session
    /// was parked on an ask, answering it. One event so a crash anywhere in
    /// the window replays to the same place.
    TurnBegan {
        consumed: Vec<String>,
        answering: Option<String>,
    },
    /// The agent asked the user something and is parked on it.
    AskRecorded {
        tool_call_id: Option<String>,
        question: String,
    },
    TurnEnded,
    TurnFailed {
        error: String,
    },
    /// The user cancelled the turn. Distinct from `TurnEnded` only in intent;
    /// both are turn boundaries, and both let the inbox drain.
    TurnStopped,
    /// Recovery found a turn that the process died in. Recorded rather than
    /// inferred, so the transition is in the log like every other one.
    TurnInterrupted,
    /// Terminal: this session can never run again.
    SessionFailed {
        reason: String,
    },
    /// One agent's cumulative usage after a completed run. Durable here so the
    /// session-level total never requires waking an idle agent.
    UsageRecorded {
        agent_id: String,
        usage_total: UsageTotal,
    },
    /// A subagent was spawned by `parent` (the main agent or another
    /// subagent). Persisted before the child actor exists — a crash between
    /// the two replays as a node that recovery reconciles to failed.
    SubAgentSpawned {
        id: Uuid,
        parent: SubAgentParent,
        label: String,
        task: String,
        depth: u32,
    },
    /// A terminal node started another run, woken to consume child results.
    SubAgentRunning {
        id: Uuid,
    },
    SubAgentCompleted {
        id: Uuid,
        output: String,
    },
    SubAgentFailed {
        id: Uuid,
        error: String,
    },
    /// The node's latest terminal result was sent to its parent. Persisted in
    /// the same effect as the send, so a reload neither re- nor never-sends.
    SubAgentNotified {
        id: Uuid,
    },
}

/// One accepted-but-undelivered user message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxMessage {
    pub id: String,
    pub text: String,
    pub at_ms: u64,
}

/// Persisted session state — purely a function of the event log.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
    pub status: SessionStatus,
    /// The ask awaiting an answer (status `AwaitingInput`).
    pub pending_ask: Option<String>,
    pub pending_question: Option<String>,
    /// Accepted user messages not yet delivered to a turn. The client shows
    /// these as unread; they go in with whatever turn starts next.
    #[serde(default)]
    pub inbox: Vec<InboxMessage>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub agent_usage: HashMap<String, UsageTotal>,
    /// The subagent tree — which agent spawned which, and what became of it.
    #[serde(default)]
    pub subagents: SubAgentTree,
}

/// One agent's own usage/context-size snapshot, labeled with the model it ran.
#[derive(Debug, Clone)]
pub struct AgentUsageEntry {
    pub model: String,
    pub snapshot: AgentUsageSnapshot,
}

/// A session's aggregated usage.
#[derive(Debug, Clone)]
pub struct SessionUsageStats {
    pub session_total: UsageTotal,
    pub main_agent: AgentUsageEntry,
}

/// Longest auto-derived session title, in characters.
const TITLE_MAX_CHARS: usize = crate::sessions::title_tool::SESSION_TITLE_MAX_CHARS;

/// A short title derived from a user's first message.
fn derive_title(text: &str) -> Option<String> {
    let first_line = text.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return None;
    }
    if first_line.chars().count() <= TITLE_MAX_CHARS {
        return Some(first_line.to_string());
    }
    let truncated: String = first_line.chars().take(TITLE_MAX_CHARS).collect();
    Some(format!("{}…", truncated.trim_end()))
}

pub struct SessionActor {
    id: Uuid,
    spec: SessionSpec,
    deps: ServerDeps,
    parent: ActorRef<SessionSupervisorCommand>,
    frames: broadcast::Sender<SessionFrame>,
    /// The session's primary agent, resident for as long as this actor is
    /// loaded. Spawned once, on recovery; `None` only in the instant before.
    main_agent: Option<ActorRef<AgentCommand>>,
    /// The subagents this session hosts, keyed by their node id in the
    /// persisted tree. Resident for the session's loaded lifetime, exactly
    /// like the main agent — the reason the session and agent stayed two
    /// actors.
    sub_agents: HashMap<Uuid, ActorRef<AgentCommand>>,
    /// The main agent's context provider, kept so [`Self::cancel_run`] can
    /// reach the runtime client the run already acquired instead of asking
    /// the manager for a fresh one.
    context_provider: Option<Arc<SessionContextProvider>>,
}

impl SessionActor {
    pub fn new(
        id: Uuid,
        spec: SessionSpec,
        deps: ServerDeps,
        parent: ActorRef<SessionSupervisorCommand>,
    ) -> Self {
        let (frames, _) = broadcast::channel(FRAME_BROADCAST_CAPACITY);
        Self {
            id,
            spec,
            deps,
            parent,
            frames,
            main_agent: None,
            sub_agents: HashMap::new(),
            context_provider: None,
        }
    }

    /// The journal identity of a session: kind `"session"`, id = the uuid.
    pub fn persistence_id_for(session_id: Uuid) -> PersistenceId {
        PersistenceId::new("session", session_id.to_string())
    }

    /// Report a status transition to the supervisor's cache and the live stream.
    async fn report(&self, status: SessionStatus) {
        let _ = self.frames.send(SessionFrame::Status {
            status: status.clone(),
        });
        let _ = self
            .parent
            .tell(SessionSupervisorCommand::SessionStatusChanged {
                id: self.id.to_string(),
                status,
            })
            .await;
    }

    /// Publish the queue as it stands once `events` fold onto `state`.
    ///
    /// One frame per command, carrying the whole queue — never an intermediate.
    /// A message that is accepted and immediately drained therefore publishes
    /// an empty inbox once, rather than "queued" followed by "gone", which a
    /// client would render as a flicker. A no-op when nothing touched the queue,
    /// so callers may call it unconditionally.
    fn publish_inbox(&self, state: &SessionState, events: &[SessionDomainEvent]) {
        if !events.iter().any(|e| {
            matches!(
                e,
                SessionDomainEvent::MessageQueued { .. } | SessionDomainEvent::TurnBegan { .. }
            )
        }) {
            return;
        }
        let next = events
            .iter()
            .cloned()
            .fold(state.clone(), Self::apply_event);
        let _ = self
            .frames
            .send(SessionFrame::InboxChanged { queued: next.inbox });
    }

    /// Persist a session title through the supervisor, then publish it.
    async fn rename_session(&mut self, title: String) -> Result<String, String> {
        let id = self.id.to_string();
        let persisted = self
            .parent
            .ask(|reply| SessionSupervisorCommand::RenameSession {
                id: id.clone(),
                name: title.clone(),
                reply,
            })
            .await
            .map_err(|e| format!("session supervisor unavailable: {e}"))?;
        persisted.map_err(|e| format!("persist session title: {e}"))?;

        self.spec.name = Some(title.clone());
        let _ = self
            .parent
            .tell(SessionSupervisorCommand::PublishSessionTitle {
                id,
                name: title.clone(),
            })
            .await;
        Ok(title)
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
            session_id: self.id,
            kind: SessionAgentKind::Main,
            session: ctx.self_ref(),
            frames: self.frames.clone(),
            last_client: Mutex::new(None),
        });
        self.context_provider = Some(context_provider.clone());
        let mut params = AgentParams::from_def(&session_run_def(&self.spec.agent));
        params.interactive = true;
        params.optional_handoff_tool = Some(ASK_USER_TOOL.to_string());
        params.thinking_effort = self
            .spec
            .agent
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let agent_ctx = AgentRuntimeContext {
            context_provider,
            event_sink: Arc::new(SessionEventSink {
                frames: self.frames.clone(),
            }),
            parent: Arc::new(SessionParent {
                target: ctx.self_ref(),
            }),
            session_id: self.id,
        };
        self.main_agent = Some(ctx.spawn(AgentActor::new(agent_ctx, params)));
    }

    /// Spawn a resident subagent actor — journal replay only; the caller
    /// decides whether a run starts (spawn) or not (recovery).
    fn spawn_sub_agent_actor(
        &mut self,
        ctx: &ActorContext<Self>,
        id: Uuid,
    ) -> ActorRef<AgentCommand> {
        let context_provider = Arc::new(SessionContextProvider {
            runtimes: self
                .deps
                .runtimes
                .provider(self.id.to_string(), self.spec.vendor.clone()),
            registry: self.deps.provider_registry.clone(),
            mcp: self.deps.mcp.clone(),
            memory: self.deps.memory.clone(),
            settings: self.spec.agent.clone(),
            session_id: self.id,
            kind: SessionAgentKind::Sub(id),
            session: ctx.self_ref(),
            frames: self.frames.clone(),
            last_client: Mutex::new(None),
        });
        let mut params = AgentParams::from_def(&session_run_def(&self.spec.agent));
        params.interactive = true;
        // No handoff tool: a subagent ends its turn with plain text, which
        // becomes the output its parent is notified with.
        params.thinking_effort = self
            .spec
            .agent
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let agent_ctx = AgentRuntimeContext {
            context_provider,
            event_sink: Arc::new(QuietEventSink),
            parent: Arc::new(SessionParent {
                target: ctx.self_ref(),
            }),
            session_id: id,
        };
        let actor = ctx.spawn(AgentActor::new(agent_ctx, params));
        self.sub_agents.insert(id, actor.clone());
        actor
    }

    fn agent(&self) -> Option<&ActorRef<AgentCommand>> {
        self.main_agent.as_ref()
    }

    /// Aggregated usage. Totals come from this session's own durable record;
    /// only the live context size is asked of the agent.
    async fn read_usage(&self, state: &SessionState) -> SessionUsageStats {
        let snapshot = match self.agent() {
            Some(agent) => agent
                .ask(|reply| AgentCommand::GetUsage { reply })
                .await
                .unwrap_or_default(),
            None => AgentUsageSnapshot::default(),
        };
        let main_usage_total = state
            .agent_usage
            .get(MAIN_AGENT_ID)
            .copied()
            .unwrap_or_default();
        let session_total = state
            .agent_usage
            .values()
            .fold(UsageTotal::default(), |acc, u| acc.combine(u));
        SessionUsageStats {
            session_total,
            main_agent: AgentUsageEntry {
                model: self.spec.agent.model.clone(),
                snapshot: AgentUsageSnapshot {
                    usage_total: main_usage_total,
                    last_turn_usage: snapshot.last_turn_usage,
                    context_tokens: snapshot.context_tokens,
                },
            },
        }
    }

    /// Start a turn from everything in the inbox, if there is anything and no
    /// run is in flight. The single place a turn ever begins.
    ///
    /// Called only at turn boundaries (a message arriving while idle, a turn
    /// ending, a stop) — never on load, which is what keeps opening a session
    /// free of side effects.
    async fn drain(&mut self, state: &SessionState) -> Vec<SessionDomainEvent> {
        // Owed subagent results ride every turn the main agent starts; with an
        // empty inbox they can also *start* one, but only from Idle — never
        // answering a pending ask, never chasing a failure.
        let owed = state.subagents.owed_for(SubAgentParent::Main);
        if state.inbox.is_empty() && (owed.is_empty() || state.status != SessionStatus::Idle) {
            return Vec::new();
        }
        if matches!(
            state.status,
            SessionStatus::Running | SessionStatus::Unrecoverable { .. }
        ) {
            return Vec::new();
        }
        let consumed: Vec<String> = state.inbox.iter().map(|m| m.id.clone()).collect();
        let answering = state.pending_ask.clone();
        // One user message, not several: Anthropic requires alternating roles,
        // so consecutive user turns are not portable. Provenance survives in
        // the `MessageQueued` events.
        let mut parts: Vec<&str> = state.inbox.iter().map(|m| m.text.as_str()).collect();
        parts.extend(owed.iter().map(|(_, text)| text.as_str()));
        let merged = parts.join(MERGE_SEPARATOR);

        if let Some(agent) = self.agent() {
            let cmd = match &answering {
                Some(tool_call_id) => AgentCommand::InjectToolResult {
                    tool_call_id: tool_call_id.clone(),
                    content: merged,
                },
                None => AgentCommand::Run { input: merged },
            };
            let _ = agent.tell(cmd).await;
        }
        self.report(SessionStatus::Running).await;
        let mut events = vec![SessionDomainEvent::TurnBegan {
            consumed,
            answering,
        }];
        events.extend(
            owed.iter()
                .map(|(child, _)| SessionDomainEvent::SubAgentNotified { id: *child }),
        );
        events
    }

    /// Wake every idle subagent parent whose children have results it has not
    /// been sent. The main agent is deliberately excluded: its owed results
    /// merge into its next turn inside `drain`.
    async fn flush_owed(&mut self, state: &SessionState) -> Vec<SessionDomainEvent> {
        let mut events = Vec::new();
        for (parent_id, owed) in state.subagents.owed_by_sub_parent() {
            if state.subagents.is_running(&parent_id) {
                continue;
            }
            let Some(agent) = self.sub_agents.get(&parent_id).cloned() else {
                continue;
            };
            let input = owed
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>()
                .join(MERGE_SEPARATOR);
            if agent.tell(AgentCommand::Run { input }).await.is_err() {
                continue;
            }
            events.push(SessionDomainEvent::SubAgentRunning { id: parent_id });
            events.extend(
                owed.iter()
                    .map(|(child, _)| SessionDomainEvent::SubAgentNotified { id: *child }),
            );
        }
        events
    }

    async fn on_user_message(
        &mut self,
        state: &SessionState,
        text: String,
        reply: oneshot::Sender<Result<String, UserMessageError>>,
    ) -> CommandEffect<SessionDomainEvent> {
        if let SessionStatus::Unrecoverable { reason } = &state.status {
            let _ = reply.send(Err(UserMessageError::Unrecoverable(reason.clone())));
            return CommandEffect::none();
        }
        // An unnamed session is titled from its first message, once.
        if self.spec.name.is_none()
            && let Some(title) = derive_title(&text)
            && let Err(error) = self.rename_session(title).await
        {
            tracing::warn!(session = %self.id, error, "failed to persist fallback session title");
        }

        let queued = SessionDomainEvent::MessageQueued {
            id: Uuid::new_v4().to_string(),
            text,
            at_ms: now_ms(),
        };
        let SessionDomainEvent::MessageQueued { id, .. } = &queued else {
            unreachable!("just constructed")
        };
        let message_id = id.clone();
        let _ = reply.send(Ok(message_id));

        // Fold the queue locally so the drain sees the message it is about to
        // persist — same fold the runtime will apply, just one step early.
        let next = Self::apply_event(state.clone(), queued.clone());
        let mut events = vec![queued];
        events.extend(self.drain(&next).await);
        self.publish_inbox(state, &events);
        CommandEffect::persist(events)
    }

    async fn on_agent_outcome(
        &mut self,
        state: &SessionState,
        outcome: AgentOutcome,
    ) -> CommandEffect<SessionDomainEvent> {
        let outcome_session = match &outcome {
            AgentOutcome::Concluded { session_id, .. }
            | AgentOutcome::Asked { session_id, .. }
            | AgentOutcome::Parked { session_id }
            | AgentOutcome::Failed { session_id, .. }
            | AgentOutcome::UsageRecorded { session_id, .. } => *session_id,
        };
        if outcome_session != self.id {
            return self
                .on_sub_agent_outcome(state, outcome_session, outcome)
                .await;
        }
        // Usage is always recorded: the tokens were spent whatever became of
        // the turn that spent them.
        if let AgentOutcome::UsageRecorded { usage_total, .. } = outcome {
            return CommandEffect::persist(vec![SessionDomainEvent::UsageRecorded {
                agent_id: MAIN_AGENT_ID.to_string(),
                usage_total,
            }]);
        }
        let (mut events, drained) = match outcome {
            AgentOutcome::UsageRecorded { .. } => unreachable!("handled above"),
            AgentOutcome::Concluded { .. } => {
                self.report(SessionStatus::Idle).await;
                (vec![SessionDomainEvent::TurnEnded], true)
            }
            AgentOutcome::Asked {
                tool_call_id,
                question,
                ..
            } => {
                self.report(SessionStatus::AwaitingInput).await;
                (
                    vec![SessionDomainEvent::AskRecorded {
                        tool_call_id,
                        question,
                    }],
                    // An ask is a turn boundary too: a message queued while the
                    // agent was working becomes the answer.
                    true,
                )
            }
            AgentOutcome::Failed {
                error, terminal, ..
            } => {
                let _ = self.frames.send(SessionFrame::Error {
                    message: error.clone(),
                });
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
                        vec![SessionDomainEvent::SessionFailed { reason: error }],
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
                    (vec![SessionDomainEvent::TurnFailed { error }], false)
                }
            }
            AgentOutcome::Parked { .. } => {
                let error = "agent parked; timers are not supported in sessions".to_string();
                let _ = self.frames.send(SessionFrame::Error {
                    message: error.clone(),
                });
                self.report(SessionStatus::Failed {
                    reason: error.clone(),
                })
                .await;
                (vec![SessionDomainEvent::TurnFailed { error }], false)
            }
        };
        if drained {
            let mut next = state.clone();
            for e in &events {
                next = Self::apply_event(next, e.clone());
            }
            events.extend(self.drain(&next).await);
        }
        self.publish_inbox(state, &events);
        CommandEffect::persist(events)
    }

    /// A subagent's outcome: record it in the tree, then deliver every result
    /// owed to idle parents — wakes for subagent parents, a turn (via
    /// `drain`) when the main agent is owed and idle.
    async fn on_sub_agent_outcome(
        &mut self,
        state: &SessionState,
        id: Uuid,
        outcome: AgentOutcome,
    ) -> CommandEffect<SessionDomainEvent> {
        if let AgentOutcome::UsageRecorded { usage_total, .. } = outcome {
            return CommandEffect::persist(vec![SessionDomainEvent::UsageRecorded {
                agent_id: id.to_string(),
                usage_total,
            }]);
        }
        let Some(rec) = state.subagents.get(&id).cloned() else {
            tracing::warn!(subagent = %id, "outcome from an unknown subagent; ignored");
            return CommandEffect::none();
        };
        let terminal = match outcome {
            AgentOutcome::Concluded { output, .. } => {
                let text = output
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| output.to_string());
                emit_progress(
                    &self.frames,
                    "subagent_completed",
                    Some(format!("\"{}\" ({id})", rec.label)),
                );
                SessionDomainEvent::SubAgentCompleted { id, output: text }
            }
            AgentOutcome::Failed { error, .. } => {
                emit_progress(
                    &self.frames,
                    "subagent_failed",
                    Some(format!("\"{}\" ({id})", rec.label)),
                );
                SessionDomainEvent::SubAgentFailed { id, error }
            }
            // Defensive: a subagent has no ask or timer tools, so neither
            // outcome should ever occur.
            AgentOutcome::Asked { .. } => SessionDomainEvent::SubAgentFailed {
                id,
                error: "subagent asked the user; not supported".to_string(),
            },
            AgentOutcome::Parked { .. } => SessionDomainEvent::SubAgentFailed {
                id,
                error: "subagent parked; timers are not supported in sessions".to_string(),
            },
            AgentOutcome::UsageRecorded { .. } => unreachable!("handled above"),
        };
        let mut events = vec![terminal];
        let mut next = events
            .iter()
            .cloned()
            .fold(state.clone(), Self::apply_event);
        let flushed = self.flush_owed(&next).await;
        next = flushed.iter().cloned().fold(next, Self::apply_event);
        events.extend(flushed);
        events.extend(self.drain(&next).await);
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
        for agent in self
            .main_agent
            .take()
            .into_iter()
            .chain(self.sub_agents.drain().map(|(_, a)| a))
        {
            // Cancel first: a stopped mailbox makes the run task's next persist
            // fail, but an in-flight tool call would run to completion first.
            let _ = agent.tell(AgentCommand::Cancel { ack: None }).await;
            let _ = agent.tell(AgentCommand::Shutdown).await;
        }
    }
}

/// Adapts the session's mailbox to the [`AgentOutcomeSink`] its agents report
/// to. No generation tag: the agent is resident and fences its own stale runs
/// by `run_id`, so every outcome that arrives here is one the session asked for.
struct SessionParent {
    target: ActorRef<SessionCommand>,
}

#[async_trait]
impl AgentOutcomeSink for SessionParent {
    async fn deliver(&self, outcome: AgentOutcome) {
        let _ = self
            .target
            .tell(SessionCommand::AgentOutcome(outcome))
            .await;
    }
}

/// The interactive session's `AgentRunDef`.
fn session_run_def(settings: &AgentSettings) -> AgentRunDef {
    AgentRunDef {
        system_prompt: None,
        output_schema: None,
        allow_ask_user: false,
        allow_timers: None,
        max_iterations: settings.max_iterations,
        max_retries: Some(settings.max_retries),
        allowed_tools: settings.allowed_tools.clone(),
    }
}

/// Wrap `base` with the memory tools and render the memory index.
async fn build_memory_layer(
    base: Arc<dyn Toolbox>,
    memory: Option<Arc<crate::memory::MemoryService>>,
    settings: &AgentSettings,
) -> Result<(Arc<dyn Toolbox>, String), String> {
    let spaces = &settings.memory_spaces;
    if spaces.is_empty() {
        return Ok((base, String::new()));
    }
    let Some(service) = memory else {
        tracing::warn!("session names memory spaces but no memory service is configured; ignoring");
        return Ok((base, String::new()));
    };
    let rows = service.memories_in(spaces).await?;
    let index = crate::memory::render_index(&rows, spaces);
    let toolbox: Arc<dyn Toolbox> = Arc::new(crate::memory::MemoryToolbox::new(
        base,
        service,
        spaces.clone(),
    ));
    Ok((toolbox, index))
}

/// Which of a session's agents a [`SessionContextProvider`] serves. The kind
/// decides the toolbox layers (session-metadata tools are main-only) and
/// whether preparation progress is broadcast (main-only — subagents are
/// quiet).
#[derive(Clone, Copy)]
enum SessionAgentKind {
    Main,
    Sub(Uuid),
}

/// Appended to a subagent's system prompt: its place in the tree and how its
/// result travels. Deliberately short — the tools carry their own docs.
const SUBAGENT_PROMPT_SUFFIX: &str = "\n\n# Subagent role\n\
You are a subagent, spawned to work on one task. Your final message is your report: \
it is delivered to the agent that spawned you — make it self-contained. You may spawn \
your own subagents with spawn_agent and check on them with subagent_status. You cannot \
ask the user or rename the session; if you are blocked, report that instead.";

/// Per-run context for a session's agent, resolved on the run's own task.
///
/// It asks the [`RuntimeClientProvider`] for a client each run rather than
/// holding one: that is what lets the agent be resident across a hibernate and
/// resume without knowing either happened.
struct SessionContextProvider {
    runtimes: RuntimeClientProvider,
    registry: crate::sessions::spec::SharedProviderRegistry,
    mcp: Option<Arc<crate::mcp::McpService>>,
    memory: Option<Arc<crate::memory::MemoryService>>,
    settings: AgentSettings,
    session_id: Uuid,
    kind: SessionAgentKind,
    /// The owning session's mailbox — routes the server-owned tools.
    session: ActorRef<SessionCommand>,
    frames: broadcast::Sender<SessionFrame>,
    /// The client the most recent `provide()` resolved. Cheap to keep — cloning
    /// shares the same in-flight-call tracking — and it is what lets
    /// [`SessionActor::cancel_run`] cancel without a fresh vendor round-trip.
    last_client: Mutex<Option<RuntimeClient>>,
}

impl SessionContextProvider {
    fn llm_provider(&self) -> Result<Arc<dyn LlmProvider>, String> {
        let reg = self
            .registry
            .read()
            .map_err(|_| "provider registry lock poisoned".to_string())?;
        reg.get(&self.settings.model)
            .cloned()
            .ok_or_else(|| format!("no provider registered for model '{}'", self.settings.model))
    }

    /// The client the run currently in flight already acquired, if any.
    fn cached_client(&self) -> Option<RuntimeClient> {
        self.last_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ContextProvider for SessionContextProvider {
    async fn provide(&self) -> Result<Contexts, ContextError> {
        let settings = &self.settings;
        let provider = self.llm_provider()?;
        let def = session_run_def(settings);
        let use_plugins = settings.use_plugins.unwrap_or(true);
        // Preparation progress is main-only: subagents are quiet by design.
        let broadcast = matches!(self.kind, SessionAgentKind::Main);

        if broadcast {
            emit_progress(&self.frames, "acquiring_runtime", None);
        }
        let runtime_client = self.runtimes.get().await.map_err(|e| match e {
            // The one failure the session can never retry: the vendor is alive
            // and says the runtime is gone. A vendor that is merely offline
            // (`Unavailable`) says nothing about the runtime's existence.
            RuntimeError::Gone(m) => ContextError::terminal(m),
            other @ (RuntimeError::Unavailable(_) | RuntimeError::Provision(_)) => {
                ContextError::retryable(other.to_string())
            }
        })?;
        *self
            .last_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(runtime_client.clone());

        if broadcast {
            emit_progress(&self.frames, "scanning_workspace", None);
        }
        let (ws, shared_scan) = scan_workspace(&runtime_client, None, use_plugins).await;
        let shared = if use_plugins {
            let bootstrap = match runtime_client.run_session_start().await {
                Ok(context) if !context.trim().is_empty() => Some(context),
                Ok(_) | Err(_) => None,
            };
            Some(SharedContext {
                skills: Arc::new(shared_scan.skills),
                root: shared_scan.root,
                bootstrap,
            })
        } else {
            None
        };
        let mcp: Vec<Arc<dyn Toolbox>> = if settings.mcp_servers.is_empty() {
            Vec::new()
        } else if let Some(mcp_svc) = self.mcp.as_ref() {
            if broadcast {
                emit_progress(&self.frames, "connecting_tools", None);
            }
            mcp_svc
                .toolboxes_for(&settings.mcp_servers)
                .await
                .map_err(|e| format!("build MCP toolboxes: {e}"))?
        } else {
            tracing::warn!(
                session = %self.session_id,
                "session names MCP servers but no MCP service is configured; ignoring"
            );
            Vec::new()
        };
        let base: Arc<dyn Toolbox> = DefaultToolboxFactory.for_agent(
            &def,
            runtime_client.clone(),
            ws.names(),
            use_plugins,
            mcp,
        );
        let (with_memory, memory_index) =
            build_memory_layer(base, self.memory.clone(), settings).await?;
        let caller = match self.kind {
            SessionAgentKind::Main => SubAgentParent::Main,
            SessionAgentKind::Sub(id) => SubAgentParent::SubAgent(id),
        };
        let with_spawn: Arc<dyn Toolbox> = Arc::new(SubAgentToolbox::new(
            with_memory,
            self.session.clone(),
            caller,
        ));
        let toolbox: Arc<dyn Toolbox> = match self.kind {
            SessionAgentKind::Main => {
                let inner: Arc<dyn Toolbox> = Arc::new(AskUserToolbox::new(with_spawn));
                Arc::new(SessionTitleToolbox::new(inner, self.session.clone()))
            }
            SessionAgentKind::Sub(_) => with_spawn,
        };
        let system_prompt = compose_system_prompt(Some(SESSION_AGENT_PROMPT), &ws, shared.as_ref());
        let system_prompt = match self.kind {
            SessionAgentKind::Main => system_prompt,
            SessionAgentKind::Sub(_) => Some(match system_prompt {
                Some(p) => format!("{p}{SUBAGENT_PROMPT_SUFFIX}"),
                None => SUBAGENT_PROMPT_SUFFIX.trim_start().to_string(),
            }),
        };
        let system_prompt = match (system_prompt, memory_index.is_empty()) {
            (Some(p), false) => Some(format!("{p}\n\n{memory_index}")),
            (Some(p), true) => Some(p),
            (None, false) => Some(memory_index),
            (None, true) => None,
        };
        if broadcast {
            emit_progress(&self.frames, "ready", None);
        }
        Ok(Contexts {
            provider,
            toolbox,
            system_prompt,
        })
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
            SessionDomainEvent::MessageQueued { id, text, at_ms } => {
                state.inbox.push(InboxMessage { id, text, at_ms });
            }
            SessionDomainEvent::TurnBegan {
                consumed,
                answering,
            } => {
                state.status = SessionStatus::Running;
                state.inbox.retain(|m| !consumed.contains(&m.id));
                if answering.is_some() {
                    state.pending_ask = None;
                    state.pending_question = None;
                }
                // The previous turn's failure is history once a new turn is
                // under way; leaving it set makes the detail endpoint report a
                // stale error for the rest of the session's life.
                state.last_error = None;
            }
            SessionDomainEvent::AskRecorded {
                tool_call_id,
                question,
            } => {
                state.status = SessionStatus::AwaitingInput;
                state.pending_ask = tool_call_id;
                state.pending_question = Some(question);
            }
            SessionDomainEvent::TurnEnded
            | SessionDomainEvent::TurnStopped
            | SessionDomainEvent::TurnInterrupted => {
                state.status = SessionStatus::Idle;
            }
            SessionDomainEvent::TurnFailed { error } => {
                state.status = SessionStatus::Failed {
                    reason: error.clone(),
                };
                state.last_error = Some(error);
            }
            SessionDomainEvent::SessionFailed { reason } => {
                state.status = SessionStatus::Unrecoverable {
                    reason: reason.clone(),
                };
                state.last_error = Some(reason);
            }
            SessionDomainEvent::UsageRecorded {
                agent_id,
                usage_total,
            } => {
                state.agent_usage.insert(agent_id, usage_total);
            }
            SessionDomainEvent::SubAgentSpawned {
                id,
                parent,
                label,
                task,
                depth,
            } => {
                state
                    .subagents
                    .apply_spawned(id, parent, label, task, depth);
            }
            SessionDomainEvent::SubAgentRunning { id } => {
                state.subagents.apply_running(id);
            }
            SessionDomainEvent::SubAgentCompleted { id, output } => {
                state.subagents.apply_completed(id, output);
            }
            SessionDomainEvent::SubAgentFailed { id, error } => {
                state.subagents.apply_failed(id, error);
            }
            SessionDomainEvent::SubAgentNotified { id } => {
                state.subagents.apply_notified(id);
            }
        }
        state
    }

    async fn handle_command(
        &mut self,
        state: &SessionState,
        cmd: SessionCommand,
        ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            SessionCommand::UserMessage { text, reply } => {
                self.on_user_message(state, text, reply).await
            }
            SessionCommand::Stop { reply } => {
                if state.status != SessionStatus::Running {
                    let _ = reply.send(());
                    return CommandEffect::none();
                }
                self.cancel_run().await;
                let _ = reply.send(());
                self.report(SessionStatus::Idle).await;
                let mut events = vec![SessionDomainEvent::TurnStopped];
                // Stop is a turn boundary like any other, so anything the user
                // queued while the cancelled turn ran starts the next one.
                let next = Self::apply_event(state.clone(), SessionDomainEvent::TurnStopped);
                events.extend(self.drain(&next).await);
                self.publish_inbox(state, &events);
                CommandEffect::persist(events)
            }
            SessionCommand::Delete { reply } => {
                self.cancel_run().await;
                self.stop_agents().await;
                self.deps
                    .runtimes
                    .delete(&self.id.to_string(), &self.spec.vendor)
                    .await;
                let _ = reply.send(());
                CommandEffect::stop()
            }
            SessionCommand::Subscribe { reply } => {
                let _ = reply.send(self.frames.subscribe());
                CommandEffect::none()
            }
            SessionCommand::History {
                agent_id,
                query,
                reply,
            } => {
                let agent = match agent_id.as_deref() {
                    None | Some("main") => self.main_agent.clone(),
                    Some(raw) => Uuid::parse_str(raw)
                        .ok()
                        .and_then(|id| self.sub_agents.get(&id).cloned()),
                };
                // Read history from the resident actor's in-memory state. No
                // journal access, no runtime — opening a session to read it
                // stays free of sandbox cost.
                let page = match agent {
                    Some(agent) => agent
                        .ask(|reply| AgentCommand::GetHistory { query, reply })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(page);
                CommandEffect::none()
            }
            SessionCommand::UsageStats { reply } => {
                let stats = self.read_usage(state).await;
                let _ = reply.send(stats);
                CommandEffect::none()
            }
            SessionCommand::PrepareOffload { reply } => {
                // A run started while the supervisor was deciding: refuse, and
                // let the idle clock start again. This is the invariant that
                // keeps a forty-minute tool call from being unloaded out from
                // under itself — the main agent's run, or any subagent's.
                if state.status == SessionStatus::Running || state.subagents.has_active() {
                    let _ = reply.send(false);
                    return CommandEffect::none();
                }
                self.stop_agents().await;
                self.deps
                    .runtimes
                    .hibernate(&self.id.to_string(), &self.spec.vendor)
                    .await;
                // Answered as this actor's last act: it writes nothing after
                // returning, so the supervisor can drop its reference the
                // moment it sees `true`.
                let _ = reply.send(true);
                CommandEffect::stop()
            }
            SessionCommand::AgentOutcome(outcome) => self.on_agent_outcome(state, outcome).await,
            SessionCommand::SubAgentTree { reply } => {
                let tree = state
                    .subagents
                    .ids()
                    .into_iter()
                    .filter_map(|id| state.subagents.get(&id).map(|rec| (id, rec.clone())))
                    .collect();
                let _ = reply.send(tree);
                CommandEffect::none()
            }
            SessionCommand::ReconcileSubAgents => {
                let interrupted = state.subagents.interrupted();
                if interrupted.is_empty() {
                    return CommandEffect::none();
                }
                CommandEffect::persist(
                    interrupted
                        .into_iter()
                        .map(|id| SessionDomainEvent::SubAgentFailed {
                            id,
                            error: INTERRUPTED_ERROR.to_string(),
                        })
                        .collect(),
                )
            }
            SessionCommand::ReconcileInterrupted => {
                if state.status == SessionStatus::Running {
                    self.report(SessionStatus::Idle).await;
                    CommandEffect::persist(vec![SessionDomainEvent::TurnInterrupted])
                } else {
                    CommandEffect::none()
                }
            }
            SessionCommand::SetSessionTitle { title, reply } => {
                let result = match normalize_session_title(&title) {
                    Ok(title) => self.rename_session(title).await,
                    Err(error) => Err(error.to_string()),
                };
                let _ = reply.send(result);
                CommandEffect::none()
            }
            SessionCommand::SpawnSubAgent {
                caller,
                label,
                task,
                reply,
            } => {
                let Some(parent_depth) = state.subagents.depth_of(caller) else {
                    let _ = reply.send(Err("caller is not a known agent".to_string()));
                    return CommandEffect::none();
                };
                if parent_depth >= MAX_SUBAGENT_DEPTH {
                    let _ = reply.send(Err(format!(
                        "max subagent depth {MAX_SUBAGENT_DEPTH} reached"
                    )));
                    return CommandEffect::none();
                }
                let max = self.spec.agent.max_subagents();
                if state.subagents.active_count() >= max {
                    let _ = reply.send(Err(format!("{max} subagents already active")));
                    return CommandEffect::none();
                }
                // Persist first, spawn second: a crash between the two replays
                // as a Running node with no actor, which recovery reconciles
                // to failed — never an untracked agent.
                let id = Uuid::new_v4();
                let spawned = SessionDomainEvent::SubAgentSpawned {
                    id,
                    parent: caller,
                    label: label.clone(),
                    task: task.clone(),
                    depth: parent_depth + 1,
                };
                let (tx, rx) = oneshot::channel();
                let self_ref = ctx.self_ref();
                tokio::spawn(async move {
                    let persisted = rx.await.unwrap_or_else(|_| {
                        Err(horsie_actor::JournalError::Backend(
                            "spawn ack channel closed".to_string(),
                        ))
                    });
                    let _ = self_ref
                        .tell(SessionCommand::FinishSpawnSubAgent {
                            id,
                            caller,
                            label,
                            task,
                            reply,
                            persisted,
                        })
                        .await;
                });
                CommandEffect::persist(vec![spawned]).and_ack(tx)
            }
            SessionCommand::FinishSpawnSubAgent {
                id,
                label,
                task,
                reply,
                persisted,
                ..
            } => {
                if let Err(e) = persisted {
                    let _ = reply.send(Err(format!("persist subagent: {e}")));
                    return CommandEffect::none();
                }
                let agent = self.spawn_sub_agent_actor(ctx, id);
                let _ = agent.tell(AgentCommand::Run { input: task }).await;
                emit_progress(
                    &self.frames,
                    "subagent_spawned",
                    Some(format!("\"{label}\" ({id})")),
                );
                let _ = reply.send(Ok(id));
                CommandEffect::none()
            }
            SessionCommand::SubAgentStatus { caller, id, reply } => {
                let rendered = match id {
                    Some(id) => state
                        .subagents
                        .render_node(&id)
                        .ok_or_else(|| format!("no such subagent: {id}")),
                    None => Ok(state.subagents.render_subtree(caller)),
                };
                let _ = reply.send(rendered);
                CommandEffect::none()
            }
        }
    }

    /// Loading a session spawns its agent and repairs a turn the process died
    /// in. It calls no vendor, starts no run, and drains nothing — an
    /// interrupted assistant turn is over, and queued user messages wait for
    /// the next turn the user starts.
    async fn on_recovery_complete(&mut self, state: &SessionState, ctx: &mut ActorContext<Self>) {
        self.spawn_main_agent(ctx);
        // Resident subagent actors — journal replay only. No runs start here:
        // nodes the process died under are reconciled to failed, and results
        // owed to parents flush at the next turn boundary, never on load.
        for id in state.subagents.ids() {
            self.spawn_sub_agent_actor(ctx, id);
        }
        if !state.subagents.interrupted().is_empty() {
            let _ = ctx
                .self_ref()
                .tell(SessionCommand::ReconcileSubAgents)
                .await;
        }
        if state.status == SessionStatus::Running {
            let _ = ctx
                .self_ref()
                .tell(SessionCommand::ReconcileInterrupted)
                .await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn queued(id: &str, text: &str) -> SessionDomainEvent {
        SessionDomainEvent::MessageQueued {
            id: id.to_string(),
            text: text.to_string(),
            at_ms: 0,
        }
    }

    fn fold(events: Vec<SessionDomainEvent>) -> SessionState {
        events
            .into_iter()
            .fold(SessionState::default(), SessionActor::apply_event)
    }

    #[test]
    fn a_fresh_session_is_idle_with_an_empty_inbox() {
        let s = SessionState::default();
        assert_eq!(s.status, SessionStatus::Idle);
        assert!(s.inbox.is_empty());
    }

    #[test]
    fn queued_messages_accumulate_without_changing_status() {
        let s = fold(vec![queued("m1", "one"), queued("m2", "two")]);
        assert_eq!(s.status, SessionStatus::Idle, "queueing is not running");
        assert_eq!(s.inbox.len(), 2);
    }

    #[test]
    fn a_turn_consumes_exactly_the_messages_it_names() {
        let s = fold(vec![
            queued("m1", "one"),
            queued("m2", "two"),
            SessionDomainEvent::TurnBegan {
                consumed: vec!["m1".into()],
                answering: None,
            },
            queued("m3", "three"),
        ]);
        assert_eq!(s.status, SessionStatus::Running);
        let ids: Vec<&str> = s.inbox.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["m2", "m3"],
            "a message that arrived after the turn began must still be owed an answer"
        );
    }

    #[test]
    fn a_turn_that_answers_an_ask_clears_it() {
        let s = fold(vec![
            SessionDomainEvent::AskRecorded {
                tool_call_id: Some("call-1".into()),
                question: "which branch?".into(),
            },
            queued("m1", "main"),
            SessionDomainEvent::TurnBegan {
                consumed: vec!["m1".into()],
                answering: Some("call-1".into()),
            },
        ]);
        assert_eq!(s.status, SessionStatus::Running);
        assert!(s.pending_ask.is_none(), "the ask was answered");
        assert!(s.pending_question.is_none());
    }

    #[test]
    fn an_ask_survives_a_crash_so_the_answer_is_not_re_asked() {
        // TurnBegan is what clears the ask, and it is journaled with the
        // consumption in one step: a crash before it replays to "still asking".
        let s = fold(vec![
            SessionDomainEvent::AskRecorded {
                tool_call_id: Some("call-1".into()),
                question: "which branch?".into(),
            },
            queued("m1", "main"),
        ]);
        assert_eq!(s.status, SessionStatus::AwaitingInput);
        assert_eq!(s.pending_ask.as_deref(), Some("call-1"));
        assert_eq!(s.inbox.len(), 1, "the answer is still owed");
    }

    #[test]
    fn stop_and_interrupt_both_land_idle_and_keep_the_inbox() {
        for boundary in [
            SessionDomainEvent::TurnStopped,
            SessionDomainEvent::TurnInterrupted,
        ] {
            let s = fold(vec![
                queued("m1", "one"),
                SessionDomainEvent::TurnBegan {
                    consumed: vec!["m1".into()],
                    answering: None,
                },
                queued("m2", "queued while running"),
                boundary,
            ]);
            assert_eq!(s.status, SessionStatus::Idle);
            assert_eq!(
                s.inbox.len(),
                1,
                "an accepted message is a promise; a stop cancels the turn, not the promise"
            );
        }
    }

    #[test]
    fn a_failed_turn_is_sticky_but_not_terminal() {
        let s = fold(vec![
            queued("m1", "still owed an answer"),
            SessionDomainEvent::TurnFailed {
                error: "provider exploded".into(),
            },
        ]);
        assert!(matches!(s.status, SessionStatus::Failed { .. }));
        assert_eq!(s.last_error.as_deref(), Some("provider exploded"));
        assert_eq!(
            s.inbox.len(),
            1,
            "a turn that failed answered nothing; the queue is still owed"
        );

        // The next turn moves it straight back to Running.
        let s = SessionActor::apply_event(
            s,
            SessionDomainEvent::TurnBegan {
                consumed: vec![],
                answering: None,
            },
        );
        assert_eq!(s.status, SessionStatus::Running);
        // The detail endpoint reports `last_error`, so a turn that has just
        // started must not still be advertising the previous turn's failure.
        assert_eq!(s.last_error, None);
    }

    #[test]
    fn a_gone_runtime_is_terminal() {
        let s = fold(vec![SessionDomainEvent::SessionFailed {
            reason: "vendor has no runtime".into(),
        }]);
        assert!(matches!(s.status, SessionStatus::Unrecoverable { .. }));
    }

    #[test]
    fn usage_is_recorded_per_agent() {
        let s = fold(vec![SessionDomainEvent::UsageRecorded {
            agent_id: MAIN_AGENT_ID.to_string(),
            usage_total: UsageTotal {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: None,
                cache_read_tokens: None,
            },
        }]);
        assert_eq!(s.agent_usage.get(MAIN_AGENT_ID).unwrap().input_tokens, 10);
    }

    #[test]
    fn subagent_events_fold_into_the_tree() {
        use crate::sessions::subagents::{SubAgentParent, SubAgentStatus};
        let id = Uuid::new_v4();
        let s = fold(vec![SessionDomainEvent::SubAgentSpawned {
            id,
            parent: SubAgentParent::Main,
            label: "research".into(),
            task: "look into it".into(),
            depth: 1,
        }]);
        assert_eq!(s.subagents.active_count(), 1);

        let s = SessionActor::apply_event(
            s,
            SessionDomainEvent::SubAgentCompleted {
                id,
                output: "answer".into(),
            },
        );
        let rec = s.subagents.get(&id).unwrap();
        assert_eq!(rec.status, SubAgentStatus::Completed);
        assert!(!rec.notified);

        let s = SessionActor::apply_event(s, SessionDomainEvent::SubAgentNotified { id });
        assert!(s.subagents.get(&id).unwrap().notified);
    }

    #[test]
    fn a_running_then_failed_subagent_reads_as_interrupted_then_terminal() {
        use crate::sessions::subagents::SubAgentParent;
        let id = Uuid::new_v4();
        let s = fold(vec![SessionDomainEvent::SubAgentSpawned {
            id,
            parent: SubAgentParent::Main,
            label: "w".into(),
            task: "t".into(),
            depth: 1,
        }]);
        assert_eq!(s.subagents.interrupted(), vec![id]);
        let s = SessionActor::apply_event(
            s,
            SessionDomainEvent::SubAgentFailed {
                id,
                error: "interrupted by restart".into(),
            },
        );
        assert!(s.subagents.interrupted().is_empty());
    }

    #[test]
    fn merging_joins_in_arrival_order_with_a_blank_line() {
        let s = fold(vec![queued("m1", "one"), queued("m2", "two")]);
        let merged = s
            .inbox
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join(MERGE_SEPARATOR);
        assert_eq!(merged, "one\n\ntwo");
    }

    #[test]
    fn a_title_is_derived_from_the_first_line_only() {
        assert_eq!(derive_title("hello\nworld").as_deref(), Some("hello"));
        assert!(derive_title("   \n").is_none());
        let long = "x".repeat(TITLE_MAX_CHARS + 10);
        let title = derive_title(&long).unwrap();
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
    }

    // ── Actor-level coverage: `drain()` and `PrepareOffload`'s refuse-if-running
    // branch. The rewrite that introduced the durable inbox dropped both.

    fn actor_spec_fixture() -> SessionSpec {
        use crate::sessions::spec::WorkspaceDef;
        use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};
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
            workspaces: vec![WorkspaceDef {
                name: "main".into(),
            }],
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

    struct ActorFixture {
        deps: ServerDeps,
        agent: crate::runtime_vendor::fake::FakeRuntimeVendor,
        _tmp: tempfile::TempDir,
    }

    async fn actor_fixture() -> ActorFixture {
        let tmp = tempfile::tempdir().unwrap();
        let agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock")
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
        ActorFixture {
            deps,
            agent,
            _tmp: tmp,
        }
    }

    /// A supervisor stand-in for tests that spawn a bare `SessionActor`: it
    /// answers nothing, and exists only so `report()`'s `.tell()` has a live
    /// mailbox to land in.
    struct DeafSupervisor;

    #[async_trait]
    impl EventSourcedActor for DeafSupervisor {
        type Command = SessionSupervisorCommand;
        type Event = ();
        type State = ();

        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("test", "deaf-supervisor")
        }
        fn initial_state() {}
        fn apply_event((): (), (): ()) {}
        async fn handle_command(
            &mut self,
            (): &(),
            _cmd: SessionSupervisorCommand,
            _ctx: &mut ActorContext<Self>,
        ) -> CommandEffect<()> {
            CommandEffect::none()
        }
    }

    fn spawn_deaf_supervisor() -> ActorRef<SessionSupervisorCommand> {
        horsie_actor::spawn_root(
            DeafSupervisor,
            Arc::new(horsie_actor::InMemoryJournal::new()),
        )
    }

    #[tokio::test]
    async fn drain_does_nothing_when_the_inbox_is_empty() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let mut actor = SessionActor::new(Uuid::new_v4(), actor_spec_fixture(), f.deps, parent);
        let events = actor.drain(&SessionState::default()).await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn drain_does_nothing_while_a_turn_is_already_running() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let mut actor = SessionActor::new(Uuid::new_v4(), actor_spec_fixture(), f.deps, parent);
        let state = fold(vec![
            queued("m1", "one"),
            SessionDomainEvent::TurnBegan {
                consumed: vec!["m1".into()],
                answering: None,
            },
            queued("m2", "queued while running"),
        ]);
        let events = actor.drain(&state).await;
        assert!(
            events.is_empty(),
            "a run in flight must never be drained into a second one"
        );
    }

    #[tokio::test]
    async fn drain_refuses_once_the_session_is_unrecoverable() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let mut actor = SessionActor::new(Uuid::new_v4(), actor_spec_fixture(), f.deps, parent);
        let state = fold(vec![
            queued("m1", "one"),
            SessionDomainEvent::SessionFailed {
                reason: "runtime gone".into(),
            },
        ]);
        let events = actor.drain(&state).await;
        assert!(
            events.is_empty(),
            "a terminal session must never start another turn"
        );
    }

    /// A failed turn is a turn boundary that deliberately does *not* drain. The
    /// cause is usually stuck — an expired key, a dead vendor — and draining
    /// would turn three queued messages into three back-to-back failures the
    /// user never asked for. The next message they send drains them.
    #[tokio::test]
    async fn a_failed_turn_does_not_drain() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let id = Uuid::new_v4();
        let mut actor = SessionActor::new(id, actor_spec_fixture(), f.deps, parent);
        // A turn is running, and a message arrived while it was.
        let state = fold(vec![
            queued("m1", "one"),
            SessionDomainEvent::TurnBegan {
                consumed: vec!["m1".into()],
                answering: None,
            },
            queued("m2", "queued while running"),
        ]);

        let effect = actor
            .on_agent_outcome(
                &state,
                AgentOutcome::Failed {
                    session_id: id,
                    error: "provider exploded".into(),
                    recoverable: true,
                    terminal: false,
                },
            )
            .await;

        let events = effect.events();
        assert_eq!(
            events.len(),
            1,
            "a failed turn records the failure and nothing else: {events:?}"
        );
        assert!(
            matches!(events[0], SessionDomainEvent::TurnFailed { .. }),
            "{events:?}"
        );
    }

    /// Stop is a turn boundary like any other: it cancels the turn, not the
    /// promise. Whatever was queued while the cancelled turn ran starts the
    /// next one immediately — which is exactly why the client marks queued
    /// messages as unread, so that next turn does not look self-inflicted.
    #[tokio::test]
    async fn stop_then_a_queued_message_starts_the_next_turn() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let mut actor = SessionActor::new(Uuid::new_v4(), actor_spec_fixture(), f.deps, parent);
        let running = fold(vec![
            queued("m1", "one"),
            SessionDomainEvent::TurnBegan {
                consumed: vec!["m1".into()],
                answering: None,
            },
            queued("m2", "queued while running"),
        ]);

        let stopped = SessionActor::apply_event(running, SessionDomainEvent::TurnStopped);
        assert_eq!(stopped.status, SessionStatus::Idle);
        let events = actor.drain(&stopped).await;

        assert_eq!(events.len(), 1, "{events:?}");
        let SessionDomainEvent::TurnBegan { consumed, .. } = &events[0] else {
            panic!("a stop must let the queue start the next turn, got {events:?}");
        };
        assert_eq!(consumed, &vec!["m2".to_string()]);
    }

    #[tokio::test]
    async fn drain_consumes_the_whole_inbox_and_starts_a_turn() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let mut actor = SessionActor::new(Uuid::new_v4(), actor_spec_fixture(), f.deps, parent);
        let state = fold(vec![queued("m1", "one"), queued("m2", "two")]);
        let events = actor.drain(&state).await;
        assert_eq!(events.len(), 1);
        let SessionDomainEvent::TurnBegan {
            consumed,
            answering,
        } = &events[0]
        else {
            panic!("expected TurnBegan, got {:?}", events[0]);
        };
        assert_eq!(consumed, &vec!["m1".to_string(), "m2".to_string()]);
        assert!(answering.is_none());
    }

    #[tokio::test]
    async fn drain_delivers_a_merged_message_as_the_pending_asks_answer() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let mut actor = SessionActor::new(Uuid::new_v4(), actor_spec_fixture(), f.deps, parent);
        let state = fold(vec![
            SessionDomainEvent::AskRecorded {
                tool_call_id: Some("call-1".into()),
                question: "which?".into(),
            },
            queued("m1", "main"),
        ]);
        let events = actor.drain(&state).await;
        assert_eq!(events.len(), 1);
        let SessionDomainEvent::TurnBegan {
            consumed,
            answering,
        } = &events[0]
        else {
            panic!("expected TurnBegan, got {:?}", events[0]);
        };
        assert_eq!(consumed, &vec!["m1".to_string()]);
        assert_eq!(answering.as_deref(), Some("call-1"));
    }

    /// An `LlmProvider` that hangs until released, so a test can hold a run
    /// genuinely `Running` for as long as it needs to.
    struct BlockingProvider {
        gate: tokio::sync::Notify,
    }

    impl BlockingProvider {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                gate: tokio::sync::Notify::new(),
            })
        }
        fn release(&self) {
            self.gate.notify_one();
        }
    }

    #[async_trait]
    impl LlmProvider for BlockingProvider {
        fn model_id(&self) -> &str {
            "mock"
        }
        async fn complete(
            &self,
            _request: horsie_agentcore::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn horsie_agentcore::EventSink,
        ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
            self.gate.notified().await;
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "done".to_string(),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        }
    }

    #[tokio::test]
    async fn prepare_offload_refuses_while_a_run_is_in_flight() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "mock", &actor_spec_fixture())
            .await
            .expect("create");
        let provider = BlockingProvider::new();
        f.deps
            .provider_registry
            .write()
            .unwrap()
            .insert("mock".to_string(), provider.clone() as Arc<dyn LlmProvider>);

        let parent = spawn_deaf_supervisor();
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        let session = horsie_actor::spawn_root(
            SessionActor::new(id, actor_spec_fixture(), f.deps.clone(), parent),
            journal,
        );

        session
            .ask(|reply| SessionCommand::UserMessage {
                text: "go".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();

        let offloadable = session
            .ask(|reply| SessionCommand::PrepareOffload { reply })
            .await
            .unwrap();
        assert!(
            !offloadable,
            "a run in flight must never be offloaded out from under itself"
        );
        assert!(
            f.agent
                .signals()
                .iter()
                .all(|s| !s.starts_with("hibernate:")),
            "refusing must not touch the runtime: {:?}",
            f.agent.signals()
        );

        // Refusing must leave the actor exactly as it was, still answering
        // commands normally rather than having torn itself down.
        provider.release();
        let (tx, rx) = oneshot::channel();
        session
            .tell(SessionCommand::UsageStats { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap();
    }

    /// A provider whose every call immediately ends the turn with plain text.
    struct EchoProvider;

    #[async_trait]
    impl LlmProvider for EchoProvider {
        fn model_id(&self) -> &str {
            "mock"
        }
        async fn complete(
            &self,
            _request: horsie_agentcore::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn horsie_agentcore::EventSink,
        ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "sub answer".to_string(),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        }
    }

    async fn spawn_session_with_provider(
        provider: Arc<dyn LlmProvider>,
    ) -> (
        ActorFixture,
        ActorRef<SessionCommand>,
        Uuid,
        Arc<dyn horsie_actor::Journal>,
    ) {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "mock", &actor_spec_fixture())
            .await
            .expect("create");
        f.deps
            .provider_registry
            .write()
            .unwrap()
            .insert("mock".to_string(), provider);
        let parent = spawn_deaf_supervisor();
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        let session = horsie_actor::spawn_root(
            SessionActor::new(id, actor_spec_fixture(), f.deps.clone(), parent),
            journal.clone(),
        );
        (f, session, id, journal)
    }

    /// Poll the session's folded state until the tree satisfies `pred` (2s
    /// cap). Subagent progress is journal-first, so the fold is the honest
    /// thing to wait on.
    async fn wait_for_tree(
        journal: &Arc<dyn horsie_actor::Journal>,
        session_id: Uuid,
        pred: impl Fn(&crate::sessions::subagents::SubAgentTree) -> bool,
    ) {
        for _ in 0..200 {
            let state = crate::sessions::events::fold_session_state(journal, session_id).await;
            if pred(&state.subagents) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("tree condition not met within 2s");
    }

    async fn spawn_sub(session: &ActorRef<SessionCommand>, label: &str, task: &str) -> Uuid {
        session
            .ask(|reply| SessionCommand::SpawnSubAgent {
                caller: crate::sessions::subagents::SubAgentParent::Main,
                label: label.into(),
                task: task.into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn spawn_records_a_running_subagent_in_the_tree() {
        // Completion routing lands with outcome handling (next task); here the
        // spawn itself is what must be durable and attributed.
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        let sub = spawn_sub(&session, "research", "dig into it").await;
        wait_for_tree(&journal, id, |t| {
            t.get(&sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
        })
        .await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        let rec = state.subagents.get(&sub).unwrap();
        assert_eq!(rec.depth, 1);
        assert_eq!(rec.parent, crate::sessions::subagents::SubAgentParent::Main);
        assert_eq!(rec.label, "research");
        assert_eq!(rec.task, "dig into it");
    }

    #[tokio::test]
    async fn spawn_beyond_depth_four_is_rejected() {
        // A hanging provider keeps every spawned node Running, so the chain
        // builds deterministically: Main → d1 → d2 → d3 → d4, and d4's spawn
        // is refused.
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        let mut parent = crate::sessions::subagents::SubAgentParent::Main;
        for _ in 0..4 {
            let id_child = session
                .ask(|reply| SessionCommand::SpawnSubAgent {
                    caller: parent,
                    label: "w".into(),
                    task: "t".into(),
                    reply,
                })
                .await
                .unwrap()
                .unwrap();
            wait_for_tree(&journal, id, |t| t.has_active()).await;
            parent = crate::sessions::subagents::SubAgentParent::SubAgent(id_child);
        }
        let res = session
            .ask(|reply| SessionCommand::SpawnSubAgent {
                caller: parent,
                label: "x".into(),
                task: "y".into(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(res.unwrap_err(), "max subagent depth 4 reached");
    }

    #[tokio::test]
    async fn spawn_beyond_the_concurrency_cap_is_rejected() {
        let gate = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
        for _ in 0..8 {
            let _ = spawn_sub(&session, "w", "t").await;
        }
        wait_for_tree(&journal, id, |t| t.active_count() == 8).await;
        let res = session
            .ask(|reply| SessionCommand::SpawnSubAgent {
                caller: crate::sessions::subagents::SubAgentParent::Main,
                label: "x".into(),
                task: "y".into(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(res.unwrap_err(), "8 subagents already active");
    }

    #[tokio::test]
    async fn spawn_from_an_unknown_caller_is_rejected() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let res = session
            .ask(|reply| SessionCommand::SpawnSubAgent {
                caller: crate::sessions::subagents::SubAgentParent::SubAgent(Uuid::new_v4()),
                label: "x".into(),
                task: "y".into(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(res.unwrap_err(), "caller is not a known agent");
    }

    #[tokio::test]
    async fn subagent_toolbox_strips_session_metadata_tools() {
        let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;

        let build = |kind: SessionAgentKind| SessionContextProvider {
            runtimes: f.deps.runtimes.provider(id.to_string(), "mock".into()),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            settings: actor_spec_fixture().agent,
            session_id: id,
            kind,
            session: session.clone(),
            frames: broadcast::channel(8).0,
            last_client: Mutex::new(None),
        };

        let main = build(SessionAgentKind::Main).provide().await.unwrap();
        let main_tools: Vec<String> = main.toolbox.specs().into_iter().map(|s| s.name).collect();
        for t in [
            "spawn_agent",
            "subagent_status",
            "set_session_title",
            "ask_user",
        ] {
            assert!(main_tools.contains(&t.to_string()), "main lacks {t}");
        }

        let sub_id = Uuid::new_v4();
        let sub = build(SessionAgentKind::Sub(sub_id))
            .provide()
            .await
            .unwrap();
        let sub_tools: Vec<String> = sub.toolbox.specs().into_iter().map(|s| s.name).collect();
        for t in ["spawn_agent", "subagent_status"] {
            assert!(sub_tools.contains(&t.to_string()), "sub lacks {t}");
        }
        for t in ["set_session_title", "ask_user"] {
            assert!(!sub_tools.contains(&t.to_string()), "sub must not have {t}");
        }
        assert!(
            sub.system_prompt.unwrap().contains("# Subagent role"),
            "the subagent prompt must explain its role"
        );
    }

    fn user_texts(page: &horsie_workflow::AgentHistoryPage) -> Vec<String> {
        page.messages
            .iter()
            .filter(|m| m.role == horsie_agentcore::Role::User)
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                horsie_agentcore::ContentPart::Text(t) => Some(t.text.clone()),
                horsie_agentcore::ContentPart::ToolCall(_)
                | horsie_agentcore::ContentPart::ToolResult(_)
                | horsie_agentcore::ContentPart::Thinking(_) => None,
            })
            .collect()
    }

    async fn main_history(session: &ActorRef<SessionCommand>) -> horsie_workflow::AgentHistoryPage {
        session
            .ask(|reply| SessionCommand::History {
                agent_id: None,
                query: horsie_workflow::HistoryQuery {
                    before: None,
                    limit: 50,
                },
                reply,
            })
            .await
            .unwrap()
            .expect("main agent history")
    }

    #[tokio::test]
    async fn a_completed_subagent_notifies_an_idle_main_agent() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, "research", "dig").await;
        // Owed, then delivered: the tree's notified flag flips exactly once.
        wait_for_tree(&journal, id, |t| t.get(&sub).is_some_and(|r| r.notified)).await;
        let texts = user_texts(&main_history(&session).await);
        assert!(
            texts.iter().any(
                |t| t.contains("[subagent \"research\" completed]") && t.contains("sub answer")
            ),
            "the main agent must be told the result: {texts:?}"
        );
    }

    /// Fails any completion whose conversation contains `needle`; answers
    /// everything else with plain text. Distinguishes the subagent's run from
    /// the main agent's when both share one provider.
    struct FailOnNeedleProvider {
        needle: String,
    }

    #[async_trait]
    impl LlmProvider for FailOnNeedleProvider {
        fn model_id(&self) -> &str {
            "mock"
        }
        async fn complete(
            &self,
            request: horsie_agentcore::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn horsie_agentcore::EventSink,
        ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
            let hit = request
                .messages
                .iter()
                .flat_map(|m| m.parts.iter())
                .any(|p| matches!(p, horsie_agentcore::ContentPart::Text(t) if t.text.contains(&self.needle)));
            if hit {
                return Err(horsie_agentcore::LlmError::ApiError {
                    status: 401,
                    message: "bad key".to_string(),
                });
            }
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "fine".to_string(),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        }
    }

    #[tokio::test]
    async fn a_failed_subagent_reports_the_error_to_its_parent() {
        let provider = FailOnNeedleProvider {
            needle: "doomed task".to_string(),
        };
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(provider)).await;
        let sub = spawn_sub(&session, "risky", "doomed task").await;
        wait_for_tree(&journal, id, |t| t.get(&sub).is_some_and(|r| r.notified)).await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        let rec = state.subagents.get(&sub).unwrap();
        assert_eq!(
            rec.status,
            crate::sessions::subagents::SubAgentStatus::Failed
        );
        assert!(rec.error.as_deref().unwrap().contains("bad key"));
        let texts = user_texts(&main_history(&session).await);
        assert!(
            texts
                .iter()
                .any(|t| t.contains("[subagent \"risky\" failed]")),
            "the parent must hear the failure: {texts:?}"
        );
    }

    #[tokio::test]
    async fn a_notification_waits_out_an_awaiting_input_session() {
        use horsie_agentcore::StopReason;
        use horsie_agentcore::testkit::{MockProvider, Script};
        // Main's first call asks the user; every later call (the subagent's
        // run, then the main agent's answer turn) ends with plain text.
        let provider = MockProvider::scripted(
            Script::of([Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::ToolCall(
                    horsie_agentcore::ToolCallPart {
                        id: "ask-1".into(),
                        name: "ask_user".into(),
                        input: serde_json::json!({"question": "which one?"}),
                    },
                )],
                stop_reason: StopReason::ToolUse,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })])
            .then_repeating_with(|| {
                Ok(horsie_agentcore::CompletionResponse {
                    parts: vec![horsie_agentcore::ContentPart::Text(
                        horsie_agentcore::TextPart {
                            text: "sub answer".to_string(),
                        },
                    )],
                    stop_reason: StopReason::EndTurn,
                    usage: horsie_agentcore::Usage::without_cache(1, 1),
                })
            }),
        );
        let (_f, session, id, journal) = spawn_session_with_provider(provider).await;

        // Park the session on the ask.
        session
            .ask(|reply| SessionCommand::UserMessage {
                text: "start".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        for _ in 0..200 {
            let state = crate::sessions::events::fold_session_state(&journal, id).await;
            if state.status == crate::sessions::spec::SessionStatus::AwaitingInput {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // A subagent completes while the session is AwaitingInput.
        let sub = spawn_sub(&session, "research", "dig").await;
        wait_for_tree(&journal, id, |t| {
            t.get(&sub).is_some_and(|r| {
                r.status == crate::sessions::subagents::SubAgentStatus::Completed && !r.notified
            })
        })
        .await;
        // The ask is still pending — the notification must not have answered it.
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.status,
            crate::sessions::spec::SessionStatus::AwaitingInput
        );

        // The user's reply carries the notification along in the same input.
        session
            .ask(|reply| SessionCommand::UserMessage {
                text: "the first one".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_tree(&journal, id, |t| t.get(&sub).is_some_and(|r| r.notified)).await;
        // The answer went in as the pending ask's tool result, so look there:
        // one result must carry both the user's reply and the notification.
        let page = main_history(&session).await;
        let results: Vec<String> = page
            .messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                horsie_agentcore::ContentPart::ToolResult(r) => Some(r.output.clone()),
                horsie_agentcore::ContentPart::Text(_)
                | horsie_agentcore::ContentPart::ToolCall(_)
                | horsie_agentcore::ContentPart::Thinking(_) => None,
            })
            .collect();
        assert!(
            results
                .iter()
                .any(|t| t.contains("the first one")
                    && t.contains("[subagent \"research\" completed]")),
            "the notification rides with the user's answer: {results:?}"
        );
    }

    #[tokio::test]
    async fn recovery_respawns_subagents_and_fails_interrupted_ones() {
        // First incarnation: a hanging provider keeps the subagent mid-run.
        let gate = BlockingProvider::new();
        let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
        let sub = spawn_sub(&session, "w", "t").await;
        wait_for_tree(&journal, id, |t| {
            t.get(&sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
        })
        .await;
        // Simulate process death: the last ref drops, the journal lives on.
        drop(session);

        // Second incarnation on the same journal.
        let parent = spawn_deaf_supervisor();
        let session2 = horsie_actor::spawn_root(
            SessionActor::new(id, actor_spec_fixture(), f.deps.clone(), parent),
            journal.clone(),
        );
        wait_for_tree(&journal, id, |t| {
            t.get(&sub)
                .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Failed)
        })
        .await;
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.subagents.get(&sub).unwrap().error.as_deref(),
            Some(crate::sessions::subagents::INTERRUPTED_ERROR)
        );
        // The transcript stays pageable: the resident actor answers history.
        let page = session2
            .ask(|reply| SessionCommand::History {
                agent_id: Some(sub.to_string()),
                query: horsie_workflow::HistoryQuery {
                    before: None,
                    limit: 10,
                },
                reply,
            })
            .await
            .unwrap();
        assert!(page.is_some(), "a reloaded subagent must answer history");
        gate.release();
    }

    #[tokio::test]
    async fn prepare_offload_refuses_with_an_active_subagent() {
        let gate = BlockingProvider::new();
        let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
        let _sub = spawn_sub(&session, "w", "t").await;
        wait_for_tree(&journal, id, |t| t.has_active()).await;

        let offloadable = session
            .ask(|reply| SessionCommand::PrepareOffload { reply })
            .await
            .unwrap();
        assert!(!offloadable, "an active subagent must block offload");
        assert!(
            f.agent
                .signals()
                .iter()
                .all(|s| !s.starts_with("hibernate:")),
            "refusing must not touch the runtime"
        );
        gate.release();
    }
}
