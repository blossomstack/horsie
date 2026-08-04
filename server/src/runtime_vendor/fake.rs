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
    CreateRuntimeResponse, DeleteRuntimeResponse, GetRuntimeResponse, HibernateRuntimeResponse,
    QueryRuntimesResponse, RequestFailed, RuntimeRelayResponse, RuntimeSpec as WireRuntimeSpec,
    RuntimeVendorCapabilities, RuntimeVendorCommand, RuntimeVendorEvent,
    RuntimeVendorInboundMessage, RuntimeVendorOutboundMessage, RuntimeVendorReady,
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
    gone_on_get: Mutex<bool>,
    tool_calls: Mutex<usize>,
    /// The agent id on each relayed tool call, in order — how a test proves the
    /// caller identity survives the trip across the vendor link.
    tool_agent_ids: Mutex<Vec<String>>,
    /// Call ids the server asked to cancel — how a test proves a stop reached
    /// the sandbox instead of merely being dropped locally.
    cancels: Mutex<Vec<String>>,
    /// Why the server refused to publish this agent, if it did.
    rejection: Mutex<Option<String>>,
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
    gone_on_get: bool,
    /// Hold every create until released, so a test can issue a get while one
    /// is genuinely in flight.
    block_create: bool,
    /// Drop the socket after this many tool calls, simulating an agent that
    /// dies mid-session.
    disconnect_after_tool_calls: Option<usize>,
}

pub struct FakeRuntimeVendor {
    recorder: Arc<Recorder>,
    /// This fake's process identity, kept so `resuming` can inherit it.
    instance_id: String,
    gate: Gate,
    create_gate: Gate,
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
            // A fresh id per builder, so two fakes are two agent processes
            // unless a test says otherwise with `resuming`.
            instance_id: uuid::Uuid::new_v4().to_string(),
            supports_provisioning: true,
            bash_stdout: "ok".to_string(),
            faults: Faults::default(),
            block: false,
            resume: None,
            owner: crate::auth::Principal::Anonymous,
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

    /// Wait for this agent to stop serving and report why the server refused
    /// it, or `None` if it stopped for any other reason.
    ///
    /// A refused agent's socket is closed by the server, so this resolves
    /// without needing a timeout of its own.
    pub async fn refusal(&mut self) -> Option<String> {
        let _ = (&mut self.task).await;
        self.recorder
            .rejection
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

    /// A one-entry vendor registry publishing this agent under `name`.
    ///
    /// The shape everything that reaches a runtime needs: a client resolves its
    /// vendor through the registry on every call, never through a link it
    /// captured, so tests hand it the same map the server keeps.
    ///
    /// # Panics
    /// If called on an agent built with `connect`, where the server owns the link.
    #[must_use]
    pub fn published_as(&self, name: &str) -> crate::sessions::spec::SharedVendors {
        let mut map = std::collections::HashMap::new();
        map.insert(name.to_string(), self.link());
        Arc::new(std::sync::RwLock::new(map))
    }

    /// Let blocked tool calls answer. No-op unless built with
    /// [`FakeRuntimeVendorBuilder::block_tool_calls`].
    pub fn release_tool_calls(&self) {
        self.gate.release();
    }

    /// Let blocked creates answer. No-op unless built with
    /// [`FakeRuntimeVendorBuilder::block_creates`].
    pub fn release_creates(&self) {
        self.create_gate.release();
    }

    /// The agent id each relayed tool call carried, in order.
    #[must_use]
    pub fn tool_agent_ids(&self) -> Vec<String> {
        self.recorder
            .tool_agent_ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
    /// The agent *process* this fake stands in for. Two fakes with different
    /// ids are two processes competing for one name; `resuming` is how a test
    /// says "the same process came back".
    instance_id: String,
    supports_provisioning: bool,
    bash_stdout: String,
    faults: Faults,
    block: bool,
    /// Runtime state carried over from a previous agent process — see
    /// [`FakeRuntimeVendorBuilder::resuming`].
    resume: Option<Arc<Recorder>>,
    /// Who this fake agent authenticated as. Defaults to `Anonymous`, matching
    /// an auth-disabled deployment.
    owner: crate::auth::Principal,
}

impl FakeRuntimeVendorBuilder {
    /// Authenticate as this principal, so tests can exercise vendor-name
    /// ownership.
    #[must_use]
    pub fn owned_by(mut self, owner: crate::auth::Principal) -> Self {
        self.owner = owner;
        self
    }

    /// Come back as the same vendor, remembering the runtimes `prior` created.
    ///
    /// A real vendor's runtimes outlive its agent process — that is the whole
    /// point of hibernate — so a reconnecting agent still owns them. Without
    /// this, a fresh fake reports every runtime `Gone`, which is a truthful
    /// answer to a *different* question and turns "the vendor came back" into
    /// "the vendor lost everything".
    #[must_use]
    pub fn resuming(mut self, prior: &FakeRuntimeVendor) -> Self {
        self.resume = Some(prior.recorder.clone());
        // Same process, so the same instance id: the server must see this as
        // the agent reclaiming its own name, not a second one taking it.
        self.instance_id = prior.instance_id.clone();
        self
    }

    /// Announce a specific instance id. Only tests about name collisions have a
    /// reason to care what it is.
    #[must_use]
    pub fn instance_id(mut self, value: &str) -> Self {
        self.instance_id = value.to_string();
        self
    }

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

    /// Answer every `GetRuntime` with a failure, so a test can drive the
    /// terminal `RuntimeGone` path without tearing the agent down.
    #[must_use]
    pub fn gone_on_get(mut self, value: bool) -> Self {
        self.faults.gone_on_get = value;
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

    /// Hold every create until [`FakeRuntimeVendor::release_creates`], so a
    /// test can prove a get waits for an in-flight create.
    #[must_use]
    pub fn block_creates(mut self) -> Self {
        self.faults.block_create = true;
        self
    }

    /// Dial a running server's `/api/vendor/connect`.
    pub async fn connect(self, url: &str) -> Result<FakeRuntimeVendor, String> {
        self.connect_with_token(url, None).await
    }

    /// Dial presenting a bearer, as a real agent does against a server with
    /// authentication on.
    pub async fn connect_with_token(
        self,
        url: &str,
        token: Option<&str>,
    ) -> Result<FakeRuntimeVendor, String> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = url
            .into_client_request()
            .map_err(|e| format!("bad url {url}: {e}"))?;
        if let Some(t) = token {
            let value = format!("Bearer {t}")
                .parse()
                .map_err(|e| format!("bad token header: {e}"))?;
            request.headers_mut().insert(
                tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
                value,
            );
        }
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| format!("dial {url}: {e}"))?;
        let recorder = self
            .resume
            .clone()
            .unwrap_or_else(|| Arc::new(Recorder::default()));
        let gate = if self.block {
            Gate::closed()
        } else {
            Gate::open()
        };
        let create_gate = if self.faults.block_create {
            Gate::closed()
        } else {
            Gate::open()
        };
        let instance_id = self.instance_id.clone();
        let task = tokio::spawn(run_agent(
            ws,
            self,
            gate.clone(),
            create_gate.clone(),
            recorder.clone(),
        ));
        Ok(FakeRuntimeVendor {
            recorder,
            instance_id,
            gate,
            create_gate,
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
        let recorder = self
            .resume
            .clone()
            .unwrap_or_else(|| Arc::new(Recorder::default()));
        let gate = if self.block {
            Gate::closed()
        } else {
            Gate::open()
        };
        let create_gate = if self.faults.block_create {
            Gate::closed()
        } else {
            Gate::open()
        };
        let owner = self.owner.clone();
        let instance_id = self.instance_id.clone();
        let task = tokio::spawn(run_agent(
            agent,
            self,
            gate.clone(),
            create_gate.clone(),
            recorder.clone(),
        ));
        let link = RuntimeVendorLink::start(server, owner).await?;
        Ok(FakeRuntimeVendor {
            recorder,
            instance_id,
            gate,
            create_gate,
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
/// Takes the builder whole rather than its fields one by one: the builder *is*
/// this agent's configuration, and both gates are already derived from it by
/// the caller.
async fn run_agent<S>(
    ws: WebSocketStream<S>,
    config: FakeRuntimeVendorBuilder,
    gate: Gate,
    create_gate: Gate,
    recorder: Arc<Recorder>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let FakeRuntimeVendorBuilder {
        owner: _,
        vendor_name,
        instance_id,
        supports_provisioning,
        bash_stdout,
        faults,
        block: _,
        resume: _,
    } = config;
    *recorder
        .gone_on_get
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = faults.gone_on_get;

    let (sink, mut stream) = ws.split();
    // Shared so a blocked tool call can be answered from its own task without
    // stalling the read loop — otherwise the cancel that releases it could
    // never be read.
    let sink = Arc::new(tokio::sync::Mutex::new(sink));

    let boot = RuntimeVendorOutboundMessage {
        request_id: "boot".to_string(),
        event: RuntimeVendorEvent::Ready(RuntimeVendorReady {
            vendor_name,
            instance_id,
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
                // Deliberately blocks this loop: lifecycle commands for one
                // runtime are serialized, so a get arriving mid-create is not
                // even read until the create resolves. That is the contract.
                create_gate.wait().await;
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
            // A get answers from what the agent actually holds, so a test that
            // never created (or that asked for `gone_on_get`) exercises the
            // terminal path instead of silently provisioning.
            RuntimeVendorCommand::GetRuntime(cmd) => {
                recorder.record(&format!("get:{}", cmd.runtime_id));
                let forced_gone = *recorder
                    .gone_on_get
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let live = recorder
                    .live
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains(&cmd.runtime_id);
                if forced_gone || !live {
                    Some(RuntimeVendorEvent::RequestFailed(RequestFailed {
                        message: format!("fake agent: no runtime '{}'", cmd.runtime_id),
                    }))
                } else {
                    Some(RuntimeVendorEvent::GetRuntime(GetRuntimeResponse {
                        runtime_id: cmd.runtime_id,
                    }))
                }
            }
            // Advisory and declined, exactly like the real process-backed
            // agent: the runtime stays live, so a later get still succeeds.
            RuntimeVendorCommand::HibernateRuntime(cmd) => {
                recorder.record(&format!("hibernate:{}", cmd.runtime_id));
                Some(RuntimeVendorEvent::HibernateRuntime(
                    HibernateRuntimeResponse {
                        runtime_id: cmd.runtime_id,
                    },
                ))
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
                        recorder
                            .tool_agent_ids
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(req.agent_id.clone());
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
                            hooks: Vec::new(),
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
                            shared_root: None,
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
            // Published. A real agent starts its heartbeat here; this one has
            // nothing to do but keep serving.
            RuntimeVendorCommand::VendorRegistered(_) => None,
            // Refused, and no retry can change that: record why and stop, the
            // way `horsie connect` exits instead of backing off forever.
            RuntimeVendorCommand::VendorRejected(rejected) => {
                *recorder
                    .rejection
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(rejected.reason);
                return;
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

/// A `RuntimeSpec` naming one workspace.
#[must_use]
pub fn runtime_spec_fixture(workspace: &str) -> RuntimeSpec {
    RuntimeSpec {
        workspaces: vec![WorkspaceSpec {
            name: workspace.to_string(),
        }],
        provision: vec![],
        env: vec![],
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
        let transport = crate::runtime_vendor::RuntimeVendorTransport::new(
            agent.published_as("test-agent"),
            "test-agent".to_string(),
            "rt-1".to_string(),
        );
        let resp = transport
            .scan_workspace(
                "scan-1",
                None,
                vec!["AGENTS.md".to_string()],
                "skills/**/*.md".to_string(),
                false,
            )
            .await
            .expect("scan must be answered, not hang");
        assert!(resp.shared_skills.is_empty());
        assert_eq!(resp.workspaces.len(), 1);
        assert_eq!(resp.workspaces[0].name, "main");
    }

    /// A cancel is the one relayed message that draws no reply, so nothing
    /// upstream fails if it silently goes nowhere. The only other assertion that
    /// it reaches the vendor lives in an `#[ignore]`d e2e (the `POST /stop` port
    /// gap), which would leave the one-way branch of the relay uncovered.
    /// The identity has to survive the whole path — `RuntimeClient` stamps it,
    /// the vendor link relays the message verbatim, and the agent reads it back
    /// off the wire. Stamping and the runtime's use of the id are unit-tested
    /// separately; without this, the plumbing between them is not.
    #[tokio::test]
    async fn the_agent_id_survives_the_trip_across_the_vendor_link() {
        use horsie_models::runtime::{BashInput, ToolCall};
        use horsie_runtime_client::RuntimeClient;

        let agent = FakeRuntimeVendor::builder("test-agent")
            .serve_in_process()
            .await
            .expect("agent");
        let transport = crate::runtime_vendor::RuntimeVendorTransport::new(
            agent.published_as("test-agent"),
            "test-agent".to_string(),
            "rt-1".to_string(),
        );
        let client = RuntimeClient::from_arc(std::sync::Arc::new(transport), "main-agent");

        client
            .invoke(ToolCall::Bash(BashInput {
                command: "true".to_string(),
                timeout_secs: None,
            }))
            .await
            .expect("tool call");
        assert_eq!(agent.tool_agent_ids(), vec!["main-agent".to_string()]);

        // A subagent's derived handle carries its own id over the same link.
        client
            .clone()
            .with_agent_id("sub-1")
            .invoke(ToolCall::Bash(BashInput {
                command: "true".to_string(),
                timeout_secs: None,
            }))
            .await
            .expect("subagent tool call");
        assert_eq!(
            agent.tool_agent_ids(),
            vec!["main-agent".to_string(), "sub-1".to_string()]
        );
    }

    #[tokio::test]
    async fn a_cancel_reaches_the_vendor_as_a_one_way_relay() {
        let agent = FakeRuntimeVendor::builder("test-agent")
            .serve_in_process()
            .await
            .expect("agent");
        let transport = crate::runtime_vendor::RuntimeVendorTransport::new(
            agent.published_as("test-agent"),
            "test-agent".to_string(),
            "rt-1".to_string(),
        );
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
        link.create("rt-1", &spec).await.expect("create");
        assert_eq!(agent.live_runtimes(), vec!["rt-1".to_string()]);
        link.hibernate("rt-1").await;
        assert_eq!(
            agent.signals(),
            vec!["create:rt-1".to_string(), "hibernate:rt-1".to_string()]
        );
    }

    #[tokio::test]
    async fn get_after_create_returns_a_client() {
        let agent = FakeRuntimeVendor::builder("test-agent")
            .serve_in_process()
            .await
            .expect("agent");
        let link = agent.link();
        link.create("rt-1", &runtime_spec_fixture("main"))
            .await
            .expect("create");
        link.get("rt-1").await.expect("get must find the runtime");
        assert_eq!(
            agent.signals(),
            vec!["create:rt-1".to_string(), "get:rt-1".to_string()]
        );
    }

    #[tokio::test]
    async fn get_without_create_is_gone() {
        let agent = FakeRuntimeVendor::builder("test-agent")
            .serve_in_process()
            .await
            .expect("agent");
        let err = agent
            .link()
            .get("rt-1")
            .await
            .expect_err("a get must never provision");
        assert!(
            matches!(err, crate::runtime_vendor::VendorError::Gone(_)),
            "an absent runtime is Gone, not Unavailable: {err:?}"
        );
    }

    #[tokio::test]
    async fn hibernate_then_get_still_returns_a_client() {
        let agent = FakeRuntimeVendor::builder("test-agent")
            .serve_in_process()
            .await
            .expect("agent");
        let link = agent.link();
        link.create("rt-1", &runtime_spec_fixture("main"))
            .await
            .expect("create");
        link.hibernate("rt-1").await;
        // Hibernate is advisory; this agent keeps the runtime, so the session
        // it belongs to is still resumable.
        link.get("rt-1").await.expect("get after hibernate");
    }

    #[tokio::test]
    async fn get_during_an_in_flight_create_waits_for_it() {
        let agent = FakeRuntimeVendor::builder("test-agent")
            .block_creates()
            .serve_in_process()
            .await
            .expect("agent");
        let link = agent.link();

        let creating = {
            let link = link.clone();
            tokio::spawn(async move { link.create("rt-1", &runtime_spec_fixture("main")).await })
        };
        // The create is parked in the agent. A get issued now must not resolve
        // to Gone just because the runtime is not there *yet*.
        let getting = {
            let link = link.clone();
            tokio::spawn(async move { link.get("rt-1").await })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), async {})
                .await
                .is_ok()
        );
        assert!(!getting.is_finished(), "the get must wait for the create");

        agent.release_creates();
        creating.await.expect("join").expect("create");
        getting
            .await
            .expect("join")
            .expect("get after create lands");
    }
}
