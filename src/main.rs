//! CADS-auction-demo bridge: a live web dashboard over CADS-Tunnel's REAL
//! workflow-pipeline auction primitive (`ct_common::pipeline::PipelineSpec::auction_view`,
//! `convene_with_policy`, `SelectionPolicy`) — not a hardcoded fixture like
//! `crew_bridge.rs`'s `demo_auction()`. The roster of bidders is real too: each named
//! provider (`aurora`, `borealis`, ...) is its own genuinely independent OS process
//! (`auction-demo-provider`, this repo's `src/bin/provider.rs`), holding its own real,
//! randomly-generated ed25519 identity, signing its own real
//! `ct_common::channel::CapacityOffer`, and submitting it to `POST /offers/submit` —
//! which this bridge verifies (`CapacityOffer::is_valid`), never re-signs or trusts
//! blind. The only thing a visitor can add on top is their own single bid via
//! `POST /offers/mine`, clearly a visitor-supplied input (like typing a task), not a
//! fabricated roster of competitors.

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
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

const ROLES: &[&str] = &["reviewer", "writer"];
/// Hard cap on bidders per role so a public-facing submit endpoint can't be used to
/// grow the bridge's memory or the auction display without bound.
const MAX_OFFERS_PER_ROLE: usize = 12;
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

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A real, independently-signed offer the bridge has accepted: from a genuinely
/// separate `auction-demo-provider` process (the normal case) or from a visitor's own
/// `POST /offers/mine` bid. Either way `offer` is a real, already-verified
/// `CapacityOffer` -- this struct never carries anything the bridge fabricated on
/// someone else's behalf.
#[derive(Clone)]
struct SubmittedOffer {
    display_name: String,
    role: String,
    offer: CapacityOffer,
}

struct BridgeState {
    spec: PipelineSpec,
    /// Keyed by the offer's real holder pubkey so a provider's periodic re-submission
    /// updates its own entry instead of accumulating duplicates.
    offers: Mutex<HashMap<[u8; 32], SubmittedOffer>>,
    selection: Mutex<SelectionState>,
    tx: broadcast::Sender<String>,
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

/// Snapshot the currently-valid offers (drops anything expired -- real time-bound
/// state, not a fixture) and build the pubkey->display-name label the real
/// `auction_view` needs.
fn live_offers(st: &BridgeState, now: u64) -> (Vec<CapacityOffer>, HashMap<[u8; 32], String>) {
    let map = st.offers.lock().unwrap_or_else(|e| e.into_inner());
    let mut offers = Vec::new();
    let mut labels = HashMap::new();
    for entry in map.values() {
        if entry.offer.is_valid(now) {
            labels.insert(entry.offer.holder_pubkey, entry.display_name.clone());
            offers.push(entry.offer.clone());
        }
    }
    (offers, labels)
}

async fn run_handler(State(st): State<Arc<BridgeState>>, axum::extract::Query(q): axum::extract::Query<RunQuery>) -> impl IntoResponse {
    let policy = parse_policy(q.policy.as_deref());
    let policy_name = q.policy.unwrap_or_else(|| "lowest_floor".into());
    let tx = st.tx.clone();
    let st2 = st.clone();
    tokio::spawn(async move {
        broadcast_event(&tx, json!({"type": "round_start", "policy": policy_name})).await;
        let now = now_secs();
        let (offers, labels) = live_offers(&st2, now);
        for o in &offers {
            let who = labels.get(&o.holder_pubkey).cloned().unwrap_or_else(|| "unknown".into());
            let role = if o.services.contains(&ServiceType::SecurityReview) { "reviewer" } else { "writer" };
            broadcast_event(&tx, json!({"type": "bid", "role": role, "who": who, "units": o.units_available, "price": o.min_price})).await;
        }
        let label = move |pk: &[u8; 32]| labels.get(pk).cloned().unwrap_or_else(|| "unknown".into());
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

/// Clears every currently-accepted offer. Real, independently-operated providers
/// self-heal within their own resubmission interval (`SUBMIT_INTERVAL_SECS`, default
/// 20s) -- this bridge never re-creates their bids on their behalf.
async fn reset_handler(State(st): State<Arc<BridgeState>>) -> impl IntoResponse {
    *st.selection.lock().unwrap_or_else(|e| e.into_inner()) = SelectionState::default();
    st.offers.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let _ = st.tx.send(json!({"type": "reset"}).to_string());
    axum::http::StatusCode::NO_CONTENT
}

async fn offers_get_handler(State(st): State<Arc<BridgeState>>) -> impl IntoResponse {
    let now = now_secs();
    let map = st.offers.lock().unwrap_or_else(|e| e.into_inner());
    let list: Vec<Value> = map
        .values()
        .filter(|e| e.offer.is_valid(now))
        .map(|e| json!({"role": e.role, "who": e.display_name, "units": e.offer.units_available, "price": e.offer.min_price}))
        .collect();
    Json(list)
}

/// A real, independently-signed offer from a genuinely separate provider process
/// (`src/bin/provider.rs`) — verified here (`CapacityOffer::is_valid`), never
/// re-signed or fabricated by this bridge.
#[derive(Deserialize)]
struct SubmitReq {
    display_name: String,
    role: String,
    offer: CapacityOffer,
}

async fn offers_submit_handler(State(st): State<Arc<BridgeState>>, Json(req): Json<SubmitReq>) -> impl IntoResponse {
    if !ROLES.contains(&req.role.as_str()) {
        return (axum::http::StatusCode::BAD_REQUEST, format!("unknown role '{}' (must be one of {ROLES:?})", req.role)).into_response();
    }
    let name = req.display_name.trim();
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return (axum::http::StatusCode::BAD_REQUEST, format!("display_name must be 1..={MAX_NAME_LEN} chars")).into_response();
    }
    let now = now_secs();
    if !req.offer.is_valid(now) {
        return (axum::http::StatusCode::UNAUTHORIZED, "offer signature does not verify (or is expired) -- rejected, not re-signed").into_response();
    }
    let Some(expected_service) = service_for_role(&req.role) else {
        return (axum::http::StatusCode::BAD_REQUEST, "unreachable: role already validated").into_response();
    };
    if !req.offer.services.contains(&expected_service) {
        return (axum::http::StatusCode::BAD_REQUEST, format!("offer's signed services don't include {expected_service:?} required for role '{}'", req.role)).into_response();
    }
    let mut map = st.offers.lock().unwrap_or_else(|e| e.into_inner());
    let already_here = map.contains_key(&req.offer.holder_pubkey);
    if !already_here {
        let count = map.values().filter(|e| e.role == req.role).count();
        if count >= MAX_OFFERS_PER_ROLE {
            return (axum::http::StatusCode::TOO_MANY_REQUESTS, format!("at most {MAX_OFFERS_PER_ROLE} bidders per role")).into_response();
        }
    }
    let holder = req.offer.holder_pubkey;
    let units = req.offer.units_available;
    let price = req.offer.min_price;
    map.insert(holder, SubmittedOffer { display_name: name.to_string(), role: req.role.clone(), offer: req.offer });
    drop(map);
    let _ = st.tx.send(json!({"type": "offer_submitted", "role": req.role, "who": name, "units": units, "price": price}).to_string());
    axum::http::StatusCode::NO_CONTENT.into_response()
}

/// A visitor's own single bid -- genuine influence over the input (like typing a task),
/// not a fabricated roster of competitors. Signed server-side with a name-derived key
/// (so re-submitting the same name updates the same entry) purely because a browser
/// tab has no ed25519 identity of its own to sign with; every other bidder on the
/// board is a real, separate process (see `POST /offers/submit`).
#[derive(Deserialize)]
struct MineReq {
    role: String,
    who: String,
    units: u64,
    price: u64,
}

async fn offers_mine_handler(State(st): State<Arc<BridgeState>>, Json(req): Json<MineReq>) -> impl IntoResponse {
    if !ROLES.contains(&req.role.as_str()) {
        return (axum::http::StatusCode::BAD_REQUEST, format!("unknown role '{}' (must be one of {ROLES:?})", req.role)).into_response();
    }
    let who = req.who.trim();
    if who.is_empty() || who.len() > MAX_NAME_LEN {
        return (axum::http::StatusCode::BAD_REQUEST, format!("name must be 1..={MAX_NAME_LEN} chars")).into_response();
    }
    if req.units == 0 {
        return (axum::http::StatusCode::BAD_REQUEST, "units must be > 0").into_response();
    }
    let service = service_for_role(&req.role).expect("role already validated");
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&Sha256::digest(format!("visitor:{who}").as_bytes()));
    let sk = SigningKey::from_bytes(&seed);
    let now = now_secs();
    let offer = CapacityOffer::sign_new_with_services(
        &sk,
        CapacityKind::CloudApiQuota,
        vec!["claude".into()],
        req.units,
        req.price,
        "demo-credits".into(),
        now,
        now + 10 * 365 * 24 * 3600,
        vec![service],
    );
    let mut map = st.offers.lock().unwrap_or_else(|e| e.into_inner());
    let already_here = map.contains_key(&offer.holder_pubkey);
    if !already_here {
        let count = map.values().filter(|e| e.role == req.role).count();
        if count >= MAX_OFFERS_PER_ROLE {
            return (axum::http::StatusCode::TOO_MANY_REQUESTS, format!("at most {MAX_OFFERS_PER_ROLE} bidders per role")).into_response();
        }
    }
    map.insert(offer.holder_pubkey, SubmittedOffer { display_name: format!("{who} (you)"), role: req.role.clone(), offer });
    drop(map);
    let _ = st.tx.send(json!({"type": "offer_submitted", "role": req.role, "who": format!("{who} (you)"), "units": req.units, "price": req.price}).to_string());
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
        offers: Mutex::new(HashMap::new()),
        selection: Mutex::new(SelectionState::default()),
        tx,
    });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/events", get(events_handler))
        .route("/run", post(run_handler))
        .route("/reset", post(reset_handler))
        .route("/offers", get(offers_get_handler))
        .route("/offers/submit", post(offers_submit_handler))
        .route("/offers/mine", post(offers_mine_handler))
        .with_state(state);

    let addr = std::env::var("AUCTION_BRIDGE_LISTEN").unwrap_or_else(|_| "0.0.0.0:8789".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("auction-demo-bridge: serving on {addr} -- waiting for real provider processes to submit offers");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real_offer(seed_input: &str, role: &str, units: u64, price: u64, now: u64) -> CapacityOffer {
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&Sha256::digest(seed_input.as_bytes()));
        let sk = SigningKey::from_bytes(&seed);
        CapacityOffer::sign_new_with_services(
            &sk,
            CapacityKind::CloudApiQuota,
            vec!["claude".into()],
            units,
            price,
            "demo-credits".into(),
            now,
            now + 10 * 365 * 24 * 3600,
            vec![service_for_role(role).unwrap()],
        )
    }

    #[test]
    fn a_real_auction_round_clears_over_independently_signed_offers() {
        let now = now_secs();
        let spec = demo_spec();
        let mut selection = SelectionState::default();
        let offers = vec![
            real_offer("aurora", "reviewer", 15, 60, now),
            real_offer("borealis", "reviewer", 12, 45, now),
            real_offer("delta", "writer", 25, 30, now),
        ];
        let mut labels = HashMap::new();
        labels.insert(offers[0].holder_pubkey, "aurora".to_string());
        labels.insert(offers[1].holder_pubkey, "borealis".to_string());
        labels.insert(offers[2].holder_pubkey, "delta".to_string());
        let label = move |pk: &[u8; 32]| labels.get(pk).cloned().unwrap_or_else(|| "unknown".into());
        let views = spec.auction_view(&offers, now, SelectionPolicy::LowestFloor, &mut selection, label).expect("clears");
        let reviewer = views.iter().find(|v| v.role == "reviewer").expect("reviewer role present");
        let winner = reviewer.bids.iter().find(|b| b.win).expect("a winner exists");
        assert_eq!(winner.who, "borealis", "lowest floor among {{45, 60}} must win");
    }

    #[test]
    fn is_valid_rejects_a_tampered_offer() {
        let now = now_secs();
        let mut offer = real_offer("aurora", "reviewer", 15, 60, now);
        offer.min_price = 1; // tamper after signing -- signature no longer covers this value
        assert!(!offer.is_valid(now), "a tampered offer must not verify");
    }

    #[test]
    fn different_seeds_never_collide_on_holder_pubkey() {
        let now = now_secs();
        let a = real_offer("aurora", "reviewer", 15, 60, now);
        let b = real_offer("borealis", "reviewer", 12, 45, now);
        assert_ne!(a.holder_pubkey, b.holder_pubkey);
    }
}
