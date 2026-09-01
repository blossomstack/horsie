//! What one agent is configured with.
//!
//! Distilled from an [`AgentRunDef`](crate::agent_loop::AgentRunDef) when the
//! agent is spawned and never journaled: it is how this *incarnation* was
//! asked to behave, not anything that happened to it.

use crate::agent_loop::AgentRunDef;

/// Per-agent configuration distilled from an [`AgentRunDef`]. Runtime only.
#[derive(Clone)]
pub struct AgentParams {
    pub system_prompt: Option<String>,
    /// Whether this agent owes a structured result — true for a workflow step,
    /// which ends only by calling `submit_result`. Everything else finishes a
    /// turn with plain text, and that text *is* its answer.
    ///
    /// The one thing this decides: what a turn ending with text means. For a
    /// step it is either a park (something will wake it) or a mistake (nothing
    /// will); for anyone else it is the answer.
    pub requires_result: bool,
    pub max_iterations: Option<u32>,
    pub max_retries: u32,
    /// Canonical thinking effort for this agent's runs, already resolved from
    /// the session's choice and the model's default. `None` sends no control.
    pub thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    /// Interactive (session) mode: recovery never injects a synthetic continue
    /// — the next user message is the continuation — and the event log is
    /// never snapshot-compacted (SSE cursors are journal sequence numbers and
    /// must stay stable). Workflow agents keep the default `false`.
    pub interactive: bool,
    /// Whether a turn ending with text is allowed to *conclude* while this
    /// agent still has delegated work in flight — subagents it spawned,
    /// workflows it invoked, timers it armed. True for a subagent: its
    /// conclusion is a report its parent consumes once, so an agent that is
    /// still waiting parks instead, and reports when its whole subtree is
    /// done. A step has the stronger `requires_result` gate; a session's
    /// final text is an answer to a person, not a report, and stays a turn
    /// boundary.
    pub park_on_outstanding_work: bool,
    /// The built-in tools this agent may call, by name. `None` is the default
    /// set, not "everything" — see [`crate::tools::resolve`].
    ///
    /// Carried down to the agent rather than applied by whoever built the
    /// toolbox, because the toolbox is only whole here: the actor is what
    /// stacks the timer and `task_list` layers on top of whatever it was
    /// handed.
    pub tools: Option<Vec<String>>,
}

impl AgentParams {
    pub fn from_def(def: &AgentRunDef) -> Self {
        Self {
            system_prompt: def.system_prompt.clone(),
            requires_result: false,
            max_iterations: def.max_iterations,
            max_retries: def.max_retries.unwrap_or(0),
            thinking_effort: None,
            interactive: false,
            park_on_outstanding_work: false,
            tools: def.allowed_tools.clone(),
        }
    }
}

