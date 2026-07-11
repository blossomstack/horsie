# horsie

**Sandboxed orchestration for LLM agent graphs.**

horsie orchestrates LLM agents as arbitrary graphs that collaborate on a task.
Each agent runs in its own [nono](https://github.com/always-further/nono)-sandboxed
`horsie-runtime` child process under an explicit, per-job capability grant. A
background daemon runs jobs in parallel, journals their progress durably, and
auto-resumes anything still in flight after a restart.

## How it works

The CLI is two binaries:

- **`horsie`** — the user-facing CLI and the daemon (the `horsie` crate, in `cli/`).
- **`horsie-runtime`** — the sandboxed child the daemon spawns once per job (the
  `horsie-runtime` crate, in `runtime/`). It is the only process that talks to the model and touches the
  workdir, and it runs under a nono sandbox restricted to the capabilities you grant.

A job carries a **workflow** (a graph of agents, e.g. `planner → coder → reviewer →
pr`) and a **capability spec** (the files, directories, and network the sandbox may
reach). The daemon streams agent events back over a unix socket, journals every step,
and lets you list, tail, cancel, resume, and remove jobs.

Wire/protocol types are generated with [fluorite](https://github.com/zhxiaogg/fluorite)
from the schemas under `models/fluorite/`; see `CLAUDE.md` for the design conventions.

## Build & install

Requires a recent Rust toolchain. From the workspace root:

```bash
make build-cli      # build `horsie` + `horsie-runtime` (release)
make install-cli    # install both into ~/.local/bin (override with PREFIX=/BINDIR=)
```

Other useful targets: `make build` (whole workspace), `make test`, `make check`
(the pre-PR gate: fmt + clippy + tests), `make help`.

## Usage

Start the daemon, then submit a workflow as a job:

```bash
horsie daemon start --background

horsie job run \
  --workflow examples/dev-workflow.json \
  --capabilities examples/dev-workflow-capabilities.json \
  --workdir /path/to/a/checkout \
  --input "Add a --version flag to the CLI."
```

Inspect and manage work:

```bash
horsie daemon status              # pid, uptime, job counts
horsie job list                   # all known jobs
horsie job status <id>            # per-agent workflow progress with timing
horsie job logs <id> --follow     # tail a job's live output
horsie job stop <id>              # cancel (job becomes resumable)
horsie job resume <id> -m "..."   # answer a job awaiting input
horsie job remove <id>            # drop a finished/failed job
```

`horsie validate --workflow <file>` checks a workflow against your config without
running anything. Stopping the daemon leaves in-progress jobs `Running`; they
auto-resume on the next `daemon start`.

## Configuration

Config is read from `$XDG_CONFIG_HOME/horsie/config.json` (else
`~/.config/horsie/config.json`), or pass `--config <path>`. It defines model
providers, models, and sandbox defaults; an absent config is treated as empty.
State (the daemon control socket, per-job capability files) lives under
`$XDG_STATE_HOME/horsie`, and the durable job journal under `$XDG_DATA_HOME/horsie`.

A worked configuration and capability file are in [`examples/`](examples/README.md).

The **session server** splits config in two, and never mixes them: `config.json`
holds only deployment/bootstrap settings (storage, sandbox, runtime, hackamore,
and the settings-DB location), while its runtime-editable settings — providers,
models, velos vendor instances, and the default vendor — live in a SQLite
database managed from the web UI (**Settings**, or `GET`/`PUT /api/config`). The
DB defaults to `<data_dir>/server/config.db` (override with `database.url` or
`$HORSIE_DATABASE_URL`). Provider/model edits apply to new turns without a
restart; velos-instance edits activate on the next restart. Secrets are never
returned by the API; the UI stores keys inline or references an env var. (The
job daemon still reads providers/models from `config.json`.)

## Session server & remote runtimes

`horsie serve` runs the session-oriented HTTP + SSE server (recoverable,
event-sourced sessions). Each session runs its tools in a **runtime vendor** — an
execution sandbox. Two vendors ship:

- **`local`** (default) — a nono-sandboxed `horsie-runtime` child process, like
  the daemon.
- **`velos`** (optional) — a remote container scheduled by
  [velos](https://github.com/blossomstack/velos). The runtime dials *back* to the
  server over an outbound WebSocket, so it works against a stock velos even
  though velos publishes no inbound container ports.

A session picks its vendor via `"vendor": "velos"` in the create request, or the
server's `default_vendor`.

### Enabling the velos vendor

Add a `velos` section to the config. Only `server_url`, `image`, and
`advertise_host` are required:

```jsonc
{
  "velos": {
    "server_url": "http://velos.internal:8080",
    "token_env": "VELOS_TOKEN",              // or inline "token"
    "image": "ghcr.io/you/horsie-runtime:latest",
    "advertise_host": "10.0.0.5",            // reachable from velos workers
    "listen": "0.0.0.0:0",                   // reverse-dial listener (ephemeral port)
    "cpu": 2,
    "memory_mib": 1024,
    "connect_timeout_secs": 60
  },
  "default_vendor": "local"                    // set "velos" to make remote the default
}
```

Build the runtime image with [`docker/runtime.Dockerfile`](docker/runtime.Dockerfile)
(it builds `horsie-runtime` without the sandbox feature — the container is the
boundary) and push it where velos workers can pull it.

**Deployment requirements:** `advertise_host:<port>` must be routable from the
velos worker's container network to this server (containers get outbound NAT).
velos has no volumes, so a remote workspace is **ephemeral** — `stop` deletes the
container and the next message schedules a fresh one; the durable session state
(the journal) lives server-side and recovers on reconnect.

## Development

The pre-PR gate (also `make check`):

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

Production code denies `unwrap`, `expect`, `panic`, and wildcard match arms; tests
opt out per-file. See `CLAUDE.md` for the full design philosophy and contribution
conventions.

## License

MIT OR Apache-2.0.
