# Faster Docker CI: shared-builder pipeline

Date: 2026-08-01 (revised same day: rebased onto #97, which added a third image)
Status: approved (brainstorming session)

## Problem

The Docker workflow takes ~5 min wall clock on every push to main. As of #97
there are **three** published images (horsie, horsie-runtime,
horsie-velos-runtime) built as **6 parallel legs** (image × arch). In the
slowest leg measured before #97, 242s of 265s was a single from-scratch
`cargo build --release` compiling 319 crates.

Root causes:

1. **Cache thrash.** The repo's GitHub Actions cache is at its 10 GB limit
   (~137 entries; rust-cache ~2 GB plus many ~430 MB buildkit `mode=max` blob
   exports across per-image-per-arch scopes). LRU eviction means the
   `/src/target` cache mounts frequently restore empty, forcing full dep-tree
   recompiles.

   > **Correction (2026-08-03).** Root cause 1 is wrong, and the design built
   > on it could not have worked. The `/src/target` cache mount does not
   > "frequently" restore empty — it restores empty **always**. BuildKit does
   > not export `RUN --mount=type=cache` contents through *any* cache backend
   > (gha, registry, or local); the mounts live only on the builder instance,
   > which dies with the runner. LRU eviction was never involved. Every CI
   > build since has recompiled the full dependency tree from scratch,
   > confirmed in run 30822453833 where the cargo step opens with `Updating
   > crates.io index` and takes 262.5s of a 319s job. Compounding it, the
   > `COPY . .` ahead of `cargo build` also made the layer cache useless: any
   > change anywhere in the context invalidated it. Fixed by the cargo-chef
   > restructure, which moves the dependency build into a real, exportable
   > image layer keyed on recipe.json.
2. **`runtime.Dockerfile` has no cache mounts at all** (and no `--locked`), so
   every runtime leg is a guaranteed cold build.
3. **Duplicated work.** The 6 matrix legs each independently compile the same
   dependency tree; all three images on the same arch share nearly all deps
   but build in separate jobs with separate cache scopes.

The repo is public, so runner minutes are free and unlimited — the goal is pure
wall-clock reduction, not cost.

## Design

### One multi-target Dockerfile

New `docker/horsie.Dockerfile` replaces `docker/server.Dockerfile`,
`docker/runtime.Dockerfile`, and `docker/velos-runtime.Dockerfile`:

- `web` stage: unchanged from `server.Dockerfile` (oven/bun → dist).
- `build` stage: `rust:1-bookworm` + `mold` installed via apt;
  `RUSTFLAGS="-C link-arg=-fuse-ld=mold"`; a single cache-mounted
  `cargo build --release --locked -p horsie-server -p horsie-runtime -p horsie-velos-runtime --no-default-features`
  with all three binaries copied out of the cache mount to `/usr/local/bin/`.
  - None of the three packages has `default` features except `horsie-runtime`,
    whose `sandbox` default is exactly what `--no-default-features` must
    disable (the container is the isolation boundary; nono is never used
    in-image).
- `server`, `runtime`, and `velos` final stages: identical to today's
  respective final stages in the three old Dockerfiles (same base image,
  packages, users, dirs, exposed ports, healthcheck, entrypoints), each copying
  its binary from `build`; `server` also copies the web dist.

The three old Dockerfiles are deleted. Stale path references are updated in
`docker/docker-compose.yml` (comment), `README.md`, `docs/guide/self-hosting.md`,
and `docs/guide/runtime-vendors.md`.

### Workflow: 6 build legs → 2

`.github/workflows/docker.yml` `build` matrix collapses to one leg per arch
(`linux/amd64` on `ubuntu-latest`, `linux/arm64` on `ubuntu-24.04-arm`). Each
leg runs three `docker/build-push-action` steps on the same buildx builder:

1. `target: server` — builds web + build + server stages, pushes by digest.
2. `target: runtime` — the entire `build` stage resolves `CACHED` from the
   local buildkit instance; only the slim final stage runs. Pushes by digest.
3. `target: velos` — same cache hit; pushes by digest.

Each step exports its own image digest; digest artifacts keep the existing
`digest-server-*` / `digest-runtime-*` / `digest-velos-*` naming so the
**merge/publish job is unchanged** (manifest list, tags, attestation, cosign).

### Cache strategy

- One gha cache scope per arch (`docker-linux/amd64`, `docker-linux/arm64`)
  instead of six — far less blob count and eviction pressure.
- `cache-from` on all steps; `cache-to: type=gha,mode=max` only on the last
  (velos) step — a single writer per scope per run.
- The PR `validate` job becomes one job building all three targets with
  `push: false` and **`cache-from` only** (no `cache-to`): PRs can no longer
  write cache entries that evict main's.

## Expected results

- Dependencies compile once per arch instead of three times; mold speeds final
  links; the runtime build gains `--locked` and cache mounts.
- Warm cache: ~1.5–2.5 min per leg → **~3 min wall clock including merge**
  (from ~5+ min). Cold: ~4.5–5 min, and cold should become rare because PRs no
  longer thrash the cache pool.

## Risks and rollback

- mold on either arch: the change is one `ENV` line plus an apt package;
  removing it reverts to GNU ld.
- Feature-unification surprises from the single cargo invocation: verified by
  the PR validate build (`--no-default-features` semantics per package are
  unchanged from today).
- First post-merge push exercises the unchanged merge job (tags, attestation,
  cosign) for all three images — watch it before considering the work done.

## Testing / verification

- The PR's own `validate` run proves all three targets build on amd64.
- Local (pre-PR): full `docker buildx build` of each target on aarch64, with
  the second/third builds showing the shared `build` stage `CACHED`, plus
  `--help` smoke tests of each binary in its image.
- `docker compose -f docker/docker-compose.yml config` still validates
  (compose job unchanged).
- Post-merge: inspect the first push run for cache-hit behavior (build stage
  should show mostly CACHED / fast cargo finish on the second push onward).
