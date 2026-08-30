---
title: GitHub repositories
description: Connect a GitHub App so sessions can check out real repositories, and optionally expose GitHub's own tools.
kind: how-to
sidebar:
  order: 4
---

Connect a GitHub App once, and sessions can be launched against real
repositories. The runtime checks out the ones you picked using a short-lived,
repository-scoped token minted for that session. The token is never stored and
never reaches the browser.

Checkout needs a runtime that builds its own workspace — that means a cloud
vendor. The local runtime works in a directory you already own, so it has
nothing to check out into. See
[Cloud runtime vendors](/operating/cloud-vendors/).

## 1. Create the App

In your GitHub organisation or account settings, create a GitHub App with
**Repository permissions → Contents: Read-only**. That is enough to clone.

Note its **App ID** and **Client ID**, generate a **client secret**, and
generate a **private key** — you get a `.pem` file. Install the App on the
repositories or the organisation you want sessions to reach.

## 2. Connect it

**Settings → Integrations → GitHub.** Fill in the App ID, client ID, client
secret, and the private key — paste the raw PEM, or a base64 encoding of it.

Save the App configuration first. **Connect GitHub** stays disabled until it is
saved; once it is, press it to run the OAuth flow. The page then shows
**Connected as @your-login**.

**Disconnect** unlinks it again.

## 3. Launch a session against repositories

With GitHub connected *and* a cloud vendor selected, the session's environment
control shows a repository picker:

1. Choose one or more repositories. Filter to narrow the list; **Refresh**
   re-pulls it from GitHub.
2. Optionally set a **ref** per repository — a branch, tag, or commit. The
   default is the repository's default branch.
3. Send the first message. The runtime checks out each repository before the
   agent's first turn, and the session shows a chip per checkout.

If the picker is not there, check both conditions: GitHub connected, and a
runtime that builds its own workspace.

To fix the same set of repositories for many sessions, save them in an
[environment](/using/environments/) instead of picking them each time.

## GitHub's own tools

Once connected, the same settings page offers a **GitHub tools (MCP)** toggle.
Turning it on adds a `github` MCP server that reuses this connection for
authentication — no second credential — so an agent can work with issues and
pull requests rather than only reading a checkout.

It is enabled per session like any other MCP server. It is managed here rather
than in the MCP servers list precisely because it shares the App connection.
See [MCP servers](/using/mcp-servers/).

Read-only Contents permission is enough for checkout. Grant the App more only
if you want those tools to do more than read.
