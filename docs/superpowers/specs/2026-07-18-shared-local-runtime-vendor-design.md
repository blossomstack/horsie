# Shared local runtime vendor — design

**Status:** proposed (2026-07-18) · branch `shared-local-runtime-vendor`
**Builds on:** PR #90 (session server + `RuntimeVendor` abstraction), PR #92 (velos vendor — reverse-dial precedent)

## Goal

Replace today's `LocalProcessVendor` (server spawns a `horsie-runtime` child
process per session, into either a managed temp dir or a caller-supplied host
path, with optional `git_checkout` provisioning) with a **shared local runtime
vendor**: a `horsie-runtime` the *user* launches by hand, pointed at a fixed
directory they already have open, that dials out to the server over WS. Any
number of sessions can attach to that one connection concurrently, all
operating on the same directory. No provisioning (no `git_checkout`, no
managed workspace) — the directory is whatever it already is.

Primary use case: a user shares a local project directory as a working
directory and runs one or more chat sessions against it, without the server
ever spawning a process or owning a workspace path.

## Why this replaces `LocalProcessVendor` outright

The old vendor's core assumption — "the server spawns and owns a runtime
process 1:1 with a session, in a dir the server also owns" — doesn't fit
"point at my already-open project dir and let me run several sessions there."
Bolting a second mode onto `LocalProcessVendor` would mean two spawn models,
two workspace-ownership models, and two provisioning models in one struct.
A separate vendor kind keeps each model simple; per user decision, the
existing managed-workspace + `git_checkout` path is dropped entirely, not kept
in parallel. Consequence: a deployment with no `velos` (or future
managed-workspace vendor) configured has **no local way to provision a
GitHub-repo session** anymore — only the bring-your-own-dir shared vendor.
Accepted as an explicit tradeoff.

## The key insight: instances are vendors, not sessions

`ServerDeps.vendors: Arc<RwLock<HashMap<String, Arc<dyn RuntimeVendor>>>>` and
`SessionSpec.vendor: String` already give every session a way to pick one
named entry out of a dynamic map — that's the exact mechanism velos and local
use today. So a connected daemon doesn't need a new "which instance" field
anywhere: **every currently-connected daemon registers itself as its own
named entry in that same map**, keyed by a label it supplies (e.g.
`"my-laptop"`). `CreateSessionRequest.vendor` selects a specific instance by
that label exactly the way it selects `"velos"` today. `runtime_id` stays
`== session_id`, unchanged, for every vendor including this one — many
sessions calling `create`/`attach` on the *same* `Arc<dyn RuntimeVendor>`
object is already how every vendor works (one `LocalProcessVendor` instance
already serves every local-vendor session). The only thing new here is that
this vendor's `create`/`attach` never spawn anything — they look up an
already-live connection and hand back a client wrapping it.

```
horsie-server                                   user's machine
┌────────────────────────────────┐             ┌───────────────────────────┐
│ LocalDaemonRegistry             │  dial-back  │ horsie runtime connect    │
│  ├─ shared TCP listener ◀───────┼─────(ws)────┤   --dir . --name my-laptop│
│  ├─ ConnectedRuntimeRegistry     │             │   --server ws://host:port │
│  │    (keyed by label, not      │             └───────────────────────────┘
│  │     session id)              │
│  └─ vendors["my-laptop"] ────────┼──▶ ServerDeps.vendors (session lookup)
└────────────────────────────────┘
        session A ─┐
        session B ─┼─▶ all call create/attach("my-laptop") independently,
        session C ─┘   share one transport, correlated by per-call `call_id`
```

## Integration seam: reuse, not rewrite

`handle_runtime_connection` (`executor/src/executor.rs`) is already fully
generic: handshake on `RuntimeReady`, register into a
`ConnectedRuntimeRegistry` keyed by whatever string the peer announces,
deregister when the socket closes. Nothing about it assumes that string is a
session id — it's already exactly what this vendor needs, keyed by label
instead. The one gap: it calls `register_transport` unconditionally
(last-write-wins), which is fine for velos (unique per-attempt incarnation
ids) but wrong here, where two different daemons could announce the same
user-chosen label. This vendor needs a **collision guard** in front of
registration: if `runtime_transport(label)` is already `Some`, reject the new
connection before calling `register_transport`, instead of silently evicting
a live, in-use connection out from under active sessions.

Everything else — `RuntimeListenerServer`, the shared TCP listener pattern,
`SocketRuntimeTransport`, `ExecutorClient`, `RuntimeClient` — is reused
unchanged from the velos vendor, confirmed at the transport layer to already
support this vendor's one real requirement:

**Verified: concurrent multi-session use of one transport is already safe.**
`SocketRuntimeTransport` correlates every `invoke`/`scan_workspace`/
`run_session_start` call by a caller-minted `call_id` (not connection order),
and a single session's `AgentActor` already runs several tool calls
concurrently against one `RuntimeClient` today
(`concurrent_invokes_each_resolve` test, `agentcore` parallel-tool-call test).
Multiple sessions sharing one label's transport is the same mechanism,
exercised by more independent callers. `RuntimeClient` is `Clone` over
`Arc<dyn RuntimeTransport>`, so each session gets its own client instance
against the same underlying connection at no extra cost.

## Lifecycle mapping

| Vendor signal | This vendor's behavior |
|---|---|
| `create` / `attach` | Look up `ConnectedRuntimeRegistry.runtime_transport(label)`. If present, wrap it in a fresh `RuntimeClient` and return `VendorRuntime`. If absent, `VendorError::Attach` (label not currently connected). No spawn, no workspace resolution — `RuntimeSpec.{workspaces, provision, env, capabilities_file, plugins_dir, hook_path}` are all ignored by this vendor (see Non-goals). |
| `stop` (`VendorRuntimeHandle::stop`) | No-op. The daemon isn't owned by any one session; stopping one session must never affect others sharing the label. |
| `delete` | No-op, for the same reason — the vendor never created the process or the directory, so it has nothing to tear down. |

Session disconnect/recovery, unchanged from the existing lazy-recovery model:
a live daemon disconnecting (network drop, ctrl-C) triggers the *existing*,
connection-owned deregistration in `handle_runtime_connection` (`registry.remove(&runtime_id)`
after the socket closes) — never a session-triggered code path. Any session
whose next `attach()` call finds the label absent gets `VendorError::Attach`,
which the existing session-actor error handling already turns into an
`Interrupted`/`RecoveryFailed` status. When the same-labeled daemon dials back
in, the label becomes resolvable again and the next user message on an
affected session succeeds and resumes normally — no new recovery machinery
needed, just the existing one fed by this vendor's failure mode.

**Invariant the whole design leans on:** `ConnectedRuntimeRegistry::remove()`
is called from exactly one place for this vendor — the connection's own
close path. No session-scoped code (`stop`, `delete`, session GC) may ever
call it. (`remove` is a bare `HashMap` removal with no reference counting, so
a session-triggered call here would evict the transport for every other
session sharing the label.)

## Components (all in `horsie-server`, no new crate)

1. **`server/src/vendor/local.rs` (rewritten)** — `LocalDaemonRegistry`: binds
   one shared TCP listener at server startup (mirrors `VelosVendor::bind`),
   accepts dial-back connections, runs the handshake with the added collision
   guard, and on success creates or reclaims a `LocalDaemonVendor` for that
   label. Keeps a concrete `HashMap<String, Arc<LocalDaemonVendor>>` (same
   pattern as velos's `BuiltVendor` side-table) and mirrors each entry into
   `ServerDeps.vendors`. Reconnect under an existing, currently-disconnected
   label reuses the same `Arc<LocalDaemonVendor>` object rather than
   replacing the map entry, so any references already resolved through
   `ServerDeps.vendors` stay valid.
2. **`LocalDaemonVendor`** — implements `RuntimeVendor` per the lifecycle
   table above. Holds just a label and a handle to the shared
   `ConnectedRuntimeRegistry`; no per-instance state beyond a cached `workdir`
   string for display.
3. **Wire addition** (`models/fluorite/runtime.fl`) — `RuntimeReady` gains a
   `workdir: String` field so the server can show "working in `/Users/x/proj`"
   for a connected label. `RuntimeOutboundMessage` union tag unchanged.
4. **CLI** — a new subcommand, e.g. `horsie runtime connect --dir <path>
   --name <label> --server <ws-url>`, that runs the runtime binary's existing
   dial-out path (`--endpoint`/`--runtime-id`) with `--workspace main=<dir>`
   derived from `--dir` and no `--sandbox-caps` (unsandboxed by design — the
   user already has whatever access their own machine and directory allow).
5. **Read endpoint** for session creation — a small new route (e.g. `GET
   /api/vendors/local/instances`) that queries `LocalDaemonRegistry` directly
   and enumerates currently-known labels with connected/disconnected +
   `workdir`, so a new-session picker can list them the way the repo picker
   lists GitHub repos. Deliberately separate from `GET /api/config` /
   `ConfigStore` (see below) rather than merged into it.
6. **`server/src/http/handlers.rs` `create_session`** — reject a request that
   combines this vendor's kind with `repos`, `provision`, or `workdirs`
   (see Non-goals) instead of silently ignoring them.

No DB/config-store involvement: unlike velos, this vendor kind has no
persisted configuration, no `VendorRow`/`kind` match arm, no Settings UI
config form, no live `reconfigure()`. The listener's bind address is a
startup-only server setting (alongside `cli/src/config.rs`'s existing
`RuntimeConfig.bin`/`hook_path`), not something edited live. A connected
label simply doesn't exist in `ServerDeps.vendors` until its daemon dials in
for the first time — no pre-registration step.

## Data flow

1. User runs the CLI in their project directory; it dials out over WS and
   sends `RuntimeReady { runtime_id: label, workdir }`.
2. `LocalDaemonRegistry` checks the label isn't already live; if free,
   registers the transport under that label in `ConnectedRuntimeRegistry`,
   creates (or reclaims) the `LocalDaemonVendor`, mirrors it into
   `ServerDeps.vendors[label]`.
3. User creates a session with `vendor: "<label>"`, picked from the list of
   currently-known labels. A session can reference a label whose daemon
   isn't connected yet — that's expected, not an error; it resolves the
   first time the daemon dials in.
4. First user message → `SessionActor` calls `vendor().create(session_id,
   spec)` as always. `LocalDaemonVendor::create` resolves the live transport,
   returns a `VendorRuntime`. No process spawned, no workspace resolved.
5. Tool calls from this session flow over the shared transport, correlated by
   per-call `call_id`; other sessions on the same label do the same
   concurrently, independently.
6. Session stop/delete: no-ops against the daemon; other sessions on the
   label are unaffected.
7. Daemon disconnects → `handle_runtime_connection`'s existing close path
   removes the transport from `ConnectedRuntimeRegistry` (the only removal
   site) → subsequent `create`/`attach` on that label fail → affected
   sessions flip to `Interrupted` via the existing lazy-recovery path.
8. Daemon reconnects under the same label → transport re-registered → the
   next message on any `Interrupted` session for that label resumes
   normally.

## Non-goals / explicit validation

- **No `git_checkout` or any provisioning.** A `CreateSessionRequest` that
  combines this vendor with `repos`, `provision` steps, or `workdirs` is
  rejected at creation time with a clear error — not silently ignored. The
  directory is whatever the daemon already has open; the request can't
  choose or populate it.
- **No sandboxing.** `RuntimeSpec.capabilities_file` doesn't apply — the
  daemon runs with whatever access the user's own machine already grants it,
  same trust level as the user running it themselves.
- **No auth/pairing beyond a label.** Per user decision, this design treats
  the dial-back connection at the same trust level as existing server access
  — first-connect-wins on an unclaimed label, reject on collision with a
  *live* one. No token issuance, no approval workflow. A future design can
  layer auth on top without changing this vendor's lifecycle model.
- **`env`, `plugins_dir`, `hook_path` are ignored** by this vendor. The
  daemon isn't spawned by the server, so there's no child-process env to
  inject and no plugin-bundle materialization step to drive from the server
  side (a daemon that wants plugins would need to fetch/apply them itself —
  out of scope here).
- **No reference counting / concurrency limits on session count per label.**
  Any number of sessions may share one label; nothing in this design caps
  that or serializes it beyond what the wire protocol's `call_id` correlation
  already provides.

## Testing

- **Unit (`LocalDaemonRegistry`)**: collision rejected while a label is live;
  reconnect under a currently-disconnected label succeeds and reuses the same
  `Arc<LocalDaemonVendor>`; disconnect clears the transport but leaves the
  label's map entry in place.
- **Unit (`LocalDaemonVendor`)**: `create`/`attach` from multiple distinct
  `runtime_id`s against one registered transport all succeed without
  interference.
- **Integration** (extend `tests/tests/session_server_e2e.rs`, reusing the
  fake-WS-runtime harness built for velos): two fake daemons under two
  labels; two sessions sharing one label plus one session on the other;
  concurrent tool calls across the shared-label sessions don't cross-talk;
  stopping/deleting one shared-label session doesn't disturb the other;
  full disconnect → `Interrupted` → reconnect → resume cycle.
- **Validation test**: a create-session request combining this vendor with
  `repos`/`provision`/`workdirs` is rejected with a clear error.

## Open questions for the implementation plan

- Exact shape of the collision-guard hook: a parameter/callback added to
  `handle_runtime_connection`, or a small parallel accept function in
  `vendor/local.rs` that duplicates the ~15-line handshake with the extra
  check. Either is consistent with this design; the plan should pick one.
- Exact response shape / route naming for the "list known labels" read
  endpoint — a web-UI-facing detail, not a vendor-lifecycle one.
