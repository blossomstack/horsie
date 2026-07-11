# Velos remote runtime vendor — design

**Status:** in progress (2026-07-10) · branch `velos-remote-runtime`
**Builds on:** PR #90 (session server + `RuntimeVendor` abstraction)

## Goal

Add an **optional, configurable** runtime vendor that runs a session's
`horsie-runtime` inside a **remote container scheduled by [velos](https://github.com/blossomstack/velos)**,
so a session's tool execution happens in an isolated cloud sandbox instead of a
local child process. Absent config → behaviour is unchanged (local-only).

## Why a vendor (not agent tools)

The `RuntimeVendor` protocol (`create`/`attach`/`stop`/`delete`) was built for
exactly this — "execution-sandbox altitude: local process, containers,
E2B-class cloud." Modeling remote execution as a *vendor* keeps the agent's
tools (bash/read/write) unchanged and transparent, reuses the whole executor
assembly, and maps cleanly onto the optional/configurable requirement. (An
agent-facing "launch a remote sandbox" toolset is a possible future layer that
would reuse these same building blocks.)

## The key insight: reverse-dial

velos has **no inbound container networking** (no port publishing, no container
IP, no exec). But it doesn't need any: horsie's runtime **dials out** —
`horsie-runtime --endpoint ws://<server>` connects *to* the server's TCP
listener (`RuntimeEndpoint::Tcp`), announces `RuntimeReady { runtime_id }`, and
the server registers a `SocketRuntimeTransport` in `ConnectedRuntimeRegistry`
keyed by `runtime_id`. velos already gives containers outbound NAT. So the
vendor works against **stock velos** using only `ContainerSpec.{image, command,
env, resources}`.

```
horsie-server                              velos control plane + worker
┌───────────────────────────┐             ┌──────────────────────────────┐
│ VelosVendor                │  POST       │ POST /api/v1/containers       │
│  ├─ shared TCP listener ◀──┼──dial-back──┤   spec.command =              │
│  ├─ ConnectedRuntimeReg    │  (ws://)    │   horsie-runtime --endpoint   │
│  └─ VelosClient (REST) ────┼────────────▶│   ws://<advertise>:<port> ... │
└───────────────────────────┘             │  → container (micro-VM)       │
   runtime_id demux                        └──────────────────────────────┘
```

## Integration seam: `RuntimeProvider`

`LocalProcessVendor` = executor assembly + `ProcessRuntimeProvider` (spawns a
child). The velos vendor swaps **only the provider**: a `VelosRuntimeProvider`
that provisions a container instead of a process. Everything else
(`ConnectedRuntimeRegistry`, `serve_runtime_connections`, `InMemExecutorTransport`,
`ExecutorClient`, `SocketRuntimeTransport`) is reused unchanged.

Difference from local: **one shared TCP listener** per vendor (not one Unix
socket per session) — all containers dial the same address; the registry
demultiplexes by `runtime_id`.

## Lifecycle mapping (velos containers are ephemeral)

| Vendor signal | velos action |
|---|---|
| `create`  | POST container (`restartPolicy: Never`, cmd = dial-back runtime); await `RuntimeReady` |
| `stop`    | DELETE container (frees the worker). Durable session state stays server-side. |
| `attach`  | POST a **fresh** container (same `runtime_id`); conversation recovers from the journal |
| `delete`  | DELETE container (idempotent; callable with no live handle after a restart) |

Consequence: the in-container workspace filesystem is **not** preserved across
stop (velos has no volumes). Acceptable for a fresh cloud sandbox — the
conversation/journal is the durable truth and recovers on attach. Documented.

**Shared registry, per-incarnation ids.** The one shared `ConnectedRuntimeRegistry`
is keyed by a **unique incarnation id** (`<session_id>-<nonce>`) that each
container announces, not by the session id — so an old container's asynchronous
disconnect can never unregister a freshly-attached incarnation. The velos object
**name** stays deterministic (`horsie-<session_id>`), so a container is still
reclaimable by name after a server restart. A very rapid stop→attach may
transiently 409 if velos hasn't finished reclaiming the old name yet; it
self-heals on the next message.

## What the runtime needs remotely (path problem, resolved)

`RuntimeSpec` inputs are local paths, meaningless in a container:
- **capabilities_file / `--sandbox-caps`** → omitted. The container *is* the
  isolation boundary (no nono); the image is built `--no-default-features`.
- **plugins_dir / hook_path** → omitted (no shared plugin library remotely, MVP).
- **workspaces** → names mapped to in-container dirs `<workspace_root>/<name>`
  (default root `/workspace`), created by the container command. Host paths
  ignored (fresh remote workspace).

## Components (all in `horsie-server`, no new crate)

1. `velos/client.rs` — `ContainerApi` trait (mockable seam) + `VelosClient`
   (reqwest REST: `POST`/`DELETE /api/v1/containers`, Bearer auth, camelCase
   JSON), `ContainerLaunchSpec`, `VelosError`. Fast-fail poll of container phase.
2. `vendor/velos.rs` — `VelosRuntimeProvider` (`RuntimeProvider`): pure
   `build_container_command()` + provision/await-ready + `VelosRuntimeHandle`
   (stop = delete container). `VelosVendor` (`RuntimeVendor`): shared
   listener/registry/serve-loop + config; `create`/`attach` via
   `ExecutorClient`, `delete` via `ContainerApi`.
3. `cli/config.rs` — optional `velos: Option<VelosVendorConfig>`
   (`server_url`, `token`/`token_env`, `image`, `runtime_bin`, `advertise_host`,
   `listen`, `workspace_root`, `cpu`, `memory_mib`) + `default_vendor`.
4. `cli/serve.rs` — when `cfg.velos` present, build `VelosVendor`, register as
   `"velos"`. Plumb `default_vendor` into `AppState`; handler defaults an omitted
   `vendor` to it (was hard-coded `"local"`).
5. `docker/runtime.Dockerfile` — Linux image bundling `horsie-runtime`
   (`--no-default-features`).
6. README/docs section.

## Testing (no live velos / Apple Containerization in CI)

- `VelosClient` REST + auth + error mapping → against an in-test axum mock server.
- `build_container_command` → pure unit tests (in-container paths, no
  sandbox-caps, mkdir, shell-quoting).
- Full reverse-dial over **TCP** → `VelosVendor` with a fake `ContainerApi` that,
  instead of calling velos, spawns an in-test WS "runtime" that dials the
  vendor's listener and answers a scan/tool call. Asserts `create` yields a
  working `RuntimeClient` and `stop`/`delete` signal the fake. (This also covers
  the TCP path the Unix-socket local vendor never exercises.)
- Config parse tests; default-vendor wiring test.

## Non-goals (MVP)

Persistent remote workspaces (velos volumes), file sync host↔container, remote
plugins/hooks, hackamore in remote, an agent-facing sandbox toolset.
