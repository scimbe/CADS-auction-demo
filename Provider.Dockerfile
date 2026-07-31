# Builds auction-demo-provider: a genuinely independent bidder process (src/bin/provider.rs).
# Same crate/build as the bridge (Dockerfile), different binary copied into a separate,
# minimal runtime image -- each named provider in compose.auction-demo.yml runs its own
# container from this image, its own random identity, never sharing a process or a
# private key with the bridge or any other provider.
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
    && cp target/release/auction-demo-provider /tmp/auction-demo-provider

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tmp/auction-demo-provider /usr/local/bin/auction-demo-provider
ENTRYPOINT ["/usr/local/bin/auction-demo-provider"]
