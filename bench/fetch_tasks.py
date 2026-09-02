#!/usr/bin/env python3
"""Pull N real SWE-bench Verified rows into `tasks.json`.

Fetches from the Hugging Face datasets-server REST API so there is nothing to
`pip install`. The problem statements are the dataset's own text -- writing them
by hand, or letting a model reconstruct them, would quietly change what is being
measured.

    python3 bench/fetch_tasks.py --count 10 --out bench/tasks.json

**Verify the image names it derives.** The mapping from `instance_id` to the
official evaluation image is a naming convention, not a dataset field, and it
has changed before. Check one against the SWE-bench docs (or
`docker manifest inspect`) before you build all ten -- a wrong base image fails
at build time, which is cheap, but a *plausible* wrong one wastes a run.
"""

from __future__ import annotations

import argparse
import json
import urllib.parse
import urllib.request
from pathlib import Path

DATASET = "princeton-nlp/SWE-bench_Verified"
ROWS_URL = "https://datasets-server.huggingface.co/rows"

# The published convention: `__` in an instance id becomes `_1776_` in the
# image tag. Verify before trusting it (see the module docstring).
IMAGE_TEMPLATE = "swebench/sweb.eval.x86_64.{mangled}:latest"


def image_for(instance_id: str) -> str:
    return IMAGE_TEMPLATE.format(mangled=instance_id.replace("__", "_1776_").lower())


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--count", type=int, default=10)
    p.add_argument("--offset", type=int, default=0)
    p.add_argument("--out", type=Path, default=Path("bench/tasks.json"))
    p.add_argument("--repo-dir", default="/testbed", help="checkout path inside the image")
    args = p.parse_args()

    query = urllib.parse.urlencode(
        {
            "dataset": DATASET,
            "config": "default",
            "split": "test",
            "offset": args.offset,
            "length": args.count,
        }
    )
    with urllib.request.urlopen(f"{ROWS_URL}?{query}", timeout=60) as resp:
        payload = json.load(resp)

    tasks = []
    for entry in payload["rows"]:
        row = entry["row"]
        tasks.append(
            {
                "instance_id": row["instance_id"],
                "repo": row["repo"],
                "base_commit": row["base_commit"],
                "problem_statement": row["problem_statement"],
                "base_image": image_for(row["instance_id"]),
                "repo_dir": args.repo_dir,
            }
        )

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(tasks, indent=2))
    print(f"wrote {len(tasks)} tasks to {args.out}")
    print("spot-check one base_image before building all of them:")
    if tasks:
        print(f"  docker manifest inspect {tasks[0]['base_image']} > /dev/null && echo ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
