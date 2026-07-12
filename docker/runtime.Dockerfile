# Container image for the horsie **runtime** as a velos remote sandbox.
#
# The velos vendor schedules this image and overrides its command with the full
# dial-back invocation:
#   /bin/sh -c "mkdir -p /workspace/<name> && exec horsie-runtime \
#       --endpoint ws://<server>:<port> --runtime-id <id> --workspace <name>=..."
#
# The container itself is the isolation boundary, so the runtime is built WITHOUT
# the `sandbox` (nono) feature — no `--sandbox-caps` is ever passed.
#
# Build from the horsie workspace ROOT (the whole workspace is the build context):
#   docker build -f docker/runtime.Dockerfile -t ghcr.io/you/horsie-runtime:latest .
#
# Then point the vendor at it via config: `velos.image = "ghcr.io/you/horsie-runtime:latest"`.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# One package, no sandbox feature. Git deps are fetched during the build.
RUN cargo build -p horsie-runtime --no-default-features --release

FROM debian:bookworm-slim
# ca-certificates: outbound TLS from tools; git: the workspace scan / git-aware
# tools; libssl3: the plugin-bundle fetch (reqwest's TLS backend is initialized
# when the HTTP client is built, even for plain-HTTP artifact URLs).
# /workspace is the default in-container root the vendor mounts under.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git libssl3 \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /workspace
COPY --from=build /src/target/release/horsie-runtime /usr/local/bin/horsie-runtime
WORKDIR /workspace
# The runtime needs outbound reachability to the horsie server's advertised
# reverse-dial address (velos gives containers outbound NAT). The vendor supplies
# the command; this entrypoint is just a sane default for manual runs.
ENTRYPOINT ["horsie-runtime"]
