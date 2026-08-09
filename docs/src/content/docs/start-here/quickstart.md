---
title: Quickstart
description: Run the server, add a model, connect a runtime, and hold your first session.
kind: tutorial
sidebar:
  order: 2
---

This takes you from an empty machine to an agent doing work on your files.
Follow it in order — a fresh server has no model and no runtime, and a session
cannot run a turn without both.

You will need Docker, and an API key for a model provider.

## 1. Start the server

From a checkout of the repository:

```bash
git clone https://github.com/blossomstack/horsie
cd horsie
docker compose -f docker/docker-compose.yml up -d
```

The server and the web UI come up together on port 3789. There is no external
database to provision and no config file to write; data persists in a
`horsie-data` Docker volume.

## 2. Sign in

The first boot creates an `admin` account and generates a password for it:

```bash
docker compose -f docker/docker-compose.yml logs horsie | grep -A4 'admin account'
```

The same password is written to `initial-admin-password` in the server's state
directory, so a rotated log is not a lockout.

Open <http://localhost:3789> and sign in as `admin`.

## 3. Add a model

Go to **Settings → Models**.

1. Under **Providers**, add one. Give it a name, pick a **kind** — start with
   **Anthropic** or **OpenAI-compatible** — and paste your API key.
2. Under **Models**, add one. Give it an alias you will recognise in a
   dropdown, pick the provider you just made, and enter the provider's own
   model id.
3. Press **Save changes**. The Models page batches its edits, so nothing is
   stored until you do.

## 4. Give sessions somewhere to run tools

On the machine holding the code you want the agent to work on — which may be
this same machine — install the CLI and connect it:

```bash
curl -fsSL https://get.horsie.dev | sh

horsie auth login --server http://localhost:3789
horsie connect --server http://localhost:3789 --workspace .
```

`horsie auth login` prints a URL and an eight-character code. Open the URL,
check the code matches, and approve.

`horsie connect` registers the current directory as a workspace and holds a
connection open to the server. Leave it running — sessions can reach this
machine only while it is up. It appears under **Settings → Runtimes** within a
second or two of dialing in.

:::note
The agent will read and write files in the directory you passed. Point it at a
project you are happy for it to change, and one that is under version control.
:::

## 5. Hold a session

Back in the browser, press **New**.

The row of controls above the composer is the session's configuration. Pick
your **model**, and pick your machine under **Environment**. Then type a
message and send it — the session is created when you send the first message,
not when you press New.

Ask it something that requires looking around, so you can watch a tool call:

> What does this project do? Read the README and the top-level directories.

The reply streams in. Tool calls appear as rows you can expand to see the exact
input and the raw output. Press the red **Stop** key to interrupt a run
mid-turn.

## What to do next

You now have a working server, but it is doing the least it can:

- **Give the agent repositories to check out** instead of a local directory —
  connect a GitHub App and a cloud vendor. See
  [GitHub repositories](/using/github-repositories/) and
  [Cloud runtime vendors](/operating/cloud-vendors/).
- **Give the agent more tools** with [MCP servers](/using/mcp-servers/) and
  [skill bundles](/using/skills-and-plugins/).
- **Make work happen without you** — a [routine](/using/routines/) runs an
  agent against a fixed prompt on a schedule.
- **Move off the single container** — see
  [Deploying the server](/operating/deploying/) for PostgreSQL and for running
  it somewhere other than your laptop.
