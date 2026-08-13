# The Browser-Plane agent for this demo, built from the standalone scimbe/ct-agent
# repo (not vendored) -- same shape as examples/help-site/Agent.Dockerfile and
# examples/flappy-demo's agent build in the CADS-Tunnel repo. This is deliberately
# closer to how a real customer installs ct-agent: build (or, once a tagged release
# exists for the pinned ref, download a prebuilt binary) from that repo directly.

FROM rust:1-slim-bookworm AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
# Which ct-agent commit/tag to build -- bump deliberately.
# Bumped 2026-08-13 to v0.4.8 (ADMISSION_EXCHANGE_TIMEOUT 15s -> 45s): the
# actual root cause of the CADS-flappy-demo admission-stall saga, pinned by
# the operator via live edge logs -- the edge parks a lone pairing member
# for a 30s TTL, but the client's own timeout was only 15s. Keep in sync
# with compose.auction-demo.yml and compose.auction-demo.selfservice.yml's
# own CT_AGENT_REF (both the agent and cert service references).
ARG CT_AGENT_REF=3823343fdc47ea4ed91819cb68bfa8e89399f3f8
RUN git clone https://github.com/scimbe/ct-agent.git /build && cd /build && git checkout "${CT_AGENT_REF}"
WORKDIR /build
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked \
    && cp target/release/ct-agent /tmp/ct-agent

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tmp/ct-agent /usr/local/bin/ct-agent
CMD ["ct-agent"]
