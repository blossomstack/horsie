# Runtime vendors

Every session runs its tools inside a **runtime** — a sandbox where the agent
reads files, runs commands, and (optionally) clones repositories. A **runtime
vendor** is a source of runtimes.

A vendor is an **agent**: a process you run that connects to the server and
manages runtimes on its behalf. The server never reaches out to a vendor; every
agent dials in. That means a vendor's own configuration — a velos URL and
token, the directories a laptop serves — lives in the agent, not in the server,
and the server's Settings page only shows which agents are connected.

Either agent reconnects on its own: a server restart or a network blip is
retried with a backoff of one second growing to thirty, indefinitely, with every
attempt printed. Runtimes already running are kept alive across the gap — only
stopping the agent stops them — but a turn that was in flight when the link
dropped has to be sent again.

The project ships two agents:

| Agent | Where runtimes run | Who runs it | Repos & skill bundles |
| --- | --- | --- | --- |
| **`horsie connect`** | Your own machine | You | ✗ repos/bundles; ✓ skills from a CLI-installed library |
| **`horsie-velos-runtime`** | velos-scheduled containers | You, once, near the server | ✓ supported |

> **Out of the box there is no vendor.** A session can be created, but it cannot
> run a turn until an agent connects. Set one up below.

## `horsie connect` — run on your own machine

Point the agent at your server and at the directory you want it to work in:

```bash
horsie connect \
  --server https://SERVER-HOST \
  --workspace main=/path/to/your/project \
  --name my-laptop
```

- `--server` — the server's HTTP(S) URL. The agent dials
  `/api/vendor/connect` on it over an outbound WebSocket; the server never
  connects to you.
- `--name` — how this machine appears when picking a runtime. Defaults to
  `local`, matching the server's default vendor. (`--runtime-id` still works as
  an alias.)
- `--workspace name=path` — a directory the agent serves, repeatable. A bare
  path becomes `main=<path>`. At least one is required.
- `--sandbox` — apply the server's sandbox policy to each runtime. Off by
  default: the machine is already yours.

Keep it running; sessions use it while it's connected. It appears in Settings →
Runtimes as soon as it dials in.

**One runtime per session.** The agent spawns a separate `horsie-runtime` child
per session, so stopping or deleting one session doesn't disturb another. When
the agent exits it kills the runtimes it started.

**Every session shares your directories.** All of them work in the paths passed
to `--workspace`, so two sessions running at once can edit the same files. The
agent prints this on startup. If you want isolation per session, use velos.

**No `--background`.** The agent is a long-lived supervisor with child
processes, so run it under a process manager (systemd, launchd, tmux) where its
lifetime and logs are managed explicitly.

**What it does not do:** check out GitHub repos, or install server-managed skill
bundles per session. It *can* load skills from a plugin library you install with
`horsie plugin install` — see
[Skills & plugins](skills-and-plugins.md#skills-on-your-own-machine-host-library).

## `horsie-velos-runtime` — managed container runtimes

This agent provisions a fresh, isolated **container** per session on a
[velos](https://github.com/blossomstack/velos) backend and tears it down when
the session ends. It supports full provisioning: GitHub checkouts and skill
bundles into the sandbox.

Run it wherever it can reach both the server and velos:

```bash
HORSIE_VELOS_TOKEN=... horsie-velos-runtime \
  --server https://SERVER-HOST \
  --name velos \
  --velos-url http://velos.example:8080 \
  --advertise AGENT-HOST:3790 \
  --image ghcr.io/you/horsie-runtime:latest
```

Prefer `HORSIE_VELOS_TOKEN` over `--velos-token` so the token never appears in
argv. The agent verifies it at startup and exits if velos is unreachable or the
token is rejected, rather than letting the first session discover it.

- `--advertise` — `host:port` this agent is reachable at **from velos's
  container network**. Containers publish no inbound ports, so each container's
  runtime dials *back* to the agent on this address, and fetches skill bundles
  over it. It must be routable from velos's workers to wherever the agent runs.
- `--listen` — where the agent binds that listener (default `0.0.0.0:3790`).
- Advanced: `--runtime-bin`, `--workspace-root`, `--cpu`, `--memory-mib`,
  `--connect-timeout-secs`.

**Build the runtime image** from `docker/runtime.Dockerfile` and push it where
velos workers can pull it.

**Ephemeral by design:** velos has no persistent volumes, so a session's
workspace is temporary. Stopping a session deletes its container; the next
message schedules a fresh one. Your session history is safe regardless — the
durable transcript lives on the server.

## Choosing a vendor per session

- **Default vendor** — Settings → **Runtimes** → **Default vendor** names which
  vendor new sessions use. It may name an agent that isn't connected yet; the
  preference takes effect once that agent dials in.
- **Per session** — the session config bar offers a **Runtime vendor** dropdown
  when more than one vendor is connected.

## Writing another vendor

A vendor agent is a `RuntimeProvider` (spawn a process, schedule a container,
call a cloud sandbox API) plus a `WorkspaceResolver` (turn a requested workspace
*name* into a path the vendor owns). Both sit behind `RuntimeVendor` in the
`horsie-runtime-vendor` crate, which owns the protocol, the runtime listener, and
relaying the runtime protocol. `horsie-velos-runtime` is the worked example: it implements
those two things and nothing else.

## Upgrading from the old dial-in runtime

Before vendor agents, a runtime dialed the server directly:

```bash
# No longer works — the route is gone.
horsie-runtime --endpoint "ws://SERVER:3789/api/runtime/connect?register=local" ...
```

Every session shared that one runtime, which is why stopping a session couldn't
tear anything down. Replace it with `horsie connect` as shown above; the
`--workspace` flags carry over unchanged and `--runtime-id` becomes `--name`.

If you configured velos in Settings, that form is gone. Move those values onto
`horsie-velos-runtime` flags: **Server URL** → `--velos-url`, **Runtime image**
→ `--image`, **Advertise address** → `--advertise`, **Token** →
`HORSIE_VELOS_TOKEN`, and the advanced fields to their matching flags.
