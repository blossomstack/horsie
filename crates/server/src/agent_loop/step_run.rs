//! Transient foreground-step state and the narrow contract for stateful tool
//! components.
//!
//! The actor-owned loop uses [`StepRun`] for process-only execution state. Its
//! durable counterpart is always reconstructed from [`AgentState`] history.
//! Timer and task-list components receive the same command context and may
//! change durable state only by returning events; they do not call each other.
//! Provider calls and special steps are actor-loop phases, not components.

use crate::agent_loop::prelude::*;
use tokio_util::sync::CancellationToken;

/// One tool call dispatched by this process and not yet answered.
#[derive(Clone)]
pub(crate) struct DispatchedCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// The one foreground activity this process is running.
///
/// Each kind has its own variant so a provider callback cannot be mistaken for
/// a compaction callback, even when both happen to carry the same history
/// sequence. Only tool execution can hold dispatched calls; only provider work
/// can hold a retry attempt.
enum ForegroundStep {
    Idle,
    Initializing {
        marker_seq: u64,
        cancel: CancellationToken,
    },
    Connecting {
        marker_seq: u64,
        cancel: CancellationToken,
    },
    PreparingInput {
        marker_seq: u64,
        cancel: CancellationToken,
    },
    CallingProvider {
        marker_seq: u64,
        attempt: u32,
        cancel: CancellationToken,
    },
    RunningTools {
        marker_seq: u64,
        cancel: CancellationToken,
        calls: Vec<DispatchedCall>,
        stopped: Vec<horsie_agentcore::StoppedCall>,
    },
    RunningStopHook {
        marker_seq: u64,
        cancel: CancellationToken,
    },
    Compacting {
        marker_seq: u64,
        cancel: CancellationToken,
    },
    SummarisingSeed {
        marker_seq: u64,
        cancel: CancellationToken,
    },
}

/// The complete process-local complement to durable [`AgentState`] history.
///
/// Recovery always starts idle and derives what is owed from history. Nothing
/// outside this type may retain foreground progress across actor commands.
pub(crate) struct StepRun {
    pub runtime_ready: bool,
    foreground: ForegroundStep,
    pub execution: Option<std::sync::Arc<ExecutionContext>>,
    pub reconnect_required: bool,
    start_hooks_ran: bool,
    /// Streamed text stays here so cancellation can salvage it after moving
    /// the foreground back to `Idle`.
    pub streamed_text: Vec<String>,
}

impl StepRun {
    pub fn new(runtime_ready: bool) -> Self {
        Self {
            runtime_ready,
            foreground: ForegroundStep::Idle,
            execution: None,
            reconnect_required: true,
            start_hooks_ran: false,
            streamed_text: Vec::new(),
        }
    }

    pub fn is_running(&self) -> bool {
        !matches!(self.foreground, ForegroundStep::Idle)
    }

    pub fn start_hooks_ran(&self) -> bool {
        self.start_hooks_ran
    }

    pub fn mark_start_hooks_ran(&mut self) {
        self.start_hooks_ran = true;
    }

    fn token() -> (CancellationToken, CancellationToken) {
        let stored = CancellationToken::new();
        (stored.clone(), stored)
    }

    pub fn begin_initialization(&mut self, marker_seq: u64) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.foreground = ForegroundStep::Initializing { marker_seq, cancel };
        returned
    }

    pub fn begin_connection(&mut self, marker_seq: u64) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.foreground = ForegroundStep::Connecting { marker_seq, cancel };
        returned
    }

    pub fn begin_start_hooks(&mut self, marker_seq: u64) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.foreground = ForegroundStep::PreparingInput { marker_seq, cancel };
        returned
    }

    pub fn begin_provider(&mut self, marker_seq: u64, attempt: u32) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.foreground = ForegroundStep::CallingProvider {
            marker_seq,
            attempt,
            cancel,
        };
        returned
    }

    pub fn begin_stop_hook(&mut self, marker_seq: u64) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.foreground = ForegroundStep::RunningStopHook { marker_seq, cancel };
        returned
    }

    pub fn begin_compaction(&mut self, marker_seq: u64) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.foreground = ForegroundStep::Compacting { marker_seq, cancel };
        returned
    }

    pub fn begin_seed_summary(&mut self, marker_seq: u64) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.foreground = ForegroundStep::SummarisingSeed { marker_seq, cancel };
        returned
    }

    /// Claim the foreground slot for a whole tool batch.
    pub fn begin_tools(
        &mut self,
        marker_seq: u64,
        calls: Vec<DispatchedCall>,
    ) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.foreground = ForegroundStep::RunningTools {
            marker_seq,
            cancel,
            calls,
            stopped: Vec::new(),
        };
        returned
    }

    pub fn push_delta(&mut self, marker_seq: u64, text: String) -> bool {
        let ForegroundStep::CallingProvider {
            marker_seq: open, ..
        } = &self.foreground
        else {
            return false;
        };
        if *open != marker_seq {
            return false;
        }
        self.streamed_text.push(text);
        true
    }

    fn finish_matching(
        &mut self,
        marker_seq: u64,
        matches: impl FnOnce(&ForegroundStep) -> bool,
    ) -> bool {
        if !matches(&self.foreground) {
            return false;
        }
        let open = match &self.foreground {
            ForegroundStep::Idle => return false,
            ForegroundStep::Initializing { marker_seq, .. }
            | ForegroundStep::Connecting { marker_seq, .. }
            | ForegroundStep::PreparingInput { marker_seq, .. }
            | ForegroundStep::CallingProvider { marker_seq, .. }
            | ForegroundStep::RunningTools { marker_seq, .. }
            | ForegroundStep::RunningStopHook { marker_seq, .. }
            | ForegroundStep::Compacting { marker_seq, .. }
            | ForegroundStep::SummarisingSeed { marker_seq, .. } => *marker_seq,
        };
        if open != marker_seq {
            return false;
        }
        self.foreground = ForegroundStep::Idle;
        true
    }

    pub fn finish_initialization(&mut self, marker_seq: u64) -> bool {
        self.finish_matching(marker_seq, |step| {
            matches!(step, ForegroundStep::Initializing { .. })
        })
    }

    pub fn finish_connection(&mut self, marker_seq: u64) -> bool {
        self.finish_matching(marker_seq, |step| {
            matches!(step, ForegroundStep::Connecting { .. })
        })
    }

    pub fn finish_start_hooks(&mut self, marker_seq: u64) -> bool {
        self.finish_matching(marker_seq, |step| {
            matches!(step, ForegroundStep::PreparingInput { .. })
        })
    }

    pub fn finish_provider(&mut self, marker_seq: u64) -> Option<u32> {
        let ForegroundStep::CallingProvider {
            marker_seq: open,
            attempt,
            ..
        } = &self.foreground
        else {
            return None;
        };
        if *open != marker_seq {
            return None;
        }
        let attempt = *attempt;
        self.foreground = ForegroundStep::Idle;
        Some(attempt)
    }

    pub fn finish_stop_hook(&mut self, marker_seq: u64) -> bool {
        self.finish_matching(marker_seq, |step| {
            matches!(step, ForegroundStep::RunningStopHook { .. })
        })
    }

    pub fn finish_compaction(&mut self, marker_seq: u64) -> bool {
        self.finish_matching(marker_seq, |step| {
            matches!(step, ForegroundStep::Compacting { .. })
        })
    }

    pub fn finish_seed_summary(&mut self, marker_seq: u64) -> bool {
        self.finish_matching(marker_seq, |step| {
            matches!(step, ForegroundStep::SummarisingSeed { .. })
        })
    }

    pub fn tools_are_running(&self, marker_seq: u64) -> bool {
        matches!(
            &self.foreground,
            ForegroundStep::RunningTools { marker_seq: open, .. } if *open == marker_seq
        )
    }

    pub fn take_tool(&mut self, marker_seq: u64, tool_call_id: &str) -> Option<DispatchedCall> {
        let ForegroundStep::RunningTools {
            marker_seq: open,
            calls,
            ..
        } = &mut self.foreground
        else {
            return None;
        };
        if *open != marker_seq {
            return None;
        }
        calls
            .iter()
            .position(|call| call.id == tool_call_id)
            .map(|position| calls.remove(position))
    }

    pub fn push_stopped(&mut self, stopped_call: horsie_agentcore::StoppedCall) {
        if let ForegroundStep::RunningTools { stopped, .. } = &mut self.foreground {
            stopped.push(stopped_call);
        }
    }

    /// Finish an empty tool batch and return every stopper it collected.
    /// `None` means other calls are still running or this is not a tool step.
    pub fn settle_tools(&mut self, marker_seq: u64) -> Option<Vec<horsie_agentcore::StoppedCall>> {
        let ForegroundStep::RunningTools {
            marker_seq: open,
            calls,
            ..
        } = &self.foreground
        else {
            return None;
        };
        if *open != marker_seq || !calls.is_empty() {
            return None;
        }
        let ForegroundStep::RunningTools { stopped, .. } =
            std::mem::replace(&mut self.foreground, ForegroundStep::Idle)
        else {
            return None;
        };
        Some(stopped)
    }

    /// Stop everything and make callbacks for the old marker stale.
    pub fn stop(&mut self) {
        match &self.foreground {
            ForegroundStep::Idle => {}
            ForegroundStep::Initializing { cancel, .. }
            | ForegroundStep::Connecting { cancel, .. }
            | ForegroundStep::PreparingInput { cancel, .. }
            | ForegroundStep::CallingProvider { cancel, .. }
            | ForegroundStep::RunningTools { cancel, .. }
            | ForegroundStep::RunningStopHook { cancel, .. }
            | ForegroundStep::Compacting { cancel, .. }
            | ForegroundStep::SummarisingSeed { cancel, .. } => cancel.cancel(),
        }
        self.foreground = ForegroundStep::Idle;
        self.reconnect_required = true;
    }
}

/// Live provider, toolbox, and hook clients shared by foreground steps.
/// Reconnection replaces this value without changing durable prompt meaning.
pub struct ExecutionContext {
    pub provider: std::sync::Arc<dyn horsie_agentcore::LlmProvider>,
    /// Stable initialization discovery. Persisted with `AgentInitialized` and
    /// reused only to rebuild live clients after reload.
    pub manifest: ContextManifest,
    /// The fully-composed, selection-filtered toolbox every call dispatches
    /// through — the components' own tools included, indistinguishable from
    /// the rest.
    pub toolbox: std::sync::Arc<dyn horsie_agentcore::Toolbox>,
    /// What the model is shown, already filtered.
    pub specs: Vec<horsie_agentcore::ToolSpec>,
    pub system_prompt: String,
    pub budget: Option<horsie_agentcore::CompactionBudget>,
    pub conversation_id: String,
    pub thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    /// For the compaction hooks a compact run fires.
    pub context_provider: std::sync::Arc<dyn crate::agent_loop::ContextProvider>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str) -> DispatchedCall {
        DispatchedCall {
            id: id.into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        }
    }
    #[test]
    fn a_stale_marker_cannot_finish_foreground_work() {
        let mut step = StepRun::new(true);
        let cancel = step.begin_provider(7, 0);
        assert!(step.is_running());
        assert_eq!(step.finish_provider(6), None);
        assert!(step.is_running());
        assert_eq!(step.finish_provider(7), Some(0));
        assert!(!step.is_running());
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn a_callback_of_the_wrong_kind_cannot_finish_the_step() {
        let mut step = StepRun::new(true);
        step.begin_stop_hook(7);
        assert_eq!(step.finish_provider(7), None);
        assert!(step.is_running());
        assert!(step.finish_stop_hook(7));
    }

    #[test]
    fn a_tool_batch_settles_only_after_every_call_returns() {
        let mut step = StepRun::new(true);
        step.begin_tools(9, vec![call("a"), call("b")]);
        assert!(step.take_tool(9, "a").is_some());
        assert!(step.settle_tools(9).is_none());
        assert!(step.take_tool(9, "b").is_some());
        assert_eq!(step.settle_tools(9).map(|stopped| stopped.len()), Some(0));
        assert!(!step.is_running());
    }

    #[test]
    fn cancel_makes_the_marker_stale() {
        let mut step = StepRun::new(true);
        let cancel = step.begin_stop_hook(11);
        step.stop();
        assert!(cancel.is_cancelled());
        assert!(!step.finish_stop_hook(11));
        assert!(!step.is_running());
    }
}
