//! Fault-capable [`RuntimeTransport`] double.
//!
//! Gated behind `cfg(any(test, feature = "test-util"))`. Before this existed the
//! only double was a transport that always succeeded, which is why the entire
//! tool-failure surface was untestable (#61 R6).

use crate::transport::{RuntimeTransport, TransportError};
use async_trait::async_trait;
use horsie_agentcore::testkit::Script;
use horsie_models::runtime::{
    PluginSkill, RunHooksResponse, RuntimeInboundMessage, RuntimeOutboundMessage, ScanResponse,
    ToolCall, ToolCallResponse, ToolError, ToolOutput, ToolResult, WorkspaceScan,
};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::Notify;

/// What a scripted `invoke` does.
pub enum TransportOutcome {
    /// Answer with this result.
    Ok(ToolResult),
    /// Fail at the transport layer — `Disconnected` is the interesting one.
    Err(TransportError),
    /// Never return.
    Hang,
}

/// Releases a transport blocked on [`MockTransport::gated_invoke`] or
/// [`MockTransport::gated_prep`]. Mirrors `mock-llm`'s handle of the same name so
/// the repo has one hang vocabulary.
#[derive(Clone)]
pub struct BlockHandle {
    gate: Arc<Notify>,
}

impl BlockHandle {
    /// A fresh gate. Needed when several transports must share one gate — e.g. a
    /// vendor factory that builds a new transport per runtime.
    #[must_use]
    pub fn new() -> Self {
        Self {
            gate: Arc::new(Notify::new()),
        }
    }

    /// Unblock every waiter.
    pub fn release(&self) {
        self.gate.notify_waiters();
    }
}

impl Default for BlockHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Observes a transport's recorded calls without owning it.
///
/// `MockVendor::with_transport` builds a fresh transport per runtime, so a test can
/// never hold the instance the session actually uses. A probe shares the recording
/// buffers, which is the only way to assert on a transport the server constructed.
#[derive(Clone, Default)]
pub struct TransportProbe {
    cancels: Arc<Mutex<Vec<String>>>,
    invocations: Arc<Mutex<Vec<ToolCall>>>,
    agent_ids: Arc<Mutex<Vec<String>>>,
    call_ids: Arc<Mutex<Vec<String>>>,
}

impl TransportProbe {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every `call_id` passed to `cancel` on any observed transport, in order.
    /// Empty means cancellation never reached the sandbox.
    pub fn cancels(&self) -> Vec<String> {
        self.cancels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every tool call any observed transport was asked to run, in order.
    pub fn invocations(&self) -> Vec<ToolCall> {
        self.invocations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The agent id each observed invoke carried, in order.
    pub fn agent_ids(&self) -> Vec<String> {
        self.agent_ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The call id each observed invoke carried, in order. This is the model's
    /// own `tool_call_id`, which is what makes a hook record joinable to the
    /// tool result in the transcript.
    pub fn call_ids(&self) -> Vec<String> {
        self.call_ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Mock transport: a canned result by default, a [`Script`] of outcomes on demand.
pub struct MockTransport {
    script: Option<Script<TransportOutcome>>,
    result: ToolResult,
    scan: Vec<WorkspaceScan>,
    shared: Vec<PluginSkill>,
    shared_root: Option<String>,
    /// Records every `RunHooks` reply carries, as a runtime that ran the
    /// server-initiated hooks would report them.
    server_hook_records: Vec<horsie_models::hooks::HookRecord>,
    /// When set, `invoke` waits on this gate before answering.
    invoke_gate: Option<Arc<Notify>>,
    /// When set, `scan_workspace` and `run_hooks` wait on this gate.
    prep_gate: Option<Arc<Notify>>,
    cancels: Arc<Mutex<Vec<String>>>,
    invocations: Arc<Mutex<Vec<ToolCall>>>,
    agent_ids: Arc<Mutex<Vec<String>>>,
    call_ids: Arc<Mutex<Vec<String>>>,
    /// Hook records every tool response carries back, as a runtime that ran
    /// plugin hooks would report them.
    hooks: Vec<horsie_models::hooks::HookRecord>,
}

impl MockTransport {
    fn base(result: ToolResult) -> Self {
        Self {
            script: None,
            result,
            hooks: Vec::new(),
            scan: Vec::new(),
            shared: Vec::new(),
            shared_root: None,
            server_hook_records: Vec::new(),
            invoke_gate: None,
            prep_gate: None,
            cancels: Arc::new(Mutex::new(Vec::new())),
            invocations: Arc::new(Mutex::new(Vec::new())),
            agent_ids: Arc::new(Mutex::new(Vec::new())),
            call_ids: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Report `hooks` on every tool response, the way a runtime that ran plugin
    /// hooks does.
    #[must_use]
    pub fn with_hooks(mut self, hooks: Vec<horsie_models::hooks::HookRecord>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Record this transport's calls into `probe` as well as returning them here.
    #[must_use]
    pub fn observed_by(mut self, probe: &TransportProbe) -> Self {
        self.cancels = probe.cancels.clone();
        self.invocations = probe.invocations.clone();
        self.agent_ids = probe.agent_ids.clone();
        self.call_ids = probe.call_ids.clone();
        self
    }

    pub fn ok(stdout: impl Into<String>) -> Self {
        Self::base(ToolResult::Ok(ToolOutput {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
        }))
    }

    /// Return a specific [`ToolOutput`] (lets tests exercise stderr / exit codes).
    pub fn output(output: ToolOutput) -> Self {
        Self::base(ToolResult::Ok(output))
    }

    pub fn err(reason: impl Into<String>) -> Self {
        Self::base(ToolResult::Err(ToolError {
            reason: reason.into(),
        }))
    }

    /// Replay `script` for every `invoke`. Exhaustion is a transport error, so a
    /// test that over-runs its script fails loudly.
    pub fn scripted(script: Script<TransportOutcome>) -> Self {
        let mut t = Self::ok("");
        t.script = Some(script);
        t
    }

    /// Answer `n` calls successfully, then report `Disconnected` forever — a
    /// runtime whose socket dropped mid-run (#61 item 2).
    pub fn disconnect_after(n: usize) -> Self {
        let oks = (0..n).map(|_| {
            TransportOutcome::Ok(ToolResult::Ok(ToolOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }))
        });
        Self::scripted(
            Script::of(oks)
                .labelled("disconnect_after")
                .then_repeating_with(|| TransportOutcome::Err(TransportError::Disconnected)),
        )
    }

    /// A transport whose `invoke` blocks until `handle` is released. Takes the
    /// handle so many transports can share one gate.
    #[must_use]
    pub fn gated_invoke(handle: &BlockHandle) -> Self {
        let mut t = Self::ok("");
        t.invoke_gate = Some(handle.gate.clone());
        t
    }

    /// A transport whose `scan_workspace` and `run_hooks` block until
    /// `handle` is released — the shape that wedges `provide()` (#61 item 5).
    #[must_use]
    pub fn gated_prep(handle: &BlockHandle) -> Self {
        let mut t = Self::ok("");
        t.prep_gate = Some(handle.gate.clone());
        t
    }

    /// Sugar: a gated-invoke transport and its own fresh handle.
    pub fn hanging_invoke() -> (Self, BlockHandle) {
        let handle = BlockHandle::new();
        (Self::gated_invoke(&handle), handle)
    }

    /// Sugar: a gated-prep transport and its own fresh handle.
    pub fn hanging_prep() -> (Self, BlockHandle) {
        let handle = BlockHandle::new();
        (Self::gated_prep(&handle), handle)
    }

    /// Override the canned scan returned by `scan_workspace`.
    #[must_use]
    pub fn with_scan(mut self, scan: Vec<WorkspaceScan>) -> Self {
        self.scan = scan;
        self
    }

    /// Override the canned shared-plugin skills returned when `include_shared` is set.
    #[must_use]
    pub fn with_shared_skills(mut self, shared: Vec<PluginSkill>) -> Self {
        self.shared = shared;
        self
    }

    /// Override the canned shared plugin library root, reported alongside the
    /// shared skills when `include_shared` is set.
    #[must_use]
    pub fn with_shared_root(mut self, root: &str) -> Self {
        self.shared_root = Some(root.to_string());
        self
    }

    /// Answer every `RunHooks` with `records`.
    ///
    /// The general form of the old canned-`SessionStart`-context knob: injected
    /// context is now derived from the records, so a test scripts the records
    /// and gets the context for free.
    #[must_use]
    pub fn with_server_hook_records(
        mut self,
        records: Vec<horsie_models::hooks::HookRecord>,
    ) -> Self {
        self.server_hook_records = records;
        self
    }

    /// Every `call_id` passed to `cancel`, in order.
    pub fn cancels(&self) -> Vec<String> {
        self.cancels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every tool call this transport was asked to run, in order.
    pub fn invocations(&self) -> Vec<ToolCall> {
        self.invocations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The agent id each invoke carried, in order.
    pub fn agent_ids(&self) -> Vec<String> {
        self.agent_ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl RuntimeTransport for MockTransport {
    async fn relay(
        &self,
        message: RuntimeInboundMessage,
    ) -> Result<RuntimeOutboundMessage, TransportError> {
        match message {
            RuntimeInboundMessage::ToolCall(req) => {
                self.invocations
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(req.call.clone());
                self.agent_ids
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(req.agent_id.clone());
                self.call_ids
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(req.call_id.clone());
                if let Some(gate) = &self.invoke_gate {
                    gate.notified().await;
                }
                let result = match &self.script {
                    None => self.result.clone(),
                    Some(script) => match script.next_step() {
                        Ok(TransportOutcome::Ok(result)) => result,
                        Ok(TransportOutcome::Err(e)) => return Err(e),
                        Ok(TransportOutcome::Hang) => std::future::pending().await,
                        Err(exhausted) => {
                            return Err(TransportError::SendFailed(exhausted.to_string()));
                        }
                    },
                };
                Ok(RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                    call_id: req.call_id,
                    result,
                    hooks: self.hooks.clone(),
                }))
            }
            RuntimeInboundMessage::ScanWorkspace(req) => {
                if let Some(gate) = &self.prep_gate {
                    gate.notified().await;
                }
                let (shared, shared_root) = if req.include_shared {
                    (self.shared.clone(), self.shared_root.clone())
                } else {
                    (Vec::new(), None)
                };
                Ok(RuntimeOutboundMessage::ScanResult(ScanResponse {
                    call_id: req.call_id,
                    workspaces: self.scan.clone(),
                    shared_skills: shared,
                    shared_root,
                }))
            }
            RuntimeInboundMessage::RunHooks(req) => {
                if let Some(gate) = &self.prep_gate {
                    gate.notified().await;
                }
                Ok(RuntimeOutboundMessage::HookRecords(RunHooksResponse {
                    call_id: req.call_id,
                    records: self.server_hook_records.clone(),
                }))
            }
            // A cancel draws no reply, so relaying one would hang a real
            // transport; a test that does it has a bug worth surfacing.
            RuntimeInboundMessage::CancelCall(_) => Err(TransportError::SendFailed(
                "CancelCall must be sent one-way, not relayed".to_string(),
            )),
        }
    }

    async fn send_oneway(&self, message: RuntimeInboundMessage) -> Result<(), TransportError> {
        if let RuntimeInboundMessage::CancelCall(req) = message {
            self.cancels
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(req.call_id);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use horsie_models::runtime::BashInput;
    use std::time::Duration;

    fn bash(cmd: &str) -> ToolCall {
        ToolCall::Bash(BashInput {
            command: cmd.to_string(),
            timeout_secs: None,
        })
    }

    #[tokio::test]
    async fn disconnect_after_serves_then_fails_forever() {
        let t = MockTransport::disconnect_after(1);
        assert!(t.invoke("c1", "test-agent", bash("echo 1")).await.is_ok());
        assert!(matches!(
            t.invoke("c2", "test-agent", bash("echo 2")).await,
            Err(TransportError::Disconnected)
        ));
        assert!(matches!(
            t.invoke("c3", "test-agent", bash("echo 3")).await,
            Err(TransportError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn records_invocations_and_cancels() {
        let t = MockTransport::ok("done");
        let _ = t.invoke("c1", "test-agent", bash("ls")).await;
        let _ = t.cancel("c1").await;
        assert_eq!(t.invocations().len(), 1);
        assert_eq!(t.cancels(), vec!["c1".to_string()]);
    }

    #[tokio::test]
    async fn a_probe_observes_transports_it_does_not_own() {
        // The shape the item 23 e2e needs: the vendor builds the transport, the
        // test holds only the probe.
        let probe = TransportProbe::new();
        let first = MockTransport::ok("").observed_by(&probe);
        let second = MockTransport::ok("").observed_by(&probe);
        let _ = first.invoke("c1", "test-agent", bash("a")).await;
        let _ = second.cancel("c2").await;
        assert_eq!(probe.invocations().len(), 1);
        assert_eq!(probe.cancels(), vec!["c2".to_string()]);
    }

    #[tokio::test]
    async fn one_gate_releases_every_transport_sharing_it() {
        let gate = BlockHandle::new();
        let t = Arc::new(MockTransport::gated_invoke(&gate));
        let call = {
            let t = t.clone();
            tokio::spawn(async move { t.invoke("c1", "test-agent", bash("slow")).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!call.is_finished(), "invoke must block on the gate");
        gate.release();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), call)
                .await
                .expect("release must unblock")
                .unwrap()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn gated_prep_blocks_scan_and_session_start() {
        let (t, handle) = MockTransport::hanging_prep();
        let t = Arc::new(t);
        let scan = {
            let t = t.clone();
            tokio::spawn(async move {
                t.scan_workspace("c1", None, vec![], "*.md".into(), false)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!scan.is_finished(), "scan_workspace must block");
        handle.release();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), scan)
                .await
                .expect("release must unblock the scan")
                .unwrap()
                .is_ok()
        );
    }

    #[tokio::test]
    async fn scripted_exhaustion_surfaces_as_a_transport_error() {
        let t = MockTransport::scripted(Script::of([TransportOutcome::Ok(ToolResult::Ok(
            ToolOutput {
                stdout: "one".into(),
                stderr: String::new(),
                exit_code: 0,
            },
        ))]));
        assert!(t.invoke("c1", "test-agent", bash("a")).await.is_ok());
        assert!(t.invoke("c2", "test-agent", bash("b")).await.is_err());
    }
}
