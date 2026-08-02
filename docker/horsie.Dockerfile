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
