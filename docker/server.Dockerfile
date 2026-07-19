# syntax=docker/dockerfile:1
#
# Container image for the horsie **session server** (`horsie-server`): the HTTP +
# SSE API plus the bundled web UI. Pairs with docker/runtime.Dockerfile, which
# builds the horsie-runtime image this server schedules onto velos workers.
#
# Build from the horsie workspace ROOT (the whole workspace is the build context):
#   docker build -f docker/server.Dockerfile -t ghcr.io/blossomstack/horsie:latest .
#
# CI (.github/workflows/docker.yml) builds this multi-arch and publishes it to
# GHCR alongside the runtime image.

# ---- Stage 1: build the web UI (clients/web -> dist) -------------------------
# The generated fluorite types are committed under clients/web/src/generated, so
# the build needs no fluorite CLI -- just `bun run build` (tsc -b && vite build),
# which emits ./dist (index.html + assets/), the layout `--web` expects.
FROM oven/bun:1 AS web
WORKDIR /web
COPY clients/web/package.json clients/web/bun.lock ./
RUN bun install --frozen-lockfile
COPY clients/web/ ./
RUN bun run build

# ---- Stage 2: build the horsie-server binary (server crate) ------------------
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# Cache the cargo registry/git and the target dir across builds. All three are
# cache mounts (not image layers), so the binary must be copied OUT to a normal
# path within this same RUN -- otherwise it vanishes with the mount.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p horsie-server \
    && cp target/release/horsie-server /usr/local/bin/horsie-server

# ---- Stage 3: minimal runtime ------------------------------------------------
FROM debian:bookworm-slim
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
# 3789 = HTTP API + web UI; 3790 = the velos reverse-dial listener (containers
# dial ws://<advertise_host>:3790). The stack publishes both.
EXPOSE 3789 3790
HEALTHCHECK --interval=30s --timeout=3s --start-period=15s --retries=3 \
    CMD curl -fsS http://127.0.0.1:3789/api/health || exit 1
ENTRYPOINT ["horsie-server"]
# Sane default; the deploy stack overrides `command:` with the full invocation
# (--config /etc/horsie/config.json, etc.).
CMD ["--addr", "0.0.0.0:3789", "--web", "/usr/local/share/horsie/web"]
