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
