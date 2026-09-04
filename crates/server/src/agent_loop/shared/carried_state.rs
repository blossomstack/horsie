//! What a compaction must carry across verbatim.
//!
//! A summary is prose and may be wrong at the edges. These are facts with ids
//! in them, and an agent that reads a paraphrase of its own task list cannot
//! call `task_list` correctly afterwards. So they are rendered from state and
//! never shown to the summariser.
//!
//! The distinction this module exists for: **state surviving is not the same as
//! the model knowing it survived.** `task_list`, `set_timer` and `ask_user` all
//! keep durable state in [`AgentState`], and every one of them is invisible to
//! the model except through the tool calls in the history that a compaction
//! summarises away. Without this an agent wakes up holding three open tasks and
//! two armed timers, with no idea it has any.
//!
//! Deliberately *not* here: the working directory and environment overrides.
//! Those live in the runtime, keyed by agent id, so reading them would mean a
//! round-trip from inside the compaction path. The loss is small and
//! self-healing — the system prompt still names the workspace root every turn,
//! and an agent that is unsure can run `pwd` — where a lost task list is
//! neither.

use crate::agent_loop::AgentState;
use horsie_agentcore::{AgentLogBody, LifecycleEvent};
use std::collections::BTreeMap;

/// Every exact fact this agent must not forget, as one block.
///
/// Empty string when there is nothing to carry, so a session that never used a
/// task list or a timer gets no section of boilerplate saying so.
#[must_use]
pub fn render_carried_state(state: &AgentState) -> String {
    let mut sections: Vec<String> = Vec::new();

    if !state.task_list().tasks().is_empty() {
        sections.push(state.task_list().render());
    }

    if !state.timers().is_empty() {
        let mut block = String::from("Armed timers:");
        for t in state.timers() {
            block.push_str(&format!(
                "\n- {} ({}) fires at {}ms: {}",
                t.id,
                t.label,
                t.fire_at_unix_ms,
                if t.message.is_empty() {
                    "(no message)"
                } else {
                    t.message.as_str()
                }
            ));
        }
        sections.push(block);
    }

    let asks = state.pending_asks();
    if !asks.is_empty() {
        let mut block = String::from("Questions you are waiting on an answer to:");
        for a in asks {
            block.push_str(&format!(
                "\n- [{}] {}",
                a.tool_call_id.as_deref().unwrap_or("unknown call"),
                a.question
            ));
        }
        sections.push(block);
    }

    let running = running_children(state);
    if !running.is_empty() {
        let mut block = String::from("Delegated work still running:");
        for (id, label) in running {
            block.push_str(&format!("\n- {id} ({label})"));
        }
        sections.push(block);
    }

    sections.join("\n\n")
}

/// Whether any work this agent delegated — a subagent it spawned, or a
/// workflow it invoked — is still running; that is, whether a report is still
/// owed to it.
///
/// The agent's own view, read off its own log: the session records every
/// spawn, every invocation and every ending on the *parent*, because the
/// parent is what a person has open while it waits. So this is not a second
/// copy of the session's forest — it is the agent actor's own state, which is
/// what makes it checkable before the agent is allowed to finish.
#[must_use]
pub fn has_outstanding_children(state: &AgentState) -> bool {
    !running_children(state).is_empty()
}

/// Delegated work that has not reported a terminal status. Invoked workflow
/// runs ride the same lifecycle vocabulary as subagents, so one read covers
/// both.
///
/// Read off the log's lifecycle entries rather than from a field, because that
/// is where the fact lives: the newest entry for an id is its current status.
fn running_children(state: &AgentState) -> Vec<(String, String)> {
    let mut latest: BTreeMap<String, (String, String)> = BTreeMap::new();
    let transcript = state.transcript();
    for entry in transcript.entries() {
        if let AgentLogBody::Lifecycle(LifecycleEvent::SubAgent(s)) = &entry.body {
            latest.insert(s.id.clone(), (s.title.clone(), s.status.clone()));
        }
    }
    latest
        .into_iter()
        .filter(|(_, (_, status))| status == "running")
        .map(|(id, (label, _))| (id, label))
        .collect()
}

/// Why a `PreCompact` hook refused, if one did.
///
/// A block *or* a halt: `{"decision":"block"}` says "not this compaction" and
/// `continue: false` says "stop entirely", and from here the answer is the same
/// — do not rewrite the history. The turn then runs uncompacted, which is worse
/// than compacting but better than compacting past a hook that was about to
/// save something.
#[must_use]
pub(crate) fn precompact_refusal(records: &[horsie_models::hooks::HookRecord]) -> Option<String> {
    use horsie_models::hooks::{HookAction, StopOutcome};
    records.iter().find_map(|r| {
        if let Some(halt) = &r.halt {
            return Some(
                halt.reason
                    .clone()
                    .unwrap_or_else(|| "a PreCompact hook set continue: false".to_string()),
            );
        }
        match &r.action {
            HookAction::PreCompact(p) => match &p.outcome {
                StopOutcome::Blocked(b) => Some(
                    b.reason
                        .clone()
                        .unwrap_or_else(|| "a PreCompact hook blocked this compaction".to_string()),
                ),
                // A hook that could not run cannot refuse: only `PreToolUse`
                // fails closed, and losing a compaction to a broken guard would
                // silently fill the context instead.
                StopOutcome::Ran(_) | StopOutcome::Failed(_) | StopOutcome::CapReached(_) => None,
            },
            // Only `PreCompact` decides a compaction. Every other record in the
            // batch is something else that happened to run.
            HookAction::PreToolUse(_)
            | HookAction::PostToolUse(_)
            | HookAction::PostToolUseFailure(_)
            | HookAction::PostToolBatch(_)
            | HookAction::PostCompact(_)
            | HookAction::SessionStart(_)
            | HookAction::SessionEnd(_)
            | HookAction::UserPromptSubmit(_)
            | HookAction::UserPromptExpansion(_)
            | HookAction::Stop(_)
            | HookAction::StopFailure(_)
            | HookAction::SubagentStart(_)
            | HookAction::SubagentStop(_)
            | HookAction::TaskCreated(_)
            | HookAction::TaskCompleted(_)
            | HookAction::Notification(_)
            | HookAction::CwdChanged(_) => None,
        }
    })
}
