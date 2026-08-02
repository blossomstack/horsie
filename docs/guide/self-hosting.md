# Self-hosting the server

From a checkout of this repo:

    docker compose -f docker/docker-compose.yml up -d

Starts the server + web UI on port 3789, no external database, no manual
config file. Data persists in a `horsie-data` Docker volume.

Next: open http://localhost:3789 → **Settings** → add a provider + model.
Then have anyone who'll run sessions against a repo on their machine follow
[Getting started](getting-started.md) to install the CLI and `horsie connect`.

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
