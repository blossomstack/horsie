# Runtime vendors

Every session runs its tools inside a **runtime** — a sandbox where the agent
reads files, runs commands, and (optionally) clones repositories. A **runtime
vendor** is a source of runtimes. The server ships two, differing mainly in
*who runs the compute*:

| Vendor | Where it runs | Who manages it | Repos & skill bundles | Best for |
| --- | --- | --- | --- | --- |
| **local** | **Your own machine** — a daemon you run, dialing back to the server | You | ✗ not supported | Working against code already on your machine |
| **velos** | Managed, ephemeral containers the server provisions for you | The server | ✓ supported | Running against GitHub repos; isolation per session |

> **Out of the box there is no active runtime.** A session can be created, but it
> cannot run a turn until at least one vendor is available. Set one up below.

## The `local` vendor — run on your own machine

The `local` vendor lets you run the runtime **on your own machine** — your laptop
or workstation, where your working files already are — and connect it back to the
server. You run a small `horsie-runtime` daemon; it dials the server over an
outbound WebSocket and registers itself as a selectable vendor. The server never
reaches into your machine; your machine reaches out to it.

This is the way to point an agent at code on your own computer. (The daemon can
run anywhere that can reach the server — including the same host as the server —
but its purpose is bring-your-own-machine compute.)

No server-side opt-in is needed: the server accepts user-launched runtimes by
default, from the same host or a remote machine. (There is no authentication on
the dial-in route, as on the rest of the API — only bind the server to networks
you trust.)

**Run the daemon on your machine**, pointing it at the server's address:

```bash
horsie-runtime \
  --endpoint "ws://SERVER-HOST:3789/api/runtime/connect?register=local" \
  --runtime-id my-laptop \
  --workspace main=/path/to/your/project
```

- `--endpoint` — the server's address with `/api/runtime/connect?register=local`.
  Replace `SERVER-HOST` with wherever the server is reachable (use `127.0.0.1`
  only if the server runs on the same machine).
- `--runtime-id` — the name the vendor shows up as in the UI (e.g. `my-laptop`).
  Use `local` if you want it to match the server's default vendor so sessions
  pick it automatically.
- `--workspace name=path` — the directory on your machine the agent works in
  (repeatable). At least one is required.

Keep the process running; sessions use it while it's connected. Once it dials in,
it appears as an active vendor in the UI.

**What the local vendor does *not* do:** it can't check out GitHub repos or
install skill/plugin bundles, and it works in the fixed directory you gave it
(there's no per-session provisioning). Session **stop** and **delete** don't tear
anything down — your daemon keeps running and is shared across sessions. If you
need per-session repos or bundles, use velos.

## The `velos` vendor — managed runtimes

The `velos` vendor is a **managed** runtime: instead of running anything
yourself, the server provisions a fresh, isolated **container** per session on a
[velos](https://github.com/blossomstack/velos) backend and tears it down when the
session ends. It supports full provisioning — it can check out GitHub
repositories and install skill/plugin bundles into the sandbox.

You configure velos once (below); after that, sessions get a runtime with nothing
to launch or babysit.

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

- You want the agent to work on code that lives **on your own machine**, and
  you're happy to run a small daemon there → **local**.
- You want a **managed** runtime with nothing to run yourself — per-session
  isolation, GitHub repo checkout, or skill bundles provisioned into the sandbox
  → **velos**.

You can configure both and choose per session.
