# SWE-bench pilot

Ten SWE-bench Verified instances through a horsie server, one Fly machine per
task. About **$20–30 on Opus 5**. Its job is to break on the integration
problems before a full 500-task run costs ~$1,500 to discover the same ones.

It is deliberately not the benchmark harness. It answers four questions:

1. Do the per-task images boot and dial back in?
2. Can we tell reliably when a turn is finished?
3. Does a usable patch come out the other end?
4. **Is prompt caching actually working?** This is the one that decides whether
   a full run costs $1.5k or $10k.

## What it needs

| Variable | Meaning |
|---|---|
| `HORSIE_URL` | Server base URL. Default `http://localhost:3789` |
| `HORSIE_TOKEN` | Bearer token — `horsie auth login`, or a device token |
| `HORSIE_PROJECT` | Project id. Default `default` |
| `HORSIE_CALLBACK_URL` | `wss://…/api/runtime/connect` — the URL a **Fly machine** reaches your server on |
| `FLY_APP` | An existing Fly app. The server creates machines, never apps |
| `FLY_API_TOKEN` | Fly API token, scoped to that app |
| `BENCH_IMAGE_TEMPLATE` | e.g. `registry.fly.io/my-bench-app:swebench-{instance_id}` |

`HORSIE_CALLBACK_URL` is the one people get wrong. It must be reachable *from a
Fly machine*, not from your laptop — a loopback address is rejected at save
time rather than accepted and then failing later as an unexplained session
timeout, which is the failure it used to cause.

The server also needs a model provider configured with a real API key. A fresh
server has none and cannot run a turn without one.

## Running it

```bash
# 1. Real task rows from the dataset (nothing invented).
python3 bench/fetch_tasks.py --count 10

# 2. Build the runtime for linux/amd64 -- SWE-bench images are x86_64.
cargo build --release --target x86_64-unknown-linux-musl -p horsie-runtime

# 3. Bake it into each task image and push.
python3 bench/build_images.py \
    --runtime target/x86_64-unknown-linux-musl/release/horsie-runtime \
    --registry registry.fly.io/my-bench-app

# 4. Smoke one task first. Twenty minutes here saves the other nine.
python3 bench/run_pilot.py --limit 1 --keep

# 5. The rest.
python3 bench/run_pilot.py --model claude-opus-5 --effort high
```

Patches land in `bench/out/patches/*.diff`; per-task status, timing, tokens and
cost land in `bench/out/results.json`. Scoring is the official SWE-bench
evaluator's job — this produces its input, it does not grade anything.

## The three things that will bite

**`horsie-runtime` must be inside the image.** A Fly machine's entrypoint
`exec`s the runtime binary directly, so there is no hook to fetch it at boot and
no way to wrap the command. That is the whole reason `build_images.py` exists.
Build it for **linux/amd64**; an arm64 binary produces a machine that boots and
dies with an exec-format error, and what you see from the outside is a session
that never leaves `Provisioning`.

**One vendor per task, because the image is a vendor-level setting.** SWE-bench
ships a separate image per instance, so the pilot creates a throwaway vendor per
task and deletes it afterwards. It is chatty but needs no server change. The
better end state is an image override on the session's environment spec — see
"Worth changing in horsie" below.

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

Everything here runs against the server as it is today. Four gaps make a full
run clumsier than it needs to be, and each is small:

- **`horsie vendors list|save|test|delete`** — the control operations already
  exist and are reachable over HTTP; the CLI just has no commands for them. This
  is the only reason the pilot talks raw JSON at all.
- **A session-create path that takes a model directly.** `horsie agent invoke`
  needs an agent *preset*, so an ad-hoc "this model, this vendor, this message"
  run has no CLI form.
- **`horsie session wait <id>`** — block until the run completes, exit non-zero
  on failure. Every non-interactive use of horsie needs this, not just
  benchmarking.
- **`--json` on `session status`** — it currently renders a human table, so
  usage and cost have to be re-fetched over HTTP to be counted.

With those four, this Python driver collapses to a shell loop.
