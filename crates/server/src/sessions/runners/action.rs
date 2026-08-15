//! What a runner or a capability asks the session to do, and what an agent is
//! equipped with when it starts.
//!
//! Nothing here performs anything: an [`Action`] is a request the session
//! carries out, so a decision stays testable with no actor and no runtime in
//! sight. That is also why [`AgentSpec`] describes its toolbox layers rather
//! than holding them — a `ToolLayer` names which toolbox to wrap, and the
//! context provider turns the list into real toolboxes when the turn is
//! assembled. Without that seam, testing what an agent is equipped with would
//! need a sandbox.
//!
//! The spec is built by [`super::capabilities::Capabilities::equip`], which is
//! async, so no decision here can produce one. [`Action::StartAgent`] carries
//! the ingredients instead and the agent's own task does the equipping.

use super::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::spec::AgentSettings;
use horsie_models::agent::SubAgentResultPart;
use horsie_models::workflow::{StepField, StepOutcome};
use serde::{Deserialize, Serialize};

/// Something the session should do.
///
/// Every field is what the session needs to perform it, so it never re-derives
/// a decision the runner already made.
#[derive(Debug, Clone)]
pub enum Action {
    /// Start an agent for this runner, and put `first` in its queue.
    ///
    /// Carries the capability set rather than a finished [`AgentSpec`], because
    /// building the spec is [`super::capabilities::Capability::setup`] and that
    /// is async: it acquires a sandbox, scans a workspace, connects MCP. The
    /// session hands this list to the agent's own task, which equips itself.
    /// A decision stays sync, and the slow part never touches the mailbox.
    ///
    /// `settings` travels with the equipment because it is the other half of
    /// what the spec is built from, and it is not always the runner's own: a
    /// workflow resolves one preset per step, which is what lets step 1 run on
    /// a large model and step 2 on a small one.
    StartAgent {
        agent: AgentId,
        equipment: super::capabilities::Capabilities,
        settings: Box<AgentSettings>,
        first: FirstInput,
    },
    /// Create a child runner.
    ///
    /// The id is chosen by the capability that asked, not by the session, so
    /// the event it journals and the action it returns name the same child.
    CreateChild {
        id: RunnerId,
        kind: RunnerKind,
        args: RunnerArgs,
        parent: AgentId,
    },
    /// Put a finished child's report in an agent's queue.
    Deliver {
        to: AgentId,
        from: RunnerId,
        part: Box<SubAgentResultPart>,
    },
    /// Stop an agent's run.
    Cancel { agent: AgentId },
    /// Answer the caller's tool call with a message rather than an effect —
    /// a refusal, or a rendered status.
    Reply { text: String },
}

/// What goes into a freshly started agent's queue.
#[derive(Debug, Clone)]
pub enum FirstInput {
    /// A subagent's task, a step's composed input, a fork's seeded message.
    Text(String),
    /// Nothing: a conversation's main agent waits for a person.
    None,
}

/// What a child runner is created with.
#[derive(Debug, Clone)]
pub enum RunnerArgs {
    SubAgent {
        label: String,
        task: String,
        agent_type: Option<String>,
        settings: Box<AgentSettings>,
    },
    Workflow {
        /// Where the graph comes from.
        source: WorkflowSource,
        input: String,
    },
    Conversation {
        /// Where this conversation branched from, if it is a fork.
        seed: Option<Branch>,
        message: String,
        settings: Box<AgentSettings>,
    },
}

/// Where a workflow run's graph comes from.
///
/// Two arms because the capability that asks for a run cannot always hand over
/// a graph: turning a name into a definition is a database read, and a
/// database read may not happen on the session mailbox. So the capability says
/// *what it wants* and the session resolves it while performing the create —
/// which is the same "decide, never perform" split every other action makes,
/// rather than a special case for one of them.
///
/// [`Self::Graph`] is what makes an ad-hoc workflow expressible: a graph built
/// at runtime needs no name and no lookup, and nothing else has to change.
#[derive(Debug, Clone)]
pub enum WorkflowSource {
    Named(String),
    Graph(std::sync::Arc<crate::sessions::workflow::WorkflowRunSpec>),
}

/// A fork's branch point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// The agent whose log this fork was cut from.
    pub source: AgentId,
    /// The source's log sequence at the cut.
    pub source_seq: u64,
    pub mode: crate::sessions::forks::ForkMode,
}

/// What an agent is equipped with, accumulated by folding its capabilities.
#[derive(Debug, Clone, Default)]
pub struct AgentSpec {
    /// The model, the caps, the instructions this agent runs under.
    pub settings: Option<AgentSettings>,
    /// Toolbox layers, innermost first. The order the context provider wraps
    /// them in, which is why the assembly order is a written property.
    pub layers: Vec<ToolLayer>,
    /// System-prompt sections, appended in order.
    pub prompt: Vec<PromptSection>,
}

impl AgentSpec {
    /// Whether this agent was equipped with a given layer. The read every
    /// `setup` test makes, and the read the context provider makes.
    #[must_use]
    pub fn has(&self, want: &ToolLayer) -> bool {
        self.layers.contains(want)
    }
}

/// One toolbox layer, named rather than built.
///
/// `PartialEq` but not `Eq`: the generated workflow types it carries are not
/// `Eq`, and comparing equipment is all this needs.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolLayer {
    /// The sandbox toolbox: shell, files, workspaces, plugin tools, skills.
    Runtime,
    Mcp {
        servers: Vec<String>,
    },
    Memory {
        spaces: Vec<String>,
    },
    ControlPlane,
    AskUser,
    SessionTitle,
    /// A fork names itself, not the session it lives in.
    ForkTitle {
        fork: RunnerId,
    },
    SpawnAgent {
        max: u32,
    },
    InvokeWorkflow,
    SubmitResult {
        outcomes: Vec<StepOutcome>,
        fields: Vec<StepField>,
    },
}

/// One system-prompt section, appended after the base prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSection {
    /// Stable name, so a duplicate is detectable and a reader can tell which
    /// capability contributed which paragraph.
    pub key: &'static str,
    pub body: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// `has` is the read every capability's `setup` test makes, so it has to
    /// compare by value rather than by discriminant: a `SpawnAgent { max: 4 }`
    /// and a `SpawnAgent { max: 0 }` are not the same equipment.
    #[test]
    fn layers_compare_by_value() {
        let mut spec = AgentSpec::default();
        spec.layers.push(ToolLayer::SpawnAgent { max: 4 });
        assert!(spec.has(&ToolLayer::SpawnAgent { max: 4 }));
        assert!(!spec.has(&ToolLayer::SpawnAgent { max: 0 }));
        assert!(!spec.has(&ToolLayer::AskUser));
    }
}
