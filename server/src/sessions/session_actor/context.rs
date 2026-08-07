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
        .tell(SessionCommand::Progress {
            key,
            stage: stage.to_string(),
            detail,
        })
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
    fn agent_key(&self) -> AgentKey {
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
it is delivered to the agent that spawned you — make it self-contained. You may spawn \
your own subagents with spawn_agent and check on them with subagent_status. You cannot \
ask the user or rename the session; if you are blocked, report that instead.";

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
