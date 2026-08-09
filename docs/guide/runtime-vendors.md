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

Vendors come in two shapes, and the difference is who holds the configuration:

| Vendor | Where runtimes run | What you run | Repos & skill bundles |
| --- | --- | --- | --- |
| **`horsie connect`** | Your own machine | A process, where the runtimes are | ✗ repos/bundles; ✓ skills from a CLI-installed library |
| **velos** | velos-scheduled containers | Nothing — configured in Settings | ✓ supported |
| **Fly** | Fly Machines | Nothing — configured in Settings | ✓ supported |

> **Out of the box there is no vendor.** A session can be created, but it cannot
> run a turn until one is configured or connects. Set one up below.

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

## Cloud vendors — managed containers and machines

Some vendors need no process of your own at all. The server talks to the
substrate's API directly, so you configure them in **Settings → Runtimes →
Cloud vendors** and they are usable on the next session, with nothing to deploy
and nothing to restart.

Both provision fully: GitHub checkouts and skill bundles into the sandbox.

**velos** schedules a container per session on a
[velos](https://github.com/blossomstack/velos) backend. Fill in the velos
server URL, an API token if your velos requires one, and the runtime image.

**Fly** starts a Fly Machine per session. Fill in the Fly app (it must already
exist — horsie creates machines, not apps), a Fly API token, and the runtime
image.

Both need a **callback URL**: the `ws://` or `wss://` address a sandbox reaches
*your server* on, from wherever it runs. This is the one field with no sensible
default, and the one worth getting right first — a sandbox that cannot dial back
never becomes a runtime, and the session just waits. An address that only
resolves on the server itself (`localhost`, `127.0.0.1`) is refused when you
save, because inside a container that name means the container.

**Build the runtime image** from `docker/horsie.Dockerfile` (target `runtime`)
and push it where the substrate can pull it.

**What an idle session costs** differs, and it is the one place the two are not
interchangeable. A Fly machine is stopped when its session goes cold and started
again on the next message; it keeps its volume, so the session finds its
workspace as it left it. velos has no way to stop a container — only to delete
one, which would throw the workspace away — so an idle velos session keeps its
container, and its compute, until the session is deleted. Your history is safe
either way: the durable transcript lives on the server.

## Choosing a vendor per session

- **Default vendor** — Settings → **Runtimes** → **Default vendor** names which
  vendor new sessions use. It may name an agent that isn't connected yet; the
  preference takes effect once that agent dials in.
- **Per session** — the session config bar offers a **Runtime vendor** dropdown
  when more than one vendor is connected.

## Writing another vendor

There are two ways in, and which one you want depends on where the runtimes
have to run.

**In the server** — implement `RuntimeVendor` and `RuntimeHandle` (in the
`horsie-runtime-vendor` crate) against your substrate's API, and add a variant
to the settings union so it can be configured. Four methods: create, get,
hibernate, delete. The velos and Fly vendors are the worked examples, and are
deliberately structural twins — everything substrate-shaped sits behind one
trait so the vendor's logic is testable without a network.

**As your own process** — run `horsie connect` against a `RuntimeProvider` of
your own if the runtimes must live somewhere the server cannot reach: a private
network, a laptop, a machine holding credentials the server should not have.

## Upgrading from the old dial-in runtime

Before vendor processes, a runtime dialed the server directly:

```bash
# No longer works — the route is gone.
horsie-runtime --endpoint "ws://SERVER:3789/api/runtime/connect?register=local" ...
```

Every session shared that one runtime, which is why stopping a session couldn't
tear anything down. Replace it with `horsie connect` as shown above; the
`--workspace` flags carry over unchanged and `--runtime-id` becomes `--name`.

If you ran `horsie-velos-runtime` as a separate process, it is gone: velos moved
into the server. Configure it under Settings → Runtimes → **Cloud vendors**
instead. The flags map across directly — `--velos-url` → **Server URL**,
`--image` → **Runtime image**, `HORSIE_VELOS_TOKEN` → **API token**, and the
sizing flags to their matching fields.

The one that does *not* map is `--advertise`. That named the agent's own
dial-back listener; there is no such listener now, because runtimes dial the
server. Put your server's own externally-reachable `ws://`/`wss://` address in
**Callback URL**.
