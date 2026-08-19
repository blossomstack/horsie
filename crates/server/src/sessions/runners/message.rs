//! What reaches a capability, and how it finds one.
//!
//! One enum with nested arms, so a capability's `handle` is a single match —
//! and so the outer arm carries the dispatch rule rather than a comment
//! carrying it. A tool call is *offered around* because the runtime's
//! namespace cannot be enumerated: it is the sandbox toolbox plus whatever the
//! plugin library scan discovered, and no static list can name it. A child's
//! outcome is *addressed*, because exactly one capability created that child
//! and there is nothing to guess.
//!
//! **There is no arm for an arriving answer.** There was, and it was routed to
//! whichever capability held a pending ask — but an answer arrives naming the
//! agent it is for, and `SessionState.agents` already maps an agent to its
//! runner. The routing question that arm existed to answer is one the session
//! can answer without asking any capability, so the arm, the pending entry it
//! was routed by, and the third routing mode they needed are all gone.

/// Error recorded for a child someone stopped.
///
/// Its own wording rather than the interrupted one's, because this reaches a
/// *model*: the parent reads it as the result of the child it is waiting on,
/// and "interrupted by restart" would have it reason about a crash that never
/// happened.
pub const STOPPED_ERROR: &str = "stopped before it finished";

use super::ids::{AgentId, RunnerId};
use serde_json::Value;

/// Something addressed to one of a runner's capabilities.
#[derive(Debug, Clone)]
pub enum Message {
    /// A tool the agent called.
    Tool(ToolCall),
    /// A `/builtin` the person typed, already parsed. Offered around like a
    /// tool call, because `/fork` and `/compact` belong to different
    /// capabilities.
    Command(Command),
    /// A runner I created moved.
    Child(ChildMsg),
}

/// One tool call, unparsed. A capability deserialises the input into its own
/// request type, which is what keeps a tool's schema and its handler's input
/// one declaration rather than two that can drift.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// The provider's `tool_use` id, carried so the result can be paired.
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// A parsed `/builtin` invocation: the name without its slash, and the rest of
/// the line.
#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub args: String,
}

/// What became of a runner I created.
#[derive(Debug, Clone)]
pub enum ChildMsg {
    /// It reached its end, already translated by the child into the vocabulary
    /// of whoever created it. The parent never sees a `TurnEnd`, and so never
    /// needs a defensive arm for an ending its child cannot produce.
    Outcome {
        child: RunnerId,
        outcome: ChildOutcome,
    },
    /// It is now runnable — a fork whose seed landed.
    Ready { child: RunnerId },
    /// It never started: the create or the seed failed.
    Failed { child: RunnerId, error: String },
}

impl ChildMsg {
    /// The child this message is about. What makes the routing addressed.
    #[must_use]
    pub fn child(&self) -> RunnerId {
        match self {
            Self::Outcome { child, .. } | Self::Ready { child } | Self::Failed { child, .. } => {
                *child
            }
        }
    }
}

/// A child's ending, in its creator's vocabulary.
///
/// There is deliberately no `Fork` arm. A fork owes nobody a result, so it
/// reaches its creator through `Ready`/`Failed` only — the asymmetry is a
/// variant that is not there rather than a check somebody has to remember.
#[derive(Debug, Clone)]
pub enum ChildOutcome {
    SubAgent(SubAgentOutcome),
    Workflow(WorkflowOutcome),
}

/// A delegated worker's report.
#[derive(Debug, Clone)]
pub enum SubAgentOutcome {
    Completed { label: String, report: String },
    Failed { label: String, error: String },
}

/// A run's terminal output.
#[derive(Debug, Clone)]
pub enum WorkflowOutcome {
    Finished { output: Value },
    Failed { error: String },
}

/// How a message finds its capability.
///
/// The variant decides, so the discipline lives in the type: there is no table
/// above this to keep in step, and no way to offer a child's outcome around by
/// accident.
///
/// Two modes, not three. The third was for an arriving answer, routed to
/// whichever capability had recorded the ask — but an answer names its agent,
/// and the session maps an agent to its runner already, so it never needed to
/// be offered around at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routing {
    /// Offer it to each capability in order until one takes it.
    Offer,
    /// Hand it to whichever capability holds this child.
    Owner(RunnerId),
}

impl Message {
    #[must_use]
    pub fn routing(&self) -> Routing {
        match self {
            Self::Tool(_) | Self::Command(_) => Routing::Offer,
            Self::Child(m) => Routing::Owner(m.child()),
        }
    }

    /// The agent-facing name this message carries, for diagnostics when no
    /// capability claims it.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Tool(t) => format!("tool call `{}`", t.name),
            Self::Command(c) => format!("command `/{}`", c.name),
            Self::Child(m) => format!("child {}", m.child()),
        }
    }
}

/// Who a message came from, and what the capability is allowed to know about
/// the session around it.
///
/// A capability reads its own slice and this, and nothing else. Handing it the
/// whole `SessionState` would let one capability read another runner's slice,
/// which is the coupling the runner split exists to remove.
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub agent: AgentId,
    /// How deep the owning runner sits, for the one budget nesting needs.
    pub depth: u32,
    /// How many agents this session already has running, against the
    /// session-wide cap. A property of the sandbox, so it is the session's
    /// number and not a per-runner one.
    pub active_agents: u32,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The outer arm carries the dispatch rule, so the session needs neither a
    /// table nor a comment to know whether to offer a message around.
    #[test]
    fn the_variant_decides_the_routing() {
        let tool = Message::Tool(ToolCall {
            id: "t1".into(),
            name: "spawn_agent".into(),
            input: serde_json::json!({}),
        });
        assert_eq!(tool.routing(), Routing::Offer);

        let command = Message::Command(Command {
            name: "fork".into(),
            args: "look into the flake".into(),
        });
        assert_eq!(command.routing(), Routing::Offer);

        let child = RunnerId::new_v4();
        assert_eq!(
            Message::Child(ChildMsg::Ready { child }).routing(),
            Routing::Owner(child)
        );
    }

    /// Every `ChildMsg` names its child, whichever arm it is — that is what
    /// makes `Routing::Owner` total rather than a lookup that can fail.
    #[test]
    fn every_child_message_names_its_child() {
        let child = RunnerId::new_v4();
        for msg in [
            ChildMsg::Ready { child },
            ChildMsg::Failed {
                child,
                error: "no".into(),
            },
            ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::Workflow(WorkflowOutcome::Failed { error: "no".into() }),
            },
        ] {
            assert_eq!(msg.child(), child);
        }
    }
}
