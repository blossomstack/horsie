//! The sandbox: shell, files, workspaces, plugin tools and skills.
//!
//! The one capability with an open namespace. What tools exist is not knowable
//! until the turn is prepared — it is whatever the runtime accepts plus
//! whatever the plugin library scan discovered — so the base toolbox answers
//! for anything nobody else equipped, and assembly therefore sorts this last.
//!
//! Sorting last is also what makes it the *innermost* toolbox: it is the base
//! every other capability decorates, and the only one that wraps nothing.
//!
//! # Setup-only: the sandbox is a `setup` layer, never a claimed name
//!
//! This capability advertises no tool and claims no message. The sandbox
//! reaches the model as a [`AgentSpec::wrap`] layer, which runs on the agent's
//! own task — and that is the whole reason it is not claimed by
//! [`super::Capability::layer`]. A name claimed there is
//! dispatched through the actor's mailbox so it can park and journal, which is
//! right for `ask_user` and wrong twice over here: the sandbox namespace cannot
//! be enumerated, so there is no list to claim, and round-tripping the
//! mailbox for every `bash` call would put the actor's inbox in the path of
//! every shell command an agent runs.
//!
//! # What its `setup` does
//!
//! Much the longest of them, and deliberately so — everything here has to
//! happen in one order, against one acquisition:
//!
//! 1. acquire the runtime, narrating the wait, and attach the hook sink;
//! 2. provision this agent's plugin tree, before anything reads it;
//! 3. scan the workspace, which is what produces the skills, the agent
//!    catalogue and the workspace section of the prompt;
//! 4. discover the plugin-declared MCP servers the scan just installed;
//! 5. resolve the plugin agent definition this agent runs as, if any, and
//!    narrow the tool allowlist and the model to what it declares;
//! 6. build the base toolbox.
//!
//! Steps 3–5 all depend on step 2, and step 6 on all of them, which is why this
//! is one method rather than six capabilities.

use super::SetupError;
use crate::agent_loop::{
    AgentRunDef, CompositeToolbox, DefaultToolboxFactory, McpUnavailable, PluginMcpToolbox,
    SharedContext, ToolboxFactory, compaction_window, scan_workspace,
};
use crate::runtime_manager::{NARRATION_BUFFER, RuntimeError};
use crate::sessions::runners::loading::{AgentFacts, AgentSpec, Loading};
use crate::sessions::session_actor::AgentKey;
use crate::sessions::session_actor::hooks::SessionHookSink;
use horsie_agentcore::Toolbox;
use horsie_models::runtime::McpServerFailure;
use horsie_runtime_host::RuntimeClient;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Appended to a subagent's system prompt: its place in the tree and how its
/// result travels. Deliberately short — the tools carry their own docs.
const SUBAGENT_PROMPT_SUFFIX: &str = "# Subagent role\n\
You are a subagent, spawned to work on one task. Your final message is your report: \
it is automatically delivered to the agent that spawned you — make it self-contained. You \
may spawn your own subagents with spawn_agent. Continue with independent work, or wait if \
none remains; do not poll subagent_status or call it repeatedly. Use subagent_status only \
when the user requests a progress update or to diagnose a suspected runtime or \
result-delivery problem. You cannot ask the user or rename the session; if you are blocked, \
report that instead.";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeCapability {
    /// The plugin-declared agent type this agent runs as, for a subagent that
    /// was spawned with one. The *name* only — the definition is resolved from
    /// the library scan on every load, so a subagent that outlives its plugin
    /// fails rather than running a prompt nobody can point at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

impl RuntimeCapability {
    #[must_use]
    pub fn new(agent_type: Option<String>) -> Self {
        Self { agent_type }
    }

    /// Acquire this agent's runtime handle, scoped to it.
    ///
    /// The wait this call *is*: a machine that has to resume takes minutes, and
    /// the vendor says why the whole time — first in what it returns, then on
    /// its sink — so those lines are carried into this agent's log as they
    /// arrive rather than summarised once it is over. They all land under
    /// `acquiring_runtime`, the stage already announced, because this is more
    /// of that stage rather than a new one.
    ///
    /// Sink-less: `setup` attaches one for the tool hooks that report
    /// themselves mid-call, while `start_hooks` returns its records to the
    /// agent, which journals them itself. A sink there would both duplicate
    /// them and race the turn they must precede.
    async fn acquire(&self, loading: &Loading) -> Result<RuntimeClient, SetupError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(NARRATION_BUFFER);
        let acquiring = loading.runtimes.get(loading.narrate.then_some(tx));
        tokio::pin!(acquiring);
        // Drained beside the acquisition rather than after it: every line is a
        // line about a wait that is still going on. The receiver stops matching
        // once the acquisition drops its sender, which is how this loop ends.
        let acquired = loop {
            tokio::select! {
                done = &mut acquiring => break done,
                Some(detail) = rx.recv() => {
                    loading.progress("acquiring_runtime", Some(detail)).await;
                }
            }
        };
        // Whatever the vendor said on its way out, before the caller reports
        // the next stage — the ordering the old joined pump bought.
        while let Ok(detail) = rx.try_recv() {
            loading.progress("acquiring_runtime", Some(detail)).await;
        }
        let client = acquired.map_err(|e| match e {
            // The one failure the session can never retry: the vendor is alive
            // and says the runtime is gone. A vendor that is merely offline
            // (`Unavailable`) says nothing about the runtime's existence.
            RuntimeError::Gone(m) => self.gone(m),
            other @ (RuntimeError::Unavailable(_) | RuntimeError::Provision(_)) => {
                self.fatal(other.to_string())
            }
        })?;
        Ok(scoped(loading.key, client))
    }

    /// Install this agent's own plugin bundles into its own tree on the runtime.
    ///
    /// The bundles come from the agent's settings, which a workflow step fills
    /// from its own preset — that is what makes a step able to run with skills
    /// its siblings do not have.
    ///
    /// Sent on every load rather than once: the runtime is the only party that
    /// knows what is already on its disk, and it absorbs the repeat.
    ///
    /// Takes the settings rather than the spec because a `&AgentSpec` cannot
    /// cross an await: a spec holds `FnOnce` layers, which are `Send` and not
    /// `Sync`. Handing over the one field this needs keeps the whole of setup
    /// on one task without making a layer shareable, which it never is.
    async fn provision(
        &self,
        loading: &Loading,
        settings: &crate::sessions::spec::AgentSettings,
        client: &RuntimeClient,
    ) -> Result<(), SetupError> {
        if !settings.use_plugins.unwrap_or(true) {
            // Provisioned with nothing, deliberately, rather than skipped: the
            // runtime refuses requests naming an agent it has never been told
            // about, and "this agent takes no plugins" is a thing to be told.
            return client
                .provision_agent(Vec::new())
                .await
                .map(|_| ())
                .map_err(|e| self.fatal(e.to_string()));
        }
        let mut names = settings.plugins.clone();
        if names.is_empty() {
            // Nothing selected falls back to the account's default-enabled set,
            // exactly as session-wide provisioning did.
            if let Some(library) = &loading.plugin_library {
                names = library.default_names().await;
            }
        }
        let bundles = match &loading.plugin_library {
            Some(library) if !names.is_empty() => library
                .resolve(&names)
                .await
                .map_err(|e| self.fatal(e))?
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
            .map_err(|e| self.fatal(e.to_string()))
    }

    /// The plugin-declared MCP servers this agent's tree brought with it.
    ///
    /// Appended *after* whatever [`super::mcp`] deposited: `CompositeToolbox`
    /// routes to the first box advertising a name, and a plugin declaring a
    /// server the user already configured must not capture those calls,
    /// arguments and all.
    ///
    /// Never fatal, and never even degraded: a plugin bringing a broken server
    /// must not stop a session that merely happens to load it, and the servers
    /// that did not answer are carried in `mcp.unavailable` so the toolbox can
    /// say why a call for one is missing.
    async fn discover_mcp(&self, loading: &Loading, spec: &mut AgentSpec, client: &RuntimeClient) {
        match client.mcp_discover().await {
            Ok(discovery) => {
                for failure in &discovery.failures {
                    match failure {
                        McpServerFailure::Unreachable(f) => {
                            tracing::warn!(
                                session = %loading.session_id,
                                server = %f.server,
                                reason = %f.reason,
                                "a plugin MCP server is unavailable; its tools are absent"
                            );
                            spec.mcp.unavailable.push(McpUnavailable::Unreachable {
                                server: f.server.clone(),
                                reason: f.reason.clone(),
                            });
                        }
                        McpServerFailure::NeedsAuth(f) => {
                            tracing::info!(
                                session = %loading.session_id,
                                server = %f.server,
                                "a plugin MCP server needs authorisation; its tools are absent"
                            );
                            spec.mcp.unavailable.push(McpUnavailable::NeedsAuth {
                                server: f.server.clone(),
                            });
                        }
                    }
                }
                if !discovery.tools.is_empty() {
                    spec.mcp.boxes.push(Arc::new(PluginMcpToolbox::new(
                        client.clone(),
                        discovery.tools,
                    )));
                }
            }
            Err(e) => tracing::warn!(
                session = %loading.session_id,
                error = %e,
                "plugin MCP discovery failed; continuing without those tools"
            ),
        }
    }

    /// The definition this agent runs as, resolved from the library *as it is
    /// now* rather than carried from the spawn — so an agent whose plugin was
    /// uninstalled between spawn and wake fails loudly.
    fn plugin_agent(
        &self,
        shared: Option<&Arc<SharedContext>>,
    ) -> Result<Option<crate::agent_loop::CatalogAgent>, SetupError> {
        match (&self.agent_type, shared) {
            (None, _) => Ok(None),
            (Some(name), Some(shared)) => {
                shared.agents.get(name).cloned().map(Some).ok_or_else(|| {
                    self.fatal(format!(
                    "this subagent runs as agent type '{name}', which no installed plugin declares"
                ))
                })
            }
            (Some(name), None) => Err(self.fatal(format!(
                "this subagent runs as agent type '{name}', but the session loads no plugins"
            ))),
        }
    }

    /// The model this agent runs on.
    ///
    /// A declared `model` is honoured only when horsie actually has it. Every
    /// model declared in the wild is an alias (`inherit`, `sonnet`, `opus`), and
    /// mapping those onto whatever the catalogue holds would let a plugin author
    /// switch a kimi session to Anthropic by writing a word in a file.
    fn provider(
        &self,
        loading: &Loading,
        spec: &AgentSpec,
        declared: Option<&str>,
    ) -> Result<Arc<dyn horsie_agentcore::LlmProvider>, SetupError> {
        let registry = loading
            .registry
            .read()
            .map_err(|_| self.fatal("provider registry lock poisoned".to_string()))?;
        if let Some(model) = declared {
            if let Some(entry) = registry.get(model) {
                return Ok(entry.provider.clone());
            }
            tracing::info!(
                model,
                "agent declares a model horsie has no provider for; inheriting the session's"
            );
        }
        registry
            .get(&spec.settings.model)
            .map(|e| e.provider.clone())
            .ok_or_else(|| {
                self.fatal(format!(
                    "no provider registered for model '{}'",
                    spec.settings.model
                ))
            })
    }

    /// Everything that goes wrong here stops the turn: an agent with no sandbox
    /// can do nothing at all.
    fn fatal(&self, reason: String) -> SetupError {
        SetupError {
            capability: self.name(),
            reason,
            fatal: true,
        }
    }

    /// The vendor says the runtime is gone. Distinguished from every other
    /// failure only by its words, because [`SetupError`] has one axis and this
    /// is the one reason a caller must not retry.
    fn gone(&self, reason: String) -> SetupError {
        self.fatal(format!("{GONE_PREFIX}{reason}"))
    }
}

/// How a `Gone` reads back. The caller turns a fatal setup error into a
/// terminal one on this, so it is a constant rather than a phrase spelled twice.
pub const GONE_PREFIX: &str = "the runtime is gone: ";

/// The runtime client an agent runs with. Subagents share the session's
/// sandbox but never its cwd/env bucket: the runtime keys that state by agent
/// id, so each acts under its own identity. A step and a fork are the same —
/// they share the run's sandbox, which is the point, and nothing else.
fn scoped(key: AgentKey, client: RuntimeClient) -> RuntimeClient {
    match key {
        AgentKey::Main => client,
        AgentKey::Sub(id) | AgentKey::Step(id) | AgentKey::Fork(id) => {
            client.with_agent_id(id.to_string())
        }
    }
}

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl RuntimeCapability {
    pub fn name(&self) -> &'static str {
        "runtime"
    }

    pub async fn setup(&self, loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        loading.progress("acquiring_runtime", None).await;
        let client = self.acquire(loading).await?;
        // Hooks run runtime-side and report what they did on the tool response.
        // Routing those records here is what makes a plugin's interventions
        // visible to the user rather than silent.
        let client = client.with_hook_sink(Arc::new(SessionHookSink::new(
            loading.session.clone(),
            loading.key,
        )));
        // Cached *after* the sink is attached, not before: `Stop` runs its hooks
        // through this handle once the turn is over, and a sink-less clone would
        // run them and drop every record on the floor. Cancellation is
        // unaffected — in-flight tracking is shared across clones.
        loading.cache_client(client.clone());

        // Before anything reads this agent's plugins — the hooks its bundles
        // declare, the skills the scan finds, the MCP servers discovery starts.
        let settings = spec.settings.clone();
        let plugins = settings.use_plugins.unwrap_or(true);
        self.provision(loading, &settings, &client).await?;

        loading.progress("scanning_workspace", None).await;
        let (ws, scan) = scan_workspace(&client, None).await;
        // No `SessionStart` here. It used to fire on this line, once per *run*,
        // so every turn re-ran every start hook; it now fires once per agent
        // load at `start_hooks`, early enough for its context to reach the turn
        // that triggered it.
        //
        // `plugins` is whether this agent loads the shared library at all — so
        // an agent that does not has no skills, no agent catalogue and no
        // plugin MCP, rather than an empty one of each.
        let shared = plugins.then(|| {
            Arc::new(SharedContext {
                skills: Arc::new(scan.skills),
                agents: Arc::new(scan.agents),
                root: scan.root,
            })
        });
        // Only when this agent loads the library at all: a session with no
        // plugins declares no MCP servers, so it asks the runtime for nothing.
        if plugins {
            self.discover_mcp(loading, spec, &client).await;
        }

        let mut def = AgentRunDef {
            system_prompt: None,
            max_iterations: settings.max_iterations,
            max_retries: Some(settings.max_retries),
            allowed_tools: settings.allowed_tools.clone(),
        };
        let plugin_agent = self.plugin_agent(shared.as_ref())?;
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
        // A typed subagent's own section follows the generic one, it does not
        // replace it: `SUBAGENT_PROMPT_SUFFIX` is the only place an agent is
        // told its final message is its report and that it cannot ask the user,
        // and no definition in the wild says either — they open "you are an
        // expert code reviewer" and stop. Both belong here rather than to
        // `sub_agent`, which answers for *spawning* one and is not equipped at
        // all when the cap is zero; this capability is the one that resolved
        // the definition, and the only one every subagent has.
        if matches!(loading.key, AgentKey::Sub(_)) {
            spec.say(
                "subagent_role",
                match &plugin_agent {
                    None => SUBAGENT_PROMPT_SUFFIX.to_string(),
                    Some(a) => format!(
                        "{SUBAGENT_PROMPT_SUFFIX}\n\n# Agent type: {}\n\n{}\n",
                        a.def.name, a.def.prompt
                    ),
                },
            );
        }

        spec.provider = Some(self.provider(
            loading,
            spec,
            plugin_agent.as_ref().and_then(|a| a.def.model.as_deref()),
        )?);
        spec.context_window = compaction_window(
            spec.settings.auto_compact,
            loading
                .registry
                .read()
                .ok()
                .and_then(|r| r.get(&spec.settings.model).and_then(|e| e.context_window)),
        );

        let names = ws.names();
        let mcp = std::mem::take(&mut spec.mcp);
        spec.facts = AgentFacts {
            workspace: ws,
            shared,
            runtime: Some(client.clone()),
        };
        spec.wrap(move |inner, _| {
            let base = DefaultToolboxFactory.for_agent(&def, client, names, plugins, mcp);
            match inner {
                None => base,
                // Unreachable while assembly keeps this capability last, and
                // composed rather than dropped if it ever stops: the base wins
                // a collision, exactly as it would have by being outside.
                Some(inner) => {
                    tracing::warn!(
                        "a capability sorted after the runtime; its tools are composed beside \
                         the sandbox rather than wrapping it"
                    );
                    Arc::new(CompositeToolbox::new(vec![base, inner])) as Arc<dyn Toolbox>
                }
            }
        });
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::{advertised_by, facts};
    use super::*;
    use crate::agent_loop::capabilities::testing::{loading, spec};
    use crate::agent_loop::capabilities::{Capabilities, Capability};

    /// **The change the move made.** Its session-side twin claimed every tool
    /// call, because the sandbox was dispatched through the offer order. Here
    /// the sandbox is a `setup` layer that answers on the agent's own task, so
    /// this capability advertises nothing at all: the sandbox namespace cannot
    /// be enumerated, so there is no list to put here — and a name listed here
    /// would be dispatched through the actor's mailbox, which is the last place
    /// a `bash` call should go.
    #[test]
    fn the_base_toolbox_is_a_layer_and_not_an_advertised_tool() {
        assert!(
            advertised_by(&Capability::Runtime(RuntimeCapability::default()), &facts()).is_empty()
        );
    }

    /// A sandbox that cannot be acquired stops the turn. Every other capability
    /// degrades; this one is why `equip` can fail at all.
    ///
    /// Not a live-sandbox test, deliberately: the acquisition is the first thing
    /// `setup` does, so a `Loading` with no vendor exercises the whole failure
    /// path — including that nothing is written to the spec on the way out.
    #[tokio::test]
    async fn no_runtime_is_fatal_and_equips_nothing() {
        let mut s = spec();
        let e = RuntimeCapability::default()
            .setup(&loading(), &mut s)
            .await
            .expect_err("no vendor is registered");
        assert_eq!(e.capability, "runtime");
        assert!(e.fatal, "an agent with no sandbox can do nothing");
        assert!(s.toolbox().is_none(), "a failed setup pushes no layer");
    }

    /// Which agent a client acts as. A subagent, a step and a fork all share
    /// the session's sandbox and none of them shares its cwd/env bucket, which
    /// the runtime keys by this id — so getting it wrong is one agent typing
    /// into another's shell.
    #[test]
    fn every_agent_but_the_main_one_acts_under_its_own_id() {
        let session = uuid::Uuid::new_v4();
        let id = uuid::Uuid::new_v4();
        let client = || {
            RuntimeClient::detached(
                horsie_runtime_host::MockTransport::ok(""),
                session.to_string(),
            )
        };
        for key in [AgentKey::Sub(id), AgentKey::Step(id), AgentKey::Fork(id)] {
            assert_eq!(
                scoped(key, client()).agent_id(),
                id.to_string(),
                "{key:?} did not scope its client"
            );
        }
        assert_eq!(
            scoped(AgentKey::Main, client()).agent_id(),
            session.to_string(),
            "the main agent is the runtime's own identity"
        );
    }

    /// The agent type is this capability's whole state, and it is resolved
    /// against the plugin library on every load — so a reload that lost it
    /// would silently run a typed subagent as an untyped one, with the generic
    /// prompt and none of the definition's tool narrowing.
    #[test]
    fn the_agent_type_survives_a_slice_round_trip() {
        let caps = Capabilities::new(vec![Capability::Runtime(RuntimeCapability::new(Some(
            "code-reviewer".into(),
        )))]);
        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        let [Capability::Runtime(back)] = read.iter().collect::<Vec<_>>()[..] else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(back.agent_type.as_deref(), Some("code-reviewer"));

        // And an untyped one comes back untyped rather than as `Some("")`.
        let caps = Capabilities::new(vec![Capability::Runtime(RuntimeCapability::default())]);
        let read: Capabilities =
            serde_json::from_str(&serde_json::to_string(&caps).expect("write")).expect("read");
        let [Capability::Runtime(back)] = read.iter().collect::<Vec<_>>()[..] else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(back.agent_type, None);
    }
}
