//! A workflow run's state: which steps executed, in what order, and what
//! became of each.
//!
//! Pure data. The session actor folds its journal through these methods, so
//! live operation and recovery follow the same path — the same split
//! `subagents` uses.
//!
//! The run log is a `Vec` because the persisted shape has to be a replayable
//! log: append-only is what makes `apply_event` a pure fold, and a graph with
//! loops is not a tree. It still projects losslessly to a graph, because every
//! entry records where it came from (`from`, `via`).

mod driver;
pub mod spec;
mod toolbox;

pub use driver::{WorkflowOrchestrator, eval_condition, next_transition};
pub use spec::{
    DEFAULT_MAX_STEPS, TransitionSpec, WorkflowRunSpec, WorkflowStepSpec, compose_step_input,
    output_as_input,
};
pub use toolbox::StepConcludeToolbox;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Lifecycle of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WorkflowRunStatus {
    /// Created; no step has started.
    #[default]
    Pending,
    Running,
    /// Stopped part-way and resumable by retrying a step: cancelled, or a step
    /// interrupted by a restart. A person decides between retry and abandon,
    /// because an interrupted step's effect on the shared workspace is unknown.
    Suspended,
    /// A step is parked on a question.
    AwaitingInput,
    Finished,
    Failed,
}

impl WorkflowRunStatus {
    /// Whether the run is over. A terminal run starts nothing further, and a
    /// retry is the only thing that can move it.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Failed)
    }
}

/// What became of one step execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Running,
    Concluded,
    Failed,
    Cancelled,
}

/// One execution of one step. A step reached twice — by a loop or by a retry —
/// has two of these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRun {
    /// Which step of the definition ran.
    pub step: String,
    /// This execution's agent. Derived from the session id and this entry's
    /// index, so replay reconstructs it.
    pub agent: Uuid,
    /// 1 for the first execution of this step on this path.
    pub attempt: u32,
    /// The entry this came out of; `None` for the start step. With `via`, this
    /// is what turns the log into an edge list.
    pub from: Option<u32>,
    /// The transition condition that matched; `None` for an unconditional edge
    /// or the start step.
    pub via: Option<String>,
    pub status: StepStatus,
    pub input: String,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
}

/// A run: its status and its append-only execution log.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkflowRunState {
    pub status: WorkflowRunStatus,
    pub steps: Vec<StepRun>,
    /// The last step's output, once the run has finished.
    pub output: Option<Value>,
    pub error: Option<String>,
}

impl WorkflowRunState {
    /// The execution in flight, if any.
    pub fn current(&self) -> Option<u32> {
        self.steps
            .iter()
            .position(|s| s.status == StepStatus::Running)
            .map(|i| i as u32)
    }

    pub fn get(&self, index: u32) -> Option<&StepRun> {
        self.steps.get(index as usize)
    }

    /// The execution an agent id belongs to.
    pub fn index_of_agent(&self, agent: Uuid) -> Option<u32> {
        self.steps
            .iter()
            .position(|s| s.agent == agent)
            .map(|i| i as u32)
    }

    /// The agent id of the execution in flight, which is the tree a spawn by
    /// that step belongs in.
    pub fn current_agent(&self) -> Option<Uuid> {
        self.current().and_then(|i| self.get(i)).map(|s| s.agent)
    }

    /// The last execution, which is the one a decision is made from.
    pub fn last(&self) -> Option<(u32, &StepRun)> {
        self.steps
            .last()
            .map(|s| ((self.steps.len() - 1) as u32, s))
    }

    /// How many times this step has already run on this path, so a new
    /// execution can number itself.
    pub fn attempts_of(&self, step: &str) -> u32 {
        self.steps.iter().filter(|s| s.step == step).count() as u32
    }

    // -- fold ------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn apply_started(
        &mut self,
        step: String,
        agent: Uuid,
        attempt: u32,
        from: Option<u32>,
        via: Option<String>,
        input: String,
        at_ms: u64,
    ) {
        self.steps.push(StepRun {
            step,
            agent,
            attempt,
            from,
            via,
            status: StepStatus::Running,
            input,
            output: None,
            error: None,
            started_at_ms: at_ms,
            ended_at_ms: None,
        });
        self.status = WorkflowRunStatus::Running;
    }

    pub fn apply_concluded(&mut self, index: u32, output: Value, at_ms: u64) {
        if let Some(s) = self.steps.get_mut(index as usize) {
            s.status = StepStatus::Concluded;
            s.output = Some(output);
            s.ended_at_ms = Some(at_ms);
        }
    }

    pub fn apply_step_failed(&mut self, index: u32, error: String, at_ms: u64) {
        if let Some(s) = self.steps.get_mut(index as usize) {
            s.status = StepStatus::Failed;
            s.error = Some(error);
            s.ended_at_ms = Some(at_ms);
        }
    }

    pub fn apply_cancelled(&mut self, index: u32, at_ms: u64) {
        if let Some(s) = self.steps.get_mut(index as usize) {
            s.status = StepStatus::Cancelled;
            s.ended_at_ms = Some(at_ms);
        }
        self.status = WorkflowRunStatus::Suspended;
    }

    pub fn apply_awaiting(&mut self) {
        self.status = WorkflowRunStatus::AwaitingInput;
    }

    pub fn apply_finished(&mut self, output: Value) {
        self.status = WorkflowRunStatus::Finished;
        self.output = Some(output);
    }

    pub fn apply_failed(&mut self, error: String) {
        self.status = WorkflowRunStatus::Failed;
        self.error = Some(error);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn started(state: &mut WorkflowRunState, step: &str, index: u32, from: Option<u32>) {
        let attempt = state.attempts_of(step) + 1;
        state.apply_started(
            step.into(),
            Uuid::from_u128(u128::from(index)),
            attempt,
            from,
            None,
            "in".into(),
            0,
        );
    }

    #[test]
    fn a_fresh_run_is_pending_with_nothing_current() {
        let s = WorkflowRunState::default();
        assert_eq!(s.status, WorkflowRunStatus::Pending);
        assert!(s.current().is_none());
        assert!(s.last().is_none());
    }

    #[test]
    fn starting_a_step_makes_it_the_current_one() {
        let mut s = WorkflowRunState::default();
        started(&mut s, "triage", 0, None);
        assert_eq!(s.status, WorkflowRunStatus::Running);
        assert_eq!(s.current(), Some(0));
        assert_eq!(s.steps[0].attempt, 1);
    }

    #[test]
    fn a_concluded_step_is_no_longer_current() {
        let mut s = WorkflowRunState::default();
        started(&mut s, "triage", 0, None);
        s.apply_concluded(0, serde_json::json!({"severity": "p0"}), 5);
        assert!(s.current().is_none());
        assert_eq!(s.steps[0].status, StepStatus::Concluded);
        assert_eq!(s.steps[0].ended_at_ms, Some(5));
    }

    /// A loop back onto a step appends rather than overwriting, and the second
    /// visit numbers itself.
    #[test]
    fn revisiting_a_step_appends_a_second_attempt() {
        let mut s = WorkflowRunState::default();
        started(&mut s, "review", 0, None);
        s.apply_concluded(0, serde_json::json!({}), 1);
        started(&mut s, "review", 1, Some(0));
        assert_eq!(s.steps.len(), 2);
        assert_eq!(s.steps[1].attempt, 2);
        assert_eq!(s.steps[1].from, Some(0));
        // The first attempt is still readable.
        assert_eq!(s.steps[0].status, StepStatus::Concluded);
    }

    #[test]
    fn cancelling_a_step_suspends_the_run() {
        let mut s = WorkflowRunState::default();
        started(&mut s, "fix", 0, None);
        s.apply_cancelled(0, 9);
        assert_eq!(s.status, WorkflowRunStatus::Suspended);
        assert_eq!(s.steps[0].status, StepStatus::Cancelled);
        assert!(s.current().is_none());
    }

    #[test]
    fn an_agent_id_resolves_to_its_execution() {
        let mut s = WorkflowRunState::default();
        started(&mut s, "a", 0, None);
        started(&mut s, "b", 1, Some(0));
        assert_eq!(s.index_of_agent(Uuid::from_u128(1)), Some(1));
        assert!(s.index_of_agent(Uuid::from_u128(99)).is_none());
    }

    /// The step in flight is the tree a spawn by that step belongs in. Subagent
    /// trees no longer hang off `StepRun` — they live in the session's forest,
    /// keyed by this id — so this is the whole of what a run tells the subagent
    /// code, and the only run-shaped fact that code ever learns.
    #[test]
    fn the_step_in_flight_names_the_tree_a_spawn_belongs_in() {
        let mut s = WorkflowRunState::default();
        started(&mut s, "a", 0, None);
        assert_eq!(s.current_agent(), Some(Uuid::from_u128(0)));
        s.apply_concluded(0, Value::Null, 200);
        // Between steps nothing is in flight, so a spawn would belong to the
        // conversation's tree — which is exactly why a run must never be
        // between steps while one of its agents can spawn.
        assert_eq!(s.current_agent(), None);
        started(&mut s, "b", 1, Some(0));
        assert_eq!(s.current_agent(), Some(Uuid::from_u128(1)));
    }

    #[test]
    fn terminal_covers_only_finished_and_failed() {
        assert!(WorkflowRunStatus::Finished.is_terminal());
        assert!(WorkflowRunStatus::Failed.is_terminal());
        assert!(!WorkflowRunStatus::Suspended.is_terminal());
        assert!(!WorkflowRunStatus::AwaitingInput.is_terminal());
    }
}
