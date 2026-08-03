# Getting started

This walks you from nothing to a working chat session.

## 1. Install the CLI

    curl -fsSL https://get.horsie.dev | sh

This installs a single binary, `horsie`, for your OS/arch.

## 2. Log in

If the server has authentication on (the default), authorize this machine once:

    horsie auth login --server https://horsie.example.com

    To authorize this machine, open:

        https://horsie.example.com/auth/device?code=PXL8-7TL7

    and confirm the code:  PXL8-7TL7

Open the link, check the code matches what your terminal printed, and approve.
Credentials land in `~/.config/horsie/credentials.json` (readable only by you)
and refresh themselves as they age.

`horsie auth status` lists the servers you are logged in to, and
`horsie auth logout --server <url>` forgets one. For scripts and CI, set
`HORSIE_TOKEN` instead of logging in.

### Default server

The first server you log in to becomes your **default**: from then on,
commands that talk to a session server — `horsie session …`, `horsie agent …`,
`horsie connect`, `horsie auth login` — work without `--server`, targeting
the default. A later login never moves the default on its own; pass
`--default` to `horsie auth login` to switch it explicitly, or manage it
directly:

    horsie config set default-server https://horsie.example.com
    horsie config get default-server
    horsie config unset default-server

With no default configured, commands fall back to the hosted service at
`https://auth.horsie.dev` instead of a local `127.0.0.1` address.

## 3. Connect to a server

Someone (maybe you) runs the horsie server somewhere — see
[Self-hosting the server](self-hosting.md) if that's you. Once you have its
address:

    horsie connect --server https://horsie.example.com --workspace .

    connected to https://horsie.example.com as vendor "local" · workspace "main" -> /Users/shawn/proj
    note: every session on this vendor works in /Users/shawn/proj; concurrent sessions will edit the same files
    open https://horsie.example.com in your browser to start a session

If that server is your default (the first one you logged in to, or set with
`horsie config set default-server`), `horsie connect --workspace .` works too.

This registers your current directory as workspace `main` and dials the
server. It uses the login from step 2 — without one, against a server with
authentication on, it stops and tells you to log in rather than retrying. Sessions can reach this machine only while the process is up, so run
it under a process manager if you want it to survive a logout. Pass
`--workspace` more than once to serve several directories, and `--name` if
more than one machine connects to the same server.

Each session gets its own runtime process, so stopping one session leaves the
others alone — but they all work in the directories you passed, so concurrent
sessions can edit the same files.

## 4. Open the web UI, create a session

Browse to the server's URL. On first visit you'll need a provider/model in
**Settings** (your admin may have already done this). Then **New** → pick a
model → **Create**, and start chatting. Press **Stop** to interrupt a run.

From here:

- [Sessions](sessions.md) — everything the chat view and New Session dialog offer.
- [GitHub](github.md) — run sessions against real repositories.
- [MCP servers](mcp-servers.md) and [Skills & plugins](skills-and-plugins.md) —
  give agents more tools and capabilities.

## Manual / advanced setup

`horsie connect` is a **vendor agent**: it holds one connection to the server and spawns a `horsie-runtime` child per session. Running `horsie-runtime` by hand no longer connects it to a server — the runtime talks only to its agent now. Runtimes are sandboxed by default with the vendor's baseline capability spec (the agent probes sandbox support at startup and refuses to start on a host that can't be confined); pass `--no-sandbox` to run unsandboxed. See [Runtime vendors](runtime-vendors.md) for the full picture, including the managed **velos** option and how to write another vendor.

Building the CLI from source instead of the install script:

    make build-cli
    make install-cli

## Troubleshooting

- **"Couldn't load settings / bundles"** in the UI — the browser can't reach
  the server. Confirm it's running and that the URL you opened matches its
  `--addr`.
- **A session won't run a turn** — you have no active runtime. Check that
  `horsie connect` (or a velos vendor) is connected and that a model is set.
  See [Runtime vendors](runtime-vendors.md).
- **Your runtime doesn't appear in Settings** — confirm the `horsie connect`
  process is still running. Registrations are in-memory only, so a server
  restart drops them; the agent reconnects on its own within about half a
  minute, and prints each attempt to its terminal.
