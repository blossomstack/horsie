# Runtime vendors

Every session runs its tools inside a **runtime** — a sandbox where the agent
reads files, runs commands, and (optionally) clones repositories. A **runtime
vendor** is a source of runtimes. The server ships two:

| Vendor | Where it runs | Repos & skill bundles | Best for |
| --- | --- | --- | --- |
| **local** | A daemon you launch on the server host | ✗ not supported | Quick local use, a fixed working directory |
| **velos** | Ephemeral containers on a [velos](https://github.com/blossomstack/velos) cluster | ✓ supported | Running against GitHub repos, isolation per session |

> **Out of the box there is no active runtime.** A session can be created, but it
> cannot run a turn until at least one vendor is available. Set one up below.

## The `local` vendor

The local vendor is a `horsie-runtime` daemon that you start yourself on the
server host. It dials back to the server and registers as a selectable vendor.

**Enable it** in `config.json`:

```jsonc
{ "local_runtime": true }
```

Without this, the server rejects the daemon's connection with `403`.

**Launch the daemon:**

```bash
horsie-runtime \
  --endpoint "ws://127.0.0.1:3789/api/runtime/connect?register=local" \
  --runtime-id local \
  --workspace main=/path/to/working/directory
```

- `--endpoint` — the server's HTTP address with `/api/runtime/connect?register=local`.
  Use the address other machines can reach if the daemon runs elsewhere.
- `--runtime-id` — the name the vendor appears under in the UI. Use `local` to
  match the server's default vendor, so sessions pick it automatically.
- `--workspace name=path` — the working directory the agent operates in
  (repeatable). At least one is required.

Keep the process running; sessions use it while it's connected.

**What the local vendor does *not* do:** it can't check out GitHub repos or
install skill/plugin bundles, and it runs in the fixed working directory you gave
it (there's no per-session provisioning). Session **stop** and **delete** don't
tear anything down — the daemon keeps running and is shared across sessions. If
you need per-session repos or bundles, use velos.

## The `velos` vendor

The velos vendor schedules a fresh runtime **container** per session on a
[velos](https://github.com/blossomstack/velos) cluster. It supports full
provisioning: it can check out GitHub repositories and install skill/plugin
bundles into the sandbox.

**Configure it in the UI** — Settings → **Velos remote runtimes** → add a vendor:

| Field | Meaning |
| --- | --- |
| **Name** | How the vendor appears when picking a runtime |
| **Server URL** | Your velos server, e.g. `http://velos.example:8080` |
| **Runtime image** | The `horsie-runtime` container image velos should run |
| **Advertise address** | `host:port` the container uses to dial *back* to this server — must be reachable from velos's container network |
| **Token** | velos API token (entered inline) |
| Advanced | Runtime binary path, workspace root, CPU, memory (MiB), connect timeout |

Use **Test connection** on the row to check reachability and the token before
saving. Editing a vendor's server URL or advertise address changes how the
listener behaves, so the UI shows a **restart required** banner for those.

**Build the runtime image** from `docker/runtime.Dockerfile` and push it where
velos workers can pull it; set that image in the vendor config.

**How it connects:** velos containers publish no inbound ports, so the runtime
dials *back* to the server over an outbound WebSocket to the **advertise
address** — the same HTTP port everything else uses. The advertise address must
be routable from velos's container network to your server.

**Ephemeral by design:** velos has no persistent volumes, so a session's
workspace is temporary. Stopping a session deletes its container; the next
message schedules a fresh one. Your session history is safe regardless — the
durable transcript lives on the server and reconnects automatically.

## Choosing a vendor per session

- **Default vendor** — Settings → **Default vendor** picks which vendor new
  sessions use. Only *active* vendors (a connected local daemon, or a reachable
  velos vendor) are selectable. If unset, it falls back to `local`.
- **Per session** — the New Session dialog shows a **Runtime vendor** dropdown
  **only when more than one vendor is active**. With a single vendor, sessions
  just use the default silently.

## Which should I use?

- Trying things out on one machine, or you want the agent to work in a specific
  local directory → **local**.
- Running against GitHub repos, want per-session isolation, or want skill bundles
  provisioned into the sandbox → **velos**.

You can configure both and choose per session.
