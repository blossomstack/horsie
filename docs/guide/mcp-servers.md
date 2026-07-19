# MCP servers

[MCP](https://modelcontextprotocol.io) servers give agents extra tools. The
session server connects to **remote MCP servers** and lets you enable them per
session. Their tools show up to the agent as `mcp__<server-name>__<tool>`.

## Add a server

Open **Settings → MCP servers** and add a row:

- **Name** — an id for the server (fixed once saved; it's how the server is
  referenced everywhere).
- **URL** — the MCP server's endpoint.
- **Auth** — one of:
  - **None** — no credentials.
  - **Bearer token** — a static token you paste in.
  - **OAuth 2.1** — the server authorizes you via a browser flow (below).

Click **Save**.

## Test it

Click **Test** on the row. The server connects and lists the available tools,
showing something like **enabled · N tools**. Use this to confirm the URL and
auth are right before you rely on it in a session.

## OAuth servers

For an **OAuth 2.1** server, after saving click **Connect** (or **Reauthorize**
later). Your browser is redirected to the provider to authorize; on success you
land back on the Settings page and the server is ready. OAuth supports automatic
client registration — if you leave the client id blank, the server registers
itself with the provider.

## Enable a server for a session

Adding a server doesn't force it on every session. In the New Session dialog,
under **Advanced → MCP servers**, tick the servers you want available for that
session. Only servers you've enabled appear there.

## Remove a server

Click **Remove** on the row in Settings.

## The `github` server

The `github` MCP server is special: it's managed from **Settings → GitHub** (the
**GitHub tools (MCP)** toggle), not from the MCP servers list, because it reuses
your GitHub App connection for authentication. See [GitHub](github.md). It's still
enabled per session like any other MCP server.
