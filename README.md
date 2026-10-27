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

## What a visitor does

One input (a text and a budget), one button: **Auftrag ausschreiben**. The bridge then runs
one real tender, streamed (`POST /tender`, SSE) to that visitor only:

1. it asks all six provider processes live for a signed `CapacityOffer` (bids appear in
   arrival order, with the measured answer time);
2. it verifies each offer (`CapacityOffer::is_valid`) and drops bids that cannot be part of
   any plan within the budget (shown as "over budget");
3. `PipelineSpec::auction_view` clears the two-role pipeline (writer, reviewer) with the
   chosen `SelectionPolicy` (lowest floor, round robin, least calls);
4. the winners really do the work (extractive summary, then a review report against the
   original) and sign the result; the bridge verifies the signature against the winner's
   key from its offer.

## What's real, what's not

- **Real**: six independent processes (`src/bin/provider.rs`), each with its own random
  ed25519 key, signing its own offers and results; the clearing algorithm and its winner;
  the measured timings; the work itself. The one-line "why" is derived from the data the
  algorithm actually used (price vs. runner-up, rotation, jobs so far). No latency or
  reputation weights are shown because the algorithm has none.
- **Not real**: the work is deterministic text processing, not an LLM. The short pause
  between cards in the browser is display pacing only. Offers are requested on demand by
  the bridge (a request for quote) instead of being published to a plane.

## Running it locally

```
cargo build --bins
# six providers (PROVIDER_NAME, PROVIDER_ROLE=writer|reviewer, PROVIDER_UNITS, PROVIDER_PRICE,
# PROVIDER_LISTEN) and the bridge (AUCTION_PROVIDERS=comma-separated provider URLs)
```
or `docker compose -f compose.auction-demo.selfservice.yml --env-file .env.selfservice up --build -d`.
Providers are reachable only on the compose network (`expose`, no host ports).

```
curl -N -X POST localhost:8789/tender -H 'content-type: application/json' \
  -d '{"text":"Die Stadt baut 1200 Wohnungen. ...","budget":100,"policy":"lowest_floor"}'
curl localhost:8789/roster   # live health of the providers
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

## Self-hosting: running your own instance (not on the operator's host)

Everything above the "Publishing it live" section works with zero CADS-Tunnel
plane at all (local Docker Compose only). Only the public Browser-Plane
publishing step needs one, and it doesn't have to be the operator's:

1. **Your own CADS-Tunnel plane** — `./scripts/deploy-selfhost.sh --frontdoor`
   in a `CADS-Tunnel` checkout (see its
   [`docs/ops/runbook.md`](https://github.com/scimbe/CADS-Tunnel/blob/main/docs/ops/runbook.md)).
   Generic, not tied to the operator's domain/account: set `DESEC_TOKEN` and
   `PORTAL_PUBLIC_HOST` to **your own** domain (deSEC is free and works with any
   domain you own, or even a free `yourname.dedyn.io` name — see
   [`docs/dns01-desec.md`](https://github.com/scimbe/CADS-Tunnel/blob/main/docs/dns01-desec.md)).
2. **A cert for your own subdomain** (e.g. `auction-demo.yourdomain.tld`) —
   same deSEC DNS-01 mechanism your plane's front door already uses, or any
   ACME method you prefer — into a local `fullchain.pem`/`privkey.pem` dir.
3. **Run it against your plane, not the operator's:**
   ```
   AUCTION_CERT_DIR=/path/to/your/cert-dir \
   HOSTNAME_FQDN=auction-demo.yourdomain.tld \
   CP_URL=http://<your-plane-host>:8090 EDGE=<your-plane-host>:4433 \
     ./run-demo.sh up
   ```
   (`run-demo.sh`'s own header comment documents every override var, including
   host-authorization vars if your plane's edge gates that.)
4. Point DNS for `auction-demo.yourdomain.tld` at your plane's host, then verify
   with the same `curl`/browser checks in "Verifying the wiring" above, against
   your own URL.

Once this runs stably end to end on your own infrastructure, the operator's copy
can be taken down.

## Layout

- `src/main.rs` — the bridge: `POST /tender` (SSE per visitor, max 4 concurrent), `GET /roster`,
  and the page.
- `src/lib.rs` — the real work functions and the signed-result format shared with providers.
- `src/bin/provider.rs` — `auction-demo-provider`: a genuinely independent bidder
  process. Generates its own real random ed25519 identity and serves `/quote` (signed offer),
  `/job` (work + signed result) and `/health`. Never shares a process, filesystem, or private key with
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
