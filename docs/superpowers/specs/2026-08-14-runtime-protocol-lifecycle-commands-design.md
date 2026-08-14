# Runtime protocol: lifecycle as commands, provisioning as an agent concern

Every operation the server asks of a runtime becomes a correlated request on one
path, and the unit of provisioning becomes the agent rather than the session.

## Why

Two problems, one shape.

**Provisioning is invisible.** A runtime does three things before it is usable:
it fetches the session's plugin bundles, it runs the session's provision steps,
and it announces itself. Only the middle one has any presence on the wire, and
even that is a boot-phase state machine (`Provisioning` → `ProvisionFailed` →
`Ready`) rather than a request anyone issued. Bundle fetching has none at all:
`plugins_fetch::provision_plugins` runs before the runtime dials, logs every
failure to its own stderr, and returns. A session whose skills never arrived is
indistinguishable from one that never had any. So `Ready` today asserts three
different things at once, and the one most likely to have gone wrong is the one
it does not cover.

**Provisioning is session-scoped, and agents are not.** `runtime_id` is the
session id, and the plugin manifest is baked into the create-time environment as
`HORSIE_PLUGIN_MANIFEST`. One runtime, one plugin tree, one set of skills, hooks
and MCP servers for every agent that ever runs in that session. `AgentPreset`
already has a `plugins` field, but it is flattened into the session's set at
creation and never consulted again.

Workflows are what force the issue. A workflow run is a session and each step is
an agent, and the point of a step is that it can be a *different* agent — its own
model, its own instructions, its own skills. Today a step gets whatever the
session was created with. Subagents and forks have the same limitation; it has
simply not bitten yet because they inherit the main agent's setup by design.

## What already exists

Read this before designing anything adjacent — three mechanisms here look like
gaps and are not.

**The reconciler is the timeout.** `server/src/runtime_reconciler.rs` polls each
active runtime with `Ping` every 10s and diffs the returned `in_flight` list
against the caller's own `InFlight` map. A call the runtime reports is working,
however long it takes. A call it stops reporting gets a 30s grace, then fails. A
ping unanswered inside 20s fails everything outstanding against that runtime.
This deliberately replaces per-request deadlines — the module doc explains why a
tool call has no natural bound worth timing. **No design here may introduce a
deadline parameter on the wire.**

**Operations bound themselves.** Nothing above `tools::dispatch` imposes a limit;
`bash.rs` applies its own `DEFAULT_TIMEOUT_SECS` and reports a typed timeout.
That is the pattern: the caller supplies cancellation and reconciliation, the
operation supplies its own bound.

**Disconnect is already reconciled.** `SocketRuntimeTransport`'s pending map
resolves every outstanding waiter with `TransportError::Disconnected` when the
reader task ends.

## Decisions

1. **One plugin tree per agent**, built from a content-addressed store with
   symlinks. Chosen over a shared tree with per-agent views because a filtered
   view isolates skills but not hooks or MCP servers, and those are the ones
   whose leakage is silent.
2. **The server never predicts whether an agent needs provisioning.** It sends
   `ProvisionAgent` on every agent load; the runtime makes it idempotent. The
   runtime is the only party that knows what is on its disk, and a server that
   tracks that drifts from it.
3. **`ProvisionAgent` and `ScanWorkspace` stay separate commands.** Provisioning
   writes the tree; the scan reads it. Merging them would make a re-scan
   impossible without a re-provision.
4. **Workspace provisioning becomes a command too.** `ProvisionWorkspace`
   replaces the `Provisioning`/`ProvisionFailed` boot handshake, so `Ready` means
   one thing: the process is up, confined and listening.
5. **A failed `ProvisionAgent` fails the whole command.** Provisioned means fully
   provisioned. Noted consequence: `runtime_manager.rs` falls back to the
   account's `default_names()` when a session names no plugins, so an unavailable
   *default* bundle fails every agent in every session. Accepted.

## Protocol

### `runtime.fl` — added

```
struct BundleRef { name: String, hash: String }

/// Bring this runtime's workspaces to the state `steps` describes.
/// Idempotent: a step whose effect is already present does nothing, so a
/// resumed or rebuilt runtime is provisioned by the same call as a fresh one.
struct ProvisionWorkspaceRequest { call_id: String, steps: Vec<ProvisionStep> }
struct ProvisionWorkspaceResponse { call_id: String, result: ProvisionResult }

/// Install this agent's bundle set and make it the tree its tool calls, hooks,
/// scans and MCP servers read. Idempotent: a bundle already in the store is
/// linked, not refetched.
struct ProvisionAgentRequest { call_id: String, agent_id: String, bundles: Vec<BundleRef> }
struct ProvisionAgentResponse {
    call_id: String,
    /// Absolute root of this agent's tree — what `ScanResponse.shared_root`
    /// reports for this agent from now on.
    root: String,
    result: ProvisionResult,
}

struct ProvisionOk { installed: Vec<String> }
struct ProvisionError { reason: String }
#[type_tag = "status"]
union ProvisionResult { Ok(ProvisionOk), Err(ProvisionError) }

/// The runtime abandoned this call because the caller asked it to.
///
/// Replaces the synthetic `ToolCallResponse` a `CancelCall` used to draw. That
/// answer was tool-shaped whatever the request was, so cancelling any other
/// command resolved its waiter with "the runtime answered X with the wrong
/// message" — a protocol confusion reported in place of the cancellation that
/// actually happened.
struct CancelledResponse { call_id: String }
```

Both requests join `RuntimeInboundMessage`; all three responses join
`RuntimeOutboundMessage`.

### `runtime.fl` — changed

`ScanRequest` and `McpDiscoverRequest` each gain `agent_id: String`, matching
`ToolCallRequest`. Both now resolve against that agent's tree rather than the one
shared directory. `ScanRequest.include_shared` is removed: an agent that loads no
plugins is provisioned with an empty bundle set, which says the same thing once
instead of twice.

`McpDiscoverRequest` gaining `agent_id` also makes the runtime's MCP registry
per-agent instead of per-connection. It has to be — two agents with different
bundles declare different servers.

### `runtime.fl` — removed

`RuntimeProvisioning`, `RuntimeProvisionFailed` and their `RuntimeOutboundMessage`
arms. `Ready` is the only handshake message left.

### `runtime_vendor.fl` — removed

`RuntimeSpec.provision` — steps travel in `ProvisionWorkspaceRequest` now. The
`HORSIE_PLUGIN_MANIFEST` entry in `RuntimeSpec.env` — bundles travel per agent.
`RuntimeSpec` shrinks to `workspaces` + `env` (the dial token and the session's
own variables), which removes the create-time-env-freezes-forever hazard for both.

`RuntimeVendorCapabilities.supports_provisioning` stays — it means the vendor
allocates workspace directories it owns, which is still true and still
distinguishes `horsie connect` from Fly. Its doc comment loses the mention of
provision steps.

## On-disk layout

```
<plugins_dir>/               ← the one path the vendor granted
  store/<hash>/              ← content-addressed, written once, never mutated
  agents/<agent_id>/
    <bundle_name> -> ../../store/<hash>
    .manifest.json           ← the bundle set this tree was last built from
```

`ProvisionAgent` for each ref: if `store/<hash>` is absent, fetch to a temp path
under `store/`, verify the hash, then rename into place. Then clear
`agents/<agent_id>/` and rebuild it as symlinks. Identical set → the marker
matches → no I/O at all.

Three constraints this satisfies:

- **The sandbox grants one path.** `plugins_fetch.rs` documents that the plugins
  dir is granted by path and the runtime has no write grant on its parent. Both
  the store and the per-agent trees live under it.
- **The scanner already follows symlinks.** `plugins.rs` filters with
  `Path::is_dir()`, which resolves through a link, so `discover_skills` and
  `discover_agents` work against `agents/<agent_id>/` unchanged.
- **Provisioning is cancellable, so store writes must be atomic.** A user hitting
  Stop mid-fetch aborts the task wherever it is. Fetch-verify-rename means a
  half-written directory is never named after its hash, so it can never be
  mistaken for a cache hit later.

Store entries are never garbage-collected within a runtime's life. A hibernated
runtime that resumes keeps its store, which is where the caching pays off.

## Cancellation and reconciliation

Every new command goes through `RuntimeClient` exactly as `invoke` does: mint a
`call_id`, `track()`, relay, `untrack()`. No new mechanism, no deadline.

**This also fixes a live defect.** `track()`/`untrack()` today have only two call
sites, `invoke` and `mcp_invoke`. But the runtime registers `ScanWorkspace`,
`RunHooks` and `McpDiscover` in its own in-flight map and reports all of them in
`Pong`. The reconciler cancels every reported id with no issuer in `InFlight`
(`runtime_reconciler.rs:108`), so a scan, hook run or MCP discovery still
executing when a ping lands is cancelled as an orphan. It is masked by the
`outstanding.is_empty()` short-circuit — it needs another tracked call in flight
on the same runtime, i.e. a session with a subagent working in parallel.

So `scan_workspace`, `run_hooks` and `mcp_discover` gain `track()`/`untrack()`
alongside the two new methods. All seven commands then reconcile identically, and
`cancel_in_flight()` reaches all of them instead of two.

`CancelledResponse` ships in the same change, not later. Extending `track()`
makes the cancel path *reachable* for the five commands where it is currently
dead code, so without it a Stop during provisioning surfaces as a wrong-message
error. This changes what the tool path sees — a cancelled tool call becomes
`TransportError::Cancelled` instead of `RuntimeCallError::ToolFailed("cancelled")`
— which is more accurate but is a real edit to the agent loop's error handling,
not a rename.

Each provision operation bounds itself runtime-side, as `bash` does: a step
timeout on `git_checkout`, an HTTP timeout on a bundle fetch, each reported as a
typed `ProvisionError`.

## Ordering, and a fail-closed check

Per agent load, in `session_actor/context.rs`:

1. `ProvisionAgent { agent_id, bundles }` — bundles resolved from that agent's
   own preset, falling back to the session's
2. `RunHooks(SessionStart | SubagentStart)` — hooks come from that agent's tree
3. `ScanWorkspace { agent_id }`
4. `McpDiscover { agent_id }`
5. tool calls

The runtime **refuses** a `ToolCall`, `ScanWorkspace`, `RunHooks` or
`McpDiscover` naming an `agent_id` it has never provisioned. An agent with no
plugins is still provisioned, with an empty set — "provisioned" is an explicit
state, not something inferred from a directory happening to exist. A sequencing
bug then fails loudly instead of silently running an agent with no skills, which
is the failure mode nobody can see from outside.

## Where bundle resolution moves

Out of `runtime_manager::runtime_spec` (session create) and into the agent load.
The session actor resolves the agent's preset → plugin names → `resolve()` →
`Vec<BundleRef>` → `ProvisionAgent`. This is the change that makes a workflow
step with its own preset work, and it is why `AgentPresetInput.plugins` stops
being a field that gets flattened away.

`ProvisionWorkspace` is sent by the session actor after the first successful
acquisition, on every acquisition, relying on step idempotence rather than on the
server remembering. Same principle as decision 2: the runtime is the only party
that knows whether its workspace survived a hibernate.

## What this deletes

- `RuntimeProvisioning`, `RuntimeProvisionFailed`, and their arms
- `ENV_PROVISION`, `ENV_PLUGIN_MANIFEST`, and `provision_plugins()` at boot
- `RuntimeSpec.provision`
- the drain-steps-on-first-connection dance in `serve_until_disconnected`
- `ScanRequest.include_shared`
- the synthetic `ToolCallResponse` on the cancel path

## Testing

- **Repro first, per PR.** The orphan-cancellation defect gets a failing test
  before the `track()` fix: a runtime reporting an untracked scan id while a
  tracked tool call is outstanding, asserting the scan is not cancelled.
- Runtime unit: store dedupe (two agents, one hash, one fetch); relink on a
  changed set; a re-provision with an identical set does no I/O; a bad hash fails
  the whole command; an aborted fetch leaves no `store/<hash>`.
- Runtime unit: a `ToolCall` for an unprovisioned `agent_id` is refused.
- Runtime unit: symlinked tree readable under the sandbox — see open questions.
- Host unit: all seven commands track and untrack; `cancel_in_flight` reaches
  each; a cancel resolves as `TransportError::Cancelled`.
- E2e: two workflow steps on different presets in one session, each seeing only
  its own skills. This is the test that cannot pass today, and the one that
  proves the feature.

## Sequencing

Three stacked PRs.

1. **`ProvisionWorkspace` as a command.** Kills the boot handshake, moves steps
   off `RuntimeSpec`, makes `Ready` mean one thing. Self-contained.
2. **The store-and-symlink layout, plus tracking and `CancelledResponse`.** Still
   fed by one session-wide bundle set, so no server-side scoping changes yet.
   Carries the orphan-cancellation fix and its repro.
3. **`agent_id` on the four commands, per-agent bundle resolution, the
   fail-closed check.** Where workflow steps start working.

## Open questions

- **Symlink traversal under the sandbox.** Both the store and the agent trees sit
  inside the granted path, so Landlock should resolve the target fine, and the
  macOS seatbelt profile should too. Neither is verified. PR 2 needs a test that
  runs a confined runtime against a linked tree before the layout is committed to.
- **Does dropping `include_shared` lose anything?** It is read today only as the
  `use_plugins` switch, which becomes "provision with an empty bundle set". No
  other consumer found, but the search was not exhaustive.
