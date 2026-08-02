//! One interactive session: lifecycle state machine, the only emitter of
//! runtime vendor signals, and host of the reused (interactive-mode)
//! [`AgentActor`].
//!
//! Recovery is lazy: `on_recovery_complete` only reconciles a mid-turn crash
//! (`Running` → `Interrupted`); no vendor call and no agent spawn happens until
//! the next user action ("a user message means make it run").

use crate::runtime_vendor::{RuntimeSpec, RuntimeVendorLink, VendorRuntime};
use crate::sessions::ask_tool::{ASK_USER_TOOL, AskUserToolbox};
use crate::sessions::events::SessionEventSink;
use crate::sessions::spec::{AgentSettings, ServerDeps, SessionSpec, SessionStatus};
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::title_tool::{SessionTitleToolbox, normalize_session_title};
use crate::sessions::{SessionFrame, UserMessageError};
use async_trait::async_trait;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_agentcore::{LlmProvider, Toolbox};
use horsie_runtime_client::RuntimeClient;
use horsie_workflow::{
    AgentActor, AgentCommand, AgentHistoryPage, AgentOutcome, AgentOutcomeSink, AgentParams,
    AgentRunDef, AgentRuntimeContext, AgentUsageSnapshot, ContextProvider, Contexts,
    DefaultToolboxFactory, HistoryQuery, SharedContext, ToolboxFactory, UsageTotal,
    compose_system_prompt, scan_workspace,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

/// Capacity of a session's live frame broadcast. Slow subscribers see `lagged`
/// drops and catch up from the journal.
const FRAME_BROADCAST_CAPACITY: usize = 256;

/// The agent id a session's single hosted agent reports usage under. A fixed
/// label rather than a generated one until a session can host more than one.
const MAIN_AGENT_ID: &str = "main";

/// How long [`SessionActor::halt`] waits for a cancelled run to finish before
/// giving up on it. Generous relative to how long prompt cancellation actually
/// takes (milliseconds), but short enough that a wedged run can never hold the
/// session mailbox — and with it the Stop button — hostage.
const HALT_CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Send a resource-preparation progression onto a session's live frame stream.
/// A best-effort live signal (no subscribers → dropped); shown while a turn
/// spins up.
fn emit_progress(frames: &broadcast::Sender<SessionFrame>, stage: &str, detail: Option<String>) {
    let _ = frames.send(SessionFrame::Progression {
        stage: stage.to_string(),
        detail,
        at_ms: now_ms(),
    });
}

/// The baseline system prompt given to every session agent: role, tool-usage
/// norms, and environment guidance. Not user-overridable — layered under the
/// `# Workspaces` / skills sections `compose_system_prompt` appends.
const SESSION_AGENT_PROMPT: &str = include_str!("system_prompt.md");

/// Commands accepted by a [`SessionActor`].
pub enum SessionCommand {
    /// Provision the runtime after creation (sent once by the supervisor).
    Provision,
    /// A user message: answer a pending ask, or start a turn — attaching or
    /// re-provisioning whatever is missing first.
    UserMessage {
        text: String,
        reply: oneshot::Sender<Result<(), UserMessageError>>,
    },
    /// Stop: cancel any turn and stop the runtime, preserving it.
    Stop { reply: oneshot::Sender<()> },
    /// Delete: cancel, stop, and let the vendor decide the runtime's fate.
    Delete { reply: oneshot::Sender<()> },
    /// Hand back a live frame subscriber for the SSE stream.
    Subscribe {
        reply: oneshot::Sender<broadcast::Receiver<SessionFrame>>,
    },
    /// Read a window of conversation history. Answered from the agent's
    /// in-memory state — the live agent if one is running, else a transient
    /// read-only agent recovered just for this query (no runtime, no run).
    History {
        query: HistoryQuery,
        reply: oneshot::Sender<AgentHistoryPage>,
    },
    /// Read this session's aggregated usage (session-level total, summed
    /// across every agent it hosts) plus the primary agent's own usage and
    /// context-size snapshot. Same live-agent-or-transient-reader answering
    /// as `History`.
    UsageStats {
        reply: oneshot::Sender<SessionUsageStats>,
    },
    /// Tear down OS resources for a clean server shutdown; no status persisted,
    /// so a `Running` session reconciles to `Interrupted` next start.
    Shutdown { reply: oneshot::Sender<()> },
    /// Internal: the hosted agent reported its terminal outcome, tagged with
    /// the generation active when that agent was spawned (see
    /// [`SessionActor::generation`]).
    AgentOutcome(AgentOutcome, u64),
    /// Internal: post-recovery reconciliation (`Running` → `Interrupted`).
    ReconcileInterrupted,
    /// Set the session title from the built-in title tool.
    SetSessionTitle {
        title: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
}

/// Events recording a session's lifecycle. Persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionDomainEvent {
    Provisioned,
    ProvisionFailed {
        error: String,
    },
    TurnStarted,
    /// A pending ask was answered and the parked turn resumed.
    ///
    /// Distinct from [`Self::TurnStarted`] because it must set `Running` while
    /// *keeping* `pending_ask`: the status makes a concurrent message 409 instead
    /// of injecting a second `tool_result` for the same call (#61 item 3), and the
    /// retained `pending_ask` keeps the resume idempotent, so a crash between this
    /// event and the agent's own durable input still resumes the ask rather than
    /// starting a fresh turn. `TurnCompleted` clears it when the turn ends.
    AskAnswered,
    TurnCompleted,
    TurnFailed {
        error: String,
    },
    Asked {
        tool_call_id: Option<String>,
        question: String,
    },
    Interrupted,
    AttachFailed {
        error: String,
    },
    Stopped,
    Deleted,
    /// One agent's cumulative usage, freshly updated after a completed run.
    /// Persisted here (not just left on the agent's own state) so the
    /// session-level usage total is durable and never requires waking an
    /// idle agent to recompute — only the reporting agent's entry changes,
    /// `agent_id` distinguishes it once a session can host more than one.
    /// `usage_total` is the agent's full cumulative figure, not a delta, so a
    /// crash between the agent journaling `RunComplete` and this event
    /// persisting only under-reports until the *next* completed run, which
    /// overwrites with a fresh cumulative total and heals it — not a leak.
    UsageRecorded {
        agent_id: String,
        usage_total: UsageTotal,
    },
}

/// Persisted session state — purely a function of the event log. `status` is
/// `None` until the first event (a freshly-created session still provisioning).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
    pub status: Option<SessionStatus>,
    /// The ask tool-call id awaiting the user's answer (status AwaitingInput).
    pub pending_ask: Option<String>,
    pub pending_question: Option<String>,
    pub last_error: Option<String>,
    /// Each hosted agent's latest known cumulative usage, keyed by agent id
    /// ("main" today — the only agent a session hosts). Durable: updated by
    /// `UsageRecorded` whenever that agent completes a run, so the
    /// session-level total never requires waking an idle agent to recompute.
    #[serde(default)]
    pub agent_usage: HashMap<String, UsageTotal>,
}

/// One agent's own usage/context-size snapshot, labeled with the model it
/// ran. Session-level usage aggregates across these (today just the one);
/// context-size never does — it stays meaningfully per-agent.
#[derive(Debug, Clone)]
pub struct AgentUsageEntry {
    pub model: String,
    pub snapshot: AgentUsageSnapshot,
}

/// A session's aggregated usage, answering `SessionCommand::UsageStats`.
/// `session_total` sums every hosted agent's `usage_total` (today, one
/// agent); `main_agent` is the primary agent's own usage plus its
/// context-size snapshot, for the UI's context-window display.
#[derive(Debug, Clone)]
pub struct SessionUsageStats {
    pub session_total: UsageTotal,
    pub main_agent: AgentUsageEntry,
}

/// Whether a wake provisions fresh or revives preserved state.
enum WakeMode {
    Create,
    Attach,
}

/// Longest auto-derived session title, in characters (display metadata only —
/// mirrors how chat products title a conversation from its first message).
const TITLE_MAX_CHARS: usize = crate::sessions::title_tool::SESSION_TITLE_MAX_CHARS;

/// A short title derived from a user's first message, or `None` if it has no
/// usable text (e.g. all whitespace).
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
    runtime: Option<VendorRuntime>,
    agent: Option<ActorRef<AgentCommand>>,
    /// Bumped every time a new agent is spawned or halted. Tags every
    /// `AgentOutcome` a spawned agent can ever deliver (via `SessionParent`),
    /// so a straggler from a turn that was cancelled or superseded — its
    /// `Cancel` signal is cooperative and only checked at specific loop
    /// checkpoints, so a run can still finish and report a real outcome after
    /// `Stop`/a fresh message has already moved the session on — is
    /// recognized as stale and dropped in `on_agent_outcome` instead of
    /// clobbering whatever the session is doing now.
    generation: u64,
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
            runtime: None,
            agent: None,
            generation: 0,
        }
    }

    /// The journal identity of a session: kind `"session"`, id = the uuid.
    pub fn persistence_id_for(session_id: Uuid) -> PersistenceId {
        PersistenceId::new("session", session_id.to_string())
    }

    /// Report a status transition to the supervisor registry and the live stream.
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

    /// Persist a session title through the supervisor, then update local state
    /// and publish the already-durable title. Live publication is best-effort;
    /// the journal remains the source of truth.
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

    fn vendor(&self) -> Result<Arc<RuntimeVendorLink>, String> {
        let vendors = self
            .deps
            .vendors
            .read()
            .map_err(|_| "vendor registry lock poisoned".to_string())?;
        vendors
            .get(&self.spec.vendor)
            .cloned()
            .ok_or_else(|| format!("unknown runtime vendor '{}'", self.spec.vendor))
    }

    /// Write the capability file (the durable source of truth for re-attach) and
    /// assemble the vendor-facing runtime spec.
    fn write_runtime_spec(&self) -> Result<RuntimeSpec, String> {
        let dir = self
            .deps
            .state_dir
            .join("sessions")
            .join(self.id.to_string());
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let caps_path = dir.join("capabilities.json");
        std::fs::write(
            &caps_path,
            serde_json::to_vec_pretty(&self.spec.capabilities).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(RuntimeSpec {
            workspaces: self
                .spec
                .workspaces
                .iter()
                .map(|w| crate::runtime_vendor::WorkspaceSpec {
                    name: w.name.clone(),
                })
                .collect(),
            provision: self
                .spec
                .provision
                .iter()
                .map(|s| horsie_models::executor::ProvisionStep {
                    name: s.name.clone(),
                    uses: s.uses.clone(),
                    with: s
                        .with
                        .iter()
                        .map(|(k, v)| horsie_models::executor::StepParam {
                            key: k.clone(),
                            value: v.clone(),
                        })
                        .collect(),
                })
                .collect(),
            env: vec![],
            capabilities_file: caps_path,
        })
    }

    /// Ensure a live runtime, emitting the explicit vendor signal for `mode`.
    ///
    /// A retained runtime is reused only while its transport is still connected.
    /// Once the socket has dropped the client is terminally dead, and reusing it
    /// made every later turn fail identically until a Stop or a server restart
    /// (#61 item 2) — the case the "a failed turn never bricks the session"
    /// comment below claimed was covered and was not.
    async fn ensure_runtime(&mut self, mode: WakeMode) -> Result<(), String> {
        if let Some(runtime) = &self.runtime {
            if runtime.runtime_client.is_connected() {
                return Ok(());
            }
            // Drop, don't stop: the sandbox itself is very likely still alive
            // vendor-side, and `attach` below re-acquires that same runtime id.
            // Calling stop on a dead transport would only fail.
            tracing::warn!(
                session = %self.id,
                "runtime transport disconnected; releasing it and re-acquiring"
            );
            self.runtime = None;
        }
        // The runtime is down: provisioning it is the slow, visible step.
        emit_progress(&self.frames, "provisioning_runtime", None);
        let vendor = self.vendor()?;
        let mut rt_spec = self.write_runtime_spec()?;
        // Fresh, scoped token at every create AND attach — never persisted. It
        // authorizes the `git_checkout` provision steps for github.com repos.
        if let Some(minter) = &self.deps.github_tokens {
            let urls: Vec<String> = rt_spec
                .provision
                .iter()
                .filter(|s| s.uses == "git_checkout")
                .filter_map(|s| {
                    s.with
                        .iter()
                        .find(|p| p.key == "url")
                        .map(|p| p.value.clone())
                })
                .collect();
            if !urls.is_empty()
                && let Some(token) = minter.mint_for(&urls).await?
            {
                rt_spec.env.push(horsie_models::executor::EnvVar {
                    name: horsie_models::ENV_GITHUB_TOKEN.to_string(),
                    value: token,
                });
            }
        }
        let id = self.id.to_string();
        // Resolve the session's selected bundles to hashes plus a scoped token,
        // injected as env the runtime reads at startup. Re-resolved on attach
        // as well, so a session picks up bundle updates.
        //
        // Where those bundles land, and what URL the runtime fetches them from,
        // are the agent's business: it knows its own filesystem and how its
        // runtimes reach this server. The server supplies only what it alone
        // can — the hashes and the token authorizing them.
        if let Some(prov) = self.deps.plugins.as_ref() {
            let mut names = self.spec.plugins.clone();
            if names.is_empty() {
                names = prov.default_names().await;
            }
            if !names.is_empty() {
                let refs = prov.resolve(&names).await?;
                let hashes: Vec<String> = refs.iter().map(|r| r.hash.clone()).collect();
                let token = prov.mint_token(&id, &hashes);
                let manifest = serde_json::to_string(&refs).map_err(|e| e.to_string())?;
                rt_spec.env.extend([
                    horsie_models::executor::EnvVar {
                        name: horsie_models::ENV_PLUGIN_MANIFEST.to_string(),
                        value: manifest,
                    },
                    horsie_models::executor::EnvVar {
                        name: horsie_models::ENV_PLUGINS_TOKEN.to_string(),
                        value: token,
                    },
                ]);
            }
        }
        let runtime = match mode {
            WakeMode::Create => vendor.create(&id, &rt_spec).await,
            WakeMode::Attach => vendor.get(&id).await,
        }
        .map_err(|e| e.to_string())?;
        self.runtime = Some(runtime);
        Ok(())
    }

    /// Ensure a live agent child (recovering its conversation from the journal
    /// on respawn). Spawning is deliberately cheap: the provider, toolbox, and
    /// system prompt are resolved lazily per run by [`SessionContextProvider`] on
    /// the run's own task, so the workspace scan / MCP connect / SessionStart
    /// hook that these used to require never block this mailbox.
    async fn ensure_agent(&mut self, ctx: &ActorContext<Self>) -> Result<(), String> {
        if self.agent.is_some() {
            return Ok(());
        }
        let Some(runtime) = &self.runtime else {
            return Err("no live runtime".to_string());
        };
        // Resolve the provider here (cheap registry lookup) so an unregistered
        // model fails the message fast rather than as an async run failure; the
        // agent is respawned per turn, so this still picks up live config edits.
        let provider = {
            let reg = self
                .deps
                .provider_registry
                .read()
                .map_err(|_| "provider registry lock poisoned".to_string())?;
            reg.get(&self.spec.agent.model).cloned()
        }
        .ok_or_else(|| {
            format!(
                "no provider registered for model '{}'",
                self.spec.agent.model
            )
        })?;
        // Capture the *current* runtime client: the agent is respawned per turn
        // (dropped on conclude), and `ensure_runtime` runs before each respawn,
        // so a fresh client is captured after any re-attach.
        let context_provider = Arc::new(SessionContextProvider {
            runtime_client: runtime.runtime_client.clone(),
            provider,
            mcp: self.deps.mcp.clone(),
            memory: self.deps.memory.clone(),
            settings: self.spec.agent.clone(),
            session_id: self.id,
            session: ctx.self_ref(),
            frames: self.frames.clone(),
        });
        let mut params = AgentParams::from_def(&session_run_def(&self.spec.agent));
        params.interactive = true;
        params.optional_handoff_tool = Some(ASK_USER_TOOL.to_string());
        // Resolved at session creation (session choice, else the model default),
        // so this is a straight parse of a value already validated there.
        params.thinking_effort = self
            .spec
            .agent
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        // A fresh generation for this incarnation: any outcome this agent ever
        // delivers is tagged with it, so a straggler from a *previous*
        // incarnation (superseded by this spawn) is recognized as stale.
        self.generation += 1;
        // The system prompt is composed per run from a live workspace scan by
        // `SessionContextProvider`, not baked here.
        let agent_ctx = AgentRuntimeContext {
            context_provider,
            event_sink: Arc::new(SessionEventSink {
                frames: self.frames.clone(),
            }),
            parent: Arc::new(SessionParent {
                target: ctx.self_ref(),
                generation: self.generation,
            }),
            session_id: self.id,
        };
        self.agent = Some(ctx.spawn(AgentActor::new(agent_ctx, params)));
        Ok(())
    }

    async fn wake(&mut self, ctx: &ActorContext<Self>, mode: WakeMode) -> Result<(), String> {
        self.ensure_runtime(mode).await?;
        self.ensure_agent(ctx).await
    }

    /// Answer a history query from the agent's in-memory state. If a live agent
    /// is running (a turn in flight or just finished), ask it directly for the
    /// freshest state. Otherwise spawn a transient read-only agent that recovers
    /// its conversation from the journal, answer from it, and let it stop — no
    /// runtime is touched, so viewing an idle session is cheap. The agent itself
    /// reads its own journal (recovery), so encapsulation holds: the server
    /// never touches the journal directly.
    async fn read_history(
        &self,
        query: HistoryQuery,
        ctx: &ActorContext<Self>,
    ) -> AgentHistoryPage {
        if let Some(agent) = &self.agent
            && let Ok(page) = agent
                .ask(|reply| AgentCommand::GetHistory {
                    query: query.clone(),
                    reply,
                })
                .await
        {
            return page;
        }
        // Transient reader: resources that error if a run is ever attempted
        // (it never is here), a no-op event sink, and a parent that ignores
        // outcomes. Dropped when this scope ends, stopping the actor. `interactive`
        // is set so recovery does NOT self-resume the interrupted turn — a read
        // must never attempt a run (which would fail `NoContextProvider` and emit
        // a spurious error).
        let mut reader_params = AgentParams::from_def(&session_run_def(&self.spec.agent));
        reader_params.interactive = true;
        let reader = ctx.spawn(AgentActor::new(
            AgentRuntimeContext {
                context_provider: Arc::new(NoContextProvider),
                event_sink: Arc::new(SessionEventSink {
                    frames: self.frames.clone(),
                }),
                parent: Arc::new(SessionParent {
                    target: ctx.self_ref(),
                    generation: self.generation,
                }),
                session_id: self.id,
            },
            reader_params,
        ));
        reader
            .ask(|reply| AgentCommand::GetHistory { query, reply })
            .await
            .unwrap_or(AgentHistoryPage {
                messages: Vec::new(),
                has_more: false,
                tasks: None,
                usage: None,
            })
    }

    /// Read this session's aggregated usage. Usage *totals* (session-level and
    /// per-agent) come from this session's own durable `agent_usage` — pushed
    /// by `UsageRecorded` whenever an agent completes a run — never a live
    /// ask, so summing across however many agents a session hosts never
    /// requires waking an idle one. `context_tokens`/`last_turn_usage` are
    /// the exception: context size is meaningfully live-only, so those still
    /// ask the live main agent (or a transient reader if idle), exactly like
    /// `read_history`.
    async fn read_usage(
        &self,
        state: &SessionState,
        ctx: &ActorContext<Self>,
    ) -> SessionUsageStats {
        let snapshot = if let Some(agent) = &self.agent
            && let Ok(snapshot) = agent.ask(|reply| AgentCommand::GetUsage { reply }).await
        {
            snapshot
        } else {
            let mut reader_params = AgentParams::from_def(&session_run_def(&self.spec.agent));
            reader_params.interactive = true;
            let reader = ctx.spawn(AgentActor::new(
                AgentRuntimeContext {
                    context_provider: Arc::new(NoContextProvider),
                    event_sink: Arc::new(SessionEventSink {
                        frames: self.frames.clone(),
                    }),
                    parent: Arc::new(SessionParent {
                        target: ctx.self_ref(),
                        generation: self.generation,
                    }),
                    session_id: self.id,
                },
                reader_params,
            ));
            reader
                .ask(|reply| AgentCommand::GetUsage { reply })
                .await
                .unwrap_or_default()
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

    /// Start a fresh turn with `text` and reply to the caller.
    async fn start_turn(
        &mut self,
        text: String,
        reply: oneshot::Sender<Result<(), UserMessageError>>,
    ) -> CommandEffect<SessionDomainEvent> {
        if let Some(agent) = &self.agent {
            let _ = agent.tell(AgentCommand::Run { input: text }).await;
        }
        let _ = reply.send(Ok(()));
        self.report(SessionStatus::Running).await;
        CommandEffect::persist(vec![SessionDomainEvent::TurnStarted])
    }

    async fn on_user_message(
        &mut self,
        state: &SessionState,
        text: String,
        reply: oneshot::Sender<Result<(), UserMessageError>>,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        // An unnamed session is titled from its first message, once — like
        // other chat products. A caller-supplied name at creation starts as
        // the title, but can still be replaced later by set_session_title.
        if self.spec.name.is_none()
            && let Some(title) = derive_title(&text)
            && let Err(error) = self.rename_session(title).await
        {
            tracing::warn!(session = %self.id, error, "failed to persist fallback session title");
        }

        match state.status.clone() {
            Some(SessionStatus::Running) => {
                let _ = reply.send(Err(UserMessageError::TurnInFlight));
                CommandEffect::none()
            }
            // Answer a pending ask. Keyed on `pending_ask` rather than on the
            // status so the resume stays idempotent: a crash mid-resume leaves
            // the status reconciled to `Interrupted`, and this branch must still
            // answer the ask rather than start a fresh turn.
            _ if state.pending_ask.is_some() => {
                let tool_call_id = state.pending_ask.clone().unwrap_or_default();
                match self.wake(ctx, WakeMode::Attach).await {
                    Ok(()) => {
                        if let Some(agent) = &self.agent {
                            let _ = agent
                                .tell(AgentCommand::InjectToolResult {
                                    tool_call_id,
                                    content: text,
                                })
                                .await;
                        }
                        let _ = reply.send(Ok(()));
                        // The resumed turn is a running turn. Without this the
                        // session stayed `AwaitingInput` for its whole duration,
                        // the composer stayed enabled, and a second answer started
                        // a concurrent run on the same journal (#61 item 3).
                        self.report(SessionStatus::Running).await;
                        CommandEffect::persist(vec![SessionDomainEvent::AskAnswered])
                    }
                    Err(e) => {
                        let _ = reply.send(Err(UserMessageError::RecoveryFailed(e.clone())));
                        self.report(SessionStatus::RecoveryFailed { reason: e.clone() })
                            .await;
                        CommandEffect::persist(vec![SessionDomainEvent::AttachFailed { error: e }])
                    }
                }
            }
            // Never provisioned (or provisioning went stale across a restart, or
            // failed): make it run by provisioning fresh.
            None | Some(SessionStatus::Provisioning) | Some(SessionStatus::Failed { .. }) => {
                match self.wake(ctx, WakeMode::Create).await {
                    Ok(()) => self.start_turn(text, reply).await,
                    Err(e) => {
                        let _ = reply.send(Err(UserMessageError::RecoveryFailed(e.clone())));
                        self.report(SessionStatus::Failed { reason: e.clone() })
                            .await;
                        CommandEffect::persist(vec![SessionDomainEvent::ProvisionFailed {
                            error: e,
                        }])
                    }
                }
            }
            // Idle/Stopped/Interrupted/RecoveryFailed (and AwaitingInput with no
            // recorded ask id): revive preserved state and run the turn.
            Some(SessionStatus::Idle)
            | Some(SessionStatus::Stopped)
            | Some(SessionStatus::Interrupted)
            | Some(SessionStatus::RecoveryFailed { .. })
            | Some(SessionStatus::AwaitingInput) => match self.wake(ctx, WakeMode::Attach).await {
                Ok(()) => self.start_turn(text, reply).await,
                Err(e) => {
                    let _ = reply.send(Err(UserMessageError::RecoveryFailed(e.clone())));
                    self.report(SessionStatus::RecoveryFailed { reason: e.clone() })
                        .await;
                    CommandEffect::persist(vec![SessionDomainEvent::AttachFailed { error: e }])
                }
            },
        }
    }

    async fn on_agent_outcome(
        &mut self,
        outcome: AgentOutcome,
        generation: u64,
    ) -> CommandEffect<SessionDomainEvent> {
        // Usage is always recorded regardless of generation: the tokens were
        // actually spent even if the turn that spent them was later cancelled
        // or superseded, so accounting must not silently drop it.
        if let AgentOutcome::UsageRecorded { usage_total, .. } = outcome {
            return CommandEffect::persist(vec![SessionDomainEvent::UsageRecorded {
                agent_id: MAIN_AGENT_ID.to_string(),
                usage_total,
            }]);
        }
        if generation != self.generation {
            // Stale: this outcome belongs to an agent incarnation that Stop
            // or a fresh user message has already superseded. Its transcript
            // (thinking/text/tool calls) was already persisted incrementally
            // via `SessionEventSink` as it happened; only the terminal status
            // transition is dropped here, so it can't clobber whatever the
            // session is doing now (e.g. a newly-started turn).
            return CommandEffect::none();
        }
        match outcome {
            AgentOutcome::UsageRecorded { .. } => unreachable!("handled above"),
            AgentOutcome::Concluded { .. } => {
                // The agent actor stopped itself; a later turn respawns it and
                // recovers the conversation from the journal.
                self.agent = None;
                self.report(SessionStatus::Idle).await;
                CommandEffect::persist(vec![SessionDomainEvent::TurnCompleted])
            }
            AgentOutcome::Asked {
                tool_call_id,
                question,
                ..
            } => {
                self.report(SessionStatus::AwaitingInput).await;
                CommandEffect::persist(vec![SessionDomainEvent::Asked {
                    tool_call_id,
                    question,
                }])
            }
            AgentOutcome::Failed { error, .. } => {
                self.agent = None;
                let _ = self.frames.send(SessionFrame::Error {
                    message: error.clone(),
                });
                // A failed turn never bricks the session: back to Idle with the
                // error recorded; the user just sends another message.
                self.report(SessionStatus::Idle).await;
                CommandEffect::persist(vec![SessionDomainEvent::TurnFailed { error }])
            }
            AgentOutcome::Parked { .. } => {
                // Sessions run with timers off, so a park should be impossible.
                let error = "agent parked; timers are not supported in sessions".to_string();
                let _ = self.frames.send(SessionFrame::Error {
                    message: error.clone(),
                });
                self.report(SessionStatus::Idle).await;
                CommandEffect::persist(vec![SessionDomainEvent::TurnFailed { error }])
            }
        }
    }

    /// Cancel any in-flight turn and stop the runtime (preserving it). Bumps
    /// the generation unconditionally: whatever outcome the halted agent
    /// (if any) eventually delivers is tagged with the *old* generation, so
    /// `on_agent_outcome` recognizes it as stale even if no replacement agent
    /// has been spawned yet.
    /// Waits (bounded by [`HALT_CANCEL_TIMEOUT`]) for the cancelled run to
    /// actually finish, so a replacement agent is not spawned onto the same
    /// journal while the old one can still append to it. Cancellation is prompt
    /// — an in-flight LLM call or tool batch is aborted, not waited out — so
    /// this normally returns in milliseconds; the timeout is a backstop for a
    /// wedged run, after which the generation fence still keeps any straggler
    /// outcome from taking effect.
    async fn halt(&mut self) {
        self.generation += 1;
        if let Some(agent) = &self.agent {
            // Tell the sandbox to abandon whatever it is running before waiting on
            // the agent. Dropping a tool future abandons it locally only, so
            // without this a Stop mid-`bash` left the command running to
            // completion inside the runtime, holding resources, with its output
            // discarded (#61 item 23). Doing it first also means the in-flight
            // call is already cancelled while we wait out `HALT_CANCEL_TIMEOUT`.
            if let Some(runtime) = &self.runtime {
                runtime.runtime_client.cancel_in_flight().await;
            }
            let (tx, rx) = oneshot::channel();
            let _ = agent.tell(AgentCommand::Cancel { ack: Some(tx) }).await;
            if tokio::time::timeout(HALT_CANCEL_TIMEOUT, rx).await.is_err() {
                tracing::warn!(
                    session = %self.id,
                    "cancelled run did not finish within {HALT_CANCEL_TIMEOUT:?}; \
                     proceeding (stale outcomes are fenced by generation)"
                );
            }
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.handle.hibernate().await;
        }
        self.agent = None;
    }
}

/// Adapts the session's mailbox to the [`AgentOutcomeSink`] its agent reports
/// to, tagging every delivery with the generation active when the agent was
/// spawned (see [`SessionActor::generation`]).
struct SessionParent {
    target: ActorRef<SessionCommand>,
    generation: u64,
}

#[async_trait]
impl AgentOutcomeSink for SessionParent {
    async fn deliver(&self, outcome: AgentOutcome) {
        let _ = self
            .target
            .tell(SessionCommand::AgentOutcome(outcome, self.generation))
            .await;
    }
}

/// The interactive session's `AgentRunDef`. A session is not a workflow graph
/// node -- no `name`/`model`/`transitions` -- so it builds a run def directly.
/// `allow_ask_user` stays `false`: sessions get their own always-available
/// `ask_user` tool instead of the workflow crate's `conclude`-based ask.
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

/// Layer the memory tools onto `base` and render the prompt index, for a
/// session's selected spaces. Factored out of `provide()` so both halves of the
/// decision are testable without standing up a session.
///
/// Returns `(base, "")` unchanged when the session selected no spaces, or when
/// it named spaces but no memory service is wired -- the tools and the index are
/// offered together or not at all, so the agent is never told about memories it
/// has no way to read.
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

/// Context provider for a transient read-only agent (history queries): it never
/// runs, so `provide` must never be called; it errors defensively if it ever is.
struct NoContextProvider;

#[async_trait]
impl ContextProvider for NoContextProvider {
    async fn provide(&self) -> Result<Contexts, String> {
        Err("read-only agent cannot run".to_string())
    }
}

/// Per-run context provider for an interactive session's agent. Its `provide`
/// runs on the agent run's own task (never the session mailbox): it resolves the
/// provider live, scans the workspace + runs SessionStart, connects the enabled
/// MCP servers, and composes the system prompt -- the sandbox round-trips that
/// used to block `ensure_agent` on the mailbox. Idempotent per run, so a live
/// runtime/MCP makes it cheap.
struct SessionContextProvider {
    runtime_client: RuntimeClient,
    provider: Arc<dyn LlmProvider>,
    mcp: Option<Arc<crate::mcp::McpService>>,
    memory: Option<Arc<crate::memory::MemoryService>>,
    settings: AgentSettings,
    session_id: Uuid,
    /// The owning session's mailbox — routes the server-owned title tool.
    session: ActorRef<SessionCommand>,
    /// Live frame stream — `ensure` emits preparation progressions onto it.
    frames: broadcast::Sender<SessionFrame>,
}

#[async_trait]
impl ContextProvider for SessionContextProvider {
    async fn provide(&self) -> Result<Contexts, String> {
        let settings = &self.settings;
        let provider = self.provider.clone();
        let def = session_run_def(settings);
        let use_plugins = settings.use_plugins.unwrap_or(true);
        emit_progress(&self.frames, "scanning_workspace", None);
        let (ws, shared_scan) = scan_workspace(&self.runtime_client, None, use_plugins).await;
        let shared = if use_plugins {
            let bootstrap = match self.runtime_client.run_session_start().await {
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
        // Connect the session's enabled MCP servers and expose their tools next
        // to the runtime tools (subject to the same allowlist).
        let mcp: Vec<Arc<dyn Toolbox>> = if settings.mcp_servers.is_empty() {
            Vec::new()
        } else if let Some(mcp_svc) = self.mcp.as_ref() {
            emit_progress(&self.frames, "connecting_tools", None);
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
            self.runtime_client.clone(),
            ws.names(),
            use_plugins,
            mcp,
        );
        let (with_memory, memory_index) =
            build_memory_layer(base, self.memory.clone(), settings).await?;
        // `AskUserToolbox` wraps the composed tools: `ask_user` is terminal and
        // the run looks it up by name via `params.optional_handoff_tool`.
        let inner: Arc<dyn Toolbox> = Arc::new(AskUserToolbox::new(with_memory));
        // `SessionTitleToolbox` is outermost: it delegates every other name, so
        // the handoff lookup above still reaches `ask_user`.
        let toolbox: Arc<dyn Toolbox> =
            Arc::new(SessionTitleToolbox::new(inner, self.session.clone()));
        let system_prompt = compose_system_prompt(Some(SESSION_AGENT_PROMPT), &ws, shared.as_ref());
        let system_prompt = match (system_prompt, memory_index.is_empty()) {
            (Some(p), false) => Some(format!("{p}\n\n{memory_index}")),
            (Some(p), true) => Some(p),
            (None, false) => Some(memory_index),
            (None, true) => None,
        };
        emit_progress(&self.frames, "ready", None);
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
            SessionDomainEvent::Provisioned => state.status = Some(SessionStatus::Idle),
            SessionDomainEvent::ProvisionFailed { error } => {
                state.status = Some(SessionStatus::Failed {
                    reason: error.clone(),
                });
                state.last_error = Some(error);
            }
            SessionDomainEvent::TurnStarted => {
                state.status = Some(SessionStatus::Running);
                state.pending_ask = None;
                state.pending_question = None;
                // The previous turn's failure is history once a new turn is
                // under way; leaving it set makes the detail endpoint report a
                // stale error for the rest of the session's life.
                state.last_error = None;
            }
            SessionDomainEvent::AskAnswered => {
                state.status = Some(SessionStatus::Running);
                // `pending_ask` deliberately survives; see the variant's docs.
                state.pending_question = None;
            }
            SessionDomainEvent::TurnCompleted => {
                state.status = Some(SessionStatus::Idle);
                state.pending_ask = None;
                state.pending_question = None;
            }
            SessionDomainEvent::TurnFailed { error } => {
                state.status = Some(SessionStatus::Idle);
                state.last_error = Some(error);
            }
            SessionDomainEvent::Asked {
                tool_call_id,
                question,
            } => {
                state.status = Some(SessionStatus::AwaitingInput);
                state.pending_ask = tool_call_id;
                state.pending_question = Some(question);
            }
            SessionDomainEvent::Interrupted => state.status = Some(SessionStatus::Interrupted),
            SessionDomainEvent::AttachFailed { error } => {
                state.status = Some(SessionStatus::RecoveryFailed {
                    reason: error.clone(),
                });
                state.last_error = Some(error);
            }
            SessionDomainEvent::Stopped => state.status = Some(SessionStatus::Stopped),
            SessionDomainEvent::Deleted => {}
            SessionDomainEvent::UsageRecorded {
                agent_id,
                usage_total,
            } => {
                state.agent_usage.insert(agent_id, usage_total);
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
            SessionCommand::Provision => match self.ensure_runtime(WakeMode::Create).await {
                Ok(()) => {
                    self.report(SessionStatus::Idle).await;
                    CommandEffect::persist(vec![SessionDomainEvent::Provisioned])
                }
                Err(e) => {
                    self.report(SessionStatus::Failed { reason: e.clone() })
                        .await;
                    CommandEffect::persist(vec![SessionDomainEvent::ProvisionFailed { error: e }])
                }
            },
            SessionCommand::UserMessage { text, reply } => {
                self.on_user_message(state, text, reply, ctx).await
            }
            SessionCommand::Stop { reply } => {
                if state.status == Some(SessionStatus::Stopped) {
                    let _ = reply.send(());
                    return CommandEffect::none();
                }
                self.halt().await;
                let _ = reply.send(());
                self.report(SessionStatus::Stopped).await;
                CommandEffect::persist(vec![SessionDomainEvent::Stopped])
            }
            SessionCommand::Delete { reply } => {
                self.halt().await;
                if let Ok(vendor) = self.vendor() {
                    vendor.delete(&self.id.to_string()).await;
                }
                let _ = reply.send(());
                // No status report: the supervisor removes the registry row.
                CommandEffect::persist_and_stop(vec![SessionDomainEvent::Deleted])
            }
            SessionCommand::Subscribe { reply } => {
                let _ = reply.send(self.frames.subscribe());
                CommandEffect::none()
            }
            SessionCommand::History { query, reply } => {
                let page = self.read_history(query, ctx).await;
                let _ = reply.send(page);
                CommandEffect::none()
            }
            SessionCommand::UsageStats { reply } => {
                let stats = self.read_usage(state, ctx).await;
                let _ = reply.send(stats);
                CommandEffect::none()
            }
            SessionCommand::Shutdown { reply } => {
                self.halt().await;
                let _ = reply.send(());
                // No status persisted: a Running session reconciles to
                // Interrupted on the next start.
                CommandEffect::stop()
            }
            SessionCommand::AgentOutcome(outcome, generation) => {
                self.on_agent_outcome(outcome, generation).await
            }
            SessionCommand::ReconcileInterrupted => {
                if state.status == Some(SessionStatus::Running) {
                    self.report(SessionStatus::Interrupted).await;
                    CommandEffect::persist(vec![SessionDomainEvent::Interrupted])
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
        }
    }

    /// Lazy recovery: no vendor calls, no agent spawn. Only reconcile a
    /// mid-turn crash so the session list is immediately honest.
    async fn on_recovery_complete(&mut self, state: &SessionState, ctx: &mut ActorContext<Self>) {
        if state.status == Some(SessionStatus::Running) {
            let _ = ctx
                .self_ref()
                .tell(SessionCommand::ReconcileInterrupted)
                .await;
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
    use crate::runtime_vendor::fake::{FakeRuntimeVendor, FakeRuntimeVendorBuilder};
    use crate::sessions::spec::AgentSettings;
    use horsie_actor::{InMemoryJournal, Journal, spawn_root};
    use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};
    use std::collections::HashMap;

    /// A trivial supervisor stand-in that records status, rename, and publish
    /// commands on channels.
    struct NullSupervisor {
        statuses: tokio::sync::mpsc::UnboundedSender<SessionStatus>,
        names: tokio::sync::mpsc::UnboundedSender<String>,
        published_titles: tokio::sync::mpsc::UnboundedSender<String>,
    }

    #[derive(Serialize, Deserialize, Default)]
    struct Empty {}

    #[async_trait]
    impl EventSourcedActor for NullSupervisor {
        type Command = SessionSupervisorCommand;
        type Event = ();
        type State = Empty;

        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("null-supervisor", "test")
        }
        fn initial_state() -> Empty {
            Empty {}
        }
        fn apply_event(state: Empty, _e: ()) -> Empty {
            state
        }
        async fn handle_command(
            &mut self,
            _state: &Empty,
            cmd: SessionSupervisorCommand,
            _ctx: &mut ActorContext<Self>,
        ) -> CommandEffect<()> {
            match cmd {
                SessionSupervisorCommand::SessionStatusChanged { status, .. } => {
                    let _ = self.statuses.send(status);
                }
                SessionSupervisorCommand::RenameSession { name, reply, .. } => {
                    let _ = self.names.send(name);
                    let _ = reply.send(Ok(()));
                }
                SessionSupervisorCommand::PublishSessionTitle { name, .. } => {
                    let _ = self.published_titles.send(name);
                }
                _ => {}
            }
            CommandEffect::none()
        }
    }

    async fn test_memory_service() -> (Arc<crate::memory::MemoryService>, tempfile::TempDir) {
        use std::str::FromStr;
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (
            Arc::new(crate::memory::MemoryService::new(
                crate::memory::MemoryStore::new(pool),
            )),
            tmp,
        )
    }

    fn settings_with_spaces(spaces: &[&str]) -> AgentSettings {
        AgentSettings {
            model: "mock".into(),
            allowed_tools: None,
            use_plugins: None,
            max_iterations: None,
            max_retries: 0,
            mcp_servers: Vec::new(),
            memory_spaces: spaces.iter().map(|s| (*s).to_string()).collect(),
            thinking_effort: None,
        }
    }

    #[tokio::test]
    async fn memory_index_and_tools_are_absent_when_no_space_is_selected() {
        let (svc, _tmp) = test_memory_service().await;
        let settings = settings_with_spaces(&[]);
        let base: Arc<dyn Toolbox> = Arc::new(horsie_agentcore::EmptyToolbox);

        let (toolbox, index) = build_memory_layer(base, Some(svc), &settings)
            .await
            .unwrap();
        assert!(index.is_empty());
        assert!(toolbox.specs().is_empty());
    }

    #[tokio::test]
    async fn memory_index_and_tools_appear_when_a_space_is_selected() {
        let (svc, _tmp) = test_memory_service().await;
        svc.create_memory(horsie_models::memory::MemoryCreateInput {
            space: "default".into(),
            name: "alpha".into(),
            description: "a durable fact".into(),
            content: "body".into(),
        })
        .await
        .unwrap();
        let settings = settings_with_spaces(&["default"]);
        let base: Arc<dyn Toolbox> = Arc::new(horsie_agentcore::EmptyToolbox);

        let (toolbox, index) = build_memory_layer(base, Some(svc), &settings)
            .await
            .unwrap();
        assert!(index.contains("- default/alpha — a durable fact"));
        let names: Vec<String> = toolbox.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"memory_create".to_string()));
    }

    #[tokio::test]
    async fn spaces_selected_with_no_service_wired_degrade_to_nothing() {
        let settings = settings_with_spaces(&["default"]);
        let base: Arc<dyn Toolbox> = Arc::new(horsie_agentcore::EmptyToolbox);

        let (toolbox, index) = build_memory_layer(base, None, &settings).await.unwrap();
        assert!(index.is_empty());
        assert!(toolbox.specs().is_empty());
    }

    fn spec_fixture(vendor: &str) -> SessionSpec {
        SessionSpec {
            name: None,
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
            vendor: vendor.into(),
            plugins: vec![],
        }
    }

    struct Harness {
        actor: ActorRef<SessionCommand>,
        vendor: FakeRuntimeVendor,
        statuses: tokio::sync::mpsc::UnboundedReceiver<SessionStatus>,
        names: tokio::sync::mpsc::UnboundedReceiver<String>,
        published_titles: tokio::sync::mpsc::UnboundedReceiver<String>,
        id: Uuid,
        _tmp: tempfile::TempDir,
    }

    /// A fake agent under the vendor name the fixtures select. Every harness
    /// goes through a real WebSocket and the real `runtime_vendor.fl` codec — there is
    /// no in-process vendor double any more, so a test that passes here
    /// exercises the same path production takes.
    fn agent() -> FakeRuntimeVendorBuilder {
        FakeRuntimeVendor::builder("mock")
    }

    async fn harness_on(journal: Arc<dyn Journal>, vendor: FakeRuntimeVendorBuilder) -> Harness {
        harness_with_id(journal, vendor, Uuid::new_v4()).await
    }

    async fn harness_with_id(
        journal: Arc<dyn Journal>,
        vendor: FakeRuntimeVendorBuilder,
        id: Uuid,
    ) -> Harness {
        harness_custom(journal, vendor, id, spec_fixture("mock"), None).await
    }

    async fn harness_custom(
        journal: Arc<dyn Journal>,
        vendor: FakeRuntimeVendorBuilder,
        id: Uuid,
        spec: SessionSpec,
        github_tokens: Option<Arc<dyn crate::github::GithubTokenMinter>>,
    ) -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = vendor.serve_in_process().await.expect("fake agent");
        let mut vendors: HashMap<String, Arc<crate::runtime_vendor::RuntimeVendorLink>> =
            HashMap::new();
        vendors.insert("mock".into(), vendor.link());
        let vendors = Arc::new(std::sync::RwLock::new(vendors));
        let deps = ServerDeps {
            runtimes: crate::runtime_manager::test_runtime_manager(&vendors, tmp.path()),
            provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
            vendors,
            state_dir: tmp.path().to_path_buf(),
            github_tokens,
            mcp: None,
            plugins: None,
            memory: None,
        };
        let (status_tx, status_rx) = tokio::sync::mpsc::unbounded_channel();
        let (names_tx, names_rx) = tokio::sync::mpsc::unbounded_channel();
        let (titles_tx, titles_rx) = tokio::sync::mpsc::unbounded_channel();
        let parent = spawn_root(
            NullSupervisor {
                statuses: status_tx,
                names: names_tx,
                published_titles: titles_tx,
            },
            journal.clone(),
        );
        let actor = spawn_root(SessionActor::new(id, spec, deps, parent), journal);
        Harness {
            actor,
            vendor,
            statuses: status_rx,
            names: names_rx,
            published_titles: titles_rx,
            id,
            _tmp: tmp,
        }
    }

    #[test]
    fn fold_covers_all_transitions() {
        use SessionDomainEvent as E;
        let s = SessionActor::apply_event(SessionState::default(), E::Provisioned);
        assert_eq!(s.status, Some(SessionStatus::Idle));
        let s = SessionActor::apply_event(s, E::TurnStarted);
        assert_eq!(s.status, Some(SessionStatus::Running));
        let s = SessionActor::apply_event(
            s,
            E::Asked {
                tool_call_id: Some("tc".into()),
                question: "q?".into(),
            },
        );
        assert_eq!(s.status, Some(SessionStatus::AwaitingInput));
        assert_eq!(s.pending_ask.as_deref(), Some("tc"));
        assert_eq!(s.pending_question.as_deref(), Some("q?"));
        let s = SessionActor::apply_event(s, E::TurnCompleted);
        assert_eq!(s.status, Some(SessionStatus::Idle));
        assert_eq!(s.pending_ask, None);
        let s = SessionActor::apply_event(s, E::Interrupted);
        assert_eq!(s.status, Some(SessionStatus::Interrupted));
        let s = SessionActor::apply_event(
            s,
            E::AttachFailed {
                error: "gone".into(),
            },
        );
        assert!(matches!(
            s.status,
            Some(SessionStatus::RecoveryFailed { .. })
        ));
        assert_eq!(s.last_error.as_deref(), Some("gone"));
        let s = SessionActor::apply_event(s, E::Stopped);
        assert_eq!(s.status, Some(SessionStatus::Stopped));
        let s = SessionActor::apply_event(
            s,
            E::TurnFailed {
                error: "boom".into(),
            },
        );
        assert_eq!(s.status, Some(SessionStatus::Idle));
        assert_eq!(s.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn a_new_turn_clears_the_previous_failure() {
        use SessionDomainEvent as E;
        let s = SessionActor::apply_event(
            SessionState::default(),
            E::TurnFailed {
                error: "boom".into(),
            },
        );
        assert_eq!(s.last_error.as_deref(), Some("boom"));
        // The detail endpoint reports `last_error`, so a turn that has just
        // started must not still be advertising the previous turn's failure.
        let s = SessionActor::apply_event(s, E::TurnStarted);
        assert_eq!(s.last_error, None);
    }

    #[test]
    fn derive_title_uses_trimmed_first_line() {
        assert_eq!(
            derive_title("what's the project about?").as_deref(),
            Some("what's the project about?")
        );
        assert_eq!(
            derive_title("  fix the login bug  \nmore detail here").as_deref(),
            Some("fix the login bug")
        );
        assert_eq!(derive_title("   \n\n  ").as_deref(), None);
        assert_eq!(derive_title("").as_deref(), None);
        let long = "x".repeat(80);
        let title = derive_title(&long).unwrap();
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1); // +1 for the ellipsis
        assert!(title.ends_with('…'));
    }

    #[tokio::test]
    async fn first_user_message_still_derives_a_fallback_title() {
        let mut h = harness_on(Arc::new(InMemoryJournal::new()), agent()).await;

        let result = h
            .actor
            .ask(|reply| SessionCommand::UserMessage {
                text: "  fix the login redirect  \nwith details".into(),
                reply,
            })
            .await
            .unwrap();

        // The test harness has no provider registered, so the turn itself fails
        // after the title is named. This assertion is about the fallback title.
        assert!(matches!(result, Err(UserMessageError::RecoveryFailed(_))));
        assert_eq!(h.names.recv().await.unwrap(), "fix the login redirect");
        assert_eq!(
            h.published_titles.recv().await.unwrap(),
            "fix the login redirect"
        );
    }

    #[test]
    fn system_prompt_instructs_the_agent_to_title_the_session() {
        assert!(SESSION_AGENT_PROMPT.contains("## Session title"));
        assert!(SESSION_AGENT_PROMPT.contains("set_session_title"));
        assert!(SESSION_AGENT_PROMPT.contains("first turn"));
        assert!(SESSION_AGENT_PROMPT.contains("latest successful call wins"));
    }

    #[tokio::test]
    async fn set_session_title_replaces_a_creation_name() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let mut spec = spec_fixture("mock");
        spec.name = Some("Creation name".into());
        let mut h = harness_custom(journal, agent(), Uuid::new_v4(), spec, None).await;

        let first = h
            .actor
            .ask(|reply| SessionCommand::SetSessionTitle {
                title: "  Better model title  ".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, "Better model title");
        assert_eq!(h.names.recv().await.unwrap(), "Better model title");
        assert_eq!(
            h.published_titles.recv().await.unwrap(),
            "Better model title"
        );

        let latest = h
            .actor
            .ask(|reply| SessionCommand::SetSessionTitle {
                title: "Latest title wins".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest, "Latest title wins");
        assert_eq!(h.names.recv().await.unwrap(), "Latest title wins");
        assert_eq!(
            h.published_titles.recv().await.unwrap(),
            "Latest title wins"
        );
    }

    #[tokio::test]
    async fn set_session_title_rejects_invalid_titles_without_renaming() {
        let mut h = harness_on(Arc::new(InMemoryJournal::new()), agent()).await;

        let too_long = "é".repeat(61);
        for title in ["   ", "one\ntwo", too_long.as_str()] {
            let error = h
                .actor
                .ask(|reply| SessionCommand::SetSessionTitle {
                    title: title.to_string(),
                    reply,
                })
                .await
                .unwrap()
                .unwrap_err();
            assert!(!error.is_empty());
            assert!(h.names.try_recv().is_err());
            assert!(h.published_titles.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn provision_emits_create_signal_and_stop_preserves() {
        let mut h = harness_on(Arc::new(InMemoryJournal::new()), agent()).await;
        h.actor.tell(SessionCommand::Provision).await.unwrap();
        h.actor
            .ask(|reply| SessionCommand::Stop { reply })
            .await
            .unwrap();
        let sid = h.id.to_string();
        assert_eq!(
            h.vendor.signals(),
            vec![format!("create:{sid}"), format!("hibernate:{sid}")]
        );
        // Status reports arrived in order: Idle (provisioned) then Stopped.
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Idle);
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Stopped);
    }

    struct FixedMinter(Option<String>);
    #[async_trait]
    impl crate::github::GithubTokenMinter for FixedMinter {
        async fn mint_for(&self, _repo_urls: &[String]) -> Result<Option<String>, String> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn provision_mints_github_token_into_env() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let mut spec = spec_fixture("mock");
        spec.provision = vec![crate::sessions::spec::ProvisionStepSpec {
            name: "checkout api".into(),
            uses: "git_checkout".into(),
            with: vec![
                ("url".into(), "https://github.com/o/api".into()),
                ("dir".into(), "api".into()),
            ],
        }];
        let mut h = harness_custom(
            journal,
            agent(),
            Uuid::new_v4(),
            spec,
            Some(Arc::new(FixedMinter(Some("ghs_x".into())))),
        )
        .await;
        h.actor.tell(SessionCommand::Provision).await.unwrap();
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Idle);
        // Assert on what actually crossed the wire, not on a server-side
        // struct: the request the agent received is the real contract.
        let request = h
            .vendor
            .last_create_request()
            .expect("the agent saw a create request");
        assert!(
            request
                .env
                .iter()
                .any(|e| e.name == horsie_models::ENV_GITHUB_TOKEN && e.value == "ghs_x"),
            "GITHUB_TOKEN injected: {:?}",
            request.env
        );
    }

    #[tokio::test]
    async fn delete_signals_vendor_discretion() {
        let mut h = harness_on(Arc::new(InMemoryJournal::new()), agent()).await;
        h.actor.tell(SessionCommand::Provision).await.unwrap();
        h.actor
            .ask(|reply| SessionCommand::Delete { reply })
            .await
            .unwrap();
        let sid = h.id.to_string();
        assert_eq!(
            h.vendor.signals(),
            vec![
                format!("create:{sid}"),
                format!("hibernate:{sid}"),
                format!("delete:{sid}")
            ]
        );
        // Only the provisioned Idle was reported; delete removes rather than reports.
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Idle);
    }

    #[tokio::test]
    async fn provision_failure_lands_failed_status() {
        let mut h = harness_on(Arc::new(InMemoryJournal::new()), agent().fail_create()).await;
        h.actor.tell(SessionCommand::Provision).await.unwrap();
        match h.statuses.recv().await.unwrap() {
            SessionStatus::Failed { reason } => assert!(reason.contains("create failed")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recovery_reconciles_running_to_interrupted_without_vendor_calls() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let id = Uuid::new_v4();
        // Simulate a mid-turn crash: the previous incarnation journaled
        // Provisioned + TurnStarted and then died.
        let pid = SessionActor::persistence_id_for(id);
        let events = vec![
            serde_json::to_vec(&SessionDomainEvent::Provisioned).unwrap(),
            serde_json::to_vec(&SessionDomainEvent::TurnStarted).unwrap(),
        ];
        journal.persist(&pid, &events).await.unwrap();

        let mut h = harness_with_id(journal, agent(), id).await;
        // Recovery reconciles to Interrupted...
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Interrupted);
        // ...without any vendor signal (lazy recovery).
        assert!(h.vendor.signals().is_empty());
    }

    #[tokio::test]
    async fn message_on_recovered_session_fails_visibly_when_the_runtime_is_gone() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let id = Uuid::new_v4();
        let pid = SessionActor::persistence_id_for(id);
        let events = vec![serde_json::to_vec(&SessionDomainEvent::Provisioned).unwrap()];
        journal.persist(&pid, &events).await.unwrap();

        let mut h = harness_with_id(journal, agent().gone_on_get(true), id).await;
        // Idle after recovery; a message triggers attach, which fails once.
        let res = h
            .actor
            .ask(|reply| SessionCommand::UserMessage {
                text: "hi".into(),
                reply,
            })
            .await
            .unwrap();
        assert!(matches!(res, Err(UserMessageError::RecoveryFailed(_))));
        match h.statuses.recv().await.unwrap() {
            SessionStatus::RecoveryFailed { reason } => {
                assert!(reason.contains("runtime is gone"));
            }
            other => panic!("expected RecoveryFailed, got {other:?}"),
        }
        let sid = h.id.to_string();
        assert_eq!(h.vendor.signals(), vec![format!("get:{sid}")]);
    }

    #[tokio::test]
    async fn message_while_running_conflicts() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let id = Uuid::new_v4();
        let pid = SessionActor::persistence_id_for(id);
        let events = vec![
            serde_json::to_vec(&SessionDomainEvent::Provisioned).unwrap(),
            serde_json::to_vec(&SessionDomainEvent::TurnStarted).unwrap(),
        ];
        journal.persist(&pid, &events).await.unwrap();
        let h = harness_with_id(journal, agent(), id).await;
        // Race the reconcile: send the message before ReconcileInterrupted may
        // have processed — both orders are valid; accept either error.
        let res = h
            .actor
            .ask(|reply| SessionCommand::UserMessage {
                text: "hi".into(),
                reply,
            })
            .await
            .unwrap();
        match res {
            Err(UserMessageError::TurnInFlight) => {}
            // Reconcile won the race → Interrupted → attach path (mock succeeds).
            Ok(()) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    // --- Stop-then-resend stale-outcome race -------------------------------
    //
    // `halt()`'s `Cancel` signal is cooperative: `agentcore::Agent::run` only
    // checks it at its loop-top and just before executing a non-handoff tool
    // batch, never *after* an in-flight provider call returns (see
    // `agentcore/src/agent.rs`). So a turn that was "stopped" can still
    // finish and deliver a real `AgentOutcome` — a plain-text `Concluded`, or
    // an `Asked` from a handoff/tool call like `ask_user` — after `Stop`, or
    // even after the user's very next message has already started a new
    // turn. These tests reproduce that race deterministically (no sleeps to
    // guess timing) via `BlockingProvider`, and assert the generation
    // fencing in `on_agent_outcome` drops the stale outcome instead of
    // clobbering the session/new turn.

    use horsie_agentcore::{
        CompletionRequest, CompletionResponse, ContentPart, EventSink, LlmError, StopReason,
        TextPart, Usage,
    };

    /// A test [`LlmProvider`] whose `complete` blocks until the test releases
    /// that specific call, letting a test land a `Stop` (or a fresh message)
    /// *while* a call is genuinely in flight instead of guessing at it with
    /// sleeps.
    struct BlockingProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<CompletionResponse>>,
        entered: tokio::sync::mpsc::UnboundedSender<oneshot::Sender<()>>,
    }

    impl BlockingProvider {
        fn new(
            responses: Vec<CompletionResponse>,
        ) -> (
            Arc<Self>,
            tokio::sync::mpsc::UnboundedReceiver<oneshot::Sender<()>>,
        ) {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    responses: std::sync::Mutex::new(responses.into()),
                    entered: tx,
                }),
                rx,
            )
        }
    }

    #[async_trait]
    impl LlmProvider for BlockingProvider {
        fn model_id(&self) -> &str {
            "blocking-mock"
        }

        async fn complete(
            &self,
            _request: CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn EventSink,
        ) -> Result<CompletionResponse, LlmError> {
            let (release_tx, release_rx) = oneshot::channel();
            let _ = self.entered.send(release_tx);
            let _ = release_rx.await;
            let response = self
                .responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .expect("BlockingProvider ran out of canned responses");
            Ok(response)
        }
    }

    async fn harness_with_provider(
        vendor: FakeRuntimeVendorBuilder,
        provider: Arc<dyn LlmProvider>,
    ) -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = vendor.serve_in_process().await.expect("fake agent");
        let mut vendors: HashMap<String, Arc<crate::runtime_vendor::RuntimeVendorLink>> =
            HashMap::new();
        vendors.insert("mock".into(), vendor.link());
        let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        providers.insert("mock".into(), provider);
        let vendors = Arc::new(std::sync::RwLock::new(vendors));
        let deps = ServerDeps {
            runtimes: crate::runtime_manager::test_runtime_manager(&vendors, tmp.path()),
            provider_registry: Arc::new(std::sync::RwLock::new(providers)),
            vendors,
            state_dir: tmp.path().to_path_buf(),
            github_tokens: None,
            mcp: None,
            plugins: None,
            memory: None,
        };
        let (status_tx, status_rx) = tokio::sync::mpsc::unbounded_channel();
        let (names_tx, names_rx) = tokio::sync::mpsc::unbounded_channel();
        let (titles_tx, titles_rx) = tokio::sync::mpsc::unbounded_channel();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let parent = spawn_root(
            NullSupervisor {
                statuses: status_tx,
                names: names_tx,
                published_titles: titles_tx,
            },
            journal.clone(),
        );
        let id = Uuid::new_v4();
        let actor = spawn_root(
            SessionActor::new(id, spec_fixture("mock"), deps, parent),
            journal,
        );
        Harness {
            actor,
            vendor,
            statuses: status_rx,
            names: names_rx,
            published_titles: titles_rx,
            id,
            _tmp: tmp,
        }
    }

    fn text_response(text: &str) -> CompletionResponse {
        CompletionResponse {
            parts: vec![ContentPart::Text(TextPart { text: text.into() })],
            stop_reason: StopReason::EndTurn,
            usage: Usage::without_cache(10, 5),
        }
    }

    /// A stale usage figure, distinct from any `text_response` usage so the
    /// assertions below can tell it apart.
    fn stale_usage() -> UsageTotal {
        UsageTotal {
            input_tokens: 99,
            output_tokens: 7,
            cache_creation_tokens: None,
            cache_read_tokens: None,
        }
    }

    /// The generation the session's *first* agent runs under: `generation`
    /// starts at 0 and is bumped to 1 by that agent's spawn. Once Stop and a
    /// resend have moved the session on, an outcome still tagged with it is by
    /// definition stale.
    const FIRST_TURN_GENERATION: u64 = 1;

    /// Drives a session to "turn 1 stopped mid-flight, turn 2 live", returning
    /// the harness and the release handle for turn 2's blocked LLM call.
    async fn stopped_then_resent(
        h: &mut Harness,
        entered: &mut tokio::sync::mpsc::UnboundedReceiver<oneshot::Sender<()>>,
    ) -> oneshot::Sender<()> {
        h.actor
            .ask(|reply| SessionCommand::UserMessage {
                text: "hi".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Running);
        let _turn1 = entered.recv().await.expect("turn 1's call entered");

        h.actor
            .ask(|reply| SessionCommand::Stop { reply })
            .await
            .unwrap();
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Stopped);

        let resent = h
            .actor
            .ask(|reply| SessionCommand::UserMessage {
                text: "again".into(),
                reply,
            })
            .await
            .unwrap();
        assert!(resent.is_ok(), "resend must be accepted: {resent:?}");
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Running);
        entered.recv().await.expect("turn 2's call entered")
    }

    // The generation fence. Prompt cancellation (see the two tests below) aborts
    // an in-flight call at Stop, which narrows the stale-outcome window but
    // cannot close it: an outcome delivered just before Stop is handled is
    // already sitting in the session's mailbox, and no amount of cancellation
    // recalls it. These tests inject exactly that -- an outcome tagged with the
    // superseded turn's generation -- and assert it cannot disturb the live turn.

    #[tokio::test]
    async fn stale_concluded_outcome_is_dropped() {
        let (provider, mut entered) =
            BlockingProvider::new(vec![text_response("turn one"), text_response("turn two")]);
        let mut h = harness_with_provider(agent(), provider).await;
        let release2 = stopped_then_resent(&mut h, &mut entered).await;

        // Turn 1's outcome lands late. Usage is applied regardless of generation
        // (the tokens were really spent); the terminal transition is not.
        h.actor
            .tell(SessionCommand::AgentOutcome(
                AgentOutcome::UsageRecorded {
                    session_id: h.id,
                    usage_total: stale_usage(),
                },
                FIRST_TURN_GENERATION,
            ))
            .await
            .unwrap();
        h.actor
            .tell(SessionCommand::AgentOutcome(
                AgentOutcome::Concluded {
                    session_id: h.id,
                    output: serde_json::Value::String("stale answer".into()),
                },
                FIRST_TURN_GENERATION,
            ))
            .await
            .unwrap();

        // One mailbox, FIFO: this `ask` is answered only after both `tell`s above
        // have been handled, so it is an exact barrier -- no polling, no sleeping.
        let stats = h
            .actor
            .ask(|reply| SessionCommand::UsageStats { reply })
            .await
            .unwrap();
        assert_eq!(
            stats.session_total.input_tokens,
            stale_usage().input_tokens,
            "a stale turn's usage must still be recorded"
        );
        assert!(
            h.statuses.try_recv().is_err(),
            "a stale Concluded must not change status"
        );

        // The live turn was never clobbered: it still completes on its own.
        let _ = release2.send(());
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Idle);
    }

    #[tokio::test]
    async fn stale_asked_outcome_is_dropped() {
        let (provider, mut entered) =
            BlockingProvider::new(vec![text_response("turn one"), text_response("turn two")]);
        let mut h = harness_with_provider(agent(), provider).await;
        let release2 = stopped_then_resent(&mut h, &mut entered).await;

        // An `ask_user` handoff from the superseded turn: applying it would strand
        // the session in AwaitingInput with a `pending_ask` naming a tool call the
        // live turn never issued -- the state that used to wedge the session.
        h.actor
            .tell(SessionCommand::AgentOutcome(
                AgentOutcome::Asked {
                    session_id: h.id,
                    tool_call_id: Some("stale-tc".into()),
                    question: "stale question?".into(),
                },
                FIRST_TURN_GENERATION,
            ))
            .await
            .unwrap();

        let _ = h
            .actor
            .ask(|reply| SessionCommand::UsageStats { reply })
            .await
            .unwrap();
        assert!(
            h.statuses.try_recv().is_err(),
            "a stale Asked must not change status or set pending_ask"
        );

        let _ = release2.send(());
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Idle);
    }

    /// Sets a flag when dropped, marking the moment an in-flight call is torn
    /// down.
    struct TeardownFlag(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for TeardownFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A provider whose call hangs until cancelled and records its own teardown,
    /// so a test can assert Stop returned *after* the run actually unwound.
    struct TeardownRecordingProvider {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        torn_down: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl LlmProvider for TeardownRecordingProvider {
        fn model_id(&self) -> &str {
            "teardown-recording"
        }
        async fn complete(
            &self,
            _request: CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn EventSink,
        ) -> Result<CompletionResponse, LlmError> {
            let _flag = TeardownFlag(self.torn_down.clone());
            let _ = self.entered.send(());
            std::future::pending().await
        }
    }

    /// Stop must not report `Stopped` until the cancelled run has actually
    /// unwound — otherwise a replacement agent could be spawned onto the same
    /// journal while the old run can still append to it.
    #[tokio::test]
    async fn stop_waits_for_the_cancelled_run_to_unwind() {
        let (tx, mut entered) = tokio::sync::mpsc::unbounded_channel();
        let torn_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut h = harness_with_provider(
            agent(),
            Arc::new(TeardownRecordingProvider {
                entered: tx,
                torn_down: Arc::clone(&torn_down),
            }),
        )
        .await;

        h.actor
            .ask(|reply| SessionCommand::UserMessage {
                text: "hi".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Running);
        entered.recv().await.expect("the LLM call is in flight");

        h.actor
            .ask(|reply| SessionCommand::Stop { reply })
            .await
            .unwrap();

        // Asserted with no intervening await: the guarantee is that `halt` already
        // waited for the run, not that teardown happens to win a race afterwards.
        assert!(
            torn_down.load(std::sync::atomic::Ordering::SeqCst),
            "Stop must wait for the cancelled run to unwind"
        );
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Stopped);
    }

    /// Stop must abort the in-flight LLM call rather than let it run to
    /// completion in the background, and must not report `Stopped` until the
    /// cancelled run has actually finished.
    #[tokio::test]
    async fn stop_aborts_the_in_flight_call_and_waits_for_the_run_to_finish() {
        let (provider, mut entered) = BlockingProvider::new(vec![text_response("never sent")]);
        let mut h = harness_with_provider(agent(), provider).await;

        h.actor
            .ask(|reply| SessionCommand::UserMessage {
                text: "hi".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Running);
        let release = entered.recv().await.expect("the LLM call is in flight");

        h.actor
            .ask(|reply| SessionCommand::Stop { reply })
            .await
            .unwrap();
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Stopped);

        // The call was aborted, not waited out: dropping the provider future
        // dropped the receiver this sender is paired with. (Before prompt
        // cancellation this send would succeed — the call was still parked in
        // the background, burning tokens.)
        assert!(
            release.send(()).is_err(),
            "Stop must abort the in-flight provider call"
        );
    }

    /// Stopping while the agent is paused on an `ask_user` question must return
    /// promptly: there is no run in flight, so the cancel ack fires immediately
    /// rather than waiting out `HALT_CANCEL_TIMEOUT`.
    #[tokio::test]
    async fn stop_while_awaiting_user_input_returns_promptly() {
        let ask = CompletionResponse {
            parts: vec![ContentPart::ToolCall(horsie_agentcore::ToolCallPart {
                id: "ask-1".into(),
                name: ASK_USER_TOOL.to_string(),
                input: serde_json::json!({"question": "which one?"}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(6, 3),
        };
        let (provider, mut entered) = BlockingProvider::new(vec![ask]);
        let mut h = harness_with_provider(agent(), provider).await;

        h.actor
            .ask(|reply| SessionCommand::UserMessage {
                text: "hi".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Running);
        let _ = entered
            .recv()
            .await
            .expect("the LLM call is in flight")
            .send(());
        assert_eq!(
            h.statuses.recv().await.unwrap(),
            SessionStatus::AwaitingInput
        );

        // Well inside HALT_CANCEL_TIMEOUT: a paused agent has no run to wind
        // down, so Stop must not block on one.
        let stopped = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            h.actor.ask(|reply| SessionCommand::Stop { reply }),
        )
        .await;
        assert!(stopped.is_ok(), "Stop must not block on a paused agent");
        assert_eq!(h.statuses.recv().await.unwrap(), SessionStatus::Stopped);
    }
}
