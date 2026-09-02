# SWE-bench pilot (velos)

Ten SWE-bench Verified instances through a horsie server, one velos container
per task. About **$30–40 on Opus 5**. Its job is to break on the integration
problems before a full 500-task run costs ~$2,000 to discover the same ones.

It is deliberately not the benchmark harness. It answers four questions:

1. Do the task containers boot and dial back in?
2. Can we tell reliably when a turn is finished?
3. Does a usable patch come out the other end?
4. **Is prompt caching actually working?** This is the one that decides whether
   a full run costs $2k or $12k.

## Read this first: velos and architecture

velos executes containers through **Apple Containerization**, which runs
`linux/arm64` guests and does not emulate x86_64. SWE-bench's official
evaluation images are published for **`linux/amd64` only**. They will not run on
velos as it stands.

`build_images.py` preflights every base image's manifest and refuses to build
one that has no manifest for the target platform, because the failure otherwise
arrives late and unreadable: the container boots, dies on `exec`, and what you
see from horsie is a session that never leaves `Provisioning`.

Three real options, in the order I'd consider them:

**A. Pick a benchmark that doesn't ship amd64-only images.** Terminal-Bench
builds its task images from Dockerfiles you control, and many are plain
Debian + apt — arch-agnostic. It is also the better test of a *harness* rather
than a model, which is what horsie is. This is the cheapest path to a real
number and it needs no changes anywhere.

**B. Add a Docker/containerd backend to velos, and run x86_64 Linux workers.**
The `ContainerRuntime` trait already exists (`velos/crates/runtime/src/lib.rs`)
and is five methods: `run`/`stop`/`start`/`remove`/`list`. veloslet currently
shells out to Apple's `container` CLI. This is the "additional runtimes and
platforms" the velos README already names as planned, and it unlocks the whole
published-benchmark ecosystem, not just SWE-bench.

**C. Rebuild the SWE-bench environments for arm64.** Possible for many
instances via SWE-bench's own build tooling, but not all — some pin x86-only
wheels. The deeper cost is not engineering: a rebuilt environment is not the
published one, so **the resulting scores are not comparable to anyone else's**,
which removes most of the reason to run SWE-bench at all.

The rest of this file assumes you have images that run on your workers.

## Why one vendor per task (and why that's a workaround)

The container image is a field on the **vendor**, not on the session. SWE-bench
ships a different image per instance, so as things stand the only way to vary it
per task is a throwaway vendor per task. That is what the pilot does.

It works today and needs no server change, but it is churn in service of a
missing feature. Three ways out, cheapest first:

- **One image per *repo* rather than per instance.** SWE-bench Verified spans
  about a dozen repositories. Build one image per repo with that project's
  dependencies, and get the per-instance checkout from
  `environment.value.repos[].gitRef` instead of from the image. A dozen vendors,
  not five hundred. The catch is real: dependency pins drift across commits
  within a repo, which is precisely why SWE-bench builds per-instance images —
  so expect some instances to fail on environment rather than on reasoning, and
  measure that rate before trusting the score.
- **An image override on the session's environment spec.** One vendor, N images,
  no vendor churn. A small, contained change to horsie and the honest fix.
- **A generic image plus `repos`, letting the agent install dependencies.**
  Simplest, and the worst science: install failures become model failures, and
  the number stops meaning what SWE-bench's number means.

## What it needs

| Variable | Meaning |
|---|---|
| `HORSIE_URL` | Server base URL. Default `http://localhost:3789` |
| `HORSIE_TOKEN` | Bearer token — `horsie auth login`, or a device token |
| `HORSIE_PROJECT` | Project id. Default `default` |
| `VELOS_URL` | velos control-plane root, e.g. `http://velos:8080` |
| `VELOS_TOKEN` | velos admin token. Omit if velos runs without auth |
| `HORSIE_CALLBACK_URL` | `ws://…/api/runtime/connect` — the URL a **container** reaches horsie on |
| `BENCH_IMAGE_TEMPLATE` | e.g. `registry.example.com/bench:swebench-{instance_id}` |
| `BENCH_RUNTIME_BIN` | Path to `horsie-runtime` inside the image |

`HORSIE_CALLBACK_URL` is the one people get wrong. It must be reachable from
**velos's container network**, not from your laptop — those are different
addresses, and a loopback URL is rejected at save time rather than accepted and
then failing later as an unexplained session timeout.

velos's launch spec carries no registry credentials, so the worker must be able
to pull the image on its own: public, or `container` already logged in.

The horsie server also needs a model provider configured with a real API key. A
fresh server has none and cannot run a turn without one.

## Running it

```bash
# 1. Real task rows from the dataset (nothing invented).
python3 bench/fetch_tasks.py --count 10

# 2. Build the runtime for the workers' platform, WITHOUT the sandbox feature --
#    the container is already the isolation boundary, and a second one inside it
#    only breaks the runtime's own writes.
cargo build --release --no-default-features \
    --target aarch64-unknown-linux-musl -p horsie-runtime

# 3. Bake it into each task image and push. Preflights the manifests first.
python3 bench/build_images.py \
    --runtime target/aarch64-unknown-linux-musl/release/horsie-runtime \
    --registry registry.example.com/bench

# 4. Smoke one task first. Twenty minutes here saves the other nine.
python3 bench/run_pilot.py --limit 1 --keep

# 5. The rest.
python3 bench/run_pilot.py --model claude-opus-5 --effort high
```

Patches land in `bench/out/patches/*.diff`; per-task status, timing, tokens and
cost land in `bench/out/results.json`. Scoring is the official evaluator's job —
this produces its input, it does not grade anything.

## The three things that will bite

**`horsie-runtime` must be inside the image, at the path the vendor names.** A
velos container's entrypoint `exec`s `runtimeBin` directly, so there is no hook
to fetch it at boot. `BENCH_RUNTIME_BIN` and `build_images.py --runtime-bin`
must agree, or the container dies on exec.

**Cleanup is load-bearing on velos.** Deleting the **session** is what destroys
the container and frees the worker slot. velos exposes no container listing, so
horsie has no orphan sweep to catch one that got skipped — a leaked container
sits on the pool until something happens to create the same name again. `--keep`
leaves both behind on purpose for debugging; remember to clean up by hand after.

**Status is not a turn boundary.** A session is created `Provisioning`, and
`Idle` means both "has not started" and "has finished" — so waiting for `Idle`
alone returns before the agent has read a single file. `wait_for_turn` guards
with two independent signals: it saw `Running` at least once, or usage is
non-zero. The clean fix is the SSE stream at `/events`, where a run's end is an
explicit `RunComplete`.

## Reading the output

The summary line that matters is the cache hit rate. Below ~50%, stop and find
what is changing at the front of the prompt between turns — a timestamp or a
session id near the top of the system prompt is the usual culprit. Caching is a
prefix match, so one varying byte early invalidates everything after it, and the
run silently costs 5–8× more.

Patch extraction is marker-delimited (`<<<HORSIE_BENCH_PATCH`). If several tasks
come back with "no patch markers in the final message", the prompt is losing to
the model's own formatting instincts — that is a real finding about the harness,
not a script bug, and it is exactly the kind of thing this pilot is for.

## Worth changing in horsie

Everything here runs against the server as it is today. These make a full run
scriptable rather than clumsy, and each is small:

- **An image override on the environment spec** — removes the per-task vendor
  churn entirely. See "Why one vendor per task" above.
- **`horsie vendors list|save|test|delete`** — the control operations already
  exist and are reachable over HTTP; the CLI has no commands for them. This is
  the only reason the pilot talks raw JSON at all.
- **A session-create path that takes a model directly.** `horsie agent invoke`
  needs an agent *preset*, so an ad-hoc "this model, this vendor, this message"
  run has no CLI form.
- **`horsie session wait <id>`** — block until the run completes, exit non-zero
  on failure. Every non-interactive use of horsie needs this, not just
  benchmarking.
- **`--json` on `session status`** — it currently renders a human table, so
  usage and cost have to be re-fetched over HTTP to be counted.
