# Vendor Agent Protocol and Link Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the server a working WebSocket link to an external runtime-vendor agent, so a session can run a full turn against a vendor the server never spawns.

**Architecture:** A new `vendor.fl` schema defines one command/event protocol. `VendorLink` owns one agent's WebSocket, correlates replies by `request_id`, and implements the *existing* `RuntimeVendor` trait — so `VendorAgentRegistry` can insert it into the same `ServerDeps.vendors` map `LocalDaemonRegistry` already writes to, and the session layer needs no changes at all. `VendorRuntimeTransport` implements `RuntimeTransport`, so `RuntimeClient` works unmodified.

**Tech Stack:** Rust 2024, fluorite 0.6 schema codegen, axum 0.7, tokio-tungstenite 0.26, tokio.

## Global Constraints

- **Do not add any in-memory transport or in-memory client.** `InMemExecutorTransport` and `ExecutorClient` are being deleted in PR 4; nothing new may depend on them. Tests use a real WebSocket over `tokio::io::duplex` or a real TCP listener.
- **`/api/runtime/connect` stays working.** It is deleted in PR 5, not here. Do not touch `server/src/http/runtime_connect.rs`, `LocalDaemonVendor`, or `VelosVendor`.
- **Workspace clippy lints deny `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`.** Non-test code must handle every error path explicitly and match every enum variant. Test modules carry `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]`, matching the existing pattern in `server/src/vendor/local.rs`.
- **Fluorite type names are global across packages** (single-file codegen). Every new type in `vendor.fl` is prefixed `Vendor*` to avoid colliding with `executor.fl`.
- **Verification command:** `make check` (runs `fmt-check`, `clippy`, `test`). Individual tests: `cargo test -p horsie-server <name> -- --nocapture`.
- The TypeScript drift job does not cover `vendor.fl` — `clients/*/package.json` list schema files explicitly and this is a server↔agent protocol. Do not add it to those lists.

---

### Task 1: The `vendor.fl` protocol schema

**Files:**
- Create: `models/fluorite/vendor.fl`
- Modify: `models/src/lib.rs` (add the `vendor` module include)
- Test: `models/src/lib.rs` (test module at the bottom)

**Interfaces:**
- Produces: `horsie_models::vendor::{VendorCommand, VendorEvent, VendorInboundMessage, VendorOutboundMessage, VendorRuntimeRequest, VendorCapabilities, VendorCreateRuntime, VendorAttachRuntime, VendorStopRuntime, VendorDeleteRuntime, VendorQueryRuntimes, VendorToolCall, VendorCancelToolCall, VendorScanWorkspace, VendorSessionStart, VendorRegistered, VendorRuntimeStateChanged, VendorRuntimesListed, VendorCommandFailed, VendorToolResult, VendorScanResult, VendorSessionStartResult}`. All derive `Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema` plus `derive_new::new`.

- [ ] **Step 1: Write `models/fluorite/vendor.fl`**

```
/// Protocol types for server <-> runtime-vendor-agent communication.
///
/// A vendor agent is an external process that owns runtime lifecycle for one
/// vendor: it spawns/schedules runtimes, listens for their dial-back, and
/// relays every tool call between them and the server over this one link.
package vendor;

use capabilities.CapabilitySpec;
use executor.EnvVar;
use executor.ProvisionStep;
use executor.RuntimeInfo;
use executor.RuntimeState;
use runtime.ScanRequest;
use runtime.ScanResponse;
use runtime.SessionStartRequest;
use runtime.SessionStartResponse;
use runtime.ToolCallRequest;
use runtime.ToolResult;

/// What a vendor can do with a session workspace, announced by the agent
/// itself so the server and UI never branch on vendor name or kind.
struct VendorCapabilities {
    /// The vendor provisions a fresh workspace it owns — cloning repos,
    /// installing skill bundles, running provision steps. An agent fixed to a
    /// user-owned directory announces false.
    supports_provisioning: bool,
}

/// Everything the server can supply about a runtime. Deliberately minimal:
/// anything the agent knows better (workspace paths, plugin unpack dirs,
/// artifact base URLs) is resolved agent-side and never crosses the wire.
struct VendorRuntimeRequest {
    /// Workspace *names*. The agent resolves each to a path it owns and fails
    /// the command with VendorCommandFailed if it cannot honor one.
    workspaces: Vec<String>,
    /// Resolved secrets and handles only the server can mint: the scoped
    /// GitHub token, the plugin bundle manifest, the plugins token.
    env: Vec<EnvVar>,
    /// Setup steps the runtime executes before the agent loop.
    provision: Vec<ProvisionStep>,
    /// Sandbox policy, inline rather than as a server-local file path. The
    /// agent writes it to its own disk and passes --sandbox-caps.
    sandbox_capabilities: Option<CapabilitySpec>,
}

// --- Commands (server -> agent) ---

struct VendorCreateRuntime { runtime_id: String, request: VendorRuntimeRequest }
/// Revive a preserved runtime. Agents that cannot resume in place provision a
/// fresh instance against the same request.
struct VendorAttachRuntime { runtime_id: String, request: VendorRuntimeRequest }
/// Halt without destroying — the runtime stays re-attachable.
struct VendorStopRuntime { runtime_id: String }
/// The owning session was deleted; the agent decides the runtime's fate.
struct VendorDeleteRuntime { runtime_id: String }
/// Asked on reconnect so the server can reconcile what survived.
struct VendorQueryRuntimes {}
struct VendorToolCall { runtime_id: String, call: ToolCallRequest }
struct VendorCancelToolCall { runtime_id: String, call_id: String }
struct VendorScanWorkspace { runtime_id: String, request: ScanRequest }
struct VendorSessionStart { runtime_id: String, request: SessionStartRequest }

#[type_tag = "type"]
union VendorCommand {
    CreateRuntime(VendorCreateRuntime),
    AttachRuntime(VendorAttachRuntime),
    StopRuntime(VendorStopRuntime),
    DeleteRuntime(VendorDeleteRuntime),
    QueryRuntimes(VendorQueryRuntimes),
    ToolCall(VendorToolCall),
    CancelToolCall(VendorCancelToolCall),
    ScanWorkspace(VendorScanWorkspace),
    SessionStart(VendorSessionStart),
}

struct VendorInboundMessage { request_id: String, command: VendorCommand }

// --- Events (agent -> server) ---

/// First message an agent sends. `vendor_name` is the name sessions select by.
struct VendorRegistered { vendor_name: String, capabilities: VendorCapabilities }
struct VendorRuntimeStateChanged { runtime_id: String, state: RuntimeState }
struct VendorRuntimesListed { runtimes: Vec<RuntimeInfo> }
struct VendorCommandFailed { message: String }
struct VendorToolResult { runtime_id: String, call_id: String, result: ToolResult }
struct VendorScanResult { runtime_id: String, response: ScanResponse }
struct VendorSessionStartResult { runtime_id: String, response: SessionStartResponse }

#[type_tag = "type"]
union VendorEvent {
    Registered(VendorRegistered),
    RuntimeStateChanged(VendorRuntimeStateChanged),
    RuntimesListed(VendorRuntimesListed),
    CommandFailed(VendorCommandFailed),
    ToolResult(VendorToolResult),
    ScanResult(VendorScanResult),
    SessionStartResult(VendorSessionStartResult),
}

/// request_id echoes the command's for responses; a fresh UUID for
/// unsolicited events (state changes, the initial Registered).
struct VendorOutboundMessage { request_id: String, event: VendorEvent }
```

- [ ] **Step 2: Register the module in `models/src/lib.rs`**

Insert immediately before the existing `pub mod workflow {` block:

```rust
#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod vendor {
    include!(concat!(env!("OUT_DIR"), "/vendor/mod.rs"));
}
```

- [ ] **Step 3: Write the failing round-trip test**

Append to the existing `#[cfg(test)] mod tests` block at the bottom of `models/src/lib.rs`:

```rust
#[test]
fn vendor_command_round_trips_with_a_type_tag() {
    use crate::vendor::{VendorCommand, VendorInboundMessage, VendorStopRuntime};
    let msg = VendorInboundMessage {
        request_id: "req-1".to_string(),
        command: VendorCommand::StopRuntime(VendorStopRuntime {
            runtime_id: "rt-1".to_string(),
        }),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"StopRuntime\""), "{json}");
    let back: VendorInboundMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn vendor_event_round_trips_and_carries_capabilities() {
    use crate::vendor::{VendorCapabilities, VendorEvent, VendorOutboundMessage, VendorRegistered};
    let msg = VendorOutboundMessage {
        request_id: "req-2".to_string(),
        event: VendorEvent::Registered(VendorRegistered {
            vendor_name: "my-laptop".to_string(),
            capabilities: VendorCapabilities {
                supports_provisioning: false,
            },
        }),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let back: VendorOutboundMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back, msg);
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p horsie-models vendor_ -- --nocapture`
Expected: compile error — `could not find 'vendor' in the crate root` — before Step 2 is applied; after Step 2 but before Step 1 the build fails on the missing generated file.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie-models vendor_ -- --nocapture`
Expected: 2 passed.

- [ ] **Step 6: Commit**

```bash
git add models/fluorite/vendor.fl models/src/lib.rs
git commit -m "feat: add the vendor agent protocol schema"
```

---

### Task 2: `VendorLink` — one agent's WebSocket with request correlation

**Files:**
- Create: `server/src/vendor/link.rs`
- Modify: `server/src/vendor/mod.rs` (add `mod link;` and re-export)
- Test: `server/src/vendor/link.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `horsie_models::vendor::*` from Task 1.
- Produces:
  - `VendorLink::start<S>(ws: WebSocketStream<S>) -> Result<Arc<VendorLink>, String>` where `S: AsyncRead + AsyncWrite + Unpin + Send + 'static`. Performs the handshake: reads the first message, requires `VendorEvent::Registered`, and returns the link. Errors on timeout (10s), a non-`Registered` first message, or a closed stream.
  - `VendorLink::vendor_name(&self) -> &str`
  - `VendorLink::announced_capabilities(&self) -> VendorCapabilities` (the `horsie_models::vendor::VendorCapabilities` wire type)
  - `VendorLink::request(&self, command: VendorCommand) -> Result<VendorEvent, String>` — sends with a fresh `request_id`, awaits the event carrying that same id. Returns `Err` on `VendorCommandFailed`, on a dropped link, or after a 15-minute ceiling (matching `PROVISION_WINDOW` in `executor/src/executor.rs`, since a create with clone steps legitimately takes minutes).
  - `VendorLink::send_oneway(&self, command: VendorCommand) -> Result<(), String>` — fire-and-forget for `CancelToolCall`, which has no reply.
  - `VendorLink::is_connected(&self) -> bool`

**Design notes for the implementer:**
Unsolicited events (`RuntimeStateChanged`, and the initial `Registered`) carry a `request_id` no waiter is registered for. The read loop must drop them without error rather than treating them as a protocol violation. Log `RuntimeStateChanged` at debug for now; PR 4 routes it to the supervisor.

- [ ] **Step 1: Write the failing test**

Create `server/src/vendor/link.rs` with only the test module first:

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use horsie_models::vendor::{
        VendorCapabilities, VendorCommand, VendorEvent, VendorInboundMessage,
        VendorOutboundMessage, VendorRegistered, VendorRuntimesListed, VendorStopRuntime,
    };
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::Role;

    /// A duplex-backed pair: (server-side link input, agent-side raw stream).
    /// No TCP, no in-memory transport type — a real WebSocket codec over an
    /// in-process duplex pipe.
    async fn ws_pair() -> (
        WebSocketStream<tokio::io::DuplexStream>,
        WebSocketStream<tokio::io::DuplexStream>,
    ) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let server = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let agent = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        (server, agent)
    }

    async fn send_event(
        sink: &mut WebSocketStream<tokio::io::DuplexStream>,
        request_id: &str,
        event: VendorEvent,
    ) {
        let msg = VendorOutboundMessage {
            request_id: request_id.to_string(),
            event,
        };
        sink.send(Message::Text(serde_json::to_string(&msg).unwrap().into()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn start_requires_a_registered_handshake_and_exposes_the_name() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(
                &mut agent_ws,
                "boot",
                VendorEvent::Registered(VendorRegistered {
                    vendor_name: "my-laptop".to_string(),
                    capabilities: VendorCapabilities {
                        supports_provisioning: false,
                    },
                }),
            )
            .await;
            // Hold the stream open so the link stays connected.
            std::future::pending::<()>().await;
        });

        let link = VendorLink::start(server_ws).await.expect("handshake");
        assert_eq!(link.vendor_name(), "my-laptop");
        assert!(!link.announced_capabilities().supports_provisioning);
        assert!(link.is_connected());
    }

    #[tokio::test]
    async fn request_correlates_the_reply_by_request_id() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(
                &mut agent_ws,
                "boot",
                VendorEvent::Registered(VendorRegistered {
                    vendor_name: "v".to_string(),
                    capabilities: VendorCapabilities {
                        supports_provisioning: true,
                    },
                }),
            )
            .await;
            // Answer whatever arrives, echoing its request_id.
            while let Some(Ok(Message::Text(text))) = agent_ws.next().await {
                let inbound: VendorInboundMessage = serde_json::from_str(&text).unwrap();
                // An unsolicited event first, to prove it is ignored, not mistaken
                // for the reply.
                send_event(
                    &mut agent_ws,
                    "unsolicited",
                    VendorEvent::RuntimesListed(VendorRuntimesListed { runtimes: vec![] }),
                )
                .await;
                send_event(
                    &mut agent_ws,
                    &inbound.request_id,
                    VendorEvent::RuntimesListed(VendorRuntimesListed { runtimes: vec![] }),
                )
                .await;
            }
        });

        let link = VendorLink::start(server_ws).await.expect("handshake");
        let event = link
            .request(VendorCommand::StopRuntime(VendorStopRuntime {
                runtime_id: "rt-1".to_string(),
            }))
            .await
            .expect("reply");
        assert!(matches!(event, VendorEvent::RuntimesListed(_)));
    }

    #[tokio::test]
    async fn request_fails_when_the_agent_reports_command_failed() {
        use horsie_models::vendor::VendorCommandFailed;
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(
                &mut agent_ws,
                "boot",
                VendorEvent::Registered(VendorRegistered {
                    vendor_name: "v".to_string(),
                    capabilities: VendorCapabilities {
                        supports_provisioning: true,
                    },
                }),
            )
            .await;
            while let Some(Ok(Message::Text(text))) = agent_ws.next().await {
                let inbound: VendorInboundMessage = serde_json::from_str(&text).unwrap();
                send_event(
                    &mut agent_ws,
                    &inbound.request_id,
                    VendorEvent::CommandFailed(VendorCommandFailed {
                        message: "no such workspace 'ghost'".to_string(),
                    }),
                )
                .await;
            }
        });

        let link = VendorLink::start(server_ws).await.expect("handshake");
        let err = link
            .request(VendorCommand::StopRuntime(VendorStopRuntime {
                runtime_id: "rt-1".to_string(),
            }))
            .await
            .expect_err("must surface the failure");
        assert!(err.contains("ghost"), "{err}");
    }

    #[tokio::test]
    async fn dropped_link_marks_disconnected_and_fails_pending_requests() {
        let (server_ws, mut agent_ws) = ws_pair().await;
        tokio::spawn(async move {
            send_event(
                &mut agent_ws,
                "boot",
                VendorEvent::Registered(VendorRegistered {
                    vendor_name: "v".to_string(),
                    capabilities: VendorCapabilities {
                        supports_provisioning: true,
                    },
                }),
            )
            .await;
            // Read the command, then hang up without replying.
            let _ = agent_ws.next().await;
            drop(agent_ws);
        });

        let link = VendorLink::start(server_ws).await.expect("handshake");
        let err = link
            .request(VendorCommand::StopRuntime(VendorStopRuntime {
                runtime_id: "rt-1".to_string(),
            }))
            .await
            .expect_err("a hung-up agent must fail the request, not hang");
        assert!(err.to_lowercase().contains("disconnect"), "{err}");
        for _ in 0..50 {
            if !link.is_connected() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("link never observed the disconnect");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-server vendor::link -- --nocapture`
Expected: FAIL to compile — `cannot find type 'VendorLink' in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `server/src/vendor/link.rs`, above the test module:

```rust
//! One connected vendor agent's WebSocket, with request/reply correlation.
//!
//! The agent owns runtime lifecycle; this link is the server's only handle on
//! it. Every command carries a fresh `request_id`; the read loop matches each
//! inbound event back to the waiter that issued it, and drops unsolicited
//! events (state changes, the boot `Registered`) rather than treating an
//! unmatched id as a protocol error.

use futures_util::{SinkExt, StreamExt};
use horsie_models::vendor::{
    VendorCapabilities, VendorCommand, VendorEvent, VendorInboundMessage, VendorOutboundMessage,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// How long the agent has to announce itself after connecting.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Ceiling on a single command. A create with `git_checkout` provision steps
/// legitimately runs for minutes, so this matches the executor's existing
/// `PROVISION_WINDOW` rather than a typical request timeout.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

type Waiters = Arc<Mutex<HashMap<String, oneshot::Sender<VendorEvent>>>>;

/// A sink that erases the socket type, so `VendorLink` is not generic over the
/// transport once constructed.
type BoxedSink = Box<dyn futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Send + Unpin>;

pub struct VendorLink {
    vendor_name: String,
    capabilities: VendorCapabilities,
    sink: Mutex<BoxedSink>,
    waiters: Waiters,
    connected: Arc<AtomicBool>,
}

impl VendorLink {
    /// Handshake on an accepted agent connection and start its read loop.
    ///
    /// The first message must be `VendorEvent::Registered`; anything else (or
    /// silence past `HANDSHAKE_TIMEOUT`) drops the connection.
    pub async fn start<S>(ws: WebSocketStream<S>) -> Result<Arc<Self>, String>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sink, mut stream) = ws.split();

        let announced = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            loop {
                match stream.next().await {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<VendorOutboundMessage>(&text) {
                            Ok(msg) => match msg.event {
                                VendorEvent::Registered(ev) => return Some(ev),
                                VendorEvent::RuntimeStateChanged(_)
                                | VendorEvent::RuntimesListed(_)
                                | VendorEvent::CommandFailed(_)
                                | VendorEvent::ToolResult(_)
                                | VendorEvent::ScanResult(_)
                                | VendorEvent::SessionStartResult(_) => return None,
                            },
                            Err(_) => return None,
                        }
                    }
                    Some(Ok(Message::Binary(_)))
                    | Some(Ok(Message::Ping(_)))
                    | Some(Ok(Message::Pong(_)))
                    | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                }
            }
        })
        .await;

        let announced = match announced {
            Ok(Some(ev)) => ev,
            Ok(None) => return Err("agent did not announce itself".to_string()),
            Err(_) => return Err("timed out waiting for the agent handshake".to_string()),
        };

        let waiters: Waiters = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let link = Arc::new(Self {
            vendor_name: announced.vendor_name,
            capabilities: announced.capabilities,
            sink: Mutex::new(Box::new(sink)),
            waiters: waiters.clone(),
            connected: connected.clone(),
        });

        tokio::spawn(async move {
            while let Some(next) = stream.next().await {
                let text = match next {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Binary(_))
                    | Ok(Message::Ping(_))
                    | Ok(Message::Pong(_))
                    | Ok(Message::Frame(_)) => continue,
                    Ok(Message::Close(_)) | Err(_) => break,
                };
                let Ok(msg) = serde_json::from_str::<VendorOutboundMessage>(&text) else {
                    tracing::warn!("vendor link: undecodable frame, ignoring");
                    continue;
                };
                if let VendorEvent::RuntimeStateChanged(ev) = &msg.event {
                    tracing::debug!(
                        runtime = %ev.runtime_id,
                        state = ?ev.state,
                        "vendor link: runtime state changed"
                    );
                }
                // An unmatched id is an unsolicited event, not an error.
                if let Some(tx) = waiters.lock().await.remove(&msg.request_id) {
                    let _ = tx.send(msg.event);
                }
            }
            connected.store(false, Ordering::Relaxed);
            // Fail every in-flight request so no caller waits on a dead socket.
            waiters.lock().await.clear();
        });

        Ok(link)
    }

    #[must_use]
    pub fn vendor_name(&self) -> &str {
        &self.vendor_name
    }

    #[must_use]
    pub fn announced_capabilities(&self) -> VendorCapabilities {
        self.capabilities.clone()
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn write(&self, msg: &VendorInboundMessage) -> Result<(), String> {
        if !self.is_connected() {
            return Err("vendor agent disconnected".to_string());
        }
        let json = serde_json::to_string(msg).map_err(|e| format!("encode command: {e}"))?;
        self.sink
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| format!("send to vendor agent: {e}"))
    }

    /// Send a command and await the event carrying the same `request_id`.
    pub async fn request(&self, command: VendorCommand) -> Result<VendorEvent, String> {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(request_id.clone(), tx);

        let msg = VendorInboundMessage {
            request_id: request_id.clone(),
            command,
        };
        if let Err(e) = self.write(&msg).await {
            self.waiters.lock().await.remove(&request_id);
            return Err(e);
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(VendorEvent::CommandFailed(ev))) => Err(ev.message),
            Ok(Ok(event)) => Ok(event),
            // The sender was dropped: the read loop exited, i.e. the socket died.
            Ok(Err(_)) => Err("vendor agent disconnected".to_string()),
            Err(_) => {
                self.waiters.lock().await.remove(&request_id);
                Err("timed out waiting for the vendor agent".to_string())
            }
        }
    }

    /// Send a command that has no reply (`CancelToolCall`).
    pub async fn send_oneway(&self, command: VendorCommand) -> Result<(), String> {
        self.write(&VendorInboundMessage {
            request_id: Uuid::new_v4().to_string(),
            command,
        })
        .await
    }
}
```

- [ ] **Step 4: Wire the module in `server/src/vendor/mod.rs`**

Add next to the existing `mod local;` / `mod velos;` declarations:

```rust
mod link;
pub use link::VendorLink;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie-server vendor::link -- --nocapture`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
git add server/src/vendor/link.rs server/src/vendor/mod.rs
git commit -m "feat: add VendorLink with request correlation"
```

---

### Task 3: `VendorRuntimeTransport` — `RuntimeTransport` over a link

**Files:**
- Create: `server/src/vendor/transport.rs`
- Modify: `server/src/vendor/mod.rs`
- Test: `server/src/vendor/transport.rs` (inline test module)

**Interfaces:**
- Consumes: `VendorLink::{request, send_oneway}` from Task 2.
- Produces: `VendorRuntimeTransport::new(link: Arc<VendorLink>, runtime_id: String) -> Self`, implementing `horsie_runtime_client::RuntimeTransport`. Constructing a `RuntimeClient` from it is `RuntimeClient::from_arc(Arc::new(transport))`.

**Design notes:**
`RuntimeTransport::invoke` must return `TransportError::Disconnected` (not `SendFailed`) when the link is down — `RuntimeClient` latches on exactly that variant to mark itself unusable, which is what makes `ensure_runtime` re-acquire.

- [ ] **Step 1: Write the failing test**

Create `server/src/vendor/transport.rs` with the test module. Reuse the `ws_pair` helper shape from Task 2 (repeated here deliberately — do not factor it out yet, Task 5's `FakeVendorAgent` supersedes it):

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use horsie_models::runtime::{BashInput, ToolCall, ToolError, ToolOutput};
    use horsie_models::vendor::{
        VendorCapabilities, VendorEvent, VendorInboundMessage, VendorOutboundMessage,
        VendorRegistered, VendorToolResult,
    };
    use horsie_runtime_client::RuntimeTransport;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::Role;

    async fn linked_to_echo_agent(stdout: &'static str) -> Arc<crate::vendor::VendorLink> {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let server = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let mut agent = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        tokio::spawn(async move {
            let boot = VendorOutboundMessage {
                request_id: "boot".to_string(),
                event: VendorEvent::Registered(VendorRegistered {
                    vendor_name: "v".to_string(),
                    capabilities: VendorCapabilities {
                        supports_provisioning: true,
                    },
                }),
            };
            agent
                .send(Message::Text(serde_json::to_string(&boot).unwrap().into()))
                .await
                .unwrap();
            while let Some(Ok(Message::Text(text))) = agent.next().await {
                let inbound: VendorInboundMessage = serde_json::from_str(&text).unwrap();
                let call_id = match &inbound.command {
                    horsie_models::vendor::VendorCommand::ToolCall(c) => c.call.call_id.clone(),
                    other => panic!("unexpected command {other:?}"),
                };
                let reply = VendorOutboundMessage {
                    request_id: inbound.request_id,
                    event: VendorEvent::ToolResult(VendorToolResult {
                        runtime_id: "rt-1".to_string(),
                        call_id,
                        result: horsie_models::runtime::ToolResult::Ok(ToolOutput {
                            stdout: stdout.to_string(),
                            stderr: String::new(),
                            exit_code: 0,
                        }),
                    }),
                };
                agent
                    .send(Message::Text(serde_json::to_string(&reply).unwrap().into()))
                    .await
                    .unwrap();
            }
        });
        crate::vendor::VendorLink::start(server).await.unwrap()
    }

    #[tokio::test]
    async fn invoke_round_trips_a_tool_call_through_the_link() {
        let link = linked_to_echo_agent("hello-from-agent").await;
        let transport = VendorRuntimeTransport::new(link, "rt-1".to_string());
        let result = transport
            .invoke(
                "call-1",
                ToolCall::Bash(BashInput {
                    command: "echo hi".to_string(),
                    timeout_secs: None,
                    workspace: None,
                }),
            )
            .await
            .expect("tool call");
        match result {
            horsie_models::runtime::ToolResult::Ok(out) => {
                assert_eq!(out.stdout, "hello-from-agent");
            }
            horsie_models::runtime::ToolResult::Err(ToolError { reason }) => {
                panic!("expected success, got {reason}")
            }
        }
    }

    #[tokio::test]
    async fn invoke_reports_disconnected_so_runtime_client_latches() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let server = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let mut agent = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        tokio::spawn(async move {
            let boot = VendorOutboundMessage {
                request_id: "boot".to_string(),
                event: VendorEvent::Registered(VendorRegistered {
                    vendor_name: "v".to_string(),
                    capabilities: VendorCapabilities {
                        supports_provisioning: true,
                    },
                }),
            };
            agent
                .send(Message::Text(serde_json::to_string(&boot).unwrap().into()))
                .await
                .unwrap();
            let _ = agent.next().await;
            drop(agent);
        });
        let link = crate::vendor::VendorLink::start(server).await.unwrap();
        let transport = VendorRuntimeTransport::new(link, "rt-1".to_string());
        let err = transport
            .invoke(
                "call-1",
                ToolCall::Bash(BashInput {
                    command: "x".to_string(),
                    timeout_secs: None,
                    workspace: None,
                }),
            )
            .await
            .expect_err("a dead link must fail the call");
        assert!(
            matches!(err, horsie_runtime_client::TransportError::Disconnected),
            "RuntimeClient only latches on Disconnected, got {err:?}"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-server vendor::transport -- --nocapture`
Expected: FAIL to compile — `cannot find type 'VendorRuntimeTransport'`.

- [ ] **Step 3: Write the implementation**

Prepend to `server/src/vendor/transport.rs`:

```rust
//! `RuntimeTransport` implemented over a `VendorLink`.
//!
//! This is the seam that leaves the session layer untouched: `RuntimeClient`
//! wraps any `RuntimeTransport`, so routing a session's tool calls through a
//! vendor agent is a matter of stamping a `runtime_id` onto each command.

use crate::vendor::VendorLink;
use async_trait::async_trait;
use horsie_models::runtime::{
    PluginSkill, ScanRequest, SessionStartRequest, ToolCall, ToolCallRequest, ToolResult,
    WorkspaceScan,
};
use horsie_models::vendor::{
    VendorCancelToolCall, VendorCommand, VendorEvent, VendorScanWorkspace, VendorSessionStart,
    VendorToolCall,
};
use horsie_runtime_client::{RuntimeTransport, TransportError};
use std::sync::Arc;

pub struct VendorRuntimeTransport {
    link: Arc<VendorLink>,
    runtime_id: String,
}

impl VendorRuntimeTransport {
    #[must_use]
    pub fn new(link: Arc<VendorLink>, runtime_id: String) -> Self {
        Self { link, runtime_id }
    }

    /// Map a link error onto the transport's vocabulary. `Disconnected` is
    /// load-bearing: `RuntimeClient` latches on exactly that variant, and a
    /// mislabelled error would leave a session reusing a dead client.
    fn transport_error(&self, message: String) -> TransportError {
        if !self.link.is_connected() || message.contains("disconnect") {
            TransportError::Disconnected
        } else {
            TransportError::SendFailed(message)
        }
    }

    async fn request(&self, command: VendorCommand) -> Result<VendorEvent, TransportError> {
        self.link
            .request(command)
            .await
            .map_err(|e| self.transport_error(e))
    }
}

#[async_trait]
impl RuntimeTransport for VendorRuntimeTransport {
    async fn invoke(&self, call_id: &str, call: ToolCall) -> Result<ToolResult, TransportError> {
        let event = self
            .request(VendorCommand::ToolCall(VendorToolCall {
                runtime_id: self.runtime_id.clone(),
                call: ToolCallRequest {
                    call_id: call_id.to_string(),
                    call,
                },
            }))
            .await?;
        match event {
            VendorEvent::ToolResult(ev) => Ok(ev.result),
            VendorEvent::Registered(_)
            | VendorEvent::RuntimeStateChanged(_)
            | VendorEvent::RuntimesListed(_)
            | VendorEvent::CommandFailed(_)
            | VendorEvent::ScanResult(_)
            | VendorEvent::SessionStartResult(_) => Err(TransportError::SendFailed(
                "vendor agent answered a tool call with the wrong event".to_string(),
            )),
        }
    }

    async fn cancel(&self, call_id: &str) -> Result<(), TransportError> {
        self.link
            .send_oneway(VendorCommand::CancelToolCall(VendorCancelToolCall {
                runtime_id: self.runtime_id.clone(),
                call_id: call_id.to_string(),
            }))
            .await
            .map_err(|e| self.transport_error(e))
    }

    async fn scan_workspace(
        &self,
        call_id: &str,
        workspace: Option<String>,
        instruction_candidates: Vec<String>,
        skills_glob: String,
        include_shared: bool,
    ) -> Result<(Vec<WorkspaceScan>, Vec<PluginSkill>), TransportError> {
        let event = self
            .request(VendorCommand::ScanWorkspace(VendorScanWorkspace {
                runtime_id: self.runtime_id.clone(),
                request: ScanRequest {
                    call_id: call_id.to_string(),
                    workspace,
                    instruction_candidates,
                    skills_glob,
                    include_shared,
                },
            }))
            .await?;
        match event {
            VendorEvent::ScanResult(ev) => Ok((ev.response.workspaces, ev.response.shared_skills)),
            VendorEvent::Registered(_)
            | VendorEvent::RuntimeStateChanged(_)
            | VendorEvent::RuntimesListed(_)
            | VendorEvent::CommandFailed(_)
            | VendorEvent::ToolResult(_)
            | VendorEvent::SessionStartResult(_) => Err(TransportError::SendFailed(
                "vendor agent answered a scan with the wrong event".to_string(),
            )),
        }
    }

    async fn run_session_start(&self, call_id: &str) -> Result<String, TransportError> {
        let event = self
            .request(VendorCommand::SessionStart(VendorSessionStart {
                runtime_id: self.runtime_id.clone(),
                request: SessionStartRequest {
                    call_id: call_id.to_string(),
                },
            }))
            .await?;
        match event {
            VendorEvent::SessionStartResult(ev) => Ok(ev.response.context),
            VendorEvent::Registered(_)
            | VendorEvent::RuntimeStateChanged(_)
            | VendorEvent::RuntimesListed(_)
            | VendorEvent::CommandFailed(_)
            | VendorEvent::ToolResult(_)
            | VendorEvent::ScanResult(_) => Err(TransportError::SendFailed(
                "vendor agent answered a session start with the wrong event".to_string(),
            )),
        }
    }
}
```

- [ ] **Step 4: Wire the module in `server/src/vendor/mod.rs`**

```rust
mod transport;
pub use transport::VendorRuntimeTransport;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie-server vendor::transport -- --nocapture`
Expected: 2 passed.

- [ ] **Step 6: Commit**

```bash
git add server/src/vendor/transport.rs server/src/vendor/mod.rs
git commit -m "feat: route tool calls through a vendor agent link"
```

---

### Task 4: `RuntimeVendor` for `VendorLink`

**Files:**
- Modify: `server/src/vendor/link.rs` (append the trait impl and its tests)

**Interfaces:**
- Consumes: `RuntimeSpec`, `VendorRuntime`, `VendorRuntimeHandle`, `VendorError`, `VendorCapabilities` (the *server-side* struct) from `server/src/vendor/mod.rs`; `VendorRuntimeTransport` from Task 3.
- Produces: `impl RuntimeVendor for VendorLink`, so `Arc<VendorLink>` coerces to `Arc<dyn RuntimeVendor>` and drops into `ServerDeps.vendors` unchanged.

**Design notes:**
Two types are both named `VendorCapabilities` — the wire one in `horsie_models::vendor` and the server-side one in `crate::vendor`. Import one and fully qualify the other; do not alias both.

`RuntimeSpec.capabilities_file` is a server-local path. Read it with `CapabilitySpec::load` and inline the value; a read failure is a create failure, not a silent `None`.

- [ ] **Step 1: Write the failing test**

Append inside the existing `mod tests` in `server/src/vendor/link.rs`:

```rust
#[tokio::test]
async fn create_stop_delete_emit_three_distinct_signals() {
    use crate::vendor::{RuntimeVendor, RuntimeSpec, WorkspaceSpec};
    use horsie_models::vendor::{VendorCommand, VendorRuntimeStateChanged};
    use std::sync::Mutex as StdMutex;

    let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let recorder = seen.clone();

    let (a, b) = tokio::io::duplex(64 * 1024);
    let server_ws = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
    let mut agent_ws = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
    tokio::spawn(async move {
        send_event(
            &mut agent_ws,
            "boot",
            VendorEvent::Registered(VendorRegistered {
                vendor_name: "v".to_string(),
                capabilities: VendorCapabilities {
                    supports_provisioning: true,
                },
            }),
        )
        .await;
        while let Some(Ok(Message::Text(text))) = agent_ws.next().await {
            let inbound: VendorInboundMessage = serde_json::from_str(&text).unwrap();
            let label = match &inbound.command {
                VendorCommand::CreateRuntime(_) => "create",
                VendorCommand::AttachRuntime(_) => "attach",
                VendorCommand::StopRuntime(_) => "stop",
                VendorCommand::DeleteRuntime(_) => "delete",
                VendorCommand::QueryRuntimes(_) => "query",
                VendorCommand::ToolCall(_) => "tool",
                VendorCommand::CancelToolCall(_) => "cancel",
                VendorCommand::ScanWorkspace(_) => "scan",
                VendorCommand::SessionStart(_) => "session-start",
            };
            recorder.lock().unwrap().push(label.to_string());
            send_event(
                &mut agent_ws,
                &inbound.request_id,
                VendorEvent::RuntimeStateChanged(VendorRuntimeStateChanged {
                    runtime_id: "rt-1".to_string(),
                    state: horsie_models::executor::RuntimeState::Running,
                }),
            )
            .await;
        }
    });

    let link = VendorLink::start(server_ws).await.expect("handshake");
    let caps_path = std::env::temp_dir().join("horsie-vendor-link-test-caps.json");
    std::fs::write(
        &caps_path,
        serde_json::to_vec(&horsie_models::capabilities::CapabilitySpec::new(
            horsie_models::capabilities::NetworkPolicy::Block(
                horsie_models::capabilities::BlockNetwork {},
            ),
            vec![],
        ))
        .unwrap(),
    )
    .unwrap();
    let spec = RuntimeSpec {
        workspaces: vec![WorkspaceSpec {
            name: "main".to_string(),
        }],
        provision: vec![],
        env: vec![],
        capabilities_file: caps_path,
    };

    let rt = link.create("rt-1", &spec).await.expect("create");
    rt.handle.stop().await;
    link.delete("rt-1").await;

    // Give the one-way delete a moment to land.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &["create".to_string(), "stop".to_string(), "delete".to_string()],
        "every lifecycle action must be its own explicit signal"
    );
}

#[tokio::test]
async fn capabilities_come_from_the_agents_announcement() {
    use crate::vendor::RuntimeVendor;
    let (a, b) = tokio::io::duplex(64 * 1024);
    let server_ws = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
    let mut agent_ws = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
    tokio::spawn(async move {
        send_event(
            &mut agent_ws,
            "boot",
            VendorEvent::Registered(VendorRegistered {
                vendor_name: "fixed-dir".to_string(),
                capabilities: VendorCapabilities {
                    supports_provisioning: false,
                },
            }),
        )
        .await;
        std::future::pending::<()>().await;
    });
    let link = VendorLink::start(server_ws).await.expect("handshake");
    assert!(
        !RuntimeVendor::capabilities(link.as_ref()).supports_provisioning,
        "the server must not second-guess what the agent announced"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-server vendor::link -- --nocapture`
Expected: FAIL to compile — `the trait bound 'VendorLink: RuntimeVendor' is not satisfied`.

- [ ] **Step 3: Write the implementation**

Append to `server/src/vendor/link.rs`, above the test module:

```rust
use crate::vendor::{
    RuntimeSpec, RuntimeVendor, VendorError, VendorRuntime, VendorRuntimeHandle,
    VendorRuntimeTransport,
};
use horsie_models::vendor::{
    VendorAttachRuntime, VendorCreateRuntime, VendorDeleteRuntime, VendorRuntimeRequest,
    VendorStopRuntime,
};
use horsie_runtime_client::RuntimeClient;

impl VendorLink {
    /// Translate the server-side spec into the wire request. The capability
    /// file is read here and inlined: a server-local path means nothing to a
    /// remote agent.
    fn runtime_request(spec: &RuntimeSpec) -> Result<VendorRuntimeRequest, String> {
        let sandbox_capabilities =
            horsie_models::capabilities::CapabilitySpec::load(&spec.capabilities_file)?;
        Ok(VendorRuntimeRequest {
            workspaces: spec.workspaces.iter().map(|w| w.name.clone()).collect(),
            env: spec.env.clone(),
            provision: spec.provision.clone(),
            sandbox_capabilities: Some(sandbox_capabilities),
        })
    }

    async fn provision(
        self: &Arc<Self>,
        runtime_id: &str,
        spec: &RuntimeSpec,
        attach: bool,
    ) -> Result<VendorRuntime, VendorError> {
        let wrap = |e: String| {
            if attach {
                VendorError::Attach(e)
            } else {
                VendorError::Provision(e)
            }
        };
        let request = Self::runtime_request(spec).map_err(wrap)?;
        let command = if attach {
            VendorCommand::AttachRuntime(VendorAttachRuntime {
                runtime_id: runtime_id.to_string(),
                request,
            })
        } else {
            VendorCommand::CreateRuntime(VendorCreateRuntime {
                runtime_id: runtime_id.to_string(),
                request,
            })
        };
        // A non-CommandFailed reply means the agent accepted it; `request`
        // already turns CommandFailed into Err.
        self.request(command).await.map_err(wrap)?;

        let transport = VendorRuntimeTransport::new(self.clone(), runtime_id.to_string());
        Ok(VendorRuntime {
            runtime_client: RuntimeClient::from_arc(Arc::new(transport)),
            handle: Arc::new(LinkRuntimeHandle {
                link: self.clone(),
                runtime_id: runtime_id.to_string(),
            }),
        })
    }
}

#[async_trait::async_trait]
impl RuntimeVendor for VendorLink {
    fn capabilities(&self) -> crate::vendor::VendorCapabilities {
        crate::vendor::VendorCapabilities {
            supports_provisioning: self.capabilities.supports_provisioning,
        }
    }

    async fn create(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError> {
        // `provision` needs an owned Arc to hand to the transport and handle.
        // Callers always hold the link as Arc<VendorLink>, so this is sound.
        let me = self.arc_self();
        me.provision(runtime_id, spec, false).await
    }

    async fn attach(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError> {
        let me = self.arc_self();
        me.provision(runtime_id, spec, true).await
    }

    async fn delete(&self, runtime_id: &str) {
        let _ = self
            .request(VendorCommand::DeleteRuntime(VendorDeleteRuntime {
                runtime_id: runtime_id.to_string(),
            }))
            .await;
    }
}

/// Lifecycle handle for one runtime on one agent. `stop` is the explicit
/// stop-preserve signal; the agent decides what preservation means.
struct LinkRuntimeHandle {
    link: Arc<VendorLink>,
    runtime_id: String,
}

#[async_trait::async_trait]
impl VendorRuntimeHandle for LinkRuntimeHandle {
    async fn stop(&self) {
        let _ = self
            .link
            .request(VendorCommand::StopRuntime(VendorStopRuntime {
                runtime_id: self.runtime_id.clone(),
            }))
            .await;
    }
}
```

`arc_self()` requires the link to hold a weak self-reference. Add to the struct and set it in `start`:

```rust
// field on VendorLink
    this: std::sync::Weak<VendorLink>,
```

Build the `Arc` with `Arc::new_cyclic` in `start` instead of `Arc::new`:

```rust
        let link = Arc::new_cyclic(|this| Self {
            vendor_name: announced.vendor_name,
            capabilities: announced.capabilities,
            sink: Mutex::new(Box::new(sink)),
            waiters: waiters.clone(),
            connected: connected.clone(),
            this: this.clone(),
        });
```

And the accessor:

```rust
    /// The `Arc` this link lives in. Held weakly so the link does not keep
    /// itself alive — dropping the last external `Arc` still tears it down.
    fn arc_self(&self) -> Arc<Self> {
        self.this
            .upgrade()
            .unwrap_or_else(|| unreachable!("a live &self implies a live Arc"))
    }
```

`unreachable!` is a `panic` under the workspace lints. Use this instead, returning the error through the caller:

```rust
    fn arc_self(&self) -> Option<Arc<Self>> {
        self.this.upgrade()
    }
```

and in `create`/`attach`:

```rust
        let Some(me) = self.arc_self() else {
            return Err(VendorError::Provision(
                "vendor link was dropped".to_string(),
            ));
        };
```

(in `attach`, use `VendorError::Attach`).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-server vendor::link -- --nocapture`
Expected: 6 passed.

- [ ] **Step 5: Commit**

```bash
git add server/src/vendor/link.rs
git commit -m "feat: expose a vendor agent link as a RuntimeVendor"
```

---

### Task 5: `FakeVendorAgent` test harness

**Files:**
- Create: `server/src/vendor/fake_agent.rs`
- Modify: `server/src/vendor/mod.rs`, `server/Cargo.toml` (the `test-util` feature must already list the module — verify)
- Test: `server/src/vendor/fake_agent.rs` (self-test)

**Interfaces:**
- Produces:
  - `FakeVendorAgent::builder(vendor_name: &str) -> FakeVendorAgentBuilder`
  - `FakeVendorAgentBuilder::supports_provisioning(bool) -> Self`
  - `FakeVendorAgentBuilder::bash_stdout(&str) -> Self` — canned stdout for every `ToolCall`
  - `FakeVendorAgentBuilder::connect(self, url: &str) -> Result<FakeVendorAgent, String>` — dials a real server over TCP
  - `FakeVendorAgent::signals(&self) -> Vec<String>` — ordered lifecycle labels (`create` / `attach` / `stop` / `delete` / `query`)
  - `FakeVendorAgent::live_runtimes(&self) -> Vec<String>`
  - `FakeVendorAgent::disconnect(&self)` — drop the socket, to exercise recovery

Mirror the gating on `server/src/vendor/mock.rs`:

```rust
#[cfg(any(test, feature = "test-util"))]
pub mod fake_agent;
```

**Design notes:**
The fake must answer `ScanWorkspace` (and `SessionStart` when the session's `use_plugins` resolves true) or session provisioning hangs forever with no error output — `session_actor.rs` calls `scan_workspace()` unconditionally at session creation. This has bitten this codebase before. Answer both by default.

- [ ] **Step 1: Write the failing self-test**

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_agent_answers_scan_so_session_provisioning_cannot_hang() {
        let agent = FakeVendorAgent::builder("test-agent")
            .bash_stdout("ok")
            .serve_in_process()
            .await
            .expect("agent");
        let link = agent.link();
        let transport =
            crate::vendor::VendorRuntimeTransport::new(link, "rt-1".to_string());
        let (workspaces, shared) = horsie_runtime_client::RuntimeTransport::scan_workspace(
            &transport,
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
    }

    #[tokio::test]
    async fn fake_agent_records_lifecycle_signals_in_order() {
        let agent = FakeVendorAgent::builder("test-agent")
            .serve_in_process()
            .await
            .expect("agent");
        let link = agent.link();
        let spec = crate::vendor::test_support::runtime_spec_fixture("main");
        let rt = crate::vendor::RuntimeVendor::create(link.as_ref(), "rt-1", &spec)
            .await
            .expect("create");
        rt.handle.stop().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(agent.signals(), vec!["create".to_string(), "stop".to_string()]);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-server vendor::fake_agent -- --nocapture`
Expected: FAIL to compile — `cannot find type 'FakeVendorAgent'`.

- [ ] **Step 3: Implement `FakeVendorAgent`**

Write the agent loop with two entry points: `serve_in_process()` (a `tokio::io::duplex` pair, returning both the link and the recorder — used by unit tests) and `connect(url)` (a real TCP dial at a running server's `/api/vendor/connect`, used by Task 7). Both share one `run_agent_loop` that:

1. Sends `VendorEvent::Registered { vendor_name, capabilities }`.
2. For every `VendorInboundMessage`, records a label into a shared `Arc<Mutex<Vec<String>>>` and replies on the same `request_id`:
   - `CreateRuntime` / `AttachRuntime` → record `create`/`attach`, insert into `live_runtimes`, reply `RuntimeStateChanged { state: Running }`.
   - `StopRuntime` → record `stop`, remove from `live_runtimes`, reply `RuntimeStateChanged { state: Stopped }`.
   - `DeleteRuntime` → record `delete`, remove, reply `RuntimeStateChanged { state: Stopped }`.
   - `QueryRuntimes` → record `query`, reply `RuntimesListed` from `live_runtimes`.
   - `ToolCall` → reply `ToolResult` with the canned stdout and `exit_code: 0`.
   - `CancelToolCall` → record `cancel`, send nothing (one-way).
   - `ScanWorkspace` → reply `ScanResult` with one `WorkspaceScan { name: "main", path: "/fake/main", is_git_repo: false, instructions: None, skills: vec![], platform: Some("linux-x86_64".into()) }` and no shared skills.
   - `SessionStart` → reply `SessionStartResult { context: String::new() }`.

Add `crate::vendor::test_support::runtime_spec_fixture(workspace: &str) -> RuntimeSpec` in the same gated module, writing a real capability file into `tempfile::tempdir()` so `CapabilitySpec::load` succeeds.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p horsie-server vendor::fake_agent -- --nocapture`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add server/src/vendor/fake_agent.rs server/src/vendor/mod.rs server/Cargo.toml
git commit -m "test: add a scriptable fake vendor agent"
```

---

### Task 6: `VendorAgentRegistry` and the `/api/vendor/connect` route

**Files:**
- Create: `server/src/vendor/agent_registry.rs`, `server/src/http/vendor_connect.rs`
- Modify: `server/src/vendor/mod.rs`, `server/src/http/mod.rs`, `server/src/bin/horsie-server/main.rs`
- Test: `server/src/http/mod.rs` (inline test module, alongside the existing route tests)

**Interfaces:**
- Consumes: `VendorLink::start` (Task 2), the `RuntimeVendor` impl (Task 4).
- Produces:
  - `VendorAgentRegistry::new(vendors: SharedVendors) -> Self`
  - `VendorAgentRegistry::register(&self, link: Arc<VendorLink>)` — inserts into the shared `vendors` map under `link.vendor_name()`, replacing any prior entry for that name (a reconnecting agent supersedes its own dead link).
  - `VendorAgentRegistry::connected_names(&self) -> Vec<String>`
  - `AppState.vendor_agents: Arc<VendorAgentRegistry>`
  - Route `GET /api/vendor/connect`

**Design notes:**
This mirrors `LocalDaemonRegistry` exactly — the same `SharedVendors` map, the same insert-on-connect shape. The difference is that a vendor agent supersedes its previous entry on reconnect (its runtimes are gone and the old link is dead), whereas `LocalDaemonRegistry` deliberately preserves vendor object identity.

Reuse the raw-upgrade mechanics from `server/src/http/runtime_connect.rs` verbatim — `SEC_WEBSOCKET_KEY`, `derive_accept_key`, `OnUpgrade`, `WebSocketStream::from_raw_socket(.., Role::Server, ..)`, and the 101 response. Do not modify that file.

- [ ] **Step 1: Write the failing test**

Append to the test module in `server/src/http/mod.rs`:

```rust
#[tokio::test]
async fn a_connected_agent_becomes_a_selectable_vendor() {
    let (state, _tmp) = test_state().await;
    let app = app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    assert!(
        !state.deps.vendors.read().unwrap().contains_key("my-laptop"),
        "no vendor before the agent dials in"
    );

    let _agent = crate::vendor::fake_agent::FakeVendorAgent::builder("my-laptop")
        .supports_provisioning(false)
        .connect(&format!("ws://{addr}/api/vendor/connect"))
        .await
        .expect("agent connects");

    for _ in 0..50 {
        if state.deps.vendors.read().unwrap().contains_key("my-laptop") {
            let vendors = state.deps.vendors.read().unwrap();
            let v = vendors.get("my-laptop").unwrap();
            assert!(!v.capabilities().supports_provisioning);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("agent never registered as a vendor");
}
```

Note: `test_state()` is the existing helper in that module. If it does not expose `deps`, add a `pub deps: ServerDeps` field read-through or use the existing accessor — check the surrounding tests before writing.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-server a_connected_agent_becomes -- --nocapture`
Expected: FAIL — 404 on the route, so the agent's connect errors.

- [ ] **Step 3: Implement `VendorAgentRegistry`**

```rust
//! Tracks every connected vendor agent and mirrors it into the shared vendor
//! map sessions select from.

use crate::sessions::spec::SharedVendors;
use crate::vendor::{RuntimeVendor, VendorLink};
use std::sync::Arc;

pub struct VendorAgentRegistry {
    vendors: SharedVendors,
}

impl VendorAgentRegistry {
    #[must_use]
    pub fn new(vendors: SharedVendors) -> Self {
        Self { vendors }
    }

    /// Publish a freshly handshaken link under its announced name. A
    /// reconnecting agent replaces its own previous entry: the old link is
    /// dead and its runtimes are gone, so keeping it would strand sessions on
    /// a socket that can never answer.
    pub fn register(&self, link: Arc<VendorLink>) {
        let name = link.vendor_name().to_string();
        let mut vendors = self
            .vendors
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        vendors.insert(name, link as Arc<dyn RuntimeVendor>);
    }

    #[must_use]
    pub fn connected_names(&self) -> Vec<String> {
        self.vendors
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
}
```

- [ ] **Step 4: Implement the route**

`server/src/http/vendor_connect.rs`, modelled on `runtime_connect.rs`:

```rust
//! `GET /api/vendor/connect` — the one endpoint runtime vendor agents dial.
//!
//! Distinct from `/api/runtime/connect`, which *runtimes* dial and which this
//! design retires in a later change. An agent that completes the handshake is
//! published as a selectable vendor under the name it announced.

use crate::http::AppState;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

pub async fn vendor_connect(
    State(state): State<AppState>,
    mut req: axum::extract::Request,
) -> Response {
    let Some(key) = req
        .headers()
        .get(header::SEC_WEBSOCKET_KEY)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return (StatusCode::BAD_REQUEST, "expected a websocket upgrade").into_response();
    };
    let Some(on_upgrade) = req.extensions_mut().remove::<OnUpgrade>() else {
        return (StatusCode::BAD_REQUEST, "connection is not upgradable").into_response();
    };
    let accept = derive_accept_key(key.as_bytes());
    let agents = state.vendor_agents.clone();

    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let ws =
                    WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None)
                        .await;
                match crate::vendor::VendorLink::start(ws).await {
                    Ok(link) => {
                        tracing::info!(vendor = %link.vendor_name(), "vendor agent connected");
                        agents.register(link);
                    }
                    Err(e) => tracing::warn!(error = %e, "vendor agent handshake failed"),
                }
            }
            Err(e) => tracing::warn!(error = %e, "vendor_connect: websocket upgrade failed"),
        }
    });

    match Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_ACCEPT, accept)
        .body(axum::body::Body::empty())
    {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(error = %e, "vendor_connect: failed to build 101 response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

- [ ] **Step 5: Wire the route, `AppState`, and startup**

In `server/src/http/mod.rs`: add `pub mod vendor_connect;`, the `vendor_agents: Arc<crate::vendor::VendorAgentRegistry>` field on `AppState` (documented like its neighbours), the route line next to the existing runtime-connect route:

```rust
        .route("/api/vendor/connect", get(vendor_connect::vendor_connect))
```

and construct the registry in the existing test `AppState` builder.

In `server/src/bin/horsie-server/main.rs`, alongside the `local_daemon_hook` block:

```rust
    // Vendor agents publish themselves into the same map sessions select from,
    // exactly as the local-daemon registry does for dial-in runtimes.
    let vendor_agents = Arc::new(horsie_server::vendor::VendorAgentRegistry::new(
        opened.vendors.clone(),
    ));
```

and add `vendor_agents` to the `AppState` literal.

- [ ] **Step 6: Run to verify it passes**

Run: `cargo test -p horsie-server a_connected_agent_becomes -- --nocapture`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add server/src/vendor/agent_registry.rs server/src/http/vendor_connect.rs \
        server/src/vendor/mod.rs server/src/http/mod.rs server/src/bin/horsie-server/main.rs
git commit -m "feat: publish connected vendor agents as selectable vendors"
```

---

### Task 7: Full-stack session turn over a vendor agent

**Files:**
- Modify: `tests/tests/session_server_e2e.rs`

**Interfaces:**
- Consumes: everything above. No new production code — this task proves the stack.

**Design notes:**
Follow the existing helpers in that file for booting a server and creating a session. Set the session's `vendor` to the fake agent's name. Send `"usePlugins": false` in camelCase — the existing helpers send `use_plugins`, which the wire protocol silently ignores, so `use_plugins` is effectively always true.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_session_runs_a_turn_against_a_connected_vendor_agent() {
    let server = start_test_server().await;
    let agent = horsie_server::vendor::fake_agent::FakeVendorAgent::builder("agent-1")
        .supports_provisioning(true)
        .bash_stdout("from-the-agent")
        .connect(&format!("ws://{}/api/vendor/connect", server.addr))
        .await
        .expect("agent connects");
    wait_for_vendor(&server, "agent-1").await;

    let session = create_session(&server, serde_json::json!({
        "vendor": "agent-1",
        "model": "mock",
        "usePlugins": false,
        "workspaces": [{ "name": "main" }],
    }))
    .await;

    send_message(&server, &session, "run a command").await;
    let outcome = wait_for_turn(&server, &session).await;
    assert!(outcome.is_ok(), "turn failed: {outcome:?}");

    assert!(
        agent.signals().contains(&"create".to_string()),
        "the agent must have been asked to create the runtime, got {:?}",
        agent.signals()
    );
}

#[tokio::test]
async fn stopping_one_session_leaves_another_on_the_same_agent_alive() {
    let server = start_test_server().await;
    let agent = horsie_server::vendor::fake_agent::FakeVendorAgent::builder("agent-2")
        .bash_stdout("ok")
        .connect(&format!("ws://{}/api/vendor/connect", server.addr))
        .await
        .expect("agent connects");
    wait_for_vendor(&server, "agent-2").await;

    let a = create_session(&server, serde_json::json!({
        "vendor": "agent-2", "model": "mock", "usePlugins": false,
        "workspaces": [{ "name": "main" }],
    }))
    .await;
    let b = create_session(&server, serde_json::json!({
        "vendor": "agent-2", "model": "mock", "usePlugins": false,
        "workspaces": [{ "name": "main" }],
    }))
    .await;
    send_message(&server, &a, "hello").await;
    let _ = wait_for_turn(&server, &a).await;
    send_message(&server, &b, "hello").await;
    let _ = wait_for_turn(&server, &b).await;

    stop_session(&server, &a).await;

    // b must still be able to run — the inverse of the old shared-daemon
    // behavior, where stop was a no-op precisely because it would have hit
    // every session at once.
    send_message(&server, &b, "again").await;
    let outcome = wait_for_turn(&server, &b).await;
    assert!(outcome.is_ok(), "session b must be unaffected: {outcome:?}");
    assert!(agent.live_runtimes().len() == 1, "only b's runtime remains");
}
```

Add the `wait_for_vendor` helper next to the file's other helpers:

```rust
async fn wait_for_vendor(server: &TestServer, name: &str) {
    for _ in 0..100 {
        let body: serde_json::Value = server.get_json("/api/config").await;
        if body["vendors"]
            .as_array()
            .is_some_and(|vs| vs.iter().any(|v| v["name"] == name))
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("vendor '{name}' never appeared");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-tests a_session_runs_a_turn_against -- --nocapture`
Expected: FAIL — the vendor is unknown, or the turn errors.

- [ ] **Step 3: Fix whatever the test exposes**

Do not change the test to match the code. Likely gaps: the config view not listing agent-registered vendors, or `ensure_runtime` failing on the capability file. Fix the production code.

- [ ] **Step 4: Run the full check**

Run: `make check`
Expected: fmt clean, clippy clean, all tests pass.

- [ ] **Step 5: Commit and open the PR**

```bash
git add tests/tests/session_server_e2e.rs
git commit -m "test: run a session turn against a connected vendor agent"
git push -u origin feat/vendor-agent-link
gh pr create --title "Vendor agent protocol and link" --body "..."
```

PR body: one paragraph on why (two unrelated runtime paths, the dead protocol promoted), a short list of what landed, and an explicit note that `/api/runtime/connect`, `LocalDaemonVendor`, and `VelosVendor` are untouched and still serving — they are retired in later PRs. No test-by-test narration, no CI status, one long line per paragraph (GitHub renders newlines as literal breaks).

---

## Self-Review

**Spec coverage for PR 1:** `vendor.fl` → Task 1. `VendorLink` → Tasks 2, 4. `VendorRuntimeTransport` → Task 3. `GET /api/vendor/connect` → Task 6. `FakeVendorAgent` → Task 5. "Nothing removed; the old route still serves" → Global Constraints, verified in Task 7's PR note. The five named test transitions from the spec: create/stop/attach distinct signals → Task 4; disconnect mid-turn → Task 3; reconnect + `QueryRuntimes` → deferred to PR 4 with the session-actor port, since it needs supervisor reconciliation that does not exist yet; two sessions, stop one → Task 7; unresolvable workspace name → Task 2 (`request_fails_when_the_agent_reports_command_failed`).

**Known gap, deliberate:** `RuntimeStateChanged` is logged, not acted on. Routing it to the supervisor needs the session-actor changes in PR 4. Recorded here so it is not mistaken for an oversight.

**Type consistency:** `VendorLink::start/request/send_oneway/vendor_name/announced_capabilities/is_connected` are used identically in Tasks 3, 4, 6. `VendorRuntimeTransport::new(Arc<VendorLink>, String)` matches its call sites in Tasks 4 and 5. The two `VendorCapabilities` types are called out explicitly in Task 4's design notes.
