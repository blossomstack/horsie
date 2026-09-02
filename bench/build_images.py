#!/usr/bin/env python3
"""Bake `horsie-runtime` into each task image and push the result.

Why this exists: a velos container's entrypoint is
`sh -c "mkdir -p <dirs> && exec <runtimeBin> --endpoint ... --runtime-id ..."`.
The runtime binary is `exec`ed directly, so it must already be in the image --
there is no hook to fetch it at boot, and no way to wrap the command.

    python3 bench/build_images.py --tasks bench/tasks.json \\
        --runtime ./target/aarch64-unknown-linux-musl/release/horsie-runtime \\
        --registry registry.example.com/bench

**Architecture.** velos executes containers through Apple Containerization,
which runs **linux/arm64** guests -- it does not emulate x86_64. SWE-bench's
official evaluation images are published for **linux/amd64 only**. So this
script preflights every base image's manifest and refuses to build one that has
no manifest for the target platform, because the alternative is a container that
boots, dies with an exec-format error, and shows up as a horsie session that
never leaves `Provisioning`.

If the preflight rejects your images, see `README.md` -> "velos and
architecture". There is no flag here that makes an amd64-only image run.

Build the runtime for the same platform, **without the sandbox feature** -- the
container is already the isolation boundary, and a second one inside it only
breaks the runtime's own writes:

    cargo build --release --no-default-features \\
        --target aarch64-unknown-linux-musl -p horsie-runtime
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
COPY horsie-runtime {runtime_bin}
RUN chmod +x {runtime_bin}
"""


def run(cmd: list[str]) -> None:
    print("  $ " + " ".join(cmd), flush=True)
    subprocess.run(cmd, check=True)


def manifest_has_platform(image: str, platform: str) -> bool | None:
    """True/False if we could read the manifest, None if we could not.

    `None` is not a failure: a registry that refuses an anonymous manifest read
    is common, and turning that into a hard stop would block a setup that is
    otherwise fine. The caller warns instead.
    """
    want_os, _, want_arch = platform.partition("/")
    try:
        out = subprocess.run(
            ["docker", "manifest", "inspect", image],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None

    doc = json.loads(out)
    entries = doc.get("manifests")
    if entries is None:
        # A single-platform manifest. Its own descriptor carries the platform.
        plat = doc.get("platform") or {}
        if not plat:
            return None
        return plat.get("os") == want_os and plat.get("architecture") == want_arch
    return any(
        (m.get("platform") or {}).get("os") == want_os
        and (m.get("platform") or {}).get("architecture") == want_arch
        for m in entries
    )


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--tasks", type=Path, default=Path("bench/tasks.json"))
    p.add_argument("--runtime", type=Path, required=True, help="horsie-runtime for --platform")
    p.add_argument("--registry", required=True, help="e.g. registry.example.com/bench")
    p.add_argument("--tag-prefix", default="swebench-")
    p.add_argument(
        "--platform",
        default="linux/arm64",
        help="what velos's workers run. Apple Containerization is linux/arm64",
    )
    p.add_argument(
        "--runtime-bin",
        default="/usr/local/bin/horsie-runtime",
        help="must match the vendor's runtimeBin (BENCH_RUNTIME_BIN)",
    )
    p.add_argument("--no-push", action="store_true")
    p.add_argument(
        "--skip-preflight",
        action="store_true",
        help="build even if a base image has no manifest for --platform. You will "
        "get containers that boot and immediately die",
    )
    args = p.parse_args()

    if not args.runtime.is_file():
        sys.exit(f"runtime binary not found: {args.runtime}")

    tasks = json.loads(args.tasks.read_text())

    if not args.skip_preflight:
        print(f"preflight: checking base images for {args.platform}", flush=True)
        missing, unknown = [], []
        for task in tasks:
            verdict = manifest_has_platform(task["base_image"], args.platform)
            if verdict is None:
                unknown.append(task["base_image"])
            elif not verdict:
                missing.append(task["base_image"])
        if unknown:
            print(
                f"  warning: could not read {len(unknown)} manifest(s); not checked.\n"
                f"           first: {unknown[0]}",
                file=sys.stderr,
            )
        if missing:
            print(
                f"\n{len(missing)} of {len(tasks)} base images have no {args.platform} "
                f"manifest.\n  first: {missing[0]}\n\n"
                "velos runs containers through Apple Containerization, which does not\n"
                "emulate other architectures -- these images cannot run on it. Building\n"
                "them anyway produces containers that die on exec, which surfaces as a\n"
                "session stuck in Provisioning rather than as an error you can read.\n\n"
                'See README.md -> "velos and architecture" for the three real options.',
                file=sys.stderr,
            )
            return 2
        print("  ok", flush=True)

    for i, task in enumerate(tasks, 1):
        instance_id = task["instance_id"]
        tag = f"{args.registry}:{args.tag_prefix}{instance_id}"
        print(f"[{i}/{len(tasks)}] {instance_id}", flush=True)

        with tempfile.TemporaryDirectory() as tmp:
            ctx = Path(tmp)
            shutil.copy2(args.runtime, ctx / "horsie-runtime")
            (ctx / "Dockerfile").write_text(
                DOCKERFILE.format(base=task["base_image"], runtime_bin=args.runtime_bin)
            )
            run(["docker", "build", "--platform", args.platform, "-t", tag, str(ctx)])
            if not args.no_push:
                run(["docker", "push", tag])

    print()
    print("Set these before running the pilot:")
    print(f"  BENCH_IMAGE_TEMPLATE={args.registry}:{args.tag_prefix}{{instance_id}}")
    print(f"  BENCH_RUNTIME_BIN={args.runtime_bin}")
    print()
    print("velos's launch spec carries no registry credentials -- the worker must be")
    print("able to pull this tag on its own (public, or `container` already logged in).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
