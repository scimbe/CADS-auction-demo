# CADS-auction-demo — a live dashboard over CADS-Tunnel's real auction algorithm

`https://auction-demo.bunsenbrenner.org` (once deployed) watches
`ct_common::pipeline::PipelineSpec::auction_view` — CADS-Tunnel's real, tested
per-role auction/clearing primitive — run live in the browser over Server-Sent
Events, clearing over offers submitted by six genuinely independent bidder
processes. It's the honest replacement pattern issue
[scimbe/CADS-Tunnel#180](https://github.com/scimbe/CADS-Tunnel/issues/180) asks for
(flappy-demo's own bridge still calls a hardcoded `demo_auction()` fixture) —
demonstrated end to end in a fresh, isolated demo rather than by modifying
flappy-demo's production bridge.

## What's real, what's simulated

- **Real**: every bidder is its own genuinely independent OS process
  (`auction-demo-provider`, `src/bin/provider.rs` — one container per named provider
  in `compose.auction-demo.yml`), holding its own randomly-generated ed25519
  identity, signing its own real `ct_common::channel::CapacityOffer`, and submitting
  it to the bridge's `POST /offers/submit` — which verifies the signature
  (`CapacityOffer::is_valid`) rather than trusting or fabricating it. The auction
  algorithm (`convene_with_policy`/`auction_view`), the `SelectionPolicy` variants,
  and the winner it computes are all real, running live on exactly those verified
  offers — switching policy visibly changes the outcome. A visitor can add one bid of
  their own too (`POST /offers/mine`).
- **Simulated**: nothing about the bidders, their identities, their signatures, or
  the clearing computation — the only thing generated per round is a visitor's own
  optional bid, same as everywhere else on these demos. Each bid also appears with a
  short deliberate pause during a round so a human can watch it happen — the real
  computation itself is effectively instant.

## Running it locally

```
docker compose -f compose.auction-demo.yml --env-file .env up --build -d \
  auction-demo-bridge auction-demo-provider-aurora auction-demo-provider-borealis \
  auction-demo-provider-cascade auction-demo-provider-delta auction-demo-provider-echo \
  auction-demo-provider-foxtrot
```
then open `http://127.0.0.1:8789/`, watch the six real bidders populate the board
over SSE, pick a policy, and click **Run auction round**. `docker compose up
<one-service>` does **not** work against this compose file (Compose interpolates
every service's required env vars up front — including `AUCTION_CERT_DIR` and the
Browser-Plane agent's vars — so it fails before a single-service `up` even starts,
live-verified) — bring up the bridge + providers together, or all services for the
full public path.

## Verifying the wiring without a browser

```
curl -N http://127.0.0.1:8789/events &
curl http://127.0.0.1:8789/offers   # real offers from the six real provider containers
curl -X POST 'http://127.0.0.1:8789/run?policy=lowest_floor'
```

Real output from the actual rework verification pass — six independently-signed
offers, a correct `lowest_floor` clear (borealis at 45 beats cascade at 55 and
aurora at 60; foxtrot at 22 beats delta at 30 and echo at 38):

```
data: {"type":"round_start","policy":"lowest_floor"}
data: {"type":"bid","role":"reviewer","who":"borealis","units":12,"price":45}
...
data: {"type":"role_cleared","role":"reviewer","bids":[
  {"who":"borealis","units":12,"price":45,"win":true},
  {"who":"cascade","units":20,"price":55,"win":false},
  {"who":"aurora","units":15,"price":60,"win":false}]}
data: {"type":"role_cleared","role":"writer","bids":[
  {"who":"foxtrot","units":28,"price":22,"win":true},
  {"who":"delta","units":25,"price":30,"win":false},
  {"who":"echo","units":30,"price":38,"win":false}]}
data: {"type":"round_done"}
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

Covers both `[[bin]]` targets (`auction-demo-bridge`, `auction-demo-provider`) —
including a test that a tampered offer (`min_price` changed after signing) correctly
fails `CapacityOffer::is_valid`, proving the bridge's server-side verification
actually rejects forgery rather than trusting the wire.

## Publishing it live (Browser Plane, same shape as help-site/flappy-demo)

```
AUCTION_CERT_DIR=<dir with fullchain.pem+privkey.pem, issued CORE-side> \
CP_URL=<control-plane URL> EDGE=<edge host:port> \
  ./run-demo.sh up
./run-demo.sh status
./run-demo.sh down
```

`run-demo.sh` mints a single-use join token, brings up the bridge + six providers +
Caddy origin + a Browser-Plane `ct-agent`, and polls until
`https://auction-demo.bunsenbrenner.org/` returns 200 — same pattern as
`CADS-flappy-demo/run-demo.sh` and CADS-Tunnel's `examples/help-site/run-demo.sh`.
The TLS certificate is issued **CORE-side** (deSEC DNS-01 with the operator's
zone-wide token) and relayed in as `fullchain.pem`/`privkey.pem` — this repo never
runs an ACME client or holds a DNS credential. See CADS-Tunnel's
`docs/dns01-desec.md` for the full cert-issuance walkthrough.

## Layout

- `src/main.rs` — the bridge: verifies real submitted `CapacityOffer`s
  (`POST /offers/submit`), accepts a visitor's own bid (`POST /offers/mine`), runs
  the real `ct_common::pipeline` auction logic, an SSE `/events` stream,
  `POST /run`/`POST /reset`, and serves the static dashboard.
- `src/bin/provider.rs` — `auction-demo-provider`: a genuinely independent bidder
  process. Generates its own real random ed25519 identity, signs its own real offer,
  submits it periodically. Never shares a process, filesystem, or private key with
  the bridge or any other provider.
- `index.html` — the dashboard: a live, read-only bidder board (populated by real
  provider submissions over SSE) plus a small form to add your own bid. Vanilla
  HTML/CSS/JS, no framework/build step, same convention as every other CADS-Tunnel
  demo.
- `Dockerfile` — builds the bridge from source. `Provider.Dockerfile` — builds
  `auction-demo-provider` from the same crate into its own, separate runtime image;
  `compose.auction-demo.yml` runs one container per named provider from it.
- `Agent.Dockerfile` / `Caddy.Dockerfile` / `Caddyfile` / `compose.auction-demo.yml`
  / `run-demo.sh` — the standalone Browser-Plane publishing pattern, copied from
  CADS-Tunnel's `examples/help-site/` rather than flappy-demo/cookbook-demo's
  workspace-relative build context (this repo has no CADS-Tunnel checkout to build
  against).
