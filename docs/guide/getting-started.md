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

    connected to https://horsie.example.com as runtime "local" · workspace "main" -> /Users/shawn/proj
    open https://horsie.example.com in your browser to start a session

This registers your current directory as workspace `main`, dials the server,
and keeps running while it's connected — sessions can only reach this
machine while this process is up (add `--background` to detach it). Run it
again from a different directory (with a different `--workspace`) to add
another workspace, or with a different `--runtime-id` if more than one
machine connects to the same server.

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

`horsie connect` wraps a standalone `horsie-runtime --endpoint ws://... --runtime-id ... --workspace ...` process — the two are equivalent, and the manual form is still useful if you don't want the `horsie` CLI's other subcommands, or need flags `connect` doesn't expose yet (`--sandbox-caps`, `--plugins-dir`, `--hook-path`). See [Runtime vendors](runtime-vendors.md) for the full picture, including the managed **velos** vendor option.

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
- **`403` when connecting** — the shared local runtime is disabled. Set
  `"local_runtime": true` in the server's config and restart it (already the
  default in `docker/docker-compose.yml`).
