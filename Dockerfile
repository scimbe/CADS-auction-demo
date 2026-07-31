# Builds auction-demo-bridge from source (pulls ct-common via the git dependency in
# Cargo.toml, pinned to CADS-Tunnel's v0.4.0 tag). Same matching-base-images
# discipline as the other CADS-Tunnel-ecosystem Dockerfiles built this session
# (CADS-p2p-vault's) -- avoids the GLIBC cross-stage drift bug found in ct-agent's
# own docker/Dockerfile.
FROM rust:1-slim-bookworm AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /work
COPY Cargo.toml Cargo.lock* ./
COPY src src
COPY index.html index.html
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/work/target \
    cargo build --release \
    && cp target/release/auction-demo-bridge /tmp/auction-demo-bridge

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tmp/auction-demo-bridge /usr/local/bin/auction-demo-bridge
ENV AUCTION_BRIDGE_LISTEN=0.0.0.0:8789
EXPOSE 8789
ENTRYPOINT ["/usr/local/bin/auction-demo-bridge"]
