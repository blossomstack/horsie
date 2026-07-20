# Frictionless self-host

Tracks the first P0 item in [market#11](https://github.com/blossomstack/market/issues/11):
"Frictionless self-host — one-command up + quickstart." Today, getting from
nothing to a working chat session takes ~11 manual steps across three
artifacts (build/download the server binary, hand-write `config.json`, build
the web UI, start the server, build/run a separate `horsie-runtime` daemon by
hand, then configure a provider/model in Settings). This is the single
largest trial killer per the completeness audit behind market#11/#12.

## Goal

Two one-liners, one per audience:

- **Server operator:** `docker compose -f docker/docker-compose.yml up -d`
- **Anyone connecting their machine to a server:** install the `horsie` CLI
  once, then `horsie connect --server <url> --workspace <path>`.

Provider/model configuration stays a manual Settings-UI step (out of scope —
no safe way to auto-supply an API key).

## Non-goals

- No env-var seeding of providers/models.
- No multi-server memory for `horsie connect` (every invocation takes
  explicit flags; revisit if a real need for remembering multiple servers
  shows up).
- No hosting/DNS for `get.horsie.dev` itself — that's infrastructure outside
  this repo (likely the ops Cloudflare stack). This spec only produces the
  install script that would live behind it.

## Components

### 1. `docker/docker-compose.yml`

One service, next to the existing `server.Dockerfile`/`runtime.Dockerfile`:

- Image: `ghcr.io/blossomstack/horsie:latest`.
- Port: `3789:3789`.
- An inline compose `config` seeding `{"local_runtime": true}` — required for
  `horsie connect` to have a listener to dial (see
  `docs/superpowers/specs/2026-07-18-shared-local-runtime-vendor-design.md`).
- A local (non-external) named volume `horsie-data:/data` for journal/state
  persistence — unlike the homelab ops stack, self-host doesn't pre-create
  this volume out of band.
- `restart: unless-stopped`. Healthcheck is inherited from the image.
- No secrets/env required to start; providers/models are added via Settings
  after first boot.

### 2. `horsie connect` (new CLI subcommand)

Added to `cli/src/main.rs` alongside `Daemon`/`Job`/`Plugin`. Wraps the
existing standalone `horsie-runtime --endpoint ... --runtime-id ... --workspace
...` flow (documented today in `docs/guide/getting-started.md`) so a user only
ever installs one binary, `horsie`.

Flags:

- `--server <url>` (required) — `http(s)://host:port` of the session server.
- `--workspace <[name=]path>` (repeatable, required, at least one) — a bare
  path (no `=`) is normalized to `main=<path>` before being handed to the
  runtime; `name=path` passes through unchanged. Reuses
  `horsie_runtime::workspace::WorkspaceRegistry::parse_arg`'s existing
  `name=path` semantics — `horsie connect` only adds the bare-path shorthand
  on top.
- `--runtime-id <id>` (default `"local"` — matches the server's default
  vendor pickup, so a fresh server's new sessions use it automatically with
  no extra flag).
- `--background` — same detach-with-logfile pattern as `daemon start
  --background` (spawn detached, redirect output to `<state_dir>/connect.log`,
  print the PID + log path instead of streaming).

Behavior:

1. Parse `--server`. Reject anything other than `http`/`https` before doing
   anything else (mirrors the existing scheme validation in
   `runtime/src/main.rs`'s `parse_endpoint`).
2. Translate it into the `--endpoint` URL `horsie-runtime` expects:
   `http` → `ws`, `https` → `wss`, path `/api/runtime/connect`, query
   `register=<runtime_id>`.
3. Normalize each `--workspace` value (bare path → `main=<path>`).
4. Locate the sibling `horsie-runtime` binary. `cli/src/daemon/mod.rs`
   already has this exact lookup (`default_runtime_bin()`, currently
   `pub(crate)`) for spawning per-job runtimes; promote it to a shared helper
   (`crate::runtime_bin::default_runtime_bin()` or similar) rather than
   duplicating the logic.
5. Spawn `horsie-runtime` with the translated flags — foreground by default
   (stream its stdout/stderr, block until it exits or is interrupted),
   detached when `--background` is set. On success, print a one-line
   confirmation: `connected to <server> as runtime "<runtime-id>" ·
   workspace "<name>" -> <path> [, ...]`.

Errors:

- Bad `--server` scheme → CLI error before any process is spawned.
- Sibling `horsie-runtime` binary missing → actionable error naming the path
  it looked for, not a raw OS "file not found".

### 3. Cut a real release

`​.github/workflows/publish.yml` already has a `release-binaries` job
(4-platform matrix, builds `horsie`/`horsie-runtime`/`horsie-server`, uploads
tarballs to a GitHub Release) gated on `v*` tags — but across the 5 existing
tags (`v0.1.0`–`v0.1.4`) it has never actually run (0 GitHub releases, 0
workflow runs; the tags predate the workflow's existence on `main`). Push
`v0.1.5` once the rest of this spec lands, to exercise it for the first time
and confirm it produces real, downloadable binaries.

### 4. `scripts/install.sh`

Detects OS/arch, resolves the latest GitHub release, downloads the matching
`horsie-<version>-<target>.tar.gz`, extracts the `horsie` and `horsie-runtime`
binaries (not `horsie-server` — that one's server-side only) — `horsie
connect` spawns `horsie-runtime` as a sibling, so both must be installed
together — and installs them to `~/.local/bin` (matching the `PREFIX`/`BINDIR`
convention already in the `Makefile`). Written so it's ready to sit behind
`get.horsie.dev` once that redirect exists (DNS/hosting is out of scope here
— see Non-goals).

### 5. Docs

Rewrite `docs/guide/getting-started.md`:

```markdown
# Getting started

## 1. Install the CLI

    curl -fsSL https://get.horsie.dev | sh

## 2. Connect to a server

    horsie connect --server https://horsie.example.com --workspace .

    connected to https://horsie.example.com as runtime "local" ·
    workspace "main" -> /Users/shawn/proj
    open http://horsie.example.com in your browser to start a session

## 3. Open the web UI, create a session

Browse to the server's URL, add a provider/model in Settings if none exist
yet, then New -> pick a model -> Create.
```

Add `docs/guide/self-hosting.md`:

```markdown
# Self-hosting the server

From a checkout of this repo:

    docker compose -f docker/docker-compose.yml up -d

Starts the server + web UI on port 3789, no external database, no manual
config. Data persists in a `horsie-data` Docker volume.

Next: open http://localhost:3789 -> Settings -> add a provider + model.
Then have anyone who'll run sessions against a repo on their machine follow
[Getting started](getting-started.md) to install the CLI and `horsie connect`.
```

The existing "build from source" / "build the container image" / manual
`horsie-runtime` instructions move into a "Manual / advanced setup" section
in `self-hosting.md` and `getting-started.md` respectively, rather than being
deleted — they're still correct, just no longer the front-door path.

## Testing

- Unit tests for the `--server` → `--endpoint` translation and the bare-path
  workspace normalization (pure functions; live alongside `cli/src/main.rs`'s
  existing `#[cfg(test)] mod tests`).
- Extend the existing fake-runtime e2e harness (`tests/tests/
  session_server_e2e.rs`, `server/src/vendor/local.rs`) to cover `horsie
  connect` spawning a real `horsie-runtime` against a test server.
- `docker/docker-compose.yml` validated with `docker compose config` in CI
  (a new lightweight check, alongside the existing Dockerfile PR-validate job
  in `.github/workflows/docker.yml`); manually smoke-tested (`docker compose
  up -d`, hit `/api/health`) before merge.
- The `v0.1.5` tag push is itself the first real test of the release job —
  confirm the GitHub Release appears with all 4 tarballs before calling this
  done.
