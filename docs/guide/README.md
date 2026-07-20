# horsie server — user guide

**horsie server** (`horsie-server`) is a self-hosted web app for running
LLM agents. You open it in a browser, create a **session**, and chat with an
agent that runs its tools inside a sandboxed **runtime**. Sessions are durable:
the full transcript is journaled server-side and streams live to the browser, so
you can close the tab, reconnect, and pick up where you left off.

This guide covers horsie server only. It does **not** cover the separate
`horsie` CLI (`horsie job`/`horsie daemon` and workflow files) — that is a
different tool.

## What you can do

- **Chat sessions** — start a session, pick a model, watch the agent think, call
  tools, and edit files live. Stop a run mid-turn, delete a session, reconnect
  anytime. → [Sessions](sessions.md)
- **Run against GitHub repos** — connect a GitHub App once, then launch sessions
  with one or more repositories checked out into the runtime. → [GitHub](github.md)
- **Give agents more tools with MCP** — connect remote MCP servers and enable
  them per session. → [MCP servers](mcp-servers.md)
- **Ship skills & plugins** — install skill/plugin bundles from git and make them
  available to sessions. → [Skills & plugins](skills-and-plugins.md)
- **Choose where tools run** — on your own machine (the `local` runtime), or on
  managed containers the server provisions for you (`velos`).
  → [Runtime vendors](runtime-vendors.md)

## How the pieces fit

```
 Browser (web UI)
    │  HTTP + SSE
    ▼
 horsie-server ──────────────► settings database (providers, models,
    │                          vendors, GitHub, MCP, skill bundles)
    │  runs each session's tools in a…
    ▼
 Runtime vendor
    ├─ local  — a horsie-runtime daemon on your own machine, dialing back
    └─ velos  — a managed, ephemeral container the server provisions for you
```

The server holds two kinds of configuration, and never mixes them:

- **`config.json`** — deployment/bootstrap only (where data lives, which
  database, whether the local runtime is allowed). You edit this file by hand.
- **The settings database** — everything you tune day to day (model providers &
  models, runtime vendors, the default vendor, GitHub, MCP servers, skill
  bundles). You edit this from the **Settings** page in the UI.

See [Settings reference](settings-reference.md) for the exact split.

## Setup order (important)

A fresh server has nothing configured, so a brand-new session can be created but
cannot run a turn yet. Do these in order:

1. **[Run the server](getting-started.md)** and open the UI.
2. **Add a model provider and a model** (Settings → Providers, Models). Nothing
   works without at least one model.
3. **Make a runtime available** (a local daemon or a velos vendor) so sessions
   have somewhere to run tools. → [Runtime vendors](runtime-vendors.md)
4. *(optional)* Connect [GitHub](github.md), add [MCP servers](mcp-servers.md),
   install [skill bundles](skills-and-plugins.md).
5. **Create a session and chat.** → [Sessions](sessions.md)

## Guides

| Guide | For |
| --- | --- |
| [Getting started](getting-started.md) | Install, configure, and run the server; first session |
| [Self-hosting the server](self-hosting.md) | Stand up the server with Docker; manual/advanced setup |
| [Runtime vendors](runtime-vendors.md) | Local daemon vs. velos; enabling each; picking one per session |
| [Sessions](sessions.md) | Creating sessions, the chat view, per-session options |
| [GitHub integration](github.md) | Connect a GitHub App; run sessions against repos |
| [MCP servers](mcp-servers.md) | Connect remote MCP servers; enable them per session |
| [Skills & plugins](skills-and-plugins.md) | Install skill bundles; select them per session |
| [Settings reference](settings-reference.md) | `config.json` vs. the Settings database; every field |

> **No built-in authentication.** The server has no login or access control. Bind
> it to a trusted network only, and put it behind your own auth proxy if it needs
> to be reachable more widely.
