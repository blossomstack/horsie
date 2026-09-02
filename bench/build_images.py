#!/usr/bin/env python3
"""Bake `horsie-runtime` into each SWE-bench task image and push the result.

Why this exists: a Fly machine's entrypoint is
`sh -c "mkdir -p <dirs> && exec <runtime_bin> --endpoint ... --runtime-id ..."`.
The runtime binary is `exec`ed directly, so it must already be in the image --
there is no hook to curl it in at boot, and no way to wrap the command.

The derived image adds one static binary on top of the upstream layers, so the
push is small even though the base is large; the base layers are already in the
registry after the first task from the same repo family.

    python3 bench/build_images.py --tasks bench/tasks.json \\
        --runtime ./target/x86_64-unknown-linux-musl/release/horsie-runtime \\
        --registry registry.fly.io/my-bench-app

Build the runtime for **linux/amd64** first -- SWE-bench's official images are
x86_64, and a runtime built for your Mac will produce a machine that boots and
then dies with an exec-format error, which surfaces as an unexplained session
timeout rather than as a build failure.

    cargo build --release --target x86_64-unknown-linux-musl -p horsie-runtime
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

DOCKERFILE = """FROM {base}
COPY horsie-runtime /usr/local/bin/horsie-runtime
RUN chmod +x /usr/local/bin/horsie-runtime
"""


def run(cmd: list[str]) -> None:
    print("  $ " + " ".join(cmd), flush=True)
    subprocess.run(cmd, check=True)


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--tasks", type=Path, default=Path("bench/tasks.json"))
    p.add_argument("--runtime", type=Path, required=True, help="linux/amd64 horsie-runtime")
    p.add_argument("--registry", required=True, help="e.g. registry.fly.io/my-bench-app")
    p.add_argument("--tag-prefix", default="swebench-")
    p.add_argument("--no-push", action="store_true")
    args = p.parse_args()

    if not args.runtime.is_file():
        sys.exit(f"runtime binary not found: {args.runtime}")

    tasks = json.loads(args.tasks.read_text())
    for i, task in enumerate(tasks, 1):
        instance_id = task["instance_id"]
        base = task["base_image"]
        tag = f"{args.registry}:{args.tag_prefix}{instance_id}"
        print(f"[{i}/{len(tasks)}] {instance_id}", flush=True)

        with tempfile.TemporaryDirectory() as tmp:
            ctx = Path(tmp)
            shutil.copy2(args.runtime, ctx / "horsie-runtime")
            (ctx / "Dockerfile").write_text(DOCKERFILE.format(base=base))
            run(["docker", "build", "--platform", "linux/amd64", "-t", tag, str(ctx)])
            if not args.no_push:
                run(["docker", "push", tag])

    print()
    print("Set BENCH_IMAGE_TEMPLATE to:")
    print(f"  {args.registry}:{args.tag_prefix}{{instance_id}}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
