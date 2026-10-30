//! CADS-auction-demo bridge: serves the page and runs one REAL tender per visitor request
//! (`POST /tender`, streamed as Server-Sent Events back to that visitor only):
//!  1. asks every provider process live for a fresh signed `CapacityOffer` (a real bid),
//!  2. verifies each offer (`CapacityOffer::is_valid`), applies the visitor's budget,
//!  3. clears the two-role pipeline with `ct_common::pipeline::PipelineSpec::auction_view`,
//!  4. dispatches the work to the winners (summary, then review) and verifies that each
//!     result is signed by the winner's key from its offer.
//! Nothing is simulated: bids, winners, timings and results all come from the provider
//! processes. The only artificial thing is a short display pause in the browser.

use auction_demo_bridge::{hex, verify_result, JobReq, JobResp, MAX_TEXT_CHARS};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use ct_common::channel::{CapacityOffer, ServiceType};
use ct_common::pipeline::{PipelineSpec, RequiredRole, SelectionPolicy, SelectionState};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Semaphore};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

const MAX_BUDGET: u64 = 10_000;
const MAX_CONCURRENT_TENDERS: usize = 4;
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(3);
const ROSTER_TTL: Duration = Duration::from_secs(5);
const DEFAULT_PROVIDERS: &str = "http://auction-demo-provider-aurora:8080,http://auction-demo-provider-borealis:8080,http://auction-demo-provider-cascade:8080,http://auction-demo-provider-delta:8080,http://auction-demo-provider-echo:8080,http://auction-demo-provider-foxtrot:8080";

fn service_for_role(role: &str) -> Option<ServiceType> {
    match role {
        "reviewer" => Some(ServiceType::SecurityReview),
        "writer" => Some(ServiceType::TextGeneration),
        _ => None,
    }
}

/// Step order = role order: write first, then review.
fn demo_spec() -> PipelineSpec {
    PipelineSpec {
        id: "auction-demo".into(),
        roles: vec![
            RequiredRole { service: ServiceType::TextGeneration, units: 20, tag: "writer".into(), selection_policy: None },
            RequiredRole { service: ServiceType::SecurityReview, units: 10, tag: "reviewer".into(), selection_policy: None },
        ],
        operator_pubkey_hex: None,
        selection_policy: SelectionPolicy::LowestFloor,
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn name_from_url(url: &str) -> String {
    let host = url.trim_start_matches("http://").split(':').next().unwrap_or(url);
    host.rsplit('-').next().unwrap_or(host).to_string()
}

fn parse_policy(s: &str) -> Option<SelectionPolicy> {
    match s {
        "lowest_floor" => Some(SelectionPolicy::LowestFloor),
        "round_robin" => Some(SelectionPolicy::RoundRobin),
        "least_calls" => Some(SelectionPolicy::LeastCalls),
        _ => None,
    }
}

struct Bridge {
    spec: PipelineSpec,
    providers: Vec<String>,
    selection: Mutex<SelectionState>,
    slots: Arc<Semaphore>,
    http: reqwest::Client,
    roster: Mutex<Option<(Instant, Value)>>,
}

#[derive(Deserialize)]
struct Quote {
    display_name: String,
    role: String,
    offer: CapacityOffer,
    #[serde(default)]
    jobs_done: u64,
}

struct Bidder {
    name: String,
    url: String,
    role: String,
    offer: CapacityOffer,
    jobs_done: u64,
}

#[derive(Deserialize)]
struct TenderReq {
    text: String,
    budget: u64,
    #[serde(default)]
    policy: Option<String>,
}

async fn fetch_quote(http: &reqwest::Client, url: &str, now: u64) -> Result<(Quote, f64), String> {
    let t0 = Instant::now();
    let resp = http.post(format!("{url}/quote")).timeout(PROVIDER_TIMEOUT).send().await.map_err(|e| if e.is_timeout() { "timeout".to_string() } else { "unreachable".to_string() })?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status().as_u16()));
    }
    let q: Quote = resp.json().await.map_err(|_| "bad answer".to_string())?;
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let expected = service_for_role(&q.role).ok_or("unknown role")?;
    if q.display_name.trim().is_empty() || q.display_name.len() > 40 {
        return Err("bad name".into());
    }
    if !q.offer.is_valid(now) {
        return Err("invalid signature".into());
    }
    if !q.offer.services.contains(&expected) {
        return Err("wrong service".into());
    }
    Ok((q, ms))
}

type Tx = mpsc::Sender<Result<Event, Infallible>>;

async fn emit(tx: &Tx, ev: Value) -> Option<()> {
    tx.send(Ok(Event::default().data(ev.to_string()))).await.ok()
}

/// Real, data-derived reason why the winner won under the policy that actually cleared.
fn why(policy: &str, bids: &[ct_common::pipeline::RoleBidView], before: &SelectionState, role: &str, pk_by_name: &HashMap<String, [u8; 32]>, name_by_pk: &HashMap<[u8; 32], String>) -> Value {
    let Some(w) = bids.iter().find(|b| b.win) else { return json!({"kind": policy}) };
    match policy {
        "round_robin" => json!({"kind": "round_robin", "previous": before.round_robin_cursor.get(role).and_then(|pk| name_by_pk.get(pk))}),
        "least_calls" => json!({"kind": "least_calls", "jobs": pk_by_name.get(&w.who).and_then(|pk| before.served_counts.get(pk)).copied().unwrap_or(0)}),
        _ => {
            let next = bids.iter().filter(|b| !b.win).min_by_key(|b| b.price);
            json!({"kind": "lowest_floor", "next": next.map(|n| json!({"who": n.who, "price": n.price}))})
        }
    }
}

struct ClearedRole {
    view: ct_common::pipeline::RoleAuctionView,
    /// Bids of this role the budget left out: (name, price).
    over_budget: Vec<(String, u64)>,
}

/// Clears the roles in order, so the budget binds the actual winners and not just the cheapest
/// possible plan: each role only sees bids that leave enough for the cheapest bid of every later
/// role, given what the earlier roles' winners really cost. Filtering every role once against the
/// other roles' minimum let round robin pick 38 + 60 = 98 on a budget of 90.
fn clear_within_budget(
    spec: &PipelineSpec,
    bidders: &[Bidder],
    budget: u64,
    now: u64,
    policy: SelectionPolicy,
    state: &mut SelectionState,
    names: &HashMap<[u8; 32], String>,
) -> Result<Vec<ClearedRole>, String> {
    let mut out = Vec::new();
    let mut remaining = budget;
    for (i, role) in spec.roles.iter().enumerate() {
        let min_of_later: u64 = spec.roles[i + 1..]
            .iter()
            .filter_map(|r| bidders.iter().filter(|x| x.role == r.tag).map(|x| x.offer.min_price).min())
            .sum();
        let (mut offers, mut over_budget) = (Vec::new(), Vec::new());
        for x in bidders.iter().filter(|x| x.role == role.tag) {
            if x.offer.min_price + min_of_later <= remaining {
                offers.push(x.offer.clone());
            } else {
                over_budget.push((x.name.clone(), x.offer.min_price));
            }
        }
        let one_role = PipelineSpec { roles: vec![role.clone()], ..spec.clone() };
        let labels = names.clone();
        let mut views = one_role
            .auction_view(&offers, now, policy, state, move |pk| labels.get(pk).cloned().unwrap_or_else(|| "unknown".into()))
            .map_err(|e| e.to_string())?;
        let view = views.pop().ok_or("no view")?;
        let price = view.bids.iter().find(|b| b.win).map(|b| b.price).ok_or("no winner")?;
        remaining = remaining.saturating_sub(price);
        out.push(ClearedRole { view, over_budget });
    }
    Ok(out)
}

async fn run_tender(b: Arc<Bridge>, req: TenderReq, policy_name: String, policy: SelectionPolicy, tx: Tx) -> Option<()> {
    let t_start = Instant::now();
    let now = now_secs();
    let job_id = hex(&rand::random::<[u8; 8]>());
    emit(&tx, json!({"type": "tender", "id": job_id, "budget": req.budget, "policy": policy_name, "providers": b.providers.len()})).await?;

    // 1. live request-for-quote to every provider, concurrently; bids are shown in arrival order.
    let (qtx, mut qrx) = mpsc::channel::<(String, String, Result<(Quote, f64), String>)>(16);
    for url in &b.providers {
        let (http, url, qtx) = (b.http.clone(), url.clone(), qtx.clone());
        tokio::spawn(async move {
            let r = fetch_quote(&http, &url, now).await;
            let _ = qtx.send((name_from_url(&url), url, r)).await;
        });
    }
    drop(qtx);
    let mut bidders: Vec<Bidder> = Vec::new();
    while let Some((name, url, r)) = qrx.recv().await {
        match r {
            Ok((q, ms)) => {
                if bidders.iter().any(|x| x.offer.holder_pubkey == q.offer.holder_pubkey) {
                    continue;
                }
                emit(&tx, json!({"type": "bid", "role": q.role, "who": q.display_name, "price": q.offer.min_price, "units": q.offer.units_available, "jobs_done": q.jobs_done, "ms": (ms * 10.0).round() / 10.0, "key": &hex(&q.offer.holder_pubkey)[..8]})).await?;
                bidders.push(Bidder { name: q.display_name, url, role: q.role, offer: q.offer, jobs_done: q.jobs_done });
            }
            Err(reason) => {
                emit(&tx, json!({"type": "no_bid", "who": name, "reason": reason})).await?;
            }
        }
    }

    // 2.+3. budget and the real clear, role by role.
    let min_of = |role: &str| bidders.iter().filter(|x| x.role == role).map(|x| x.offer.min_price).min();
    let (Some(min_w), Some(min_r)) = (min_of("writer"), min_of("reviewer")) else {
        let role = if min_of("writer").is_none() { "writer" } else { "reviewer" };
        return emit(&tx, json!({"type": "error", "code": "unfilled", "role": role})).await;
    };
    if min_w + min_r > req.budget {
        return emit(&tx, json!({"type": "error", "code": "budget", "need": min_w + min_r, "budget": req.budget})).await;
    }
    let name_by_pk: HashMap<[u8; 32], String> = bidders.iter().map(|x| (x.offer.holder_pubkey, x.name.clone())).collect();
    let pk_by_name: HashMap<String, [u8; 32]> = bidders.iter().map(|x| (x.name.clone(), x.offer.holder_pubkey)).collect();
    let (before, cleared) = {
        let mut state = b.selection.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(policy, SelectionPolicy::LeastCalls) {
            // "Fewest jobs" must mean the jobs shown on the bid cards. `served_counts` alone only
            // ever counts wins under this one policy, so a provider with 7 done jobs would read 0.
            for x in &bidders {
                state.served_counts.insert(x.offer.holder_pubkey, x.jobs_done);
            }
        }
        let before = state.clone();
        let r = clear_within_budget(&b.spec, &bidders, req.budget, now, policy, &mut state, &name_by_pk);
        (before, r)
    };
    let cleared = match cleared {
        Ok(c) => c,
        Err(e) => return emit(&tx, json!({"type": "error", "code": "unfilled", "message": e})).await,
    };
    let mut winners: Vec<(String, String, String)> = Vec::new(); // (role, winner name, url)
    let mut total_price = 0u64;
    for c in &cleared {
        for (who, price) in &c.over_budget {
            emit(&tx, json!({"type": "over_budget", "role": c.view.role, "who": who, "price": price})).await?;
        }
        let v = &c.view;
        let reason = why(&policy_name, &v.bids, &before, &v.role, &pk_by_name, &name_by_pk);
        let Some(w) = v.bids.iter().find(|x| x.win) else {
            return emit(&tx, json!({"type": "error", "code": "unfilled", "role": v.role})).await;
        };
        let Some(url) = bidders.iter().find(|x| x.name == w.who).map(|x| x.url.clone()) else {
            return emit(&tx, json!({"type": "error", "code": "unfilled", "role": v.role})).await;
        };
        total_price += w.price;
        winners.push((v.role.clone(), w.who.clone(), url));
        emit(&tx, json!({"type": "cleared", "role": v.role, "why": reason, "bids": v.bids.iter().map(|x| json!({"who": x.who, "price": x.price, "units": x.units, "win": x.win})).collect::<Vec<_>>()})).await?;
    }

    // 4. execute: writer first, reviewer gets the writer's summary.
    let mut summary: Option<String> = None;
    let mut exec_ms = 0.0;
    for (role, who, url) in &winners {
        let who = who.clone();
        emit(&tx, json!({"type": "exec_start", "role": role, "who": who})).await?;
        let body = JobReq { job_id: job_id.clone(), text: req.text.clone(), summary: summary.clone() };
        let t0 = Instant::now();
        let out: Result<(JobResp, f64), String> = async {
            let r = b.http.post(format!("{url}/job")).json(&body).timeout(PROVIDER_TIMEOUT).send().await.map_err(|_| "unreachable".to_string())?;
            if !r.status().is_success() {
                return Err(format!("http {}", r.status().as_u16()));
            }
            let jr: JobResp = r.json().await.map_err(|_| "bad answer".to_string())?;
            Ok((jr, t0.elapsed().as_secs_f64() * 1000.0))
        }
        .await;
        match out {
            Ok((jr, rtt)) => {
                let pk = bidders.iter().find(|x| x.url == *url).map(|x| x.offer.holder_pubkey);
                let sig_ok = pk.is_some_and(|pk| verify_result(&pk, &job_id, role, &jr.output, &jr.sig_hex));
                if !sig_ok {
                    return emit(&tx, json!({"type": "error", "code": "bad_signature", "role": role, "who": who})).await;
                }
                exec_ms += jr.ms;
                if role == "writer" {
                    summary = jr.output["summary"].as_str().map(str::to_string);
                }
                emit(&tx, json!({"type": "exec_done", "role": role, "who": who, "ms": (jr.ms * 1000.0).round() / 1000.0, "rtt_ms": (rtt * 10.0).round() / 10.0, "sig_ok": true, "output": jr.output})).await?;
            }
            Err(reason) => return emit(&tx, json!({"type": "error", "code": "exec", "role": role, "who": who, "reason": reason})).await,
        }
    }
    emit(&tx, json!({"type": "done", "total_price": total_price, "budget": req.budget, "work_ms": (exec_ms * 1000.0).round() / 1000.0, "total_ms": t_start.elapsed().as_millis() as u64})).await
}

async fn tender_handler(State(b): State<Arc<Bridge>>, Json(req): Json<TenderReq>) -> impl IntoResponse {
    let text_len = req.text.chars().count();
    if req.text.trim().is_empty() || text_len > MAX_TEXT_CHARS {
        return (StatusCode::BAD_REQUEST, format!("text must be 1..={MAX_TEXT_CHARS} chars")).into_response();
    }
    if req.budget == 0 || req.budget > MAX_BUDGET {
        return (StatusCode::BAD_REQUEST, format!("budget must be 1..={MAX_BUDGET}")).into_response();
    }
    let policy_name = req.policy.clone().unwrap_or_else(|| "lowest_floor".into());
    let Some(policy) = parse_policy(&policy_name) else {
        return (StatusCode::BAD_REQUEST, "unknown policy").into_response();
    };
    let Ok(permit) = b.slots.clone().try_acquire_owned() else {
        return (StatusCode::TOO_MANY_REQUESTS, "busy").into_response();
    };
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let _permit = permit;
        let _ = run_tender(b, req, policy_name, policy, tx).await;
    });
    Sse::new(ReceiverStream::new(rx).map(|e| e)).keep_alive(KeepAlive::default()).into_response()
}

/// Live health of every provider (cached a few seconds so the public page cannot hammer them).
async fn roster_handler(State(b): State<Arc<Bridge>>) -> impl IntoResponse {
    if let Some((at, v)) = b.roster.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if at.elapsed() < ROSTER_TTL {
            return Json(v.clone());
        }
    }
    let checks = b.providers.iter().map(|url| {
        let (http, url) = (b.http.clone(), url.clone());
        async move {
            match http.get(format!("{url}/health")).timeout(Duration::from_secs(1)).send().await {
                Ok(r) if r.status().is_success() => r.json::<Value>().await.map(|v| json!({"name": v["name"], "role": v["role"], "up": true, "jobs_done": v["jobs_done"]})).unwrap_or_else(|_| json!({"name": name_from_url(&url), "up": false})),
                _ => json!({"name": name_from_url(&url), "up": false}),
            }
        }
    });
    let list = futures_join(checks).await;
    let v = json!({"providers": list, "up": list.iter().filter(|p| p["up"] == true).count(), "total": list.len()});
    *b.roster.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
    Json(v)
}

async fn futures_join<F: std::future::Future<Output = Value> + Send + 'static>(it: impl Iterator<Item = F>) -> Vec<Value> {
    let handles: Vec<_> = it.map(tokio::spawn).collect();
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.unwrap_or(json!({"up": false})));
    }
    out
}

const INDEX_HTML: &str = include_str!("../index.html");

async fn index_handler() -> impl IntoResponse {
    Html(INDEX_HTML)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let providers: Vec<String> = std::env::var("AUCTION_PROVIDERS").unwrap_or_else(|_| DEFAULT_PROVIDERS.into()).split(',').map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty()).collect();
    let state = Arc::new(Bridge {
        spec: demo_spec(),
        providers,
        selection: Mutex::new(SelectionState::default()),
        slots: Arc::new(Semaphore::new(MAX_CONCURRENT_TENDERS)),
        http: reqwest::Client::builder().build()?,
        roster: Mutex::new(None),
    });
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/roster", get(roster_handler))
        .route("/tender", post(tender_handler))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
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

    use ct_common::channel::CapacityKind;
    use ed25519_dalek::SigningKey;
    use sha2::{Digest, Sha256};

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

    fn bidder(name: &str, role: &str, price: u64, jobs: u64, now: u64) -> Bidder {
        Bidder { name: name.into(), url: format!("http://{name}"), role: role.into(), offer: real_offer(name, role, 30, price, now), jobs_done: jobs }
    }

    fn live_roster(now: u64) -> Vec<Bidder> {
        vec![
            bidder("foxtrot", "writer", 22, 0, now), bidder("delta", "writer", 30, 0, now), bidder("echo", "writer", 38, 0, now),
            bidder("borealis", "reviewer", 45, 0, now), bidder("cascade", "reviewer", 55, 0, now), bidder("aurora", "reviewer", 60, 0, now),
        ]
    }

    fn total(c: &[ClearedRole]) -> u64 {
        c.iter().map(|r| r.view.bids.iter().find(|b| b.win).unwrap().price).sum()
    }

    /// Live on 2026-10-30: budget 90, round robin, three of seven rounds ended "98 of 90 credits".
    #[test]
    fn winners_never_exceed_the_budget_under_any_policy() {
        let now = now_secs();
        let spec = demo_spec();
        let bidders = live_roster(now);
        let names: HashMap<_, _> = bidders.iter().map(|x| (x.offer.holder_pubkey, x.name.clone())).collect();
        for policy in [SelectionPolicy::RoundRobin, SelectionPolicy::LeastCalls, SelectionPolicy::LowestFloor] {
            let mut state = SelectionState::default();
            for budget in 67..=120 {
                for _ in 0..7 {
                    let c = clear_within_budget(&spec, &bidders, budget, now, policy, &mut state, &names).expect("clears");
                    assert!(total(&c) <= budget, "{policy:?}: {} > {budget}", total(&c));
                }
            }
        }
    }

    #[test]
    fn a_bid_the_winner_left_unaffordable_is_reported_as_over_budget() {
        let now = now_secs();
        let bidders = live_roster(now);
        let names: HashMap<_, _> = bidders.iter().map(|x| (x.offer.holder_pubkey, x.name.clone())).collect();
        let mut state = SelectionState::default();
        let c = clear_within_budget(&demo_spec(), &bidders, 70, now, SelectionPolicy::LowestFloor, &mut state, &names).unwrap();
        assert!(c[0].over_budget.iter().any(|(n, _)| n == "echo"), "38 + 45 > 70");
        assert!(c[1].over_budget.iter().any(|(n, _)| n == "aurora"), "22 + 60 > 70");
    }

    #[test]
    fn page_reports_a_stream_that_ends_without_a_result() {
        assert!(INDEX_HTML.contains("e_cut"), "a tender that ends with neither done nor error must not end silently");
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
