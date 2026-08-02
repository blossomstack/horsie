# Getting started

This walks you from nothing to a working chat session.

## 1. Install the CLI

    curl -fsSL https://get.horsie.dev | sh

This installs a single binary, `horsie`, for your OS/arch.

## 2. Connect to a server

Someone (maybe you) runs the horsie server somewhere — see
[Self-hosting the server](self-hosting.md) if that's you. Once you have its
address:

    horsie connect --server https://horsie.example.com --workspace .

    connected to https://horsie.example.com as vendor "local" · workspace "main" -> /Users/shawn/proj
    note: every session on this vendor works in /Users/shawn/proj; concurrent sessions will edit the same files
    open https://horsie.example.com in your browser to start a session

This registers your current directory as workspace `main` and dials the
server. Sessions can reach this machine only while the process is up, so run
it under a process manager if you want it to survive a logout. Pass
`--workspace` more than once to serve several directories, and `--name` if
more than one machine connects to the same server.

Each session gets its own runtime process, so stopping one session leaves the
others alone — but they all work in the directories you passed, so concurrent
sessions can edit the same files.

## 3. Open the web UI, create a session

Browse to the server's URL. On first visit you'll need a provider/model in
**Settings** (your admin may have already done this). Then **New** → pick a
model → **Create**, and start chatting. Press **Stop** to interrupt a run.

From here:

- [Sessions](sessions.md) — everything the chat view and New Session dialog offer.
- [GitHub](github.md) — run sessions against real repositories.
- [MCP servers](mcp-servers.md) and [Skills & plugins](skills-and-plugins.md) —
  give agents more tools and capabilities.

## Manual / advanced setup

`horsie connect` is a **vendor agent**: it holds one connection to the server and spawns a `horsie-runtime` child per session. Running `horsie-runtime` by hand no longer connects it to a server — the runtime talks only to its agent now. Runtimes are sandboxed by default with the server's capability spec (the agent probes sandbox support at startup and refuses to start on a host that can't be confined); pass `--no-sandbox` to run unsandboxed. See [Runtime vendors](runtime-vendors.md) for the full picture, including the managed **velos** option and how to write another vendor.

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
