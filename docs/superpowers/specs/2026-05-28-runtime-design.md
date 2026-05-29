# Runtime Design

**Date:** 2026-05-28

## Overview

The runtime is a binary that runs inside a sandbox. It is started by the executor, connects back to the executor via WebSocket, and executes tools on behalf of an agent running server-side. Tool calls flow:

```
Server (Agent) → [RuntimeClient w/ ExecutorWsTransport] → ExecutorClient → Executor → [raw routing] → Runtime binary
```

The executor acts as a relay between the server and the runtime. The runtime never communicates directly with the server. `RuntimeClient` (with `ExecutorWsTransport`) is used server-side to build tools; the executor does raw message forwarding and does not depend on `runtime-client`.

## Topology

```
Server  <──WS──>  Executor  <──WS──>  Runtime binary
  │                  │
  │  (existing)      │  (new: executor runs WS listener
  │  executor.fl     │   for incoming runtime connections)
  │  protocol        │   runtime.fl protocol
```

- **Server → Executor**: existing `executor.fl` protocol, extended with tool call commands
- **Executor → Runtime**: new `runtime.fl` protocol over a second WS listener on the executor

## Protocol layer (fluorite)

### New `fluorite/runtime.fl`

Wire protocol between the executor and the runtime binary.

```
package runtime;

// Tool inputs
struct BashInput          { command: String }
struct ReadFileInput      { path: String, start_line: Option<u64>, end_line: Option<u64> }
struct WriteFileInput     { path: String, content: String }
struct EditFileInput      { path: String, old_text: String, new_text: String }
struct ReplaceInFileInput { path: String, replacement: String, mode: ReplaceMode }
struct ListFilesInput     { path: String }
struct GlobInput          { pattern: String, path: Option<String>, max_results: Option<u64> }
struct GrepInput          { pattern: String, path: Option<String>, file_pattern: Option<String>, max_results: Option<u64> }

#[type_tag = "type"]
union ReplaceMode { Regex(RegexMode), Lines(LinesMode) }
struct RegexMode  { pattern: String }
struct LinesMode  { start_line: u64, end_line: u64 }

/// One variant per tool. The tag doubles as the tool name.
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

// Inbound (executor → runtime)
struct ToolCallRequest  { call_id: String, call: ToolCall }
struct CancelCallRequest { call_id: String }

#[type_tag = "type"]
union RuntimeInboundMessage {
    ToolCall(ToolCallRequest),
    CancelCall(CancelCallRequest),
}

// Outbound (runtime → executor)
struct ToolOutput { stdout: String, stderr: String, exit_code: i32 }
struct ToolError  { reason: String }

#[type_tag = "status"]
union ToolResult { Ok(ToolOutput), Err(ToolError) }

struct ToolCallResponse { call_id: String, result: ToolResult }

/// First message the runtime sends after connecting.
struct RuntimeReady { runtime_id: String }
```

### Extensions to `fluorite/executor.fl`

- Add `working_dir: String` to `RuntimeConfig`
- Add to `ExecutorCommand` union:
  - `ToolCall(ToolCallCmd)` — `{ runtime_id: String, call: ToolCallRequest }`
  - `CancelToolCall(CancelToolCallCmd)` — `{ runtime_id: String, call_id: String }`
- Add to `ExecutorEvent` union:
  - `ToolResult(ToolResultEvent)` — `{ runtime_id: String, call_id: String, result: ToolResult }`

## Executor extensions

The executor gains a second responsibility: hosting a WS listener that runtimes connect to.

**Startup:**
1. Executor binds a WS listener on a random port (`runtime_listener_addr`)
2. Connects outbound to the server as before
3. When creating a runtime, spawns the binary:
   ```
   october-runtime --executor-url ws://<runtime_listener_addr> \
                   --runtime-id <id> \
                   --working-dir <path>
   ```
   (working dir comes from `RuntimeConfig.working_dir`)

**Runtime connection handling:**
- New `ConnectedRuntimeRegistry`: maps `runtime_id → WsSink`
- First message from a connecting runtime must be `RuntimeReady { runtime_id }` — registers the runtime and unblocks the `create` call (transitions state to `Running`)
- If the WS connection drops: marks runtime `Failed` in the lifecycle registry, emits `RuntimeStateChanged`

**Tool call routing:**
- `ExecutorCommand::ToolCall(cmd)`: look up `cmd.runtime_id` in `ConnectedRuntimeRegistry`, forward `RuntimeInboundMessage::ToolCall` to the runtime's WS sink
- `ExecutorCommand::CancelToolCall(cmd)`: look up runtime, forward `RuntimeInboundMessage::CancelCall`
- Incoming `ToolCallResponse` from runtime WS: wrap as `ExecutorEvent::ToolResult` and send to server

**Health check:** presence of a runtime in `ConnectedRuntimeRegistry` = alive. WS disconnect triggers `Failed` state and existing restart logic.

## Runtime binary

**CLI:**
```
october-runtime --executor-url <ws://...> --runtime-id <id> --working-dir <path>
```

**Startup:**
1. Connect to executor WS
2. Send `RuntimeReady { runtime_id }`
3. Enter dispatch loop

**Dispatch loop:**
- Deserialise `RuntimeInboundMessage`
- `ToolCall`: spawn a `tokio::task`, record `AbortHandle` in in-flight map (`call_id → AbortHandle`), execute tool, send `ToolCallResponse`
- `CancelCall`: look up `AbortHandle` in map, abort task, send `ToolCallResponse { call_id, Err("cancelled") }`
- Every request gets exactly one response
- On WS close or error: exit the process

**Tool execution:**
- `bash`: `tokio::process::Command` (async; child process killed on task abort)
- File ops (`read_file`, `write_file`, `edit_file`, `replace_in_file`, `list_files`, `glob`, `grep`): `spawn_blocking`; fast enough that cancellation is best-effort

**Source layout:**
```
runtime/src/
  main.rs          — CLI parsing, WS connect, dispatch loop, in-flight task map
  tools/
    mod.rs         — dispatch: ToolCall → ToolResult
    bash.rs
    read_file.rs
    write_file.rs
    edit_file.rs
    replace_in_file.rs
    list_files.rs
    glob.rs
    grep.rs
```

Tool fn signature: `async fn exec(working_dir: &Path, input: XInput) -> ToolResult`

## `agentcore` additions

A `Tool` trait for individual tools, and `ToolboxImpl` — a generic concrete `Toolbox` impl that aggregates registered tools:

```rust
// agentcore::tool
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, input: Value) -> Result<Value, ToolCallError>;
}

pub struct ToolboxImpl {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolboxImpl {
    pub fn new() -> Self { ... }
    pub fn add(mut self, tool: impl Tool + 'static) -> Self { ... }
}

impl Toolbox for ToolboxImpl { ... }  // routes execute() by tool name
```

## New `runtime-client` crate

Pure transport layer — no tool execution. Sends tool calls to a runtime and surfaces results.

**`RuntimeTransport` trait:**
```rust
pub trait RuntimeTransport: Send + Sync {
    async fn invoke(&self, call_id: &str, call: ToolCall) -> Result<ToolResult, TransportError>;
    async fn cancel(&self, call_id: &str) -> Result<(), TransportError>;
}
```

**`RuntimeClient`:**
```rust
pub struct RuntimeClient {
    inner: Arc<dyn RuntimeTransport>,
}
impl RuntimeClient {
    pub async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, RuntimeCallError>;
    pub async fn cancel(&self, call_id: &str);
}
```

**Transport implementations:**
- `ExecutorWsTransport` (server side): wraps `ToolCallCmd` / `ToolResultEvent` over the executor WS. Maintains a pending map of `call_id → oneshot::Sender<ToolResult>`. The server's `ExecutorEventHandler` resolves pending oneshots when `ToolResultEvent` arrives.

**Individual tools** (`BashTool`, `ReadFileTool`, `WriteFileTool`, `EditFileTool`, `ReplaceInFileTool`, `ListFilesTool`, `GlobTool`, `GrepTool`) — each holds a `RuntimeClient` and implements `Tool`.

**Convenience builder:**
```rust
pub fn add_runtime_tools(toolbox: ToolboxImpl, client: RuntimeClient) -> ToolboxImpl {
    toolbox
        .add(BashTool::new(client.clone()))
        .add(ReadFileTool::new(client.clone()))
        // ...
}
```

**Usage:**
```rust
let client = RuntimeClient::new(ExecutorWsTransport::new(...));
let toolbox = add_runtime_tools(ToolboxImpl::new(), client);
let agent = Agent::builder(provider, Arc::new(toolbox)).build()?;
```

## `ExecutorClient` (server crate)

Symmetric to `RuntimeClient`. The server's interface to a connected executor. Also used by integration tests.

```rust
pub trait ExecutorTransport: Send + Sync {
    async fn invoke(&self, request_id: &str, cmd: ExecutorCommand)
        -> Result<ExecutorEvent, TransportError>;
}

pub struct ExecutorClient {
    inner: Arc<dyn ExecutorTransport>,
}
impl ExecutorClient {
    pub async fn create_runtime(&self, id: &str, config: RuntimeConfig) -> Result<...>;
    pub async fn destroy_runtime(&self, id: &str) -> Result<...>;
    pub async fn invoke_tool(&self, runtime_id: &str, call: ToolCall) -> Result<ToolResult, ...>;
    pub async fn cancel_tool_call(&self, runtime_id: &str, call_id: &str) -> Result<...>;
}
```

Default impl: `WsExecutorTransport` — the mock WS server (or the real server) accepts the executor's inbound connection and wraps it in this transport.

## End-to-end data flow

**Happy path — agent calls `bash`:**

```
Agent::run() calls toolbox.execute("bash", {"command": "ls"})
  → BashTool::execute()
  → RuntimeClient::invoke(ToolCall::Bash { command: "ls" })
  → ExecutorWsTransport: generates call_id, parks oneshot, sends ToolCallCmd to executor WS
  → Executor receives ToolCallCmd, looks up runtime_id in ConnectedRuntimeRegistry
  → Sends RuntimeInboundMessage::ToolCall to runtime WS
  → Runtime spawns task, executes bash, sends ToolCallResponse { call_id, Ok { stdout, ... } }
  → Executor receives ToolCallResponse, sends ToolResultEvent to server
  → ExecutorWsTransport resolves oneshot → BashTool returns stdout to agent
```

**Cancellation path:**

```
CancellationToken fires
  → RuntimeClient::cancel(call_id)
  → ExecutorWsTransport sends CancelToolCallCmd to executor
  → Executor sends RuntimeInboundMessage::CancelCall to runtime
  → Runtime aborts task, kills child process
  → Sends ToolCallResponse { call_id, Err("cancelled") }
  → Executor forwards ToolResultEvent → oneshot resolves with error
```

## Testing

**Unit tests (in-crate):**
- `runtime-client`: `MockTransport` verifies call serialisation, `call_id` threading, result/error surfacing
- `agentcore`: `ToolboxImpl` dispatch correctness (routes by name, `ToolCallError` for unknown tool)
- `runtime` binary: tool fn tests with `tempdir` — the only place real execution is tested

## Crate dependency graph

```
models          ← fluorite-generated types (executor.fl, runtime.fl, agent.fl, events.fl)
agentcore       ← Tool trait, ToolboxImpl, Toolbox trait, Agent
runtime-client   ← RuntimeClient, RuntimeTransport, WsTransports, BashTool, ReadFileTool, ...
                  depends on: models, agentcore
executor        ← Executor, ProcessRuntimeProvider, ConnectedRuntimeRegistry
                  depends on: models
                  (does raw WS message routing to runtimes — no runtime-client dependency)
server          ← Server, ExecutorClient, ExecutorTransport, WsExecutorTransport
                  depends on: models, runtime-client
runtime (bin)   ← october-runtime binary, tool fns
                  depends on: models
```
