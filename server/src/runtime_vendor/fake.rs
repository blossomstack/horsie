//! A scriptable vendor agent for tests — never compiled into a production build.
//!
//! It speaks the real `runtime_vendor.fl` protocol over a real WebSocket, in the two
//! shapes production uses: dialing a running server's `/api/vendor/connect`, or
//! serving one end of an in-process duplex pipe. There is deliberately no
//! in-memory shortcut: a test that passes here exercises the same codec,
//! framing, and correlation path a shipped agent does.

// Gated behind `cfg(test)` / `feature = "test-util"`, so this never reaches a
// production build; the workspace no-panic rule is relaxed here exactly as it
// is inside `#[cfg(test)]` modules.
#![allow(clippy::panic)]

use crate::runtime_vendor::{RuntimeSpec, RuntimeVendorLink, WorkspaceSpec};
use futures_util::{SinkExt, StreamExt};
use horsie_models::runtime::{
    RuntimeInboundMessage, RuntimeOutboundMessage, ScanResponse, SessionStartResponse,
    ToolCallResponse, ToolOutput, ToolResult, WorkspaceScan,
};
use horsie_models::runtime_vendor::{
    AttachRuntimeResponse, CreateRuntimeResponse, DeleteRuntimeResponse, QueryRuntimesResponse,
    RequestFailed, RuntimeRelayResponse, RuntimeSpec as WireRuntimeSpec, RuntimeVendorCapabilities,
    RuntimeVendorCommand, RuntimeVendorEvent, RuntimeVendorInboundMessage,
    RuntimeVendorOutboundMessage, RuntimeVendorReady, StopRuntimeResponse,
};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Ordered lifecycle labels and live runtime ids, shared with the agent task.
#[derive(Default)]
struct Recorder {
    signals: Mutex<Vec<String>>,
    live: Mutex<BTreeSet<String>>,
    /// The most recent create request, so a test can assert what the server
    /// actually put on the wire (workspaces, env, provision steps).
    last_create: Mutex<Option<WireRuntimeSpec>>,
    /// Remaining attach failures to inject, and whether creates fail.
    attach_failures: Mutex<u32>,
    tool_calls: Mutex<usize>,
    /// Call ids the server asked to cancel — how a test proves a stop reached
    /// the sandbox instead of merely being dropped locally.
    cancels: Mutex<Vec<String>>,
}

impl Recorder {
    fn record(&self, label: &str) {
        self.signals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(label.to_string());
    }
}

/// How the fake answers a lifecycle command, so tests can exercise the failure
/// branches the real agents have.
#[derive(Clone)]
struct Gate {
    tx: Arc<tokio::sync::watch::Sender<bool>>,
    rx: tokio::sync::watch::Receiver<bool>,
}

impl Gate {
    fn open() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(true);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    fn closed() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(false);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    fn release(&self) {
        let _ = self.tx.send(true);
    }

    /// Wait until released. A watch channel rather than a `Notify` so a release
    /// that lands before the waiter arrives still wakes it.
    async fn wait(&self) {
        let mut rx = self.rx.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Clone, Default)]
struct Faults {
    fail_create: bool,
    fail_attach_times: u32,
    /// Drop the socket after this many tool calls, simulating an agent that
    /// dies mid-session.
    disconnect_after_tool_calls: Option<usize>,
}

pub struct FakeRuntimeVendor {
    recorder: Arc<Recorder>,
    gate: Gate,
    /// Present only for the in-process shape, where the test drives the link
    /// directly instead of going through a server.
    link: Option<Arc<RuntimeVendorLink>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeRuntimeVendor {
    #[must_use]
    pub fn builder(vendor_name: &str) -> FakeRuntimeVendorBuilder {
        FakeRuntimeVendorBuilder {
            vendor_name: vendor_name.to_string(),
            supports_provisioning: true,
            bash_stdout: "ok".to_string(),
            faults: Faults::default(),
            block: false,
        }
    }

    /// Lifecycle signals in order, each `"<action>:<runtime_id>"` — e.g.
    /// `"create:9f3a…"`. One entry per explicit signal, which is what makes
    /// "every user action is exactly one vendor signal" assertable.
    #[must_use]
    pub fn signals(&self) -> Vec<String> {
        self.recorder
            .signals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Runtime ids the agent currently considers alive.
    #[must_use]
    pub fn live_runtimes(&self) -> Vec<String> {
        self.recorder
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// The link, for the in-process shape only.
    ///
    /// # Panics
    /// If called on an agent built with `connect`, where the server owns the link.
    #[must_use]
    pub fn link(&self) -> Arc<RuntimeVendorLink> {
        match &self.link {
            Some(link) => link.clone(),
            None => panic!("link() is only available on an in-process fake agent"),
        }
    }

    /// Let blocked tool calls answer. No-op unless built with
    /// [`FakeRuntimeVendorBuilder::block_tool_calls`].
    pub fn release_tool_calls(&self) {
        self.gate.release();
    }

    /// Call ids the server asked this agent to cancel.
    #[must_use]
    pub fn cancelled_calls(&self) -> Vec<String> {
        self.recorder
            .cancels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The most recent `CreateRuntime` request the server sent.
    #[must_use]
    pub fn last_create_request(&self) -> Option<WireRuntimeSpec> {
        self.recorder
            .last_create
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Drop the socket, so the server observes a disconnect.
    pub fn disconnect(&self) {
        self.task.abort();
    }
}

pub struct FakeRuntimeVendorBuilder {
    vendor_name: String,
    supports_provisioning: bool,
    bash_stdout: String,
    faults: Faults,
    block: bool,
}

impl FakeRuntimeVendorBuilder {
    #[must_use]
    pub fn supports_provisioning(mut self, value: bool) -> Self {
        self.supports_provisioning = value;
        self
    }

    /// Canned stdout every `ToolCall` answers with.
    #[must_use]
    pub fn bash_stdout(mut self, value: &str) -> Self {
        self.bash_stdout = value.to_string();
        self
    }

    /// Fail every `CreateRuntime` with `RequestFailed`.
    #[must_use]
    pub fn fail_create(mut self) -> Self {
        self.faults.fail_create = true;
        self
    }

    /// Fail the first `n` `AttachRuntime` commands, then succeed — the shape a
    /// session's recovery retry has to survive.
    #[must_use]
    pub fn fail_attach_times(mut self, n: u32) -> Self {
        self.faults.fail_attach_times = n;
        self
    }

    /// Drop the socket after `n` tool calls, so the server sees the agent die
    /// mid-session.
    #[must_use]
    pub fn disconnect_after_tool_calls(mut self, n: usize) -> Self {
        self.faults.disconnect_after_tool_calls = Some(n);
        self
    }

    /// Hold every tool call until [`FakeRuntimeVendor::release_tool_calls`], so a
    /// test can act while one is genuinely in flight.
    #[must_use]
    pub fn block_tool_calls(mut self) -> Self {
        self.block = true;
        self
    }

    /// Dial a running server's `/api/vendor/connect`.
    pub async fn connect(self, url: &str) -> Result<FakeRuntimeVendor, String> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| format!("dial {url}: {e}"))?;
        let recorder = Arc::new(Recorder::default());
        let gate = if self.block {
            Gate::closed()
        } else {
            Gate::open()
        };
        let task = tokio::spawn(run_agent(
            ws,
            self.vendor_name,
            self.supports_provisioning,
            self.bash_stdout,
            self.faults,
            gate.clone(),
            recorder.clone(),
        ));
        Ok(FakeRuntimeVendor {
            recorder,
            gate,
            link: None,
            task,
        })
    }

    /// Serve one end of an in-process duplex pipe and hand back the link the
    /// server side would hold.
    pub async fn serve_in_process(self) -> Result<FakeRuntimeVendor, String> {
        use tokio_tungstenite::tungstenite::protocol::Role;
        let (a, b) = tokio::io::duplex(256 * 1024);
        let server = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let agent = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        let recorder = Arc::new(Recorder::default());
        let gate = if self.block {
            Gate::closed()
        } else {
            Gate::open()
        };
        let task = tokio::spawn(run_agent(
            agent,
            self.vendor_name,
            self.supports_provisioning,
            self.bash_stdout,
            self.faults,
            gate.clone(),
            recorder.clone(),
        ));
        let link = RuntimeVendorLink::start(server).await?;
        Ok(FakeRuntimeVendor {
            recorder,
            gate,
            link: Some(link),
            task,
        })
    }
}

/// The agent loop, shared by both shapes.
///
/// It answers `ScanWorkspace` and `SessionStart` unconditionally. That is not
/// optional politeness: `session_actor` calls `scan_workspace()` at session
/// creation regardless of vendor, so an agent that ignores it hangs session
/// provisioning forever with no error output.
async fn run_agent<S>(
    ws: WebSocketStream<S>,
    vendor_name: String,
    supports_provisioning: bool,
    bash_stdout: String,
    faults: Faults,
    gate: Gate,
    recorder: Arc<Recorder>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    *recorder
        .attach_failures
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = faults.fail_attach_times;

    let (sink, mut stream) = ws.split();
    // Shared so a blocked tool call can be answered from its own task without
    // stalling the read loop — otherwise the cancel that releases it could
    // never be read.
    let sink = Arc::new(tokio::sync::Mutex::new(sink));

    let boot = RuntimeVendorOutboundMessage {
        request_id: "boot".to_string(),
        event: RuntimeVendorEvent::Ready(RuntimeVendorReady {
            vendor_name,
            capabilities: RuntimeVendorCapabilities {
                supports_provisioning,
            },
        }),
    };
    let Ok(json) = serde_json::to_string(&boot) else {
        return;
    };
    if sink
        .lock()
        .await
        .send(Message::Text(json.into()))
        .await
        .is_err()
    {
        return;
    }

    while let Some(Ok(Message::Text(text))) = stream.next().await {
        let Ok(inbound) = serde_json::from_str::<RuntimeVendorInboundMessage>(&text) else {
            continue;
        };
        let reply = match inbound.command {
            RuntimeVendorCommand::CreateRuntime(cmd) => {
                recorder.record(&format!("create:{}", cmd.runtime_id));
                *recorder
                    .last_create
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(cmd.spec.clone());
                if faults.fail_create {
                    Some(RuntimeVendorEvent::RequestFailed(RequestFailed {
                        message: "fake agent: create failed".to_string(),
                    }))
                } else {
                    recorder
                        .live
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(cmd.runtime_id.clone());
                    Some(RuntimeVendorEvent::CreateRuntime(CreateRuntimeResponse {
                        runtime_id: cmd.runtime_id,
                    }))
                }
            }
            RuntimeVendorCommand::AttachRuntime(cmd) => {
                recorder.record(&format!("attach:{}", cmd.runtime_id));
                let remaining = {
                    let mut g = recorder
                        .attach_failures
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    let n = *g;
                    *g = g.saturating_sub(1);
                    n
                };
                if remaining > 0 {
                    Some(RuntimeVendorEvent::RequestFailed(RequestFailed {
                        message: "fake agent: attach failed".to_string(),
                    }))
                } else {
                    recorder
                        .live
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(cmd.runtime_id.clone());
                    Some(RuntimeVendorEvent::AttachRuntime(AttachRuntimeResponse {
                        runtime_id: cmd.runtime_id,
                    }))
                }
            }
            RuntimeVendorCommand::StopRuntime(cmd) => {
                recorder.record(&format!("stop:{}", cmd.runtime_id));
                recorder
                    .live
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&cmd.runtime_id);
                Some(RuntimeVendorEvent::StopRuntime(StopRuntimeResponse {
                    runtime_id: cmd.runtime_id,
                }))
            }
            RuntimeVendorCommand::DeleteRuntime(cmd) => {
                recorder.record(&format!("delete:{}", cmd.runtime_id));
                recorder
                    .live
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&cmd.runtime_id);
                Some(RuntimeVendorEvent::DeleteRuntime(DeleteRuntimeResponse {
                    runtime_id: cmd.runtime_id,
                }))
            }
            RuntimeVendorCommand::QueryRuntimes(_) => {
                recorder.record("query");
                let runtimes = recorder
                    .live
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .map(|id| horsie_models::executor::RuntimeInfo {
                        runtime_id: id.clone(),
                        state: horsie_models::executor::RuntimeState::Running,
                        restart_count: 0,
                    })
                    .collect();
                Some(RuntimeVendorEvent::QueryRuntimes(QueryRuntimesResponse {
                    runtimes,
                }))
            }
            RuntimeVendorCommand::Runtime(cmd) => {
                let runtime_id = cmd.runtime_id;
                let answer = match cmd.message {
                    RuntimeInboundMessage::ToolCall(req) => {
                        let seen = {
                            let mut g = recorder
                                .tool_calls
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            *g += 1;
                            *g
                        };
                        if faults
                            .disconnect_after_tool_calls
                            .is_some_and(|limit| seen > limit)
                        {
                            // Hang up mid-call: the server must observe the
                            // transport die rather than wait forever.
                            return;
                        }
                        // A blocked call must not stall the read loop, or the
                        // cancel that releases it could never arrive.
                        gate.wait().await;
                        Some(RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                            call_id: req.call_id,
                            result: ToolResult::Ok(ToolOutput {
                                stdout: bash_stdout.clone(),
                                stderr: String::new(),
                                exit_code: 0,
                            }),
                        }))
                    }
                    RuntimeInboundMessage::CancelCall(req) => {
                        recorder.record("cancel");
                        recorder
                            .cancels
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(req.call_id);
                        // Unblock the call this cancel targets. A real runtime
                        // abandons the command and answers with an error, and the
                        // server's stop path waits for that unwind — leaving it
                        // blocked deadlocks the stop instead of testing it.
                        gate.release();
                        // One-way by protocol: no reply.
                        None
                    }
                    RuntimeInboundMessage::ScanWorkspace(req) => {
                        Some(RuntimeOutboundMessage::ScanResult(ScanResponse {
                            call_id: req.call_id,
                            workspaces: vec![WorkspaceScan {
                                name: "main".to_string(),
                                path: "/fake/main".to_string(),
                                is_git_repo: false,
                                instructions: None,
                                skills: vec![],
                                platform: Some("linux-x86_64".to_string()),
                            }],
                            shared_skills: vec![],
                        }))
                    }
                    RuntimeInboundMessage::SessionStart(req) => Some(
                        RuntimeOutboundMessage::SessionStartResult(SessionStartResponse {
                            call_id: req.call_id,
                            context: String::new(),
                        }),
                    ),
                };
                answer.map(|message| {
                    RuntimeVendorEvent::Runtime(RuntimeRelayResponse {
                        runtime_id,
                        message,
                    })
                })
            }
        };

        let Some(event) = reply else { continue };
        let out = RuntimeVendorOutboundMessage {
            request_id: inbound.request_id,
            event,
        };
        let Ok(json) = serde_json::to_string(&out) else {
            continue;
        };
        if sink
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
            .is_err()
        {
            return;
        }
    }
}

/// A `RuntimeSpec` with a real capability file on disk, so `CapabilitySpec::load`
/// succeeds. The temp dir is leaked deliberately: it must outlive the spec, and
/// tests are short-lived.
#[must_use]
pub fn runtime_spec_fixture(workspace: &str) -> RuntimeSpec {
    use horsie_models::capabilities::{BlockNetwork, CapabilitySpec, NetworkPolicy};
    let dir = std::env::temp_dir().join(format!("horsie-fake-agent-{}", uuid::Uuid::new_v4()));
    let path = dir.join("capabilities.json");
    let spec = CapabilitySpec {
        network: NetworkPolicy::Block(BlockNetwork {}),
        grants: vec![],
        unsafe_seatbelt_rules: None,
    };
    let write = std::fs::create_dir_all(&dir)
        .map_err(|e| e.to_string())
        .and_then(|()| serde_json::to_vec(&spec).map_err(|e| e.to_string()))
        .and_then(|bytes| std::fs::write(&path, bytes).map_err(|e| e.to_string()));
    if let Err(e) = write {
        tracing::error!(error = %e, "fake agent fixture: could not write capability file");
    }
    RuntimeSpec {
        workspaces: vec![WorkspaceSpec {
            name: workspace.to_string(),
        }],
        provision: vec![],
        env: vec![],
        capabilities_file: path,
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

    use horsie_runtime_client::RuntimeTransport;

    #[tokio::test]
    async fn fake_agent_answers_scan_so_session_provisioning_cannot_hang() {
        let agent = FakeRuntimeVendor::builder("test-agent")
            .bash_stdout("ok")
            .serve_in_process()
            .await
            .expect("agent");
        let transport =
            crate::runtime_vendor::RuntimeVendorTransport::new(agent.link(), "rt-1".to_string());
        let (workspaces, shared) = transport
            .scan_workspace(
                "scan-1",
                None,
                vec!["AGENTS.md".to_string()],
                "skills/**/*.md".to_string(),
                false,
            )
            .await
            .expect("scan must be answered, not hang");
        assert!(shared.is_empty());
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "main");
    }

    /// A cancel is the one relayed message that draws no reply, so nothing
    /// upstream fails if it silently goes nowhere. The only other assertion that
    /// it reaches the vendor lives in an `#[ignore]`d e2e (the `POST /stop` port
    /// gap), which would leave the one-way branch of the relay uncovered.
    #[tokio::test]
    async fn a_cancel_reaches_the_vendor_as_a_one_way_relay() {
        let agent = FakeRuntimeVendor::builder("test-agent")
            .serve_in_process()
            .await
            .expect("agent");
        let transport =
            crate::runtime_vendor::RuntimeVendorTransport::new(agent.link(), "rt-1".to_string());
        transport.cancel("call-7").await.expect("cancel must send");

        // One-way by protocol: the send returns before the agent has read it.
        for _ in 0..100 {
            if agent.cancelled_calls() == vec!["call-7".to_string()] {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!(
            "the cancel never reached the vendor (saw {:?})",
            agent.cancelled_calls()
        );
    }

    #[tokio::test]
    async fn fake_agent_records_lifecycle_signals_in_order() {
        let agent = FakeRuntimeVendor::builder("test-agent")
            .serve_in_process()
            .await
            .expect("agent");
        let link = agent.link();
        let spec = runtime_spec_fixture("main");
        let rt = link.create("rt-1", &spec).await.expect("create");
        assert_eq!(agent.live_runtimes(), vec!["rt-1".to_string()]);
        rt.handle.stop().await;
        assert_eq!(
            agent.signals(),
            vec!["create:rt-1".to_string(), "stop:rt-1".to_string()]
        );
        assert!(agent.live_runtimes().is_empty());
    }
}
