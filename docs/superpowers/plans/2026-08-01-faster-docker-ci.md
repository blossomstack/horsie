# Faster Docker CI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut Docker workflow wall clock by compiling all three images in one shared build stage per arch, with cache scopes that survive.

**Architecture:** One multi-target `docker/horsie.Dockerfile` (targets `server`, `runtime`, `velos`) with a shared cargo `build` stage; the workflow matrix collapses from 6 (image × arch) legs to 2 (arch) legs, each running three `docker/build-push-action` steps on the same buildx builder so the second and third targets' build stage resolves CACHED. Merge/publish jobs unchanged.

**Tech Stack:** Dockerfile (buildkit cache mounts), GitHub Actions, docker/build-push-action, mold linker.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-01-faster-docker-ci-design.md`
- Work happens in the worktree `.horsie/worktrees/faster-docker-ci` on branch `faster-docker-ci`, rebased onto `origin/main` AFTER #97 ("Publish a horsie-velos-runtime image") landed.
- Only `horsie-runtime` has `default` features (`sandbox`); the single cargo invocation is exactly
  `cargo build --release --locked -p horsie-server -p horsie-runtime -p horsie-velos-runtime --no-default-features`
  (disables only runtime's `sandbox` default, matching today's runtime image).
- gha cache scopes: exactly `docker-linux/amd64` and `docker-linux/arm64`. PR jobs use `cache-from` only (never `cache-to`).
- Digest artifact names must stay `digest-server-*` / `digest-runtime-*` / `digest-velos-*` (merge job glob depends on it); digest files strip the `sha256:` prefix.
- The `merge` and `compose-validate` jobs must remain functionally unchanged.

---

### Task 1: Multi-target `docker/horsie.Dockerfile`

**Files:**
- Create: `docker/horsie.Dockerfile`
- Delete: `docker/server.Dockerfile`, `docker/runtime.Dockerfile`, `docker/velos-runtime.Dockerfile`

**Interfaces:**
- Consumes: nothing (first task).
- Produces: Dockerfile with targets `web` (internal), `build` (internal), `server`, `runtime`, `velos`. Binaries land at `/usr/local/bin/horsie-server`, `/usr/local/bin/horsie-runtime`, `/usr/local/bin/horsie-velos-runtime` in the `build` stage; final stages copy from there. Task 2's workflow references `file: docker/horsie.Dockerfile` with `target: server` / `target: runtime` / `target: velos`.

- [ ] **Step 1: Write `docker/horsie.Dockerfile`**

```dockerfile
# syntax=docker/dockerfile:1
#
# Multi-target Dockerfile for all three published horsie images:
#   - ghcr.io/<owner>/horsie                (session server: HTTP/SSE API + web UI) -> --target server
#   - ghcr.io/<owner>/horsie-runtime        (sandbox scheduled onto velos)          -> --target runtime
#   - ghcr.io/<owner>/horsie-velos-runtime  (the vendor agent doing the scheduling) -> --target velos
#
# All binaries are compiled in ONE shared `build` stage so the dependency tree
# is compiled once per architecture. CI (.github/workflows/docker.yml) runs one
# job per arch that builds the `server`, `runtime`, and `velos` targets in
# sequence on the same buildx builder, so the later targets' build stage
# resolves CACHED.
#
# Build from the horsie workspace ROOT (the whole workspace is the build context):
#   docker build -f docker/horsie.Dockerfile --target server  -t ghcr.io/blossomstack/horsie:latest .
#   docker build -f docker/horsie.Dockerfile --target runtime -t ghcr.io/blossomstack/horsie-runtime:latest .
#   docker build -f docker/horsie.Dockerfile --target velos   -t ghcr.io/blossomstack/horsie-velos-runtime:latest .

# ---- Stage: build the web UI (clients/web -> dist) ---------------------------
# The generated fluorite types are committed under clients/web/src/generated, so
# the build needs no fluorite CLI -- just `bun run build` (tsc -b && vite build),
# which emits ./dist (index.html + assets/), the layout `--web` expects.
FROM oven/bun:1 AS web
WORKDIR /web
COPY clients/web/package.json clients/web/bun.lock ./
RUN bun install --frozen-lockfile
COPY clients/web/ ./
RUN bun run build

# ---- Stage: build all three Rust binaries ------------------------------------
# Single cargo invocation: of the three packages only horsie-runtime has
# `default` features (`sandbox`), so --no-default-features only drops that --
# exactly what the runtime image wants (the container is the isolation
# boundary; nono is never used in-image). mold is the linker: it cuts link time
# on the large server binary.
FROM rust:1-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends mold \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ENV RUSTFLAGS="-C link-arg=-fuse-ld=mold"
# Cache the cargo registry/git and the target dir across builds. All three are
# cache mounts (not image layers), so the binaries must be copied OUT to a
# normal path within this same RUN -- otherwise they vanish with the mount.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p horsie-server -p horsie-runtime -p horsie-velos-runtime --no-default-features \
    && cp target/release/horsie-server target/release/horsie-runtime target/release/horsie-velos-runtime /usr/local/bin/

# ---- Target: horsie (session server + web UI) --------------------------------
FROM debian:bookworm-slim AS server
# ca-certificates: outbound TLS to the LLM provider. curl: the HEALTHCHECK probe.
# git: cloning plugin-bundle repos at install time (skill-bundle ingestion).
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --home-dir /home/horsie --shell /usr/sbin/nologin horsie \
 && install -d -o horsie -g horsie /data
COPY --from=build /usr/local/bin/horsie-server /usr/local/bin/horsie-server
# Web UI assets served via `--web`.
COPY --from=web /web/dist /usr/local/share/horsie/web
USER horsie
# /data holds the session journal + state (mount a volume here); config is
# bind-mounted at /etc/horsie/config.json by the deploy stack.
WORKDIR /data
# 3789 is the only port: HTTP API, web UI, and the WebSocket routes agents and
# clients dial. The reverse-dial listeners moved out with the vendor agents.
EXPOSE 3789
HEALTHCHECK --interval=30s --timeout=3s --start-period=15s --retries=3 \
    CMD curl -fsS http://127.0.0.1:3789/api/health || exit 1
ENTRYPOINT ["horsie-server"]
# Sane default; the deploy stack overrides `command:` with the full invocation
# (--config /etc/horsie/config.json, etc.).
CMD ["--addr", "0.0.0.0:3789", "--web", "/usr/local/share/horsie/web"]

# ---- Target: horsie-runtime (sandbox scheduled onto velos) -------------------
FROM debian:bookworm-slim AS runtime
# ca-certificates: outbound TLS from tools; git: the workspace scan / git-aware
# tools; libssl3: the plugin-bundle fetch (reqwest's TLS backend is initialized
# when the HTTP client is built, even for plain-HTTP artifact URLs).
# /workspace is the default in-container root the vendor mounts under.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git libssl3 \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /workspace
COPY --from=build /usr/local/bin/horsie-runtime /usr/local/bin/horsie-runtime
WORKDIR /workspace
# The runtime needs outbound reachability to the vendor agent's advertised
# address (velos gives containers outbound NAT). The vendor supplies
# the command; this entrypoint is just a sane default for manual runs.
ENTRYPOINT ["horsie-runtime"]

# ---- Target: horsie-velos-runtime (velos vendor agent) -----------------------
FROM debian:bookworm-slim AS velos
# ca-certificates: outbound TLS to the session server (wss://) and to velos.
# No curl: the agent serves no HTTP health route, so there is no HEALTHCHECK to
# probe -- liveness is "the vendor is listed in the server's Settings".
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --home-dir /home/horsie --shell /usr/sbin/nologin horsie \
 && install -d -o horsie -g horsie /var/lib/horsie-velos-runtime
COPY --from=build /usr/local/bin/horsie-velos-runtime /usr/local/bin/horsie-velos-runtime
USER horsie
WORKDIR /var/lib/horsie-velos-runtime
# The runtime dial-back listener (--listen). It must be published on the host
# and reachable at whatever --advertise names.
EXPOSE 3790
ENTRYPOINT ["horsie-velos-runtime"]
# No default CMD: every required flag (--server, --velos-url, --advertise,
# --image) is deployment-specific, and a partial default would only fail later.
```

- [ ] **Step 2: Delete the old Dockerfiles**

```bash
git rm docker/server.Dockerfile docker/runtime.Dockerfile docker/velos-runtime.Dockerfile
```

- [ ] **Step 3: Syntax-check the Dockerfile**

Run: `docker buildx build --check -f docker/horsie.Dockerfile .`
Expected: PASS, no warnings.

- [ ] **Step 4: Build the `server` target locally (aarch64 — matches the CI arm64 leg)**

Run: `docker buildx build -f docker/horsie.Dockerfile --target server -t horsie-test-server .`
Expected: SUCCESS. First build is a full cold compile (10-20 min is normal).

- [ ] **Step 5: Build the `runtime` and `velos` targets locally and verify the shared stage is reused**

```bash
docker buildx build -f docker/horsie.Dockerfile --target runtime -t horsie-test-runtime .
docker buildx build -f docker/horsie.Dockerfile --target velos -t horsie-test-velos .
```

Expected: both SUCCESS, and the `build` stage steps show `CACHED` (only the slim final stages run). This is the core mechanism the CI speedup relies on — if the build stage is NOT cached here, stop and investigate before continuing.

- [ ] **Step 6: Smoke-test all three binaries in their images**

```bash
docker run --rm horsie-test-server --help | head -5
docker run --rm horsie-test-runtime --help | head -5
docker run --rm horsie-test-velos --help | head -5
```

Expected: all print clap help text (proves the mold-linked binaries execute and the entrypoints are intact).

- [ ] **Step 7: Commit**

```bash
git add docker/horsie.Dockerfile
git commit -m "ci(docker): one multi-target Dockerfile with a shared build stage"
```

---

### Task 2: Rework `.github/workflows/docker.yml` build/validate jobs

**Files:**
- Modify: `.github/workflows/docker.yml`

**Interfaces:**
- Consumes: `docker/horsie.Dockerfile` targets `server` / `runtime` / `velos` from Task 1.
- Produces: digest artifacts `digest-server-${{ strategy.job-index }}`, `digest-runtime-${{ strategy.job-index }}`, `digest-velos-${{ strategy.job-index }}` consumed by the unchanged `merge` job via glob `digest-${{ matrix.key }}-*`.

- [ ] **Step 1: Replace the workflow header comment block (lines 3-15) with the new structure description**

```yaml
# Builds and publishes the three horsie images to GitHub Container Registry on
# every push to main and every v* release tag:
#   - ghcr.io/<owner>/horsie                (session server: HTTP/SSE API + web UI)
#   - ghcr.io/<owner>/horsie-runtime        (sandbox scheduled onto velos)
#   - ghcr.io/<owner>/horsie-velos-runtime  (the vendor agent doing the scheduling)
#
# Mirrors the velos pipeline: auth via the workflow's OIDC-minted GITHUB_TOKEN
# (no stored registry secret); OIDC also backs the keyless cosign signature and
# the build-provenance attestation. Tags are immutable: sha-<shortsha> on every
# build, plus <version>/v<version> on release tags. No moving tags.
#
# Multi-arch is built on native runners (no QEMU): ONE job per architecture
# builds ALL THREE images from docker/horsie.Dockerfile's shared build stage
# (the later build-push steps resolve it CACHED locally), pushes each by
# digest, then a per-image merge job assembles the manifest lists and applies
# the tags. The runtime image's arm64 variant is the one velos schedules onto
# the Apple-Containerization Mac workers.
```

- [ ] **Step 2: Replace the `validate` job with a single-job, all-targets, read-only-cache version**

```yaml
  # PR gate: build every image (single-arch, no push) so a broken Dockerfile is
  # caught before merge. cache-from only: PR builds get warm starts but never
  # write cache, so they can't evict main's cache entries.
  validate:
    name: Validate build
    if: github.event_name == 'pull_request'
    runs-on: ubuntu-latest
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v7
      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3
      - name: Build server (no push)
        uses: docker/build-push-action@v6
        with:
          context: .
          file: docker/horsie.Dockerfile
          target: server
          platforms: linux/amd64
          push: false
          cache-from: type=gha,scope=docker-linux/amd64
      - name: Build runtime (no push)
        uses: docker/build-push-action@v6
        with:
          context: .
          file: docker/horsie.Dockerfile
          target: runtime
          platforms: linux/amd64
          push: false
          cache-from: type=gha,scope=docker-linux/amd64
      - name: Build velos (no push)
        uses: docker/build-push-action@v6
        with:
          context: .
          file: docker/horsie.Dockerfile
          target: velos
          platforms: linux/amd64
          push: false
          cache-from: type=gha,scope=docker-linux/amd64
```

- [ ] **Step 3: Replace the `build` job with the 2-leg (per-arch) version**

```yaml
  # One leg per arch. The server step builds the shared `build` stage (all
  # three binaries) + web stage; the runtime and velos steps reuse it CACHED
  # from this job's local buildkit instance, so deps compile once per arch
  # instead of three times.
  build:
    name: Build (${{ matrix.platform }})
    if: github.event_name != 'pull_request'
    runs-on: ${{ matrix.runner }}
    permissions:
      contents: read
      packages: write # push image blobs/digests to GHCR
    strategy:
      fail-fast: false
      matrix:
        include:
          - { platform: linux/amd64, runner: ubuntu-latest }
          - { platform: linux/arm64, runner: ubuntu-24.04-arm }
    steps:
      - uses: actions/checkout@v7

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3

      - name: Log in to GHCR
        uses: docker/login-action@v3
        with:
          registry: ${{ env.REGISTRY }}
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Build and push server by digest
        id: build-server
        uses: docker/build-push-action@v6
        with:
          context: .
          file: docker/horsie.Dockerfile
          target: server
          platforms: ${{ matrix.platform }}
          # Push by digest only; the merge job applies the human-readable tags to
          # the combined manifest list.
          outputs: type=image,name=${{ env.REGISTRY }}/${{ github.repository_owner }}/horsie,push-by-digest=true,name-canonical=true,push=true
          cache-from: type=gha,scope=docker-${{ matrix.platform }}

      - name: Build and push runtime by digest
        id: build-runtime
        uses: docker/build-push-action@v6
        with:
          context: .
          file: docker/horsie.Dockerfile
          target: runtime
          platforms: ${{ matrix.platform }}
          outputs: type=image,name=${{ env.REGISTRY }}/${{ github.repository_owner }}/horsie-runtime,push-by-digest=true,name-canonical=true,push=true
          cache-from: type=gha,scope=docker-${{ matrix.platform }}

      - name: Build and push velos by digest
        id: build-velos
        uses: docker/build-push-action@v6
        with:
          context: .
          file: docker/horsie.Dockerfile
          target: velos
          platforms: ${{ matrix.platform }}
          outputs: type=image,name=${{ env.REGISTRY }}/${{ github.repository_owner }}/horsie-velos-runtime,push-by-digest=true,name-canonical=true,push=true
          cache-from: type=gha,scope=docker-${{ matrix.platform }}
          # Single cache writer per scope, after all targets are built:
          # mode=max exports the shared build stage including cache mounts.
          cache-to: type=gha,mode=max,scope=docker-${{ matrix.platform }}

      - name: Export digests
        run: |
          mkdir -p /tmp/digests/server /tmp/digests/runtime /tmp/digests/velos
          server_digest="${{ steps.build-server.outputs.digest }}"
          runtime_digest="${{ steps.build-runtime.outputs.digest }}"
          velos_digest="${{ steps.build-velos.outputs.digest }}"
          touch "/tmp/digests/server/${server_digest#sha256:}"
          touch "/tmp/digests/runtime/${runtime_digest#sha256:}"
          touch "/tmp/digests/velos/${velos_digest#sha256:}"

      # `key` (server|runtime|velos) — not `image` (horsie|horsie-runtime|
      # horsie-velos-runtime) — so the merge job's download glob can't
      # cross-match (horsie is a prefix of both others). Keys must stay
      # mutually non-prefixing for the same reason.
      - name: Upload server digest
        uses: actions/upload-artifact@v4
        with:
          name: digest-server-${{ strategy.job-index }}
          path: /tmp/digests/server/*
          if-no-files-found: error
          retention-days: 1

      - name: Upload runtime digest
        uses: actions/upload-artifact@v4
        with:
          name: digest-runtime-${{ strategy.job-index }}
          path: /tmp/digests/runtime/*
          if-no-files-found: error
          retention-days: 1

      - name: Upload velos digest
        uses: actions/upload-artifact@v4
        with:
          name: digest-velos-${{ strategy.job-index }}
          path: /tmp/digests/velos/*
          if-no-files-found: error
          retention-days: 1
```

- [ ] **Step 4: Verify the `merge` and `compose-validate` jobs are untouched**

Run: `git diff origin/main -- .github/workflows/docker.yml | grep -E '^-' | grep -vE '^---' | grep -iE 'imagetools|cosign|attest|compose|metadata-action'`
Expected: no removed lines belonging to the `merge` or `compose-validate` jobs (only header-comment and build/validate changes).

- [ ] **Step 5: Lint the workflow with actionlint (via Docker)**

Run: `docker run --rm -v "$PWD":/repo --workdir /repo rhysd/actionlint:latest .github/workflows/docker.yml`
Expected: PASS, no errors.

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/docker.yml
git commit -m "ci(docker): one build leg per arch, shared build stage, per-arch cache scopes"
```

---

### Task 3: Update stale Dockerfile references in docs

**Files:**
- Modify: `README.md:85`
- Modify: `docs/guide/self-hosting.md:22`
- Modify: `docs/guide/runtime-vendors.md:95`
- Modify: `docker/docker-compose.yml:3`

**Interfaces:**
- Consumes: `docker/horsie.Dockerfile` from Task 1 (targets `server` / `runtime` / `velos`).
- Produces: nothing downstream.

- [ ] **Step 1: `README.md:85`** — replace `[`docker/server.Dockerfile`](docker/server.Dockerfile))` with `[`docker/horsie.Dockerfile`](docker/horsie.Dockerfile) (target `server`))`

- [ ] **Step 2: `docs/guide/self-hosting.md:22`** — replace `docker build -f docker/server.Dockerfile -t horsie-server:latest .` with `docker build -f docker/horsie.Dockerfile --target server -t horsie-server:latest .`

- [ ] **Step 3: `docs/guide/runtime-vendors.md:95`** — replace `**Build the runtime image** from `docker/runtime.Dockerfile`` with `**Build the runtime image** from `docker/horsie.Dockerfile` (target `runtime`)`

- [ ] **Step 4: `docker/docker-compose.yml:3`** — replace `# ../docker/server.Dockerfile).` with `# ../docker/horsie.Dockerfile).`

- [ ] **Step 5: Verify no stale references remain**

Run: `grep -rn "server\.Dockerfile\|runtime\.Dockerfile\|velos-runtime\.Dockerfile" README.md docs/ docker/ .github/`
Expected: no matches.

- [ ] **Step 6: Verify compose still validates**

Run: `docker compose -f docker/docker-compose.yml config > /dev/null && echo OK`
Expected: `OK`.

- [ ] **Step 7: Commit**

```bash
git add README.md docs/guide/self-hosting.md docs/guide/runtime-vendors.md docker/docker-compose.yml
git commit -m "docs: point image build instructions at docker/horsie.Dockerfile"
```

---

### Task 4: Push, open PR, verify CI green and faster

**Files:** none (verification only).

**Interfaces:**
- Consumes: all previous tasks.
- Produces: merged PR; post-merge timing data.

- [ ] **Step 1: Push and open the PR**

```bash
git push -u origin faster-docker-ci
gh pr create --title "ci(docker): shared build stage, one leg per arch" --body "..."
```

Body: why (5+ min wall clock, cache thrash, triplicated dep compiles across 6 legs) / what (multi-target Dockerfile, 2-leg matrix, per-arch scopes, mold, read-only PR cache) / callouts (merge job unchanged; digest artifact naming preserved; runtime gains `--locked` + cache mounts; absorbs #97's third image into the same shared stage).

- [ ] **Step 2: Watch the PR checks**

Run: `gh pr checks --watch`
Expected: all green. Note the `Validate build` duration — first run may be cold (~5 min); that is expected.

- [ ] **Step 3: Inspect the validate run log for cache behavior**

Run: `gh run view --log --job <validate-job-id> | grep -E 'CACHED|importing cache'`
Expected: runtime and velos steps' `build` stage shows `CACHED`.

- [ ] **Step 4: Fix any CI failures until green, then report**

Per repo convention the PR is not done until checks are green and it is mergeable. Do not merge without user approval.

- [ ] **Step 5 (post-merge follow-up): watch the first push run on main**

```bash
gh run list --workflow=docker.yml --event push --limit 1
gh run watch <run-id>
```

Expected: both `Build (linux/...)` legs green; merge/publish green for all three images; tags, attestation, and cosign signatures created as before. Record the wall-clock time and report the before/after.
