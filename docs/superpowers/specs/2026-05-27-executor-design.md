# Executor / Server / Runtime Design

**Date:** 2026-05-27
**Status:** Approved

## Context

The `agentcore` crate implements the agent execution loop. This spec covers the infrastructure layer that manages agent runtimes: how runtimes are created and destroyed, how a server orchestrates multiple executors, and how state is communicated back.

## Scope

- **`executor`** crate: WebSocket client connecting to server; owns `RuntimeProvider` and `RuntimeHandle` traits; manages a runtime registry; runs health-check and auto-restart loop.
- **`server`** crate: WebSocket server accepting executor connections; provides a command API for lifecycle operations; dispatches inbound executor events to a caller-supplied handler.
- **`runtime`** crate: reserved for future use (e.g. runtime-side SDK or subprocess binary). Empty for now.
- **`fluorite/executor.fl`**: protocol message types for the executor ↔ server transport.

**Explicitly out of scope:**
- Concrete `RuntimeProvider` implementations (container, VM, subprocess) — future tasks.
- Tool-call forwarding content between runtime and server agent — future tasks.
- Server-side agent integration.

---

## Protocol (`fluorite/executor.fl`)

### Envelopes

```
// Server → Executor
struct ExecutorInboundMessage {
    request_id: String,
    command: ExecutorCommand,
}

// Executor → Server
struct ExecutorOutboundMessage {
    request_id: Option<String>,  // present when responding to a command
    event: ExecutorEvent,
}
```

### Commands (`ExecutorCommand` union)

| Variant | Fields | Description |
|---|---|---|
| `CreateRuntime` | `runtime_id: String, config: RuntimeConfig` | Create and start a new runtime |
| `DestroyRuntime` | `runtime_id: String` | Stop and discard a runtime |
| `RestartRuntime` | `runtime_id: String` | Stop-then-start in place |
| `QueryRuntimes` | _(none)_ | List all known runtimes and their states |

### Events (`ExecutorEvent` union)

| Variant | Fields | Triggered by |
|---|---|---|
| `Registered` | `executor_id: String` | Sent by executor immediately on WS connect; no `request_id` |
| `RuntimeStateChanged` | `runtime_id: String, state: RuntimeState` | Any state transition (command response or autonomous) |
| `RuntimesListed` | `runtimes: Vec<RuntimeInfo>` | Response to `QueryRuntimes` |
| `CommandFailed` | `message: String` | Error response to any command |

`RuntimeStateChanged` serves dual purpose: direct response (carries `request_id`) and unsolicited health-check notification (no `request_id`).

### Supporting types

```
enum RuntimeState { Creating, Running, Stopping, Stopped, Failed }

struct RuntimeInfo {
    runtime_id: String,
    state: RuntimeState,
    restart_count: u32,
}

struct RuntimeConfig {}   // empty; extensible
```

---

## Server crate

### WebSocket listener

Binds a TCP address and upgrades connections via `tokio-tungstenite`. On each new connection:

1. Reads the first `ExecutorOutboundMessage`; expects `Registered { executor_id }`.
2. Wraps the write half in `ExecutorConn { executor_id, sink: Mutex<SplitSink> }`.
3. Registers in `ExecutorRegistry` (shared `Arc<Mutex<HashMap<ExecutorId, ExecutorConn>>>`).
4. Spawns a read loop; removes registration on disconnect.

### `ExecutorConn`

```rust
struct ExecutorConn { executor_id: ExecutorId, sink: Mutex<SplitSink<...>> }

impl ExecutorConn {
    async fn send_command(&self, request_id: String, command: ExecutorCommand) -> Result<()>
}
```

### Server handle API

```rust
impl Server {
    pub async fn create_runtime(executor_id, runtime_id, config) -> Result<()>
    pub async fn destroy_runtime(executor_id, runtime_id) -> Result<()>
    pub async fn restart_runtime(executor_id, runtime_id) -> Result<()>
    pub async fn query_runtimes(executor_id) -> Result<()>
}
```

Each method looks up the executor in the registry and calls `send_command`. Returns `ExecutorNotFound` if the executor isn't registered.

### Event dispatch

Inbound messages from executors are forwarded to a caller-supplied `ExecutorEventHandler` trait:

```rust
trait ExecutorEventHandler: Send + Sync {
    fn on_event(&self, executor_id: &ExecutorId, request_id: Option<&str>, event: &ExecutorEvent);
}
```

The server crate has no business logic — routing and state interpretation belong to the caller.

---

## Executor crate

### `RuntimeProvider` and `RuntimeHandle` traits

```rust
#[async_trait]
pub trait RuntimeProvider: Send + Sync {
    async fn create(
        &self,
        id: RuntimeId,
        config: &RuntimeConfig,
    ) -> Result<Box<dyn RuntimeHandle>, RuntimeError>;
}

#[async_trait]
pub trait RuntimeHandle: Send + Sync {
    async fn stop(&self) -> Result<(), RuntimeError>;
    async fn health_check(&self) -> Result<HealthStatus, RuntimeError>;
}

pub enum HealthStatus { Healthy, Unhealthy { reason: String } }
```

### `RuntimeRegistry`

Owns all live runtimes in memory. Each entry:

```rust
struct RuntimeEntry {
    id: RuntimeId,
    state: RuntimeState,
    handle: Option<Box<dyn RuntimeHandle>>,
    config: RuntimeConfig,
    restart_count: u32,
}
```

State transitions enforced at this layer:
- `create`: only if no entry with that ID exists
- `destroy`: only from `Running` or `Failed`
- `restart`: only from `Running` or `Failed`
- Invalid transitions return `RuntimeError::InvalidStateTransition`

### `Executor`

```rust
pub struct Executor {
    executor_id: ExecutorId,
    server_url: String,
    provider: Box<dyn RuntimeProvider>,
    registry: RuntimeRegistry,
    health_check_interval: Duration,
    max_restarts: u32,
}

impl Executor {
    pub async fn run(self, cancel: CancellationToken) -> Result<(), ExecutorError>
}
```

`run` connects to the server, sends `Registered`, then drives two concurrent loops:

**Read loop** — deserializes `ExecutorInboundMessage`, dispatches:
- `CreateRuntime` → `registry.create(...)` → transitions `Creating → Running`, emits `RuntimeStateChanged` twice (once on start, once on success/failure)
- `DestroyRuntime` → `registry.destroy(...)` → transitions `Stopping → Stopped`
- `RestartRuntime` → `registry.restart(...)` → `Stopping → Stopped → Creating → Running`
- `QueryRuntimes` → `registry.list()` → emits `RuntimesListed`
- On any error → emits `CommandFailed`

**Health-check loop** — every `health_check_interval`:
- For each `Running` runtime: calls `handle.health_check()`
- On `Unhealthy`: transitions to `Failed`, emits `RuntimeStateChanged` (no `request_id`)
- If `restart_count < max_restarts`: attempts restart, increments `restart_count`
- If at max: leaves in `Failed`, does not retry

---

## Design decisions

| Decision | Rationale |
|---|---|
| `ExecutorInboundMessage` / `ExecutorOutboundMessage` naming | Named from the executor's perspective; consistent with executor as the primary actor |
| `RuntimeStateChanged` serves both response and unsolicited roles | Avoids duplicate message types; `request_id` presence distinguishes the two cases |
| `RuntimeProvider` + `RuntimeHandle` in executor, not runtime crate | Executor owns lifecycle management; runtime crate reserved for future runtime-side SDK |
| `ExecutorEventHandler` trait on server, no business logic in server crate | Keeps server as pure transport; caller owns routing and state |
| Health-check loop with `max_restarts` cap | Prevents infinite restart storms on persistently broken runtimes |
| `runtime` crate left empty | Container/VM/subprocess impls are future tasks; no premature abstraction |
