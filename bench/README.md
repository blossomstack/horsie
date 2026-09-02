# Terminal-Bench adapter for horsie

Runs [Terminal-Bench](https://github.com/laude-institute/terminal-bench) tasks
against a horsie server, so horsie can be measured the way other coding-agent
harnesses are.

**Status: working.** `hello-world` passes end to end against a live server —
Terminal-Bench builds the task container, the adapter drives a real horsie
session, and the task's own pytest suite scores it green.

## Why it looks different from every other agent

Every other Terminal-Bench agent installs a CLI in the task container and types
at it through tmux. horsie is inverted: the **agent runs on a horsie server**,
and only a thin runtime lives in the container, dialling *out* to reach it.

So the adapter works both sides at once:

| Where | What |
|---|---|
| In the container | Install `horsie` + `horsie-runtime`, run `horsie connect` — which publishes the container as a runtime vendor |
| On the host | Create a session against that vendor and drive it over the HTTP API |

The tmux session is used only for setup. There is nothing to type at.

Two consequences worth knowing. The container needs to reach the horsie server
and **nothing else** — that is one outbound websocket, so a task environment
with no general internet is fine as long as that one host is routable. And the
asciinema recording will be near-empty, because the work happens through
horsie's runtime rather than the pane; scoring is unaffected, since
Terminal-Bench grades by running the task's tests.

## Running it

```bash
uv venv && uv pip install terminal-bench

# Static binaries -- see "The glibc problem" below. Alpine's native target is
# musl, so no --target flag is needed.
docker run --rm -v "$PWD":/src -w /src \
    -e OPENSSL_STATIC=1 -e OPENSSL_DIR=/usr rust:alpine sh -c \
    'apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static perl make && \
     cargo build --release -p horsie --bin horsie && \
     cargo build --release --no-default-features -p horsie-runtime --bin horsie-runtime'

export HORSIE_URL=https://your-horsie-server
export HORSIE_TOKEN=...        # horsie auth login, or a device token
export HORSIE_PROJECT=1
export HORSIE_BIN_DIR=$PWD/target/release

PYTHONPATH=/path/to/bench tb run \
    --dataset-path tasks --task-id hello-world \
    --agent-import-path tb_agent.horsie_agent:HorsieAgent \
    --agent-kwarg model_name=<alias> \
    --n-concurrent 1
```

Secrets come from the environment, never from `--agent-kwarg`, so they stay out
of Terminal-Bench's run manifests.

| `--agent-kwarg` | Default | Meaning |
|---|---|---|
| `model_name` | *(required)* | Model alias as configured on the server |
| `workdir` | `/app` | Workspace the agent operates in |
| `binaries_dir` | `$HORSIE_BIN_DIR` | Directory holding static `horsie` + `horsie-runtime` |
| `timeout_sec` | `1200` | Wall clock for the turn |
| `connect_timeout_sec` | `90` | How long to wait for the vendor to announce itself |
| `effort` | server default | Thinking effort |
| `max_iterations` | server default | Cap on agent loop iterations |

## The glibc problem

**Every published horsie Linux binary is dynamically linked against glibc
≥ 2.38.** Terminal-Bench's own base image (`python-3-13:20250620`) is Debian
bookworm — glibc 2.36 — so the network installer cannot work there. This is not
a Terminal-Bench quirk; bookworm and Ubuntu 22.04 are extremely common bases.

`install-horsie.sh` checks the version up front and fails with that sentence,
because otherwise the binary installs cleanly and then dies with
`GLIBC_2.38 not found` at first use, far from its cause.

The fix is static musl binaries, built as shown above and passed via
`binaries_dir`. They are ~17 MB each, statically linked, and run on any Linux of
the right architecture including Alpine. The installer prefers them and only
falls back to downloading when they are absent.

**horsie should publish musl builds.** Its release currently ships only
`*-unknown-linux-gnu`, which excludes a large share of real-world container
bases — a distribution gap this benchmark merely happened to expose first.

## What would make this simpler

`horsie connect` runs a whole vendor agent inside the task container to publish
one container that already exists. The mechanism underneath is more general than
that: the server mints a dial token, puts it in the runtime's environment, and
waits for the dial-back.

An **external vendor kind** — one whose `create` returns the endpoint and token
instead of launching anything, and whose `delete` is a no-op — would collapse
this adapter to: mint, copy one static binary in, exec it. No CLI, no auth login
inside the container, no vendor process. It is also how you would attach horsie
to a CI job or a developer's running container, which is worth more than the
benchmark.

## Files

| File | Purpose |
|---|---|
| `tb_agent/horsie_agent.py` | The `BaseAgent` subclass Terminal-Bench loads |
| `tb_agent/install-horsie.sh` | Runs in the container: static binaries or download, then log in |
| `horsie_client.py` | Minimal horsie HTTP client — sessions, messages, vendors |
| `horsie_turn.py` | Waiting for a turn to actually end, and reading its result |

## Two things that were wrong before real data corrected them

**`TurnEnded` is on the wire.** It appears on `GET /sessions/{id}/messages` as a
`Lifecycle` body with `value.kind == "TurnEnded"`. Session *status* is not a
turn boundary: a session is created `Provisioning`, and `Idle` means both "has
not started" and "has finished", so waiting on status alone returns before the
agent has read a single file. `wait_for_turn` polls status because it is cheap
but only believes it once the transcript contains a `TurnEnded`.

**Assistant parts are tagged, and the role is capitalised.** An assistant entry
is `body.type == "Llm"` with `body.value.role == "Assistant"`. Its `parts` carry
a `type`, and only `Text` is the reply — `Thinking` and `ToolCall` parts also
have a `text` field, so anything that greps the JSON for `"text"` silently
returns the model's reasoning instead of its answer.

## Cost accounting

`usage_of()` reports input, output, and cache tokens; `reports_cache()` says
whether the provider reported cache accounting **at all**. Absent cache numbers
and a genuine 0% hit rate are different findings and must not collapse into the
same value — the ChatGPT/codex backend reports input and output only, and
reading that as "0% cache hits" would raise a false alarm on every run.
