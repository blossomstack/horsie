//! How one turn is assembled.
//!
//! A [`SessionContextProvider`] is what an [`AgentActor`](crate::agent_loop::AgentActor)
//! asks, on its own task, for everything a run needs: the runtime handle, the
//! LLM provider, the toolbox and the system prompt. It resolves them per run
//! rather than holding them, which is what lets an agent stay resident across a
//! hibernate and resume without knowing either happened.
//!
//! One type serves every kind of agent a session hosts, because they differ
//! only in which layers they get — and which layers is not decided here. The
//! owning runner resolved it all into an [`AgentRole`], so this file reads
//! values (`may_ask`, `titles`, `step_result`, `prompt_suffix`) and never asks
//! what kind of agent it is assembling for.

use super::CoreCommand;
use super::runner::ids::AgentId;
use super::runner::role::{AgentRole, StopHookKind, TitleScope};
use super::{SessionCommand, hooks::SessionHookSink};
use crate::agent_loop::{
    AgentRunDef, ContextError, ContextProvider, Contexts, DefaultToolboxFactory, SharedContext,
    StartTurn, ToolboxFactory, TurnPreparation, compose_system_prompt, scan_workspace,
};
use crate::sessions::addressing::SessionRef;
use crate::{
    runtime_manager::{NARRATION_BUFFER, RuntimeClientProvider, RuntimeError},
    sessions::{
        ask_tool::AskUserToolbox, spawn_tool::SubAgentToolbox, spec::AgentSettings,
        title_tool::SessionTitleToolbox,
    },
};
use async_trait::async_trait;
use horsie_agentcore::{LlmProvider, Toolbox};
use horsie_models::{
    hooks::HookRecord,
    runtime::{
        McpServerFailure, ServerHookEvent, SessionStartInput, SubagentStartInput,
        UserPromptExpansionInput, UserPromptSubmitInput,
    },
};
use horsie_runtime_host::RuntimeClient;
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
async fn emit_progress(session: &SessionRef, agent: AgentId, stage: &str, detail: Option<String>) {
    let _ = session
        .tell(SessionCommand::Core(CoreCommand::Progress {
            agent,
            stage: stage.to_string(),
            detail,
        }))
        .await;
}

/// Carry a vendor's running account of an acquisition into `key`'s log.
///
/// Every line arrives under `acquiring_runtime` — the stage the caller has
/// already announced — because this is more of that stage rather than a new
/// one: the agent is still waiting for the same runtime, and now says what the
/// vendor says is happening to it.
///
/// Returns the sink to hand the acquisition and the task draining it. The task
/// ends when the sink is dropped, which the acquisition does on its way out.
fn narration_pump(
    session: &SessionRef,
    agent: AgentId,
) -> (
    crate::runtime_manager::NarrationSink,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(NARRATION_BUFFER);
    let session = session.clone();
    let task = tokio::spawn(async move {
        while let Some(detail) = rx.recv().await {
            emit_progress(&session, agent, "acquiring_runtime", Some(detail)).await;
        }
    });
    (tx, task)
}

/// The baseline system prompt given to every session agent.
const SESSION_AGENT_PROMPT: &str = include_str!("system_prompt.md");

/// The interactive session's `AgentRunDef`.
pub(super) fn session_run_def(settings: &AgentSettings) -> AgentRunDef {
    AgentRunDef {
        system_prompt: None,
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

/// What a workflow step promises to return, carried to the toolbox that builds
/// its `submit_result` tool. Default (empty outcomes, no fields, not
/// interactive) for every agent that is not a step; those never get the layer.
#[derive(Clone, Debug, Default)]
pub(crate) struct StepResultDef {
    pub(crate) outcomes: Vec<horsie_models::workflow::StepOutcome>,
    pub(crate) fields: Vec<horsie_models::workflow::StepField>,
    /// Whether the step may ask. Consumed by the runner when it resolves the
    /// role's `may_ask`; carried here so the contract stays one value.
    #[allow(dead_code)]
    pub(crate) interactive: bool,
}

/// Wrap `base` with the control-plane tools, and render the command index for
/// the system prompt.
///
/// Main-agent only. A subagent, a workflow step and a fork all inherit the
/// session's settings, but authority over the server is not a setting they
/// should carry — the same rule that keeps session-metadata tools off them.
fn build_control_layer(
    base: Arc<dyn Toolbox>,
    services: Option<&Arc<crate::users::UserServices>>,
    enabled: bool,
) -> (Arc<dyn Toolbox>, String) {
    if !enabled {
        return (base, String::new());
    }
    let Some(services) = services else {
        tracing::warn!("session asks for the control plane but no services are wired; ignoring");
        return (base, String::new());
    };
    let toolbox = crate::control::toolbox::ControlToolbox::new(
        base,
        services.clone(),
        crate::control::operations(),
    );
    let index = format!(
        "## Managing this horsie server\n\n\
         You can manage this server through the `horsie_*` tools: {}\n\n\
         Call a resource's tool with an `action`. Changes take effect \
         immediately and are not confirmed with the user first, so read before \
         you write when you are unsure which row you mean.",
        toolbox.command_index()
    );
    (Arc::new(toolbox), index)
}

use super::runner::role::SUBAGENT_PROMPT_SUFFIX;

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
    /// The account's whole service bundle, for the control-plane tools — which
    /// reach agents, routines and environments alike, so unlike `memory` there
    /// is no single service to hold. `None` wherever the control plane is not
    /// wired, which is every test that does not exercise it.
    pub(super) services: Option<Arc<crate::users::UserServices>>,
    pub(super) session_id: Uuid,
    /// Everything kind-specific about the agent this provider serves, resolved
    /// by its runner: settings, toolbox layers, prompt suffix, hook shape. The
    /// provider reads values and never asks what kind of agent it is.
    pub(super) role: AgentRole,
    /// The owning session's mailbox — routes the server-owned tools.
    pub(super) session: SessionRef,
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
        self.registry
            .read()
            .ok()?
            .get(model)
            .map(|e| e.provider.clone())
    }

    fn llm_provider(&self) -> Result<Arc<dyn LlmProvider>, String> {
        let reg = self
            .registry
            .read()
            .map_err(|_| "provider registry lock poisoned".to_string())?;
        reg.get(&self.role.settings.model)
            .map(|e| e.provider.clone())
            .ok_or_else(|| {
                format!(
                    "no provider registered for model '{}'",
                    self.role.settings.model
                )
            })
    }

    /// This session's model's context window, when its card declares one.
    ///
    /// Absent for a model horsie has no provider for at all, which is a failure
    /// `llm_provider` reports first — so a `None` here always means "the card
    /// says nothing", never "the model is missing".
    fn context_window(&self) -> Option<u32> {
        self.registry
            .read()
            .ok()?
            .get(&self.role.settings.model)?
            .context_window
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
        self.role.settings.use_plugins.unwrap_or(true)
    }

    /// Install this agent's own plugin bundles into its own tree on the runtime.
    ///
    /// The bundles come from the agent's settings, which a workflow step fills
    /// from its own preset — that is what makes a step able to run with skills
    /// its siblings do not have. It used to be the session's union, installed
    /// once for everyone.
    ///
    /// Retryable on failure. The bundles come from the artifact store over the
    /// runtime's own connection, and a store that is briefly unreachable is the
    /// ordinary transient — not a reason to make the session terminal.
    async fn provision_agent(&self, client: &RuntimeClient) -> Result<(), ContextError> {
        if !self.use_plugins() {
            // Provisioned with nothing, deliberately, rather than skipped: the
            // runtime refuses requests naming an agent it has never been told
            // about, and "this agent takes no plugins" is a thing to be told.
            return client
                .provision_agent(Vec::new())
                .await
                .map(|_| ())
                .map_err(|e| ContextError::retryable(e.to_string()));
        }
        let mut names = self.role.settings.plugins.clone();
        if names.is_empty() {
            // Nothing selected falls back to the account's default-enabled set,
            // exactly as session-wide provisioning did.
            if let Some(library) = &self.plugin_library {
                names = library.default_names().await;
            }
        }
        let bundles = match &self.plugin_library {
            Some(library) if !names.is_empty() => library
                .resolve(&names)
                .await
                .map_err(ContextError::retryable)?
                .iter()
                .map(|r| horsie_models::runtime::BundleRef {
                    name: r.name.clone(),
                    hash: r.hash.clone(),
                })
                .collect(),
            _ => Vec::new(),
        };
        client
            .provision_agent(bundles)
            .await
            .map(|_| ())
            .map_err(|e| ContextError::retryable(e.to_string()))
    }

    /// Acquire this agent's runtime handle, scoped to it. Sink-less: `provide`
    /// attaches one for the tool hooks that report themselves mid-call, while
    /// `start_hooks` returns its records to the agent, which journals them
    /// itself. A sink there would both duplicate them and race the turn they
    /// must precede.
    async fn runtime_client(&self) -> Result<RuntimeClient, ContextError> {
        // The wait this call *is*. A machine that has to resume takes minutes,
        // and the vendor says why the whole time — first in what it returns,
        // then on its sink — so those words are carried into this agent's log
        // as they arrive rather than summarised once it is over.
        let (narrate, task) = self
            .role
            .broadcasts
            .then(|| narration_pump(&self.session, self.role.agent))
            .unzip();
        let acquired = self.runtimes.get(narrate).await;
        // Joined rather than detached. The acquisition dropped the sender on
        // its way out, so this ends immediately — and waiting for it is what
        // keeps every line of narration ordered before whatever stage the
        // caller reports next.
        if let Some(task) = task {
            let _ = task.await;
        }
        let client = acquired.map_err(|e| match e {
            // The one failure the session can never retry: the vendor is alive
            // and says the runtime is gone. A vendor that is merely offline
            // (`Unavailable`) says nothing about the runtime's existence.
            RuntimeError::Gone(m) => ContextError::terminal(m),
            other @ (RuntimeError::Unavailable(_) | RuntimeError::Provision(_)) => {
                ContextError::retryable(other.to_string())
            }
        })?;
        // Every agent but the main one acts under its own identity: they
        // share the sandbox, never its cwd/env bucket — the runtime keys that
        // state by agent id.
        Ok(match self.role.scoped {
            Some(id) => client.with_agent_id(id.to_string()),
            None => client,
        })
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
        if crate::agent_loop::start_blocked(&records).is_some() {
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
        self.role
            .agent_type
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
        // KNOWN GAP: this seam runs *before* `provide` (see the
        // `ContextProvider` docs), so the hooks below run against an agent whose
        // plugin tree has not been built yet — and a hook is itself a plugin
        // file. A runtime refuses a request naming an unprovisioned agent, and
        // `run_hooks` swallows that with `unwrap_or_default`, so the hooks
        // simply never fire.
        //
        // Provisioning here is the obvious fix and is NOT applied, because the
        // extra pre-turn round trip wedges a fork's turn: with it, three fork
        // tests and one subagent test hang; without it, all 36 pass. That is a
        // fork-path fragility this change surfaces rather than causes, and it
        // needs its own diagnosis before this line goes in.
        // Before the hooks, because a hook *is* a plugin file. This seam runs
        // ahead of `provide` — see the `ContextProvider` docs — so it is the
        // first place an agent's tree can exist, and hooks fired against an
        // agent the runtime has never been told about are refused. `run_hooks`
        // swallows that with `unwrap_or_default`, so the failure would be every
        // plugin hook silently not running.
        //
        // `provide` provisions too. Both is correct rather than wasteful: this
        // method is skipped entirely when `has_start_hooks` is false, and the
        // runtime absorbs a repeat for a set it has already built.
        self.provision_agent(&client).await?;
        let mut records = Vec::new();
        if let Some(source) = turn.start_source {
            // A subagent's start is a `SubagentStart`. It used to be a
            // `SessionStart`, because this call was not gated on the kind at
            // all — a subagent is not a session, and the two events carry
            // different matcher domains.
            let event = match self.role.stop_hook {
                StopHookKind::SubagentStop => ServerHookEvent::SubagentStart(SubagentStartInput {
                    agent_id: self.role.agent.to_string(),
                    agent_type: self.agent_type(),
                }),
                // A step keeps `SessionStart`: it roots its own subagent tree,
                // so answering `SubagentStart` would contradict its own start.
                StopHookKind::Stop => ServerHookEvent::SessionStart(SessionStartInput { source }),
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
            if crate::agent_loop::start_blocked(&records).is_some() {
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
        let settings = &self.role.settings;
        let mut def = session_run_def(settings);
        let use_plugins = settings.use_plugins.unwrap_or(true);
        // Preparation progress reaches watchers only where somebody watches:
        // subagents are quiet by design.
        let broadcast = self.role.broadcasts;

        if broadcast {
            emit_progress(&self.session, self.role.agent, "acquiring_runtime", None).await;
        }
        let runtime_client = self.runtime_client().await?;
        // Hooks run runtime-side and report what they did on the tool response.
        // Routing those records here is what makes a plugin's interventions
        // visible to the user rather than silent.
        let runtime_client = runtime_client.with_hook_sink(Arc::new(SessionHookSink::new(
            self.session.clone(),
            self.role.agent,
        )));
        // Cached *after* the sink is attached, not before: `Stop` runs its hooks
        // through this handle once the turn is over, and a sink-less clone would
        // run them and drop every record on the floor. Cancellation is
        // unaffected — in-flight tracking is shared across clones.
        *self
            .last_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(runtime_client.clone());

        // Before anything reads this agent's plugins — the hooks its bundles
        // declare, the skills the scan finds, the MCP servers discovery starts.
        // Sent on every load rather than once: the runtime is the only party
        // that knows what is already on its disk, and it absorbs the repeat.
        self.provision_agent(&runtime_client).await?;

        if broadcast {
            emit_progress(&self.session, self.role.agent, "scanning_workspace", None).await;
        }
        let (ws, shared_scan) = scan_workspace(&runtime_client, None).await;
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
        let plugin_agent = match (&self.role.agent_type, shared.as_ref()) {
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
        let mut mcp: crate::agent_loop::McpToolboxes = if settings.mcp_servers.is_empty() {
            crate::agent_loop::McpToolboxes::default()
        } else if let Some(mcp_svc) = self.mcp.as_ref() {
            if broadcast {
                emit_progress(&self.session, self.role.agent, "connecting_tools", None).await;
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
            crate::agent_loop::McpToolboxes::default()
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
                            McpServerFailure::Unreachable(f) => {
                                tracing::warn!(
                                    session = %self.session_id,
                                    server = %f.server,
                                    reason = %f.reason,
                                    "a plugin MCP server is unavailable; its tools are absent"
                                );
                                mcp.unavailable.push(
                                    crate::agent_loop::McpUnavailable::Unreachable {
                                        server: f.server.clone(),
                                        reason: f.reason.clone(),
                                    },
                                );
                            }
                            McpServerFailure::NeedsAuth(f) => {
                                tracing::info!(
                                    session = %self.session_id,
                                    server = %f.server,
                                    "a plugin MCP server needs authorisation; its tools are absent"
                                );
                                mcp.unavailable.push(
                                    crate::agent_loop::McpUnavailable::NeedsAuth {
                                        server: f.server.clone(),
                                    },
                                );
                            }
                        }
                    }
                    if !discovery.tools.is_empty() {
                        mcp.boxes
                            .push(Arc::new(crate::agent_loop::PluginMcpToolbox::new(
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
        let (with_memory, control_index) =
            build_control_layer(with_memory, self.services.as_ref(), self.role.control_plane);
        // Whoever this agent is, its spawns register under its own id — the
        // unified identity is what dissolved the old per-kind caller enum.
        // A zero cap disables subagents outright: no tools advertised, so the
        // model never meets a tool that only ever rejects.
        let with_spawn: Arc<dyn Toolbox> = if settings.max_subagents() == 0 {
            with_memory
        } else {
            Arc::new(SubAgentToolbox::new(
                with_memory,
                self.session.clone(),
                self.role.agent,
                shared
                    .as_ref()
                    .map(|s| Arc::clone(&s.agents))
                    .unwrap_or_default(),
            ))
        };
        // The role's values, layered in a fixed order: the result contract
        // innermost (a step's `submit_result`), then `ask_user` where asking
        // is allowed, then the title tool the role names. An unattended main
        // agent simply has `may_ask` false, so the ask layer never exists to
        // offer a tool whose answer would never come.
        let mut toolbox: Arc<dyn Toolbox> = with_spawn;
        if let Some(step_result) = &self.role.step_result {
            toolbox = crate::sessions::workflow::StepResultToolbox::wrap(
                toolbox,
                step_result.outcomes.clone(),
                step_result.fields.clone(),
            );
        }
        if self.role.may_ask {
            toolbox = Arc::new(AskUserToolbox::new(toolbox));
        }
        toolbox = match self.role.titles {
            TitleScope::Session => {
                Arc::new(SessionTitleToolbox::new(toolbox, self.session.clone()))
            }
            TitleScope::Fork(id) => Arc::new(SessionTitleToolbox::for_fork(
                toolbox,
                self.session.clone(),
                id,
            )),
            TitleScope::None => toolbox,
        };
        let system_prompt = compose_system_prompt(
            Some(SESSION_AGENT_PROMPT),
            &ws,
            shared.as_ref(),
            settings.instructions.as_deref(),
        );
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
        // The role's suffix, except a typed subagent's composed section takes
        // the generic one's place — `SUBAGENT_PROMPT_SUFFIX` is folded into
        // the composition above.
        let suffix: Option<&str> = match (&subagent_role, self.role.prompt_suffix) {
            (Some(composed), Some(s)) if std::ptr::eq(s, SUBAGENT_PROMPT_SUFFIX) => {
                Some(composed.as_str())
            }
            (_, suffix) => suffix,
        };
        let system_prompt = match suffix {
            None => system_prompt,
            Some(suffix) => Some(match system_prompt {
                Some(p) => format!("{p}{suffix}"),
                None => suffix.trim_start().to_string(),
            }),
        };
        let sections: Vec<String> = [memory_index, control_index]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        let system_prompt = match (system_prompt, sections.is_empty()) {
            (Some(p), false) => Some(format!("{p}\n\n{}", sections.join("\n\n"))),
            (Some(p), true) => Some(p),
            (None, false) => Some(sections.join("\n\n")),
            (None, true) => None,
        };
        if broadcast {
            emit_progress(&self.session, self.role.agent, "ready", None).await;
        }
        Ok(Contexts {
            provider,
            toolbox,
            system_prompt,
            context_window: crate::agent_loop::compaction_window(
                self.role.settings.auto_compact,
                self.context_window(),
            ),
        })
    }
}
