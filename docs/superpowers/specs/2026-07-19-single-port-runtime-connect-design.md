# Single-port runtime connect — design

## Problem

The `horsie-server` process currently opens **three** TCP listeners:

- `3789` — the axum HTTP API + SSE + web UI.
- The **velos** vendor's reverse-dial listener (`listen`, e.g. `0.0.0.0:3790`) —
  velos-scheduled runtime containers dial back into it.
- The **shared local-runtime** vendor's reverse-dial listener
  (`local_runtime_listen`, e.g. `0.0.0.0:3791`) — user-launched
  `horsie-runtime` daemons dial back into it.

Each reverse-dial listener is a standalone `RuntimeListenerServer` (a raw
`tokio` `TcpListener` + `tokio_tungstenite` accept loop from the `horsie-executor`
crate), independent of axum. Consequences:

- Every deployment must publish, firewall, and document 2–3 ports instead of 1
  (`ops/stacks/horsie` publishes `3789`, `3790`, `3791`).
- Each velos vendor and the local registry carry their own `listen`/
  `advertise_host` config that must be kept in sync.
- The extra listeners are bound eagerly at startup and held alive only by
  `DropGuard`s inside vendor objects — a fragile lifetime coupling that has
  already produced "listener silently not bound" symptoms in deployment.

## Goal

Serve **all** runtime reverse-dial connections over the single existing HTTP
port (`3789`) as a WebSocket-upgrade route on the axum server. After this
change `horsie-server` opens exactly one listener; ops publishes exactly one
port.

Non-goal: the CLI / job-daemon path. The `horsie` job daemon connects runtimes
over **unix sockets** via `RuntimeListenerServer` (`ProcessRuntimeProvider`);
that path is single-process and unaffected. This change touches the **server**
crate's two TCP reverse-dial listeners only.

## Approach

### The reusable seam

`horsie_executor::executor::handle_runtime_connection<S>` already performs the
full per-connection lifecycle — bounded handshake wait, `Ready`/`Provisioning`
handling, race-safe `try_register_transport`, `on_registered` hook, and
deregister-on-close — and is **generic over the socket type**
(`S: AsyncRead + AsyncWrite + Unpin + Send + 'static`). It only needs a
`tokio_tungstenite::WebSocketStream<S>`.

We reuse it verbatim. The executor change is minimal: make
`handle_runtime_connection` `pub` (re-exported from the crate root) so the
server can call it. All handshake/registration/race logic stays in one place.

### The axum route

Add `GET /api/runtime/connect` to the server router. It performs a **raw hyper
WebSocket upgrade** (not axum's `WebSocketUpgrade` wrapper, whose `WebSocket`
type can't be handed to `handle_runtime_connection`):

1. Validate the `Upgrade: websocket` + `Sec-WebSocket-Key` headers; respond
   `101 Switching Protocols` with the computed `Sec-WebSocket-Accept`.
2. Spawn a task that awaits `hyper::upgrade::on(req)` → `Upgraded` IO →
   `tokio_tungstenite::WebSocketStream::from_raw_socket(TokioIo::new(upgraded),
   Role::Server, None)`.
3. Call `handle_runtime_connection(ws, shared_registry, hook)`.

This standard "manual WS upgrade over hyper" recipe keeps the executor
transport layer 100% intact.

### One shared registry

Introduce a single `Arc<ConnectedRuntimeRegistry>` in `ServerDeps`, shared by
**every** velos vendor and **every** local-daemon label — replacing the
per-vendor registries that each previously paired with a listener. All runtime
ids are unique strings (velos dial-back id `<session>-<nonce>`; local
user-chosen label), so one namespace is safe. Both vendor kinds look up their
transport in this shared registry exactly as before; only the *source* of the
connection changes (axum route instead of a private listener).

### Distinguishing velos containers from local daemons

The only behavioral difference between the two connection kinds is that a
**local daemon** connection must fire the vendor-auto-registration hook (mirror
the label into `ServerDeps.vendors` as an `Arc<dyn RuntimeVendor>`), whereas a
**velos container** connection just needs to land in the registry for a waiting
`provision()`. The route distinguishes them by a query parameter:

- `GET /api/runtime/connect?register=local` → pass the local-vendor
  registration hook to `handle_runtime_connection`.
- `GET /api/runtime/connect` (no `register`) → no hook (velos).

`register=local` is **gated** by an opt-in server flag (see config below); when
disabled the route rejects `register=local` with `403` so arbitrary daemons
cannot register themselves as vendors. Velos connections are always accepted
(the vendor must already exist and be awaiting a specific dial-back id).

## Component changes

### `horsie-executor`
- Make `handle_runtime_connection` `pub` and re-export it from the crate root.
  No behavioral change. `RuntimeListenerServer` / `serve_runtime_connections*`
  stay (still used by the CLI unix-socket path).

### `server/src/vendor/velos.rs`
- Drop the owned `RuntimeListenerServer` + `_serve_guard` + private
  `ConnectedRuntimeRegistry`. `VelosVendor::bind` becomes
  `VelosVendor::new(client, settings, shared_registry)` — no bind, no port.
- Config: remove `listen`; remove `advertise_host`; add **`advertise_address`**
  (`host:port`, the externally-reachable HTTP endpoint the worker containers
  dial). The runtime `--endpoint` the provider injects becomes
  `ws://<advertise_address>/api/runtime/connect?id=<dialback_id>`.
  (`RuntimeProvisioning`/`RuntimeReady` still carry the id in-band; the `?id=`
  query is advisory/logging only — registration keys off the handshake id as
  today.)
- `provision()` uses the shared registry's existing pending / `fail_pending`
  mechanism unchanged.

### `server/src/vendor/local.rs`
- `LocalDaemonRegistry` no longer binds a listener or holds a `DropGuard`. It
  becomes a holder for the shared registry + the `local_vendors` map + the
  `ConnectHook`, exposing that hook for the axum route to pass into
  `handle_runtime_connection` when `register=local`.
- `LocalDaemonVendor` (per-label vendor object) is unchanged in behavior.

### `server/src/config/store.rs`
- `build_vendors` / `activate_vendor` / reconcile: velos vendors are built
  against the shared registry (threaded in via `StoreDeps`), not their own
  listener. The `restart_required` special-case for changing
  `listen`/`advertise_host` collapses — there is no listener to rebind; all
  velos edits (including `advertise_address`) apply live.
- `VelosConfig` / views: `listen` + `advertise_host` → `advertise_address`.

### `server/src/bin/horsie-server/{config,main}.rs`
- Boot config: replace `local_runtime_listen: Option<String>` (a bind address)
  with `local_runtime: bool` (default `false`) — the opt-in that enables
  `?register=local`. No port.
- `main.rs`: construct the one shared `ConnectedRuntimeRegistry`, put it in
  `ServerDeps`, thread it into `DbConfigStore::open` (so velos vendors share
  it) and into `AppState` (so the route can reach it + the local hook).

### `server/src/http/mod.rs`
- Add the `GET /api/runtime/connect` route + handler module
  (`http/runtime_connect.rs`).

### Wire protocol / runtime binary
- `horsie-runtime` needs **no code change**: it already dials an arbitrary
  `--endpoint ws://…` and speaks the same `Ready`/`Provisioning` handshake. The
  endpoint URL now carries a path + query, which `connect_async` handles.

## Ops / deployment (separate `blossomstack/ops` change, coordinated)

- `stacks/horsie/docker-compose.yml`: remove the `3790` and `3791` port
  publishes and the `local_runtime_listen` config line; add
  `"local_runtime": true` to the boot config if the shared local vendor is
  wanted. Only `3789` remains.
- Re-seed / migrate the velos vendor row: `listen` + `advertise_host` →
  `advertise_address: "192.168.68.60:3789"`.
- RUNBOOK: local daemons now dial
  `ws://192.168.68.60:3789/api/runtime/connect?register=local`.

This is a breaking change to the velos vendor config schema and the runtime
dial endpoint. Server + runtime images must be rebuilt from the same commit and
deployed together with the ops change (the new server dials/serves the HTTP
path; the old images use the removed ports).

## Testing

- **Executor:** existing `handle_runtime_connection` unit tests stay green
  (unchanged logic).
- **Server (new):** an axum-level e2e in `tests/tests/session_server_e2e.rs`
  style — start `app()` with the shared registry, open a real WS client to
  `/api/runtime/connect?register=local`, send `Ready{runtime_id,workdir}`,
  assert the label appears in `ServerDeps.vendors` and a session can resolve it
  (reuses the existing fake-daemon helper, re-pointed from the removed listener
  to the HTTP route).
- **Velos path:** unit-test that `provision()` resolves against the shared
  registry when a fake connection lands via the route with no `register` flag.
- **Gating:** `register=local` with `local_runtime=false` → `403`.
- `make check` green (fmt + clippy + tests + fluorite drift).

## Risks / notes

- Raw hyper upgrade under axum must compute `Sec-WebSocket-Accept` correctly and
  not consume the body extractor before upgrading — validate against the
  `tokio-tungstenite`/`hyper` upgrade example.
- Id-namespace collision between a velos dial-back id and a local label is
  possible in principle; both are effectively unique in practice. If ever a
  concern, prefix keys by kind — deferred (YAGNI).
- The `?id=` query on velos connections is advisory only; the registry keys off
  the in-band handshake id, preserving the existing race-safety guarantees.
