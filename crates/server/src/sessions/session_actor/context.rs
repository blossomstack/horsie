//! How one turn is assembled.
//!
//! A [`SessionContextProvider`] is what an
//! [`AgentActor`](crate::agent_loop::AgentActor) asks, on its own task, for
//! everything a run needs: the runtime handle, the LLM provider, the toolbox
//! and the system prompt. It resolves them per run rather than holding them,
//! which is what lets an agent stay resident across a hibernate and resume
//! without knowing either happened.
//!
//! One type serves all three kinds of agent a session hosts — main, subagent
//! and workflow step — because they differ only in which layers they get.
//! [`SessionAgentKind`] is what decides: the session-metadata tools are
//! main-only, `submit_result` is step-only, and preparation progress is
//! broadcast for everything except a subagent, which is quiet by design.

use super::CoreCommand;
use super::{AgentKey, SessionCommand, hooks::SessionHookSink};
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
async fn emit_progress(session: &SessionRef, key: AgentKey, stage: &str, detail: Option<String>) {
    let _ = session
        .tell(SessionCommand::Core(CoreCommand::Progress {
            key,
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
    key: AgentKey,
) -> (
    crate::runtime_manager::NarrationSink,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(NARRATION_BUFFER);
    let session = session.clone();
    let task = tokio::spawn(async move {
        while let Some(detail) = rx.recv().await {
            emit_progress(&session, key, "acquiring_runtime", Some(detail)).await;
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
    pub(crate) interactive: bool,
}

/// Wrap `base` with the control-plane tools, and render the command index for
/// the system prompt.
///
/// The layer is built only when the session's tool selection names a `horsie_*`
/// tool. The outermost filter would remove them anyway, but the filter cannot
/// unwrite a system prompt — building unconditionally would tell every model on
/// the server it can manage horsie and then reject the call.
///
/// Main-agent only. A subagent, a workflow step and a sub session all inherit
/// the session's settings, but authority over the server is not a setting they
/// should carry — the same rule that keeps session-metadata tools off them.
fn build_control_layer(
    base: Arc<dyn Toolbox>,
    services: Option<&Arc<crate::projects::ProjectServices>>,
    settings: &AgentSettings,
    kind: SessionAgentKind,
) -> (Arc<dyn Toolbox>, String) {
    if !matches!(kind, SessionAgentKind::Main)
        || !crate::tools::grants_control_plane(settings.allowed_tools.as_deref())
    {
        return (base, String::new());
    }
    let Some(services) = services else {
        tracing::warn!("session asks for the control plane but no services are wired; ignoring");
        return (base, String::new());
    };
    // Only the resources the selection actually names. The filter above would
    // drop the rest, but the command index below is written from what this
    // toolbox holds — so narrowing here is what keeps the prompt honest about
    // which parts of the server this session can touch.
    let selected = crate::tools::resolve(settings.allowed_tools.as_deref());
    let operations = crate::control::operations()
        .into_iter()
        .filter(|o| selected.contains(&crate::tools::control_tool_name(o.resource)))
        .collect();
    let toolbox = crate::control::toolbox::ControlToolbox::new(base, services.clone(), operations);
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

/// Wrap `base` with the authoring tools.
///
/// Built only when the session's tool selection names one, and main-agent only
/// — for the same reason the control plane is. A skill authored here is offered
/// to every session this account starts afterwards, which is authority over the
/// server rather than a setting a subagent should inherit.
fn build_authoring_layer(
    base: Arc<dyn Toolbox>,
    services: Option<&Arc<crate::projects::ProjectServices>>,
    settings: &AgentSettings,
    kind: SessionAgentKind,
) -> Arc<dyn Toolbox> {
    if !matches!(kind, SessionAgentKind::Main) {
        return base;
    }
    let selected = crate::tools::resolve(settings.allowed_tools.as_deref());
    if !crate::plugins::authored::toolbox::TOOLS
        .iter()
        .any(|(name, _)| selected.contains(*name))
    {
        return base;
    }
    let Some(services) = services else {
        tracing::warn!("session asks to author skills but no services are wired; ignoring");
        return base;
    };
    Arc::new(crate::plugins::authored::AuthoringToolbox::new(
        base,
        services.authored.clone(),
    ))
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
    /// A sub session of a session. Its own kind, not `Sub`: it owes nobody a
    /// result, it can ask the user, and it names itself.
    SubSession(Uuid),
}

impl SessionAgentKind {
    /// The key this agent is registered under on the session. One vocabulary:
    /// what the provider knows itself as is what the session looks it up by.
    pub(super) fn agent_key(&self) -> AgentKey {
        match self {
            Self::Main => AgentKey::Main,
            Self::Sub(id) => AgentKey::Sub(*id),
            Self::Step(id) => AgentKey::Step(*id),
            Self::SubSession(id) => AgentKey::SubSession(*id),
        }
    }

    /// Whether this agent narrates its own setup. Everything a person opens a
    /// session to watch does; a subagent is quiet by design, and its progress
    /// reaches the reader as the parent's `SubAgent` entry instead.
    fn broadcasts(&self) -> bool {
        matches!(self, Self::Main | Self::Step(_) | Self::SubSession(_))
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
        SessionAgentKind::Sub(id)
        | SessionAgentKind::Step(id)
        | SessionAgentKind::SubSession(id) => client.with_agent_id(id.to_string()),
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

/// Appended to a workflow step's system prompt: what a step is, how it ends,
/// and that its result is what decides where the run goes next. Deliberately
/// short — `submit_result` carries its own schema.
///
/// The paragraph about ending a turn earns its length. A step ends when it
/// calls `submit_result`, but a turn may legitimately end without one — parked
/// on a question, on a timer, or waiting for subagents — and a model that does
/// not know the difference either submits early to be safe or stops with
/// nothing to wake it.
const STEP_PROMPT_SUFFIX: &str = "\n\n# Workflow step\n\
You are one step of a workflow, not a session. Your instruction and the previous \
step's result are in the message above. You share one workspace with every other step: \
what you change on disk is what the next step sees. You may spawn subagents with \
spawn_agent. You cannot rename the session.\n\n\
Finish by calling `submit_result`. What you submit is this step's result *and* what the \
workflow reads to decide which step runs next, so make it accurate and self-contained. \
Ending a turn without it is only safe while something will wake you — a question you \
asked, a timer you armed, or a subagent still running. If nothing will, and the work is \
done, submit.";

/// Appended to a sub session's system prompt.
///
/// A sub session is a session, so almost nothing a subagent is told applies: it
/// can ask the user, and it owes nobody a report. What it does need is to know
/// it is one of several under one session sharing one workspace, and that its
/// title is how a person tells them apart.
const FORK_PROMPT_SUFFIX: &str = "\n\n# Forked session\n\
You are a sub_session: a session branched from another one in this session, carrying its \
history up to the branch point. You share one workspace with it — what you change on disk \
is what it sees. Name yourself with set_session_title as soon as the new direction is \
clear; that title is how a person tells this session from the one it came from.";

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
    /// The account's whole service bundle, for the control-plane tools — which
    /// reach agents, routines and environments alike, so unlike `memory` there
    /// is no single service to hold. `None` wherever the control plane is not
    /// wired, which is every test that does not exercise it.
    pub(super) services: Option<Arc<crate::projects::ProjectServices>>,
    pub(super) settings: AgentSettings,
    /// What a workflow step promises to return, and whether it may ask. Empty
    /// and false for every other kind of agent, which never gets the
    /// `submit_result` layer at all.
    pub(super) step_result: StepResultDef,
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
    pub(super) session: SessionRef,
    /// The plugin bundles this session selected, and the library that can say
    /// what they offer. Together they answer "is `/commit` a command?" from the
    /// database, with no runtime involved — which is what lets a prompt merely
    /// *starting* with a slash cost nothing.
    pub(super) plugins: Vec<String>,
    pub(super) plugin_library: Option<Arc<dyn crate::plugins::PluginProvisioner>>,
    /// The client the most recent `provide()` resolved. Cheap to keep —
    /// cloning shares the same in-flight-call tracking — and it is what lets
    /// [`SessionActor::cancel_run`](super::SessionActor) cancel without a
    /// fresh vendor round-trip.
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
        reg.get(&self.settings.model)
            .map(|e| e.provider.clone())
            .ok_or_else(|| format!("no provider registered for model '{}'", self.settings.model))
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
            .get(&self.settings.model)?
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
        self.settings.use_plugins.unwrap_or(true)
    }

    /// Install this agent's own plugin bundles into its own tree on the
    /// runtime.
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
        let mut names = self.settings.plugins.clone();
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
                .map_err(ContextError::retryable)?,
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
            .kind
            .broadcasts()
            .then(|| narration_pump(&self.session, self.kind.agent_key()))
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
    /// failures fall back to no records, exactly as the `SessionStart`
    /// bootstrap did. Acquiring the runtime is the only step that can fail the
    /// turn, and it fails it the same way `provide` would have, one step
    /// later.
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
        // `ContextProvider` docs), so the hooks below run against an agent
        // whose plugin tree has not been built yet — and a hook is itself a
        // plugin file. A runtime refuses a request naming an unprovisioned
        // agent, and `run_hooks` swallows that with `unwrap_or_default`, so
        // the hooks simply never fire.
        //
        // Provisioning here is the obvious fix and is NOT applied, because the
        // extra pre-turn round trip wedges a sub session's turn: with it,
        // three sub session tests and one subagent test hang; without it, all
        // 36 pass. That is a sub session-path fragility this change surfaces
        // rather than causes, and it needs its own diagnosis before this line
        // goes in. Before the hooks, because a hook *is* a plugin file. This
        // seam runs ahead of `provide` — see the `ContextProvider` docs — so
        // it is the first place an agent's tree can exist, and hooks fired
        // against an agent the runtime has never been told about are refused.
        // `run_hooks` swallows that with `unwrap_or_default`, so the failure
        // would be every plugin hook silently not running.
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
            let event = match self.kind {
                SessionAgentKind::Sub(id) => ServerHookEvent::SubagentStart(SubagentStartInput {
                    agent_id: id.to_string(),
                    agent_type: self.agent_type(),
                }),
                SessionAgentKind::Main
                | SessionAgentKind::Step(_)
                | SessionAgentKind::SubSession(_) => {
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
        let settings = &self.settings;
        let def = session_run_def(settings);
        // Set only by a typed subagent, whose plugin definition may narrow what
        // the session grants it. See `Contexts::tool_narrowing`.
        let mut tool_narrowing: Option<Vec<String>> = None;
        let use_plugins = settings.use_plugins.unwrap_or(true);
        // Preparation progress is main-only: subagents are quiet by design.
        let broadcast = self.kind.broadcasts();

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
        // Cached *after* the sink is attached, not before: `Stop` runs its
        // hooks through this handle once the turn is over, and a sink-less
        // clone would run them and drop every record on the floor.
        // Cancellation is unaffected — in-flight tracking is shared across
        // clones.
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
            emit_progress(
                &self.session,
                self.kind.agent_key(),
                "scanning_workspace",
                None,
            )
            .await;
        }
        let (ws, shared_scan) = scan_workspace(&runtime_client, None).await;
        // No `SessionStart` here any more. It used to fire on this line, once
        // per *run* — `provide` is per-run — so every turn re-ran every start
        // hook, always reporting `source: "startup"`. It now fires once per
        // agent load at `start_hooks`, early enough for its context to reach
        // the turn that triggered it.
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
            // Intersected with the session's own selection, never substituted
            // for it. An agent definition is a file inside a plugin: it may say
            // which of the tools this session already grants it wants, and must
            // not be able to grant itself one the session withheld.
            //
            // Rides to the actor as `Contexts::tool_narrowing` rather than
            // rewriting `def`: the actor built its params from the def at spawn
            // and never reads it again, so a mutation here would go nowhere.
            tool_narrowing = Some(match &def.allowed_tools {
                None => allowed,
                Some(session) => allowed
                    .into_iter()
                    .filter(|t| session.contains(t))
                    .collect(),
            });
        }
        // A declared `model` is honoured only when horsie actually has it.
        // Every model declared in the wild is an alias (`inherit`, `sonnet`,
        // `opus`), and mapping those onto whatever the catalogue holds would
        // let a plugin author switch a kimi session to Anthropic by writing a
        // word in a file.
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
                // Never fatal: a plugin bringing a broken server must not stop
                // a session that merely happens to load it.
                Err(e) => tracing::warn!(
                    session = %self.session_id,
                    error = %e,
                    "plugin MCP discovery failed; continuing without those tools"
                ),
            }
        }
        let base: Arc<dyn Toolbox> =
            DefaultToolboxFactory.for_agent(runtime_client.clone(), ws.names(), use_plugins, mcp);
        let (with_memory, memory_index) =
            build_memory_layer(base, self.memory.clone(), settings).await?;
        let (with_memory, control_index) =
            build_control_layer(with_memory, self.services.as_ref(), settings, self.kind);
        let with_memory =
            build_authoring_layer(with_memory, self.services.as_ref(), settings, self.kind);
        // Spawns and invocations attribute to the actual agent making them —
        // the main agent's id is the session's.
        let caller = match self.kind {
            SessionAgentKind::Main => self.session_id,
            SessionAgentKind::Step(id)
            | SessionAgentKind::SubSession(id)
            | SessionAgentKind::Sub(id) => id,
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
        // Every kind of agent may invoke a workflow — main, subagents, steps
        // and sub sessions alike; the session gates at call time (depth, live
        // runs). The saved workflows ride in the tool description so the model
        // knows what exists; with none saved the tools are not offered at all,
        // so a session without workflows sees exactly the toolbox it saw
        // before they existed. Re-read per turn, where this toolbox is
        // rebuilt, so a workflow saved mid-session appears at the next turn.
        let workflow_catalog: Vec<(String, String)> = match &self.services {
            Some(services) => services
                .workflows
                .list()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|w| (w.name, w.description))
                .collect(),
            None => Vec::new(),
        };
        let with_spawn: Arc<dyn Toolbox> = match (&self.services, workflow_catalog.is_empty()) {
            (Some(services), false) => Arc::new(
                crate::sessions::invoke_workflow_tool::InvokeWorkflowToolbox::new(
                    with_spawn,
                    self.session.clone(),
                    caller,
                    services.clone(),
                    workflow_catalog,
                ),
            ),
            (Some(_), true) | (None, _) => with_spawn,
        };
        // `/fork`, addressed to the model. Sessions only: a subagent's history
        // is delegated work and a step's belongs to the run, so neither has a
        // branch to take — the same rule the composer's `/fork` follows. An
        // unattended session is excluded for the reason it has no `ask_user`:
        // a sub session is a session, and nobody is there to have one.
        let with_spawn: Arc<dyn Toolbox> = match self.kind {
            SessionAgentKind::Main | SessionAgentKind::SubSession(_) if !self.unattended => {
                Arc::new(crate::sessions::sub_session_tool::SubSessionToolbox::new(
                    with_spawn,
                    self.session.clone(),
                    caller,
                ))
            }
            SessionAgentKind::Main
            | SessionAgentKind::SubSession(_)
            | SessionAgentKind::Step(_)
            | SessionAgentKind::Sub(_) => with_spawn,
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
            // A step gets `submit_result` instead of the title layer — its
            // title belongs to the run rather than to one step — and `ask_user`
            // only when the definition says it is interactive and somebody is
            // there to answer.
            SessionAgentKind::Step(_) => {
                let result = crate::sessions::workflow::StepResultToolbox::wrap(
                    with_spawn,
                    self.step_result.outcomes.clone(),
                    self.step_result.fields.clone(),
                );
                if self.step_result.interactive && !self.unattended {
                    Arc::new(AskUserToolbox::new(result))
                } else {
                    result
                }
            }
            // A sub session takes the main agent's arms: it is a session, so
            // it can ask the user — and it names *itself*, not the session.
            SessionAgentKind::SubSession(id) => {
                let inner: Arc<dyn Toolbox> = Arc::new(AskUserToolbox::new(with_spawn));
                Arc::new(SessionTitleToolbox::for_sub_session(
                    inner,
                    self.session.clone(),
                    id,
                ))
            }
            SessionAgentKind::Sub(_) => with_spawn,
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
        let suffix: Option<&str> = match &self.kind {
            SessionAgentKind::Main if self.unattended => Some(UNATTENDED_PROMPT_SUFFIX),
            SessionAgentKind::Main => None,
            SessionAgentKind::Step(_) => Some(STEP_PROMPT_SUFFIX),
            SessionAgentKind::SubSession(_) => Some(FORK_PROMPT_SUFFIX),
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
            emit_progress(&self.session, self.kind.agent_key(), "ready", None).await;
        }
        Ok(Contexts {
            provider,
            toolbox,
            tool_narrowing,
            system_prompt,
            context_window: crate::agent_loop::compaction_window(
                self.settings.auto_compact,
                self.context_window(),
            ),
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
    use crate::sessions::addressing::SessionInbox;

    use crate::agent_loop::{ContextProvider, Contexts, StartTurn};
    use horsie_models::hooks::HookAction;
    use std::sync::Arc;
    use uuid::Uuid;

    /// The gate, at the layer that applies it: off unless the preset says so,
    /// and never for an agent that is not the session's main one.
    #[tokio::test]
    async fn control_tools_reach_only_a_main_agent_that_asked_for_them() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::testing::state(dir.path()).build().await;
        let services = state.services().await;
        let base: Arc<dyn Toolbox> = Arc::new(horsie_agentcore::EmptyToolbox);

        let mut settings = AgentSettings {
            source: crate::sessions::spec::AgentSource::AdHoc,
            model: "m".into(),
            allowed_tools: None,
            use_plugins: None,
            max_iterations: None,
            max_retries: 0,
            mcp_servers: Vec::new(),
            memory_spaces: Vec::new(),
            thinking_effort: None,
            max_concurrent_subagents: None,
            instructions: None,
            auto_compact: None,
            plugins: Vec::new(),
        };
        let (toolbox, index) = build_control_layer(
            base.clone(),
            Some(&services),
            &settings,
            SessionAgentKind::Main,
        );
        assert!(
            !toolbox
                .specs()
                .iter()
                .any(|s| s.name.starts_with("horsie_")),
            "a preset that never asked must not get them"
        );
        assert!(index.is_empty());

        settings.allowed_tools = Some(vec!["horsie_agents".into()]);
        let (toolbox, index) = build_control_layer(
            base.clone(),
            Some(&services),
            &settings,
            SessionAgentKind::Main,
        );
        assert!(toolbox.specs().iter().any(|s| s.name == "horsie_agents"));
        assert!(index.contains("agents {"), "{index}");

        for kind in [
            SessionAgentKind::Sub(Uuid::new_v4()),
            SessionAgentKind::Step(Uuid::new_v4()),
            SessionAgentKind::SubSession(Uuid::new_v4()),
        ] {
            let (toolbox, _) = build_control_layer(base.clone(), Some(&services), &settings, kind);
            assert!(
                !toolbox
                    .specs()
                    .iter()
                    .any(|s| s.name.starts_with("horsie_")),
                "a non-main agent inherits the setting but must not inherit the authority"
            );
        }
    }

    #[tokio::test]
    async fn subagent_toolbox_strips_session_metadata_tools() {
        let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;

        let build = |kind: SessionAgentKind| SessionContextProvider {
            agent_type: None,
            runtimes: f.deps.runtimes.provider(
                id.to_string(),
                "i1".to_string(),
                false,
                "mock".into(),
                crate::sessions::spec::SessionSpec::for_vendor("mock"),
            ),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            services: None,
            settings: agent_settings_fixture(),
            step_result: StepResultDef::default(),
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
        let mut settings = agent_settings_fixture();
        settings.max_concurrent_subagents = Some(0);
        let provider = SessionContextProvider {
            runtimes: f.deps.runtimes.provider(
                id.to_string(),
                "i1".to_string(),
                false,
                "mock".into(),
                crate::sessions::spec::SessionSpec::for_vendor("mock"),
            ),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            services: None,
            settings,
            step_result: StepResultDef::default(),
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
            runtimes: f.deps.runtimes.provider(
                id.to_string(),
                "i1".to_string(),
                false,
                "mock".into(),
                crate::sessions::spec::SessionSpec::for_vendor("mock"),
            ),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            services: None,
            settings: agent_settings_fixture(),
            step_result: StepResultDef::default(),
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
        assert!(!tools.contains(&crate::sessions::ask_tool::ASK_USER_TOOL.to_string()));
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
        assert!(names(&attended).contains(&crate::sessions::ask_tool::ASK_USER_TOOL.to_string()));
        assert!(!attended.system_prompt.unwrap().contains("# Unattended run"));
    }

    #[test]
    fn a_subagent_gets_its_own_runtime_identity() {
        let client = horsie_runtime_host::RuntimeClient::detached(
            horsie_runtime_host::MockTransport::ok(""),
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
            crate::agent_loop::start_blocked(&prep.records).as_deref(),
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
        // Asserted on the narrowing rather than on `toolbox`, because the
        // toolbox handed back here is deliberately unfiltered: the selection is
        // applied by the actor, once, after the last layer is stacked on.
        let tools = contexts.tool_narrowing.expect("a typed agent narrows");
        assert!(tools.contains(&"read_file".to_string()), "{tools:?}");
        assert!(tools.contains(&"grep".to_string()), "{tools:?}");
        assert!(
            !tools.contains(&"bash".to_string()),
            "the narrowing must exclude what it did not name: {tools:?}"
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
        let tools = contexts.tool_narrowing.expect("a typed agent narrows");
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
            runtimes: f.deps.runtimes.provider(
                id.to_string(),
                "i1".to_string(),
                false,
                "mock".to_string(),
                crate::sessions::spec::SessionSpec::for_vendor("mock"),
            ),
            registry: f.deps.provider_registry.clone(),
            mcp: None,
            memory: None,
            services: None,
            settings: agent_settings_fixture(),
            step_result: StepResultDef::default(),
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

    /// `SessionStart` used to fire from `provide()`, which is per-run — so
    /// every turn re-ran every start hook, always reporting `source:
    /// "startup"`. It fires once per agent load now; `UserPromptSubmit` is the
    /// one that belongs to every turn.
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

    /// A subagent is not a session. The call fired `SessionStart` for one
    /// before this, because it was not gated on the agent's kind at all — so a
    /// hook matching `startup` fired again for every subagent spawned.
    #[tokio::test]
    async fn a_subagent_fires_subagent_start_never_session_start() {
        let (f, session) = stop_harness(vec![]).await;
        spawn_sub(&session, "research", "dig into it").await;
        // Waited for on the *last* of the events asserted about, not the first.
        // `SessionStart` here belongs to the turn the main agent runs after the
        // subagent reports back, so stopping at `SubagentStart` left the
        // assertion reading a list the run had not finished writing — and the
        // count it wanted was the one still to come.
        for _ in 0..200 {
            if f.agent.hook_events().contains(&"SessionStart") {
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

    /// A session stand-in that keeps whatever progress it is told about, so a
    /// test can watch the narration pump without assembling a whole session.
    /// Every progress report a session was told about: the stage, and whatever
    /// detail came with it.
    type Reported = Arc<Mutex<Vec<(String, Option<String>)>>>;

    struct RecordingSession(Reported);

    #[async_trait]
    impl horsie_actor::EventSourcedActor for RecordingSession {
        type Command = SessionInbox;
        type Event = ();
        type State = ();

        fn persistence_id(&self) -> horsie_actor::PersistenceId {
            horsie_actor::PersistenceId::new("test", "recording-session")
        }

        fn initial_state() {}

        fn apply_event((): (), (): ()) {}

        async fn handle_command(
            &mut self,
            (): &(),
            cmd: SessionInbox,
            _ctx: &mut horsie_actor::ActorContext<SessionInbox>,
        ) -> super::super::CommandEffect<()> {
            if let SessionCommand::Core(CoreCommand::Progress { stage, detail, .. }) = cmd.cmd {
                self.0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((stage, detail));
            }
            super::super::CommandEffect::none()
        }
    }

    /// A context provider over a vendor that has to boot something, reporting
    /// into a session that keeps whatever it is told.
    fn booting_provider(seen: &Reported, kind: SessionAgentKind) -> SessionContextProvider {
        let mut vendors = std::collections::HashMap::new();
        vendors.insert(
            "mock".to_string(),
            Arc::new(BootingVendor) as Arc<dyn crate::runtime_vendor::RuntimeVendor>,
        );
        let vendors = Arc::new(std::sync::RwLock::new(vendors));
        let id = Uuid::new_v4();
        let session = SessionRef::new(
            crate::testing::spawn_detached(
                &horsie_actor::ActorSystem::new(Arc::new(horsie_actor::InMemoryJournal::new())),
                RecordingSession(seen.clone()),
            ),
            crate::projects::ProjectId::generate(),
            id,
            None,
        );
        SessionContextProvider {
            agent_type: None,
            runtimes: crate::runtime_manager::test_runtime_manager(&vendors).provider(
                id.to_string(),
                "i1".to_string(),
                false,
                "mock".into(),
                crate::sessions::spec::SessionSpec::for_vendor("mock"),
            ),
            registry: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            mcp: None,
            memory: None,
            services: None,
            settings: agent_settings_fixture(),
            step_result: StepResultDef::default(),
            session_id: id,
            kind,
            unattended: false,
            session,
            plugins: Vec::new(),
            plugin_library: None,
            last_client: Mutex::new(None),
        }
    }

    /// Whatever the session was told, once it has had a chance to hear it.
    async fn settled(seen: &Reported) -> Vec<(String, Option<String>)> {
        for _ in 0..200 {
            if !seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        seen.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The wait a person actually sits through, and what it says. An
    /// acquisition can hold here for minutes while a machine resumes; the
    /// vendor describes it the whole time, and those words belong in the log of
    /// the agent that is waiting, under the stage it already announced.
    #[tokio::test]
    async fn a_vendors_account_of_an_acquisition_reaches_the_agents_log() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = booting_provider(&seen, SessionAgentKind::Main);
        provider.runtime_client().await.expect("acquire");

        assert_eq!(
            settled(&seen).await,
            vec![(
                "acquiring_runtime".to_string(),
                Some(BOOTING_ACQUIRE.to_string())
            )],
            "the vendor's account of the wait has to reach the log, under the \
             stage the agent is actually in"
        );
    }

    /// And a subagent stays quiet, exactly as it does for every other
    /// preparation stage: its progress reaches a reader as the parent's
    /// `SubAgent` entry, not as a second narration of the same sandbox.
    /// Provisioning reaches the runtime before the hooks that read what it
    /// installed.
    ///
    /// A hook is a file inside a plugin bundle, so hooks fired against an agent
    /// whose tree does not exist find nothing — and a runtime refuses a request
    /// naming an agent it has never been told about. `run_hooks` swallows that
    /// refusal with `unwrap_or_default`, so getting this order wrong is not an
    /// error anywhere: it is every plugin hook silently never running.
    ///
    /// Ordering rather than mere presence, because `start_hooks` runs *ahead*
    /// of `provide` — provisioning only in `provide` looks correct and is
    /// exactly the bug.
    #[tokio::test]
    async fn an_agent_is_provisioned_before_its_hooks_run() {
        let (f, session, id) = catalog_harness_with(Vec::new(), Vec::new()).await;
        let provider = catalog_provider(&f, &session, id);

        provider
            .start_hooks(StartTurn {
                start_source: Some(horsie_models::runtime::SessionStartSource::Startup),
                prompt: Some("hello".to_string()),
            })
            .await
            .expect("prepare");

        let relayed = f.agent.relayed();
        let first_provision = relayed.iter().position(|k| k == "ProvisionAgent");
        let first_hooks = relayed.iter().position(|k| k == "RunHooks");
        assert!(
            first_provision.is_some(),
            "the agent must be provisioned at all: {relayed:?}"
        );
        assert!(
            first_hooks.is_some(),
            "the hooks must actually have run: {relayed:?}"
        );
        assert!(
            first_provision < first_hooks,
            "provisioning must precede the hooks that read it: {relayed:?}"
        );
    }

    #[tokio::test]
    async fn a_subagent_narrates_nothing() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = booting_provider(&seen, SessionAgentKind::Sub(Uuid::new_v4()));
        provider.runtime_client().await.expect("acquire");
        // Long enough for a line to have arrived if one were ever sent.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
    }
}
