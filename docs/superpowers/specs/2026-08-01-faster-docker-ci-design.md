# Faster Docker CI: shared-builder pipeline

Date: 2026-08-01
Status: approved (brainstorming session)

## Problem

The Docker workflow takes ~5 min wall clock on every push to main, gated by the
slowest of 4 parallel build legs (measured: horsie/amd64 284s, runtime/amd64
225s, horsie/arm64 214s, runtime/arm64 149s, plus a ~45s merge/publish tail).
Inside the slowest leg, 242s of 265s is a single from-scratch
`cargo build --release` compiling 319 crates.

Root causes:

1. **Cache thrash.** The repo's GitHub Actions cache is at its 10 GB limit
   (~137 entries; rust-cache ~2 GB plus many ~430 MB buildkit `mode=max` blob
   exports across 4 per-image-per-arch scopes). LRU eviction means the
   `/src/target` cache mounts in `server.Dockerfile` frequently restore empty,
   forcing full dep-tree recompiles.
2. **`runtime.Dockerfile` has no cache mounts at all** (and no `--locked`), so
   every runtime leg is a guaranteed cold build.
3. **Duplicated work.** The 4 matrix legs each independently compile the same
   dependency tree; server and runtime on the same arch share nearly all deps
   but build in separate jobs with separate cache scopes.

The repo is public, so runner minutes are free and unlimited — the goal is pure
wall-clock reduction, not cost.

## Design

### One multi-target Dockerfile

New `docker/horsie.Dockerfile` replaces `docker/server.Dockerfile` and
`docker/runtime.Dockerfile`:

- `web` stage: unchanged from `server.Dockerfile` (oven/bun → dist).
- `build` stage: `rust:1-bookworm` + `mold` installed via apt;
  `RUSTFLAGS="-C link-arg=-fuse-ld=mold"`; a single cache-mounted
  `cargo build --release --locked -p horsie-server -p horsie-runtime --no-default-features`
  with both binaries copied out of the cache mount to `/usr/local/bin/`.
  - `horsie-server` does not depend on the `runtime` crate, and has no
    `default` features, so `--no-default-features` only disables runtime's
    `sandbox` default — exactly what the runtime image already builds with.
- `server` and `runtime` final stages: identical to today's respective stage 3
  (same base image, packages, users, env, healthcheck, entrypoints), each
  copying its binary from `build`; `server` also copies the web dist.

The old Dockerfiles are deleted. Stale path references are updated in
`docker/docker-compose.yml` (comment), `README.md`, `docs/guide/self-hosting.md`,
and `docs/guide/runtime-vendors.md`.

### Workflow: 4 build legs → 2

`.github/workflows/docker.yml` `build` matrix collapses to one leg per arch
(`linux/amd64` on `ubuntu-latest`, `linux/arm64` on `ubuntu-24.04-arm`). Each
leg runs two `docker/build-push-action` steps on the same buildx builder:

1. `target: server` — builds web + build + server stages, pushes by digest.
2. `target: runtime` — the entire `build` stage resolves `CACHED` from the
   local buildkit instance; only the slim final stage runs. Pushes by digest.

Each step exports its own image digest; digest artifacts keep the existing
`digest-server-*` / `digest-runtime-*` naming so the **merge/publish job is
unchanged** (manifest list, tags, attestation, cosign).

### Cache strategy

- One gha cache scope per arch (`docker-linux/amd64`, `docker-linux/arm64`)
  instead of four — halves blob count and eviction pressure.
- `cache-from` on both steps; `cache-to: type=gha,mode=max` only on the second
  (runtime) step — a single writer per scope per run.
- The PR `validate` job becomes one job building both targets with `push:
  false` and **`cache-from` only** (no `cache-to`): PRs can no longer write
  cache entries that evict main's.

## Expected results

- Dependencies compile once per arch instead of twice; mold speeds final
  links; the runtime build gains `--locked` and cache mounts.
- Warm cache: ~1.5–2.5 min per leg → **~3 min wall clock including merge**
  (from ~5 min). Cold: ~4–4.5 min, and cold should become rare because PRs no
  longer thrash the cache pool.

## Risks and rollback

- mold on either arch: the change is one `ENV` line plus an apt package;
  removing it reverts to GNU ld.
- Feature-unification surprises from the single cargo invocation: verified by
  the PR validate build and by comparing runtime binary behavior
  (`--no-default-features` semantics are unchanged from today).
- First post-merge push exercises the unchanged merge job (tags, attestation,
  cosign) — watch it before considering the work done.

## Testing / verification

- The PR's own `validate` run proves both targets build on amd64.
- `docker compose -f docker/docker-compose.yml config` still validates
  (compose job unchanged).
- Post-merge: inspect the first push run for cache-hit behavior (build stage
  should show mostly CACHED / fast cargo finish on the second push onward).
