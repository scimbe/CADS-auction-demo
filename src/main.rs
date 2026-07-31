//! CADS-auction-demo bridge: a live web dashboard over CADS-Tunnel's REAL
//! workflow-pipeline auction primitive (`ct_common::pipeline::PipelineSpec::auction_view`,
//! `convene_with_policy`, `SelectionPolicy`) — not a hardcoded fixture like
//! `crew_bridge.rs`'s `demo_auction()`. The offers start as clearly-labeled synthetic
//! demo providers (see README), but every term (price, units, even adding/removing a
//! bidder) is editable from the dashboard: `POST /offers` re-signs real
//! `ct_common::channel::CapacityOffer`s with the caller's own numbers, so the visitor
//! has genuine influence over the input, not just a choice of `SelectionPolicy`.

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use ct_common::channel::{CapacityKind, CapacityOffer, ServiceType};
use ct_common::pipeline::{PipelineSpec, RequiredRole, SelectionPolicy, SelectionState};
use ed25519_dalek::SigningKey;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

const ROLES: &[&str] = &["reviewer", "writer"];
/// Hard cap on bidders per role so a public-facing `/offers` can't be used to grow the
/// bridge's memory or the auction display without bound.
const MAX_OFFERS_PER_ROLE: usize = 8;
const MAX_NAME_LEN: usize = 40;

fn service_for_role(role: &str) -> Option<ServiceType> {
    match role {
        "reviewer" => Some(ServiceType::SecurityReview),
        "writer" => Some(ServiceType::TextGeneration),
        _ => None,
    }
}

fn demo_spec() -> PipelineSpec {
    PipelineSpec {
        id: "auction-demo".into(),
        roles: vec![
            RequiredRole { service: ServiceType::SecurityReview, units: 10, tag: "reviewer".into(), selection_policy: None },
            RequiredRole { service: ServiceType::TextGeneration, units: 20, tag: "writer".into(), selection_policy: None },
        ],
        operator_pubkey_hex: None,
        selection_policy: SelectionPolicy::LowestFloor,
    }
}

/// One editable bidder: the terms a visitor can set, plus the resulting *really signed*
/// offer. Re-derived whenever the terms change (`ProviderInput::sign`), never hand-built
/// as a `CapacityOffer` fixture.
#[derive(Clone, Deserialize)]
struct ProviderInput {
    role: String,
    who: String,
    units: u64,
    price: u64,
}

impl ProviderInput {
    /// Deterministic per-name identity: `sha256(who)` as the ed25519 seed, so re-signing
    /// the same visitor-chosen name (e.g. after editing its price) keeps the same holder
    /// key rather than minting a fresh stranger each time — and two different sessions
    /// typing the same name get the same identity too, matching this codebase's own
    /// fixed-seed demo-identity convention (`pipeline.rs`'s test `offer()`/`holder()`).
    fn seed(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.who.as_bytes());
        hasher.finalize().into()
    }

    fn sign(&self, now: u64) -> Option<CapacityOffer> {
        let service = service_for_role(&self.role)?;
        let sk = SigningKey::from_bytes(&self.seed());
        let far_future = now + 10 * 365 * 24 * 3600;
        Some(CapacityOffer::sign_new_with_services(
            &sk,
            CapacityKind::CloudApiQuota,
            vec!["claude".into()],
            self.units,
            self.price,
            "demo-credits".into(),
            now,
            far_future,
            vec![service],
        ))
    }
}

fn default_providers() -> Vec<ProviderInput> {
    vec![
        ProviderInput { role: "reviewer".into(), who: "aurora".into(), units: 15, price: 60 },
        ProviderInput { role: "reviewer".into(), who: "borealis".into(), units: 12, price: 45 },
        ProviderInput { role: "reviewer".into(), who: "cascade".into(), units: 20, price: 55 },
        ProviderInput { role: "writer".into(), who: "delta".into(), units: 25, price: 30 },
        ProviderInput { role: "writer".into(), who: "echo".into(), units: 30, price: 38 },
        ProviderInput { role: "writer".into(), who: "foxtrot".into(), units: 28, price: 22 },
    ]
}

/// Maps a signed offer's holder pubkey back to the display name the visitor gave it —
/// derived straight from `ProviderInput::seed`, so it always agrees with what `sign()`
/// actually produced.
fn label_for(providers: &[ProviderInput]) -> impl Fn(&[u8; 32]) -> String {
    let names: Vec<([u8; 32], String)> = providers
        .iter()
        .map(|p| (SigningKey::from_bytes(&p.seed()).verifying_key().to_bytes(), p.who.clone()))
        .collect();
    move |pk: &[u8; 32]| names.iter().find(|(k, _)| k == pk).map(|(_, n)| n.clone()).unwrap_or_else(|| "unknown".into())
}

struct BridgeState {
    spec: PipelineSpec,
    providers: Mutex<Vec<ProviderInput>>,
    selection: Mutex<SelectionState>,
    tx: broadcast::Sender<String>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

async fn broadcast_event(tx: &broadcast::Sender<String>, ev: Value) {
    let _ = tx.send(ev.to_string());
    // Small pacing delay so a human watching the dashboard can actually see each step,
    // not because the real computation is slow -- it isn't. Disclosed in the README.
    tokio::time::sleep(std::time::Duration::from_millis(180)).await;
}

#[derive(Deserialize)]
struct RunQuery {
    #[serde(default)]
    policy: Option<String>,
}

fn parse_policy(s: Option<&str>) -> SelectionPolicy {
    match s {
        Some("round_robin") => SelectionPolicy::RoundRobin,
        Some("least_calls") => SelectionPolicy::LeastCalls,
        _ => SelectionPolicy::LowestFloor,
    }
}

async fn run_handler(State(st): State<Arc<BridgeState>>, axum::extract::Query(q): axum::extract::Query<RunQuery>) -> impl IntoResponse {
    let policy = parse_policy(q.policy.as_deref());
    let policy_name = q.policy.unwrap_or_else(|| "lowest_floor".into());
    let tx = st.tx.clone();
    let st2 = st.clone();
    tokio::spawn(async move {
        broadcast_event(&tx, json!({"type": "round_start", "policy": policy_name})).await;
        let providers = st2.providers.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for p in &providers {
            broadcast_event(&tx, json!({"type": "bid", "role": p.role, "who": p.who, "units": p.units, "price": p.price})).await;
        }
        let now = now_secs();
        let offers: Vec<CapacityOffer> = providers.iter().filter_map(|p| p.sign(now)).collect();
        let label = label_for(&providers);
        let result = {
            let mut state = st2.selection.lock().unwrap_or_else(|e| e.into_inner());
            st2.spec.auction_view(&offers, now, policy, &mut state, label)
        };
        match result {
            Ok(views) => {
                for v in views {
                    broadcast_event(
                        &tx,
                        json!({"type": "role_cleared", "role": v.role, "bids": v.bids.iter().map(|b| json!({"who": b.who, "units": b.units, "price": b.price, "win": b.win})).collect::<Vec<_>>()}),
                    )
                    .await;
                }
                broadcast_event(&tx, json!({"type": "round_done"})).await;
            }
            Err(e) => {
                broadcast_event(&tx, json!({"type": "round_error", "message": e.to_string()})).await;
            }
        }
    });
    axum::http::StatusCode::ACCEPTED
}

async fn reset_handler(State(st): State<Arc<BridgeState>>) -> impl IntoResponse {
    *st.selection.lock().unwrap_or_else(|e| e.into_inner()) = SelectionState::default();
    *st.providers.lock().unwrap_or_else(|e| e.into_inner()) = default_providers();
    let _ = st.tx.send(json!({"type": "reset", "providers": default_providers().iter().map(|p| json!({"role": p.role, "who": p.who, "units": p.units, "price": p.price})).collect::<Vec<_>>()}).to_string());
    axum::http::StatusCode::NO_CONTENT
}

/// A visitor's proposed bidder set for one role — real influence over the auction's
/// input, not just a policy pick. Validated and re-signed, never trusted as-is.
#[derive(Deserialize)]
struct OffersReq {
    providers: Vec<ProviderInput>,
}

async fn offers_get_handler(State(st): State<Arc<BridgeState>>) -> impl IntoResponse {
    let providers = st.providers.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Json(providers.iter().map(|p| json!({"role": p.role, "who": p.who, "units": p.units, "price": p.price})).collect::<Vec<_>>())
}

async fn offers_post_handler(State(st): State<Arc<BridgeState>>, Json(req): Json<OffersReq>) -> impl IntoResponse {
    if req.providers.is_empty() {
        return (axum::http::StatusCode::BAD_REQUEST, "at least one provider required").into_response();
    }
    for role in ROLES {
        let count = req.providers.iter().filter(|p| &p.role == role).count();
        if count > MAX_OFFERS_PER_ROLE {
            return (axum::http::StatusCode::BAD_REQUEST, format!("at most {MAX_OFFERS_PER_ROLE} providers per role")).into_response();
        }
    }
    for p in &req.providers {
        if !ROLES.contains(&p.role.as_str()) {
            return (axum::http::StatusCode::BAD_REQUEST, format!("unknown role '{}' (must be one of {ROLES:?})", p.role)).into_response();
        }
        let trimmed = p.who.trim();
        if trimmed.is_empty() || trimmed.len() > MAX_NAME_LEN {
            return (axum::http::StatusCode::BAD_REQUEST, format!("provider name must be 1..={MAX_NAME_LEN} chars")).into_response();
        }
        if p.units == 0 {
            return (axum::http::StatusCode::BAD_REQUEST, "units must be > 0").into_response();
        }
    }
    let cleaned: Vec<ProviderInput> = req
        .providers
        .into_iter()
        .map(|p| ProviderInput { who: p.who.trim().to_string(), ..p })
        .collect();
    *st.providers.lock().unwrap_or_else(|e| e.into_inner()) = cleaned.clone();
    let _ = st.tx.send(
        json!({"type": "offers_updated", "providers": cleaned.iter().map(|p| json!({"role": p.role, "who": p.who, "units": p.units, "price": p.price})).collect::<Vec<_>>()})
            .to_string(),
    );
    axum::http::StatusCode::NO_CONTENT.into_response()
}

async fn events_handler(State(st): State<Arc<BridgeState>>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = st.tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(line) => Some(Ok(Event::default().data(line))),
        Err(_) => None, // a lagging subscriber just misses old events, never errors the stream
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

const INDEX_HTML: &str = include_str!("../index.html");

async fn index_handler() -> impl IntoResponse {
    Html(INDEX_HTML)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, _rx) = broadcast::channel::<String>(64);
    let state = Arc::new(BridgeState {
        spec: demo_spec(),
        providers: Mutex::new(default_providers()),
        selection: Mutex::new(SelectionState::default()),
        tx,
    });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/events", get(events_handler))
        .route("/run", post(run_handler))
        .route("/reset", post(reset_handler))
        .route("/offers", get(offers_get_handler).post(offers_post_handler))
        .with_state(state);

    let addr = std::env::var("AUCTION_BRIDGE_LISTEN").unwrap_or_else(|_| "0.0.0.0:8789".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("auction-demo-bridge: serving on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_offers_are_validly_signed_and_clear_via_the_real_auction() {
        let now = 1_000;
        let providers = default_providers();
        let offers: Vec<CapacityOffer> = providers.iter().filter_map(|p| p.sign(now)).collect();
        assert_eq!(offers.len(), providers.len());
        for o in &offers {
            assert!(o.is_valid(now), "every demo offer must carry a real, currently-valid signature");
        }

        let spec = demo_spec();
        let label = label_for(&providers);
        let mut state = SelectionState::default();
        let views = spec
            .auction_view(&offers, now, SelectionPolicy::LowestFloor, &mut state, label)
            .expect("both roles have qualifying offers");
        assert_eq!(views.len(), 2);

        let reviewer = views.iter().find(|v| v.role == "reviewer").unwrap();
        assert_eq!(reviewer.bids.len(), 3, "all three reviewer providers are shown");
        let winner = reviewer.bids.iter().find(|b| b.win).unwrap();
        assert_eq!(winner.who, "borealis", "cheapest reviewer floor (45) wins under LowestFloor");

        let writer = views.iter().find(|v| v.role == "writer").unwrap();
        let winner = writer.bids.iter().find(|b| b.win).unwrap();
        assert_eq!(winner.who, "foxtrot", "cheapest writer floor (22) wins under LowestFloor");
    }

    #[test]
    fn round_robin_alternates_across_repeated_clears_unlike_lowest_floor() {
        // Demonstrates the actual point of the demo: switching SelectionPolicy really
        // changes the outcome, using the real convene_with_policy/auction_view logic.
        let now = 1_000;
        let providers = default_providers();
        let offers: Vec<CapacityOffer> = providers.iter().filter_map(|p| p.sign(now)).collect();
        let spec = demo_spec();
        let label = label_for(&providers);

        let mut lf_state = SelectionState::default();
        let a = spec.auction_view(&offers, now, SelectionPolicy::LowestFloor, &mut lf_state, &label).unwrap();
        let b = spec.auction_view(&offers, now, SelectionPolicy::LowestFloor, &mut lf_state, &label).unwrap();
        let winner_a = a.iter().find(|v| v.role == "reviewer").unwrap().bids.iter().find(|x| x.win).unwrap().who.clone();
        let winner_b = b.iter().find(|v| v.role == "reviewer").unwrap().bids.iter().find(|x| x.win).unwrap().who.clone();
        assert_eq!(winner_a, winner_b, "LowestFloor is deterministic across repeated clears");

        let mut rr_state = SelectionState::default();
        let mut winners = vec![];
        for _ in 0..3 {
            let v = spec.auction_view(&offers, now, SelectionPolicy::RoundRobin, &mut rr_state, &label).unwrap();
            winners.push(v.iter().find(|x| x.role == "reviewer").unwrap().bids.iter().find(|x| x.win).unwrap().who.clone());
        }
        assert_ne!(winners[0], winners[1], "RoundRobin rotates the winner across clears, ignoring price");
    }

    #[test]
    fn user_edited_offer_terms_flow_through_to_the_real_clear() {
        // The point of /offers: a visitor's own numbers, re-signed for real, actually
        // change who wins -- not a cosmetic edit.
        let now = 1_000;
        let mut providers = default_providers();
        // Undercut the current cheapest reviewer bid (borealis @ 45) with a new bidder.
        providers.push(ProviderInput { role: "reviewer".into(), who: "zeta".into(), units: 10, price: 5 });
        let offers: Vec<CapacityOffer> = providers.iter().filter_map(|p| p.sign(now)).collect();
        let spec = demo_spec();
        let label = label_for(&providers);
        let mut state = SelectionState::default();
        let views = spec.auction_view(&offers, now, SelectionPolicy::LowestFloor, &mut state, label).unwrap();
        let reviewer = views.iter().find(|v| v.role == "reviewer").unwrap();
        assert_eq!(reviewer.bids.len(), 4, "the new bidder is shown alongside the defaults");
        let winner = reviewer.bids.iter().find(|b| b.win).unwrap();
        assert_eq!(winner.who, "zeta", "the visitor's cheaper offer wins for real");
    }
}
