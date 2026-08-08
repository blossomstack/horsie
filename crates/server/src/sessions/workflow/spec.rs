//! A run's snapshot of the workflow it was started from.
//!
//! STORAGE types (journal-owned), distinct from the fluorite wire types in
//! `horsie_models::workflow`. Everything a run needs to keep going is resolved
//! once, here, at creation: the graph, and each step's agent preset flattened
//! into the same [`AgentSettings`] an interactive session carries.
//!
//! Snapshotting is what makes editing safe. A definition or a preset can be
//! changed or deleted while a run is under way; the run keeps the graph it
//! started with, so step 4 cannot change shape while step 2 is working. It also
//! keeps the orchestrator pure — deciding the next step never has to reach a
//! store.

use crate::sessions::spec::AgentSettings;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Most steps one run may execute before it is failed. A definition with a
/// loop and a condition that never flips would otherwise run forever.
pub const DEFAULT_MAX_STEPS: u32 = 100;

/// A directed edge out of a step (storage twin of the wire `WorkflowTransition`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionSpec {
    pub to: String,
    /// Expression over the producing step's output, bound to `output`. `None`
    /// is an unconditional catch-all.
    pub condition: Option<String>,
}

/// One step of the graph, with its preset already resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowStepSpec {
    pub name: String,
    /// The preset this was resolved from, for display. The resolution itself
    /// is in `settings`; this is not re-read.
    pub agent: String,
    pub prompt: String,
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub transitions: Vec<TransitionSpec>,
    /// The preset flattened at run creation: model, MCP servers, memory
    /// spaces, thinking effort, iteration and retry budgets.
    pub settings: AgentSettings,
}

/// Everything a run needs, fixed at creation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunSpec {
    /// The workflow this was started from. A name for display and filtering —
    /// the definition itself is `steps`, and may since have changed or gone.
    pub workflow: String,
    pub start: String,
    pub steps: Vec<WorkflowStepSpec>,
    /// What the start step is handed.
    pub input: String,
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
}

fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}

impl WorkflowRunSpec {
    pub fn step(&self, name: &str) -> Option<&WorkflowStepSpec> {
        self.steps.iter().find(|s| s.name == name)
    }

    /// The agent id of the execution at `index`.
    ///
    /// Derived rather than stored: replay reconstructs identical ids, so
    /// recovery resolves the same agent journals with nothing to keep in sync,
    /// and the orchestrator stays pure (minting a v4 would be an effect).
    pub fn step_agent_id(session_id: Uuid, index: u32) -> Uuid {
        Uuid::new_v5(&session_id, format!("step:{index}").as_bytes())
    }
}

/// How a step is handed its input: its own instruction, then whatever it was
/// given, under a header naming where that came from.
///
/// There is no template language. Transitions already provide the one
/// expression surface a definition needs, and a second one would want its own
/// escaping rules, failure modes and documentation.
pub fn compose_step_input(prompt: &str, from_step: Option<&str>, incoming: &str) -> String {
    let header = match from_step {
        Some(step) => format!("## Input from step `{step}`"),
        None => "## Input".to_string(),
    };
    format!("{}\n\n{header}\n{incoming}", prompt.trim_end())
}

/// A structured output rendered as the next step's incoming text. A JSON string
/// is passed through unquoted; anything else is its JSON form.
pub fn output_as_input(output: &Value) -> String {
    output
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| output.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_start_step_is_handed_the_run_input_under_a_plain_header() {
        assert_eq!(
            compose_step_input("Triage it.", None, "the build is red"),
            "Triage it.\n\n## Input\nthe build is red"
        );
    }

    #[test]
    fn a_later_step_is_told_which_step_its_input_came_from() {
        let got = compose_step_input("Review it.\n", Some("fix"), "{\"files\":3}");
        assert_eq!(got, "Review it.\n\n## Input from step `fix`\n{\"files\":3}");
    }

    #[test]
    fn a_string_output_is_passed_through_unquoted() {
        assert_eq!(output_as_input(&Value::String("done".into())), "done");
        assert_eq!(
            output_as_input(&serde_json::json!({"ok": true})),
            "{\"ok\":true}"
        );
    }

    /// Same session and index must always give the same agent id: recovery
    /// depends on it to find the step's journal.
    #[test]
    fn step_agent_ids_are_stable_and_distinct() {
        let session = Uuid::new_v4();
        assert_eq!(
            WorkflowRunSpec::step_agent_id(session, 3),
            WorkflowRunSpec::step_agent_id(session, 3)
        );
        assert_ne!(
            WorkflowRunSpec::step_agent_id(session, 3),
            WorkflowRunSpec::step_agent_id(session, 4)
        );
        assert_ne!(
            WorkflowRunSpec::step_agent_id(session, 3),
            WorkflowRunSpec::step_agent_id(Uuid::new_v4(), 3)
        );
    }

    /// The step agent id must never collide with the session id, which is the
    /// main agent's journal key in an interactive session.
    #[test]
    fn a_step_id_is_never_the_session_id() {
        let session = Uuid::new_v4();
        assert_ne!(WorkflowRunSpec::step_agent_id(session, 0), session);
    }
}
