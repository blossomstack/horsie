//! How one turn is assembled.
//!
//! A [`SessionContextProvider`] is what an [`AgentActor`](horsie_workflow::AgentActor)
//! asks, on its own task, for everything a run needs: the runtime handle, the
//! LLM provider, the toolbox and the system prompt. It resolves them per run
//! rather than holding them, which is what lets an agent stay resident across a
//! hibernate and resume without knowing either happened.
//!
//! One type serves all three kinds of agent a session hosts — main, subagent
//! and workflow step — because they differ only in which layers they get.
//! [`SessionAgentKind`] is what decides: the session-metadata tools are
//! main-only, `conclude` is step-only, and preparation progress is broadcast
//! for everything except a subagent, which is quiet by design.

use super::CoreCommand;
use super::{AgentKey, SessionCommand, hooks::SessionHookSink};
use crate::{
    runtime_manager::{RuntimeClientProvider, RuntimeError},
    sessions::{
        ask_tool::AskUserToolbox, spawn_tool::SubAgentToolbox, spec::AgentSettings,
        subagents::SubAgentParent, title_tool::SessionTitleToolbox,
    },
};
use async_trait::async_trait;
use horsie_actor::ActorRef;
use horsie_agentcore::{LlmProvider, Toolbox};
use horsie_models::{
    hooks::HookRecord,
    runtime::{
        McpServerFailure, ServerHookEvent, SessionStartInput, SubagentStartInput,
        UserPromptExpansionInput, UserPromptSubmitInput,
    },
};
use horsie_runtime_client::RuntimeClient;
use horsie_workflow::{
    AgentRunDef, ContextError, ContextProvider, Contexts, DefaultToolboxFactory, SharedContext,
    StartTurn, ToolboxFactory, TurnPreparation, compose_system_prompt, scan_workspace,
};
use serde_json::Value;
use std::sync::{Arc, Mutex, PoisonError};
use uuid::Uuid;

/// Report turn-preparation progress into an agent's log.
///
/// Journaled rather than broadcast. It used to ride an ephemeral frame nobody
/// could ask for again, so a client that connected mid-preparation saw nothing
/// and a reload lost the sequence entirely. Four entries per turn buys a
/// preparation history that reads back like everything else.
///
/// Routed through the session's mailbox rather than straight at the agent, so
/// a caller needs only the session handle it already holds and the entry is
/// ordered against whatever else the session is doing.
async fn emit_progress(
    session: &ActorRef<SessionCommand>,
    key: AgentKey,
    stage: &str,
    detail: Option<String>,
) {
    let _ = session
        .tell(SessionCommand::Core(CoreCommand::Progress {
            key,
            stage: stage.to_string(),
            detail,
        }))
        .await;
}

/// The baseline system prompt given to every session agent.
const SESSION_AGENT_PROMPT: &str = include_str!("system_prompt.md");

/// The interactive session's `AgentRunDef`.
pub(super) fn session_run_def(settings: &AgentSettings) -> AgentRunDef {
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
pub(super) enum SessionAgentKind {
    Main,
    Sub(Uuid),
    Step(Uuid),
}

impl SessionAgentKind {
    /// The key this agent is registered under on the session. One vocabulary:
    /// what the provider knows itself as is what the session looks it up by.
    pub(super) fn agent_key(&self) -> AgentKey {
        match self {
            Self::Main => AgentKey::Main,
            Self::Sub(id) => AgentKey::Sub(*id),
            Self::Step(id) => AgentKey::Step(*id),
        }
    }
}

/// The runtime client an agent runs with. Subagents share the session's
/// sandbox but never its cwd/env bucket: the runtime keys that state by
/// agent id, so each subagent acts under its own identity.
pub(super) fn scoped_client(kind: &SessionAgentKind, client: RuntimeClient) -> RuntimeClient {
    match kind {
        SessionAgentKind::Main => client,
        // Steps share the run's sandbox — that is the point — but never its
        // cwd/env bucket: the runtime keys that state by agent id, so each acts
        // under its own identity, exactly as a subagent does.
        SessionAgentKind::Sub(id) | SessionAgentKind::Step(id) => {
            client.with_agent_id(id.to_string())
        }
    }
}

/// Appended to a subagent's system prompt: its place in the tree and how its
/// result travels. Deliberately short — the tools carry their own docs.
const SUBAGENT_PROMPT_SUFFIX: &str = "\n\n# Subagent role\n\
You are a subagent, spawned to work on one task. Your final message is your report: \
it is automatically delivered to the agent that spawned you — make it self-contained. You \
may spawn your own subagents with spawn_agent. Continue with independent work, or wait if \
none remains; do not poll subagent_status or call it repeatedly. Use subagent_status only \
when the user requests a progress update or to diagnose a suspected runtime or \
result-delivery problem. You cannot ask the user or rename the session; if you are blocked, \
report that instead.";

/// Appended to a workflow step's system prompt: what a step is, and that its
/// structured output is what decides where the run goes next. Deliberately
/// short — the `conclude` tool carries its own schema.
const STEP_PROMPT_SUFFIX: &str = "\n\n# Workflow step\n\
You are one step of a workflow, not a conversation. Your instruction and the previous \
step's result are in the message above. Finish by calling `conclude` — what you submit \
is both this step's result and what the workflow reads to decide which step runs next, \
so make it accurate and self-contained. You share one workspace with every other step: \
what you change on disk is what the next step sees. You may spawn subagents with \
spawn_agent. You cannot rename the session.";

/// Appended to an unattended session's system prompt (a routine run). It has
/// no `ask_user` tool, so the prompt says why rather than leaving the model to
/// discover a tool it was told about is missing.
const UNATTENDED_PROMPT_SUFFIX: &str = "\n\n# Unattended run\n\
This session was started by a routine, not by a person, and nobody is reading it while \
it runs. There is no ask_user tool: a question would park the run with nobody to answer \
it. Work from the instructions you were given — where they leave a choice open, make the \
reasonable one, say which you made and why, and carry on. Your final message is the \
report; make it self-contained.";

/// Per-run context for a session's agent, resolved on the run's own task.
///
/// It asks the [`RuntimeClientProvider`] for a client each run rather than
/// holding one: that is what lets the agent be resident across a hibernate and
/// resume without knowing either happened.
pub(super) struct SessionContextProvider {
    pub(super) runtimes: RuntimeClientProvider,
    pub(super) registry: crate::sessions::spec::SharedProviderRegistry,
    pub(super) mcp: Option<Arc<crate::mcp::McpService>>,
    pub(super) memory: Option<Arc<crate::memory::MemoryService>>,
    pub(super) settings: AgentSettings,
    /// A workflow step's declared output schema, which becomes the input
    /// schema of its `conclude` tool. `None` for every other kind of agent.
    pub(super) step_output_schema: Option<Value>,
    pub(super) session_id: Uuid,
    pub(super) kind: SessionAgentKind,
    /// The plugin-declared agent type this agent runs as, for a subagent that
    /// was spawned with one. The *name* only — the definition is resolved from
    /// the library scan on every `provide()`, so a subagent that outlives its
    /// plugin fails rather than running a prompt nobody can point at.
    pub(super) agent_type: Option<String>,
    /// Whether nobody is watching this session (a routine run). Decides one
    /// thing: the main agent gets no `ask_user`, and is told why.
    pub(super) unattended: bool,
    /// The owning session's mailbox — routes the server-owned tools.
    pub(super) session: ActorRef<SessionCommand>,
    /// The plugin bundles this session selected, and the library that can say
    /// what they offer. Together they answer "is `/commit` a command?" from the
    /// database, with no runtime involved — which is what lets a prompt merely
    /// *starting* with a slash cost nothing.
    pub(super) plugins: Vec<String>,
    pub(super) plugin_library: Option<Arc<dyn crate::plugins::PluginProvisioner>>,
    /// The client the most recent `provide()` resolved. Cheap to keep — cloning
    /// shares the same in-flight-call tracking — and it is what lets
    /// [`SessionActor::cancel_run`](super::SessionActor) cancel without a fresh vendor round-trip.
    pub(super) last_client: Mutex<Option<RuntimeClient>>,
}

impl SessionContextProvider {
    /// The provider for one named model, or `None` when horsie has none.
    ///
    /// Separate from [`Self::llm_provider`] because a missing model means two
    /// different things: the session's own model is a failure, while a
    /// plugin agent's is a declaration horsie cannot honour and inherits past.
    fn provider_for(&self, model: &str) -> Option<Arc<dyn LlmProvider>> {
        self.registry.read().ok()?.get(model).cloned()
    }

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
    pub(super) fn cached_client(&self) -> Option<RuntimeClient> {
        self.last_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether this agent loads the shared plugin library — and so whether any
    /// hook could possibly be declared for it.
    pub(super) fn use_plugins(&self) -> bool {
        self.settings.use_plugins.unwrap_or(true)
    }

    /// Acquire this agent's runtime handle, scoped to it. Sink-less: `provide`
    /// attaches one for the tool hooks that report themselves mid-call, while
    /// `start_hooks` returns its records to the agent, which journals them
    /// itself. A sink there would both duplicate them and race the turn they
    /// must precede.
    async fn runtime_client(&self) -> Result<RuntimeClient, ContextError> {
        let client = self.runtimes.get().await.map_err(|e| match e {
            // The one failure the session can never retry: the vendor is alive
            // and says the runtime is gone. A vendor that is merely offline
            // (`Unavailable`) says nothing about the runtime's existence.
            RuntimeError::Gone(m) => ContextError::terminal(m),
            other @ (RuntimeError::Unavailable(_) | RuntimeError::Provision(_)) => {
                ContextError::retryable(other.to_string())
            }
        })?;
        Ok(scoped_client(&self.kind, client))
    }

    /// Expand `/name` or `@name`, if this prompt is one.
    ///
    /// Returns the message to send in place of the prompt, and whatever
    /// `UserPromptExpansion` hooks produced. `None` leaves the prompt exactly
    /// as the user wrote it — which covers a message that merely begins with a
    /// slash, since an unknown name is not an error. `/etc/hosts` has to
    /// survive being sent.
    ///
    /// The catalogue is a database read: it was derived when the bundle was
    /// installed. Nothing here touches the runtime, so a prompt that turns out
    /// not to be an invocation costs one indexed lookup.
    async fn expand_invocation(
        &self,
        client: &RuntimeClient,
        prompt: &str,
    ) -> (Option<String>, Vec<HookRecord>) {
        use horsie_support::plugin::{
            catalog::{self, CatalogKind},
            commands,
        };

        if !self.use_plugins() {
            return (None, Vec::new());
        }
        let Some(library) = &self.plugin_library else {
            return (None, Vec::new());
        };
        // Cheapest test first: neither sigil, nothing to look up.
        let Some((sigil, name, args)) = commands::parse_invocation(prompt, '/')
            .map(|(n, a)| ('/', n, a))
            .or_else(|| commands::parse_invocation(prompt, '@').map(|(n, a)| ('@', n, a)))
        else {
            return (None, Vec::new());
        };

        let catalog = library.catalog(&self.plugins).await;
        // The sigil is part of the identity: `@review` must not find a command
        // called `review`, or `@` would silently become a second `/`.
        let Some(entry) = catalog
            .iter()
            .find(|e| e.name == name && e.kind.sigil() == sigil)
        else {
            return (None, Vec::new());
        };

        let records = client
            .run_hooks(ServerHookEvent::UserPromptExpansion(
                UserPromptExpansionInput {
                    prompt: prompt.to_string(),
                    command: name.to_string(),
                    kind: entry.kind.element().to_string(),
                },
            ))
            .await
            .unwrap_or_default();
        // A hook that refused stops the expansion here, rather than being
        // noticed a layer later with the work already done. `start_blocked`,
        // not the halt: `{"decision":"block"}` and `continue: false` are two
        // different statements and only the second sets a halt.
        if horsie_workflow::start_blocked(&records).is_some() {
            return (None, records);
        }

        let body = match entry.kind {
            CatalogKind::Command => {
                commands::expand(entry.template.as_deref().unwrap_or_default(), args)
            }
            // A skill and an agent have no template. The expansion names the
            // thing and hands over the arguments; the agent then reaches for
            // the skill tool or `spawn_agent` exactly as it would have if the
            // user had asked in prose.
            CatalogKind::Skill => match args {
                "" => format!("Use the `{name}` skill.", name = entry.name),
                _ => format!("Use the `{name}` skill. {args}", name = entry.name),
            },
            CatalogKind::Agent => match args {
                "" => format!(
                    "Delegate this to the `{name}` agent via `spawn_agent`.",
                    name = entry.name
                ),
                _ => format!(
                    "Delegate this to the `{name}` agent via `spawn_agent`: {args}",
                    name = entry.name
                ),
            },
        };
        (
            Some(catalog::frame(entry.kind, &entry.name, args, &body)),
            records,
        )
    }

    /// The `agent_type` a `SubagentStart` / `SubagentStop` hook matches on.
    ///
    /// The plugin-declared type when the spawn named one, so a hook may select
    /// `reviewer` and fire for reviewers only. An untyped spawn reports
    /// `"subagent"` — the general-purpose case, which is a kind and not a lie.
    pub(super) fn agent_type(&self) -> String {
        self.agent_type
            .clone()
            .unwrap_or_else(|| "subagent".to_string())
    }
}

#[async_trait]
impl ContextProvider for SessionContextProvider {
    fn has_start_hooks(&self) -> bool {
        self.use_plugins()
    }

    /// Fire this turn's start hooks, before the run snapshots its history.
    ///
    /// A hook that cannot run is not a turn that cannot start: `run_hooks`
    /// failures fall back to no records, exactly as the `SessionStart` bootstrap
    /// did. Acquiring the runtime is the only step that can fail the turn, and
    /// it fails it the same way `provide` would have, one step later.
    async fn start_hooks(&self, turn: StartTurn) -> Result<TurnPreparation, ContextError> {
        // Reuse the handle the last run resolved when there is one, so a warm
        // agent pays one vendor round-trip per turn rather than two. Only the
        // first turn of a load has nothing cached — and that is the turn whose
        // hooks could not have run any earlier anyway.
        let client = match self.cached_client() {
            Some(cached) => cached.without_hook_sink(),
            None => self.runtime_client().await?,
        };
        let mut records = Vec::new();
        if let Some(source) = turn.start_source {
            // A subagent's start is a `SubagentStart`. It used to be a
            // `SessionStart`, because this call was not gated on the kind at
            // all — a subagent is not a session, and the two events carry
            // different matcher domains.
            let event = match self.kind {
                SessionAgentKind::Sub(id) => ServerHookEvent::SubagentStart(SubagentStartInput {
                    agent_id: id.to_string(),
                    agent_type: self.agent_type(),
                }),
                SessionAgentKind::Main | SessionAgentKind::Step(_) => {
                    ServerHookEvent::SessionStart(SessionStartInput { source })
                }
            };
            records.extend(client.run_hooks(event).await.unwrap_or_default());
        }
        let mut message = None;
        if let Some(prompt) = turn.prompt {
            // Expansion runs before `UserPromptSubmit`, which is where the spec
            // puts it: a submit guard reads the prompt the model will see, not
            // the four characters the user typed to summon it.
            let (expansion, expansion_records) = self.expand_invocation(&client, &prompt).await;
            records.extend(expansion_records);
            // A refused expansion never becomes a turn — `start_blocked` reads
            // the refusal off these records one layer up — so there is nothing
            // left to submit.
            if horsie_workflow::start_blocked(&records).is_some() {
                return Ok(TurnPreparation {
                    records,
                    message: Some(prompt),
                });
            }
            let prompt = expansion.unwrap_or(prompt);
            records.extend(
                client
                    .run_hooks(ServerHookEvent::UserPromptSubmit(UserPromptSubmitInput {
                        prompt: prompt.clone(),
                    }))
                    .await
                    .unwrap_or_default(),
            );
            message = Some(prompt);
        }
        Ok(TurnPreparation { records, message })
    }

    async fn provide(&self) -> Result<Contexts, ContextError> {
        let settings = &self.settings;
        let mut def = session_run_def(settings);
        let use_plugins = settings.use_plugins.unwrap_or(true);
        // Preparation progress is main-only: subagents are quiet by design.
        let broadcast = matches!(
            self.kind,
            SessionAgentKind::Main | SessionAgentKind::Step(_)
        );

        if broadcast {
            emit_progress(
                &self.session,
                self.kind.agent_key(),
                "acquiring_runtime",
                None,
            )
            .await;
        }
        let runtime_client = self.runtime_client().await?;
        // Hooks run runtime-side and report what they did on the tool response.
        // Routing those records here is what makes a plugin's interventions
        // visible to the user rather than silent.
        let runtime_client = runtime_client.with_hook_sink(Arc::new(SessionHookSink::new(
            self.session.clone(),
            self.kind.agent_key(),
        )));
        // Cached *after* the sink is attached, not before: `Stop` runs its hooks
        // through this handle once the turn is over, and a sink-less clone would
        // run them and drop every record on the floor. Cancellation is
        // unaffected — in-flight tracking is shared across clones.
        *self
            .last_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(runtime_client.clone());

        if broadcast {
            emit_progress(
                &self.session,
                self.kind.agent_key(),
                "scanning_workspace",
                None,
            )
            .await;
        }
        let (ws, shared_scan) = scan_workspace(&runtime_client, None, use_plugins).await;
        // No `SessionStart` here any more. It used to fire on this line, once
        // per *run* — `provide` is per-run — so every turn re-ran every start
        // hook, always reporting `source: "startup"`. It now fires once per
        // agent load at `start_hooks`, early enough for its context to reach the
        // turn that triggered it.
        let shared = use_plugins.then(|| SharedContext {
            skills: Arc::new(shared_scan.skills),
            agents: Arc::new(shared_scan.agents),
            root: shared_scan.root,
        });
        // Resolved here rather than carried from the spawn: the definition is a
        // property of the library as it is *now*, so an agent whose plugin was
        // uninstalled between spawn and wake fails loudly.
        let plugin_agent = match (&self.agent_type, shared.as_ref()) {
            (None, _) => None,
            (Some(name), Some(shared)) => Some(shared.agents.get(name).cloned().ok_or_else(|| {
                ContextError::retryable(format!(
                    "this subagent runs as agent type '{name}', which no installed plugin declares"
                ))
            })?),
            (Some(name), None) => {
                return Err(ContextError::retryable(format!(
                    "this subagent runs as agent type '{name}', but the session loads no plugins"
                )));
            }
        };
        if let Some(agent) = &plugin_agent
            && !agent.def.tools.is_empty()
        {
            // The declared allowlist is in Claude's vocabulary; horsie's filter
            // is in horsie's. Same table the hook matchers use, read backwards.
            let allowed: Vec<String> = agent
                .def
                .tools
                .iter()
                .flat_map(|t| horsie_support::plugin::hooks::horsie_tools_for(t))
                .map(str::to_string)
                .collect();
            if allowed.is_empty() {
                tracing::warn!(
                    agent = %agent.def.name,
                    declared = ?agent.def.tools,
                    "agent's tool allowlist names no tool horsie has; it will run with none"
                );
            }
            // Intersected with the session's own allowlist, never substituted
            // for it. An agent definition is a file inside a plugin: it may say
            // which of the tools this session already grants it wants, and must
            // not be able to grant itself one the session withheld.
            def.allowed_tools = Some(match &def.allowed_tools {
                None => allowed,
                Some(session) => allowed
                    .into_iter()
                    .filter(|t| session.contains(t))
                    .collect(),
            });
        }
        // A declared `model` is honoured only when horsie actually has it.
        // Every model declared in the wild is an alias (`inherit`, `sonnet`,
        // `opus`), and mapping those onto whatever the catalogue holds would let
        // a plugin author switch a kimi session to Anthropic by writing a word
        // in a file.
        let provider = match plugin_agent.as_ref().and_then(|a| a.def.model.as_deref()) {
            Some(model) => match self.provider_for(model) {
                Some(provider) => provider,
                None => {
                    tracing::info!(
                        model,
                        "agent declares a model horsie has no provider for; inheriting the session's"
                    );
                    self.llm_provider()?
                }
            },
            None => self.llm_provider()?,
        };
        // Plugin-declared MCP servers, hosted by the runtime. Discovered on the
        // same pass as the workspace scan and only when this agent loads the
        // library at all — a session with no plugins asks for nothing.
        let mut mcp: Vec<Arc<dyn Toolbox>> = if settings.mcp_servers.is_empty() {
            Vec::new()
        } else if let Some(mcp_svc) = self.mcp.as_ref() {
            if broadcast {
                emit_progress(
                    &self.session,
                    self.kind.agent_key(),
                    "connecting_tools",
                    None,
                )
                .await;
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
        // Plugin-declared MCP servers, hosted by the runtime. Discovered on the
        // same pass as the workspace scan and only when this agent loads the
        // library at all — a session with no plugins asks for nothing.
        //
        // Appended *after* the admin boxes: `CompositeToolbox` routes to the
        // first box advertising a name, and a plugin declaring a server the
        // user already configured must not capture those calls, arguments and
        // all.
        if use_plugins {
            match runtime_client.mcp_discover().await {
                Ok(discovery) => {
                    for failure in &discovery.failures {
                        match failure {
                            McpServerFailure::Unreachable(f) => tracing::warn!(
                                session = %self.session_id,
                                server = %f.server,
                                reason = %f.reason,
                                "a plugin MCP server is unavailable; its tools are absent"
                            ),
                            McpServerFailure::NeedsAuth(f) => tracing::info!(
                                session = %self.session_id,
                                server = %f.server,
                                "a plugin MCP server needs authorisation; its tools are absent"
                            ),
                        }
                    }
                    if !discovery.tools.is_empty() {
                        mcp.push(Arc::new(horsie_workflow::PluginMcpToolbox::new(
                            runtime_client.clone(),
                            discovery.tools,
                        )));
                    }
                }
                // Never fatal: a plugin bringing a broken server must not stop a
                // session that merely happens to load it.
                Err(e) => tracing::warn!(
                    session = %self.session_id,
                    error = %e,
                    "plugin MCP discovery failed; continuing without those tools"
                ),
            }
        }
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
            // A step roots its own tree, so its spawns are that tree's `Main`.
            SessionAgentKind::Main | SessionAgentKind::Step(_) => SubAgentParent::Main,
            SessionAgentKind::Sub(id) => SubAgentParent::SubAgent(id),
        };
        // A zero cap disables subagents outright: no tools advertised, so the
        // model never meets a tool that only ever rejects.
        let with_spawn: Arc<dyn Toolbox> = if settings.max_subagents() == 0 {
            with_memory
        } else {
            Arc::new(SubAgentToolbox::new(
                with_memory,
                self.session.clone(),
                caller,
                shared
                    .as_ref()
                    .map(|s| Arc::clone(&s.agents))
                    .unwrap_or_default(),
            ))
        };
        let toolbox: Arc<dyn Toolbox> = match self.kind {
            // An unattended session skips the ask layer entirely rather than
            // offering a tool whose answer would never come.
            SessionAgentKind::Main if self.unattended => {
                Arc::new(SessionTitleToolbox::new(with_spawn, self.session.clone()))
            }
            SessionAgentKind::Main => {
                let inner: Arc<dyn Toolbox> = Arc::new(AskUserToolbox::new(with_spawn));
                Arc::new(SessionTitleToolbox::new(inner, self.session.clone()))
            }
            // A step gets `conclude` instead of the ask and title layers: it
            // asks through `conclude(kind=ask)`, and its title belongs to the
            // run rather than to one step.
            SessionAgentKind::Step(_) => crate::sessions::workflow::StepConcludeToolbox::wrap(
                with_spawn,
                self.step_output_schema.as_ref(),
                // Same rule as the run def: asking rides on `conclude`, which
                // only a step with a declared output has.
                self.step_output_schema.is_some() && !self.unattended,
            ),
            SessionAgentKind::Sub(_) => with_spawn,
        };
        let system_prompt = compose_system_prompt(Some(SESSION_AGENT_PROMPT), &ws, shared.as_ref());
        // A typed subagent's own section follows the generic one, it does not
        // replace it: `SUBAGENT_PROMPT_SUFFIX` is the only place an agent is
        // told its final message is its report and that it cannot ask the user,
        // and no definition in the wild says either — they open "you are an
        // expert code reviewer" and stop. The workspace and skill sections
        // around both are untouched: a named agent still works in the same
        // workspace, with the same skills.
        let subagent_role = plugin_agent.as_ref().map(|a| {
            format!(
                "{SUBAGENT_PROMPT_SUFFIX}\n\n# Agent type: {}\n\n{}\n",
                a.def.name, a.def.prompt
            )
        });
        let suffix: Option<&str> = match &self.kind {
            SessionAgentKind::Main if self.unattended => Some(UNATTENDED_PROMPT_SUFFIX),
            SessionAgentKind::Main => None,
            SessionAgentKind::Step(_) => Some(STEP_PROMPT_SUFFIX),
            SessionAgentKind::Sub(_) => {
                Some(subagent_role.as_deref().unwrap_or(SUBAGENT_PROMPT_SUFFIX))
            }
        };
        let system_prompt = match suffix {
            None => system_prompt,
            Some(suffix) => Some(match system_prompt {
                Some(p) => format!("{p}{suffix}"),
                None => suffix.trim_start().to_string(),
            }),
        };
        let system_prompt = match (system_prompt, memory_index.is_empty()) {
            (Some(p), false) => Some(format!("{p}\n\n{memory_index}")),
            (Some(p), true) => Some(p),
            (None, false) => Some(memory_index),
            (None, true) => None,
        };
        if broadcast {
            emit_progress(&self.session, self.kind.agent_key(), "ready", None).await;
        }
        Ok(Contexts {
            provider,
            toolbox,
            system_prompt,
        })
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
    //! What a turn is assembled from: which tools each kind of agent gets, and
    //! what a slash command expands into.
    use super::super::testing::*;
    use super::super::*;
    use super::*;

    use horsie_models::hooks::HookAction;
    use horsie_workflow::{ContextProvider, Contexts, StartTurn};
    use std::sync::Arc;
    use uuid::Uuid;

    #[tokio::test]
    async fn subagent_toolbox_strips_session_metadata_tools() {
        let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;

        let build = |kind: SessionAgentKind| SessionContextProvider {
            agent_type: None,
            runtimes: f.deps.runtimes.provider(id.to_string(), "mock".into()),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            settings: actor_spec_fixture().agent,
            step_output_schema: None,
            session_id: id,
            kind,
            unattended: false,
            session: session.clone(),
            plugins: Vec::new(),
            plugin_library: None,
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
        let prompt = sub.system_prompt.unwrap();
        assert!(
            prompt.contains("# Subagent role"),
            "the subagent prompt must explain its role"
        );
        assert!(prompt.contains("automatically delivered"), "{prompt}");
        assert!(prompt.contains("do not poll"), "{prompt}");
        assert!(
            prompt.contains("user requests a progress update"),
            "{prompt}"
        );
    }

    #[tokio::test]
    async fn a_zero_subagent_cap_hides_the_spawn_tools() {
        let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let mut settings = actor_spec_fixture().agent;
        settings.max_concurrent_subagents = Some(0);
        let provider = SessionContextProvider {
            runtimes: f.deps.runtimes.provider(id.to_string(), "mock".into()),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            settings,
            step_output_schema: None,
            session_id: id,
            kind: SessionAgentKind::Main,
            agent_type: None,
            unattended: false,
            session: session.clone(),
            plugins: Vec::new(),
            plugin_library: None,
            last_client: Mutex::new(None),
        };
        let tools: Vec<String> = provider
            .provide()
            .await
            .unwrap()
            .toolbox
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        // Disabled, not merely unusable: an advertised tool that always
        // rejects reads as a bug to the model.
        for t in ["spawn_agent", "subagent_status"] {
            assert!(!tools.contains(&t.to_string()), "disabled session has {t}");
        }
    }

    #[tokio::test]
    async fn an_unattended_session_is_offered_no_ask_user_tool() {
        // A routine run has nobody to answer a question: offering `ask_user`
        // would let the agent park the run forever. The prompt has to say so
        // too -- the base prompt tells the model the tool exists.
        let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let build = |unattended: bool| SessionContextProvider {
            runtimes: f.deps.runtimes.provider(id.to_string(), "mock".into()),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            settings: actor_spec_fixture().agent,
            step_output_schema: None,
            session_id: id,
            kind: SessionAgentKind::Main,
            agent_type: None,
            unattended,
            session: session.clone(),
            plugins: Vec::new(),
            plugin_library: None,
            last_client: Mutex::new(None),
        };
        let names = |c: &Contexts| -> Vec<String> {
            c.toolbox.specs().into_iter().map(|s| s.name).collect()
        };

        let unattended = build(true).provide().await.unwrap();
        let tools = names(&unattended);
        assert!(!tools.contains(&ASK_USER_TOOL.to_string()));
        // Everything else the main agent has is untouched.
        assert!(tools.contains(&"set_session_title".to_string()));
        assert!(tools.contains(&"spawn_agent".to_string()));
        assert!(
            unattended
                .system_prompt
                .unwrap()
                .contains("# Unattended run"),
            "an unattended run must be told there is no user"
        );

        let attended = build(false).provide().await.unwrap();
        assert!(names(&attended).contains(&ASK_USER_TOOL.to_string()));
        assert!(!attended.system_prompt.unwrap().contains("# Unattended run"));
    }

    #[test]
    fn a_subagent_gets_its_own_runtime_identity() {
        let client = horsie_runtime_client::RuntimeClient::new(
            horsie_runtime_client::MockTransport::ok(""),
            "session-id",
        );
        let main = scoped_client(&SessionAgentKind::Main, client.clone());
        assert_eq!(main.agent_id(), "session-id");

        let sub_id = Uuid::new_v4();
        let sub = scoped_client(&SessionAgentKind::Sub(sub_id), client);
        assert_eq!(sub.agent_id(), sub_id.to_string());
    }

    #[tokio::test]
    async fn a_slash_command_expands_into_its_framed_template() {
        let (f, session, id) = catalog_harness(vec![catalog_entry(
            horsie_support::plugin::catalog::CatalogKind::Command,
            "review",
            Some("Review $1 for bugs. Full args: $ARGUMENTS"),
        )])
        .await;
        let provider = catalog_provider(&f, &session, id);
        let message = prepared_message(&provider, "/review src/a.rs carefully")
            .await
            .expect("a command expands");
        assert!(
            message.starts_with("<command name=\"review\" args=\"src/a.rs carefully\">"),
            "framed so a client can tell an invocation from typed text: {message}"
        );
        assert!(message.contains("Review src/a.rs for bugs."), "{message}");
        assert!(
            message.contains("Full args: src/a.rs carefully"),
            "{message}"
        );
    }

    /// A skill and an agent have no template, so expansion names the thing and
    /// lets the agent reach for the tool it already has.
    #[tokio::test]
    async fn a_skill_and_an_agent_expand_under_their_own_sigils() {
        use horsie_support::plugin::catalog::CatalogKind;
        let (f, session, id) = catalog_harness(vec![
            catalog_entry(CatalogKind::Skill, "tdd", None),
            catalog_entry(CatalogKind::Agent, "reviewer", None),
        ])
        .await;
        let provider = catalog_provider(&f, &session, id);

        let skill = prepared_message(&provider, "/tdd on the parser")
            .await
            .unwrap();
        assert!(skill.starts_with("<skill name=\"tdd\""), "{skill}");
        assert!(skill.contains("Use the `tdd` skill."), "{skill}");
        assert!(skill.contains("on the parser"), "{skill}");

        let agent = prepared_message(&provider, "@reviewer this diff")
            .await
            .unwrap();
        assert!(agent.starts_with("<agent name=\"reviewer\""), "{agent}");
        assert!(agent.contains("spawn_agent"), "{agent}");

        // The sigil is part of the identity: `@` must not become a second `/`.
        assert_eq!(
            prepared_message(&provider, "@tdd").await.as_deref(),
            Some("@tdd"),
            "a skill is not reachable as an agent"
        );
    }

    /// An unknown name is left exactly as written: a message may legitimately
    /// begin with a slash, and refusing it would make `/etc/hosts` unsendable.
    #[tokio::test]
    async fn an_unknown_name_leaves_the_prompt_alone() {
        let (f, session, id) = catalog_harness(vec![catalog_entry(
            horsie_support::plugin::catalog::CatalogKind::Command,
            "review",
            Some("body"),
        )])
        .await;
        let provider = catalog_provider(&f, &session, id);
        for prompt in [
            "/nosuch thing",
            "/etc/hosts is a file",
            "hello",
            "mail me at a@b.com",
        ] {
            assert_eq!(
                prepared_message(&provider, prompt).await.as_deref(),
                Some(prompt),
                "{prompt} must reach the model unchanged"
            );
        }
    }

    /// Expanding costs no runtime call — which is the whole reason the
    /// catalogue moved to the server.
    #[tokio::test]
    async fn expansion_makes_no_workspace_scan() {
        let (f, session, id) = catalog_harness(vec![catalog_entry(
            horsie_support::plugin::catalog::CatalogKind::Command,
            "review",
            Some("body"),
        )])
        .await;
        let provider = catalog_provider(&f, &session, id);
        prepared_message(&provider, "/review a.rs").await;
        assert_eq!(
            f.agent.scan_count(),
            0,
            "the seam answers from the database, not the sandbox"
        );
    }

    /// `UserPromptExpansion` fires for the entry being expanded, carrying its
    /// name as the matcher domain and its kind alongside — and before
    /// `UserPromptSubmit` sees the result, which is the order the spec gives
    /// them.
    #[tokio::test]
    async fn expansion_is_hooked_before_submission() {
        let (f, session, id) = catalog_harness(vec![catalog_entry(
            horsie_support::plugin::catalog::CatalogKind::Command,
            "review",
            Some("body"),
        )])
        .await;
        let provider = catalog_provider(&f, &session, id);
        prepared_message(&provider, "/review a.rs").await;

        let events = f.agent.hook_events();
        let expansion = events.iter().position(|e| *e == "UserPromptExpansion");
        let submit = events.iter().position(|e| *e == "UserPromptSubmit");
        assert!(
            expansion.is_some(),
            "the expansion must be hooked: {events:?}"
        );
        assert!(
            expansion < submit,
            "expansion runs first, so a submit guard reads what the model will: {events:?}"
        );
        let named: Vec<(String, String)> = f
            .agent
            .server_hook_events()
            .into_iter()
            .filter_map(|e| match e {
                horsie_models::runtime::ServerHookEvent::UserPromptExpansion(i) => {
                    Some((i.command, i.kind))
                }
                _ => None,
            })
            .collect();
        assert_eq!(named, vec![("review".to_string(), "command".to_string())]);
    }

    /// A hook answering `{"decision":"block"}` must stop the expansion itself,
    /// not merely be noticed a layer later with the work already done. The
    /// block is not a halt, and reading only the halt is how this regressed.
    #[tokio::test]
    async fn a_blocking_expansion_hook_stops_the_expansion() {
        let blocked = HookRecord {
            plugin: "guard".into(),
            duration_ms: 0,
            halt: None,
            action: HookAction::UserPromptExpansion(
                horsie_models::hooks::UserPromptExpansionRecord {
                    command: "review".into(),
                    system_message: None,
                    outcome: horsie_models::hooks::UserPromptExpansionOutcome::Blocked(
                        horsie_models::hooks::HookBlocked {
                            reason: Some("not this one".into()),
                        },
                    ),
                },
            ),
        };
        let (f, session, id) = catalog_harness_with(
            vec![catalog_entry(
                horsie_support::plugin::catalog::CatalogKind::Command,
                "review",
                Some("the template"),
            )],
            vec![vec![blocked]],
        )
        .await;
        let provider = catalog_provider(&f, &session, id);
        let prep = provider
            .start_hooks(StartTurn {
                start_source: None,
                prompt: Some("/review a.rs".to_string()),
            })
            .await
            .expect("prepare");
        assert_eq!(
            prep.message.as_deref(),
            Some("/review a.rs"),
            "a refused expansion leaves the prompt as typed"
        );
        assert_eq!(
            horsie_workflow::start_blocked(&prep.records).as_deref(),
            Some("not this one"),
            "and the refusal still abandons the turn"
        );
        assert!(
            !f.agent.hook_events().contains(&"UserPromptSubmit"),
            "a refused prompt never becomes a submission: {:?}",
            f.agent.hook_events()
        );
    }

    /// The agent's body is added to the generic subagent role, and its `tools`
    /// allowlist reaches the toolbox through the same alias table hook matchers
    /// use.
    #[tokio::test]
    async fn a_typed_subagent_runs_with_its_plugins_prompt() {
        let (f, session, id) = agent_harness().await;
        let sub = spawn_typed(&session, Some("code-reviewer")).await.unwrap();

        let provider = typed_provider(&f, &session, id, sub, None);
        let contexts = provider.provide().await.expect("contexts");
        let prompt = contexts.system_prompt.unwrap_or_default();
        assert!(
            prompt.contains("# Agent type: code-reviewer"),
            "the plugin's agent names its own section: {prompt}"
        );
        // The generic framing is the only place a subagent is told where its
        // output goes; a plugin's prompt never says it, so it must survive.
        assert!(
            prompt.contains("Your final message is your report"),
            "a typed subagent must still know it reports to its parent: {prompt}"
        );
        assert!(
            prompt.contains("Report only high-confidence bugs."),
            "the plugin's body is the role: {prompt}"
        );
        // `Read, Grep` in Claude's vocabulary is `read_file, grep` in horsie's.
        let tools: Vec<String> = contexts
            .toolbox
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(tools.contains(&"read_file".to_string()), "{tools:?}");
        assert!(tools.contains(&"grep".to_string()), "{tools:?}");
        assert!(
            !tools.contains(&"bash".to_string()),
            "the allowlist must exclude what it did not name: {tools:?}"
        );
    }

    /// An agent definition is a file inside a plugin. It may narrow the tools
    /// the session already grants it and must not be able to widen them —
    /// otherwise installing a plugin would hand back what an operator withheld.
    #[tokio::test]
    async fn an_agents_tools_cannot_widen_the_sessions_own_allowlist() {
        let (f, session, id) = agent_harness().await;
        let sub = spawn_typed(&session, Some("code-reviewer")).await.unwrap();

        // The session grants `grep` only; the agent asks for `Read, Grep`.
        let provider = typed_provider(&f, &session, id, sub, Some(vec!["grep".to_string()]));
        let contexts = provider.provide().await.expect("contexts");
        let tools: Vec<String> = contexts
            .toolbox
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(tools.contains(&"grep".to_string()), "{tools:?}");
        assert!(
            !tools.contains(&"read_file".to_string()),
            "an agent must not grant itself a tool the session withheld: {tools:?}"
        );
    }

    /// The definition is resolved when the subagent runs, not carried from the
    /// spawn — so an agent whose plugin has gone fails loudly rather than
    /// running a prompt nobody can point at.
    #[tokio::test]
    async fn a_subagent_whose_agent_type_is_gone_fails_rather_than_running_generic() {
        let (f, session, id) = agent_harness().await;
        let provider = SessionContextProvider {
            runtimes: f.deps.runtimes.provider(id.to_string(), "mock".to_string()),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            settings: actor_spec_fixture().agent,
            step_output_schema: None,
            session_id: id,
            kind: SessionAgentKind::Sub(Uuid::new_v4()),
            agent_type: Some("uninstalled-agent".to_string()),
            unattended: false,
            session: session.clone(),
            plugins: Vec::new(),
            plugin_library: None,
            last_client: Mutex::new(None),
        };
        let Err(err) = provider.provide().await else {
            panic!("a subagent whose agent type is gone must not run generic");
        };
        assert!(err.message.contains("uninstalled-agent"), "{}", err.message);
        assert!(
            !err.terminal,
            "a missing plugin is not the end of a session"
        );
    }

    /// The type is what `SubagentStart` / `SubagentStop` matchers select on. It
    /// was the constant `"subagent"` for every subagent before agent types
    /// existed, so a matcher could only select all or none.
    #[tokio::test]
    async fn the_agent_type_reaches_the_subagent_hook_matcher() {
        let (f, session, _id) = agent_harness().await;
        spawn_typed(&session, Some("code-reviewer")).await.unwrap();
        for _ in 0..200 {
            if f.agent.hook_events().contains(&"SubagentStart") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let types: Vec<String> = f
            .agent
            .server_hook_events()
            .into_iter()
            .filter_map(|e| match e {
                horsie_models::runtime::ServerHookEvent::SubagentStart(i) => Some(i.agent_type),
                _ => None,
            })
            .collect();
        assert_eq!(types, vec!["code-reviewer".to_string()]);
    }

    /// An untyped spawn is the general-purpose subagent, unchanged.
    #[tokio::test]
    async fn an_untyped_spawn_still_reports_the_generic_type() {
        let (f, session, _id) = agent_harness().await;
        spawn_typed(&session, None).await.unwrap();
        for _ in 0..200 {
            if f.agent.hook_events().contains(&"SubagentStart") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let types: Vec<String> = f
            .agent
            .server_hook_events()
            .into_iter()
            .filter_map(|e| match e {
                horsie_models::runtime::ServerHookEvent::SubagentStart(i) => Some(i.agent_type),
                _ => None,
            })
            .collect();
        assert_eq!(types, vec!["subagent".to_string()]);
    }

    /// `SessionStart` used to fire from `provide()`, which is per-run — so every
    /// turn re-ran every start hook, always reporting `source: "startup"`. It
    /// fires once per agent load now; `UserPromptSubmit` is the one that belongs
    /// to every turn.
    #[tokio::test]
    async fn a_session_starts_once_but_every_prompt_is_hooked() {
        let (f, session) = stop_harness(vec![]).await;
        send(&session, "first").await;
        settled_inputs(&session).await;
        send(&session, "second").await;
        settled_inputs(&session).await;

        let starts = f
            .agent
            .hook_events()
            .into_iter()
            .filter(|e| *e == "SessionStart")
            .count();
        let prompts = f
            .agent
            .hook_events()
            .into_iter()
            .filter(|e| *e == "UserPromptSubmit")
            .count();
        assert_eq!(starts, 1, "the start hook is due once per agent load");
        assert_eq!(prompts, 2, "the prompt hook is due every turn");
    }

    /// A subagent is not a session. The call fired `SessionStart` for one before
    /// this, because it was not gated on the agent's kind at all — so a hook
    /// matching `startup` fired again for every subagent spawned.
    #[tokio::test]
    async fn a_subagent_fires_subagent_start_never_session_start() {
        let (f, session) = stop_harness(vec![]).await;
        spawn_sub(&session, "research", "dig into it").await;
        for _ in 0..200 {
            if f.agent.hook_events().contains(&"SubagentStart") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // The main agent runs a turn of its own once the subagent reports back,
        // so one `SessionStart` is correct here. What must never happen is the
        // subagent contributing a second one — which is what it did before,
        // because the call was not gated on the agent's kind.
        let events = f.agent.hook_events();
        assert_eq!(
            events.iter().filter(|e| **e == "SubagentStart").count(),
            1,
            "the subagent starts as a subagent, got {events:?}"
        );
        assert_eq!(
            events.iter().filter(|e| **e == "SessionStart").count(),
            1,
            "only the session's own agent may claim a session start, got {events:?}"
        );
    }
}
