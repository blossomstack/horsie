#!/usr/bin/env python3
"""Run a handful of SWE-bench instances through a horsie server, one velos
container per task, and write a patch per instance for the official evaluator.

This is a *pilot*, not the benchmark harness. Its job is to surface the
integration problems -- image baking, turn-completion detection, patch
extraction, prompt-cache hit rate -- for about twenty dollars instead of
fifteen hundred. Read `bench/README.md` before running it.

    python3 bench/run_pilot.py --tasks bench/tasks.json --model claude-opus-5

Everything it needs comes from the environment; see `README.md` for the list.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from horsie_client import Horsie, HorsieError

# The session is done for our purposes when it reaches one of these. `Finished`
# is a workflow-run state we never produce, but treating it as terminal costs
# nothing and avoids a hang if that ever changes.
TERMINAL = {"Idle", "AwaitingInput", "Failed", "Unrecoverable", "Finished"}
FAILED = {"Failed", "Unrecoverable"}

PATCH_OPEN = "<<<HORSIE_BENCH_PATCH"
PATCH_CLOSE = "HORSIE_BENCH_PATCH>>>"

# USD per million tokens: (fresh input, output, cached read). Cache reads are a
# tenth of input on every current model, and they dominate an agentic run --
# which is why the hit rate, not the turn count, is what decides the bill.
PRICES = {
    "claude-opus-5": (5.00, 25.00, 0.50),
    "claude-sonnet-5": (3.00, 15.00, 0.30),
    "claude-haiku-4-5": (1.00, 5.00, 0.10),
}

PROMPT = """You are fixing a bug in the repository checked out at {repo_dir}.

<issue>
{problem}
</issue>

Do this:

1. Read enough of the codebase to understand the issue.
2. Make the smallest change to non-test source files that fixes it. Do not edit
   tests -- the grader runs the repository's own tests against your change.
3. Verify the fix as best you can with the tools you have.
4. Finish by running exactly this, from {repo_dir}:

       git add -A && git diff --cached

   Then reply with the complete output of that command, wrapped in these
   markers on their own lines and nothing else after them:

   {open}
   ...the diff...
   {close}

If you cannot produce a fix, still emit the markers with an empty body between
them, and say why above them.
"""


@dataclass
class Result:
    instance_id: str
    ok: bool
    status: str = ""
    session_id: str = ""
    seconds: float = 0.0
    input_tokens: int = 0
    output_tokens: int = 0
    cache_read_tokens: int = 0
    cache_creation_tokens: int = 0
    patch_bytes: int = 0
    note: str = ""
    cost_usd: float = 0.0

    def as_row(self) -> dict[str, Any]:
        return {k: v for k, v in self.__dict__.items()}


@dataclass
class Config:
    base_url: str
    token: str
    project: str
    model: str
    velos_url: str
    velos_token: str | None
    callback_url: str
    image_template: str
    runtime_bin: str
    cpu: int
    memory_mb: int
    timeout_s: int
    poll_s: float
    keep: bool
    out_dir: Path
    effort: str | None = None
    max_iterations: int | None = None
    extra: dict[str, Any] = field(default_factory=dict)


def env(name: str, *, required: bool = True, default: str | None = None) -> str:
    value = os.environ.get(name, default)
    if required and not value:
        sys.exit(f"missing required environment variable: {name}")
    return value or ""


def cost_of(model: str, r: Result) -> float:
    """Dollars for one instance. Unknown models cost nothing rather than
    guessing -- a wrong number is worse than a blank one."""
    if model not in PRICES:
        return 0.0
    fresh, out, cached = PRICES[model]
    return (
        r.input_tokens * fresh
        + r.output_tokens * out
        + r.cache_read_tokens * cached
        # A cache write is 1.25x fresh input on the 5-minute TTL this uses.
        + r.cache_creation_tokens * fresh * 1.25
    ) / 1_000_000


def wait_for_turn(h: Horsie, session_id: str, cfg: Config) -> tuple[str, dict[str, Any]]:
    """Block until the session's first turn is over, and return its status.

    **Status is not a turn boundary.** A session is created `Provisioning`,
    and `Idle` means both "has not started" and "has finished" -- so polling for
    `Idle` alone returns the instant the session exists, before the agent has
    read a single file. Two independent guards, either of which is sufficient:

    * we saw the session `Running` at least once, or
    * usage is non-zero, which only happens once a turn has completed.

    The right long-term answer is the SSE stream at `/events`, where a run's end
    is an explicit `RunComplete`. Polling is here because a pilot should not
    also be debugging a stream reader.
    """
    deadline = time.monotonic() + cfg.timeout_s
    seen_running = False
    detail: dict[str, Any] = {}

    while time.monotonic() < deadline:
        detail = h.get_session(session_id)
        status = detail.get("status", "")

        if status == "Running":
            seen_running = True
        if status in FAILED:
            return status, detail

        usage = detail.get("usageTotal") or {}
        turn_happened = seen_running or (usage.get("outputTokens") or 0) > 0
        if status in TERMINAL and turn_happened:
            return status, detail

        time.sleep(cfg.poll_s)

    return "Timeout", detail


def last_assistant_text(h: Horsie, session_id: str) -> str:
    """The text of the newest assistant entry, or "" if there is none.

    Entry shapes vary by kind, so this is deliberately forgiving: it walks the
    page backwards and returns the first thing that looks like assistant text.
    """
    try:
        page = h.read_messages(session_id)
    except HorsieError:
        return ""
    entries = (page or {}).get("entries") or []
    for entry in reversed(entries):
        blob = json.dumps(entry)
        if '"assistant"' not in blob:
            continue
        text = "".join(re.findall(r'"text"\s*:\s*("(?:[^"\\]|\\.)*")', blob))
        if text:
            # Re-parse each captured JSON string so escapes come back as text.
            return "".join(json.loads(s) for s in re.findall(r'"(?:[^"\\]|\\.)*"', text))
    return ""


def extract_patch(text: str) -> str:
    """Pull the diff out from between the markers.

    Marker-delimited rather than "assume the whole reply is a diff": the model
    is asked for a diff and will sometimes also explain itself, and a stray
    sentence at the top of a patch file makes `git apply` fail with a message
    that points at the wrong thing.
    """
    start = text.find(PATCH_OPEN)
    if start == -1:
        return ""
    start += len(PATCH_OPEN)
    end = text.find(PATCH_CLOSE, start)
    body = text[start:] if end == -1 else text[start:end]
    body = body.strip("\n")
    # Strip a fenced code block if the model wrapped the diff in one.
    if body.startswith("```"):
        body = "\n".join(body.split("\n")[1:])
        if body.rstrip().endswith("```"):
            body = body.rstrip()[: -3].rstrip("\n")
    return body + "\n" if body.strip() else ""


def run_one(cfg: Config, task: dict[str, Any]) -> Result:
    instance_id = task["instance_id"]
    # velos containers are named `horsie-{runtime_id}` and a vendor name lands
    # in paths -- keep it boring.
    vendor = "bench-" + re.sub(r"[^a-zA-Z0-9-]", "-", instance_id).lower()[:40]
    image = cfg.image_template.format(instance_id=instance_id)
    repo_dir = task.get("repo_dir", "/testbed")

    h = Horsie(cfg.base_url, cfg.token, cfg.project)
    r = Result(instance_id=instance_id, ok=False)
    started = time.monotonic()
    session_id = ""

    try:
        h.save_vendor_velos(
            vendor,
            server_url=cfg.velos_url,
            image=image,
            callback_url=cfg.callback_url,
            credential=cfg.velos_token,
            cpu=cfg.cpu,
            memory_mb=cfg.memory_mb,
            runtime_bin=cfg.runtime_bin,
        )

        probe = h.test_vendor(vendor) or {}
        if probe.get("ok") is False:
            r.status = "VendorUnusable"
            r.note = str(probe.get("message", ""))[:400]
            return r

        session_id = h.create_session(
            message=PROMPT.format(
                repo_dir=repo_dir,
                problem=task["problem_statement"],
                open=PATCH_OPEN,
                close=PATCH_CLOSE,
            ),
            model=cfg.model,
            vendor=vendor,
            name=f"bench {instance_id}",
            max_iterations=cfg.max_iterations,
            thinking_effort=cfg.effort,
        )
        r.session_id = session_id

        status, detail = wait_for_turn(h, session_id, cfg)
        r.status = status

        usage = detail.get("usageTotal") or {}
        r.input_tokens = usage.get("inputTokens") or 0
        r.output_tokens = usage.get("outputTokens") or 0
        r.cache_read_tokens = usage.get("cacheReadTokens") or 0
        r.cache_creation_tokens = usage.get("cacheCreationTokens") or 0

        if status in FAILED or status == "Timeout":
            r.note = str(detail.get("lastError", ""))[:400]
            return r

        patch = extract_patch(last_assistant_text(h, session_id))
        r.patch_bytes = len(patch)
        if patch:
            (cfg.out_dir / "patches" / f"{instance_id}.diff").write_text(patch)
            r.ok = True
        else:
            r.note = "no patch markers in the final message"
        return r

    except HorsieError as e:
        r.status = r.status or "ApiError"
        r.note = str(e)[:400]
        return r
    finally:
        r.seconds = round(time.monotonic() - started, 1)
        r.cost_usd = round(cost_of(cfg.model, r), 4)
        if not cfg.keep:
            # Order matters, and on velos it matters more than elsewhere.
            # Deleting the *session* is what destroys the container and frees
            # the worker slot. velos exposes no container listing, so there is
            # no orphan sweep to catch one that got skipped -- it would sit on
            # the pool until something happened to create the same name again.
            for cleanup in (
                lambda: h.delete_session(session_id) if session_id else None,
                lambda: h.delete_vendor(vendor),
            ):
                try:
                    cleanup()
                except HorsieError as e:
                    print(f"  cleanup warning: {e}", file=sys.stderr)


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--tasks", type=Path, default=Path("bench/tasks.json"))
    p.add_argument("--model", default="claude-opus-5")
    p.add_argument("--effort", default=None, help="thinking effort, e.g. high or xhigh")
    p.add_argument("--max-iterations", type=int, default=60)
    p.add_argument("--limit", type=int, default=0, help="run only the first N tasks")
    p.add_argument("--timeout", type=int, default=1800, help="seconds per task")
    p.add_argument("--poll", type=float, default=5.0)
    p.add_argument("--cpu", type=int, default=2)
    p.add_argument("--memory-mb", type=int, default=4096)
    p.add_argument("--out", type=Path, default=Path("bench/out"))
    p.add_argument(
        "--keep",
        action="store_true",
        help="leave sessions and vendors behind so you can inspect a failure",
    )
    args = p.parse_args()

    cfg = Config(
        base_url=env("HORSIE_URL", default="http://localhost:3789"),
        token=env("HORSIE_TOKEN"),
        project=env("HORSIE_PROJECT", required=False, default="default"),
        model=args.model,
        velos_url=env("VELOS_URL"),
        velos_token=env("VELOS_TOKEN", required=False) or None,
        callback_url=env("HORSIE_CALLBACK_URL"),
        image_template=env("BENCH_IMAGE_TEMPLATE"),
        cpu=args.cpu,
        memory_mb=args.memory_mb,
        runtime_bin=env(
            "BENCH_RUNTIME_BIN", required=False, default="/usr/local/bin/horsie-runtime"
        ),
        timeout_s=args.timeout,
        poll_s=args.poll,
        keep=args.keep,
        out_dir=args.out,
        effort=args.effort,
        max_iterations=args.max_iterations,
    )

    tasks = json.loads(args.tasks.read_text())
    if args.limit:
        tasks = tasks[: args.limit]

    (cfg.out_dir / "patches").mkdir(parents=True, exist_ok=True)
    results: list[Result] = []

    for i, task in enumerate(tasks, 1):
        print(f"[{i}/{len(tasks)}] {task['instance_id']} ...", flush=True)
        r = run_one(cfg, task)
        results.append(r)
        print(
            f"    {'ok' if r.ok else r.status or 'failed'}"
            f"  {r.seconds}s  ${r.cost_usd:.2f}"
            f"  cache_read={r.cache_read_tokens}"
            + (f"  {r.note}" if r.note else ""),
            flush=True,
        )

    (cfg.out_dir / "results.json").write_text(
        json.dumps([r.as_row() for r in results], indent=2)
    )

    produced = sum(1 for r in results if r.ok)
    total = sum(r.cost_usd for r in results)
    cache_read = sum(r.cache_read_tokens for r in results)
    cache_write = sum(r.cache_creation_tokens for r in results)
    fresh = sum(r.input_tokens for r in results)

    print()
    print(f"patches produced : {produced}/{len(results)}")
    print(f"total cost       : ${total:.2f}  (mean ${total / max(len(results), 1):.2f}/task)")

    # Absent cache numbers and a genuine 0% hit rate are different findings, and
    # only one of them is a problem. The ChatGPT backend reports input/output
    # tokens and nothing else, so treating "no fields" as "no hits" would raise
    # a false alarm on every run against it.
    if cache_read == 0 and cache_write == 0:
        print("cache            : not reported by this provider")
        print()
        print("  No cache accounting means no way to tell a working cache from a")
        print("  broken one. Before committing to a full run on a metered API, do")
        print("  one pass on a provider that reports it -- caching is the difference")
        print("  between a $2k run and a $12k one, and it fails silently.")
    else:
        hit_rate = cache_read / (cache_read + fresh) if (cache_read + fresh) else 0.0
        print(f"cache hit rate   : {hit_rate:.0%}")
        if hit_rate < 0.5:
            print()
            print("  Cache hit rate is low. Something in the prompt prefix is changing")
            print("  between turns -- a timestamp or a session id near the front of the")
            print("  system prompt is the usual cause. Fix this before a full run: it is")
            print("  the difference between a $2k run and a $12k one.")
    print()
    print(f"patches in {cfg.out_dir / 'patches'} -- hand them to the official evaluator.")
    return 0 if produced else 1


if __name__ == "__main__":
    raise SystemExit(main())
