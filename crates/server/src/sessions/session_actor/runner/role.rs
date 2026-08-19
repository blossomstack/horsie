//! What it takes to run one agent: everything kind-specific, precomputed.
//!
//! An [`AgentRole`] is a runner's answer to "how does agent X run under me" —
//! settings, journal identity, toolbox layers, prompt suffix, hook shape. The
//! spawner and the context provider consume the values and never match on a
//! kind, which is what dissolved the eight scattered `match self.kind` seams
//! the component model grew.

use crate::sessions::spec::AgentSettings;
use uuid::Uuid;

use super::super::context::StepResultDef;
use super::ids::AgentId;

/// Everything kind-specific about running one agent.
#[derive(Clone)]
pub(crate) struct AgentRole {
    pub agent: AgentId,
    /// `"main"`, or the agent's uuid — the journal key, the revision channel,
    /// the actor name and the usage key, which deliberately share one
    /// vocabulary.
    pub name: String,
    /// The id this agent journals under: the session's for the main agent —
    /// its transcript *is* the session's — and its own for everything else.
    pub journal: Uuid,
    /// The settings this agent runs under, resolved by its runner: the
    /// session's for a conversation, the snapshot in its own record for a
    /// subagent, its step's own preset for a workflow step.
    pub settings: AgentSettings,
    /// Extra system-prompt section for this role. `None` for an attended main
    /// agent, which needs no explanation of what it is.
    pub prompt_suffix: Option<&'static str>,
    /// Whether turn preparation is narrated to watchers. Everything a person
    /// opens a session to watch does; a subagent is quiet by design.
    pub broadcasts: bool,
    /// Scope the runtime client to this agent's own cwd/env bucket. Every
    /// agent but the main one: they share the sandbox, never its state.
    pub scoped: Option<Uuid>,
    /// May manage the server through the control-plane tools. Main-only, and
    /// only when the session's settings say so.
    pub control_plane: bool,
    /// May park on questions for a person (`ask_user`).
    pub may_ask: bool,
    /// The title tool this agent gets, and what it names.
    pub titles: TitleScope,
    /// What this agent promises to return (`submit_result`), for a workflow
    /// step. `Some` also means a turn ending in plain text is not an answer.
    pub step_result: Option<StepResultDef>,
    /// What this agent's stop hook is reported as to plugins.
    pub stop_hook: StopHookKind,
    /// The plugin-declared agent type this agent runs as, if any.
    pub agent_type: Option<String>,
}

impl AgentRole {
    /// Whether a turn may end in plain text, or owes a structured result.
    #[must_use]
    pub fn requires_result(&self) -> bool {
        self.step_result.is_some()
    }
}

/// The title tool an agent gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TitleScope {
    /// `set_session_title` names the session.
    Session,
    /// `set_session_title` names this fork — the model should not have to know
    /// which kind of conversation it is in to name it.
    Fork(Uuid),
    /// No title tool: a step's title belongs to the run, a subagent's to
    /// nobody.
    None,
}

/// What an agent's stop hook is reported as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopHookKind {
    /// A conversation or a step ending its turn.
    Stop,
    /// A subagent ending its task, reported with its agent type.
    SubagentStop,
}

/// Appended to a subagent's system prompt: its place in the tree and how its
/// result travels. Deliberately short — the tools carry their own docs.
pub(crate) const SUBAGENT_PROMPT_SUFFIX: &str = "\n\n# Subagent role\n\
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
pub(crate) const STEP_PROMPT_SUFFIX: &str = "\n\n# Workflow step\n\
You are one step of a workflow, not a conversation. Your instruction and the previous \
step's result are in the message above. You share one workspace with every other step: \
what you change on disk is what the next step sees. You may spawn subagents with \
spawn_agent. You cannot rename the session.\n\n\
Finish by calling `submit_result`. What you submit is this step's result *and* what the \
workflow reads to decide which step runs next, so make it accurate and self-contained. \
Ending a turn without it is only safe while something will wake you — a question you \
asked, a timer you armed, or a subagent still running. If nothing will, and the work is \
done, submit.";

/// Appended to a fork's system prompt.
///
/// A fork is a conversation, so almost nothing a subagent is told applies: it
/// can ask the user, and it owes nobody a report. What it does need is to know
/// it is one of several under one session sharing one workspace, and that its
/// title is how a person tells them apart.
pub(crate) const FORK_PROMPT_SUFFIX: &str = "\n\n# Forked conversation\n\
You are a fork: a conversation branched from another one in this session, carrying its \
history up to the branch point. You share one workspace with it — what you change on disk \
is what it sees. Name yourself with set_session_title as soon as the new direction is \
clear; that title is how a person tells this conversation from the one it came from.";

/// Appended to an unattended session's system prompt (a routine run). It has
/// no `ask_user` tool, so the prompt says why rather than leaving the model to
/// discover a tool it was told about is missing.
pub(crate) const UNATTENDED_PROMPT_SUFFIX: &str = "\n\n# Unattended run\n\
This session was started by a routine, not by a person, and nobody is reading it while \
it runs. There is no ask_user tool: a question would park the run with nobody to answer \
it. Work from the instructions you were given — where they leave a choice open, make the \
reasonable one, say which you made and why, and carry on. Your final message is the \
report; make it self-contained.";
