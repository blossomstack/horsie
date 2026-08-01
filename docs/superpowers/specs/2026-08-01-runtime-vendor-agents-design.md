# Runtime vendor agents

Replace the two unrelated ways the server acquires a runtime with one: an
external **vendor agent** process that owns runtime lifecycle and speaks a
single protocol to the server.

Diagram: <https://excalidraw.com/#json=C-GCfcXic1QswawzUK_92,hiLuItx8V4mdB9zJCHUtTg>

## Why

Today two paths do the same job, and neither is a vendor in the sense the
name implies.

`LocalDaemonVendor` has no lifecycle at all. A user launches one
`horsie-runtime`, it self-registers under a label at
`/api/runtime/connect?register=local`, and every session shares that one
process. `create`/`attach` look up a live transport; `stop`/`delete` are
no-ops, because no session owns the daemon. There is no sandbox, no
provisioning, and no way to reclaim anything.

`VelosVendor` puts the lifecycle in the wrong process. The server holds the
velos API token, POSTs a container per session over REST, and waits for that
container's runtime to dial back to the same route. Adding a third vendor
means adding another REST client to the server and another set of
credentials to its database.

Meanwhile `models/fluorite/executor.fl` already defines exactly the protocol
this design needs — `CreateRuntime` / `AttachRuntime` / `StopRuntime` /
`DeleteRuntime` / `QueryRuntimes`, plus `ToolCall` / `CancelToolCall` /
`ScanWorkspace` / `SessionStart` relayed through the executor, with
`RuntimeStateChanged` / `ToolResult` events returning. `Executor::run()` is a
complete WS client loop for it and `ProcessRuntimeProvider` already spawns
sandboxed `horsie-runtime` children. The server uses none of it; it shortcuts
through `InMemExecutorTransport` in-process. PR #83 proposed deleting all
1,835 lines of it; it is closed in favour of this design.

So this is mostly a promotion, not an invention: make the dead protocol the
only path, and move each vendor's lifecycle into a process that owns its own
credentials.

## Shape

One process per vendor, connected to the server by one WebSocket. Both
shipped agents have the same three parts — a `RuntimeProvider`, a listener
their runtimes dial, and a registry keyed by runtime id. A third vendor is a
new `RuntimeProvider` impl and nothing else.

```
horsie-server
  SessionActor -> RuntimeClient -> VendorRuntimeTransport{runtime_id}
                                -> VendorRegistry: name -> VendorLink
        |                                   |
        |  WS /api/vendor/connect           |  WS /api/vendor/connect
        |  VendorCommand / VendorEvent      |
        v                                   v
  horsie connect                      horsie-vendor-velos
    ProcessRuntimeProvider              VelosContainerProvider
    RuntimeListener (unix)              RuntimeListener (tcp, advertise)
    ConnectedRuntimeRegistry            ConnectedRuntimeRegistry
        |                                   |  velos REST
        v                                   v
  horsie-runtime (per session)        velos -> container: horsie-runtime
                                              (dials back to the agent)
```

Tool calls are proxied: `horsie-runtime` talks only to its agent, and the
agent talks to the server. The runtime never needs to reach the server.

One exception, taken deliberately: plugin/skill bundles keep flowing
`horsie-runtime -> server` over plain HTTP. Relaying multi-MB artifacts
through a protocol built for small JSON messages would mean adding chunking
and backpressure for no benefit today. The agent announces the artifact base
URL that is reachable *from where its runtimes actually run*, so the server
never has to guess.

## The protocol

`models/fluorite/executor.fl` becomes `models/fluorite/vendor.fl`, with
`executor` -> `vendor` throughout the package and type names so the wire
matches what the UI and docs already call these things. The command and
event sets carry over unchanged except for four things.

**`Registered` carries exactly one capability.**

```
struct VendorCapabilities { supports_provisioning: bool }
struct RegisteredEvent { vendor_name: String, capabilities: VendorCapabilities }
```

`supports_provisioning` earns its place: `settings.fl` and
`config/store.rs` already expose it so the UI adapts to a vendor without
branching on its name.

The other three `RuntimeVendor` methods — `artifact_base_url()`,
`plugins_dir_for()`, `plugins_cache_dir()` — are **deleted, not ported**.
They exist only because the server does the spawning today, so it had to
know values it has no opinion about. Once the agent spawns, each reason
evaporates:

- `artifact_base_url()` feeds one line, `PluginService::resolve`, which
  builds `{base}/api/plugin-artifacts/{hash}.zip`. The hash is already in
  `PluginArtifactRef`. Drop the `url` field from that struct and have the
  agent prefix its own base — it is connected to the server, so by
  definition it knows how its runtimes reach it. `resolve(names, base)`
  loses its second parameter.
- `plugins_dir_for()` and `plugins_cache_dir()` only produce
  `HORSIE_PLUGINS_DIR` / `HORSIE_PLUGINS_CACHE`, paths inside the agent's
  own filesystem. The agent injects them when it spawns the child.

The server keeps what is genuinely server-owned: resolving bundle names to
hashes and minting the scoped token.

**`RuntimeConfig` shrinks to what only the server can supply.** A
server-computed workspace path is meaningless to a remote agent — the
server-side `WorkspaceSpec` is already name-only, and the velos vendor is
the only thing that resolves `<workspace_root>/<name>`. Resolution moves to
the agent.

```
struct RuntimeConfig {
    workspaces: Vec<String>,        // names; the agent resolves each to a path
    env: Vec<EnvVar>,               // resolved secrets: github token,
                                    // bundle manifest, scoped plugins token
    provision: Vec<ProvisionStep>,
    sandbox_capabilities: Option<Capabilities>,
}
```

`plugins_dir` and `hook_path` are absent by the same argument, and PR #84
is already deleting them from `SessionSpec`/`RuntimeSpec` server-side on the
grounds that no vendor reads them. `horsie connect` resolves the host plugin
library itself and passes `--plugins-dir` straight to the child it spawns;
that never needed to round-trip through the server.

An agent that cannot honor a requested workspace name fails the create with
an explicit `CommandFailed`, surfacing as the session's provisioning error.

**Sandbox capabilities travel inline.** `RuntimeSpec.capabilities_file` is a
server-local path today, written to `state_dir/sessions/<id>/`. It becomes
the `Capabilities` value itself (the existing `capabilities.fl` type); the
agent writes it to its own disk and passes `--sandbox-caps`. Side effect:
local runtimes become sandboxable for the first time, opt-in via
`horsie connect --sandbox`, defaulting off to match today's behavior.

**`RegisteredEvent.executor_id` becomes `vendor_name`** — the name sessions
select by, replacing the `?register=local` label.

Everything else is already in `executor.fl` and needs no change.
`QueryRuntimes` / `RuntimesListed` become the reconnect story: the server
asks a reconnecting agent what is still alive, and sessions whose runtime is
gone fall into the existing lazy-recovery path and re-create on next wake.

Authentication is explicitly out of scope. Any process that can reach
`/api/vendor/connect` registers under a name it chooses, exactly as today.
The gap is already tracked as a self-hosting blocker and is not made worse
here.

## Server side

The session layer does not change. `RuntimeClient` wraps
`Arc<dyn RuntimeTransport>`; a new `VendorRuntimeTransport { link, runtime_id }`
implements that trait by wrapping each call in a `VendorCommand` and
correlating the reply by `request_id`. `SessionActor` keeps caching a
`RuntimeClient`, and `is_connected()` keeps working: when the link drops,
calls return `TransportError::Disconnected`, the existing latch in
`RuntimeClient` fires, and `ensure_runtime` re-acquires.

`ensure_runtime` gets *smaller*: it stops consulting three vendor methods to
build plugin env vars, and its whole `if let (Some(prov), Some(base))` block
collapses to resolving bundle names to hashes and minting the scoped token.
Where it gets the client is the only other change.

New:

- `VendorLink` — one per connected agent. Owns the WS, correlates
  `request_id`, dispatches `ToolResult` / `ScanResult` /
  `SessionStartResult` to waiters, and routes unsolicited
  `RuntimeStateChanged` to the supervisor.
- `VendorRegistry` — `name -> Arc<VendorLink>`, replacing
  `HashMap<String, Arc<dyn RuntimeVendor>>`. Populated on `Registered`,
  pruned on disconnect.
- `VendorRuntimeTransport` — the `RuntimeTransport` impl above.
- `GET /api/vendor/connect` — the raw WS upgrade, reusing the upgrade
  mechanics already in `http/runtime_connect.rs`.

Deleted:

```
server/src/vendor/mod.rs             RuntimeVendor, VendorRuntime, VendorRuntimeHandle
server/src/vendor/local.rs           LocalDaemonVendor, LocalDaemonRegistry, ConnectHook
server/src/vendor/velos.rs           VelosVendor, VelosRuntimeProvider
server/src/vendor/mock.rs            MockVendor
server/src/velos/                    REST client -> moves into horsie-vendor-velos
server/src/http/runtime_connect.rs   replaced by http/vendor_connect.rs
config store `vendors` table, the Settings velos form, POST /api/config/vendors/:name/test
```

**No in-memory transport or in-memory client survives anywhere in the
workspace** — not in the server, not behind a test feature, not in the CLI.
The only way anything reaches a runtime is a real transport.

`InMemExecutorTransport` and the whole `ExecutorClient` abstraction are
deleted. The server replaces them with `VendorLink` over a real WebSocket.
The one other caller, `supervisor/src/job_actor.rs`, is pure ceremony: it
builds `ExecutorClient::new(InMemExecutorTransport::new(provider, connected))`
and then calls exactly two things through it — `create_runtime(id, config)`
and `runtime_transport(id)` — which are a direct `provider.create(id, &config)`
and a direct `connected.runtime_transport(id)` lookup with a fake
message-passing layer in between. Calling the two directly removes the layer
and the crate.

`executor-client` therefore disappears as a crate; its one useful part,
`WsExecutorTransport::accept`, is the server side of the WS and becomes
`VendorLink`.

## The two agents

**`horsie connect`** becomes a supervising daemon rather than a one-shot
wrapper. It holds one WS to the server, binds a unix-socket
`RuntimeListenerServer`, and spawns one `horsie-runtime` child per session
via the existing `ProcessRuntimeProvider`. `--runtime-id` becomes `--name`
(old flag kept as an alias) and now names the *vendor*, not a runtime.

Every runtime it spawns points at the directory the daemon was launched in.
It announces `supports_provisioning: false` — no repo checkout, no bundle
install — matching today's contract. `stop` and `delete` become real: they
kill that session's child. Nothing is shared between sessions any more.

Because the workspace is fixed, two concurrent sessions get two processes
editing the same files. This is documented in `runtime-vendors.md`, and the
agent logs a warning when it starts a second runtime against a workspace
that already has one. No UI gate — the failure mode is visible and the fix
(one agent per directory) is the user's.

**`horsie-vendor-velos`** is a new binary crate holding the velos REST
client moved out of the server. It binds a TCP `RuntimeListenerServer` on an
advertise address reachable from velos's container network, schedules one
container per session, and lets each container's runtime dial back to *it*
rather than to the server. It announces `supports_provisioning: true`.

Its config — velos URL, token, image, runtime binary path, workspace root,
CPU, memory, connect timeout — moves entirely into the agent's own flags and
config file. The server stores none of it.

## Consequences

- **`horsie-runtime --endpoint .../api/runtime/connect?register=local` stops
  working.** The route is gone. This is the documented way to connect a
  machine, so `runtime-vendors.md`, `getting-started.md`, and
  `self-hosting.md` all need rewriting.
- **velos loses its Settings form and Test-connection button.** Settings ->
  Runtimes becomes a read-only list of live vendors (name, capabilities,
  runtime count, connected since) plus the existing default-vendor picker.
  Keeping a form that wrote to a table the server no longer reads would be
  worse than removing it. Deploying velos now means running a second
  process; the ops repo needs a service for it.
- **PR #83 (`refactor/remove-executor-ws-protocol`) is closed** — it deleted
  the code this design promotes.
- **PR #84 (`refactor/remove-legacy-plugin-chain`) is a prerequisite, not a
  conflict.** It removes the dead `plugins_dir`/`hook_path` chain from
  `SessionSpec`/`RuntimeSpec`, which is exactly the shape `RuntimeConfig`
  needs here. Merge it first.

## Testing

A `FakeVendorAgent` test helper, behind a `test-util` feature, speaks the
real `vendor.fl` protocol over a loopback WebSocket and serves scripted tool
results, scan responses, and lifecycle events. It replaces `MockVendor` in
the ~40 `session_actor` unit tests, which keep their current shape — only
the harness swaps. This is slower than an in-process double and is the
point: nothing is verified through a path production does not take.

Coverage the fake agent must support, because these are the transitions the
old design could not express:

- `create` -> `stop` -> `attach` on one session, asserting the agent saw
  three distinct signals and the second runtime is a different child.
- Agent disconnect mid-turn: `RuntimeClient.is_connected()` goes false, the
  turn fails, the next message re-acquires.
- Agent reconnect: server sends `QueryRuntimes`, agent reports a subset,
  sessions missing a runtime recover lazily.
- Two sessions on one agent, asserting `stop` on one leaves the other's
  runtime alive — the inverse of today's shared-daemon behavior.
- `CommandFailed` on an unresolvable workspace name surfaces as a session
  provisioning error, not a hang.

`cli/tests/connect_e2e.rs` and the velos vendor's fake-container tests port
to the new shape; `runtime/tests/provision_steps.rs` is unaffected (it
drives the runtime protocol, which does not change).

## Plan

Five PRs, each green on its own. `/api/runtime/connect` stays alive until
the last one, so every intermediate commit has a working way to connect a
machine and the cutover is a single reviewable step.

1. **#87 — protocol and link.** `vendor.fl` + `VendorLink` /
   `VendorAgentRegistry` / `VendorRuntimeTransport` + `GET /api/vendor/connect`
   + the `FakeVendorAgent` harness. Nothing removed.
2. **#88 — local agent.** `horsie connect` rewritten as a vendor agent with
   one runtime per session; `VendorAgent` added to the executor crate as the
   reusable half. `LocalDaemonVendor` and `LocalDaemonRegistry` deleted.
3. **#89 — velos agent.** The `horsie-vendor-velos` crate, reusing
   `VendorAgent` behind a container provider and a managed-workspace resolver.
   **Adds only** — see the revision note below.
4. **The excision.** Delete the server's `VelosVendor` and `server/src/velos/`,
   the config-store vendor rows and the Settings velos form, the
   `RuntimeVendor` trait, `MockVendor`, `InMemExecutorTransport`, and the
   `executor-client` crate. Port the `session_actor` tests onto
   `FakeVendorAgent`.
5. **Cutover.** Delete `http/runtime_connect.rs` and the
   `/api/runtime/connect` route, plus `ConnectHook` and the
   `serve_runtime_connections_with_hook` variant that only existed to feed it.
   Docs (`runtime-vendors.md`, `getting-started.md`, `self-hosting.md`) and the
   ops-repo service for the velos agent land here, so the breaking change and
   its documentation ship together.

### Revision: why the velos deletion moved out of PR 3

The original plan had PR 3 delete the server's velos vendor as it added the
agent. That is not reviewable as one change: `server/src/config/store.rs` has
~200 vendor references, and `settings.fl` plus eight files under
`clients/web/src` hang off the vendor rows it builds. Removing the vendor
forces removing the rows, which forces removing the settings schema and the UI
that renders it.

So PR 3 only adds the agent crate, and the whole server-side removal — velos,
config rows, UI, the `RuntimeVendor` trait, and the in-memory transport — lands
together in PR 4, where it is one coherent deletion rather than a change
smeared across two PRs. The cost is that the velos REST client is duplicated
for exactly one PR.

## Decisions taken, with the alternative rejected

- **Proxy tool calls through the agent** rather than keeping a direct
  runtime->server dial for tool traffic. Costs a hop per tool result and
  makes the agent relay large stdout; buys one outbound connection per
  machine and a runtime that needs no server reachability.
- **Local keeps its fixed launch directory** rather than allocating
  per-session dirs and gaining provisioning. Preserves the
  bring-your-own-machine story at the price of concurrent sessions sharing
  files.
- **velos config lives in the agent** rather than staying in the server DB
  and being pushed down. Removes the velos token from the server entirely at
  the price of the Settings UI.
- **Full replacement** rather than keeping the shared-daemon vendor
  alongside. Two paths to maintain is what produced this state.
- **Bundles keep their direct HTTP GET** rather than relaying over the
  protocol. Breaks the "runtime only talks to its agent" invariant in one
  narrow place, in exchange for not building artifact chunking.
- **No authentication**, unchanged from today. Tracked separately.
- **Three of the four vendor capabilities are deleted rather than ported.**
  `artifact_base_url` / `plugins_dir_for` / `plugins_cache_dir` are
  artifacts of the server doing the spawning; moving the spawn to the agent
  removes the reason they existed. The alternative — carrying them onto the
  wire because they exist today — would have made every future vendor
  declare three values the server does nothing with.
