//! How an agent is loaded: what its capabilities acquire from, and what they
//! fill in.
//!
//! Two types, and the split between them is the point. [`Loading`] is the
//! *session's* — its runtime provider, its MCP and memory services, its
//! address. [`AgentSpec`] is the *agent's* — the model, the toolbox, the
//! prompt. A capability reads the first and writes the second, and the config
//! it needs to do so it already holds itself.
//!
//! Neither is a fifth kind of object. `Loading` is the argument `setup` needs
//! and nothing else; it exists for exactly as long as an agent takes to load.
//!
//! # Two orders, one list
//!
//! A capability list has to satisfy two orderings that look opposite:
//!
//! - **Offering a tool call**: fixed-name capabilities first, open-namespace
//!   ones last, so `set_session_title` reaches `title` before the sandbox layer
//!   can swallow it.
//! - **Wrapping a toolbox**: the sandbox base is innermost, and the outermost
//!   decorator wins a name collision.
//!
//! Those are one rule read from two ends — whoever wins a name is first in the
//! offer list and outermost in the toolbox — so [`super::assemble`]'s order
//! serves both and there is no second list to keep in step.
//!
//! The catch is that a decorator needs its inner toolbox when it is built,
//! while `setup` has to run front-to-back for an unrelated reason:
//! [`crate::agent_loop::capabilities::mcp`] must deposit its connections before
//! [`crate::agent_loop::capabilities::runtime`] builds the base out of them. So a
//! capability does not build a toolbox — it pushes a [`Layer`], and the spec
//! composes them at the end, innermost last.

use super::ids::AgentId;
use crate::agent_loop::{McpToolboxes, SharedContext, WorkspaceContext};
use crate::sessions::addressing::SessionRef;
use crate::sessions::session_actor::AgentKey;
use crate::sessions::spec::AgentSettings;
use horsie_agentcore::{LlmProvider, Toolbox};
use horsie_runtime_host::RuntimeClient;
use std::sync::{Arc, Mutex, PoisonError};
use uuid::Uuid;

/// A toolbox decorator, waiting for the layer it will wrap.
///
/// `None` reaches the innermost one — the sandbox base, which wraps nothing.
/// A capability that equips no tools pushes no layer at all, rather than one
/// that passes its inner through.
type Layer = Box<dyn FnOnce(Option<Arc<dyn Toolbox>>, &AgentFacts) -> Arc<dyn Toolbox> + Send>;

/// What the workspace scan found, as a layer reads it.
///
/// Handed to every [`Layer`] at compose time rather than at push time, which is
/// what lets a capability that sorts *before* the runtime still read the scan.
/// `sub_agent` is the case that forces it: it must win the `spawn_agent` name,
/// so it sorts early, but `SubAgentToolbox` needs the agent catalogue, which
/// only exists after the runtime has scanned. A layer reads facts; it never
/// writes them.
#[derive(Default, Clone)]
pub struct AgentFacts {
    pub workspace: WorkspaceContext,
    pub shared: Option<Arc<SharedContext>>,
    pub runtime: Option<RuntimeClient>,
}

/// What an agent runs with, filled in by its capabilities' `setup`.
///
/// Not a description that something else realises later. An earlier draft made
/// it one — a list of named layers, turned into real toolboxes by a nameless
/// third party — and that third party was the tell that the split was wrong.
pub struct AgentSpec {
    /// What this agent runs under. Not optional: `equip` is handed the
    /// settings, so there is no window in which a spec has none.
    pub settings: AgentSettings,

    /// The model. Only the runtime capability sets it, because only a plugin
    /// agent definition can declare one; everything else leaves the session's.
    pub provider: Option<Arc<dyn LlmProvider>>,
    pub context_window: Option<u32>,

    /// System-prompt sections, appended in setup order after the base prompt.
    pub prompt: Vec<PromptSection>,

    /// MCP connections, deposited by `mcp` and consumed by `runtime` when it
    /// builds the base. The one place a capability hands another an ingredient
    /// rather than a layer, and the reason setup runs front-to-back.
    pub mcp: McpToolboxes,

    /// What the scan found. Written by `runtime`, read by the prompt composer
    /// and by any layer at compose time.
    pub facts: AgentFacts,

    /// Toolbox decorators, outermost first.
    layers: Vec<Layer>,
}

impl AgentSpec {
    #[must_use]
    pub fn new(settings: AgentSettings) -> Self {
        Self {
            settings,
            provider: None,
            context_window: None,
            prompt: Vec::new(),
            mcp: McpToolboxes::default(),
            facts: AgentFacts::default(),
            layers: Vec::new(),
        }
    }

    /// Add a decorator.
    ///
    /// Called in setup order, so an earlier call ends up further out — the same
    /// order tool calls are offered in, which is what keeps the two consistent.
    pub fn wrap(
        &mut self,
        layer: impl FnOnce(Option<Arc<dyn Toolbox>>, &AgentFacts) -> Arc<dyn Toolbox> + Send + 'static,
    ) {
        self.layers.push(Box::new(layer));
    }

    /// Contribute a system-prompt section.
    pub fn say(&mut self, key: &'static str, body: impl Into<String>) {
        self.prompt.push(PromptSection {
            key,
            body: body.into(),
        });
    }

    /// Fold the decorators into one toolbox, innermost first.
    ///
    /// `None` when nothing equipped a tool, which is a real state: a capability
    /// set can be entirely prompt.
    #[must_use]
    pub fn toolbox(self) -> Option<Arc<dyn Toolbox>> {
        let facts = self.facts;
        self.layers
            .into_iter()
            .rev()
            .fold(None, |inner, layer| Some(layer(inner, &facts)))
    }
}

impl std::fmt::Debug for AgentSpec {
    /// Hand-written because a [`Layer`] is a closure. The count is what a
    /// reader wants anyway — which layers, by name, is the capability list.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSpec")
            .field("model", &self.settings.model)
            .field("layers", &self.layers.len())
            .field(
                "prompt",
                &self.prompt.iter().map(|s| s.key).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// One system-prompt section, appended after the base prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSection {
    /// Stable name, so a duplicate is detectable and a reader can tell which
    /// capability contributed which paragraph.
    pub key: &'static str,
    pub body: String,
}

/// What a capability acquires from while an agent loads.
///
/// The session's own services, plus who is being equipped. Deliberately *not*
/// the agent's config — the model, the step's declared outcomes, whether this
/// conversation is a fork — because every one of those already lives in the
/// capability that answers for it.
pub struct Loading {
    pub session: SessionRef,
    pub session_id: Uuid,
    /// Who is being equipped: for progress narration, for the hook sink, and
    /// for scoping the runtime client.
    pub key: AgentKey,
    /// The same agent in the runners' vocabulary.
    pub agent: AgentId,
    /// Subagents are quiet by design, so their setup narrates nothing.
    pub narrate: bool,
    pub runtimes: crate::runtime_manager::RuntimeClientProvider,
    pub registry: crate::sessions::spec::SharedProviderRegistry,
    pub mcp: Option<Arc<crate::mcp::McpService>>,
    pub memory: Option<Arc<crate::memory::MemoryService>>,
    pub services: Option<Arc<crate::users::UserServices>>,
    pub plugin_library: Option<Arc<dyn crate::plugins::PluginProvisioner>>,
    /// The client the last load acquired, kept so `Stop` hooks and `Cancel`
    /// reach the same handle rather than a sink-less clone.
    pub last_client: Mutex<Option<RuntimeClient>>,
}

impl Loading {
    /// Tell the user which stage of setup is running.
    ///
    /// Here rather than in each capability so no capability author has to
    /// remember to, and so "subagents are silent" is decided once.
    pub async fn progress(&self, stage: &str, detail: Option<String>) {
        if self.narrate {
            crate::sessions::session_actor::context::emit_progress(
                &self.session,
                self.key,
                stage,
                detail,
            )
            .await;
        }
    }

    /// The client the last load cached, if any.
    #[must_use]
    pub fn cached_client(&self) -> Option<RuntimeClient> {
        self.last_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn cache_client(&self, client: RuntimeClient) {
        *self
            .last_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(client);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec};

    /// A toolbox that answers for one name and delegates nothing, so a
    /// composed stack can be read back by asking who owns what.
    struct Named {
        name: &'static str,
        inner: Option<Arc<dyn Toolbox>>,
    }

    #[async_trait::async_trait]
    impl Toolbox for Named {
        fn specs(&self) -> Vec<ToolSpec> {
            let mut specs = match &self.inner {
                Some(i) => i.specs(),
                None => Vec::new(),
            };
            specs.push(ToolSpec {
                name: self.name.to_string(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            });
            specs
        }

        async fn execute(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _tool_call_id: &str,
        ) -> Result<ToolOutcome, ToolCallError> {
            unreachable!("a composition test never executes a tool")
        }
    }

    fn settings() -> AgentSettings {
        crate::sessions::runners::empty_settings()
    }

    fn layer(spec: &mut AgentSpec, name: &'static str) {
        spec.wrap(move |inner, _| Arc::new(Named { name, inner }));
    }

    /// The first capability to push is the outermost layer, because the first
    /// capability to be offered a tool call is the one that wins the name.
    /// Two orders, one list — so this is the test that pins them together.
    #[test]
    fn the_first_layer_pushed_ends_up_outermost() {
        let mut spec = AgentSpec::new(settings());
        layer(&mut spec, "title"); // offered first
        layer(&mut spec, "memory");
        layer(&mut spec, "runtime"); // offered last
        let toolbox = spec.toolbox().expect("three layers");
        // `definitions` walks inward first, so the innermost name comes first
        // and the outermost — the winner — comes last.
        let names: Vec<String> = toolbox.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["runtime", "memory", "title"]);
    }

    /// A capability set that equips no tools is a real state, not an error:
    /// a set can be entirely prompt.
    #[test]
    fn no_layers_is_no_toolbox() {
        assert!(AgentSpec::new(settings()).toolbox().is_none());
    }

    /// A layer reads the scan at compose time, which is what lets `sub_agent`
    /// sort before `runtime` — winning the `spawn_agent` name — and still see
    /// the catalogue the runtime's scan produced.
    #[test]
    fn a_layer_reads_facts_the_runtime_wrote_after_it_was_pushed() {
        let mut spec = AgentSpec::new(settings());
        let seen = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&seen);
        spec.wrap(move |inner, facts| {
            *sink.lock().unwrap() = facts.workspace.platform.clone();
            Arc::new(Named {
                name: "reader",
                inner,
            })
        });
        // Written after the layer was pushed, exactly as the runtime does.
        spec.facts.workspace.platform = Some("linux".into());
        let _ = spec.toolbox();
        assert_eq!(seen.lock().unwrap().as_deref(), Some("linux"));
    }
}
