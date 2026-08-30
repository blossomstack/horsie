use crate::agent_loop::agent_actor::UsageTotal;
use crate::agent_loop::mcp_toolbox::CompositeToolbox;
use async_trait::async_trait;
use horsie_agentcore::{LlmProvider, ToolCallError, ToolOutcome, ToolSpec, Toolbox, ToolboxImpl};
use horsie_runtime_host::{RuntimeClient, add_runtime_tools};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

/// Name of the builtin terminal tool an agent calls to finish its turn —
/// either delivering its structured output or asking the user a question. The
/// subset of an agent's configuration that [`ToolboxFactory::for_agent`] and
/// [`AgentParams::from_def`](crate::agent_loop::AgentParams::from_def)
/// actually need: tool shape and turn-shape, and nothing about *where this
/// agent sits*. A workflow step builds one from its definition and preset; an
/// interactive session builds one from its settings.
#[derive(Debug, Clone, Default)]
pub struct AgentRunDef {
    pub system_prompt: Option<String>,
    pub max_iterations: Option<u32>,
    pub max_retries: Option<u32>,
    pub allowed_tools: Option<Vec<String>>,
}

/// Name of the builtin tool an agent calls to load a skill's full instructions
/// on demand (progressive disclosure). Always advertised; re-scans the
/// workspace live.
pub const SKILL_TOOL: &str = "skill";

/// Name of the builtin tool that re-scans the workspace(s) and returns the
/// current catalog (path, git status, instruction presence, skills). Always
/// advertised, like `skill`. Replaces the former `list_skills`.
pub const INSPECT_WORKSPACE_TOOL: &str = "inspect_workspace";

/// One question an agent parked on, and the call that asked it.
///
/// Serializable because an agent folds its own pending questions into state:
/// what it is waiting on is durable agent state, exactly like its timers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskedQuestion {
    /// `None` only for a pre-#62 journal, where the call id was not recorded.
    pub tool_call_id: Option<String>,
    pub question: String,
    /// The suggested answers the model offered, if any.
    ///
    /// Carried here so an inbox row can render the same answer control the
    /// transcript does. The transcript reads them off the echoed tool call it
    /// is already showing; nothing outside a transcript has that, and a
    /// question whose choices are only visible in one of the two places it can
    /// be answered is answerable differently in each.
    #[serde(default)]
    pub choices: Vec<String>,
    /// Whether several `choices` may be picked at once. Meaningless without
    /// them, and false is the honest default for a question that offered none.
    #[serde(default)]
    pub multiple: bool,
}

/// A terminal outcome an [`AgentActor`](crate::agent_loop::AgentActor) reports
/// to whoever spawned it — the workflow that orchestrates it, or an
/// interactive session.
///
/// Every variant names the agent that reported it, because one owner hosts
/// many: a session's sink receives from its main agent, from every subagent
/// and from every workflow step, and routing is the first thing it does. The
/// id is the agent's *journal* id — the session's own for a main agent, since
/// its transcript is the session's, and the agent's own for anything else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentOutcome {
    /// The agent started a turn off its own queue.
    ///
    /// Not a terminal outcome, and the one report that flows *before* the work
    /// rather than after it. It exists because the agent, not its owner,
    /// decides when its queue becomes a turn — so the owner can no longer
    /// learn that a turn began by being the thing that began it.
    Started { agent: Uuid },
    /// A turn the process died inside, found at recovery and reported by the
    /// only thing that can tell: the agent whose turn it was.
    ///
    /// Delivered from `on_recovery_complete`, which the runtime runs before the
    /// first live command — so an agent physically cannot begin a new turn
    /// before this has been sent, and an owner reading it never has to work out
    /// *which* turn it means. That ordering is the whole mechanism; there is no
    /// fence and no turn number anywhere.
    Interrupted { agent: Uuid },
    /// The agent produced its output (structured, or its final text).
    Concluded { agent: Uuid, output: Value },
    /// The agent paused to ask the user. A turn may ask more than once — each
    /// question is its own tool call, and they are answered together, since the
    /// run cannot resume while any of them is still missing a result.
    Asked {
        agent: Uuid,
        asks: Vec<AskedQuestion>,
    },
    /// The agent parked itself awaiting its timers.
    Parked { agent: Uuid },
    /// The agent run failed. `recoverable` is about the *run* — whether trying
    /// it again could work — while `terminal` is about the agent's owner: its
    /// sandbox is gone and no later message can bring it back. A provider `401`
    /// is neither recoverable nor terminal; fix the key and the next turn runs.
    Failed {
        agent: Uuid,
        error: String,
        recoverable: bool,
        terminal: bool,
    },
    /// A run completed successfully, carrying this agent's freshly-updated
    /// cumulative usage. Delivered alongside `Concluded`/`Asked` (never
    /// `Failed`/`Parked`, which have no completed run's usage to report), so
    /// a parent hosting multiple agents can maintain its own durable
    /// session-level usage total without waking an idle agent to ask for it.
    UsageRecorded {
        agent: Uuid,
        usage_total: UsageTotal,
        /// How full this agent's context is now. Reported alongside the total
        /// so its owner can bank both, and answer for either without waking
        /// the agent again.
        context_tokens: u32,
    },
    /// A `/summary-n-fork` turn produced the summary the sub sessions
    /// branching off this agent are waiting on.
    ///
    /// Not a terminal outcome, and not about this agent at all: its own
    /// history is untouched, and this turn still ends however it was going to.
    /// Delivered as its own report because the summary belongs to a
    /// *different* session, and the owner is the only thing that can reach it.
    ///
    /// `sub sessions` is a list because sub sessions queued into one turn
    /// share a branch point, so one provider call serves all of them.
    SeedSummary {
        agent: Uuid,
        sub_sessions: Vec<Uuid>,
        result: Result<String, String>,
    },
}

/// Where an [`AgentActor`](crate::agent_loop::AgentActor) delivers its
/// [`AgentOutcome`]. Implemented by the workflow (mapping outcomes into its
/// own commands) and by the session server; keeps the agent decoupled from any
/// one parent's command enum.
#[async_trait]
pub trait AgentOutcomeSink: Send + Sync {
    async fn deliver(&self, outcome: AgentOutcome);
}

/// The per-run contexts an agent run executes within — the provider it calls,
/// the toolbox it acts through, and the system prompt that frames it. Produced
/// fresh by [`ContextProvider::provide`] at the top of every run. The timer /
/// `task_list` wrappers are layered on by the
/// [`AgentActor`](crate::agent_loop::AgentActor) itself, not here.
pub struct Contexts {
    pub provider: Arc<dyn LlmProvider>,
    /// The agent's tools, already composed but **not** narrowed: the selection
    /// is applied once, outermost, by the actor that stacks the last layers on.
    pub toolbox: Arc<dyn Toolbox>,
    /// A further narrowing this run is subject to, on top of the agent's own
    /// selection. `None` — the usual case — means no extra narrowing.
    ///
    /// Exists for one caller: a subagent whose type comes from a plugin's agent
    /// definition, where the definition's `tools:` list may narrow what the
    /// session already grants and must never widen it. Carried as a separate
    /// list rather than folded into the agent's own selection because the two
    /// are decided by different people — the operator picks the session's, a
    /// plugin author picks this — and stacking two filters cannot accidentally
    /// widen, whichever way round they are applied.
    pub tool_narrowing: Option<Vec<String>>,
    /// The composed system prompt, when the context layer owns it (interactive
    /// sessions compose it from a live workspace scan). `None` means "use the
    /// agent's configured prompt" — workflow agents carry a static prompt in
    /// their params and return `None` here.
    pub system_prompt: Option<String>,
    /// This run's model's context window, when its card declares one.
    ///
    /// Resolved here rather than by the agent because an agent does not know
    /// which models are configured — the same reason the HTTP layer has had to
    /// attach it to the agent document. `None` disables automatic compaction
    /// for this run: without a window there is no share of it to trigger on,
    /// and a guessed default would either compact a session that had room or
    /// fail to compact one that did not.
    pub context_window: Option<u32>,
}

/// What a run's pre-start hooks need to know about the turn about to begin.
///
/// Split across the two layers that each hold half the answer: the agent actor
/// knows whether this load has already fired its start hook and whether the
/// turn begins on a user message; the provider knows whether this agent is a
/// session or a subagent, and so which event that start actually is.
#[derive(Debug, Clone)]
pub struct StartTurn {
    /// `Some(source)` when this agent load has not yet fired its start hook.
    /// `Startup` for a fresh agent, `Resume` for one recovered from a journal —
    /// the only two of the spec's five lifecycle transitions horsie has.
    pub start_source: Option<horsie_models::runtime::SessionStartSource>,
    /// The user prompt this run starts on, when it has one.
    pub prompt: Option<String>,
}

/// Provides the per-run [`Contexts`] an
/// [`AgentActor`](crate::agent_loop::AgentActor) needs.
///
/// `provide` is called on the run's *spawned task* — never an actor mailbox —
/// at the top of every run path (fresh input, resume, timer wake).
/// Implementations do their heavy, idempotent setup here (rehydrate a
/// suspended runtime, reconnect a dropped MCP, scan the workspace); it must be
/// cheap when everything is already live. Spawning an agent does *not* call
/// `provide`, so an agent can be recovered purely to answer read queries
/// without touching any runtime.
#[async_trait]
pub trait ContextProvider: Send + Sync {
    async fn provide(&self) -> Result<Contexts, ContextError>;

    /// Whether this provider has hooks to fire before a run starts.
    ///
    /// `false` skips the prepare round-trip entirely, which is what keeps a
    /// session with no plugins exactly as fast as it was before the seam
    /// existed. Answered without I/O, on the mailbox.
    fn has_start_hooks(&self) -> bool {
        false
    }

    /// Fire the hooks that must run *before* the turn snapshots its history —
    /// `SessionStart` / `SubagentStart` and `UserPromptSubmit`.
    ///
    /// Called on a spawned task, never a mailbox, exactly like `provide`. It
    /// runs before the snapshot because that is the only place a record can
    /// land early enough to reach the very first turn's prompt: `provide` runs
    /// after it, which is why the context these hooks inject used to bypass the
    /// session entirely.
    ///
    /// Returns the records to journal, and optionally a rewritten prompt.
    /// Their consequences are read off them by the caller — the agent
    /// translates the context and [`crate::agent_loop::start_blocked`] reads a
    /// refusal — so this never decides anything itself.
    async fn start_hooks(&self, turn: StartTurn) -> Result<TurnPreparation, ContextError> {
        let _ = turn;
        Ok(TurnPreparation::default())
    }

    /// Fire one of the compaction hooks and hand back what they did.
    ///
    /// Separate from `start_hooks` because a compaction is not a turn: it can
    /// happen in the middle of one, and there is no prompt for a hook to
    /// rewrite. Called on the run's task, never a mailbox.
    ///
    /// The default is "no hooks ran", which is also what a session with no
    /// plugins and a workflow step both mean.
    async fn compaction_hooks(
        &self,
        event: horsie_models::runtime::ServerHookEvent,
    ) -> Vec<horsie_models::hooks::HookRecord> {
        let _ = event;
        Vec::new()
    }
}

/// What the pre-run seam produced.
///
/// `records` are journaled and their consequences read off them by the caller.
/// `message` is set only when something *rewrote* the turn's input — today, a
/// slash command expanding into its template. `None` means "unchanged", which
/// is not the same as "empty": a turn with no user message at all is a resume.
#[derive(Debug, Default)]
pub struct TurnPreparation {
    pub records: Vec<horsie_models::hooks::HookRecord>,
    pub message: Option<String>,
}

/// Why a run's contexts could not be produced.
///
/// `terminal` is the whole point of the type: it says the owner can never run
/// this agent again — its sandbox is gone for good — as opposed to the ordinary
/// case of a setup that failed this time and may well work on the next message.
/// Classifying that by matching on the message text is how it was done before,
/// and it made every reworded error a silent behaviour change.
#[derive(Debug, Clone)]
pub struct ContextError {
    pub message: String,
    pub terminal: bool,
}

impl ContextError {
    /// A failure the caller may retry — the default reading of any error.
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            terminal: false,
        }
    }

    /// A failure that ends this agent's owner for good.
    pub fn terminal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            terminal: true,
        }
    }
}

impl From<String> for ContextError {
    fn from(message: String) -> Self {
        Self::retryable(message)
    }
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The window a run gets, from what the session asked for and what its model's
/// card declares.
///
/// One function because the two reasons a session never compacts — it was
/// turned off, or the card names no window — must produce the same answer, and
/// the run must not be able to tell them apart. There is nothing for a run to
/// do differently in the two cases, and a second signal would be a second thing
/// to keep consistent.
#[must_use]
pub fn compaction_window(auto_compact: Option<bool>, card_window: Option<u32>) -> Option<u32> {
    if auto_compact == Some(false) {
        return None;
    }
    card_window
}

/// A [`ContextProvider`] that hands back the same contexts every time — built
/// once and reused, by any owner whose agent's runtime and toolbox are
/// fixed for the agent's life, so `provide` is a trivial clone (and a recovery
/// self-resume gets them back unchanged).
pub struct FixedContextProvider {
    pub provider: Arc<dyn LlmProvider>,
    pub toolbox: Arc<dyn Toolbox>,
}

#[async_trait]
impl ContextProvider for FixedContextProvider {
    async fn provide(&self) -> Result<Contexts, ContextError> {
        Ok(Contexts {
            provider: self.provider.clone(),
            toolbox: self.toolbox.clone(),
            tool_narrowing: None,
            system_prompt: None,
            // A fixed-context agent is a workflow step or a test fixture; it
            // has no model card to read a window from and never auto-compacts.
            context_window: None,
        })
    }
}

/// Resources injected into an [`AgentActor`](crate::agent_loop::AgentActor) at
/// spawn. Holds only cheap, stable wiring — the volatile per-run contexts
/// (provider, toolbox, prompt) are obtained lazily via
/// [`ContextProvider::provide`], so spawning is free of any runtime/MCP/scan
/// work.
#[derive(Clone)]
pub struct AgentRuntimeContext {
    /// Per-run context supplier; see [`ContextProvider`].
    pub context_provider: Arc<dyn ContextProvider>,
    /// Where this agent announces that it has moved, for readers to wait on.
    ///
    /// Injected rather than created by the actor so its lifetime can be longer
    /// than the actor's: a session agent's belongs to the supervisor, which is
    /// what lets an idle offload leave a reader waiting instead of
    /// disconnecting it into a reconnect-then-reload loop.
    pub revision: Arc<tokio::sync::watch::Sender<crate::sessions::Revision>>,
    /// Whoever spawned this agent; receives its terminal outcome.
    pub parent: Arc<dyn AgentOutcomeSink>,
    /// This agent's identity: the id it journals under, and the id it names
    /// itself by in every [`AgentOutcome`] it reports.
    ///
    /// The session's own id for a main agent — its transcript *is* the
    /// session's — and the agent's own id for a subagent or a workflow step.
    /// One id space, which is what lets an owner hosting all three route by
    /// comparison alone.
    pub journal_id: Uuid,
    /// Whether the session this agent belongs to has a runtime to run on, as of
    /// this spawn.
    ///
    /// The one input the agent's drain gate cannot derive: it can see that it
    /// is running and that it is parked, but a sandbox still being built is
    /// its owner's business entirely. Starting a turn without one asks a
    /// vendor for a runtime it has never heard of, and the vendor's answer is
    /// terminal.
    ///
    /// Only the *starting* value. Changes arrive as
    /// [`LifecycleEvent::Runtime`](horsie_agentcore::LifecycleEvent::Runtime)
    /// records, which this agent is sent anyway so a reader can see them — so
    /// there is no second channel carrying the same fact, and an agent spawned
    /// after a change is simply built with the new answer.
    ///
    /// An agent whose owner has no sandbox to wait on passes `true`.
    pub ready: bool,
}

/// Builds the toolbox an agent runs with: the runtime-backed tools plus any
/// server-side MCP ones, and nothing about how its turn ends — a tool that ends
/// a run says so itself, and the layer adding one is stacked by the caller.
///
/// Takes no `AgentRunDef`: what an agent may *call* is decided once,
/// outermost, by [`FilteredToolbox`]. A factory that narrowed here could only
/// ever narrow its own layer, which is the bug that made a tool selection mean
/// two different things depending on which tool you asked about.
pub trait ToolboxFactory: Send + Sync + 'static {
    fn for_agent(
        &self,
        runtime_client: RuntimeClient,
        workspace_names: Vec<String>,
        use_plugins: bool,
        mcp: crate::agent_loop::McpToolboxes,
    ) -> Arc<dyn Toolbox>;
}

/// Default factory: the standard runtime-backed tools, composed with whatever
/// server-side MCP toolboxes this agent connected.
pub struct DefaultToolboxFactory;

impl ToolboxFactory for DefaultToolboxFactory {
    fn for_agent(
        &self,
        runtime_client: RuntimeClient,
        workspace_names: Vec<String>,
        use_plugins: bool,
        mcp: crate::agent_loop::McpToolboxes,
    ) -> Arc<dyn Toolbox> {
        let client = runtime_client.clone();
        let runtime = add_runtime_tools(ToolboxImpl::new(), runtime_client);
        // The runtime tools and any server-side MCP toolboxes, flattened into
        // one tool set. MCP names are not governed by a selection — see
        // `crate::tools` — so nothing narrows them here.
        let composed: Arc<dyn Toolbox> = if mcp.is_empty() {
            Arc::new(runtime)
        } else {
            // Composed even when *every* server failed: the composite is what
            // knows why they are missing, and a bare runtime toolbox would
            // answer a call for one with "no tool named …" all over again.
            let mut boxes: Vec<Arc<dyn Toolbox>> = Vec::with_capacity(1 + mcp.boxes.len());
            boxes.push(Arc::new(runtime));
            boxes.extend(mcp.boxes);
            Arc::new(CompositeToolbox::new(boxes).with_unavailable(mcp.unavailable))
        };
        // No filtering here any more: the selection is applied once, outermost,
        // in `AgentActor` — see `FilteredToolbox`. Narrowing at this depth was
        // what confined a selection to runtime and MCP tools.
        Arc::new(AgentToolbox {
            base: composed,
            runtime_client: client,
            workspace_names,
            use_plugins,
        })
    }
}

/// A toolbox = a base (permitted runtime tools) plus the always-present
/// `skill` and `inspect_workspace` tools. Those two re-scan the workspace live
/// on each call (no cached skill set), so a skill added mid-run is immediately
/// loadable, and both bypass the allowlist.
struct AgentToolbox {
    base: Arc<dyn Toolbox>,
    runtime_client: RuntimeClient,
    /// Names of the job's workspaces (stable for the job); used to apply the
    /// "optional iff single" rule and to list valid names in errors. The
    /// runtime owns the actual name→path resolution.
    workspace_names: Vec<String>,
    /// Whether this agent may see the shared plugin library (`horsie_shared`).
    use_plugins: bool,
}

impl AgentToolbox {
    /// Resolve the optional `workspace` argument of a skill-side tool to a
    /// concrete name. `None` is allowed only when there is exactly one
    /// workspace.
    fn resolve_workspace(&self, requested: Option<&str>) -> Result<String, ToolCallError> {
        match requested {
            Some(name) => {
                if self.workspace_names.iter().any(|n| n == name) {
                    Ok(name.to_string())
                } else {
                    Err(ToolCallError::InvalidInput(format!(
                        "unknown workspace '{name}'; available: {}",
                        self.workspace_names.join(", ")
                    )))
                }
            }
            None => match self.workspace_names.as_slice() {
                [only] => Ok(only.clone()),
                _ => Err(ToolCallError::InvalidInput(format!(
                    "specify a workspace: {}",
                    self.workspace_names.join(", ")
                ))),
            },
        }
    }
}

#[async_trait]
impl Toolbox for AgentToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.base.specs();
        specs.push(ToolSpec {
            name: SKILL_TOOL.to_string(),
            description:
                "Load the full instructions for a named skill in a workspace (see '# Workspaces' or inspect_workspace)."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string", "description": "The skill name." },
                    "workspace": { "type": "string", "description": "Which workspace the skill belongs to (see '# Workspaces'). Required when there is more than one workspace." }
                }
            }),
        });
        specs.push(ToolSpec {
            name: INSPECT_WORKSPACE_TOOL.to_string(),
            description:
                "Re-scan and show the current state of the workspace(s): path, git status, instruction-file presence, and available skills (name + description)."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Limit to one workspace (see '# Workspaces'). Omit to show all." }
                }
            }),
        });
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if name == SKILL_TOOL {
            let requested = input
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let requested_ws = input.get("workspace").and_then(Value::as_str);
            // Shared plugin library: addressed by the reserved `horsie_shared`
            // name, resolved against the shared skill set (not a job
            // workspace).
            if requested_ws == Some(crate::agent_loop::workspace::SHARED_WORKSPACE) {
                if !self.use_plugins {
                    return Err(ToolCallError::InvalidInput(
                        "the shared plugin library 'horsie_shared' is not enabled for this agent"
                            .to_string(),
                    ));
                }
                let (_, shared) =
                    crate::agent_loop::workspace::scan(&self.runtime_client, None).await;
                return match shared.skills.get(requested) {
                    Some(skill) => Ok(ToolOutcome::Result(Value::String(skill_body(skill)))),
                    None => Err(ToolCallError::InvalidInput(format!(
                        "unknown shared skill '{requested}'; available: {}",
                        shared.skills.names().join(", ")
                    ))),
                };
            }
            let ws_name = self.resolve_workspace(requested_ws)?;
            let (ws, _) =
                crate::agent_loop::workspace::scan(&self.runtime_client, Some(ws_name.clone()))
                    .await;
            let Some(info) = ws.find(&ws_name) else {
                return Err(ToolCallError::InvalidInput(format!(
                    "workspace '{ws_name}' is not available"
                )));
            };
            return match info.skills.get(requested) {
                Some(skill) => Ok(ToolOutcome::Result(Value::String(skill_body(skill)))),
                None => Err(ToolCallError::InvalidInput(format!(
                    "unknown skill '{requested}' in workspace '{ws_name}'; available: {}",
                    info.skills.names().join(", ")
                ))),
            };
        }
        if name == INSPECT_WORKSPACE_TOOL {
            let filter = input
                .get("workspace")
                .and_then(Value::as_str)
                .map(str::to_string);
            // Shared-only view.
            if filter.as_deref() == Some(crate::agent_loop::workspace::SHARED_WORKSPACE) {
                if !self.use_plugins {
                    return Err(ToolCallError::InvalidInput(
                        "the shared plugin library 'horsie_shared' is not enabled for this agent"
                            .to_string(),
                    ));
                }
                let (_, shared) =
                    crate::agent_loop::workspace::scan(&self.runtime_client, None).await;
                return Ok(ToolOutcome::Result(Value::String(
                    crate::agent_loop::workspace::shared_inspect(
                        &shared.skills,
                        shared.root.as_deref(),
                    ),
                )));
            }
            let (ws, shared) =
                crate::agent_loop::workspace::scan(&self.runtime_client, filter.clone()).await;
            let mut out = crate::agent_loop::workspace::inspect_result(&ws);
            // Append the shared library when listing everything for an
            // opted-in agent.
            if self.use_plugins && filter.is_none() {
                out.push_str("\n\n");
                out.push_str(&crate::agent_loop::workspace::shared_inspect(
                    &shared.skills,
                    shared.root.as_deref(),
                ));
            }
            return Ok(ToolOutcome::Result(Value::String(out)));
        }
        self.base.execute(name, input, tool_call_id).await
    }
}

/// A skill's body plus, when its directory is known, a hint pointing at it so
/// the agent can read sibling resources with the filesystem tools. The path is
/// absolute because that is the only addressing those tools take — and because
/// a shared skill's directory is not under any workspace, so nothing else
/// would resolve it.
fn skill_body(skill: &crate::agent_loop::workspace::Skill) -> String {
    match &skill.dir {
        Some(dir) => format!(
            "{}\n\n[resources] This skill's files are in {}/. \
             Read one with read_file(path=\"{}/<file>\").",
            skill.body, dir, dir,
        ),
        None => skill.body.clone(),
    }
}

/// Wraps a fully-composed toolbox and removes the tools this agent's selection
/// left out.
///
/// Applied once, outermost, so it reaches every layer — the runtime tools, the
/// timers, the subagent and workflow tools, the session's own. It used to sit
/// three layers down, which is why a selection could only ever speak for
/// runtime and MCP tools.
///
/// It filters by *two* sets, not one. `governed` is every name
/// [`crate::tools::catalog`] knows; a tool outside it is passed through
/// whatever the selection says. That is deliberate and load-bearing — MCP
/// tools, a plugin's MCP tools, `memory_*` and `submit_result` all have names
/// that no saved selection could have known, and are gated by their own
/// channels. See [`crate::tools`] for why each one.
pub struct FilteredToolbox {
    inner: Arc<dyn Toolbox>,
    allowed: HashSet<String>,
    governed: HashSet<String>,
}

impl FilteredToolbox {
    #[must_use]
    pub fn new(
        inner: Arc<dyn Toolbox>,
        allowed: HashSet<String>,
        governed: HashSet<String>,
    ) -> Self {
        Self {
            inner,
            allowed,
            governed,
        }
    }

    /// Wrap `inner` with the selection this agent runs under. A `None`
    /// selection is still a filter, not a bypass: it resolves to the default
    /// set, which excludes the control plane.
    #[must_use]
    pub fn apply(inner: Arc<dyn Toolbox>, selection: Option<&[String]>) -> Arc<dyn Toolbox> {
        Arc::new(Self::new(
            inner,
            crate::tools::resolve(selection),
            crate::tools::governed(),
        ))
    }

    fn permits(&self, name: &str) -> bool {
        !self.governed.contains(name) || self.allowed.contains(name)
    }
}

#[async_trait]
impl Toolbox for FilteredToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner
            .specs()
            .into_iter()
            .filter(|s| self.permits(&s.name))
            .collect()
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if !self.permits(name) {
            return Err(ToolCallError::InvalidInput(format!(
                "tool '{name}' is not permitted for this agent"
            )));
        }
        self.inner.execute(name, input, tool_call_id).await
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

    /// The value an ordinary tool answered with. Panics on a tool that ended
    /// the run, which no test here calls.
    fn value(outcome: ToolOutcome) -> Value {
        match outcome {
            ToolOutcome::Result(v) => v,
            ToolOutcome::StopRun => panic!("expected a value, got a run-stopping call"),
        }
    }
    use horsie_runtime_host::MockTransport;

    fn scan_with_skill(name: &str) -> horsie_models::runtime::WorkspaceScan {
        let content = "---\nname: git-bisect\ndescription: find bad commit\n---\nStep 1...";
        horsie_models::runtime::WorkspaceScan {
            name: name.into(),
            path: format!("/ws/{name}"),
            is_git_repo: false,
            instructions: None,
            // Absolute, as the runtime's glob produces it.
            skills: vec![horsie_models::runtime::ScannedFile {
                path: format!("/ws/{name}/.claude/skills/git-bisect/SKILL.md"),
                content: content.into(),
            }],
            platform: None,
        }
    }

    #[test]
    fn the_factory_no_longer_narrows_anything() {
        let client = RuntimeClient::detached(MockTransport::ok(""), "test-agent");
        let tb = DefaultToolboxFactory.for_agent(
            client,
            vec!["october".into()],
            false,
            crate::agent_loop::McpToolboxes::default(),
        );
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"read_file".to_string()));
    }

    #[test]
    fn a_selection_narrows_the_whole_stack() {
        let client = RuntimeClient::detached(MockTransport::ok(""), "test-agent");
        let inner = DefaultToolboxFactory.for_agent(
            client,
            vec!["october".into()],
            false,
            crate::agent_loop::McpToolboxes::default(),
        );
        let tb = FilteredToolbox::apply(inner, Some(&["bash".to_string()]));
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"bash".to_string()));
        assert!(!names.contains(&"read_file".to_string()));
        // `skill` and `inspect_workspace` are catalogued, so leaving them out
        // of a selection really does remove them. They used to bypass the
        // filter by sitting above it, which made "only bash" quietly untrue.
        assert!(!names.contains(&SKILL_TOOL.to_string()));
    }

    #[tokio::test]
    async fn a_tool_the_catalogue_does_not_know_is_never_filtered() {
        // Stands in for an MCP tool, a plugin's MCP tool, `memory_*` and
        // `submit_result` — every name a saved selection could not have known.
        let inner: Arc<dyn Toolbox> =
            horsie_agentcore::testkit::MockToolbox::echo("mcp__notion__search");
        let tb = FilteredToolbox::apply(inner, Some(&["bash".to_string()]));
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["mcp__notion__search".to_string()]);
        assert!(
            tb.execute("mcp__notion__search", json!({}), "tc1")
                .await
                .is_ok(),
            "an ungoverned tool must still be callable"
        );
    }

    #[test]
    fn an_absent_selection_still_filters_the_control_plane() {
        let inner: Arc<dyn Toolbox> = horsie_agentcore::testkit::MockToolbox::echo("horsie_agents");
        let tb = FilteredToolbox::apply(inner, None);
        assert!(
            tb.specs().is_empty(),
            "a session that never asked for the control plane must not get it"
        );
    }

    #[tokio::test]
    async fn skill_and_inspect_always_present() {
        let client = RuntimeClient::detached(MockTransport::ok(""), "test-agent"); // empty scan
        let tb = DefaultToolboxFactory.for_agent(
            client,
            vec!["october".into()],
            false,
            crate::agent_loop::McpToolboxes::default(),
        );
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&SKILL_TOOL.to_string()));
        assert!(names.contains(&INSPECT_WORKSPACE_TOOL.to_string()));
    }

    #[tokio::test]
    async fn skill_fetches_live_for_single_workspace_default() {
        let client = RuntimeClient::detached(
            MockTransport::ok("").with_scan(vec![scan_with_skill("october")]),
            "test-agent",
        );
        let tb = DefaultToolboxFactory.for_agent(
            client,
            vec!["october".into()],
            false,
            crate::agent_loop::McpToolboxes::default(),
        );

        // Single workspace → `workspace` may be omitted.
        let body = tb
            .execute(SKILL_TOOL, json!({ "name": "git-bisect" }), "tc1")
            .await
            .unwrap();
        // A workspace skill carries its directory too, so the agent can read
        // sibling resources without guessing the layout.
        assert_eq!(
            value(body),
            json!(
                "Step 1...\n\n[resources] This skill's files are in \
                 /ws/october/.claude/skills/git-bisect/. Read one with \
                 read_file(path=\"/ws/october/.claude/skills/git-bisect/<file>\")."
            )
        );

        let err = tb
            .execute(SKILL_TOOL, json!({ "name": "nope" }), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));

        let listed = tb
            .execute(INSPECT_WORKSPACE_TOOL, json!({}), "tc1")
            .await
            .unwrap();
        let listed = value(listed);
        let text = listed.as_str().unwrap();
        assert!(text.contains("## october — /ws/october"));
        // Directory relative to the workspace root named in the header above.
        assert!(
            text.contains("- git-bisect — .claude/skills/git-bisect/: find bad commit"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn skill_requires_workspace_when_multiple() {
        let client = RuntimeClient::detached(
            MockTransport::ok("").with_scan(vec![scan_with_skill("october")]),
            "test-agent",
        );
        let tb = DefaultToolboxFactory.for_agent(
            client,
            vec!["alpha".into(), "beta".into()],
            false,
            crate::agent_loop::McpToolboxes::default(),
        );
        // Omitting `workspace` with several workspaces is rejected before any
        // scan.
        let err = tb
            .execute(SKILL_TOOL, json!({ "name": "git-bisect" }), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
        // An unknown workspace name is also rejected.
        let err = tb
            .execute(
                SKILL_TOOL,
                json!({ "name": "git-bisect", "workspace": "zzz" }),
                "tc1",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    fn shared_skill() -> horsie_models::runtime::PluginSkill {
        horsie_models::runtime::PluginSkill {
            plugin: "sp".into(),
            rel_dir: "sp/skills/brainstorming".into(),
            content: "---\nname: brainstorming\ndescription: explore first\n---\nDo it.".into(),
        }
    }

    #[tokio::test]
    async fn shared_skill_loads_with_resource_hint_when_opted_in() {
        let client = RuntimeClient::detached(
            MockTransport::ok("")
                .with_shared_skills(vec![shared_skill()])
                .with_shared_root("/opt/plugins"),
            "test-agent",
        );
        let tb = DefaultToolboxFactory.for_agent(
            client,
            vec!["october".into()],
            true,
            crate::agent_loop::McpToolboxes::default(),
        );
        let body = tb
            .execute(
                SKILL_TOOL,
                json!({ "name": "brainstorming", "workspace": "horsie_shared" }),
                "tc1",
            )
            .await
            .unwrap();
        let body = value(body);
        let text = body.as_str().unwrap();
        assert!(text.contains("Do it."));
        // The library is not a workspace, so the hint must be an absolute path
        // — there is no `workspace` argument left to name it with.
        assert!(
            text.contains("read_file(path=\"/opt/plugins/sp/skills/brainstorming/<file>\")"),
            "{text}"
        );
        assert!(!text.contains("workspace="), "{text}");
    }

    #[tokio::test]
    async fn shared_skill_rejected_when_opted_out() {
        let client = RuntimeClient::detached(
            MockTransport::ok("").with_shared_skills(vec![shared_skill()]),
            "test-agent",
        );
        let tb = DefaultToolboxFactory.for_agent(
            client,
            vec!["october".into()],
            false,
            crate::agent_loop::McpToolboxes::default(),
        );
        let err = tb
            .execute(
                SKILL_TOOL,
                json!({ "name": "brainstorming", "workspace": "horsie_shared" }),
                "tc1",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn inspect_includes_shared_section_only_when_opted_in() {
        let client = RuntimeClient::detached(
            MockTransport::ok("").with_shared_skills(vec![shared_skill()]),
            "test-agent",
        );
        let tb = DefaultToolboxFactory.for_agent(
            client.clone(),
            vec!["october".into()],
            true,
            crate::agent_loop::McpToolboxes::default(),
        );
        let out = tb
            .execute(INSPECT_WORKSPACE_TOOL, json!({}), "tc1")
            .await
            .unwrap();
        let out = value(out);
        let text = out.as_str().unwrap();
        assert!(text.contains("## horsie_shared"));
        assert!(text.contains("- brainstorming: explore first"));

        // Opted-out agent never sees the shared section.
        let tb_off = DefaultToolboxFactory.for_agent(
            client,
            vec!["october".into()],
            false,
            crate::agent_loop::McpToolboxes::default(),
        );
        let out = tb_off
            .execute(INSPECT_WORKSPACE_TOOL, json!({}), "tc1")
            .await
            .unwrap();
        assert!(!value(out).as_str().unwrap().contains("horsie_shared"));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod compaction_window_tests {
    use super::compaction_window;

    /// The two ways a session ends up never compacting produce the same answer,
    /// which is what lets the run stay ignorant of both.
    #[test]
    fn a_window_reaches_a_run_only_when_wanted_and_declared() {
        assert_eq!(
            compaction_window(None, Some(200_000)),
            Some(200_000),
            "on by default"
        );
        assert_eq!(compaction_window(Some(true), Some(200_000)), Some(200_000));
        assert_eq!(
            compaction_window(Some(false), Some(200_000)),
            None,
            "turned off"
        );
        assert_eq!(
            compaction_window(None, None),
            None,
            "the card declares none"
        );
        assert_eq!(
            compaction_window(Some(true), None),
            None,
            "asked for, but there is nothing to be a share of — no guessed default"
        );
    }
}
