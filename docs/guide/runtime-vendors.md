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

## Authorizing an agent

A vendor process has to prove who it is before the server will publish it — a
name like `local` is owned by whoever claimed it, so nobody else can take it
over and start receiving your tool calls.

On a machine you sit at, a normal login is enough:

    horsie auth login --server https://SERVER-HOST

For an agent that runs unattended — a container, a CI runner, anything with
nobody to approve a code — mint a **machine token** in
**Settings → Account → Machine tokens** and pass it as `HORSIE_TOKEN`. The
secret is shown once; only its hash is stored, so there is nothing to recover
if you lose it. Revoke it from the same page.

Against a server running with authentication disabled, neither is needed and
nothing below changes.

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
- `--no-sandbox` — do not sandbox the runtimes this agent spawns. Sandboxing
  is on by default: the agent confines each runtime with its own baseline
  capability spec (workspaces read-write, system toolchain read-only, network
  allowed), probes sandbox support at startup, and refuses to start on a host
  that can't be confined unless this flag is given.

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

**One agent per name.** A name belongs to the agent holding it, for as long as
that agent is connected. Starting a second one on a name already in use stops
immediately:

```
vendor name "my-laptop" is already in use by another agent. Stop the agent
already serving it, or run with `--name <label>` to serve under a different one.
```

Your own agent reconnecting is not a collision — it reclaims its name straight
away after a network blip or a server restart. A name is released as soon as its
agent disconnects, and within 45 seconds if the machine vanishes without
hanging up (a closed laptop, a dropped VPN); the agent heartbeats every 15
seconds so the server can tell the two apart.

**What it does not do:** check out GitHub repos. It *does* load the skill
bundles a session selects — the runtime fetches them itself, which needs no
workspace to have been provisioned — see
[Skills & plugins](skills-and-plugins.md#skills-on-your-own-machine).

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

This agent needs *two* tokens, and they are unrelated: `HORSIE_VELOS_TOKEN`
authenticates it to **velos**, while `HORSIE_TOKEN` (a machine token) is how it
authenticates to **horsie**. Prefer both as environment variables over
`--velos-token`/`--token` so neither appears in argv. The agent verifies it at startup and exits if velos is unreachable or the
token is rejected, rather than letting the first session discover it.

- `--advertise` — `host:port` this agent is reachable at **from velos's
  container network**. Containers publish no inbound ports, so each container's
  runtime dials *back* to the agent on this address, and fetches skill bundles
  over it. It must be routable from velos's workers to wherever the agent runs.
- `--listen` — where the agent binds that listener (default `0.0.0.0:3790`).
- Advanced: `--runtime-bin`, `--workspace-root`, `--cpu`, `--memory-mib`,
  `--connect-timeout-secs`.

**Build the runtime image** from `docker/horsie.Dockerfile` (target `runtime`) and push it where
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

A vendor process is a `RuntimeProvider` (spawn a process, schedule a container,
call a cloud sandbox API) plus a `WorkspaceResolver` (turn a requested workspace
*name* into a path the vendor owns). Both sit behind `RuntimeVendor` in the
`horsie-runtime-vendor` crate, which owns the protocol, the runtime listener, and
relaying the runtime protocol. `horsie-velos-runtime` is the worked example: it implements
those two things and nothing else.

## Upgrading from the old dial-in runtime

Before vendor processes, a runtime dialed the server directly:

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
