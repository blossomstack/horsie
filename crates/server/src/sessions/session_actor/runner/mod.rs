//! Runners: the units of work a session hosts, and the sub-state-machines
//! that run them.
//!
//! A session is one journal, one sandbox, and a tree of runners. A **runner**
//! wraps one unit of work — the session's own conversation
//! (`MainAgentRunner`), a fork of it (`ForkAgentRunner`), a delegated task
//! (`SubAgentRunner`), or a workflow run (`WorkflowRunner`) — owns its slice
//! of the folded state, and reacts to the lifecycle of the agents that carry
//! it out. Runners replace the component model: where a component was a
//! session-wide unit struct that could exist once, a runner is an entry in
//! [`state::SessionState::runners`], so a session holds as many of each kind
//! as its work created — which is what makes nesting (a subagent spawning
//! subagents, an agent invoking a workflow) structure instead of special
//! cases.
//!
//! Runners only ever **decide**. The fold in [`state`] is pure; the behavior
//! half (spawn plans, boundary actions, repairs) is pure too, rebuilt from
//! `(spec, state)` whenever needed. The session actor is the only thing that
//! acts: it performs what runners ask for, journals what they decide, and
//! folds what it journaled.

#![allow(dead_code)]

pub mod event;
pub mod ids;
pub mod state;

/// Deepest the combined runner tree may grow: every agent→child-runner edge
/// costs 1, whether the child is a subagent or a nested workflow run. One
/// budget, because the runaway it bounds — a machine creating workers in a
/// loop — does not care which kind of worker it creates. A node *at* this
/// depth cannot create.
pub const MAX_RUNNER_DEPTH: u32 = 4;

/// Cap on concurrently-live (non-terminal) workflow runs a session may hold,
/// so a loop of `invoke_workflow` calls fails fast instead of exhausting the
/// session's agents.
pub const MAX_LIVE_RUNS: usize = 8;

/// Error recorded for work that was mid-run when the process died.
pub const INTERRUPTED_ERROR: &str = "interrupted by restart";

/// Error recorded for work someone stopped.
///
/// Its own wording rather than [`INTERRUPTED_ERROR`]'s, because this one
/// reaches a *model*: the parent reads it as the result of the child it is
/// waiting on, and "interrupted by restart" would have it reason about a
/// crash that never happened.
pub const STOPPED_ERROR: &str = "stopped before it finished";

/// Error a cancelled run reports to whoever asked for it.
pub const CANCELLED_ERROR: &str = "cancelled";
