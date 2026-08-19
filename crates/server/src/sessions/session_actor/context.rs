//! How one turn is assembled.
//!
//! A [`SessionContextProvider`] is what an [`AgentActor`](crate::agent_loop::AgentActor)
//! asks, on its own task, for everything a run needs: the runtime handle, the
//! LLM provider, the toolbox and the system prompt. It resolves them per run
//! rather than holding them, which is what lets an agent stay resident across a
//! hibernate and resume without knowing either happened.
//!
//! One type serves all three kinds of agent a session hosts — main, subagent
//! and workflow step — because they differ only in what they are equipped with.
//! What that is, this file no longer decides at all: the agent's
//! [`Capabilities`] are built by whoever spawns it and handed over,
//! [`SessionContextProvider::provide`] hands them a [`Loading`] and returns
//! what they filled in. That is the shape a runner needs — it holds its own
//! list and equips the agents it starts — and [`SessionAgentKind`] is left
//! deciding only who this agent *is*.

use super::{CoreCommand, SessionCommand};
use crate::{
    agent_loop::{
        ContextError, ContextProvider, Contexts, StartTurn, TurnPreparation,
        capabilities::{Capabilities, SetupError, runtime::GONE_PREFIX},
        compose_system_prompt,
    },
    runtime_manager::{NARRATION_BUFFER, RuntimeError},
    sessions::{
        addressing::SessionRef,
        runners::loading::{AgentSpec, Loading},
        spec::AgentSettings,
    },
};
use async_trait::async_trait;
use horsie_agentcore::LlmProvider;
use horsie_models::{
    hooks::HookRecord,
    runtime::{
        ServerHookEvent, SessionStartInput, SubagentStartInput, UserPromptExpansionInput,
        UserPromptSubmitInput,
    },
};
use horsie_runtime_host::RuntimeClient;
use std::sync::Arc;
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
pub(crate) async fn emit_progress(
    session: &SessionRef,
    key: crate::sessions::runners::ids::AgentId,
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
    key: crate::sessions::runners::ids::AgentId,
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

/// The session's half of a load, for one of its agents.
///
/// Here rather than at each construction site because every field but the kind
/// is the session's own, and the three the kind decides — the key, the id, and
/// whether it narrates — must not be able to disagree with the provider's.
pub(super) fn loading_for(
    agent: crate::sessions::runners::ids::AgentId,
    role: crate::sessions::runners::loading::AgentRole,
    session: SessionRef,
    session_id: Uuid,
    deps: LoadingDeps,
) -> Loading {
    Loading {
        session,
        session_id,
        role,
        agent,
        // Workers are quiet by design, so their setup narrates nothing. A
        // conversation and a step both have someone watching.
        narrate: !matches!(role, crate::sessions::runners::loading::AgentRole::Sub),
        runtimes: deps.runtimes,
        registry: deps.registry,
        mcp: deps.mcp,
        memory: deps.memory,
        services: deps.services,
        plugin_library: deps.plugin_library,
        last_client: std::sync::Mutex::new(None),
    }
}

/// What [`loading_for`] needs that the kind does not decide: the session's
/// services, exactly as the session holds them.
pub(super) struct LoadingDeps {
    pub(super) runtimes: crate::runtime_manager::RuntimeClientProvider,
    pub(super) registry: crate::sessions::spec::SharedProviderRegistry,
    pub(super) mcp: Option<Arc<crate::mcp::McpService>>,
    pub(super) memory: Option<Arc<crate::memory::MemoryService>>,
    /// The account's whole service bundle, for the control-plane tools — which
    /// reach agents, routines and environments alike, so unlike `memory` there
    /// is no single service to hold. `None` wherever the control plane is not
    /// wired, which is every test that does not exercise it.
    pub(super) services: Option<Arc<crate::users::UserServices>>,
    pub(super) plugin_library: Option<Arc<dyn crate::plugins::PluginProvisioner>>,
}

/// The runtime client an agent runs with. Subagents share the session's
/// sandbox but never its cwd/env bucket: the runtime keys that state by
/// agent id, so each subagent acts under its own identity.
pub(super) fn scoped_client(
    agent: crate::sessions::runners::ids::AgentId,
    role: crate::sessions::runners::loading::AgentRole,
    client: RuntimeClient,
) -> RuntimeClient {
    // Steps and forks share the sandbox — that is the point — but never its
    // cwd/env bucket: the runtime keys that state by agent id, so each acts
    // under its own identity, exactly as a subagent does. Only the root
    // conversation is unscoped.
    match role.scoped() {
        false => client,
        true => client.with_agent_id(agent.to_string()),
    }
}

// The four role suffixes are gone from this file. Each one now belongs to the
// capability that grants the arms the role has — `ask_user` says why there is
// no `ask_user`, `title` says what a fork's own name is for — so a tool and the
// paragraph explaining it are one edit in one place. `STEP_PROMPT_SUFFIX` goes
// to `step_result`, `SUBAGENT_PROMPT_SUFFIX` to `runtime`, which is the only
// capability that sees the plugin agent definition its typed variant composes
// with.

/// Per-run context for a session's agent, resolved on the run's own task.
///
/// It asks the [`RuntimeClientProvider`](crate::runtime_manager::RuntimeClientProvider)
/// for a client each run rather than holding one: that is what lets the agent
/// be resident across a hibernate and resume without knowing either happened.
///
/// Two halves, and the split is [`Loading`]'s: everything the *session* brings
/// to a load lives in `loading`, and what is left here is this one agent's
/// config. The capability list reads the first, and is itself handed in — this
/// type composes an agent out of the two, and chooses neither.
pub(super) struct SessionContextProvider {
    /// The session's services, its address, and the client cache.
    ///
    /// Built once when the agent is spawned rather than per `provide()`,
    /// because the cache has to outlive a single turn: `Stop` hooks and
    /// [`SessionActor::cancel_agent`](super::SessionActor) both read the handle
    /// the last load acquired.
    pub(super) loading: Loading,
    pub(super) settings: AgentSettings,
    /// What this agent is equipped with, decided by whoever started it.
    ///
    /// Given rather than derived, and that is the point: a kind no longer says
    /// what an agent can do. The spawn site holds the list — a runner will —
    /// which is also what lets the per-agent extras a step needs (its typed
    /// `submit_result`, whether it may ask) be equipped without this file
    /// knowing steps exist.
    pub(super) equipment: Capabilities,
    /// What this agent is, for the decisions that are not identity.
    pub(super) role: crate::sessions::runners::loading::AgentRole,
    /// Who it is.
    pub(super) agent: crate::sessions::runners::ids::AgentId,
    /// The plugin-declared agent type this agent runs as, for a subagent that
    /// was spawned with one. The *name* only — the definition is resolved from
    /// the library scan on every `provide()`, so a subagent that outlives its
    /// plugin fails rather than running a prompt nobody can point at.
    pub(super) agent_type: Option<String>,
    /// The plugin bundles this session selected. With the library in
    /// [`Loading`] they answer "is `/commit` a command?" from the database,
    /// with no runtime involved — which is what lets a prompt merely *starting*
    /// with a slash cost nothing.
    pub(super) plugins: Vec<String>,
}

impl SessionContextProvider {
    /// The session's own model's provider.
    ///
    /// The fallback, not the answer: a capability may have picked one — a
    /// plugin agent definition can declare a model — and this is what the agent
    /// runs on when none did.
    fn llm_provider(&self) -> Result<Arc<dyn LlmProvider>, String> {
        let reg = self
            .loading
            .registry
            .read()
            .map_err(|_| "provider registry lock poisoned".to_string())?;
        reg.get(&self.settings.model)
            .map(|e| e.provider.clone())
            .ok_or_else(|| format!("no provider registered for model '{}'", self.settings.model))
    }

    /// The client the run currently in flight already acquired, if any.
    pub(super) fn cached_client(&self) -> Option<RuntimeClient> {
        self.loading.cached_client()
    }

    /// Whether this agent loads the shared plugin library — and so whether any
    /// hook could possibly be declared for it.
    pub(super) fn use_plugins(&self) -> bool {
        self.settings.use_plugins.unwrap_or(true)
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
        let mut names = self.settings.plugins.clone();
        if names.is_empty() {
            // Nothing selected falls back to the account's default-enabled set,
            // exactly as session-wide provisioning did.
            if let Some(library) = &self.loading.plugin_library {
                names = library.default_names().await;
            }
        }
        let bundles = match &self.loading.plugin_library {
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
            .loading
            .narrate
            .then(|| narration_pump(&self.loading.session, self.loading.agent))
            .unzip();
        let acquired = self.loading.runtimes.get(narrate).await;
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
        Ok(scoped_client(self.agent, self.role, client))
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
        let Some(library) = &self.loading.plugin_library else {
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

    /// Why the turn cannot be prepared.
    ///
    /// Retryable unless the runtime is *gone*, which is the one failure a
    /// session can never retry: the vendor is alive and says the sandbox does
    /// not exist. A vendor that is merely offline says nothing about that, so
    /// it is a wait rather than an ending.
    ///
    /// The test is on the reason's text, because [`SetupError`] has one axis —
    /// `fatal` — and it does not separate "wait" from "this session is over".
    /// [`GONE_PREFIX`] is the second axis, written once by the capability that
    /// knows and read once here.
    fn fatal(e: SetupError) -> ContextError {
        match e.reason.strip_prefix(GONE_PREFIX) {
            Some(what) => ContextError::terminal(what),
            None => ContextError::retryable(e.to_string()),
        }
    }

    /// The agent's system prompt: the base, then what each capability said.
    ///
    /// The sections arrive in setup order, which is the order tool calls are
    /// offered in — so the paragraph about a tool and the tool itself cannot
    /// end up in different places.
    fn compose_prompt(&self, spec: &AgentSpec) -> Option<String> {
        let base = compose_system_prompt(
            Some(SESSION_AGENT_PROMPT),
            &spec.facts.workspace,
            spec.facts.shared.as_deref(),
            self.settings.instructions.as_deref(),
        );
        let sections: Vec<&str> = spec
            .prompt
            .iter()
            .map(|s| s.body.trim())
            .filter(|s| !s.is_empty())
            .collect();
        match (base, sections.is_empty()) {
            (Some(base), true) => Some(base),
            (Some(base), false) => Some(format!("{base}\n\n{}", sections.join("\n\n"))),
            (None, false) => Some(sections.join("\n\n")),
            (None, true) => None,
        }
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
            let event = match self.role {
                crate::sessions::runners::loading::AgentRole::Sub => {
                    ServerHookEvent::SubagentStart(SubagentStartInput {
                        agent_id: self.agent.to_string(),
                        agent_type: self.agent_type(),
                    })
                }
                _ => {
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

    /// Everything the run needs, from the agent's capability list.
    ///
    /// There is no match on the kind here, and that absence is the point. This
    /// method used to carry two tables — one choosing toolbox layers, one
    /// choosing a prompt suffix — that had to be edited together and were
    /// nowhere near each other. A capability now contributes both, so a tool
    /// and the paragraph that explains it are one edit in one file.
    async fn provide(&self) -> Result<Contexts, ContextError> {
        let (spec, degraded) = self
            .equipment
            .equip(&self.loading, self.settings.clone())
            .await
            .map_err(Self::fatal)?;
        // Not fatal, by the failing capability's own judgement: an MCP server
        // that will not connect costs the agent some tools and not its turn.
        // Logged rather than swallowed, because "the agent started without
        // them" is exactly the thing nobody notices otherwise.
        for e in degraded {
            tracing::warn!(
                session = %self.loading.session_id,
                capability = e.capability,
                reason = %e.reason,
                "the agent starts without this capability's tools"
            );
        }
        // The session's own model unless a capability picked one — only a
        // plugin agent definition can, and only when horsie has it.
        let provider = match &spec.provider {
            Some(provider) => provider.clone(),
            None => self.llm_provider()?,
        };
        // Taken as the capability left it, never re-derived: `auto_compact:
        // false` is expressed as `None`, so a fallback here would hand the
        // window back to a session that turned compaction off.
        let context_window = spec.context_window;
        let system_prompt = self.compose_prompt(&spec);
        // Cloned before the spec is consumed. The run needs them for what the
        // toolbox cannot carry: which tools its capabilities advertise, and
        // which agent types `spawn_agent` will accept.
        let facts = spec.facts.clone();
        // Last, because it consumes the spec: the layers each capability pushed
        // are folded here, innermost last.
        let toolbox = spec.toolbox().ok_or_else(|| {
            ContextError::retryable(
                "this agent was equipped with no tools at all, not even the sandbox",
            )
        })?;
        // The only stage this method still reports itself. The three before it
        // belong to the capabilities that do the waiting.
        self.loading.progress("ready", None).await;
        Ok(Contexts {
            provider,
            toolbox,
            system_prompt,
            facts,
            context_window,
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
    use std::sync::{Arc, Mutex, PoisonError};
    use uuid::Uuid;

    /// The gate, at the layer that now applies it: a worker inherits the
    /// session's settings but not its authority over the server.
    ///
    /// It used to be a `matches!(kind, Main)` inside a toolbox builder here.
    /// It is `assemble`'s now — the capability is simply not in the list — so
    /// what this asserts is that the kind still reaches that decision intact.
    /// The one difference is deliberate and `assemble`'s: a fork is a
    /// conversation of this session, so it is trusted with what the session is.
    #[tokio::test]
    async fn control_tools_reach_only_a_conversation_that_asked_for_them() {
        let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let build = |kind: SessionAgentKind, control_plane: Option<bool>| {
            let mut settings = agent_settings_fixture();
            settings.control_plane = control_plane;
            SessionContextProvider {
                loading: test_loading(&f, &session, id, kind),
                equipment: test_equipment(kind, &settings, false, None),
                settings,
                kind,
                agent_type: None,
                plugins: Vec::new(),
            }
        };

        assert!(
            !build(SessionAgentKind::Main, None)
                .equipment
                .has("control_plane"),
            "a preset that never asked must not get them"
        );
        for kind in [
            SessionAgentKind::Main,
            SessionAgentKind::Fork(Uuid::new_v4()),
        ] {
            assert!(
                build(kind, Some(true)).equipment.has("control_plane"),
                "a conversation that asked for them must have them"
            );
        }
        for kind in [
            SessionAgentKind::Sub(Uuid::new_v4()),
            SessionAgentKind::Step(Uuid::new_v4()),
        ] {
            assert!(
                !build(kind, Some(true)).equipment.has("control_plane"),
                "a worker inherits the setting but must not inherit the authority"
            );
        }
    }

    /// Every tool this agent will actually be offered.
    ///
    /// Two halves, because that is how the agent runs: the layers `provide`
    /// composed — the sandbox, MCP, memory — plus what its capabilities answer
    /// for on the mailbox, which the agent actor advertises beside them. A test
    /// reading only the toolbox would pass with `ask_user` missing entirely.
    fn offered(provider: &SessionContextProvider, contexts: &Contexts) -> Vec<String> {
        run_toolbox(provider, contexts)
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    /// The toolbox this run would hand the model: the sandbox `provide`
    /// composed, wrapped in each capability's own layer.
    ///
    /// Built the way the agent actor builds it, so what these tests read is
    /// what the model is shown rather than a second list assembled beside it.
    fn run_toolbox(
        provider: &SessionContextProvider,
        contexts: &Contexts,
    ) -> Arc<dyn horsie_agentcore::Toolbox> {
        crate::agent_loop::capabilities::testing::composed(
            &provider.equipment,
            Arc::clone(&contexts.toolbox),
            &contexts.facts,
        )
    }

    /// **What `provide` must carry out of the spec, and used to drop.**
    ///
    /// `spawn_agent`'s description is the only place a model learns which agent
    /// types exist, and the list comes from the library scan — which happens
    /// inside `equip`, in a capability that runs *after* `sub_agent`. So the
    /// advertisement is built on the run's task from the facts this returns; a
    /// `provide` that dropped them offers a parameter with nothing behind it,
    /// and every `agent_type` the model can name is a guess.
    #[tokio::test]
    async fn the_scanned_agent_types_reach_the_spawn_tools_description() {
        let (f, session, id) = agent_harness().await;
        let kind = SessionAgentKind::Main;
        let provider = SessionContextProvider {
            loading: test_loading(&f, &session, id, kind),
            equipment: test_equipment(kind, &agent_settings_fixture(), false, None),
            settings: agent_settings_fixture(),
            kind,
            agent_type: None,
            plugins: Vec::new(),
        };
        let contexts = provider.provide().await.expect("contexts");
        let spawn = run_toolbox(&provider, &contexts)
            .specs()
            .into_iter()
            .find(|t| t.name == "spawn_agent")
            .expect("spawn_agent is advertised");
        assert!(
            spawn.description.contains("- code-reviewer: reviews diffs"),
            "the scan found an agent type the model is never told about: {}",
            spawn.description
        );
    }

    #[tokio::test]
    async fn subagent_toolbox_strips_session_metadata_tools() {
        let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;

        let build = |kind: SessionAgentKind| SessionContextProvider {
            loading: test_loading(&f, &session, id, kind),
            equipment: test_equipment(kind, &agent_settings_fixture(), false, None),
            settings: agent_settings_fixture(),
            kind,
            agent_type: None,
            plugins: Vec::new(),
        };

        let main_provider = build(SessionAgentKind::Main);
        let main = main_provider.provide().await.unwrap();
        let main_tools = offered(&main_provider, &main);
        for t in [
            "spawn_agent",
            "subagent_status",
            "set_session_title",
            "ask_user",
        ] {
            assert!(main_tools.contains(&t.to_string()), "main lacks {t}");
        }

        let sub_id = Uuid::new_v4();
        let sub_provider = build(SessionAgentKind::Sub(sub_id));
        let sub = sub_provider.provide().await.unwrap();
        let sub_tools = offered(&sub_provider, &sub);
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
            loading: test_loading(&f, &session, id, SessionAgentKind::Main),
            equipment: test_equipment(SessionAgentKind::Main, &settings, false, None),
            settings,
            kind: SessionAgentKind::Main,
            agent_type: None,
            plugins: Vec::new(),
        };
        let contexts = provider.provide().await.unwrap();
        let tools = offered(&provider, &contexts);
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
            loading: test_loading(&f, &session, id, SessionAgentKind::Main),
            equipment: test_equipment(
                SessionAgentKind::Main,
                &agent_settings_fixture(),
                unattended,
                None,
            ),
            settings: agent_settings_fixture(),
            kind: SessionAgentKind::Main,
            agent_type: None,
            plugins: Vec::new(),
        };
        let unattended_provider = build(true);
        let unattended = unattended_provider.provide().await.unwrap();
        let tools = offered(&unattended_provider, &unattended);
        assert!(!tools.contains(&crate::agent_loop::capabilities::ask_user::TOOL.to_string()));
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

        let attended_provider = build(false);
        let attended = attended_provider.provide().await.unwrap();
        assert!(
            offered(&attended_provider, &attended)
                .contains(&crate::agent_loop::capabilities::ask_user::TOOL.to_string())
        );
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
        let kind = SessionAgentKind::Sub(Uuid::new_v4());
        let provider = SessionContextProvider {
            loading: test_loading(&f, &session, id, kind),
            equipment: test_equipment(
                kind,
                &agent_settings_fixture(),
                false,
                Some("uninstalled-agent".to_string()),
            ),
            settings: agent_settings_fixture(),
            kind,
            agent_type: Some("uninstalled-agent".to_string()),
            plugins: Vec::new(),
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
            crate::auth::UserId::bootstrap(),
            id,
            None,
        );
        let loading = loading_for(
            kind,
            session,
            id,
            LoadingDeps {
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
                plugin_library: None,
            },
        );
        SessionContextProvider {
            loading,
            equipment: test_equipment(kind, &agent_settings_fixture(), false, None),
            settings: agent_settings_fixture(),
            kind,
            agent_type: None,
            plugins: Vec::new(),
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
    /// Ordering rather than mere presence, because `start_hooks` runs *ahead* of
    /// `provide` — provisioning only in `provide` looks correct and is exactly
    /// the bug.
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
