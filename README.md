# CADS-auction-demo — a live dashboard over CADS-Tunnel's real auction algorithm

`https://auction-demo.bunsenbrenner.org` (once deployed) watches
`ct_common::pipeline::PipelineSpec::auction_view` — CADS-Tunnel's real, tested
per-role auction/clearing primitive — run live in the browser over Server-Sent
Events. It's the honest replacement pattern issue
[scimbe/CADS-Tunnel#180](https://github.com/scimbe/CADS-Tunnel/issues/180) asks for
(flappy-demo's own bridge still calls a hardcoded `demo_auction()` fixture) —
demonstrated end to end in a fresh, isolated demo rather than by modifying
flappy-demo's production bridge.

## What's real, what's simulated

- **Real**: the auction algorithm (`convene_with_policy`/`auction_view`), the
  `SelectionPolicy` variants (`LowestFloor`/`RoundRobin`/`LeastCalls`), the signed
  `CapacityOffer` structs, the winner each computes, and how switching policy
  visibly changes the outcome.
- **Simulated**: the six named providers (`aurora`, `borealis`, `cascade`, `delta`,
  `echo`, `foxtrot`) are fixed demo identities this bridge signs at startup, not
  agents actually online somewhere. A real deployment would discover offers from
  `GET /registry/agents` instead (see `src/main.rs`'s `demo_offers`/`demo_spec`).
  Each bid also appears with a short deliberate pause during a round so a human can
  watch it happen — the real computation itself is effectively instant.

## Running it locally (no tunnel needed)

Build and run the bridge directly — `docker compose up <one-service>` does NOT work for
this here, since Compose interpolates every service's env vars up front (including the
other two services' required `AUCTION_CERT_DIR`/`AUCTION_AGENT_EDGE`/etc.), so it fails
before a single-service `up` even starts, live-verified while writing this doc:

```
docker build -t auction-demo-bridge .
docker run --rm -p 8789:8789 auction-demo-bridge
```
then open `http://127.0.0.1:8789/`, pick a policy, and click **Run auction round**.
Watch `/reviewer`/`/writer` fill in live over SSE; switch policy and re-run to see
the winner actually change.

## Verifying the wiring without a browser

```
curl -N http://127.0.0.1:8789/events &
curl -X POST 'http://127.0.0.1:8789/run?policy=round_robin'
```

## Hermetic build/test

No local Rust toolchain assumed — build and test through Docker, matching the
convention used across the CADS-Tunnel ecosystem this session:

```
docker run --rm -v "$PWD":/work -w /work \
  -v cads-auction-demo-cargo-registry:/usr/local/cargo/registry \
  -v cads-auction-demo-cargo-git:/usr/local/cargo/git \
  -e RUSTFLAGS='-D warnings' rust:1-slim-bookworm bash -c 'cargo test'
```

## Publishing it live (Browser Plane, same shape as help-site/flappy-demo)

```
AUCTION_CERT_DIR=<dir with fullchain.pem+privkey.pem, issued CORE-side> \
CP_URL=<control-plane URL> EDGE=<edge host:port> \
  ./run-demo.sh up
./run-demo.sh status
./run-demo.sh down
```

`run-demo.sh` mints a single-use join token, brings up the bridge + Caddy origin +
a Browser-Plane `ct-agent`, and polls until `https://auction-demo.bunsenbrenner.org/`
returns 200 — same pattern as `CADS-flappy-demo/run-demo.sh` and CADS-Tunnel's
`examples/help-site/run-demo.sh`. The TLS certificate is issued **CORE-side**
(deSEC DNS-01 with the operator's zone-wide token) and relayed in as
`fullchain.pem`/`privkey.pem` — this repo never runs an ACME client or holds a DNS
credential. See CADS-Tunnel's `docs/dns01-desec.md` for the full cert-issuance
walkthrough.

## Layout

- `src/main.rs` — the bridge: real `ct_common::pipeline` auction logic, an SSE
  `/events` stream, `POST /run`/`POST /reset`, and serves the static dashboard.
- `index.html` — the dashboard (vanilla HTML/CSS/JS, no framework/build step, same
  convention as every other CADS-Tunnel demo).
- `Dockerfile` — builds the bridge from source.
- `Agent.Dockerfile` / `Caddy.Dockerfile` / `Caddyfile` / `compose.auction-demo.yml`
  / `run-demo.sh` — the standalone Browser-Plane publishing pattern, copied from
  CADS-Tunnel's `examples/help-site/` rather than flappy-demo/cookbook-demo's
  workspace-relative build context (this repo has no CADS-Tunnel checkout to build
  against).
