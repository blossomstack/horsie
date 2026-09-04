//! Every durable and process-local step type.
//!
//! The complete flow is:
//!
//! | durable marker | live variant | completion command |
//! |---|---|---|
//! | `Initialize` | `Initializing` | `Context::InitializationReady` |
//! | `Connect` | `Connecting` | `Context::ConnectionReady` |
//! | `PrepareInput` | `PreparingInput` | `Incoming::InputPrepared` |
//! | `Provider` | `CallingProvider` | `Provider::StepDone` / `StepFailed` |
//! | `Provider` | `RunningTools` | `Core::ToolReturned` |
//! | `StopHook` | `RunningStopHook` | `Core::StopHookReturned` |
//! | `Compaction` | `Compacting` | `Compaction::Landed` |
//! | `SeedSummary` | `SummarisingSeed` | `Seed::SummaryTaken` |

use crate::agent_loop::ContextManifest;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// A durable step boundary. Its history sequence is its identity and callback
/// fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepKind {
    Initialize,
    Connect,
    PrepareInput,
    Provider,
    StopHook,
    Compaction,
    SeedSummary { request_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepFailure {
    Interrupted,
    Provider(String),
    TimedOut,
}

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
pub(crate) enum ActiveStep {
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

impl ActiveStep {
    fn marker_seq(&self) -> Option<u64> {
        match self {
            Self::Idle => None,
            Self::Initializing { marker_seq, .. }
            | Self::Connecting { marker_seq, .. }
            | Self::PreparingInput { marker_seq, .. }
            | Self::CallingProvider { marker_seq, .. }
            | Self::RunningTools { marker_seq, .. }
            | Self::RunningStopHook { marker_seq, .. }
            | Self::Compacting { marker_seq, .. }
            | Self::SummarisingSeed { marker_seq, .. } => Some(*marker_seq),
        }
    }

    fn belongs_to(&self, kind: &StepKind) -> bool {
        match kind {
            StepKind::Initialize => matches!(self, Self::Initializing { .. }),
            StepKind::Connect => matches!(self, Self::Connecting { .. }),
            StepKind::PrepareInput => matches!(self, Self::PreparingInput { .. }),
            StepKind::Provider => {
                matches!(
                    self,
                    Self::CallingProvider { .. } | Self::RunningTools { .. }
                )
            }
            StepKind::StopHook => matches!(self, Self::RunningStopHook { .. }),
            StepKind::Compaction => matches!(self, Self::Compacting { .. }),
            StepKind::SeedSummary { .. } => matches!(self, Self::SummarisingSeed { .. }),
        }
    }
}

/// The complete process-local complement to durable [`crate::agent_loop::AgentState`] history.
///
/// Recovery always starts idle and derives what is owed from history. Nothing
/// outside this type may retain active progress across actor commands.
pub(crate) struct StepRun {
    pub runtime_ready: bool,
    active: ActiveStep,
    pub execution: Option<std::sync::Arc<ExecutionContext>>,
    pub reconnect_required: bool,
    start_hooks_ran: bool,
    /// Streamed text stays here so cancellation can salvage it after the step returns to `Idle`.
    pub streamed_text: Vec<String>,
}

impl StepRun {
    pub fn new(runtime_ready: bool) -> Self {
        Self {
            runtime_ready,
            active: ActiveStep::Idle,
            execution: None,
            reconnect_required: true,
            start_hooks_ran: false,
            streamed_text: Vec::new(),
        }
    }

    pub fn is_running(&self) -> bool {
        !matches!(self.active, ActiveStep::Idle)
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
        self.active = ActiveStep::Initializing { marker_seq, cancel };
        returned
    }

    pub fn begin_connection(&mut self, marker_seq: u64) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.active = ActiveStep::Connecting { marker_seq, cancel };
        returned
    }

    pub fn begin_input_preparation(&mut self, marker_seq: u64) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.active = ActiveStep::PreparingInput { marker_seq, cancel };
        returned
    }

    pub fn begin_provider(&mut self, marker_seq: u64, attempt: u32) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.active = ActiveStep::CallingProvider {
            marker_seq,
            attempt,
            cancel,
        };
        returned
    }

    pub fn begin_stop_hook(&mut self, marker_seq: u64) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.active = ActiveStep::RunningStopHook { marker_seq, cancel };
        returned
    }

    pub fn begin_compaction(&mut self, marker_seq: u64) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.active = ActiveStep::Compacting { marker_seq, cancel };
        returned
    }

    pub fn begin_seed_summary(&mut self, marker_seq: u64) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.active = ActiveStep::SummarisingSeed { marker_seq, cancel };
        returned
    }

    /// Claim the active slot for a whole tool batch.
    pub fn begin_tools(
        &mut self,
        marker_seq: u64,
        calls: Vec<DispatchedCall>,
    ) -> CancellationToken {
        let (returned, cancel) = Self::token();
        self.active = ActiveStep::RunningTools {
            marker_seq,
            cancel,
            calls,
            stopped: Vec::new(),
        };
        returned
    }

    pub fn push_delta(&mut self, marker_seq: u64, text: String) -> bool {
        let ActiveStep::CallingProvider {
            marker_seq: open, ..
        } = &self.active
        else {
            return false;
        };
        if *open != marker_seq {
            return false;
        }
        self.streamed_text.push(text);
        true
    }

    fn finish_matching(&mut self, marker_seq: u64, kind: &StepKind) -> bool {
        if !self.active.belongs_to(kind) {
            return false;
        }
        if self.active.marker_seq() != Some(marker_seq) {
            return false;
        }
        self.active = ActiveStep::Idle;
        true
    }

    pub fn finish_initialization(&mut self, marker_seq: u64) -> bool {
        self.finish_matching(marker_seq, &StepKind::Initialize)
    }

    pub fn finish_connection(&mut self, marker_seq: u64) -> bool {
        self.finish_matching(marker_seq, &StepKind::Connect)
    }

    pub fn finish_input_preparation(&mut self, marker_seq: u64) -> bool {
        self.finish_matching(marker_seq, &StepKind::PrepareInput)
    }

    pub fn finish_provider(&mut self, marker_seq: u64) -> Option<u32> {
        let ActiveStep::CallingProvider {
            marker_seq: open,
            attempt,
            ..
        } = &self.active
        else {
            return None;
        };
        if *open != marker_seq {
            return None;
        }
        let attempt = *attempt;
        self.active = ActiveStep::Idle;
        Some(attempt)
    }

    pub fn finish_stop_hook(&mut self, marker_seq: u64) -> bool {
        self.finish_matching(marker_seq, &StepKind::StopHook)
    }

    pub fn finish_compaction(&mut self, marker_seq: u64) -> bool {
        self.finish_matching(marker_seq, &StepKind::Compaction)
    }

    pub fn finish_seed_summary(&mut self, marker_seq: u64) -> bool {
        if !matches!(self.active, ActiveStep::SummarisingSeed { .. })
            || self.active.marker_seq() != Some(marker_seq)
        {
            return false;
        }
        self.active = ActiveStep::Idle;
        true
    }

    pub fn tools_are_running(&self, marker_seq: u64) -> bool {
        matches!(
            &self.active,
            ActiveStep::RunningTools { marker_seq: open, .. } if *open == marker_seq
        )
    }

    pub fn take_tool(&mut self, marker_seq: u64, tool_call_id: &str) -> Option<DispatchedCall> {
        let ActiveStep::RunningTools {
            marker_seq: open,
            calls,
            ..
        } = &mut self.active
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
        if let ActiveStep::RunningTools { stopped, .. } = &mut self.active {
            stopped.push(stopped_call);
        }
    }

    /// Finish an empty tool batch and return every stopper it collected.
    /// `None` means other calls are still running or this is not a tool step.
    pub fn settle_tools(&mut self, marker_seq: u64) -> Option<Vec<horsie_agentcore::StoppedCall>> {
        let ActiveStep::RunningTools {
            marker_seq: open,
            calls,
            ..
        } = &self.active
        else {
            return None;
        };
        if *open != marker_seq || !calls.is_empty() {
            return None;
        }
        let ActiveStep::RunningTools { stopped, .. } =
            std::mem::replace(&mut self.active, ActiveStep::Idle)
        else {
            return None;
        };
        Some(stopped)
    }

    /// Stop everything and make callbacks for the old marker stale.
    pub fn stop(&mut self) {
        match &self.active {
            ActiveStep::Idle => {}
            ActiveStep::Initializing { cancel, .. }
            | ActiveStep::Connecting { cancel, .. }
            | ActiveStep::PreparingInput { cancel, .. }
            | ActiveStep::CallingProvider { cancel, .. }
            | ActiveStep::RunningTools { cancel, .. }
            | ActiveStep::RunningStopHook { cancel, .. }
            | ActiveStep::Compacting { cancel, .. }
            | ActiveStep::SummarisingSeed { cancel, .. } => cancel.cancel(),
        }
        self.active = ActiveStep::Idle;
        self.reconnect_required = true;
    }
}

/// Live provider, toolbox, and hook clients shared by active steps.
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
    fn a_stale_marker_cannot_finish_active_work() {
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
    fn every_durable_step_names_its_live_phase() {
        let token = CancellationToken::new;
        let cases = [
            (
                StepKind::Initialize,
                ActiveStep::Initializing {
                    marker_seq: 1,
                    cancel: token(),
                },
            ),
            (
                StepKind::Connect,
                ActiveStep::Connecting {
                    marker_seq: 1,
                    cancel: token(),
                },
            ),
            (
                StepKind::PrepareInput,
                ActiveStep::PreparingInput {
                    marker_seq: 1,
                    cancel: token(),
                },
            ),
            (
                StepKind::Provider,
                ActiveStep::CallingProvider {
                    marker_seq: 1,
                    attempt: 0,
                    cancel: token(),
                },
            ),
            (
                StepKind::Provider,
                ActiveStep::RunningTools {
                    marker_seq: 1,
                    cancel: token(),
                    calls: Vec::new(),
                    stopped: Vec::new(),
                },
            ),
            (
                StepKind::StopHook,
                ActiveStep::RunningStopHook {
                    marker_seq: 1,
                    cancel: token(),
                },
            ),
            (
                StepKind::Compaction,
                ActiveStep::Compacting {
                    marker_seq: 1,
                    cancel: token(),
                },
            ),
            (
                StepKind::SeedSummary {
                    request_id: "request".into(),
                },
                ActiveStep::SummarisingSeed {
                    marker_seq: 1,
                    cancel: token(),
                },
            ),
        ];

        for (kind, active) in cases {
            assert!(active.belongs_to(&kind), "{kind:?}");
            assert_eq!(active.marker_seq(), Some(1));
        }
        assert!(!ActiveStep::Idle.belongs_to(&StepKind::Provider));
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
