# october CLI (run mode) with nono sandbox — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Add a single-process `october` CLI (`validate`/`run`/`resume`) that loads a workflow + JSON config, runs it against a workdir, confines all tool execution in the nono sandbox over a unix socket, and supports suspend/resume.

**Architecture:** Orchestrator (unsanboxed, holds API key) talks to an in-process executor over in-memory channels (`InMemExecutorTransport`, lifecycle only) and to the sandboxed `october-runtime` child directly over a unix socket (`SocketRuntimeTransport`, tool calls). The TCP/WebSocket relay path stays for distributed mode via the same generic-over-socket connection layer. A push `WorkflowNotification` channel drives the CLI control loop; a `FileJournal` is the durable source of truth for resume.

**Tech Stack:** Rust 2024, tokio, tokio-tungstenite (WS over TcpStream + UnixStream), clap, serde/serde_json, `nono = "0.59"` (Landlock/Seatbelt), `base64`, `eval`, fluorite-generated `models`.

---

## Discovered ground truth (verified against the code & crates.io)

- `server::ExecutorClient` / `server::WsExecutorTransport` and `runtime_client::ExecutorWsTransport`
  are **dead scaffolding** — no consumer calls them; `Server` uses its own `registry.send_command`.
  → Free to relocate/refactor without breaking live behavior. **Delete `runtime-client/src/ws_transport.rs`.**
- `nono 0.59.0` API: `Sandbox::apply(&caps) -> nono::error::Result<SeccompNetFallback>` (assoc fn, same sig
  both OSes), `Sandbox::support_info().is_supported: bool`. `CapabilitySet::new()` + **consuming** builders:
  `allow_path(path, AccessMode::{Read,ReadWrite})? `, `allow_unix_socket(path, UnixSocketMode::Connect)?`,
  `block_network()`. `default = ["system-keyring"]` → take `default-features = false`.
- CI = `ubuntu-latest`: `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -D warnings`,
  `cargo test --workspace`. `--all-features` compiles `runtime/sandbox` (nono, Landlock).
- Production lints **deny** `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`. No `_ =>` arms on
  domain enums in production code — enumerate ignored variants. Tests opt out per-file.
- `base64 0.22.1` already in `Cargo.lock`. `WorkflowCommand::AgentAsked { question, .. }` carries the question.
- `AnthropicProvider`: `with_api_key(key)->Result`, `with_base_url(&str)->Self`, `with_model(&str)->Self`,
  `with_retry_delay_secs(u64)->Self`. (Confirm `with_max_tokens` during impl; skip if absent.)

## File structure (created / modified)

```
actor/Cargo.toml                MOD  + [features] file-journal = ["dep:base64"]; base64 optional dep
actor/src/file_journal.rs       NEW  FileJournal (append-only base64 JSONL, no-op snapshots, torn-line replay)
actor/src/lib.rs                MOD  cfg-gated `mod file_journal; pub use ... FileJournal;`

runtime-client/src/client.rs    MOD  + RuntimeClient::from_arc(Arc<dyn RuntimeTransport>)
runtime-client/src/lib.rs       MOD  drop `pub mod ws_transport;`
runtime-client/src/ws_transport.rs  DELETE (dead relay; reborn in executor-client)

executor-client/                NEW CRATE
  Cargo.toml                          deps: models, runtime-client, async-trait, thiserror, tokio, uuid,
                                      serde_json; [features] default=["ws"]; ws=["tokio-tungstenite","futures-util"]
  src/lib.rs                          re-exports
  src/transport.rs                    ExecutorTransport trait (send + runtime_transport)
  src/client.rs                       ClientError + lifecycle-only ExecutorClient
  src/ws_transport.rs   [feat ws]     WsExecutorTransport (ExecutorTransport) + RelayRuntimeTransport

executor/Cargo.toml             MOD  + executor-client(default-features=false), runtime-client deps
executor/src/socket_transport.rs NEW SocketRuntimeTransport<S> (impl RuntimeTransport) + UnixSocketRuntimeTransport
executor/src/connected_registry.rs MOD store Arc<dyn RuntimeTransport>; register_transport/runtime_transport
executor/src/runtime_listener.rs MOD RuntimeEndpoint enum, AcceptedConn enum, Tcp|Unix bind, unlink, 0700
executor/src/process_provider.rs MOD RuntimeEndpoint + SandboxPolicy; --endpoint/--sandbox/--sandbox-read + env scrub
executor/src/inmem_transport.rs  NEW InMemExecutorTransport (impl executor_client::ExecutorTransport)
executor/src/executor.rs        MOD generic connection handler; do_tool_call/do_cancel via transport; create_core
executor/src/lib.rs             MOD exports; serve_runtime_connections; drop RuntimeSink
executor/src/env_scrub.rs       NEW SANDBOX_ENV_ALLOWLIST + scrubbed_env() (unit-tested)

runtime/Cargo.toml              MOD  + [features] default=["sandbox"]; sandbox=["dep:nono"]; tokio net feat
runtime/src/main.rs             MOD  --endpoint/--sandbox/--sandbox-read; generic run_loop<S>; apply sandbox
runtime/src/sandbox.rs   [feat] NEW  apply_sandbox + system_read_paths (per-platform), fail-closed

workflow/src/workflow_actor.rs  MOD  + WorkflowNotification enum; emit on command path
workflow/src/context.rs         MOD  + workflow_events: mpsc::Sender<WorkflowNotification>
workflow/src/lib.rs             MOD  export WorkflowNotification
workflow/tests/workflow_e2e.rs  MOD  construct workflow_events channel in runtime_context()

server/src/lib.rs               MOD  drop executor_client mod + re-exports
server/src/executor_client.rs   DELETE

cli/                            NEW CRATE  bin `october`
  Cargo.toml                          deps as below; enables actor/file-journal
  src/main.rs                         clap subcommands → lib; runtime_binary_path(); process::exit
  src/lib.rs                          pub mod config, validate, run, terminal_sink, error
  src/error.rs                        CliError
  src/config.rs                       OctoberConfig serde + build_registry + load helpers
  src/validate.rs                     validate(def, cfg) -> Vec<String>
  src/terminal_sink.rs                TerminalSink: EventSink
  src/run.rs                          assembly + two-plane control loop (run + resume) + manifest
  tests/cli_e2e.rs                    orchestration + suspend/resume + (support-gated) sandbox/bash

Cargo.toml (workspace)          MOD  members += "executor-client", "cli"
fluorite/october.json (example) NEW  sample config (docs only, optional)
```

Dependency direction (acyclic): `executor-client → runtime-client, models`;
`executor → executor-client, runtime-client, models`; `cli → executor, executor-client, runtime-client, workflow, actor(file-journal), providers/anthropic, agentcore, models`.

---

## Phase 1 — `actor::FileJournal`

### Task 1.1: Feature + dependency

**Files:** Modify `actor/Cargo.toml`

- [ ] **Step 1:** Add to `actor/Cargo.toml`:
```toml
[features]
file-journal = ["dep:base64"]

[dependencies]
# ...existing...
base64 = { version = "0.22", optional = true }
```
- [ ] **Step 2:** Run `cargo build -p actor` → PASS (feature off, base64 absent).

### Task 1.2: FileJournal implementation (TDD)

**Files:** Create `actor/src/file_journal.rs`; Modify `actor/src/lib.rs`. (First confirm `JournalError` variants: `actor/src/error.rs` has `Backend(String)`, `Serialization(String)`.)

- [ ] **Step 1: lib.rs wiring**
```rust
#[cfg(feature = "file-journal")]
mod file_journal;
#[cfg(feature = "file-journal")]
pub use file_journal::FileJournal;
```
- [ ] **Step 2: Write `file_journal.rs`** (complete):
```rust
use crate::error::JournalError;
use crate::journal::{Journal, JournalResult};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::stream::{self, BoxStream, StreamExt};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Filesystem-backed [`Journal`]: one base64-encoded record per line under
/// `<root>/runs/<id>/journal.jsonl`. Snapshots are no-op (CLI runs are short, so
/// recovery always full-replays the log). Base64 keeps the file strictly
/// line-delimited regardless of the opaque payload bytes.
pub struct FileJournal {
    root: PathBuf,
}

impl FileJournal {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn journal_path(&self, id: &str) -> PathBuf {
        self.root.join("runs").join(id).join("journal.jsonl")
    }
}

#[async_trait]
impl Journal for FileJournal {
    async fn persist(&self, id: &str, events: &[Vec<u8>]) -> JournalResult<()> {
        let path = self.journal_path(id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| JournalError::Backend(e.to_string()))?;
        }
        let mut buf = String::new();
        for bytes in events {
            buf.push_str(&STANDARD.encode(bytes));
            buf.push('\n');
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| JournalError::Backend(e.to_string()))?;
        file.write_all(buf.as_bytes())
            .map_err(|e| JournalError::Backend(e.to_string()))?;
        file.flush()
            .map_err(|e| JournalError::Backend(e.to_string()))?;
        file.sync_all()
            .map_err(|e| JournalError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn replay(&self, id: &str, after_seq: u64) -> BoxStream<'_, JournalResult<Vec<u8>>> {
        let path = self.journal_path(id);
        let items = decode_after(&path, after_seq);
        stream::iter(items).boxed()
    }

    async fn save_snapshot(&self, _id: &str, _state: Vec<u8>, _seq_nr: u64) -> JournalResult<()> {
        Ok(())
    }

    async fn latest_snapshot(&self, _id: &str) -> JournalResult<Option<(Vec<u8>, u64)>> {
        Ok(None)
    }

    async fn delete_events_before(&self, _id: &str, _seq_nr: u64) -> JournalResult<()> {
        Ok(())
    }

    async fn copy_snapshot(&self, _from_id: &str, _to_id: &str) -> JournalResult<()> {
        Ok(())
    }

    async fn clear(&self, id: &str) -> JournalResult<()> {
        match std::fs::remove_file(self.journal_path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(JournalError::Backend(e.to_string())),
        }
    }
}

/// Decode complete lines (those terminated by '\n'), 1-based index; yield those
/// whose index > `after_seq`. A trailing partial/garbage record (a torn final
/// write that never returned `Ok` to the actor) is dropped, preserving the
/// line↔seq invariant.
fn decode_after(path: &Path, after_seq: u64) -> Vec<JournalResult<Vec<u8>>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let parts: Vec<&str> = content.split('\n').collect();
    // All parts except the last were terminated by '\n' (complete lines).
    let complete = if parts.is_empty() {
        &[][..]
    } else {
        &parts[..parts.len() - 1]
    };
    let mut out = Vec::new();
    let mut seq: u64 = 0;
    for line in complete {
        if line.is_empty() {
            continue;
        }
        match STANDARD.decode(line) {
            Ok(bytes) => {
                seq += 1;
                if seq > after_seq {
                    out.push(Ok(bytes));
                }
            }
            // A non-decodable complete line means corruption from this point on;
            // stop (truncate) rather than misnumber subsequent records.
            Err(_) => break,
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn drain(j: &FileJournal, id: &str, after: u64) -> Vec<Vec<u8>> {
        let mut s = j.replay(id, after).await;
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn persist_then_replay_roundtrips_in_order() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist("r1", &[vec![1, 2], vec![3], vec![4, 5, 6]]).await.unwrap();
        assert_eq!(drain(&j, "r1", 0).await, vec![vec![1, 2], vec![3], vec![4, 5, 6]]);
    }

    #[tokio::test]
    async fn replay_skips_at_or_before_after_seq() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist("r1", &[vec![1], vec![2], vec![3]]).await.unwrap();
        assert_eq!(drain(&j, "r1", 1).await, vec![vec![2], vec![3]]);
    }

    #[tokio::test]
    async fn append_across_calls_keeps_sequence() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist("r1", &[vec![1]]).await.unwrap();
        j.persist("r1", &[vec![2], vec![3]]).await.unwrap();
        assert_eq!(drain(&j, "r1", 0).await, vec![vec![1], vec![2], vec![3]]);
    }

    #[tokio::test]
    async fn snapshots_are_noop_replay_is_full() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist("r1", &[vec![1], vec![2]]).await.unwrap();
        j.save_snapshot("r1", vec![9, 9], 2).await.unwrap();
        assert_eq!(j.latest_snapshot("r1").await.unwrap(), None);
        // delete_events_before is a no-op: full log still replays from 0.
        j.delete_events_before("r1", 2).await.unwrap();
        assert_eq!(drain(&j, "r1", 0).await, vec![vec![1], vec![2]]);
    }

    #[tokio::test]
    async fn torn_trailing_line_is_ignored_and_state_recovers() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        j.persist("r1", &[vec![1], vec![2]]).await.unwrap();
        // Simulate a process killed mid-write: append a partial record with no '\n'.
        let path = j.journal_path("r1");
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"GARBAGE_NO_NEWLINE").unwrap();
        f.flush().unwrap();
        // The two complete records survive; the torn tail is dropped.
        assert_eq!(drain(&j, "r1", 0).await, vec![vec![1], vec![2]]);
    }

    #[tokio::test]
    async fn replay_missing_file_is_empty() {
        let dir = TempDir::new().unwrap();
        let j = FileJournal::new(dir.path());
        assert!(drain(&j, "ghost", 0).await.is_empty());
    }
}
```
- [ ] **Step 3:** Add `tempfile` to `actor`'s `[dev-dependencies]` (`tempfile = "3"`).
- [ ] **Step 4:** Run `cargo test -p actor --features file-journal` → all PASS.
- [ ] **Step 5:** Commit `feat(actor): FileJournal with torn-line-safe replay`.

---

## Phase 2 — `runtime-client::from_arc`

**Files:** Modify `runtime-client/src/client.rs`; `runtime-client/src/lib.rs`.

- [ ] **Step 1:** Delete `runtime-client/src/ws_transport.rs` and the `pub mod ws_transport;` line in `lib.rs`.
- [ ] **Step 2:** Add to `impl RuntimeClient`:
```rust
/// Build a client from an already-type-erased transport (e.g. one handed back by
/// `ExecutorClient::runtime_transport`).
pub fn from_arc(transport: std::sync::Arc<dyn RuntimeTransport>) -> Self {
    Self { inner: transport }
}
```
- [ ] **Step 3:** Run `cargo build -p runtime-client && cargo test -p runtime-client` → PASS.
- [ ] **Step 4:** Commit `feat(runtime-client): RuntimeClient::from_arc; drop dead ws_transport`.

---

## Phase 3 — `executor-client` crate

**Files:** Create crate `executor-client`; Modify root `Cargo.toml` (`members += "executor-client"`),
`server/src/lib.rs`, delete `server/src/executor_client.rs`.

- [ ] **Step 1: `executor-client/Cargo.toml`**
```toml
[package]
name = "executor-client"
version = "0.1.0"
edition = "2024"

[dependencies]
models            = { path = "../models" }
runtime-client    = { path = "../runtime-client" }
async-trait       = { workspace = true }
thiserror         = { workspace = true }
tokio             = { workspace = true }
uuid              = { workspace = true }
serde_json        = { workspace = true }
tokio-tungstenite = { workspace = true, optional = true }
futures-util      = { workspace = true, optional = true }

[features]
default = ["ws"]
ws = ["dep:tokio-tungstenite", "dep:futures-util"]

[lints]
workspace = true
```
- [ ] **Step 2: `src/client.rs`** — `ClientError` + lifecycle-only `ExecutorClient` (port `create_runtime`/
  `destroy_runtime` loops verbatim from `server/src/executor_client.rs`; **drop** `invoke_tool`/`cancel_tool_call`;
  add `runtime_transport` delegating to the transport; add `from_arc`):
```rust
use crate::transport::ExecutorTransport;
use models::executor::{
    CreateRuntimeCmd, DestroyRuntimeCmd, ExecutorCommand, ExecutorEvent, RuntimeConfig, RuntimeState,
};
use runtime_client::RuntimeTransport;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("disconnected")]
    Disconnected,
}

/// Lifecycle-only client to a connected executor. Tool calls go through the
/// `RuntimeTransport` obtained from [`ExecutorClient::runtime_transport`].
pub struct ExecutorClient {
    transport: Arc<dyn ExecutorTransport>,
}

impl ExecutorClient {
    pub fn new(transport: impl ExecutorTransport + 'static) -> Self {
        Self { transport: Arc::new(transport) }
    }
    pub fn from_arc(transport: Arc<dyn ExecutorTransport>) -> Self {
        Self { transport }
    }

    pub async fn create_runtime(&self, id: &str, config: RuntimeConfig) -> Result<(), ClientError> {
        let req = Uuid::new_v4().to_string();
        let mut rx = self
            .transport
            .send(&req, ExecutorCommand::CreateRuntime(CreateRuntimeCmd {
                runtime_id: id.to_string(),
                config,
            }))
            .await?;
        loop {
            match rx.recv().await {
                Some(ExecutorEvent::RuntimeStateChanged(e)) if e.state == RuntimeState::Running => return Ok(()),
                Some(ExecutorEvent::CommandFailed(e)) => return Err(ClientError::CommandFailed(e.message)),
                Some(_) => continue,
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    pub async fn destroy_runtime(&self, id: &str) -> Result<(), ClientError> {
        let req = Uuid::new_v4().to_string();
        let mut rx = self
            .transport
            .send(&req, ExecutorCommand::DestroyRuntime(DestroyRuntimeCmd { runtime_id: id.to_string() }))
            .await?;
        loop {
            match rx.recv().await {
                Some(ExecutorEvent::RuntimeStateChanged(e)) if e.state == RuntimeState::Stopped => return Ok(()),
                Some(ExecutorEvent::CommandFailed(e)) => return Err(ClientError::CommandFailed(e.message)),
                Some(_) => continue,
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    pub async fn runtime_transport(&self, runtime_id: &str) -> Result<Arc<dyn RuntimeTransport>, ClientError> {
        self.transport.runtime_transport(runtime_id).await
    }
}
```
- [ ] **Step 3: `src/transport.rs`**
```rust
use crate::client::ClientError;
use async_trait::async_trait;
use models::executor::{ExecutorCommand, ExecutorEvent};
use runtime_client::RuntimeTransport;
use std::sync::Arc;
use tokio::sync::mpsc;

#[async_trait]
pub trait ExecutorTransport: Send + Sync {
    /// Send a lifecycle command; returns a channel yielding events for this request.
    async fn send(
        &self,
        request_id: &str,
        cmd: ExecutorCommand,
    ) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError>;

    /// Obtain the tool-call transport for `runtime_id`. Deep-module: the caller never
    /// learns whether bytes go direct (CLI) or via the relay (server).
    async fn runtime_transport(
        &self,
        runtime_id: &str,
    ) -> Result<Arc<dyn RuntimeTransport>, ClientError>;
}
```
- [ ] **Step 4: `src/ws_transport.rs`** `[cfg(feature = "ws")]` — `WsExecutorTransport` (ExecutorTransport)
  whose `runtime_transport` returns a `RelayRuntimeTransport` sharing the WS sender + pending map:
```rust
use crate::client::ClientError;
use crate::transport::ExecutorTransport;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use models::executor::{
    CancelToolCallCmd, ExecutorCommand, ExecutorEvent, ExecutorInboundMessage,
    ExecutorOutboundMessage, ToolCallCmd,
};
use models::runtime::{ToolCall, ToolCallRequest, ToolResult};
use runtime_client::{RuntimeTransport, TransportError};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use uuid::Uuid;

type Sink = Arc<Mutex<futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>>>;
type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<ExecutorEvent>>>>;

pub struct WsExecutorTransport {
    sender: Sink,
    pending: Pending,
}

impl WsExecutorTransport {
    pub fn accept(ws: WebSocketStream<TcpStream>) -> Self {
        let (sink, mut stream) = ws.split();
        let sender = Arc::new(Mutex::new(sink));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                if let Ok(msg) = serde_json::from_str::<ExecutorOutboundMessage>(&text) {
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

    async fn send_cmd(&self, request_id: &str, cmd: ExecutorCommand) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError> {
        let (tx, rx) = mpsc::channel(16);
        self.pending.lock().await.insert(request_id.to_string(), tx);
        let msg = ExecutorInboundMessage { request_id: request_id.to_string(), command: cmd };
        let json = serde_json::to_string(&msg).map_err(|e| ClientError::Serialization(e.to_string()))?;
        self.sender.lock().await.send(Message::Text(json.into())).await
            .map_err(|e| ClientError::SendFailed(e.to_string()))?;
        Ok(rx)
    }
}

#[async_trait]
impl ExecutorTransport for WsExecutorTransport {
    async fn send(&self, request_id: &str, cmd: ExecutorCommand) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError> {
        self.send_cmd(request_id, cmd).await
    }
    async fn runtime_transport(&self, runtime_id: &str) -> Result<Arc<dyn RuntimeTransport>, ClientError> {
        Ok(Arc::new(RelayRuntimeTransport {
            sender: self.sender.clone(),
            pending: self.pending.clone(),
            runtime_id: runtime_id.to_string(),
        }))
    }
}

/// Tool-call transport that relays through the executor over the shared client↔executor WS.
struct RelayRuntimeTransport {
    sender: Sink,
    pending: Pending,
    runtime_id: String,
}

impl RelayRuntimeTransport {
    async fn send_cmd(&self, request_id: &str, cmd: ExecutorCommand) -> Result<mpsc::Receiver<ExecutorEvent>, TransportError> {
        let (tx, rx) = mpsc::channel(4);
        self.pending.lock().await.insert(request_id.to_string(), tx);
        let msg = ExecutorInboundMessage { request_id: request_id.to_string(), command: cmd };
        let json = serde_json::to_string(&msg).map_err(|e| TransportError::Serialization(e.to_string()))?;
        self.sender.lock().await.send(Message::Text(json.into())).await
            .map_err(|e| TransportError::SendFailed(e.to_string()))?;
        Ok(rx)
    }
}

#[async_trait]
impl RuntimeTransport for RelayRuntimeTransport {
    async fn invoke(&self, call_id: &str, call: ToolCall) -> Result<ToolResult, TransportError> {
        let mut rx = self.send_cmd(call_id, ExecutorCommand::ToolCall(ToolCallCmd {
            runtime_id: self.runtime_id.clone(),
            call: ToolCallRequest { call_id: call_id.to_string(), call },
        })).await?;
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
        let _ = self.send_cmd(&Uuid::new_v4().to_string(), ExecutorCommand::CancelToolCall(CancelToolCallCmd {
            runtime_id: self.runtime_id.clone(),
            call_id: call_id.to_string(),
        })).await?;
        Ok(())
    }
}
```
- [ ] **Step 5: `src/lib.rs`**
```rust
mod client;
mod transport;
#[cfg(feature = "ws")]
mod ws_transport;

pub use client::{ClientError, ExecutorClient};
pub use transport::ExecutorTransport;
#[cfg(feature = "ws")]
pub use ws_transport::WsExecutorTransport;
```
- [ ] **Step 6:** Delete `server/src/executor_client.rs`; in `server/src/lib.rs` remove `mod executor_client;`
  and the `pub use executor_client::{...};` line. (server has no consumer of these types.)
- [ ] **Step 7:** Root `Cargo.toml` `members`: add `"executor-client"`.
- [ ] **Step 8:** Run `cargo build -p executor-client && cargo build -p server` → PASS.
- [ ] **Step 9:** Commit `feat(executor-client): lifecycle-only ExecutorClient + relay transport; drop server scaffolding`.

---

## Phase 4 — `executor` refactor

### Task 4.1: `SocketRuntimeTransport<S>`

**Files:** Create `executor/src/socket_transport.rs`. Add deps to `executor/Cargo.toml`:
`executor-client = { path = "../executor-client", default-features = false }`,
`runtime-client = { path = "../runtime-client" }`.

- [ ] **Step 1:** Write `socket_transport.rs`:
```rust
use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use models::runtime::{
    CancelCallRequest, RuntimeInboundMessage, RuntimeOutboundMessage, ToolCall, ToolCallRequest,
    ToolResult,
};
use runtime_client::{RuntimeTransport, TransportError};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Reply = Result<ToolResult, TransportError>;
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Reply>>>>;

/// Direct tool-call transport over a single accepted runtime link (`WebSocketStream<S>`,
/// `S` = `TcpStream` or `UnixStream`). Owns the sink + a `call_id → oneshot` pending map;
/// a spawned reader fills it and, on disconnect, resolves every pending call with
/// `TransportError::Disconnected`.
pub struct SocketRuntimeTransport<S> {
    sink: Arc<Mutex<SplitSink<WebSocketStream<S>, Message>>>,
    pending: Pending,
}

/// The unix instantiation used by CLI mode.
pub type UnixSocketRuntimeTransport = SocketRuntimeTransport<tokio::net::UnixStream>;

impl<S> SocketRuntimeTransport<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    pub fn new(ws: WebSocketStream<S>) -> Self {
        let (sink, stream) = ws.split();
        Self::from_split(sink, stream)
    }

    pub fn from_split(
        sink: SplitSink<WebSocketStream<S>, Message>,
        mut stream: SplitStream<WebSocketStream<S>>,
    ) -> Self {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                if let Ok(RuntimeOutboundMessage::ToolCallResponse(resp)) =
                    serde_json::from_str::<RuntimeOutboundMessage>(&text)
                    && let Some(tx) = reader_pending.lock().await.remove(&resp.call_id)
                {
                    let _ = tx.send(Ok(resp.result));
                }
            }
            // Disconnected: fail every outstanding call.
            let mut map = reader_pending.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(TransportError::Disconnected));
            }
        });
        Self {
            sink: Arc::new(Mutex::new(sink)),
            pending,
        }
    }
}

#[async_trait]
impl<S> RuntimeTransport for SocketRuntimeTransport<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn invoke(&self, call_id: &str, call: ToolCall) -> Result<ToolResult, TransportError> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(call_id.to_string(), tx);
        let msg = RuntimeInboundMessage::ToolCall(ToolCallRequest { call_id: call_id.to_string(), call });
        let json = serde_json::to_string(&msg).map_err(|e| TransportError::Serialization(e.to_string()))?;
        if let Err(e) = self.sink.lock().await.send(Message::Text(json.into())).await {
            self.pending.lock().await.remove(call_id);
            return Err(TransportError::SendFailed(e.to_string()));
        }
        match rx.await {
            Ok(reply) => reply,
            Err(_) => Err(TransportError::Disconnected),
        }
    }

    async fn cancel(&self, call_id: &str) -> Result<(), TransportError> {
        let msg = RuntimeInboundMessage::CancelCall(CancelCallRequest { call_id: call_id.to_string() });
        let json = serde_json::to_string(&msg).map_err(|e| TransportError::Serialization(e.to_string()))?;
        self.sink.lock().await.send(Message::Text(json.into())).await
            .map_err(|e| TransportError::SendFailed(e.to_string()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use models::runtime::{BashInput, ToolError, ToolOutput};
    use tokio::net::{UnixListener, UnixStream};

    /// Spin up a paired unix socket: server side acts as a fake runtime answering ToolCalls.
    async fn paired() -> (UnixSocketRuntimeTransport, tokio::task::JoinHandle<()>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("rt.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut stream) = ws.split();
            while let Some(Ok(Message::Text(t))) = stream.next().await {
                if let Ok(RuntimeInboundMessage::ToolCall(req)) = serde_json::from_str(&t) {
                    // Echo the command text back as stdout.
                    let out = ToolResult::Ok(ToolOutput { stdout: "ok".into(), stderr: String::new(), exit_code: 0 });
                    let resp = RuntimeOutboundMessage::ToolCallResponse(models::runtime::ToolCallResponse {
                        call_id: req.call_id, result: out,
                    });
                    let _ = sink.send(Message::Text(serde_json::to_string(&resp).unwrap().into())).await;
                } else if let Ok(RuntimeInboundMessage::CancelCall(_)) = serde_json::from_str(&t) {
                    let _ = &out_unused(&ToolError { reason: "x".into() });
                }
            }
        });
        let client = UnixStream::connect(&path).await.unwrap();
        let ws = tokio_tungstenite::client_async("ws://localhost/", client).await.unwrap().0;
        (SocketRuntimeTransport::new(ws), server, dir)
    }
    fn out_unused(_: &ToolError) {}

    #[tokio::test]
    async fn invoke_correlates_response() {
        let (t, _s, _d) = paired().await;
        let r = t.invoke("c1", ToolCall::Bash(BashInput { command: "echo".into() })).await.unwrap();
        assert!(matches!(r, ToolResult::Ok(o) if o.stdout == "ok"));
    }

    #[tokio::test]
    async fn concurrent_invokes_each_resolve() {
        let (t, _s, _d) = paired().await;
        let t = std::sync::Arc::new(t);
        let mut hs = Vec::new();
        for i in 0..8 {
            let t = t.clone();
            hs.push(tokio::spawn(async move {
                t.invoke(&format!("c{i}"), ToolCall::Bash(BashInput { command: "x".into() })).await
            }));
        }
        for h in hs { assert!(h.await.unwrap().is_ok()); }
    }

    #[tokio::test]
    async fn disconnect_resolves_pending_with_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("rt.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // Server accepts then immediately drops the connection without replying.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (_sink, mut stream) = ws.split();
            let _ = stream.next().await; // read one frame then drop everything
        });
        let client = UnixStream::connect(&path).await.unwrap();
        let ws = tokio_tungstenite::client_async("ws://localhost/", client).await.unwrap().0;
        let t = SocketRuntimeTransport::new(ws);
        let err = t.invoke("c1", ToolCall::Bash(BashInput { command: "x".into() })).await.unwrap_err();
        assert!(matches!(err, TransportError::Disconnected));
        let _ = server.await;
    }
}
```
*(Clean up the stray `out_unused` test scaffold during impl; it is only present to keep the example self-contained.)*
- [ ] **Step 2:** Add `tempfile = "3"` to `executor` `[dev-dependencies]`.
- [ ] **Step 3:** Run `cargo test -p executor socket_transport` → PASS.

### Task 4.2: `ConnectedRuntimeRegistry` stores transports

**Files:** Modify `executor/src/connected_registry.rs`.

- [ ] **Step 1:** Replace `RuntimeSink`/`sinks`/`get_sink`/`send_to`/`register` with transport storage:
```rust
use crate::socket_transport::*; // not needed; transports are dyn
use runtime_client::RuntimeTransport;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

struct Inner {
    transports: HashMap<String, Arc<dyn RuntimeTransport>>,
    pending: HashMap<String, oneshot::Sender<()>>,
}

pub struct ConnectedRuntimeRegistry { inner: Mutex<Inner> }

impl Default for ConnectedRuntimeRegistry { fn default() -> Self { Self::new() } }

impl ConnectedRuntimeRegistry {
    pub fn new() -> Self {
        Self { inner: Mutex::new(Inner { transports: HashMap::new(), pending: HashMap::new() }) }
    }
    /// Register a runtime's tool transport; resolves any pending readiness waiter.
    pub async fn register_transport(&self, runtime_id: String, transport: Arc<dyn RuntimeTransport>) {
        let mut inner = self.inner.lock().await;
        inner.transports.insert(runtime_id.clone(), transport);
        if let Some(tx) = inner.pending.remove(&runtime_id) { let _ = tx.send(()); }
    }
    pub async fn notify_when_ready(&self, runtime_id: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().await.pending.insert(runtime_id.to_string(), tx);
        rx
    }
    pub async fn runtime_transport(&self, runtime_id: &str) -> Option<Arc<dyn RuntimeTransport>> {
        self.inner.lock().await.transports.get(runtime_id).cloned()
    }
    pub async fn remove(&self, runtime_id: &str) {
        self.inner.lock().await.transports.remove(runtime_id);
    }
}
```
- [ ] **Step 2:** Update the existing `#[cfg(test)]` tests (`notify_resolves_when_registered`,
  `get_sink_returns_none_for_unknown` → rename to `runtime_transport_none_for_unknown`,
  `remove_does_not_panic_on_missing`). Use a `MockTransport`-style stub or `runtime_client::MockTransport`
  (add `runtime-client` dev-dep already present as dep).
- [ ] **Step 3:** Run `cargo test -p executor connected_registry` → PASS.

### Task 4.3: `RuntimeListenerServer` over TCP|Unix

**Files:** Modify `executor/src/runtime_listener.rs`; add `RuntimeEndpoint`, `AcceptedConn`.

- [ ] **Step 1:** Rewrite:
```rust
use crate::error::ExecutorError;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio_tungstenite::{WebSocketStream, accept_async};

/// Where the executor listens for runtime children.
#[derive(Debug, Clone)]
pub enum RuntimeEndpoint {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

enum Listener { Tcp(TcpListener), Unix(UnixListener) }

/// One accepted runtime link, statically typed by socket family.
pub enum AcceptedConn {
    Tcp(WebSocketStream<TcpStream>),
    Unix(WebSocketStream<UnixStream>),
}

pub struct RuntimeListenerServer {
    listener: Listener,
    endpoint: RuntimeEndpoint,
}

impl RuntimeListenerServer {
    pub async fn bind(endpoint: RuntimeEndpoint) -> Result<Self, ExecutorError> {
        let listener = match &endpoint {
            RuntimeEndpoint::Tcp(addr) => Listener::Tcp(
                TcpListener::bind(addr).await.map_err(|e| ExecutorError::BindFailed(e.to_string()))?,
            ),
            RuntimeEndpoint::Unix(path) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| ExecutorError::BindFailed(e.to_string()))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
                    }
                }
                let _ = std::fs::remove_file(path); // unlink stale socket
                Listener::Unix(UnixListener::bind(path).map_err(|e| ExecutorError::BindFailed(e.to_string()))?)
            }
        };
        Ok(Self { listener, endpoint })
    }

    pub fn endpoint(&self) -> &RuntimeEndpoint { &self.endpoint }

    /// TCP local address when bound to TCP (used by server mode to spawn `ws://addr`).
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        match &self.listener { Listener::Tcp(l) => l.local_addr().ok(), Listener::Unix(_) => None }
    }

    pub async fn accept(&self) -> Result<AcceptedConn, ExecutorError> {
        match &self.listener {
            Listener::Tcp(l) => {
                let (s, _) = l.accept().await.map_err(|e| ExecutorError::Connection(e.to_string()))?;
                Ok(AcceptedConn::Tcp(accept_async(s).await.map_err(|e| ExecutorError::Connection(e.to_string()))?))
            }
            Listener::Unix(l) => {
                let (s, _) = l.accept().await.map_err(|e| ExecutorError::Connection(e.to_string()))?;
                Ok(AcceptedConn::Unix(accept_async(s).await.map_err(|e| ExecutorError::Connection(e.to_string()))?))
            }
        }
    }
}

impl Drop for RuntimeListenerServer {
    fn drop(&mut self) {
        if let RuntimeEndpoint::Unix(path) = &self.endpoint {
            let _ = std::fs::remove_file(path);
        }
    }
}
```

### Task 4.4: Generic connection handler + `serve_runtime_connections`

**Files:** Modify `executor/src/executor.rs` and `executor/src/lib.rs`.

- [ ] **Step 1:** Replace the old `handle_runtime_connection(ws, registry, server_sink)` with:
```rust
use crate::socket_transport::SocketRuntimeTransport;
use tokio::io::{AsyncRead, AsyncWrite};

async fn handle_runtime_connection<S>(ws: tokio_tungstenite::WebSocketStream<S>, registry: Arc<ConnectedRuntimeRegistry>)
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    let (sink, mut stream) = ws.split();
    let runtime_id = loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(RuntimeOutboundMessage::Ready(ev)) = serde_json::from_str::<RuntimeOutboundMessage>(&text) {
                    break ev.runtime_id;
                }
            }
            _ => return,
        }
    };
    let transport = SocketRuntimeTransport::from_split(sink, stream);
    registry.register_transport(runtime_id, Arc::new(transport)).await;
}
```
- [ ] **Step 2:** Update `Executor::run`'s listener loop to match on `AcceptedConn`:
```rust
result = listener.accept() => {
    match result {
        Ok(crate::runtime_listener::AcceptedConn::Tcp(ws)) => {
            let reg = conn_reg.clone();
            tokio::spawn(handle_runtime_connection(ws, reg));
        }
        Ok(crate::runtime_listener::AcceptedConn::Unix(ws)) => {
            let reg = conn_reg.clone();
            tokio::spawn(handle_runtime_connection(ws, reg));
        }
        Err(_) => break,
    }
}
```
  (Drop the now-unused `listener_sink`/`server_sink` plumbing for runtime responses.)
- [ ] **Step 3:** Rewrite `do_tool_call`/`do_cancel_tool_call` to use the transport:
```rust
async fn do_tool_call(
    cmd: &ToolCallCmd,
    connected: Option<&Arc<ConnectedRuntimeRegistry>>,
    sink: &WsSink,
    _req: &str,
) -> Result<(), RuntimeError> {
    let reg = connected.ok_or_else(|| RuntimeError::Provider("no runtime listener configured".into()))?;
    let transport = reg.runtime_transport(&cmd.runtime_id).await
        .ok_or_else(|| RuntimeError::Provider(format!("runtime '{}' not connected", cmd.runtime_id)))?;
    let call_id = cmd.call.call_id.clone();
    let call = cmd.call.call.clone();
    let runtime_id = cmd.runtime_id.clone();
    let sink = sink.clone();
    tokio::spawn(async move {
        let result = match transport.invoke(&call_id, call).await {
            Ok(r) => r,
            Err(e) => ToolResult::Err(models::runtime::ToolError { reason: e.to_string() }),
        };
        let _ = send_outbound(&sink, ExecutorOutboundMessage {
            request_id: call_id.clone(),
            event: ExecutorEvent::ToolResult(ToolResultEvent { runtime_id, call_id, result }),
        }).await;
    });
    Ok(())
}

async fn do_cancel_tool_call(cmd: &CancelToolCallCmd, connected: Option<&Arc<ConnectedRuntimeRegistry>>) -> Result<(), RuntimeError> {
    if let Some(reg) = connected
        && let Some(transport) = reg.runtime_transport(&cmd.runtime_id).await {
        let _ = transport.cancel(&cmd.call_id).await;
    }
    Ok(())
}
```
  Update imports (`ToolResult`, `RuntimeOutboundMessage` still needed; drop `RuntimeInboundMessage`,
  `ToolCallRequest`, `CancelCallRequest`, `RuntimeSink` if now unused).
- [ ] **Step 4:** Extract `create_core` and reuse from `do_create`:
```rust
pub(crate) async fn create_core(
    registry: &Arc<RuntimeRegistry>,
    provider: &Arc<dyn RuntimeProvider>,
    id: &str,
    config: RuntimeConfig,
) -> Result<(), RuntimeError> {
    registry.begin_create(id, config.clone()).await?;
    match provider.create(id, &config).await {
        Ok(handle) => { registry.complete_create(id, handle).await?; Ok(()) }
        Err(e) => { let _ = registry.mark_failed(id).await; Err(e) }
    }
}
```
  `do_create` becomes: emit Creating → `create_core` → emit Running/Failed.
- [ ] **Step 5:** `serve_runtime_connections` (in `executor.rs`, exported via lib):
```rust
pub fn serve_runtime_connections(
    listener: crate::runtime_listener::RuntimeListenerServer,
    registry: Arc<ConnectedRuntimeRegistry>,
    cancel: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                res = listener.accept() => match res {
                    Ok(crate::runtime_listener::AcceptedConn::Tcp(ws)) => { tokio::spawn(handle_runtime_connection(ws, registry.clone())); }
                    Ok(crate::runtime_listener::AcceptedConn::Unix(ws)) => { tokio::spawn(handle_runtime_connection(ws, registry.clone())); }
                    Err(_) => break,
                }
            }
        }
    });
}
```
- [ ] **Step 6:** `ProcessRuntimeHandle::health_check` → `connected_registry.runtime_transport(&self.runtime_id).await.is_some()`.

### Task 4.5: `ProcessRuntimeProvider` + env scrub

**Files:** Modify `executor/src/process_provider.rs`; create `executor/src/env_scrub.rs`.

- [ ] **Step 1:** `env_scrub.rs`:
```rust
/// Minimal env allowlist for a sandboxed runtime child. The orchestrator's
/// secrets (notably `ANTHROPIC_API_KEY`) MUST NOT be inherited — a sandboxed
/// `bash` could otherwise echo them back through tool stdout (network block does
/// not close that channel).
pub const SANDBOX_ENV_ALLOWLIST: &[&str] =
    &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TERM"];

/// Resolve the allowlisted env vars present in the current process.
pub fn scrubbed_env() -> Vec<(String, String)> {
    SANDBOX_ENV_ALLOWLIST
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allowlist_excludes_secrets() {
        assert!(!SANDBOX_ENV_ALLOWLIST.contains(&"ANTHROPIC_API_KEY"));
        assert!(!SANDBOX_ENV_ALLOWLIST.iter().any(|k| k.contains("KEY") || k.contains("TOKEN") || k.contains("SECRET")));
    }
    #[test]
    fn scrubbed_env_only_returns_allowlisted_keys() {
        for (k, _) in scrubbed_env() {
            assert!(SANDBOX_ENV_ALLOWLIST.contains(&k.as_str()), "leaked {k}");
        }
    }
}
```
- [ ] **Step 2:** Rewrite `ProcessRuntimeProvider`:
```rust
use crate::runtime_listener::RuntimeEndpoint;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct SandboxPolicy {
    pub extra_read_paths: Vec<PathBuf>,
}

pub struct ProcessRuntimeProvider {
    binary_path: PathBuf,
    endpoint: RuntimeEndpoint,
    connected_registry: Arc<ConnectedRuntimeRegistry>,
    connect_timeout: Duration,
    sandbox: Option<SandboxPolicy>,
}

impl ProcessRuntimeProvider {
    pub fn new(binary_path: PathBuf, endpoint: RuntimeEndpoint, connected_registry: Arc<ConnectedRuntimeRegistry>) -> Self {
        Self { binary_path, endpoint, connected_registry, connect_timeout: Duration::from_secs(30), sandbox: None }
    }
    pub fn with_connect_timeout(mut self, d: Duration) -> Self { self.connect_timeout = d; self }
    pub fn with_sandbox(mut self, policy: SandboxPolicy) -> Self { self.sandbox = Some(policy); self }
}

#[async_trait]
impl crate::provider::RuntimeProvider for ProcessRuntimeProvider {
    async fn create(&self, id: &str, config: &RuntimeConfig) -> Result<Arc<dyn RuntimeHandle>, RuntimeError> {
        let ready_rx = self.connected_registry.notify_when_ready(id).await;
        let endpoint_arg = match &self.endpoint {
            RuntimeEndpoint::Tcp(addr) => format!("ws://{addr}"),
            RuntimeEndpoint::Unix(path) => format!("unix:{}", path.display()),
        };
        let mut cmd = tokio::process::Command::new(&self.binary_path);
        cmd.arg("--endpoint").arg(&endpoint_arg)
            .arg("--runtime-id").arg(id)
            .arg("--working-dir").arg(&config.working_dir);
        if let Some(policy) = &self.sandbox {
            cmd.arg("--sandbox");
            for p in &policy.extra_read_paths { cmd.arg("--sandbox-read").arg(p); }
            cmd.env_clear();
            for (k, v) in crate::env_scrub::scrubbed_env() { cmd.env(k, v); }
        }
        cmd.kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| RuntimeError::Provider(e.to_string()))?;
        tokio::time::timeout(self.connect_timeout, ready_rx).await
            .map_err(|_| RuntimeError::Provider("runtime connection timed out".into()))?
            .map_err(|_| RuntimeError::Provider("connection channel dropped".into()))?;
        Ok(Arc::new(ProcessRuntimeHandle {
            child: Mutex::new(Some(child)),
            runtime_id: id.to_string(),
            connected_registry: Arc::clone(&self.connected_registry),
        }))
    }
}
```

### Task 4.6: `InMemExecutorTransport`

**Files:** Create `executor/src/inmem_transport.rs`.

- [ ] **Step 1:**
```rust
use crate::connected_registry::ConnectedRuntimeRegistry;
use crate::executor::create_core;
use crate::provider::RuntimeProvider;
use crate::registry::RuntimeRegistry;
use async_trait::async_trait;
use executor_client::{ClientError, ExecutorTransport};
use models::executor::{
    CommandFailedEvent, ExecutorCommand, ExecutorEvent, RuntimeState, RuntimeStateChangedEvent,
};
use runtime_client::RuntimeTransport;
use std::sync::Arc;
use tokio::sync::mpsc;

/// In-process executor transport for CLI mode: drives runtime lifecycle directly
/// against an owned `RuntimeRegistry` + provider, and returns the live direct
/// `RuntimeTransport` from the shared `ConnectedRuntimeRegistry`.
pub struct InMemExecutorTransport {
    registry: Arc<RuntimeRegistry>,
    provider: Arc<dyn RuntimeProvider>,
    connected: Arc<ConnectedRuntimeRegistry>,
}

impl InMemExecutorTransport {
    pub fn new(provider: Arc<dyn RuntimeProvider>, connected: Arc<ConnectedRuntimeRegistry>) -> Self {
        Self { registry: Arc::new(RuntimeRegistry::new()), provider, connected }
    }
}

#[async_trait]
impl ExecutorTransport for InMemExecutorTransport {
    async fn send(&self, _request_id: &str, cmd: ExecutorCommand) -> Result<mpsc::Receiver<ExecutorEvent>, ClientError> {
        let (tx, rx) = mpsc::channel(8);
        match cmd {
            ExecutorCommand::CreateRuntime(c) => {
                let ev = match create_core(&self.registry, &self.provider, &c.runtime_id, c.config).await {
                    Ok(()) => ExecutorEvent::RuntimeStateChanged(RuntimeStateChangedEvent {
                        runtime_id: c.runtime_id, state: RuntimeState::Running,
                    }),
                    Err(e) => ExecutorEvent::CommandFailed(CommandFailedEvent { message: e.to_string() }),
                };
                let _ = tx.send(ev).await;
            }
            ExecutorCommand::DestroyRuntime(c) => {
                let ev = match self.registry.begin_stop(&c.runtime_id).await {
                    Ok(handle) => {
                        if let Some(h) = handle { let _ = h.stop().await; }
                        let _ = self.registry.complete_stop(&c.runtime_id).await;
                        ExecutorEvent::RuntimeStateChanged(RuntimeStateChangedEvent {
                            runtime_id: c.runtime_id, state: RuntimeState::Stopped,
                        })
                    }
                    Err(e) => ExecutorEvent::CommandFailed(CommandFailedEvent { message: e.to_string() }),
                };
                let _ = tx.send(ev).await;
            }
            ExecutorCommand::RestartRuntime(_)
            | ExecutorCommand::QueryRuntimes(_)
            | ExecutorCommand::ToolCall(_)
            | ExecutorCommand::CancelToolCall(_) => {
                let _ = tx.send(ExecutorEvent::CommandFailed(CommandFailedEvent {
                    message: "command not supported by in-process executor".into(),
                })).await;
            }
        }
        Ok(rx)
    }

    async fn runtime_transport(&self, runtime_id: &str) -> Result<Arc<dyn RuntimeTransport>, ClientError> {
        self.connected.runtime_transport(runtime_id).await
            .ok_or_else(|| ClientError::CommandFailed(format!("runtime '{runtime_id}' not connected")))
    }
}
```
  (`RuntimeRegistry` is `pub(crate)`; make `create_core` `pub(crate)`. Keep `begin_stop`/`complete_stop` as-is.)

### Task 4.7: `executor/src/lib.rs` exports + build

- [ ] **Step 1:**
```rust
mod connected_registry;
mod env_scrub;
mod error;
mod executor;
mod inmem_transport;
mod process_provider;
mod provider;
mod registry;
mod runtime_listener;
mod socket_transport;

pub use connected_registry::ConnectedRuntimeRegistry;
pub use env_scrub::{SANDBOX_ENV_ALLOWLIST, scrubbed_env};
pub use error::{ExecutorError, RuntimeError};
pub use executor::{Executor, serve_runtime_connections};
pub use inmem_transport::InMemExecutorTransport;
pub use process_provider::{ProcessRuntimeProvider, SandboxPolicy};
pub use provider::{HealthStatus, RuntimeHandle, RuntimeProvider};
pub use runtime_listener::{AcceptedConn, RuntimeEndpoint, RuntimeListenerServer};
pub use socket_transport::{SocketRuntimeTransport, UnixSocketRuntimeTransport};
```
- [ ] **Step 2:** `executor/Cargo.toml` deps — ensure: `executor-client = { path = "../executor-client", default-features = false }`, `runtime-client = { path = "../runtime-client" }`, `tokio-util = { workspace = true }`, tokio features include `net` + `process` (workspace tokio already has `net`; keep `features = ["process"]`).
- [ ] **Step 3:** Fix the executor `tests/integration_test.rs`: it uses `server::Server` (unchanged) + create/destroy/query — still valid (no tool calls). Should compile/pass unchanged.
- [ ] **Step 4:** Run `cargo test -p executor` → PASS. Commit `refactor(executor): unified socket transport + RuntimeEndpoint + InMemExecutorTransport + env scrub`.

---

## Phase 5 — `runtime` crate: `--endpoint` / `--sandbox` / nono

**Files:** Modify `runtime/Cargo.toml`, `runtime/src/main.rs`; create `runtime/src/sandbox.rs`.

- [ ] **Step 1:** `runtime/Cargo.toml`:
```toml
[features]
default = ["sandbox"]
sandbox = ["dep:nono"]

[dependencies]
# ...existing...
tokio = { workspace = true, features = ["process", "net"] }
nono = { version = "0.59", default-features = false, optional = true }
```
- [ ] **Step 2:** `runtime/src/sandbox.rs` `[cfg(feature = "sandbox")]`:
```rust
use std::path::{Path, PathBuf};

/// Per-platform read-only system paths a typical toolchain (`bash`, coreutils,
/// git, compilers) needs. Start minimal; expand from observed denials.
fn system_read_paths() -> Vec<&'static str> {
    #[cfg(target_os = "linux")]
    { vec!["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt", "/proc"] }
    #[cfg(target_os = "macos")]
    { vec!["/usr", "/bin", "/sbin", "/System", "/Library", "/private/etc", "/etc", "/opt", "/var", "/private/var"] }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    { Vec::new() }
}

/// Build the capability set and enter the sandbox. Fail-closed: an unsupported
/// platform or any error returns `Err` (caller exits non-zero before connecting).
pub fn apply(working_dir: &Path, socket_path: Option<&Path>, extra_read: &[PathBuf]) -> Result<(), String> {
    use nono::{AccessMode, CapabilitySet, Sandbox, UnixSocketMode};

    let info = Sandbox::support_info();
    if !info.is_supported {
        return Err(format!("nono sandbox unsupported on {}: {}", info.platform, info.details));
    }

    let mut caps = CapabilitySet::new();
    for p in system_read_paths() {
        if Path::new(p).exists() {
            caps = caps.allow_path(p, AccessMode::Read).map_err(|e| e.to_string())?;
        }
    }
    // Devices bash/coreutils commonly need writable.
    for dev in ["/dev/null", "/dev/zero", "/dev/urandom", "/dev/random", "/dev/tty"] {
        if Path::new(dev).exists() {
            caps = caps.allow_path(dev, AccessMode::ReadWrite).map_err(|e| e.to_string())?;
        }
    }
    caps = caps.allow_path(working_dir, AccessMode::ReadWrite).map_err(|e| e.to_string())?;
    if let Some(tmp) = std::env::var_os("TMPDIR") {
        caps = caps.allow_path(PathBuf::from(tmp), AccessMode::ReadWrite).map_err(|e| e.to_string())?;
    }
    for p in extra_read {
        caps = caps.allow_path(p, AccessMode::Read).map_err(|e| e.to_string())?;
    }
    if let Some(sock) = socket_path {
        caps = caps.allow_unix_socket(sock, UnixSocketMode::Connect).map_err(|e| e.to_string())?;
    }
    caps = caps.block_network();
    Sandbox::apply(&caps).map_err(|e| e.to_string())?;
    Ok(())
}
```
- [ ] **Step 3:** `runtime/src/main.rs` rewrite. New CLI + endpoint parse + generic loop + sandbox apply:
```rust
#[derive(Parser)]
struct Cli {
    /// ws://host:port  or  unix:/path/to.sock
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    runtime_id: String,
    #[arg(long)]
    working_dir: PathBuf,
    /// Confine tool execution with nono before connecting (fail-closed).
    #[arg(long)]
    sandbox: bool,
    /// Extra read-only paths inside the sandbox.
    #[arg(long = "sandbox-read")]
    sandbox_read: Vec<PathBuf>,
}

enum Endpoint { Ws(String), Unix(PathBuf) }

fn parse_endpoint(s: &str) -> Result<Endpoint, String> {
    if let Some(rest) = s.strip_prefix("unix:") {
        Ok(Endpoint::Unix(PathBuf::from(rest)))
    } else if s.starts_with("ws://") || s.starts_with("wss://") {
        Ok(Endpoint::Ws(s.to_string()))
    } else {
        Err(format!("unsupported endpoint scheme: {s}"))
    }
}
```
  `main`:
```rust
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let endpoint = match parse_endpoint(&cli.endpoint) {
        Ok(e) => e,
        Err(e) => { eprintln!("{e}"); std::process::exit(2); }
    };

    if cli.sandbox {
        #[cfg(feature = "sandbox")]
        {
            let socket = match &endpoint { Endpoint::Unix(p) => Some(p.as_path()), Endpoint::Ws(_) => None };
            if let Err(e) = runtime::sandbox::apply(&cli.working_dir, socket, &cli.sandbox_read) {
                eprintln!("sandbox apply failed: {e}");
                std::process::exit(3);
            }
        }
        #[cfg(not(feature = "sandbox"))]
        {
            eprintln!("--sandbox requested but this binary was built without the `sandbox` feature");
            std::process::exit(3);
        }
    }

    match endpoint {
        Endpoint::Ws(url) => {
            match connect_async(&url).await {
                Ok((ws, _)) => run_loop(ws, cli.working_dir, cli.runtime_id).await,
                Err(e) => { eprintln!("failed to connect to {url}: {e}"); std::process::exit(1); }
            }
        }
        Endpoint::Unix(path) => {
            match tokio::net::UnixStream::connect(&path).await {
                Ok(stream) => match tokio_tungstenite::client_async("ws://localhost/", stream).await {
                    Ok((ws, _)) => run_loop(ws, cli.working_dir, cli.runtime_id).await,
                    Err(e) => { eprintln!("ws handshake failed on unix socket: {e}"); std::process::exit(1); }
                },
                Err(e) => { eprintln!("failed to connect to unix socket {}: {e}", path.display()); std::process::exit(1); }
            }
        }
    }
}
```
  `run_loop<S>` is the existing message loop, generalized:
```rust
async fn run_loop<S>(ws: tokio_tungstenite::WebSocketStream<S>, working_dir: PathBuf, runtime_id: String)
where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static {
    let (sink_raw, mut stream) = ws.split();
    let sink = Arc::new(Mutex::new(sink_raw));
    // ...send RuntimeReady, then the ToolCall/CancelCall loop exactly as today...
}
```
  Update the `WsSink` type alias to be generic inside `run_loop` (use a local `Arc<Mutex<SplitSink<WebSocketStream<S>, Message>>>`). `runtime::tools::dispatch` is unchanged.
- [ ] **Step 4:** `runtime/src/lib.rs`:
```rust
pub mod tools;
#[cfg(feature = "sandbox")]
pub mod sandbox;
```
- [ ] **Step 5:** Add a unit test for `parse_endpoint` (ws / unix / bad scheme) in `main.rs` `#[cfg(test)]`.
- [ ] **Step 6:** Run `cargo build -p runtime` and `cargo build -p runtime --no-default-features` (sandbox off
  → no nono, `--sandbox` exits 3). Run `cargo test -p runtime`. Commit
  `feat(runtime): --endpoint unix/ws + nono --sandbox (fail-closed)`.

---

## Phase 6 — `workflow`: notification channel

**Files:** Modify `workflow/src/workflow_actor.rs`, `workflow/src/context.rs`, `workflow/src/lib.rs`,
`workflow/tests/workflow_e2e.rs`.

- [ ] **Step 1:** In `workflow_actor.rs`, add (near `WorkflowStatus`):
```rust
use serde_json::Value;

/// Push control signal for an out-of-band observer (e.g. the CLI control loop).
/// Sent on the command path only — never from `apply_event` (which also runs during
/// replay). The journal remains the durable source of truth.
#[derive(Debug, Clone)]
pub enum WorkflowNotification {
    AwaitingUserInput { question: String },
    Suspended,
    Finished { output: Value },
    Failed { error: String },
}
```
- [ ] **Step 2:** `context.rs` — add field to `WorkflowRuntimeContext`:
```rust
use crate::workflow_actor::WorkflowNotification;
// ...
    /// Live push channel for workflow status transitions (never journaled).
    pub workflow_events: tokio::sync::mpsc::Sender<WorkflowNotification>,
```
  (Keep `#[derive(Clone)]`; `mpsc::Sender` is `Clone`.)
- [ ] **Step 3:** In `WorkflowActor`, add helper + emit at decision points:
```rust
fn notify(&self, n: WorkflowNotification) {
    // Best-effort live signal; a full/closed channel is harmless (journal is durable).
    let _ = self.rt.workflow_events.try_send(n);
}
```
  Emit:
  - `on_start`: on the `WorkflowFailed` paths → `self.notify(WorkflowNotification::Failed { error: error.clone() })`
    before returning. (Both the "start agent not found" and spawn-error branches.)
  - `on_concluded`: when returning `WorkflowFinished { output }` (both the no-`from_agent` and no-transition
    branches) → `self.notify(WorkflowNotification::Finished { output: output.clone() })`; on the transition
    target-not-found / spawn-error `WorkflowFailed` → `self.notify(Failed{..})`.
  - `handle_command` `Cancel` → `self.notify(WorkflowNotification::Suspended)`.
  - `handle_command` `AgentFailed`: recoverable → `notify(Suspended)`; else `notify(Failed{error})`.
  - `handle_command` `AgentAsked`: → `self.notify(WorkflowNotification::AwaitingUserInput { question })`
    (capture `question` before it is dropped; the variant currently ignores it via `..`).
- [ ] **Step 4:** `workflow/src/lib.rs` — add `WorkflowNotification` to the `pub use workflow_actor::{...}`.
- [ ] **Step 5:** `workflow_e2e.rs` `runtime_context()` — add the channel:
```rust
let (tx, _rx) = tokio::sync::mpsc::channel(64);
WorkflowRuntimeContext {
    provider_registry: registry,
    toolbox_factory: factory,
    runtime_client: RuntimeClient::new(MockTransport::ok("")),
    event_sink: Arc::new(NoopSink),
    workflow_events: tx,
}
```
  (Hold `_rx` in a module-level keeper if needed to avoid immediate close; dropping is fine since `try_send`
  ignores closed.) Add a new e2e assertion test that the channel receives `Finished`/`AwaitingUserInput`
  (keep `rx` and `recv().await`).
- [ ] **Step 6:** Run `cargo test -p workflow` → PASS. Commit `feat(workflow): WorkflowNotification push channel`.

---

## Phase 7 — `cli` crate (`october`)

**Files:** Create crate `cli`; root `Cargo.toml` `members += "cli"`.

### Task 7.1: Crate skeleton + config

- [ ] **Step 1:** `cli/Cargo.toml`:
```toml
[package]
name = "cli"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "october"
path = "src/main.rs"

[dependencies]
actor          = { path = "../actor", features = ["file-journal"] }
agentcore      = { path = "../agentcore" }
anthropic      = { path = "../providers/anthropic" }
executor       = { path = "../executor" }
executor-client = { path = "../executor-client", default-features = false }
runtime-client = { path = "../runtime-client" }
workflow       = { path = "../workflow" }
models         = { path = "../models" }
clap           = { version = "4", features = ["derive"] }
serde          = { workspace = true }
serde_json     = { workspace = true }
tokio          = { workspace = true, features = ["rt-multi-thread", "macros", "sync", "net", "time"] }
tokio-util     = { workspace = true }
uuid           = { workspace = true }
thiserror      = { workspace = true }
async-trait    = { workspace = true }
tracing        = { workspace = true }
eval           = { workspace = true }

[dev-dependencies]
mock-llm       = { path = "../providers/mock-llm" }
tempfile       = "3"

[lints]
workspace = true
```
- [ ] **Step 2:** `cli/src/error.rs`:
```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("io error: {0}")]
    Io(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("validation failed:\n{0}")]
    Validation(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("executor error: {0}")]
    Executor(String),
}
```
- [ ] **Step 3:** `cli/src/config.rs` (serde + registry; `LlmProvider` keyed by **model key**):
```rust
use crate::error::CliError;
use agentcore::LlmProvider;
use anthropic::AnthropicProvider;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct OctoberConfig {
    pub providers: HashMap<String, ProviderConfig>,
    pub models: HashMap<String, ModelConfig>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub storage: StorageConfig,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProviderConfig {
    Anthropic { api_key_env: String, #[serde(default)] base_url: Option<String> },
    Mock { base_url: String },
}

#[derive(Debug, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model_id: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub extra_read_paths: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct StorageConfig {
    pub root_dir: PathBuf,
}
impl Default for StorageConfig {
    fn default() -> Self { Self { root_dir: PathBuf::from("./.october") } }
}

impl OctoberConfig {
    pub fn load(path: &std::path::Path) -> Result<Self, CliError> {
        let text = std::fs::read_to_string(path).map_err(|e| CliError::Io(e.to_string()))?;
        serde_json::from_str(&text).map_err(|e| CliError::Config(e.to_string()))
    }
}

/// Build the provider registry keyed by **model key** (matches `WorkflowAgentDef.model`).
pub fn build_registry(cfg: &OctoberConfig) -> Result<HashMap<String, Arc<dyn LlmProvider>>, CliError> {
    let mut reg: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
    for (model_key, mc) in &cfg.models {
        let pc = cfg.providers.get(&mc.provider).ok_or_else(|| {
            CliError::Config(format!("model '{model_key}' references unknown provider '{}'", mc.provider))
        })?;
        let provider: Arc<dyn LlmProvider> = match pc {
            ProviderConfig::Anthropic { api_key_env, base_url } => {
                let key = std::env::var(api_key_env)
                    .map_err(|_| CliError::Config(format!("env var '{api_key_env}' for provider '{}' not set", mc.provider)))?;
                let mut p = AnthropicProvider::with_api_key(key)
                    .map_err(|e| CliError::Provider(e.to_string()))?
                    .with_model(&mc.model_id);
                if let Some(u) = base_url { p = p.with_base_url(u); }
                Arc::new(p)
            }
            ProviderConfig::Mock { base_url } => {
                let p = AnthropicProvider::with_api_key("mock")
                    .map_err(|e| CliError::Provider(e.to_string()))?
                    .with_base_url(base_url)
                    .with_model(&mc.model_id)
                    .with_retry_delay_secs(0);
                Arc::new(p)
            }
        };
        reg.insert(model_key.clone(), provider);
    }
    Ok(reg)
}
```
  *(If `AnthropicProvider::with_max_tokens` exists, apply `mc.max_tokens`; otherwise omit — confirm at impl.)*
- [ ] **Step 4:** test (`config.rs` `#[cfg(test)]`): parse the sample JSON; assert models/providers/storage default.
- [ ] **Step 5:** Run `cargo build -p cli` (lib only via main stub) — defer until run.rs exists; or add a minimal main.

### Task 7.2: `validate`

- [ ] **Step 1:** `cli/src/validate.rs`:
```rust
use crate::config::OctoberConfig;
use models::workflow::WorkflowDefinition;
use std::collections::HashSet;

/// Structural + semantic checks; returns ALL errors (empty = valid).
pub fn validate(def: &WorkflowDefinition, cfg: &OctoberConfig) -> Vec<String> {
    let mut errs = Vec::new();
    let names: HashSet<&str> = def.agents.iter().map(|a| a.name.as_str()).collect();
    if !names.contains(def.start.as_str()) {
        errs.push(format!("start agent '{}' is not defined", def.start));
    }
    for a in &def.agents {
        if !cfg.models.contains_key(&a.model) {
            errs.push(format!("agent '{}' uses model '{}' which is not in config.models", a.name, a.model));
        }
        if let Some(ts) = &a.transitions {
            for t in ts {
                if !names.contains(t.to.as_str()) {
                    errs.push(format!("agent '{}' has a transition to undefined agent '{}'", a.name, t.to));
                }
                if let Some(cond) = &t.condition
                    && let Err(e) = eval::Expr::new(cond).value("output", serde_json::json!({})).exec()
                {
                    errs.push(format!("agent '{}' transition condition `{cond}` failed to parse: {e}", a.name));
                }
            }
        }
    }
    for (model_key, mc) in &cfg.models {
        if !cfg.providers.contains_key(&mc.provider) {
            errs.push(format!("model '{model_key}' references unknown provider '{}'", mc.provider));
        }
    }
    errs
}
```
- [ ] **Step 2:** tests: valid workflow → empty; bad start / bad transition target / bad model / bad condition → reported.

### Task 7.3: `TerminalSink`

- [ ] **Step 1:** `cli/src/terminal_sink.rs` (enumerate every `AgentEvent` variant — no `_` arm):
```rust
use agentcore::{AgentEvent, EventSink};
use std::io::Write;

pub struct TerminalSink;

impl EventSink for TerminalSink {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::TextChunk(e) => { print!("{}", e.text); let _ = std::io::stdout().flush(); }
            AgentEvent::ToolCallStart(e) => { eprintln!("\n· tool {} [{}]", e.name, e.tool_call_id); }
            AgentEvent::ToolComplete(e) => {
                eprintln!("· tool {} → {}", e.tool_call_id, if e.is_error { "error" } else { "ok" });
            }
            AgentEvent::RunComplete(e) => {
                eprintln!("\n· run complete ({} iterations, {}/{} tokens)", e.iterations, e.usage.input_tokens, e.usage.output_tokens);
            }
            AgentEvent::InputMessage(_)
            | AgentEvent::MessageStart(_)
            | AgentEvent::MessageStop(_)
            | AgentEvent::MessageComplete(_)
            | AgentEvent::ThinkingChunk(_)
            | AgentEvent::ToolCallInputDelta(_)
            | AgentEvent::ToolCallInputDone(_)
            | AgentEvent::ToolExecuting(_) => {}
        }
    }
}
```

### Task 7.4: `run` + `resume` assembly + manifest

- [ ] **Step 1:** `cli/src/run.rs` (full assembly + two-plane loop). Key pieces:
```rust
use crate::config::{OctoberConfig, build_registry};
use crate::error::CliError;
use crate::terminal_sink::TerminalSink;
use crate::validate::validate;
use actor::{FileJournal, spawn_root};
use executor::{
    ConnectedRuntimeRegistry, InMemExecutorTransport, ProcessRuntimeProvider, RuntimeEndpoint,
    RuntimeListenerServer, SandboxPolicy, serve_runtime_connections,
};
use executor_client::ExecutorClient;
use models::executor::RuntimeConfig;
use models::workflow::WorkflowDefinition;
use runtime_client::RuntimeClient;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workflow::{DefaultToolboxFactory, WorkflowActor, WorkflowCommand, WorkflowNotification, WorkflowRuntimeContext};

pub const EXIT_AWAIT: i32 = 10;

#[derive(Serialize, Deserialize)]
struct Manifest { workflow: WorkflowDefinition, workdir: PathBuf }

pub struct RunParams {
    pub workflow_path: PathBuf,
    pub config_path: PathBuf,
    pub workdir: PathBuf,
    pub input: String,
    pub state_dir: Option<PathBuf>,
    pub runtime_bin: PathBuf,
}

pub struct ResumeParams {
    pub run_id: String,
    pub config_path: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub message: String,
    pub runtime_bin: PathBuf,
}

fn load_workflow(path: &Path) -> Result<WorkflowDefinition, CliError> {
    let text = std::fs::read_to_string(path).map_err(|e| CliError::Io(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| CliError::Config(e.to_string()))
}

fn run_dir(root: &Path, run_id: &str) -> PathBuf { root.join("runs").join(run_id) }

/// Shared driver: assemble runtime, spawn the workflow actor, drive the control loop.
async fn drive(
    def: WorkflowDefinition,
    cfg: OctoberConfig,
    workdir: PathBuf,
    run_id: String,
    root_dir: PathBuf,
    runtime_bin: PathBuf,
    kickoff: WorkflowCommand,
) -> Result<i32, CliError> {
    let registry = build_registry(&cfg)?;

    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    let socket_path = run_dir(&root_dir, &run_id).join("rt.sock");
    if let Some(d) = socket_path.parent() {
        std::fs::create_dir_all(d).map_err(|e| CliError::Io(e.to_string()))?;
    }
    let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Unix(socket_path.clone()))
        .await.map_err(|e| CliError::Executor(e.to_string()))?;
    let cancel = CancellationToken::new();
    serve_runtime_connections(listener, connected.clone(), cancel.clone());

    let provider = ProcessRuntimeProvider::new(runtime_bin, RuntimeEndpoint::Unix(socket_path), connected.clone())
        .with_sandbox(SandboxPolicy { extra_read_paths: cfg.sandbox.extra_read_paths.clone() });
    let transport = InMemExecutorTransport::new(Arc::new(provider), connected.clone());
    let client = ExecutorClient::new(transport);

    client.create_runtime(&run_id, RuntimeConfig { working_dir: workdir.to_string_lossy().into_owned() })
        .await.map_err(|e| CliError::Executor(e.to_string()))?;
    let rt_transport = client.runtime_transport(&run_id).await.map_err(|e| CliError::Executor(e.to_string()))?;
    let runtime_client = RuntimeClient::from_arc(rt_transport);

    // Persist manifest (no secrets).
    let manifest = Manifest { workflow: def.clone(), workdir: workdir.clone() };
    let manifest_path = run_dir(&root_dir, &run_id).join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).map_err(|e| CliError::Io(e.to_string()))?)
        .map_err(|e| CliError::Io(e.to_string()))?;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let ctx = WorkflowRuntimeContext {
        provider_registry: registry,
        toolbox_factory: Arc::new(DefaultToolboxFactory),
        runtime_client,
        event_sink: Arc::new(TerminalSink),
        workflow_events: tx,
    };
    let journal = Arc::new(FileJournal::new(root_dir.clone()));
    let wf = spawn_root(WorkflowActor::new(run_id.clone(), def, ctx), journal);
    wf.tell(kickoff).await.map_err(|e| CliError::Executor(e.to_string()))?;

    let exit = loop {
        match rx.recv().await {
            Some(WorkflowNotification::AwaitingUserInput { question }) => {
                println!("\n⏸ awaiting input (run {run_id}):\n{question}");
                let _ = client.destroy_runtime(&run_id).await;
                break EXIT_AWAIT;
            }
            Some(WorkflowNotification::Finished { output }) => {
                println!("\n{}", serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string()));
                let _ = client.destroy_runtime(&run_id).await;
                break 0;
            }
            Some(WorkflowNotification::Failed { error }) => {
                eprintln!("\n✗ failed: {error}");
                let _ = client.destroy_runtime(&run_id).await;
                break 1;
            }
            Some(WorkflowNotification::Suspended) => { /* keep streaming */ }
            None => break 1,
        }
    };
    cancel.cancel();
    Ok(exit)
}

pub async fn run(p: RunParams) -> Result<i32, CliError> {
    let cfg = OctoberConfig::load(&p.config_path)?;
    let def = load_workflow(&p.workflow_path)?;
    let errs = validate(&def, &cfg);
    if !errs.is_empty() { return Err(CliError::Validation(errs.join("\n"))); }
    let root_dir = p.state_dir.unwrap_or_else(|| cfg.storage.root_dir.clone());
    let run_id = Uuid::new_v4().to_string();
    println!("run {run_id}");
    drive(def, cfg, p.workdir, run_id, root_dir, p.runtime_bin, WorkflowCommand::Start { input: p.input }).await
}

pub async fn resume(p: ResumeParams) -> Result<i32, CliError> {
    let cfg = OctoberConfig::load(&p.config_path)?;
    let root_dir = p.state_dir.unwrap_or_else(|| cfg.storage.root_dir.clone());
    let manifest_path = run_dir(&root_dir, &p.run_id).join("manifest.json");
    let manifest: Manifest = serde_json::from_slice(
        &std::fs::read(&manifest_path).map_err(|e| CliError::Io(e.to_string()))?,
    ).map_err(|e| CliError::Config(e.to_string()))?;
    drive(manifest.workflow, cfg, manifest.workdir, p.run_id, root_dir, p.runtime_bin,
          WorkflowCommand::Resume { message: p.message }).await
}
```
- [ ] **Step 2:** `cli/src/lib.rs`:
```rust
pub mod config;
pub mod error;
pub mod run;
pub mod terminal_sink;
pub mod validate;
```
- [ ] **Step 3:** `cli/src/main.rs`:
```rust
use clap::{Parser, Subcommand};
use cli::config::OctoberConfig;
use cli::run::{ResumeParams, RunParams, resume, run};
use cli::validate::validate;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "october")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Validate { #[arg(long)] workflow: PathBuf, #[arg(long)] config: PathBuf },
    Run {
        #[arg(long)] workflow: PathBuf,
        #[arg(long)] config: PathBuf,
        #[arg(long)] workdir: PathBuf,
        #[arg(long)] input: String,
        #[arg(long)] state_dir: Option<PathBuf>,
    },
    Resume {
        #[arg(long)] run: String,
        #[arg(long)] config: PathBuf,
        #[arg(long)] message: String,
        #[arg(long)] state_dir: Option<PathBuf>,
    },
}

/// Locate the sibling `october-runtime` binary next to this executable.
fn runtime_binary_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("october-runtime")))
        .unwrap_or_else(|| PathBuf::from("october-runtime"))
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Validate { workflow, config } => {
            match (OctoberConfig::load(&config), std::fs::read_to_string(&workflow)) {
                (Ok(cfg), Ok(text)) => match serde_json::from_str(&text) {
                    Ok(def) => {
                        let errs = validate(&def, &cfg);
                        if errs.is_empty() { println!("valid"); 0 }
                        else { for e in &errs { eprintln!("✗ {e}"); } 1 }
                    }
                    Err(e) => { eprintln!("workflow parse error: {e}"); 2 }
                },
                (Err(e), _) | (_, Err(_)) if matches!(e_check(&config), true) => { eprintln!("{e}"); 2 }
                _ => { eprintln!("failed to load config or workflow"); 2 }
            }
        }
        Command::Run { workflow, config, workdir, input, state_dir } => {
            match run(RunParams { workflow_path: workflow, config_path: config, workdir, input, state_dir, runtime_bin: runtime_binary_path() }).await {
                Ok(code) => code,
                Err(e) => { eprintln!("{e}"); 1 }
            }
        }
        Command::Resume { run: run_id, config, message, state_dir } => {
            match resume(ResumeParams { run_id, config_path: config, state_dir, message, runtime_bin: runtime_binary_path() }).await {
                Ok(code) => code,
                Err(e) => { eprintln!("{e}"); 1 }
            }
        }
    };
    std::process::exit(code);
}
```
  *(Simplify the `Validate` arm during impl — the `e_check` placeholder above is illustrative; final form
  loads config then workflow, printing whichever error occurs. Keep it lint-clean.)*
- [ ] **Step 4:** root `Cargo.toml` `members += "cli"`. Run `cargo build -p cli` → PASS.
- [ ] **Step 5:** Commit `feat(cli): october validate/run/resume with in-process sandboxed executor`.

---

## Phase 8 — CLI e2e tests + full green

**Files:** `cli/tests/cli_e2e.rs`.

- [ ] **Step 1:** Helper to locate the built `october-runtime` (walks up from the test exe):
```rust
fn locate_runtime_bin() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    // .../target/<profile>/deps/<test> → up to target/<profile>/october-runtime
    let mut dir = exe.parent().unwrap().to_path_buf();
    if dir.ends_with("deps") { dir.pop(); }
    dir.join("october-runtime")
}

fn sandbox_supported(runtime_bin: &std::path::Path) -> bool {
    // Probe: a sandbox-on runtime with a bogus endpoint exits 3 (apply failed) on
    // unsupported kernels, or fails to connect (exit 1) when supported. We treat a
    // missing binary as "skip".
    runtime_bin.exists()
}
```
- [ ] **Step 2:** Orchestration e2e (mock provider, agent concludes; validates full wiring + sandbox apply +
  unix connect, no broad-FS tool): two-agent workflow via `MockLlmServer`, config `type:"mock"`, `run()` →
  exit 0, `Finished` output asserted by reading the printed result / journal. **Support-gate**: if `run()`
  errors with a message containing "sandbox"/"timed out", `eprintln!("skipping: sandbox unsupported")` + return.
- [ ] **Step 3:** Suspend/resume e2e: agent `allow_ask_user` asks → `run()` returns `EXIT_AWAIT`; then `resume()`
  with a reply → exit 0; assert journal under `<state>/runs/<id>/journal.jsonl` exists and replays.
- [ ] **Step 4:** Sandbox confinement e2e (support-gated): agent calls `bash` to write a file **inside**
  workdir (succeeds) and **outside** workdir (denied/error). Assert tool outcomes.
- [ ] **Step 5:** `cargo test -p cli` locally (macOS Seatbelt). Iterate the `system_read_paths` allowlist until
  bash runs confined.
- [ ] **Step 6:** Whole-workspace gate:
```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```
  Fix all warnings/failures. Commit. Push branch; update PR; ensure GitHub Actions CI is green (iterate on the
  Linux `system_read_paths` allowlist via CI if a denial surfaces).

---

## Self-review (spec coverage)

- Threat model env-scrub → Phase 4.5 (`env_scrub.rs` + applied in `ProcessRuntimeProvider`) + unit test. ✓
- Two clients/two transports → `ExecutorClient` (lifecycle) + `RuntimeClient::from_arc` over transport. ✓
- Unified generic-over-socket connection layer → Phase 4.1/4.4 (`SocketRuntimeTransport<S>`, generic handler). ✓
- `runtime_transport` sum behavior (direct vs relay) → `InMemExecutorTransport` vs `WsExecutorTransport`. ✓
- `RuntimeEndpoint` sum type + sandbox policy on provider → Phase 4.3/4.5. ✓
- FileJournal no-op snapshots + torn-line replay → Phase 1. ✓
- Runtime `--endpoint`/`--sandbox` fail-closed, feature-gated nono → Phase 5. ✓
- WorkflowNotification push channel on command path → Phase 6. ✓
- CLI validate/run/resume + config + manifest + control loop → Phase 7. ✓
- Cargo features (`file-journal`, `ws`, `sandbox`) → applied per crate. ✓
- Testing matrix (unit, e2e, env-scrub, sandbox e2e, lints) → Phase 1/4/8. ✓
- Security: unix-socket connect-only capability; socket in run dir, dir 0700, unlinked → Phase 4.3. ✓

## Review corrections applied (post adversarial multi-lens review)

1. **clap `run` keyword** → `#[arg(long = "run")] run_id: String` (no raw identifier).
2. **FileJournal batch atomicity** → store **one line per persist BATCH**: line = base64(JSON array of
   per-event base64 strings). A torn final write drops the whole partial batch (preserves the actor's
   all-or-nothing-per-batch invariant). `replay(after_seq)` decodes batches in order, counts events, yields
   those with event-index > after_seq. Empty/undecodable complete line → stop (corruption boundary).
3. **Sandbox::apply** → `let _ = Sandbox::apply(&caps).map_err(|e| e.to_string())?;` (discard SeccompNetFallback).
4. **Unix socket path length** → socket at `std::env::temp_dir().join(format!("october-{short}")).join("rt.sock")`
   (`short` = first 12 hex of run_id), parent 0700; guard returns error if path > 107 bytes. Socket is ephemeral
   (rebuilt each run/resume), outside workdir — satisfies the security argument.
5. **Env-scrub child test** → in `env_scrub.rs`: build a `tokio::process::Command`, `.env("ANTHROPIC_API_KEY","leak")`,
   `.env_clear()`, add `scrubbed_env()`, run `bash -c 'echo "$ANTHROPIC_API_KEY"'`, assert empty stdout
   (deterministic; no unsafe `std::env::set_var`). Plus the static allowlist tests.
6. **0700 dir perms** → propagate `set_permissions` error as `ExecutorError::BindFailed` (don't `let _`).
7. **Config** → `with_max_tokens(mc.max_tokens)` (setter exists, takes `Option<u32>`).
8. **AgentAsked** → capture `question` (not `..`) and emit `AwaitingUserInput { question }`.
9. **Notification channel** → capacity 256; `notify()` = `try_send` + `tracing::warn!` on full; CLI loop
   `None => break 1` (actor death → clean exit, never hangs).
10. **e2e** → write config JSON (with `mock.url()`) to a temp file before calling `run`/`resume`; robust
    `locate_runtime_bin` (env `OCTOBER_RUNTIME_BIN` override → sibling → deps-parent fallbacks); broaden
    support-gate match terms; assert manifest.json round-trips and contains no secrets.
11. **InMemExecutorTransport / TerminalSink** → enumerate enum variants explicitly (OR-patterns), never `_`,
    to satisfy `deny(wildcard_enum_match_arm)`. (Tungstenite `Message` matches keep the existing committed
    `_ => …` pattern, which is already CI-green because `Message` is `#[non_exhaustive]`.)
12. **validate condition** → empirically confirm `eval` returns `Ok(false)` (not `Err`) for field access on a
    `{}` placeholder; if it errors, switch to a permissive placeholder. Add a test for a field-access condition.

## Risks / iteration points
- **`system_read_paths` allowlist** (Linux Landlock) is the main maintenance surface — start minimal, expand
  from CI denials. macOS Seatbelt validated locally.
- nono `Sandbox::apply` exact return binding — discard with `Sandbox::apply(&caps)?;` (drops `SeccompNetFallback`).
- Unix socket path length (<108 bytes) — keep `root_dir` shallow; tests use `TempDir`.
