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
# Bumped 2026-08-19 to v0.5.7 (CADS-Tunnel#587): the previous pin
# (3823343fdc47ea4ed91819cb68bfa8e89399f3f8) had silently drifted 77
# commits/~6 days behind ct-agent main -- pulls in ct-agent#35/#41 (Noise-key
# attestation now enforced on all three channel-session paths). Using a real
# tag from here on, matching CADS-Tunnel's own relay-node.Dockerfile
# convention. Keep in sync with compose.auction-demo.yml and
# compose.auction-demo.selfservice.yml's own CT_AGENT_REF (both the agent
# and cert service references).
ARG CT_AGENT_REF=v0.7.24
# Optional gh-token secret (--secret id=gh_token,src=<file>): GitHub's anonymous
# git-clone rate limit for this host's IP was hit 2026-09-02 (same fix already
# applied to CADS-cookbook-demo/CADS-DEMO-deutschlandatlas-callcenter/
# CADS-webconference-demo/CADS-a2a-demo). Falls back to a plain anonymous clone
# when no secret is passed, so this is a no-op for anyone building without a token.
RUN --mount=type=secret,id=gh_token \
    if [ -s /run/secrets/gh_token ]; then \
      git -c http.https://github.com/.extraheader="AUTHORIZATION: basic $(printf 'x:%s' "$(cat /run/secrets/gh_token)" | base64 -w0)" clone https://github.com/scimbe/ct-agent.git /build; \
    else \
      git clone https://github.com/scimbe/ct-agent.git /build; \
    fi \
    && cd /build && git checkout "${CT_AGENT_REF}"
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
