# Self-hosting the server

From a checkout of this repo:

    docker compose -f docker/docker-compose.yml up -d

Starts the server + web UI on port 3789, no external database, no manual
config file. Data persists in a `horsie-data` Docker volume.

Next: open http://localhost:3789 → **Settings** → add a provider + model.
Then have anyone who'll run sessions against a repo on their machine follow
[Getting started](getting-started.md) to install the CLI and `horsie connect`.

## Deploying to Render

[![Deploy to Render](https://render.com/images/deploy-to-render-button.svg)](https://render.com/deploy?repo=https://github.com/blossomstack/horsie)

Runs the published image (`render.yaml` at the repo root) with a managed
Render Postgres database wired in as `HORSIE_DATABASE_URL` — no volume needed,
since both the settings store and the session journal live in that database.

To use a different Postgres instance (Neon, Supabase, RDS, or any other
`postgres://`-compatible provider) instead of the one the button provisions,
edit the `HORSIE_DATABASE_URL` environment variable on the Render service after
deploy, or fork `render.yaml` and drop the `databases:` block entirely. Any
connection string works as-is, including one with `?sslmode=require`.

## Deploying to Fly.io

Fly.io has no browser one-click button (Fly staff have said they don't plan
to ship one), so this is a handful of `flyctl` commands from a checkout of
this repo instead of a link:

```bash
fly launch --no-deploy                          # reads fly.toml, creates the app
fly postgres create                              # managed Postgres cluster
fly postgres attach --app <app-name> --variable-name HORSIE_DATABASE_URL
fly deploy
```

`fly launch` assigns its own unique app name — the `app` value committed in
`fly.toml` is only a placeholder it overwrites. `--variable-name
HORSIE_DATABASE_URL` on `fly postgres attach` sets the secret under the name
horsie already reads, so nothing needs renaming afterward.

To use an external Postgres instead of a Fly-managed cluster, skip the two
`fly postgres` commands and set the connection string directly:

```bash
fly secrets set HORSIE_DATABASE_URL=postgres://user:password@host/horsie
```

## Image tags

All three paths above run `ghcr.io/blossomstack/horsie:latest`, which tracks
`main`. Every build also publishes an immutable `sha-<short>` tag, and releases
publish `<version>` and `v<version>` — pin to one of those instead of `latest`
if you want upgrades to be a deliberate step rather than a `docker compose
pull` or a Render redeploy.

## Manual / advanced setup

Building the server image or binary yourself instead of using the published
one, writing your own `config.json`, or running behind your own reverse
proxy / auth layer — all still work exactly as before:

**Build the image:**

    docker build -f docker/horsie.Dockerfile --target server -t horsie-server:latest .

**Or build the binary from source** (needs a recent Rust toolchain):

    make build-server      # builds ./target/release/horsie-server
    make install-server    # optional: install it into ~/.local/bin

**`config.json`** holds only deployment settings (storage locations, the
database URL, plugin hook paths — see the [Settings reference](settings-reference.md)).
Everything you tune later lives in the Settings UI; `docker/docker-compose.yml`
seeds just the storage paths for you. If you're running the binary directly
and want non-default paths, write that file yourself and pass `--config <path>`.

## Using PostgreSQL

The default is SQLite in the server's data directory, and nothing about it
needs configuring. Point `database.url` at PostgreSQL when you would rather the
database be the thing that gets backed up than the container's volume:

    {
      "database": { "url": "postgres://user:password@host/horsie" }
    }

Also settable as `HORSIE_DATABASE_URL`, which takes precedence. Migrations run
at startup on either backend, and `max_connections` (default 10) sizes the pool
that settings reads and journal writes share.

**Where sessions are stored.** Session and agent history lives in an *actor
journal*, which is separate from the settings tables and has its own setting:

| `journal.backend` | Where history goes |
| --- | --- |
| `file` | JSONL files under `storage.data_dir` — needs a durable volume |
| `database` | the `journal_*` tables in `database.url` |

Left unset, both backends get `database`. The resolved choice is printed at
startup and shown under **Settings → Integrations → Server**.

> **Switching an existing server from `file` to `database` starts from an empty
> journal.** Sessions already in the UI disappear. Nothing is deleted — the
> JSONL tree stays on the volume — but nothing is imported either, so treat it
> as a one-way door and do it on a server whose history you can afford to lose.

## Signing in

The first time the server starts it creates an `admin` account and prints a
generated password:

    docker compose -f docker/docker-compose.yml logs horsie | grep -A4 'admin account'

The same password is written to `initial-admin-password` in the server's state
directory, so a rotated log is not a lockout. Change it from
**Settings → Account**, which deletes that file.

**Turning it off.** On a trusted network — or behind an auth proxy that already
identifies callers — set `HORSIE_AUTH_ENABLED=false`, or `"auth": {"enabled":
false}` in `config.json`. The server then behaves exactly as it did before
authentication existed: anything that can reach the port has full access.
