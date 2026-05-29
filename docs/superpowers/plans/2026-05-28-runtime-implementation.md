# Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the runtime binary, executor extensions, runtime-client crate, and server ExecutorClient per the design spec at `docs/superpowers/specs/2026-05-28-runtime-design.md`.

**Architecture:** The runtime binary connects back to the executor via WebSocket, executes tool calls in parallel, and returns results. The server uses `RuntimeClient` (via `ExecutorWsTransport`) to invoke tools on a runtime through the executor as a relay. `ExecutorClient` provides a typed interface to the executor for tests.

**Tech Stack:** Rust, tokio, tokio-tungstenite, serde_json, clap, async-trait, thiserror, uuid, fluorite

---

## File Map

**New files:**
- `fluorite/runtime.fl` — executor↔runtime wire protocol
- `models/src/lib.rs` — add `pub mod runtime` block (modify)
- `executor/src/connected_registry.rs` — tracks live runtime WS connections
- `executor/src/runtime_listener.rs` — WS server accepting runtime connections
- `executor/src/process_provider.rs` — RuntimeProvider that spawns the binary
- `runtime/src/main.rs` — CLI entry point, WS connect, dispatch loop
- `runtime/src/tools/mod.rs` — dispatch ToolCall → ToolResult
- `runtime/src/tools/bash.rs`
- `runtime/src/tools/read_file.rs`
- `runtime/src/tools/write_file.rs`
- `runtime/src/tools/edit_file.rs`
- `runtime/src/tools/replace_in_file.rs`
- `runtime/src/tools/list_files.rs`
- `runtime/src/tools/glob.rs`
- `runtime/src/tools/grep.rs`
- `runtime-client/Cargo.toml` — new crate
- `runtime-client/src/lib.rs`
- `runtime-client/src/transport.rs` — RuntimeTransport trait
- `runtime-client/src/client.rs` — RuntimeClient
- `runtime-client/src/ws_transport.rs` — ExecutorWsTransport
- `runtime-client/src/tools/mod.rs`
- `runtime-client/src/tools/bash.rs` … (one per tool)
- `runtime-client/src/tools/builder.rs` — add_runtime_tools
- `server/src/executor_client.rs` — ExecutorClient + WsExecutorTransport

**Modified files:**
- `fluorite/executor.fl` — add working_dir, ToolCallCmd, CancelToolCallCmd, ToolResultEvent
- `executor/src/error.rs` — add BindFailed, SpawnFailed
- `executor/src/executor.rs` — add listener integration, ToolCall/CancelToolCall dispatch
- `executor/src/lib.rs` — re-export new types
- `executor/Cargo.toml` — add process feature to tokio, add tokio-tungstenite
- `agentcore/src/tool.rs` — add Tool trait + ToolboxImpl
- `agentcore/src/lib.rs` — re-export Tool, ToolboxImpl
- `runtime/src/lib.rs` — re-export tools module
- `runtime/Cargo.toml` — add binary target + deps
- `server/src/lib.rs` — re-export ExecutorClient
- `server/Cargo.toml` — add runtime-client dep
- `Cargo.toml` — add runtime-client to workspace members, add tempfile to workspace deps

---

### Task 1: Protocol — fluorite/runtime.fl + extend executor.fl + update models

**Files:**
- Create: `fluorite/runtime.fl`
- Modify: `fluorite/executor.fl`
- Modify: `models/src/lib.rs`

- [ ] **Step 1: Create `fluorite/runtime.fl`**

```
/// Protocol types for executor ↔ runtime communication
package runtime;

// --- Tool inputs ---

struct BashInput { command: String }
struct ReadFileInput { path: String, start_line: Option<u64>, end_line: Option<u64> }
struct WriteFileInput { path: String, content: String }
struct EditFileInput { path: String, old_text: String, new_text: String }

struct RegexMode { pattern: String }
struct LinesMode { start_line: u64, end_line: u64 }

#[type_tag = "kind"]
union ReplaceMode {
    Regex(RegexMode),
    Lines(LinesMode),
}

struct ReplaceInFileInput { path: String, replacement: String, mode: ReplaceMode }
struct ListFilesInput { path: String }
struct GlobInput { pattern: String, path: Option<String>, max_results: Option<u64> }
struct GrepInput { pattern: String, path: Option<String>, file_pattern: Option<String>, max_results: Option<u64> }

/// One variant per tool. The tag doubles as the tool name seen by the LLM.
#[type_tag = "tool"]
union ToolCall {
    Bash(BashInput),
    ReadFile(ReadFileInput),
    WriteFile(WriteFileInput),
    EditFile(EditFileInput),
    ReplaceInFile(ReplaceInFileInput),
    ListFiles(ListFilesInput),
    Glob(GlobInput),
    Grep(GrepInput),
}

// --- Inbound (executor → runtime) ---

struct ToolCallRequest  { call_id: String, call: ToolCall }
struct CancelCallRequest { call_id: String }

#[type_tag = "type"]
union RuntimeInboundMessage {
    ToolCall(ToolCallRequest),
    CancelCall(CancelCallRequest),
}

// --- Outbound (runtime → executor) ---

struct ToolOutput { stdout: String, stderr: String, exit_code: i32 }
struct ToolError  { reason: String }

#[type_tag = "status"]
union ToolResult { Ok(ToolOutput), Err(ToolError) }

struct ToolCallResponse { call_id: String, result: ToolResult }

/// First message the runtime sends after connecting.
struct RuntimeReady { runtime_id: String }

/// All messages the runtime sends to the executor.
#[type_tag = "type"]
union RuntimeOutboundMessage {
    Ready(RuntimeReady),
    ToolCallResponse(ToolCallResponse),
}
```

- [ ] **Step 2: Extend `fluorite/executor.fl`**

Add `working_dir` to `RuntimeConfig`, and new command/event structs. Replace the existing `RuntimeConfig` struct and add after the existing unions:

```
// Replace:
struct RuntimeConfig {}
// With:
struct RuntimeConfig { working_dir: String }
```

Add before the `ExecutorCommand` union:
```
use runtime.ToolCallRequest;
use runtime.ToolResult;

struct ToolCallCmd { runtime_id: String, call: ToolCallRequest }
struct CancelToolCallCmd { runtime_id: String, call_id: String }
struct ToolResultEvent { runtime_id: String, call_id: String, result: ToolResult }
```

Add to `ExecutorCommand` union:
```
    ToolCall(ToolCallCmd),
    CancelToolCall(CancelToolCallCmd),
```

Add to `ExecutorEvent` union:
```
    ToolResult(ToolResultEvent),
```

- [ ] **Step 3: Add `pub mod runtime` to `models/src/lib.rs`**

Add after the existing `pub mod executor` block:
```rust
#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod runtime {
    include!(concat!(env!("OUT_DIR"), "/runtime/mod.rs"));
}
```

- [ ] **Step 4: Verify models build**

```bash
cargo build -p models
```

Expected: compiles without errors. The `models::runtime` module exposes `ToolCall`, `ToolCallRequest`, `ToolResult`, etc.

- [ ] **Step 5: Fix RuntimeConfig usage in executor tests**

`RuntimeConfig {}` is now invalid. Update `executor/src/registry.rs` test helper:
```rust
fn cfg() -> RuntimeConfig {
    RuntimeConfig { working_dir: "/tmp".to_string() }
}
```

- [ ] **Step 6: Fix wildcard match in executor.rs for new ExecutorCommand variants**

The `dispatch` function in `executor/src/executor.rs` has exhaustive match on `ExecutorCommand`. Add stub arms for the new variants (they'll be implemented fully in Task 6):
```rust
ExecutorCommand::ToolCall(_) | ExecutorCommand::CancelToolCall(_) => {}
```

- [ ] **Step 7: Verify full build**

```bash
cargo build --workspace
```

Expected: compiles. (Tests may fail if RuntimeConfig is used elsewhere — fix any remaining `RuntimeConfig {}` occurrences.)

- [ ] **Step 8: Commit**

```bash
git add fluorite/runtime.fl fluorite/executor.fl models/src/lib.rs executor/src/registry.rs executor/src/executor.rs
git commit -m "feat: runtime.fl protocol + extend executor.fl"
```

---

### Task 2: agentcore — Tool trait + ToolboxImpl

**Files:**
- Modify: `agentcore/src/tool.rs`
- Modify: `agentcore/src/lib.rs`

- [ ] **Step 1: Write failing tests in `agentcore/src/tool.rs`**

Replace the entire file:

```rust
use crate::error::ToolCallError;
use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[async_trait]
pub trait Toolbox: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError>;
}

/// A single named tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError>;
}

/// Generic Toolbox impl — register individual Tool implementations into it.
pub struct ToolboxImpl {
    tools: Vec<Box<dyn Tool>>,
}

impl Default for ToolboxImpl {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolboxImpl {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn add(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Box::new(tool));
        self
    }
}

#[async_trait]
impl Toolbox for ToolboxImpl {
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        match self.tools.iter().find(|t| t.spec().name == name) {
            Some(tool) => tool.execute(input).await,
            None => Err(ToolCallError::InvalidInput(format!("no tool named '{name}'"))),
        }
    }
}

pub struct EmptyToolbox;

#[async_trait]
impl Toolbox for EmptyToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![]
    }

    async fn execute(&self, name: &str, _input: Value) -> Result<Value, ToolCallError> {
        Err(ToolCallError::InvalidInput(format!(
            "no tool named '{name}'"
        )))
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
    use serde_json::json;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".to_string(),
                description: "echoes input".to_string(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
            Ok(input)
        }
    }

    #[tokio::test]
    async fn toolbox_impl_routes_by_name() {
        let tb = ToolboxImpl::new().add(EchoTool);
        let result = tb.execute("echo", json!({"x": 1})).await.unwrap();
        assert_eq!(result, json!({"x": 1}));
    }

    #[tokio::test]
    async fn toolbox_impl_unknown_tool_returns_error() {
        let tb = ToolboxImpl::new();
        let err = tb.execute("nope", json!({})).await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn toolbox_impl_specs_returns_all() {
        let tb = ToolboxImpl::new().add(EchoTool);
        let specs = tb.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
    }
}
```

- [ ] **Step 2: Update `agentcore/src/lib.rs` to re-export new types**

Add to the existing re-exports:
```rust
pub use tool::{EmptyToolbox, Tool, ToolboxImpl, ToolSpec, Toolbox};
```

(Remove the old `pub use tool::{EmptyToolbox, ToolSpec, Toolbox};` line and replace with the above.)

- [ ] **Step 3: Run tests**

```bash
cargo test -p agentcore
```

Expected: all tests pass including the new `toolbox_impl_*` tests.

- [ ] **Step 4: Commit**

```bash
git add agentcore/src/tool.rs agentcore/src/lib.rs
git commit -m "feat: Tool trait + ToolboxImpl in agentcore"
```

---

### Task 3: Executor — ConnectedRuntimeRegistry

**Files:**
- Create: `executor/src/connected_registry.rs`
- Modify: `executor/src/error.rs`
- Modify: `executor/src/lib.rs`

- [ ] **Step 1: Add errors to `executor/src/error.rs`**

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime already exists: {0}")]
    AlreadyExists(String),
    #[error("runtime not found: {0}")]
    NotFound(String),
    #[error("invalid state transition from {from}: cannot {action}")]
    InvalidTransition { from: String, action: String },
    #[error("provider error: {0}")]
    Provider(String),
}

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("connection failed: {0}")]
    Connection(String),
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("bind failed: {0}")]
    BindFailed(String),
    #[error("spawn failed: {0}")]
    SpawnFailed(String),
}
```

- [ ] **Step 2: Create `executor/src/connected_registry.rs`**

```rust
use futures_util::SinkExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

pub(crate) type RuntimeSink = Arc<
    Mutex<
        futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>,
    >,
>;

struct Inner {
    sinks: HashMap<String, RuntimeSink>,
    pending: HashMap<String, oneshot::Sender<()>>,
}

/// Tracks live WebSocket connections from runtime binaries.
pub(crate) struct ConnectedRuntimeRegistry {
    inner: Mutex<Inner>,
}

impl ConnectedRuntimeRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                sinks: HashMap::new(),
                pending: HashMap::new(),
            }),
        }
    }

    /// Register a runtime's WS sink. Resolves any pending `notify_when_ready` waiter.
    pub async fn register(&self, runtime_id: String, sink: RuntimeSink) {
        let mut inner = self.inner.lock().await;
        inner.sinks.insert(runtime_id.clone(), sink);
        if let Some(tx) = inner.pending.remove(&runtime_id) {
            let _ = tx.send(());
        }
    }

    /// Returns a receiver that resolves when `register` is called for `runtime_id`.
    /// Must be called BEFORE the process is spawned.
    pub async fn notify_when_ready(&self, runtime_id: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .lock()
            .await
            .pending
            .insert(runtime_id.to_string(), tx);
        rx
    }

    /// Look up a connected runtime's sink.
    pub async fn get_sink(&self, runtime_id: &str) -> Option<RuntimeSink> {
        self.inner.lock().await.sinks.get(runtime_id).cloned()
    }

    /// Remove a runtime (called when its WS connection drops).
    pub async fn remove(&self, runtime_id: &str) {
        self.inner.lock().await.sinks.remove(runtime_id);
    }

    /// Send a serialized message to a connected runtime.
    pub async fn send_to(
        &self,
        runtime_id: &str,
        json: String,
    ) -> Result<(), crate::error::ExecutorError> {
        match self.get_sink(runtime_id).await {
            Some(sink) => sink
                .lock()
                .await
                .send(Message::Text(json.into()))
                .await
                .map_err(|e| crate::error::ExecutorError::SendFailed(e.to_string())),
            None => Err(crate::error::ExecutorError::SendFailed(format!(
                "runtime '{runtime_id}' not connected"
            ))),
        }
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

    #[tokio::test]
    async fn notify_resolves_when_registered() {
        let reg = ConnectedRuntimeRegistry::new();
        let rx = reg.notify_when_ready("rt-1").await;

        // Simulate registration from a different task
        // We can't create a real RuntimeSink without a WS, so just check
        // that notify_when_ready inserts into pending
        let has_pending = reg.inner.lock().await.pending.contains_key("rt-1");
        assert!(has_pending);
        drop(rx); // cleanup
    }

    #[tokio::test]
    async fn get_sink_returns_none_for_unknown() {
        let reg = ConnectedRuntimeRegistry::new();
        assert!(reg.get_sink("ghost").await.is_none());
    }

    #[tokio::test]
    async fn remove_clears_entry() {
        let reg = ConnectedRuntimeRegistry::new();
        // Can't register a real sink, but we can verify remove is a no-op on missing
        reg.remove("ghost").await; // should not panic
    }
}
```

- [ ] **Step 3: Wire into `executor/src/lib.rs`**

Add `mod connected_registry;` and `pub(crate) use connected_registry::ConnectedRuntimeRegistry;`.

- [ ] **Step 4: Run tests**

```bash
cargo test -p executor
```

Expected: all existing tests pass, new registry tests pass.

- [ ] **Step 5: Commit**

```bash
git add executor/src/connected_registry.rs executor/src/error.rs executor/src/lib.rs
git commit -m "feat: ConnectedRuntimeRegistry + executor errors"
```

---

### Task 4: Executor — RuntimeListenerServer

**Files:**
- Create: `executor/src/runtime_listener.rs`
- Modify: `executor/Cargo.toml`

- [ ] **Step 1: Verify `executor/Cargo.toml` has required deps**

Check current content. It needs `tokio-tungstenite` and `futures-util`. Add if missing:

```toml
[dependencies]
models          = { path = "../models" }
async-trait     = { workspace = true }
thiserror       = { workspace = true }
tokio           = { workspace = true }
tokio-util      = { workspace = true }
tokio-tungstenite = { workspace = true }
futures-util    = { workspace = true }
serde_json      = { workspace = true }
uuid            = { workspace = true }
```

- [ ] **Step 2: Create `executor/src/runtime_listener.rs`**

```rust
use crate::error::ExecutorError;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, accept_async};

/// Listens for incoming WebSocket connections from runtime binaries.
pub(crate) struct RuntimeListenerServer {
    listener: TcpListener,
    local_addr: SocketAddr,
}

impl RuntimeListenerServer {
    pub async fn bind(addr: &str) -> Result<Self, ExecutorError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| ExecutorError::BindFailed(e.to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| ExecutorError::BindFailed(e.to_string()))?;
        Ok(Self {
            listener,
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn accept(&self) -> Result<WebSocketStream<TcpStream>, ExecutorError> {
        let (stream, _) = self
            .listener
            .accept()
            .await
            .map_err(|e| ExecutorError::Connection(e.to_string()))?;
        accept_async(stream)
            .await
            .map_err(|e| ExecutorError::Connection(e.to_string()))
    }
}
```

- [ ] **Step 3: Add to `executor/src/lib.rs`**

```rust
mod runtime_listener;
pub(crate) use runtime_listener::RuntimeListenerServer;
```

- [ ] **Step 4: Verify build**

```bash
cargo build -p executor
```

- [ ] **Step 5: Commit**

```bash
git add executor/src/runtime_listener.rs executor/src/lib.rs executor/Cargo.toml
git commit -m "feat: RuntimeListenerServer in executor"
```

---

### Task 5: Executor — ProcessRuntimeProvider

**Files:**
- Create: `executor/src/process_provider.rs`

- [ ] **Step 1: Create `executor/src/process_provider.rs`**

```rust
use crate::{
    connected_registry::ConnectedRuntimeRegistry,
    error::RuntimeError,
    provider::{HealthStatus, RuntimeHandle},
};
use async_trait::async_trait;
use models::executor::RuntimeConfig;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::{process::Child, sync::Mutex};

pub struct ProcessRuntimeHandle {
    child: Mutex<Option<Child>>,
    runtime_id: String,
    connected_registry: Arc<ConnectedRuntimeRegistry>,
}

#[async_trait]
impl RuntimeHandle for ProcessRuntimeHandle {
    async fn stop(&self) -> Result<(), RuntimeError> {
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.kill().await;
        }
        self.connected_registry.remove(&self.runtime_id).await;
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus, RuntimeError> {
        let connected = self
            .connected_registry
            .get_sink(&self.runtime_id)
            .await
            .is_some();
        if connected {
            Ok(HealthStatus::Healthy)
        } else {
            Ok(HealthStatus::Unhealthy {
                reason: "runtime disconnected".to_string(),
            })
        }
    }
}

/// RuntimeProvider that spawns `october-runtime` as a child process.
/// Used for testing and for environments where the runtime runs locally.
pub struct ProcessRuntimeProvider {
    binary_path: PathBuf,
    listener_addr: SocketAddr,
    connected_registry: Arc<ConnectedRuntimeRegistry>,
    connect_timeout: Duration,
}

impl ProcessRuntimeProvider {
    pub fn new(
        binary_path: PathBuf,
        listener_addr: SocketAddr,
        connected_registry: Arc<ConnectedRuntimeRegistry>,
    ) -> Self {
        Self {
            binary_path,
            listener_addr,
            connected_registry,
            connect_timeout: Duration::from_secs(30),
        }
    }

    pub fn with_connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }
}

#[async_trait]
impl crate::provider::RuntimeProvider for ProcessRuntimeProvider {
    async fn create(
        &self,
        id: &str,
        config: &RuntimeConfig,
    ) -> Result<Arc<dyn RuntimeHandle>, RuntimeError> {
        // Register a watcher BEFORE spawning to avoid a race.
        let ready_rx = self.connected_registry.notify_when_ready(id).await;

        let child = tokio::process::Command::new(&self.binary_path)
            .arg("--executor-url")
            .arg(format!("ws://{}", self.listener_addr))
            .arg("--runtime-id")
            .arg(id)
            .arg("--working-dir")
            .arg(&config.working_dir)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RuntimeError::Provider(e.to_string()))?;

        tokio::time::timeout(self.connect_timeout, ready_rx)
            .await
            .map_err(|_| RuntimeError::Provider("runtime connection timed out".to_string()))?
            .map_err(|_| {
                RuntimeError::Provider("connection channel dropped".to_string())
            })?;

        Ok(Arc::new(ProcessRuntimeHandle {
            child: Mutex::new(Some(child)),
            runtime_id: id.to_string(),
            connected_registry: Arc::clone(&self.connected_registry),
        }))
    }
}
```

- [ ] **Step 2: Wire into `executor/src/lib.rs`**

```rust
mod process_provider;
pub use process_provider::ProcessRuntimeProvider;
```

- [ ] **Step 3: Build**

```bash
cargo build -p executor
```

- [ ] **Step 4: Commit**

```bash
git add executor/src/process_provider.rs executor/src/lib.rs
git commit -m "feat: ProcessRuntimeProvider"
```

---

### Task 6: Executor — listener integration + tool call routing

**Files:**
- Modify: `executor/src/executor.rs`

This is the most involved task. We extend `Executor` with an optional `RuntimeListenerServer` + `ConnectedRuntimeRegistry`, add a `with_runtime_listener` builder, extend `dispatch` to handle `ToolCall`/`CancelToolCall`, and add `handle_runtime_connection`.

- [ ] **Step 1: Replace `executor/src/executor.rs`**

```rust
use crate::{
    connected_registry::{ConnectedRuntimeRegistry, RuntimeSink},
    error::{ExecutorError, RuntimeError},
    provider::{HealthStatus, RuntimeProvider},
    registry::RuntimeRegistry,
    runtime_listener::RuntimeListenerServer,
};
use futures_util::{SinkExt, StreamExt};
use models::executor::{
    CancelToolCallCmd, CommandFailedEvent, CreateRuntimeCmd, DestroyRuntimeCmd, ExecutorCommand,
    ExecutorEvent, ExecutorInboundMessage, ExecutorOutboundMessage, RegisteredEvent,
    RestartRuntimeCmd, RuntimeState, RuntimeStateChangedEvent, RuntimesListedEvent,
    ToolCallCmd, ToolResultEvent,
};
use models::runtime::{
    CancelCallRequest, RuntimeInboundMessage, RuntimeOutboundMessage, ToolCallRequest,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::{MaybeTlsStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type WsSink = Arc<
    Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
            Message,
        >,
    >,
>;

async fn send_outbound(sink: &WsSink, msg: ExecutorOutboundMessage) -> Result<(), ExecutorError> {
    let json =
        serde_json::to_string(&msg).map_err(|e| ExecutorError::Serialization(e.to_string()))?;
    sink.lock()
        .await
        .send(Message::Text(json.into()))
        .await
        .map_err(|e| ExecutorError::SendFailed(e.to_string()))
}

async fn emit_state(sink: &WsSink, request_id: &str, runtime_id: &str, state: RuntimeState) {
    let _ = send_outbound(
        sink,
        ExecutorOutboundMessage {
            request_id: request_id.to_string(),
            event: ExecutorEvent::RuntimeStateChanged(RuntimeStateChangedEvent {
                runtime_id: runtime_id.to_string(),
                state,
            }),
        },
    )
    .await;
}

pub struct Executor {
    executor_id: String,
    server_url: String,
    provider: Box<dyn RuntimeProvider>,
    health_check_interval: Duration,
    max_restarts: u32,
    runtime_listener: Option<RuntimeListenerServer>,
    connected_registry: Option<Arc<ConnectedRuntimeRegistry>>,
}

impl Executor {
    pub fn new(
        executor_id: String,
        server_url: String,
        provider: Box<dyn RuntimeProvider>,
    ) -> Self {
        Self {
            executor_id,
            server_url,
            provider,
            health_check_interval: Duration::from_secs(30),
            max_restarts: 3,
            runtime_listener: None,
            connected_registry: None,
        }
    }

    pub fn with_health_check_interval(mut self, interval: Duration) -> Self {
        self.health_check_interval = interval;
        self
    }

    pub fn with_max_restarts(mut self, max: u32) -> Self {
        self.max_restarts = max;
        self
    }

    pub fn with_runtime_listener(
        mut self,
        listener: RuntimeListenerServer,
        registry: Arc<ConnectedRuntimeRegistry>,
    ) -> Self {
        self.runtime_listener = Some(listener);
        self.connected_registry = Some(registry);
        self
    }

    pub async fn run(self, cancel: CancellationToken) -> Result<(), ExecutorError> {
        let (ws, _) = connect_async(&self.server_url)
            .await
            .map_err(|e| ExecutorError::Connection(e.to_string()))?;
        let (sink_inner, mut stream) = ws.split();
        let sink: WsSink = Arc::new(Mutex::new(sink_inner));

        send_outbound(
            &sink,
            ExecutorOutboundMessage {
                request_id: Uuid::new_v4().to_string(),
                event: ExecutorEvent::Registered(RegisteredEvent {
                    executor_id: self.executor_id.clone(),
                }),
            },
        )
        .await?;

        let registry = Arc::new(RuntimeRegistry::new());
        let provider: Arc<dyn RuntimeProvider> = Arc::from(self.provider);
        let max_restarts = self.max_restarts;
        let connected_registry = self.connected_registry;

        // Start runtime listener if configured
        if let (Some(listener), Some(conn_reg)) = (self.runtime_listener, connected_registry.clone()) {
            let listener_sink = sink.clone();
            let listener_cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = listener_cancel.cancelled() => break,
                        result = listener.accept() => {
                            match result {
                                Ok(ws) => {
                                    let reg = conn_reg.clone();
                                    let srv_sink = listener_sink.clone();
                                    tokio::spawn(handle_runtime_connection(ws, reg, srv_sink));
                                }
                                Err(_) => break,
                            }
                        }
                    }
                }
            });
        }

        let hc_sink = sink.clone();
        let hc_reg = registry.clone();
        let hc_prov = provider.clone();
        let hc_cancel = cancel.clone();
        let hc_interval = self.health_check_interval;
        let health_task = tokio::spawn(async move {
            let start = tokio::time::Instant::now() + hc_interval;
            let mut ticker = tokio::time::interval_at(start, hc_interval);
            loop {
                tokio::select! {
                    _ = hc_cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        run_health_check(&hc_reg, &hc_prov, &hc_sink, max_restarts).await;
                    }
                }
            }
        });

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(inbound) = serde_json::from_str::<ExecutorInboundMessage>(&text) {
                                dispatch(&inbound, &registry, &provider, &sink, connected_registry.as_ref()).await;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        Some(Ok(Message::Binary(_)))
                        | Some(Ok(Message::Ping(_)))
                        | Some(Ok(Message::Pong(_)))
                        | Some(Ok(Message::Frame(_))) => {}
                    }
                }
            }
        }

        health_task.abort();
        Ok(())
    }
}

async fn dispatch(
    msg: &ExecutorInboundMessage,
    registry: &Arc<RuntimeRegistry>,
    provider: &Arc<dyn RuntimeProvider>,
    sink: &WsSink,
    connected_registry: Option<&Arc<ConnectedRuntimeRegistry>>,
) {
    let req = &msg.request_id;
    let result = match &msg.command {
        ExecutorCommand::CreateRuntime(cmd) => do_create(cmd, registry, provider, sink, req).await,
        ExecutorCommand::DestroyRuntime(cmd) => do_destroy(cmd, registry, sink, req).await,
        ExecutorCommand::RestartRuntime(cmd) => {
            do_restart(cmd, registry, provider, sink, req).await
        }
        ExecutorCommand::QueryRuntimes(_) => {
            let runtimes = registry.list().await;
            let _ = send_outbound(
                sink,
                ExecutorOutboundMessage {
                    request_id: req.clone(),
                    event: ExecutorEvent::RuntimesListed(RuntimesListedEvent { runtimes }),
                },
            )
            .await;
            Ok(())
        }
        ExecutorCommand::ToolCall(cmd) => {
            do_tool_call(cmd, connected_registry, sink, req).await
        }
        ExecutorCommand::CancelToolCall(cmd) => {
            do_cancel_tool_call(cmd, connected_registry).await
        }
    };
    if let Err(e) = result {
        let _ = send_outbound(
            sink,
            ExecutorOutboundMessage {
                request_id: req.clone(),
                event: ExecutorEvent::CommandFailed(CommandFailedEvent {
                    message: e.to_string(),
                }),
            },
        )
        .await;
    }
}

async fn do_tool_call(
    cmd: &ToolCallCmd,
    connected_registry: Option<&Arc<ConnectedRuntimeRegistry>>,
    sink: &WsSink,
    _req: &str,
) -> Result<(), RuntimeError> {
    let reg = connected_registry.ok_or_else(|| {
        RuntimeError::Provider("no runtime listener configured".to_string())
    })?;
    let msg = RuntimeInboundMessage::ToolCall(ToolCallRequest {
        call_id: cmd.call.call_id.clone(),
        call: cmd.call.call.clone(),
    });
    let json = serde_json::to_string(&msg)
        .map_err(|e| RuntimeError::Provider(e.to_string()))?;
    reg.send_to(&cmd.runtime_id, json)
        .await
        .map_err(|e| RuntimeError::Provider(e.to_string()))
}

async fn do_cancel_tool_call(
    cmd: &CancelToolCallCmd,
    connected_registry: Option<&Arc<ConnectedRuntimeRegistry>>,
) -> Result<(), RuntimeError> {
    let reg = connected_registry.ok_or_else(|| {
        RuntimeError::Provider("no runtime listener configured".to_string())
    })?;
    let msg = RuntimeInboundMessage::CancelCall(CancelCallRequest {
        call_id: cmd.call_id.clone(),
    });
    let json = serde_json::to_string(&msg)
        .map_err(|e| RuntimeError::Provider(e.to_string()))?;
    // Best-effort — if runtime isn't connected the call will time out on the server
    let _ = reg.send_to(&cmd.runtime_id, json).await;
    Ok(())
}

async fn handle_runtime_connection(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    registry: Arc<ConnectedRuntimeRegistry>,
    server_sink: WsSink,
) {
    use futures_util::StreamExt;
    let (ws_sink, mut ws_stream) = ws.split();
    let ws_sink: RuntimeSink = Arc::new(Mutex::new(ws_sink));

    // First message must be RuntimeReady
    let runtime_id = loop {
        match ws_stream.next().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(RuntimeOutboundMessage::Ready(ev)) =
                    serde_json::from_str::<RuntimeOutboundMessage>(&text)
                {
                    break ev.runtime_id;
                }
            }
            _ => return,
        }
    };

    registry.register(runtime_id.clone(), ws_sink).await;

    // Process ToolCallResponse messages
    while let Some(msg) = ws_stream.next().await {
        if let Ok(Message::Text(text)) = msg {
            if let Ok(RuntimeOutboundMessage::ToolCallResponse(resp)) =
                serde_json::from_str::<RuntimeOutboundMessage>(&text)
            {
                let event = ExecutorOutboundMessage {
                    request_id: resp.call_id.clone(),
                    event: ExecutorEvent::ToolResult(ToolResultEvent {
                        runtime_id: runtime_id.clone(),
                        call_id: resp.call_id,
                        result: resp.result,
                    }),
                };
                let _ = send_outbound(&server_sink, event).await;
            }
        } else {
            break;
        }
    }

    registry.remove(&runtime_id).await;
}

async fn do_create(
    cmd: &CreateRuntimeCmd,
    registry: &Arc<RuntimeRegistry>,
    provider: &Arc<dyn RuntimeProvider>,
    sink: &WsSink,
    req: &str,
) -> Result<(), RuntimeError> {
    registry
        .begin_create(&cmd.runtime_id, cmd.config.clone())
        .await?;
    emit_state(sink, req, &cmd.runtime_id, RuntimeState::Creating).await;
    match provider.create(&cmd.runtime_id, &cmd.config).await {
        Ok(handle) => {
            registry.complete_create(&cmd.runtime_id, handle).await?;
            emit_state(sink, req, &cmd.runtime_id, RuntimeState::Running).await;
            Ok(())
        }
        Err(e) => {
            let _ = registry.mark_failed(&cmd.runtime_id).await;
            emit_state(sink, req, &cmd.runtime_id, RuntimeState::Failed).await;
            Err(e)
        }
    }
}

async fn do_destroy(
    cmd: &DestroyRuntimeCmd,
    registry: &Arc<RuntimeRegistry>,
    sink: &WsSink,
    req: &str,
) -> Result<(), RuntimeError> {
    let handle = registry.begin_stop(&cmd.runtime_id).await?;
    emit_state(sink, req, &cmd.runtime_id, RuntimeState::Stopping).await;
    if let Some(h) = handle {
        let _ = h.stop().await;
    }
    registry.complete_stop(&cmd.runtime_id).await?;
    emit_state(sink, req, &cmd.runtime_id, RuntimeState::Stopped).await;
    Ok(())
}

async fn do_restart(
    cmd: &RestartRuntimeCmd,
    registry: &Arc<RuntimeRegistry>,
    provider: &Arc<dyn RuntimeProvider>,
    sink: &WsSink,
    req: &str,
) -> Result<(), RuntimeError> {
    let config = registry
        .get_config(&cmd.runtime_id)
        .await
        .ok_or_else(|| RuntimeError::NotFound(cmd.runtime_id.clone()))?;
    let old_handle = registry.begin_restart(&cmd.runtime_id).await?;
    emit_state(sink, req, &cmd.runtime_id, RuntimeState::Creating).await;
    if let Some(h) = old_handle {
        let _ = h.stop().await;
    }
    match provider.create(&cmd.runtime_id, &config).await {
        Ok(handle) => {
            registry.complete_create(&cmd.runtime_id, handle).await?;
            emit_state(sink, req, &cmd.runtime_id, RuntimeState::Running).await;
            Ok(())
        }
        Err(e) => {
            let _ = registry.mark_failed(&cmd.runtime_id).await;
            emit_state(sink, req, &cmd.runtime_id, RuntimeState::Failed).await;
            Err(e)
        }
    }
}

async fn run_health_check(
    registry: &Arc<RuntimeRegistry>,
    provider: &Arc<dyn RuntimeProvider>,
    sink: &WsSink,
    max_restarts: u32,
) {
    let handles = registry.running_handles().await;
    for (id, handle) in handles {
        let healthy = matches!(handle.health_check().await, Ok(HealthStatus::Healthy));
        if healthy {
            continue;
        }
        let _ = registry.mark_failed(&id).await;
        let unsolicited = Uuid::new_v4().to_string();
        emit_state(sink, &unsolicited, &id, RuntimeState::Failed).await;

        let count = registry.get_restart_count(&id).await.unwrap_or(u32::MAX);
        if count >= max_restarts {
            continue;
        }
        if let Some(config) = registry.get_config(&id).await
            && let Ok(old) = registry.begin_restart(&id).await
        {
            emit_state(sink, &unsolicited, &id, RuntimeState::Creating).await;
            if let Some(h) = old {
                let _ = h.stop().await;
            }
            match provider.create(&id, &config).await {
                Ok(new_handle) => {
                    let _ = registry.complete_create(&id, new_handle).await;
                    emit_state(sink, &unsolicited, &id, RuntimeState::Running).await;
                }
                Err(_) => {
                    let _ = registry.mark_failed(&id).await;
                    emit_state(sink, &unsolicited, &id, RuntimeState::Failed).await;
                }
            }
        }
    }
}
```

- [ ] **Step 2: Build and test**

```bash
cargo build -p executor && cargo test -p executor
```

Expected: clean build and all tests pass.

- [ ] **Step 3: Commit**

```bash
git add executor/src/executor.rs
git commit -m "feat: executor listener integration + tool call routing"
```

---

### Task 7: Runtime binary — tool implementations

**Files:**
- Modify: `runtime/Cargo.toml`
- Modify: `runtime/src/lib.rs`
- Create: `runtime/src/tools/mod.rs`
- Create: `runtime/src/tools/bash.rs`
- Create: `runtime/src/tools/read_file.rs`
- Create: `runtime/src/tools/write_file.rs`
- Create: `runtime/src/tools/edit_file.rs`
- Create: `runtime/src/tools/replace_in_file.rs`
- Create: `runtime/src/tools/list_files.rs`
- Create: `runtime/src/tools/glob.rs`
- Create: `runtime/src/tools/grep.rs`

- [ ] **Step 1: Update `runtime/Cargo.toml`**

```toml
[package]
name = "runtime"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "october-runtime"
path = "src/main.rs"

[dependencies]
models        = { path = "../models" }
tokio         = { workspace = true, features = ["process"] }
tokio-tungstenite = { workspace = true }
futures-util  = { workspace = true }
serde_json    = { workspace = true }
clap          = { version = "4", features = ["derive"] }

[dev-dependencies]
tempfile = "3"

[lints]
workspace = true
```

Also add to workspace `Cargo.toml` `[workspace.dependencies]`:
```toml
tempfile = "3"
clap     = { version = "4", features = ["derive"] }
```

- [ ] **Step 2: Update `runtime/src/lib.rs`**

```rust
pub mod tools;
```

- [ ] **Step 3: Create `runtime/src/tools/bash.rs`**

```rust
use models::runtime::{BashInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: BashInput) -> ToolResult {
    let child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&input.command)
        .current_dir(working_dir)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    match child {
        Ok(child) => match child.wait_with_output().await {
            Ok(output) => ToolResult::Ok(ToolOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
            }),
            Err(e) => ToolResult::Err(ToolError {
                reason: e.to_string(),
            }),
        },
        Err(e) => ToolResult::Err(ToolError {
            reason: e.to_string(),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn bash_echo() {
        let dir = TempDir::new().unwrap();
        let result = exec(dir.path(), BashInput { command: "echo hello".to_string() }).await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout.trim(), "hello"),
            ToolResult::Err(e) => panic!("unexpected error: {}", e.reason),
        }
    }

    #[tokio::test]
    async fn bash_nonzero_exit() {
        let dir = TempDir::new().unwrap();
        let result = exec(dir.path(), BashInput { command: "exit 42".to_string() }).await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.exit_code, 42),
            ToolResult::Err(e) => panic!("unexpected error: {}", e.reason),
        }
    }

    #[tokio::test]
    async fn bash_uses_working_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("sentinel.txt"), "found").unwrap();
        let result = exec(dir.path(), BashInput { command: "cat sentinel.txt".to_string() }).await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout.trim(), "found"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}
```

- [ ] **Step 4: Create `runtime/src/tools/read_file.rs`**

```rust
use models::runtime::{ReadFileInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: ReadFileInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let result = match (input.start_line, input.end_line) {
            (Some(s), Some(e)) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = (s as usize).saturating_sub(1).min(lines.len());
                let end = (e as usize).min(lines.len());
                lines[start..end].join("\n")
            }
            (Some(s), None) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = (s as usize).saturating_sub(1).min(lines.len());
                lines[start..].join("\n")
            }
            _ => content,
        };
        Ok::<String, String>(result)
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput { stdout, stderr: String::new(), exit_code: 0 }),
        Ok(Err(reason)) => ToolResult::Err(ToolError { reason }),
        Err(e) => ToolResult::Err(ToolError { reason: e.to_string() }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn read_full_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "line1\nline2\nline3").unwrap();
        let result = exec(dir.path(), ReadFileInput { path: "f.txt".into(), start_line: None, end_line: None }).await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "line1\nline2\nline3"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn read_line_range() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd").unwrap();
        let result = exec(dir.path(), ReadFileInput { path: "f.txt".into(), start_line: Some(2), end_line: Some(3) }).await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "b\nc"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}
```

- [ ] **Step 5: Create `runtime/src/tools/write_file.rs`**

```rust
use models::runtime::{ToolError, ToolOutput, ToolResult, WriteFileInput};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: WriteFileInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, &input.content).map_err(|e| e.to_string())
    })
    .await
    {
        Ok(Ok(())) => ToolResult::Ok(ToolOutput {
            stdout: "File written.".into(),
            stderr: String::new(),
            exit_code: 0,
        }),
        Ok(Err(reason)) => ToolResult::Err(ToolError { reason }),
        Err(e) => ToolResult::Err(ToolError { reason: e.to_string() }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn write_creates_file() {
        let dir = TempDir::new().unwrap();
        exec(dir.path(), WriteFileInput { path: "out.txt".into(), content: "hello".into() }).await;
        assert_eq!(std::fs::read_to_string(dir.path().join("out.txt")).unwrap(), "hello");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        exec(dir.path(), WriteFileInput { path: "a/b/c.txt".into(), content: "x".into() }).await;
        assert!(dir.path().join("a/b/c.txt").exists());
    }
}
```

- [ ] **Step 6: Create `runtime/src/tools/edit_file.rs`**

```rust
use models::runtime::{EditFileInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: EditFileInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        if !content.contains(&input.old_text) {
            return Err(format!("old_text not found in '{}'", input.path));
        }
        let new_content = content.replacen(&input.old_text, &input.new_text, 1);
        std::fs::write(&path, new_content).map_err(|e| e.to_string())?;
        Ok::<String, String>(format!("Edited '{}'.", input.path))
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput { stdout, stderr: String::new(), exit_code: 0 }),
        Ok(Err(reason)) => ToolResult::Err(ToolError { reason }),
        Err(e) => ToolResult::Err(ToolError { reason: e.to_string() }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn edit_replaces_text() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello world").unwrap();
        let result = exec(dir.path(), EditFileInput { path: "f.txt".into(), old_text: "world".into(), new_text: "rust".into() }).await;
        assert!(matches!(result, ToolResult::Ok(_)));
        assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "hello rust");
    }

    #[tokio::test]
    async fn edit_returns_error_when_not_found() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello").unwrap();
        let result = exec(dir.path(), EditFileInput { path: "f.txt".into(), old_text: "missing".into(), new_text: "x".into() }).await;
        assert!(matches!(result, ToolResult::Err(_)));
    }
}
```

- [ ] **Step 7: Create `runtime/src/tools/replace_in_file.rs`**

```rust
use models::runtime::{ReplaceInFileInput, ReplaceMode, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: ReplaceInFileInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let new_content = match &input.mode {
            ReplaceMode::Regex(r) => {
                let re = regex::Regex::new(&r.pattern).map_err(|e| e.to_string())?;
                re.replace_all(&content, input.replacement.as_str()).into_owned()
            }
            ReplaceMode::Lines(l) => {
                let mut lines: Vec<&str> = content.lines().collect();
                let start = (l.start_line as usize).saturating_sub(1).min(lines.len());
                let end = (l.end_line as usize).min(lines.len());
                let replacement_lines: Vec<&str> = input.replacement.lines().collect();
                lines.splice(start..end, replacement_lines);
                lines.join("\n")
            }
        };
        std::fs::write(&path, new_content).map_err(|e| e.to_string())?;
        Ok::<String, String>(format!("Replaced in '{}'.", input.path))
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput { stdout, stderr: String::new(), exit_code: 0 }),
        Ok(Err(reason)) => ToolResult::Err(ToolError { reason }),
        Err(e) => ToolResult::Err(ToolError { reason: e.to_string() }),
    }
}
```

Add `regex = "1"` to `runtime/Cargo.toml` dependencies and workspace deps.

- [ ] **Step 8: Create `runtime/src/tools/list_files.rs`**

```rust
use models::runtime::{ListFilesInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: ListFilesInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        let entries = std::fs::read_dir(&path).map_err(|e| e.to_string())?;
        let mut lines = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let meta = entry.metadata().map_err(|e| e.to_string())?;
            let kind = if meta.is_dir() { "d" } else { "f" };
            let name = entry.file_name().to_string_lossy().into_owned();
            lines.push(format!("{kind} {name}"));
        }
        lines.sort();
        Ok::<String, String>(lines.join("\n"))
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput { stdout, stderr: String::new(), exit_code: 0 }),
        Ok(Err(reason)) => ToolResult::Err(ToolError { reason }),
        Err(e) => ToolResult::Err(ToolError { reason: e.to_string() }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn list_files_shows_entries() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let result = exec(dir.path(), ListFilesInput { path: ".".into() }).await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("a.txt"));
                assert!(o.stdout.contains("sub"));
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}
```

- [ ] **Step 9: Create `runtime/src/tools/glob.rs`**

```rust
use models::runtime::{GlobInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: GlobInput) -> ToolResult {
    let base = match &input.path {
        Some(p) => working_dir.join(p),
        None => working_dir.to_path_buf(),
    };
    let pattern = format!("{}/{}", base.display(), input.pattern);
    let max = input.max_results.unwrap_or(1000) as usize;
    match tokio::task::spawn_blocking(move || {
        let matches: Vec<String> = glob::glob(&pattern)
            .map_err(|e| e.to_string())?
            .take(max)
            .filter_map(|e| e.ok())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        Ok::<String, String>(matches.join("\n"))
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput { stdout, stderr: String::new(), exit_code: 0 }),
        Ok(Err(reason)) => ToolResult::Err(ToolError { reason }),
        Err(e) => ToolResult::Err(ToolError { reason: e.to_string() }),
    }
}
```

Add `glob = "0.3"` to `runtime/Cargo.toml` and workspace deps.

- [ ] **Step 10: Create `runtime/src/tools/grep.rs`**

```rust
use models::runtime::{GrepInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: GrepInput) -> ToolResult {
    let base = match &input.path {
        Some(p) => working_dir.join(p),
        None => working_dir.to_path_buf(),
    };
    let file_pat = input.file_pattern.clone().unwrap_or_else(|| "**/*".to_string());
    let max = input.max_results.unwrap_or(1000) as usize;
    let pattern = input.pattern.clone();
    match tokio::task::spawn_blocking(move || {
        let re = regex::Regex::new(&pattern).map_err(|e| e.to_string())?;
        let glob_pat = format!("{}/{}", base.display(), file_pat);
        let mut results = Vec::new();
        'outer: for path in glob::glob(&glob_pat).map_err(|e| e.to_string())?.flatten() {
            if path.is_file() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    for (i, line) in content.lines().enumerate() {
                        if re.is_match(line) {
                            results.push(format!("{}:{}: {}", path.display(), i + 1, line));
                            if results.len() >= max {
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
        Ok::<String, String>(results.join("\n"))
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput { stdout, stderr: String::new(), exit_code: 0 }),
        Ok(Err(reason)) => ToolResult::Err(ToolError { reason }),
        Err(e) => ToolResult::Err(ToolError { reason: e.to_string() }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn grep_finds_match() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello world\nfoo bar").unwrap();
        let result = exec(dir.path(), GrepInput { pattern: "hello".into(), path: None, file_pattern: None, max_results: None }).await;
        match result {
            ToolResult::Ok(o) => assert!(o.stdout.contains("hello world")),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}
```

- [ ] **Step 11: Create `runtime/src/tools/mod.rs`**

```rust
pub mod bash;
pub mod edit_file;
pub mod glob;
pub mod grep;
pub mod list_files;
pub mod read_file;
pub mod replace_in_file;
pub mod write_file;

use models::runtime::{ToolCall, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn dispatch(working_dir: &Path, call: ToolCall) -> ToolResult {
    match call {
        ToolCall::Bash(input) => bash::exec(working_dir, input).await,
        ToolCall::ReadFile(input) => read_file::exec(working_dir, input).await,
        ToolCall::WriteFile(input) => write_file::exec(working_dir, input).await,
        ToolCall::EditFile(input) => edit_file::exec(working_dir, input).await,
        ToolCall::ReplaceInFile(input) => replace_in_file::exec(working_dir, input).await,
        ToolCall::ListFiles(input) => list_files::exec(working_dir, input).await,
        ToolCall::Glob(input) => glob::exec(working_dir, input).await,
        ToolCall::Grep(input) => grep::exec(working_dir, input).await,
    }
}
```

- [ ] **Step 12: Run tool tests**

```bash
cargo test -p runtime
```

Expected: all tool unit tests pass.

- [ ] **Step 13: Commit**

```bash
git add runtime/
git commit -m "feat: runtime tool implementations"
```

---

### Task 8: Runtime binary — main

**Files:**
- Create: `runtime/src/main.rs`

- [ ] **Step 1: Create `runtime/src/main.rs`**

```rust
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::wildcard_enum_match_arm
    )
)]

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use models::runtime::{
    RuntimeInboundMessage, RuntimeOutboundMessage, RuntimeReady, ToolCallResponse,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    executor_url: String,
    #[arg(long)]
    runtime_id: String,
    #[arg(long)]
    working_dir: PathBuf,
}

type WsSink = Arc<
    Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
    >,
>;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let (ws, _) = connect_async(&cli.executor_url)
        .await
        .unwrap_or_else(|e| {
            eprintln!("failed to connect to executor: {e}");
            std::process::exit(1);
        });

    let (sink_raw, mut stream) = ws.split();
    let sink: WsSink = Arc::new(Mutex::new(sink_raw));

    // Announce ourselves
    let ready = serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
        runtime_id: cli.runtime_id.clone(),
    }))
    .unwrap_or_else(|e| {
        eprintln!("serialization error: {e}");
        std::process::exit(1);
    });
    if let Err(e) = sink.lock().await.send(Message::Text(ready.into())).await {
        eprintln!("failed to send RuntimeReady: {e}");
        std::process::exit(1);
    }

    // in-flight task map: call_id → AbortHandle
    let in_flight: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                let inbound = match serde_json::from_str::<RuntimeInboundMessage>(&text) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                match inbound {
                    RuntimeInboundMessage::ToolCall(req) => {
                        let call_id = req.call_id.clone();
                        let working_dir = cli.working_dir.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        let handle = tokio::spawn(async move {
                            let result =
                                runtime::tools::dispatch(&working_dir, req.call).await;
                            let response = serde_json::to_string(
                                &RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                                    call_id: call_id.clone(),
                                    result,
                                }),
                            );
                            if let Ok(json) = response {
                                let _ = sink_clone
                                    .lock()
                                    .await
                                    .send(Message::Text(json.into()))
                                    .await;
                            }
                            in_flight_clone.lock().await.remove(&call_id);
                        });

                        in_flight.lock().await.insert(req.call_id, handle.abort_handle());
                    }
                    RuntimeInboundMessage::CancelCall(req) => {
                        if let Some(handle) = in_flight.lock().await.remove(&req.call_id) {
                            handle.abort();
                        }
                        // Send cancelled response
                        let response = serde_json::to_string(
                            &RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                                call_id: req.call_id,
                                result: models::runtime::ToolResult::Err(
                                    models::runtime::ToolError {
                                        reason: "cancelled".to_string(),
                                    },
                                ),
                            }),
                        );
                        if let Ok(json) = response {
                            let _ = sink.lock().await.send(Message::Text(json.into())).await;
                        }
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
}
```

- [ ] **Step 2: Build the binary**

```bash
cargo build -p runtime
```

Expected: `target/debug/october-runtime` is produced.

- [ ] **Step 3: Commit**

```bash
git add runtime/src/main.rs runtime/src/lib.rs runtime/Cargo.toml
git commit -m "feat: october-runtime binary"
```

---

### Task 9: runtime-client crate — scaffold + RuntimeTransport + RuntimeClient

**Files:**
- Create: `runtime-client/Cargo.toml`
- Create: `runtime-client/src/lib.rs`
- Create: `runtime-client/src/transport.rs`
- Create: `runtime-client/src/client.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Add to workspace `Cargo.toml`**

```toml
members = [
    "agentcore",
    "server",
    "executor",
    "runtime",
    "runtime-client",
    "models",
    "providers/anthropic",
    "providers/mock-llm",
    "tests",
]
```

- [ ] **Step 2: Create `runtime-client/Cargo.toml`**

```toml
[package]
name = "runtime-client"
version = "0.1.0"
edition = "2024"

[dependencies]
models      = { path = "../models" }
agentcore   = { path = "../agentcore" }
async-trait = { workspace = true }
thiserror   = { workspace = true }
tokio       = { workspace = true }
tokio-tungstenite = { workspace = true }
futures-util = { workspace = true }
serde_json  = { workspace = true }
uuid        = { workspace = true }

[lints]
workspace = true
```

- [ ] **Step 3: Create `runtime-client/src/transport.rs`**

```rust
use async_trait::async_trait;
use models::runtime::{ToolCall, ToolResult};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("disconnected")]
    Disconnected,
}

#[async_trait]
pub trait RuntimeTransport: Send + Sync {
    async fn invoke(
        &self,
        call_id: &str,
        call: ToolCall,
    ) -> Result<ToolResult, TransportError>;

    async fn cancel(&self, call_id: &str) -> Result<(), TransportError>;
}

/// Mock transport for tests — returns configurable canned results.
pub struct MockTransport {
    result: ToolResult,
}

impl MockTransport {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            result: ToolResult::Ok(models::runtime::ToolOutput {
                stdout: stdout.into(),
                stderr: String::new(),
                exit_code: 0,
            }),
        }
    }

    pub fn err(reason: impl Into<String>) -> Self {
        Self {
            result: ToolResult::Err(models::runtime::ToolError {
                reason: reason.into(),
            }),
        }
    }
}

#[async_trait]
impl RuntimeTransport for MockTransport {
    async fn invoke(&self, _call_id: &str, _call: ToolCall) -> Result<ToolResult, TransportError> {
        Ok(self.result.clone())
    }

    async fn cancel(&self, _call_id: &str) -> Result<(), TransportError> {
        Ok(())
    }
}
```

- [ ] **Step 4: Create `runtime-client/src/client.rs`**

```rust
use crate::transport::{RuntimeTransport, TransportError};
use models::runtime::{ToolCall, ToolError, ToolOutput, ToolResult};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug)]
pub enum RuntimeCallError {
    Transport(TransportError),
    ToolFailed(String),
}

impl std::fmt::Display for RuntimeCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::ToolFailed(r) => write!(f, "tool failed: {r}"),
        }
    }
}

impl std::error::Error for RuntimeCallError {}

/// Client handle for invoking tools on a remote runtime.
/// Cheap to clone — Arc-backed.
#[derive(Clone)]
pub struct RuntimeClient {
    inner: Arc<dyn RuntimeTransport>,
}

impl RuntimeClient {
    pub fn new(transport: impl RuntimeTransport + 'static) -> Self {
        Self {
            inner: Arc::new(transport),
        }
    }

    pub async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        match self.inner.invoke(&call_id, call).await {
            Ok(ToolResult::Ok(output)) => Ok(output),
            Ok(ToolResult::Err(ToolError { reason })) => {
                Err(RuntimeCallError::ToolFailed(reason))
            }
            Err(e) => Err(RuntimeCallError::Transport(e)),
        }
    }

    pub async fn cancel(&self, call_id: &str) {
        let _ = self.inner.cancel(call_id).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use crate::transport::MockTransport;
    use models::runtime::BashInput;

    #[tokio::test]
    async fn client_returns_ok_output() {
        let client = RuntimeClient::new(MockTransport::ok("hello"));
        let output = client
            .invoke(ToolCall::Bash(BashInput { command: "echo hello".into() }))
            .await
            .unwrap();
        assert_eq!(output.stdout, "hello");
    }

    #[tokio::test]
    async fn client_returns_err_on_tool_failure() {
        let client = RuntimeClient::new(MockTransport::err("oops"));
        let err = client
            .invoke(ToolCall::Bash(BashInput { command: "bad".into() }))
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeCallError::ToolFailed(_)));
    }
}
```

- [ ] **Step 5: Create `runtime-client/src/lib.rs`**

```rust
mod client;
mod transport;
mod ws_transport;
pub mod tools;

pub use client::{RuntimeCallError, RuntimeClient};
pub use transport::{MockTransport, RuntimeTransport, TransportError};
pub use ws_transport::ExecutorWsTransport;
pub use tools::add_runtime_tools;
```

(Note: `ws_transport` and `tools` are stubs — they'll be filled in Tasks 10 and 11. Create empty files for now.)

Create `runtime-client/src/ws_transport.rs`:
```rust
// filled in Task 10
```

Create `runtime-client/src/tools/mod.rs`:
```rust
// filled in Task 11
```

- [ ] **Step 6: Run tests**

```bash
cargo test -p runtime-client
```

Expected: `client_returns_ok_output` and `client_returns_err_on_tool_failure` pass.

- [ ] **Step 7: Commit**

```bash
git add runtime-client/
git commit -m "feat: runtime-client crate with RuntimeClient + MockTransport"
```

---

### Task 10: runtime-client — ExecutorWsTransport

**Files:**
- Modify: `runtime-client/src/ws_transport.rs`

- [ ] **Step 1: Replace `runtime-client/src/ws_transport.rs`**

```rust
use crate::transport::{RuntimeTransport, TransportError};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use models::executor::{
    CancelToolCallCmd, ExecutorCommand, ExecutorEvent, ExecutorInboundMessage,
    ExecutorOutboundMessage, ToolCallCmd,
};
use models::runtime::{ToolCall, ToolCallRequest, ToolResult};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use uuid::Uuid;

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<ExecutorEvent>>>>;

/// Server-side WS transport. Wraps the connection the executor dials into.
/// Translates RuntimeTransport::invoke into ToolCallCmd + ToolResultEvent correlation.
pub struct ExecutorWsTransport {
    runtime_id: String,
    sender: Arc<Mutex<futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>>>,
    pending: Pending,
}

impl ExecutorWsTransport {
    /// Wrap an already-accepted WebSocket connection.
    /// Spawns a reader task that routes ToolResultEvents to pending callers.
    pub fn new(
        runtime_id: String,
        ws: WebSocketStream<TcpStream>,
    ) -> Self {
        let (sink, mut stream) = ws.split();
        let sender = Arc::new(Mutex::new(sink));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();

        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                if let Ok(msg) = serde_json::from_str::<ExecutorOutboundMessage>(&text) {
                    if let Some(tx) = pending_clone.lock().await.get(&msg.request_id) {
                        let _ = tx.send(msg.event).await;
                    }
                }
            }
        });

        Self { runtime_id, sender, pending }
    }

    async fn send_cmd(&self, request_id: &str, cmd: ExecutorCommand) -> Result<(), TransportError> {
        let msg = ExecutorInboundMessage {
            request_id: request_id.to_string(),
            command: cmd,
        };
        let json = serde_json::to_string(&msg)
            .map_err(|e| TransportError::Serialization(e.to_string()))?;
        self.sender
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| TransportError::SendFailed(e.to_string()))
    }
}

#[async_trait]
impl RuntimeTransport for ExecutorWsTransport {
    async fn invoke(&self, call_id: &str, call: ToolCall) -> Result<ToolResult, TransportError> {
        let (tx, mut rx) = mpsc::channel(4);
        self.pending.lock().await.insert(call_id.to_string(), tx);

        self.send_cmd(
            call_id,
            ExecutorCommand::ToolCall(ToolCallCmd {
                runtime_id: self.runtime_id.clone(),
                call: ToolCallRequest {
                    call_id: call_id.to_string(),
                    call,
                },
            }),
        )
        .await?;

        loop {
            match rx.recv().await {
                Some(ExecutorEvent::ToolResult(ev)) if ev.call_id == call_id => {
                    self.pending.lock().await.remove(call_id);
                    return Ok(ev.result);
                }
                Some(_) => continue,
                None => {
                    self.pending.lock().await.remove(call_id);
                    return Err(TransportError::Disconnected);
                }
            }
        }
    }

    async fn cancel(&self, call_id: &str) -> Result<(), TransportError> {
        let request_id = Uuid::new_v4().to_string();
        self.send_cmd(
            &request_id,
            ExecutorCommand::CancelToolCall(CancelToolCallCmd {
                runtime_id: self.runtime_id.clone(),
                call_id: call_id.to_string(),
            }),
        )
        .await
    }
}
```

- [ ] **Step 2: Build**

```bash
cargo build -p runtime-client
```

- [ ] **Step 3: Commit**

```bash
git add runtime-client/src/ws_transport.rs
git commit -m "feat: ExecutorWsTransport"
```

---

### Task 11: runtime-client — individual tools + add_runtime_tools

**Files:**
- Create: `runtime-client/src/tools/mod.rs`
- Create: `runtime-client/src/tools/bash.rs` (and one per tool)

- [ ] **Step 1: Create `runtime-client/src/tools/bash.rs`**

```rust
use crate::client::{RuntimeCallError, RuntimeClient};
use agentcore::{Tool, ToolCallError, ToolSpec};
use async_trait::async_trait;
use models::runtime::{BashInput, ToolCall};
use serde_json::{Value, json};

pub struct BashTool {
    client: RuntimeClient,
}

impl BashTool {
    pub fn new(client: RuntimeClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".to_string(),
            description: "Execute a bash command in the runtime's working directory.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let command = input["command"]
            .as_str()
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'command'".into()))?
            .to_string();
        self.client
            .invoke(ToolCall::Bash(BashInput { command }))
            .await
            .map(|o| Value::String(o.stdout))
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
    }
}
```

Repeat the same pattern for each remaining tool. Each tool has:
- `spec()` returning a `ToolSpec` with appropriate name, description, and JSON schema
- `execute()` parsing input JSON, calling `client.invoke(ToolCall::XInput(...))`, returning `Value::String(stdout)`

- [ ] **Step 2: Create `runtime-client/src/tools/read_file.rs`**

```rust
use crate::client::{RuntimeCallError, RuntimeClient};
use agentcore::{Tool, ToolCallError, ToolSpec};
use async_trait::async_trait;
use models::runtime::{ReadFileInput, ToolCall};
use serde_json::{Value, json};

pub struct ReadFileTool { client: RuntimeClient }
impl ReadFileTool { pub fn new(client: RuntimeClient) -> Self { Self { client } } }

#[async_trait]
impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".to_string(),
            description: "Read file contents, optionally limited to a 1-based line range.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" }
                },
                "required": ["path"]
            }),
        }
    }
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let path = input["path"].as_str().ok_or_else(|| ToolCallError::InvalidInput("missing 'path'".into()))?.to_string();
        let start_line = input["start_line"].as_u64();
        let end_line = input["end_line"].as_u64();
        self.client.invoke(ToolCall::ReadFile(ReadFileInput { path, start_line, end_line }))
            .await.map(|o| Value::String(o.stdout))
            .map_err(|e: RuntimeCallError| ToolCallError::ExecutionFailed(e.to_string()))
    }
}
```

- [ ] **Step 3: Create remaining tool files**

Create `write_file.rs`, `edit_file.rs`, `replace_in_file.rs`, `list_files.rs`, `glob.rs`, `grep.rs` following the same pattern. Each maps JSON input fields to the corresponding `models::runtime::*Input` struct.

`write_file.rs` input: `path: String`, `content: String`  
`edit_file.rs` input: `path: String`, `old_text: String`, `new_text: String`  
`replace_in_file.rs` input: `path: String`, `replacement: String`, plus either `regex: String` or `start_line + end_line: u64` — build `ReplaceMode` accordingly  
`list_files.rs` input: `path: String`  
`glob.rs` input: `pattern: String`, optional `path: String`, optional `max_results: u64`  
`grep.rs` input: `pattern: String`, optional `path: String`, optional `file_pattern: String`, optional `max_results: u64`

- [ ] **Step 4: Create `runtime-client/src/tools/mod.rs`**

```rust
mod bash;
mod edit_file;
mod glob;
mod grep;
mod list_files;
mod read_file;
mod replace_in_file;
mod write_file;

pub use bash::BashTool;
pub use edit_file::EditFileTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_files::ListFilesTool;
pub use read_file::ReadFileTool;
pub use replace_in_file::ReplaceInFileTool;
pub use write_file::WriteFileTool;

use crate::client::RuntimeClient;
use agentcore::ToolboxImpl;

/// Add all runtime-backed tools to an existing ToolboxImpl.
pub fn add_runtime_tools(toolbox: ToolboxImpl, client: RuntimeClient) -> ToolboxImpl {
    toolbox
        .add(BashTool::new(client.clone()))
        .add(ReadFileTool::new(client.clone()))
        .add(WriteFileTool::new(client.clone()))
        .add(EditFileTool::new(client.clone()))
        .add(ReplaceInFileTool::new(client.clone()))
        .add(ListFilesTool::new(client.clone()))
        .add(GlobTool::new(client.clone()))
        .add(GrepTool::new(client))
}
```

- [ ] **Step 5: Add `ToolCallError::ExecutionFailed` variant to agentcore if missing**

Check `agentcore/src/error.rs`. If `ExecutionFailed` doesn't exist, add it:
```rust
#[error("execution failed: {0}")]
ExecutionFailed(String),
```

- [ ] **Step 6: Build and test**

```bash
cargo build -p runtime-client && cargo test -p runtime-client
```

- [ ] **Step 7: Commit**

```bash
git add runtime-client/src/tools/ runtime-client/src/lib.rs
git commit -m "feat: runtime-client tools + add_runtime_tools"
```

---

### Task 12: server — ExecutorClient + WsExecutorTransport

**Files:**
- Create: `server/src/executor_client.rs`
- Modify: `server/src/lib.rs`
- Modify: `server/Cargo.toml`

- [ ] **Step 1: Update `server/Cargo.toml`**

Add:
```toml
runtime-client = { path = "../runtime-client" }
uuid           = { workspace = true }
```

- [ ] **Step 2: Create `server/src/executor_client.rs`**

```rust
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use models::executor::{
    CancelToolCallCmd, CreateRuntimeCmd, DestroyRuntimeCmd, ExecutorCommand, ExecutorEvent,
    ExecutorInboundMessage, ExecutorOutboundMessage, RuntimeConfig, ToolCallCmd,
};
use models::runtime::{ToolCall, ToolCallRequest, ToolOutput, ToolResult};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("tool failed: {0}")]
    ToolFailed(String),
    #[error("disconnected")]
    Disconnected,
}

#[async_trait]
pub trait ExecutorTransport: Send + Sync {
    /// Send a command; returns a receiver that yields all events with this request_id.
    async fn send(
        &self,
        request_id: &str,
        cmd: ExecutorCommand,
    ) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError>;
}

/// Typed client interface to a connected executor.
pub struct ExecutorClient {
    transport: Arc<dyn ExecutorTransport>,
}

impl ExecutorClient {
    pub fn new(transport: impl ExecutorTransport + 'static) -> Self {
        Self {
            transport: Arc::new(transport),
        }
    }

    pub async fn create_runtime(
        &self,
        id: &str,
        config: RuntimeConfig,
    ) -> Result<(), ClientError> {
        let req = Uuid::new_v4().to_string();
        let mut rx = self
            .transport
            .send(
                &req,
                ExecutorCommand::CreateRuntime(CreateRuntimeCmd {
                    runtime_id: id.to_string(),
                    config,
                }),
            )
            .await?;
        loop {
            match rx.recv().await {
                Some(ExecutorEvent::RuntimeStateChanged(e))
                    if e.state == models::executor::RuntimeState::Running =>
                {
                    return Ok(())
                }
                Some(ExecutorEvent::CommandFailed(e)) => {
                    return Err(ClientError::CommandFailed(e.message))
                }
                Some(_) => continue,
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    pub async fn destroy_runtime(&self, id: &str) -> Result<(), ClientError> {
        let req = Uuid::new_v4().to_string();
        let mut rx = self
            .transport
            .send(
                &req,
                ExecutorCommand::DestroyRuntime(DestroyRuntimeCmd {
                    runtime_id: id.to_string(),
                }),
            )
            .await?;
        loop {
            match rx.recv().await {
                Some(ExecutorEvent::RuntimeStateChanged(e))
                    if e.state == models::executor::RuntimeState::Stopped =>
                {
                    return Ok(())
                }
                Some(ExecutorEvent::CommandFailed(e)) => {
                    return Err(ClientError::CommandFailed(e.message))
                }
                Some(_) => continue,
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    pub async fn invoke_tool(
        &self,
        runtime_id: &str,
        call: ToolCall,
    ) -> Result<ToolOutput, ClientError> {
        let call_id = Uuid::new_v4().to_string();
        let mut rx = self
            .transport
            .send(
                &call_id,
                ExecutorCommand::ToolCall(ToolCallCmd {
                    runtime_id: runtime_id.to_string(),
                    call: ToolCallRequest {
                        call_id: call_id.clone(),
                        call,
                    },
                }),
            )
            .await?;
        loop {
            match rx.recv().await {
                Some(ExecutorEvent::ToolResult(ev)) if ev.call_id == call_id => {
                    return match ev.result {
                        ToolResult::Ok(o) => Ok(o),
                        ToolResult::Err(e) => Err(ClientError::ToolFailed(e.reason)),
                    }
                }
                Some(ExecutorEvent::CommandFailed(e)) => {
                    return Err(ClientError::CommandFailed(e.message))
                }
                Some(_) => continue,
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    pub async fn cancel_tool_call(
        &self,
        runtime_id: &str,
        call_id: &str,
    ) -> Result<(), ClientError> {
        let req = Uuid::new_v4().to_string();
        let _rx = self
            .transport
            .send(
                &req,
                ExecutorCommand::CancelToolCall(CancelToolCallCmd {
                    runtime_id: runtime_id.to_string(),
                    call_id: call_id.to_string(),
                }),
            )
            .await?;
        // Fire and forget — the result comes back as a ToolResult on the original call
        Ok(())
    }
}

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<ExecutorEvent>>>>;

/// WS transport wrapping the server side of an executor connection.
pub struct WsExecutorTransport {
    sender: Arc<Mutex<futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>>>,
    pending: Pending,
}

impl WsExecutorTransport {
    /// Wrap an accepted WebSocket stream (server side).
    /// Consumes the first Registered event, then routes subsequent events by request_id.
    pub async fn accept(ws: WebSocketStream<TcpStream>) -> Self {
        let (sink, mut stream) = ws.split();
        let sender = Arc::new(Mutex::new(sink));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();

        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                if let Ok(msg) = serde_json::from_str::<ExecutorOutboundMessage>(&text) {
                    // Skip Registered event
                    if matches!(msg.event, ExecutorEvent::Registered(_)) {
                        continue;
                    }
                    if let Some(tx) = pending_clone.lock().await.get(&msg.request_id) {
                        let _ = tx.send(msg.event).await;
                    }
                }
            }
        });

        Self { sender, pending }
    }
}

#[async_trait]
impl ExecutorTransport for WsExecutorTransport {
    async fn send(
        &self,
        request_id: &str,
        cmd: ExecutorCommand,
    ) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError> {
        let (tx, rx) = mpsc::channel(16);
        self.pending.lock().await.insert(request_id.to_string(), tx);

        let msg = ExecutorInboundMessage {
            request_id: request_id.to_string(),
            command: cmd,
        };
        let json = serde_json::to_string(&msg)
            .map_err(|e| ClientError::Serialization(e.to_string()))?;
        self.sender
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| ClientError::SendFailed(e.to_string()))?;

        Ok(rx)
    }
}
```

- [ ] **Step 3: Update `server/src/lib.rs`**

Add:
```rust
mod executor_client;
pub use executor_client::{ClientError, ExecutorClient, ExecutorTransport, WsExecutorTransport};
```

- [ ] **Step 4: Build**

```bash
cargo build -p server
```

- [ ] **Step 5: Commit**

```bash
git add server/src/executor_client.rs server/src/lib.rs server/Cargo.toml
git commit -m "feat: ExecutorClient + WsExecutorTransport"
```

---

### Task 13: Final build + push

- [ ] **Step 1: Full workspace build and test**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --workspace
```

Fix any warnings or test failures before continuing.

- [ ] **Step 2: Push**

```bash
git push
```
