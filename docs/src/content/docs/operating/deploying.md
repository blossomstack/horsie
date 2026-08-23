---
title: Deploying the server
description: Run horsie server with Docker, on Render or Fly.io, or as a binary, and choose where its data lives.
kind: how-to
sidebar:
  order: 1
---

The server is one process that serves the API and the web UI on a single port.
It needs somewhere to keep its data and, once you want it to be more than a
laptop toy, a PostgreSQL database.

## Docker Compose

From a checkout of the repository:

```bash
docker compose -f docker/docker-compose.yml up -d
```

Server and web UI on port 3789, no external database, no config file. Data
persists in a `horsie-data` Docker volume.

Then open <http://localhost:3789>, sign in, and add a provider and a model —
see [Models & providers](/operating/models-and-providers/). A fresh server has
none, and no session can run a turn without one.

## Render

[![Deploy to Render](https://render.com/images/deploy-to-render-button.svg)](https://render.com/deploy?repo=https://github.com/blossomstack/horsie)

The button runs the published image from `render.yaml` with a managed Render
PostgreSQL database wired in as `HORSIE_DATABASE_URL`. No volume is needed —
both the settings store and the session journal live in that database.

To use a different PostgreSQL instance — Neon, Supabase, RDS, anything
speaking `postgres://` — edit `HORSIE_DATABASE_URL` on the service after
deploy, or fork `render.yaml` and drop its `databases:` block. Any connection
string works as-is, including one with `?sslmode=require`.

## Fly.io

Fly has no one-click button, so this is a handful of commands from a checkout:

```bash
fly launch --no-deploy   # reads fly.toml, creates the app
fly postgres create
fly postgres attach --app <app-name> --variable-name HORSIE_DATABASE_URL
fly deploy
```

`fly launch` assigns its own app name — the `app` value in `fly.toml` is a
placeholder it overwrites. `--variable-name HORSIE_DATABASE_URL` sets the
secret under the name horsie already reads, so nothing needs renaming
afterwards.

For an external database, skip the two `fly postgres` commands and set the
string directly:

```bash
fly secrets set HORSIE_DATABASE_URL=postgres://user:password@host/horsie
```

## Which image tag

All three paths above pin a release tag — `ghcr.io/blossomstack/horsie:0.3.0`
at the time of writing. Upgrading is a deliberate step: edit the tag, then
`docker compose pull` or redeploy.

They pin rather than track because the server is only half of a horsie. The
other half is the `horsie` CLI behind [the local runtime](/operating/local-runtime/),
which `get.horsie.dev` installs from the newest *release*. A server on a
moving tag and a CLI on a release tag drift apart between releases, and the
link between them carries no version negotiation that would notice.

`latest` still exists and moves with the default branch. Images are only
published for a commit whose full test suite passed, so it never moves to a
broken build — but a server on `latest` is ahead of any CLI installed from a
release, so match it with a runtime built from the same commit. Every build
also publishes an immutable `sha-<short>` tag, and a release publishes
`<version>` and `v<version>`.

## Building it yourself

```bash
docker build -f docker/horsie.Dockerfile --target server -t horsie-server:latest .
```

Or, with a recent Rust toolchain:

```bash
make build-server     # ./target/release/horsie-server
make install-server   # optional, into ~/.local/bin
```

Run the binary directly with:

```bash
horsie-server --addr 0.0.0.0:3789 --web clients/web/dist
```

`--web` is what makes one process serve the UI as well as the API. Without it
the binary serves the API only.

## PostgreSQL

The default is SQLite in the server's data directory, and it needs no
configuration at all. Point `database.url` at PostgreSQL when you would rather
the database were the thing that gets backed up than a container's volume:

```json
{
  "database": { "url": "postgres://user:password@host/horsie" }
}
```

`HORSIE_DATABASE_URL` sets the same thing and takes precedence. Migrations run
at startup on either backend, and `max_connections` — 10 by default — sizes the
pool that settings reads and journal writes share.

Session and agent history lives in an actor journal: the `journal_*` tables in
the same database. There is nothing to configure and nothing separate to back
up.

## What to back up

**`data_dir`** holds plugin artifacts and, with the default SQLite database,
the settings database and the journal under `<data_dir>/server/`. This is the
one to back up, and the one to mount a volume at in a container. A PostgreSQL
deployment keeps only plugin artifacts here.

**`state_dir`** is ephemeral. Losing it across a restart is fine.

Every field and its default is in the
[Configuration reference](/operating/configuration/).

## Next

- [Authentication & accounts](/operating/authentication/) — the generated first
  password, and the three auth modes.
- [The local runtime](/operating/local-runtime/) and
  [Cloud runtime vendors](/operating/cloud-vendors/) — sessions cannot run
  tools until one exists.
