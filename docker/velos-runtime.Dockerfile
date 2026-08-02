# syntax=docker/dockerfile:1
#
# Container image for the **velos runtime vendor agent** (`horsie-velos-runtime`):
# the long-lived process that serves a session server's runtimes by scheduling
# one velos container each. It is not the sandbox — docker/runtime.Dockerfile
# builds the image it schedules.
#
# The agent dials out twice and is dialed back once, which is what its flags are
# about:
#   --server https://SERVER-HOST   the session server; the agent opens an
#                                  outbound WebSocket to /api/vendor/connect,
#                                  so the server never connects to it
#   --velos-url http://velos:8080  the velos control plane, plus a token via
#   HORSIE_VELOS_TOKEN             the environment (never on argv); both are
#                                  verified at startup, so a bad token fails
#                                  here rather than in the first session
#   --advertise HOST:3790          where *velos's container network* reaches
#                                  this agent — containers publish no inbound
#                                  ports, so each container's horsie-runtime
#                                  dials back to this address
#   --image ghcr.io/.../horsie-runtime:tag   the sandbox image to schedule
#
# --listen (default 0.0.0.0:3790) is the socket behind --advertise and is the
# only port this image exposes. --name defaults to "velos", which is the vendor
# name sessions select. --state-dir defaults to /var/lib/horsie-velos-runtime
# (per-runtime capability files); it is created below and owned by the runtime
# user, so it works unmounted, but a volume there survives restarts.
#
# Build from the horsie workspace ROOT (the whole workspace is the build context):
#   docker build -f docker/velos-runtime.Dockerfile -t ghcr.io/blossomstack/horsie-velos-runtime:latest .
#
# CI (.github/workflows/docker.yml) builds this multi-arch and publishes it to
# GHCR alongside the server and runtime images.

# ---- Stage 1: build the horsie-velos-runtime binary --------------------------
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# Cache the cargo registry/git and the target dir across builds. All three are
# cache mounts (not image layers), so the binary must be copied OUT to a normal
# path within this same RUN -- otherwise it vanishes with the mount.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p horsie-velos-runtime \
    && cp target/release/horsie-velos-runtime /usr/local/bin/horsie-velos-runtime

# ---- Stage 2: minimal runtime ------------------------------------------------
FROM debian:bookworm-slim
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
