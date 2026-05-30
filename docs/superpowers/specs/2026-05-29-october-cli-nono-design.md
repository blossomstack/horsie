# october CLI (run mode) with nono sandbox — design

**Date:** 2026-05-29
**Status:** Approved design, ready for implementation plan
**Revised (2026-05-29, post design-review):** env-scrub promoted to a v1 requirement;
workflow-status observation via a push notification channel (not `AgentEvent`);
single `--endpoint` arg → `RuntimeEndpoint` sum type; the executor path is *unified*
rather than frozen (generic-over-socket handler, lifecycle-only `ExecutorClient`);
`FileJournal` durability + torn-line recovery specified; nono 0.59 API confirmed.

## Goal

Add a single-process **CLI mode** to october (alongside the existing distributed
server mode) that:

- loads a workflow definition and a JSON config from files,
- runs the workflow against a given working directory,
- confines all untrusted tool execution with the [nono](https://github.com/always-further/nono)
  capability-based sandbox (Landlock on Linux, Seatbelt on macOS),
- supports suspend/resume when an agent pauses to ask the user a question.

The CLI is the **first end-to-end wiring of the full stack** in production code.
Today only `workflow/tests/workflow_e2e.rs` wires a `WorkflowActor`, and it uses a
`MockTransport` runtime client + `InMemoryJournal`.

## Non-goals

- No operator-facing WS `Server` (that stays for distributed mode).
- No SQLite / multi-run query surface (JSONL journal is enough for v1).
- No interactive TTY prompting for `ask_user` (suspend/resume instead).
- No nono-CLI wrapping or audit/snapshot/credential-proxy features (embed only).

## Threat model

The sandbox confines **untrusted, LLM-driven tool actions** (`bash`, file writes,
etc.), not a compromised october binary. The orchestrator process is *not*
sandboxed: it holds the API key and the LLM network connection by design. Only
the `october-runtime` child — which runs the tools — is sandboxed.

Crucially, the runtime child **must not inherit the orchestrator's secrets**. nono
blocks the child's network, but a tool's stdout flows back over the socket to the
orchestrator and into the next LLM turn — so `bash -c 'echo $ANTHROPIC_API_KEY'`
would exfiltrate the key into model context (and logs, and any file the agent then
writes) *even with the network blocked*. Closing that channel (env-scrub) is part
of the model, not an afterthought — see *Security notes*.

---

## Architecture

```
october process  (orchestrator — UNSANDBOXED: holds API key + LLM network)
 ├─ WorkflowActor / AgentActors  +  provider_registry (anthropic / mock)
 │     ├─ ExecutorClient(InMemExecutorTransport)     — runtime lifecycle only
 │     ├─ RuntimeClient(UnixSocketRuntimeTransport)  — tool calls (direct)
 │     ├─ TerminalSink: EventSink                    — live AgentEvents → stdout/stderr
 │     └─ workflow_events ──▶ CLI control loop       — status transitions (await/finish/fail)
 │
 ├─ Executor  (ProcessRuntimeProvider + RuntimeListenerServer + ConnectedRuntimeRegistry)
 │     └─ spawns october-runtime child ──[UNIX SOCKET]──▶ [NONO SANDBOX]
 │            (env scrubbed: no API key)                   workdir RW + connect(socket) only
 └─ FileJournal  (NEW; the durable source of truth for resume)
```

Two communication seams:

- **orchestrator ↔ executor** — in-memory channels (`InMemExecutorTransport`).
  Carries `ExecutorCommand` / `ExecutorEvent`. Used for runtime *lifecycle* only.
- **executor ↔ runtime child** — a **unix socket** (TCP/WebSocket in distributed
  mode). Carries the runtime protocol (`RuntimeInboundMessage` /
  `RuntimeOutboundMessage`).

The orchestrator's **tool calls bypass the executor entirely** in CLI mode: with an
in-process `ProcessRuntimeProvider` the runtime is a local child, so the
orchestrator talks to it directly over the unix socket via
`UnixSocketRuntimeTransport`. The executor only does lifecycle (spawn / health /
restart / destroy) and hands back the transport — **the distributed relay bridge is
never exercised in CLI mode.**

**The prior "freeze the TCP path" constraint is lifted; the two modes are
*unified*, not duplicated.** The listener accept/handshake/frame layer is generic
over the socket type (`WebSocketStream<S>`), shared by TCP and unix. Both modes
obtain a `RuntimeClient` from `runtime_transport`. The executor's distributed
relay-forwarding is *refactored* to back the relay `RuntimeTransport`; it remains
only because the client↔executor↔runtime path is genuinely two hops in distributed
mode. CLI mode runs no bridge.

**Transport mechanism and sandboxing are runtime spawn-arg decisions, not provider
traits.** `ProcessRuntimeProvider` is made transport- and sandbox-agnostic by this
change: today it carries a `listener_addr: SocketAddr` and hardcodes
`format!("ws://{addr}")` (`executor/src/process_provider.rs`). It is reworked to
carry a `RuntimeEndpoint` (sum type, below) and an optional sandbox policy, and
spawns the runtime binary with whatever it was *constructed* with:

- **CLI** constructs it with a **unix endpoint + sandbox-on**.
- **Server mode / the e2e test** construct it with a **TCP/WebSocket endpoint +
  sandbox-off** — same behavior as today, expressed through the new endpoint type.

The runtime selects its behavior purely from its arguments (see *Runtime
configuration* under the `runtime` crate). A future container / k8s / remote-VM
provider would bring its own listener + `RuntimeTransport` impl; everything
downstream only ever sees `Arc<dyn RuntimeTransport>`, so adding a provider does
not touch the registry, the `ExecutorClient`, or the CLI.

### Two clients, two transports

| Client | Purpose | Server-mode transport | CLI-mode transport |
|---|---|---|---|
| `ExecutorClient` | runtime **lifecycle** (create / destroy / `runtime_transport`) | `WsExecutorTransport` | `InMemExecutorTransport` (new) |
| `RuntimeClient` | tool calls | `ExecutorWsTransport` (relay) | `UnixSocketRuntimeTransport` (direct, new) |

The orchestrator obtains a `RuntimeClient` uniformly in both modes:

```rust
executor_client.create_runtime(run_id, RuntimeConfig { working_dir }).await?;     // lifecycle
let rt: Arc<dyn RuntimeTransport> = executor_client.runtime_transport(run_id).await?;
let runtime_client = RuntimeClient::from_arc(rt);                                  // tools
```

`RuntimeClient::from_arc(Arc<dyn RuntimeTransport>)` is a **new** additive
constructor — today only `RuntimeClient::new(impl RuntimeTransport + 'static)`
exists (`runtime-client/src/client.rs`), which can't accept the type-erased `Arc`
that `runtime_transport` returns.

What `runtime_transport` returns depends on the `ExecutorTransport` impl
(deep-module: the caller never knows whether bytes go direct or via relay):

| `ExecutorTransport` impl | `runtime_transport(id)` returns | Why |
|---|---|---|
| `InMemExecutorTransport` (CLI) | the live `UnixSocketRuntimeTransport` | executor in-process → hand back the socket-owning transport → bypass the bridge |
| `WsExecutorTransport` (server) | a relay `ExecutorWsTransport` bound to `id` | client remote → runtime socket lives on the executor → must relay |

The relay `ExecutorWsTransport` **shares the existing client↔executor WS** sender +
pending-correlation map (it does not open a new connection); it just tags frames
with `runtime_id`.

### `RuntimeTransport` impls (consistent naming triad)

| impl | role |
|---|---|
| `UnixSocketRuntimeTransport` (new) | direct to a local runtime child over `WebSocketStream<UnixStream>` |
| `ExecutorWsTransport` (exists) | relay through the executor |
| `MockTransport` (exists) | tests |

### Workflow status observation (push channel)

Tool/LLM observability and workflow control are **two distinct planes** and the CLI
needs both:

- **Live output** — `AgentEvent`s (`TextChunk`, `ToolCallStart`, `ToolComplete`,
  `RunComplete`, …; `fluorite/events.fl`) are emitted through
  `EventSink::emit(&self, AgentEvent)` (`agentcore/src/events.rs`). They are
  ephemeral and **never journaled**. The CLI's `TerminalSink` implements `emit` to
  print synchronously as the actor runs. **`AgentEvent` has no `AwaitingUserInput`
  variant** — suspension is not observable here.
- **Control flow** — `AwaitingUserInput` / `Suspended` / `Finished` / `Failed` are
  `WorkflowStatus` (`workflow/src/workflow_actor.rs`), held in the actor's
  persisted state. The e2e test observes them only by polling + replaying the
  journal (`wait_for_status`, `workflow/tests/workflow_e2e.rs`).

To give the CLI a clean, push-based control signal, `WorkflowRuntimeContext` gains
a bounded status channel:

```rust
pub workflow_events: tokio::sync::mpsc::Sender<WorkflowNotification>,
```

`WorkflowNotification` (a workflow-crate semantic type, sibling to `WorkflowStatus`):

```rust
pub enum WorkflowNotification {
    AwaitingUserInput { question: String },   // question = the `ask`-kind conclude payload
    Suspended,
    Finished { output: serde_json::Value },
    Failed { error: String },
    // (Started / Resumed optional — the CLI only needs terminal/await transitions)
}
```

The `WorkflowActor` sends on this channel **on the command path
(`handle_command`)** at the point it decides a transition — **never in
`apply_event`**, which also runs during replay and would re-fire notifications on
every recovery. The journal remains the durable source of truth; the channel is
only the live signal, so a notification sent for a transition whose `persist` then
fails (logged, state unchanged) is harmless — the next incarnation rebuilds state
from the journal. The question text rides on `AwaitingUserInput`, sourced from the
agent's `ask`-kind `conclude` tool payload. (For symmetry with `EventSink` this
could be wrapped behind an `Arc<dyn WorkflowObserver>` trait; a typed channel is
chosen to pair directly with the CLI's `recv()` control loop.)

---

## Components

### New crate: `executor-client`

Holds the lifecycle client and its transports, so the CLI does not depend on the
`server` crate (which carries the WS `Server`).

- `ExecutorTransport` trait — add `runtime_transport(runtime_id) -> Arc<dyn RuntimeTransport>`
  alongside the existing `send(request_id, cmd) -> mpsc::Receiver<ExecutorEvent>`
  (`server/src/executor_client.rs`).
- `ExecutorClient` (moved from `server`) — now **lifecycle-only**: `create_runtime`,
  `destroy_runtime`, and a delegating `runtime_transport`. The current
  `invoke_tool` / `cancel_tool_call` relay helpers are **removed** — both modes get
  tools through a `RuntimeClient` obtained from `runtime_transport`. (There is no
  `query_runtimes` today; if a query surface is wanted later it is additive.)
- `ClientError` (moved from `server`).
- `WsExecutorTransport` (moved from `server`) — its `runtime_transport` returns an
  `ExecutorWsTransport` relay bound to `runtime_id`, sharing this transport's WS
  connection + pending map.
- `server` is updated to import these from `executor-client` directly — **no
  re-export shim**.

Dependency direction: `executor-client → runtime-client` (for `RuntimeTransport`),
`executor-client → models`. The `InMemExecutorTransport` impl lives in the
`executor` crate (it needs executor internals); it implements
`executor-client::ExecutorTransport`.

### `executor` crate changes

The distributed TCP relay path is **refactored, not frozen** — the unix path is
woven in by generalizing shared code rather than duplicating it:

- **Generic connection layer.** `handle_runtime_connection` and the listener accept
  are made generic over the socket: `WebSocketStream<S>` where
  `S: AsyncRead + AsyncWrite + Unpin + Send`. Both `TcpStream` and `UnixStream`
  satisfy the bounds, so TCP and unix share one accept / `Ready{runtime_id}`
  handshake / frame-read path. After the handshake the executor's *role* differs by
  how it was constructed:
  - **in-process (CLI):** build a `UnixSocketRuntimeTransport` over the accepted
    link and `register_transport(id, Arc::new(transport))` for direct retrieval.
  - **distributed (server):** bridge — forward `ToolCall` frames from the
    client↔executor WS to the runtime link and responses back. This is the existing
    relay, now reading frames through the shared generic helper.
- **`UnixSocketRuntimeTransport`** — owns the accepted runtime link
  (`WebSocketStream<UnixStream>` split into sink + reader) and a
  `call_id → oneshot` pending map. Implements `RuntimeTransport`:
  - `invoke(call_id, call)` → register pending, send `RuntimeInboundMessage::ToolCall`,
    await `RuntimeOutboundMessage::ToolCallResponse` correlated by `call_id`.
  - `cancel(call_id)` → send `CancelCallRequest`.
  - A spawned reader task fills the pending map; supports concurrent in-flight calls.
    On disconnect it resolves every pending call with `TransportError::Disconnected`.
- **`ConnectedRuntimeRegistry`** — stores `Arc<dyn RuntimeTransport>` per runtime
  (`register_transport(id, ..)` / `runtime_transport(id) -> Option<..>`), replacing
  the write-only `RuntimeSink` map as the unit of storage so a future provider can
  register a different transport impl. Readiness signaling (`notify_when_ready`) is
  reused; the direct handler **calls `register_transport` before signaling ready**,
  so `runtime_transport(id)` is never `None` once `create_runtime` returns.
- **`RuntimeListenerServer`** — binds either TCP or a unix socket (per the
  configured `RuntimeEndpoint`); `accept()` yields `WebSocketStream<S>`. Unix bind:
  create the run dir `0700`, **unlink any stale socket path first**, and **unlink on
  shutdown**.

`ProcessRuntimeProvider` is reworked to be transport- and sandbox-agnostic. Its
`listener_addr: SocketAddr` field is replaced by a `RuntimeEndpoint` sum type:

```rust
pub enum RuntimeEndpoint {
    Tcp(SocketAddr),   // spawns the child with --endpoint ws://<addr>
    Unix(PathBuf),     // spawns the child with --endpoint unix:<path>
}
```

It also gains an optional sandbox policy (`--sandbox` + `--sandbox-read <path>`).
The caller chooses: CLI → `Unix` + sandbox-on; server/e2e → `Tcp` + sandbox-off
(today's behavior, now type-routed).

**Env-scrub is a v1 requirement (not a TODO).** When sandbox-on, the child is
spawned with `Command::env_clear()` + a minimal allowlist (`PATH`, `HOME`,
`TMPDIR`, locale, `TERM`) so the API key and other orchestrator secrets cannot be
read by a sandboxed `bash` and returned through tool stdout. The network block does
*not* close this channel (see *Threat model*), so the scrub ships with the sandbox,
and a test asserts `ANTHROPIC_API_KEY` is absent from the child env.

### `actor` crate: `FileJournal`

Implements the existing opaque-bytes `Journal` trait (`actor/src/journal.rs`).
Constructed with a **root dir** (from config, see *Config*); writes
`<root>/runs/<persistence_id>/journal.jsonl`. The actor's `persistence_id` is the
`run_id`, so `run` and `resume` key the same file.

**Snapshots are no-op** (per design): runs are short, so we trade snapshotting for
always full-replaying the event log. Concretely:

- `persist(id, events)` — append the whole batch and flush before returning `Ok`,
  one record per line; each record is base64-encoded so the file stays strictly
  line-delimited regardless of payload bytes. Sequence numbers are implicit (1-based
  index of each **complete** line); the file is the only state. `persist` returns
  `()` (the actor runtime tracks `seq_nr` itself, advancing it only after `Ok`;
  `actor/src/runtime.rs`), so deterministic append-order is the contract `FileJournal`
  must honor.
- `replay(id, after_seq)` — stream records whose line index `> after_seq`, in order,
  base64-decoded back to `Vec<u8>`. **A trailing partial / non-decodable line is
  ignored** (truncated): a process killed mid-write leaves a torn final line for an
  event whose `persist` never returned `Ok`, so it was never counted by the actor;
  dropping it keeps the 1-based line ↔ seq invariant intact.
- `save_snapshot` / `copy_snapshot` / `delete_events_before` — **no-op**
  (`Ok(())`, nothing written). `latest_snapshot` — returns `Ok(None)`, so
  `spawn_root`'s recovery starts from `initial_state` and replays the full log
  (`after_seq = 0`) every time.
- `clear(id)` — remove the run's `journal.jsonl` (test helper).

Consequence: recovery is always a full replay from event 0. Acceptable for CLI run
lengths. (Fork — which relies on `copy_snapshot` — is not a CLI feature, so the
no-op is fine; if fork is ever wanted in CLI mode, `copy_snapshot` would copy the
`journal.jsonl` instead.)

### `runtime` crate (the sandboxed child)

**Runtime configuration — how the runtime knows its transport and whether to
sandbox.** Everything is driven by spawn args (set by `ProcessRuntimeProvider`):

- **Transport:** a single `--endpoint <value>` argument, parsed by scheme into the
  runtime's transport selection — `ws://<host:port>` → WebSocket/TCP, `unix:<path>`
  → unix socket. One argument with exactly one value makes "both / neither" endpoint
  unrepresentable at the CLI surface (vs. two optional flags). This replaces the
  current `--executor-url`; the only caller of the runtime binary's CLI is
  `ProcessRuntimeProvider`, so it is an internal rename.
- **Sandbox:** `--sandbox` requests confinement; absent → no nono (today's
  behavior). When `--sandbox` is given, capabilities are built from:
  - system paths (RO) — per-platform set required by the toolchain,
  - `--working-dir` (ReadWrite),
  - the executor unix socket path — `CapabilitySet::allow_unix_socket(path, UnixSocketMode::Connect)`,
  - each `--sandbox-read <path>` (RO) — from the CLI config's `sandbox.extra_read_paths`.

Sequence in `main.rs`: parse args → if `--sandbox`, build the `CapabilitySet` and
call `Sandbox::apply(&caps)?` **before** connecting or running any tool.
(`Sandbox::apply` returns `Result<SeccompNetFallback>` on Linux and `Result<()>` on
macOS; `Sandbox::apply(&caps)?;` discarding the value compiles on both.)
**Fail-closed:** if `Sandbox::support_info().is_supported` is false or `apply()`
errors, exit non-zero before connecting — there is **no bypass flag**; an
unsupported platform or a failed apply is a hard failure (the orchestrator surfaces
it as a clear sandbox error). Then connect on the chosen transport and run the
existing tool loop unchanged — `bash` and the file tools (`runtime/src/tools/`)
need no change; confinement is inherited by child processes.

The `--sandbox`/nono code path is behind the `runtime` crate's `sandbox` feature
(default on); a build without the feature has no `--sandbox` support and no `nono`
dependency, for unit tests and unsupported-platform development. The `nono`
dependency is taken with `default-features = false` (the sandbox/capability APIs
are not feature-gated; this skips the unneeded `system-keyring`/`keyring` dep).

### New crate: `cli` (binary `october`)

- Arg parsing (`clap`), JSON config loading, provider registry construction,
  in-process executor assembly, the three subcommands.
- `TerminalSink: EventSink` — prints `AgentEvent`s (text chunks, tool start/complete,
  run complete) to stdout/stderr as they are emitted.
- Holds the `WorkflowNotification` receiver; its control loop awaits status
  transitions to drive suspend/finish/fail (see *Subcommands → run*).

---

## Cargo features

CLI/optional-backend additions are gated only where a feature isolates a real
dependency or an optional backend — not to fence off the unix path (which has no
extra deps now that the relay path is no longer frozen). Core types and the server
path are always available.

| Crate | Feature | Gates | Default |
|---|---|---|---|
| `actor` | `file-journal` | `FileJournal` (filesystem-backed `Journal`) | off |
| `executor-client` | `ws` | `WsExecutorTransport` (pulls `tokio-tungstenite`) | on |
| `runtime` (binary) | `sandbox` | `nono` dep + `Sandbox::apply` in `main` | on |

Notes:

- The unix listener, `UnixSocketRuntimeTransport`, the generic connection handler,
  registry transport storage, and `InMemExecutorTransport` are **always compiled**
  (no `executor/local` gate — unix sockets pull no extra deps, and the relay path is
  no longer frozen).
- The `runtime_transport` accessor is on the always-on
  `ExecutorTransport`/`ExecutorClient` interface; `WsExecutorTransport` returns a
  relay transport and `InMemExecutorTransport` returns a direct one — both always
  built.
- `runtime`'s `sandbox` feature is on by default (the shipped binary supports
  `--sandbox`); it can be compiled out for unit tests or unsupported-platform dev,
  in which case `--sandbox` is unavailable and there is no `nono` dependency. There
  is **no run-time bypass flag** — when `--sandbox` is requested, an unsupported
  platform or failed apply is a hard failure.
- The `cli` crate enables `actor/file-journal` and builds/locates the
  `october-runtime` binary with `sandbox` on. It can disable `executor-client/ws`
  for a pure-local build.

## Config (JSON)

`october.json`:

```json
{
  "providers": {
    "anthropic": {
      "type": "anthropic",
      "api_key_env": "ANTHROPIC_API_KEY",
      "base_url": "https://api.anthropic.com"
    }
  },
  "models": {
    "sonnet": {
      "provider": "anthropic",
      "model_id": "claude-sonnet-4-6",
      "max_tokens": 8192
    }
  },
  "sandbox": {
    "extra_read_paths": []
  },
  "storage": {
    "root_dir": "./.october"
  }
}
```

- `storage.root_dir` is the CLI's working root for run state: per-run data lives
  under `<root_dir>/runs/<run_id>/` (`journal.jsonl`, `manifest.json`). It is
  outside any agent `--workdir`. An optional `--state-dir` flag overrides it.
- A `WorkflowAgentDef.model` is a **model key** (e.g. `"sonnet"`) →
  `models.sonnet` → `providers.anthropic`.
- The CLI config is **CLI-owned policy**, hand-written serde in the `cli` crate —
  NOT a fluorite protocol type. The workflow file remains a pure
  `WorkflowDefinition` (fluorite JSON), reusable across server/CLI.
- `provider_registry: HashMap<String, Arc<dyn LlmProvider>>` is built keyed by
  model key. For each `models.<key>`, resolve its provider's `api_key_env` to a
  value via `std::env::var` and construct (note `with_api_key` returns
  `Result<_, LlmError>`, so the chain uses `?`; `providers/anthropic/src/lib.rs`):

  ```rust
  let key = std::env::var(&provider_cfg.api_key_env)?;          // var *name* → value
  let provider = AnthropicProvider::with_api_key(key)?
      .with_base_url(&provider_cfg.base_url)
      .with_model(&model_cfg.model_id);
  registry.insert(model_key.clone(), Arc::new(provider));
  ```

  (or a mock provider for `"type": "mock"`). A missing/empty `api_key_env` fails
  here, before any runtime is spawned.

---

## Subcommands

### `october validate --workflow x.json --config october.json`

Structural + semantic checks; reports **all** errors; non-zero exit on any:

- `start` names an existing agent.
- every transition `to` names an existing agent.
- every transition `condition` parses. `eval::Expr` parses lazily at `.exec()`
  (`workflow/src/workflow_actor.rs`), so `validate` evaluates each condition once
  with a placeholder `output` binding (`eval::Expr::new(cond).value("output", json!({})).exec()`)
  and reports any that fail to **parse**. This matters because at runtime a bad
  condition is swallowed as a `tracing::warn!` and silently never matches.
- every `agent.model` ∈ config `models`.
- every referenced `model.provider` ∈ config `providers`.

### `october run --workflow x.json --config october.json --workdir DIR --input STR [--state-dir DIR]`

1. Run `validate`; abort on error.
2. Build `provider_registry`.
3. Start in-process `Executor` (`ProcessRuntimeProvider` with `RuntimeEndpoint::Unix`
   + sandbox-on, a unix `RuntimeListenerServer`, shared `ConnectedRuntimeRegistry`).
4. `ExecutorClient(InMemExecutorTransport).create_runtime(run_id, RuntimeConfig { working_dir })`
   → spawns the nono-sandboxed, env-scrubbed child.
5. `executor_client.runtime_transport(run_id)` → `RuntimeClient::from_arc(..)`.
6. Build `WorkflowRuntimeContext { provider_registry, toolbox_factory: DefaultToolboxFactory, runtime_client, event_sink: TerminalSink, workflow_events: tx }`;
   `spawn_root(WorkflowActor::new(run_id, def, ctx), FileJournal(root_dir))`; send `Start { input }`.
7. **Two planes run concurrently:** `TerminalSink::emit` prints `AgentEvent`s live
   as the actor runs, while the CLI control loop awaits `WorkflowNotification`s:

   ```rust
   while let Some(n) = workflow_events_rx.recv().await {
       match n {
           WorkflowNotification::AwaitingUserInput { question } => {
               print(question, run_id); destroy_runtime(run_id).await; exit(EXIT_AWAIT);
           }
           WorkflowNotification::Finished { output } => {
               print(output); destroy_runtime(run_id).await; exit(0);
           }
           WorkflowNotification::Failed { error } => {
               eprintln(error); destroy_runtime(run_id).await; exit(1);
           }
           WorkflowNotification::Suspended => { /* keep streaming or treat as await */ }
       }
   }
   ```

8. Per-run state at `<root_dir>/runs/<run_id>/{journal.jsonl, manifest.json}`.
   `manifest.json` = resolved workflow def + workdir (**no secrets**).

### `october resume --run <run_id> --config october.json [--state-dir DIR] [--message STR]`

1. Load `manifest.json` (def + workdir) + `FileJournal` for `run_id`.
2. Re-create a fresh sandboxed runtime for the workdir (lifecycle wiring is not
   persisted — runtime context is rebuilt every run).
3. `spawn_root(WorkflowActor::new(run_id, def, ctx), FileJournal)` — recovery
   replays the journal to reconstruct workflow state.
4. Send `Resume { message }` and drive the same two-plane loop as `run`.

---

## Error handling

- **Sandbox unavailable / apply failure** → runtime child exits non-zero before
  connecting; `create_runtime` times out / fails; CLI reports a clear sandbox
  error and exits. Fail-closed; never runs tools unconfined — there is no bypass.
- **Validation failure** → all errors printed, non-zero exit, nothing spawned.
- **Provider/credential errors** (missing `api_key_env`) → fail during registry
  build, before spawning the runtime.
- **Runtime disconnect mid-run** → `UnixSocketRuntimeTransport`'s reader resolves
  pending calls with `TransportError::Disconnected`; the agent surfaces a tool
  error; the workflow's retry/failure model applies, and a terminal
  `WorkflowNotification::Failed` ends the control loop.
- **`Resume` on a non-awaiting workflow** → the `WorkflowActor` already owns this
  (no-op / error per its status machine).

---

## Security notes

- **Unix socket > TCP loopback under nono.** One `UnixSocketCapability`
  (`UnixSocketMode::Connect`, `SocketScope::File`) grants `connect(2)` to exactly
  one socket path, versus `allow_tcp_connect(port)` opening a whole loopback port to
  any local listener. The socket lives in the run dir (outside workdir), dir mode
  `0700`, unlinked on bind and on shutdown.
- **Env-scrub (v1 requirement).** The runtime child is spawned with
  `Command::env_clear()` + a minimal allowlist when sandbox-on, so secrets in the
  orchestrator's env (notably `ANTHROPIC_API_KEY`) can't be read by a sandboxed
  `bash` and returned through tool stdout. nono's network block does **not** close
  this exfiltration channel (stdout → orchestrator → LLM context → workdir file), so
  the scrub is not optional and is asserted by a test.
- **Linux Landlock caveat.** Path-based unix socket connect is governed by a
  filesystem rule on the socket path; we use a path-based (pathname) socket, not an
  abstract one, matching nono's pathname-socket model (`UnixSocketCapability` covers
  only filesystem-backed sockets; abstract-namespace scoping needs the
  all-or-nothing Landlock `Scope::AbstractUnixSocket` on V6+ kernels, not used here).
- **macOS Seatbelt verification.** Confirm confinement holds for the real `bash`
  toolchain (compilers/git spawn helpers) during implementation; Seatbelt honors
  connect-only by leaving `bind(2)` denied under the base `(deny network*)` clause.

---

## Crate / file layout

```
cli/                         NEW   binary `october`; subcommands, config, TerminalSink,
                                   WorkflowNotification control loop; enables actor/file-journal
executor-client/             NEW   ExecutorTransport trait (+ runtime_transport), lifecycle-only
                                   ExecutorClient, ClientError, WsExecutorTransport [feat ws]
                                   (moved from server; no invoke_tool/cancel_tool_call)
executor/                          generic-over-socket connection handler (TCP + unix),
                                   UnixSocketRuntimeTransport, registry stores Arc<dyn RuntimeTransport>,
                                   InMemExecutorTransport, ProcessRuntimeProvider(RuntimeEndpoint) +
                                   env_clear scrub (v1); distributed relay refactored, not frozen
runtime/                           [feat sandbox]: nono dep (default-features=false) + Sandbox::apply
                                   in main + --sandbox/--sandbox-read, fail-closed (no bypass);
                                   single --endpoint ws://… | unix:…
actor/                             + FileJournal [feat file-journal]; no-op snapshots, torn-line-safe replay
runtime-client/                    + RuntimeClient::from_arc(Arc<dyn RuntimeTransport>); WsExecutorTransport
                                   runtime_transport-returns-relay (shares WS conn) (additive)
workflow/                          + WorkflowNotification + workflow_events channel in WorkflowRuntimeContext;
                                   emitted on the command path (not apply_event)
server/                            imports ExecutorClient/etc from executor-client; relay forwarding refactored
```

Dependency direction (acyclic): `executor-client → runtime-client`;
`executor → executor-client + runtime-client`; `server → executor-client`;
`cli → executor + executor-client + runtime-client + workflow + actor + providers + models`.

---

## Testing

- **Unit**: `FileJournal` persist/replay/recover round-trips, plus a **torn-line
  recovery** test (a trailing partial/garbage line is ignored and state still
  recovers); `UnixSocketRuntimeTransport` invoke/cancel correlation (concurrent
  calls + disconnect-resolves-pending) over a paired unix-socket harness; config
  parse + validation rules (including condition-parse failures).
- **e2e** (`tests/`): full CLI `run` against a temp workdir with `mock-llm` driving
  a tiny two-agent workflow that calls `bash`; assert tool output and a final
  result. A **suspend/resume** e2e: an agent that `ask_user`s →
  `WorkflowNotification::AwaitingUserInput { question }` arrives on the channel and
  `run` exits with the await status → `resume` injects the reply →
  `WorkflowNotification::Finished`; assert journal replay reconstructs state.
- **Env-scrub** (sandbox-on): a `bash -c 'echo "$ANTHROPIC_API_KEY"'` tool call
  returns empty — the key is absent from the child env.
- **Sandbox e2e** (platform-gated): assert a `bash` write outside `--workdir` is
  denied and inside is allowed.
- Workspace lints clean (`unwrap_used`/`expect_used`/`panic`/`wildcard_enum_match_arm`
  denied in production code).

---

## Open risks

- **nono API — confirmed against 0.59.0 source** (the version this targets):
  `Sandbox::apply`, `Sandbox::support_info().is_supported`, `CapabilitySet`,
  `CapabilitySet::allow_unix_socket(path, UnixSocketMode::Connect)` /
  `UnixSocketCapability`, and `allow_path` + `block_network` all exist with the
  assumed semantics, and `Connect`+`File`-scope is genuinely tighter than loopback
  TCP. Take the dep with `default-features = false`. *Residual* risk is the
  **system-path RO allowlist for a real toolchain** — the main ongoing maintenance
  surface; start minimal and expand from observed denials (nono exposes
  `DenialRecord` / `SandboxViolation` diagnostics to drive this).
- **macOS Seatbelt** confinement for toolchain helper spawns (compilers, git)
  remains to be verified on the real `bash` path during implementation.
