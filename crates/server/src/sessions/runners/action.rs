//! What a runner or a capability asks the session to do, and what an agent is
//! equipped with when it starts.
//!
//! Nothing here performs anything: an [`Action`] is a request the session
//! carries out, so a decision stays testable with no actor and no runtime in
//! sight.
//!
//! What an agent runs with is [`super::loading::AgentSpec`], built by
//! [`crate::agent_loop::capabilities::Capabilities::equip`] — which is async, so no
//! decision here can produce one. [`Action::StartAgent`] carries the
//! ingredients instead, and the agent's own task does the equipping.

use super::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::spec::AgentSettings;
use horsie_models::agent::SubAgentResultPart;
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
    /// building the spec is [`crate::agent_loop::capabilities::Capability::setup`] and that
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
        equipment: crate::agent_loop::capabilities::Capabilities,
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
    /// Acquire the sandbox this session runs in.
    ///
    /// The one action nobody's agent asks for: it is the runtime runner's, and
    /// it is what puts provisioning on the same "a `Pending` runner asks for
    /// its first thing" footing as every other kind. Before it, provisioning
    /// was driven by a lifecycle command and so had no answer at recovery — a
    /// session whose sandbox died between the ask and the answer sat `Pending`
    /// with nothing to restart it.
    ///
    /// Carries nothing. What to provision is the session's spec, which the
    /// session already holds; a runner that copied it would be a second place
    /// for it to be wrong.
    Provision,
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
///
/// The two kinds that own exactly one agent carry its id, minted beside the
/// [`RunnerId`] by whichever capability asked for the child. A child that has
/// to be *addressed* before it has said anything is the ordinary case, not the
/// exception: `/fork` answers the person with the new conversation's agent, the
/// fork's row in the session list is keyed on it, and `spawn_agent` hands it
/// back to the model that called it. Minted at start time instead, all three
/// would have to wait for an agent that has not been equipped yet.
///
/// A workflow carries none, because it owns many agents over time and each
/// step's is derived from the run — which is exactly why this is a field on two
/// of the three arms rather than an equality between [`RunnerId`] and
/// [`AgentId`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunnerArgs {
    SubAgent {
        agent: AgentId,
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
        agent: AgentId,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkflowSource {
    Named(String),
    Graph(std::sync::Arc<crate::sessions::workflow::WorkflowRunSpec>),
}

/// How a fork's history was seeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForkMode {
    /// `/fork` — the source's log, copied and scrubbed.
    Copy,
    /// `/summary-n-fork` — a summary of the source, produced out of band.
    Summary,
}

impl ForkMode {
    /// The wire spelling, and what a lifecycle entry carries.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Summary => "summary",
        }
    }
}

/// A fork's branch point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// The agent whose log this fork was cut from.
    pub source: AgentId,
    /// The source's log sequence at the cut.
    pub source_seq: u64,
    pub mode: ForkMode,
}
