---
title: MCP servers
description: Connect remote MCP servers and enable them per session to give agents extra tools.
kind: how-to
sidebar:
  order: 4
---

[MCP](https://modelcontextprotocol.io) servers give agents tools horsie does
not ship. The server connects to **remote** MCP servers, and you enable them
per session. Their tools appear to the agent as
`mcp__<server-name>__<tool>`.

## Add one

**Settings → Integrations → MCP servers**, then add a row:

- **Name** — the id, fixed once saved. It is how the server is referred to
  everywhere.
- **URL** — the server's endpoint.
- **Auth** — **None**, a **bearer token** you paste in, or **OAuth 2.1**.

Save it.

## Check it works

Press **Test** on the row. horsie connects and lists the tools it found,
showing something like **enabled · 12 tools**. Do this before relying on it in
a session — a wrong URL and a wrong token look identical from a transcript.

## OAuth servers

For an **OAuth 2.1** server, save it and then press **Connect** — or
**Reauthorize** later. Your browser goes to the provider to authorize, and
lands back on the settings page.

Automatic client registration is supported: leave the client id blank and the
server registers itself with the provider.

## Enable one for a session

Adding a server does not force it on every session. In the session's config
row, under **MCP servers**, tick the ones that session may use.

The same picker sits on an agent preset, so every session started from that
preset gets the same set.

## Remove one

**Remove** on the row.

## Servers that come from plugins

A skill bundle can declare its own MCP servers in `.mcp.json`. Those are a
different thing from the ones here: they run **inside the runtime**, next to
the workspace, rather than in the server process, and they cannot do OAuth.
See [Skills & plugins](/using/skills-and-plugins/).

If a plugin declares a name your settings list already has, the one you
configured wins and the plugin's is ignored — logged, not silently merged.
