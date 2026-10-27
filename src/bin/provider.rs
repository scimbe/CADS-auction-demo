//! auction-demo-provider: a genuinely independent bidder AND worker process. It holds its
//! OWN random ed25519 identity (never shared with the bridge or another provider) and
//! serves three endpoints on the internal compose network:
//!   GET  /health -- name, role, public key, jobs done
//!   POST /quote  -- a freshly signed real `ct_common::channel::CapacityOffer` (the bid)
//!   POST /job    -- performs the real work (summary or review) and signs the result
//! The bridge verifies both the offer and the result signature; it never signs for anyone.

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use auction_demo_bridge::{hex, review, sign_result, summarize, JobReq, JobResp, MAX_TEXT_CHARS};
use ct_common::channel::{CapacityKind, CapacityOffer, ServiceType};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

struct Provider {
    name: String,
    role: String,
    units: u64,
    price: u64,
    key: SigningKey,
    jobs_done: AtomicU64,
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

fn service_for_role(role: &str) -> ServiceType {
    match role {
        "reviewer" => ServiceType::SecurityReview,
        "writer" => ServiceType::TextGeneration,
        other => panic!("unknown PROVIDER_ROLE '{other}' (must be reviewer|writer)"),
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

async fn health(State(p): State<Arc<Provider>>) -> Json<Value> {
    Json(json!({
        "name": p.name, "role": p.role,
        "pubkey": hex(&p.key.verifying_key().to_bytes()),
        "jobs_done": p.jobs_done.load(Ordering::Relaxed),
    }))
}

async fn quote(State(p): State<Arc<Provider>>) -> Json<Value> {
    let now = now_secs();
    // Short validity: a quote is a live bid for this tender, not a standing fixture.
    let offer = CapacityOffer::sign_new_with_services(
        &p.key,
        CapacityKind::CloudApiQuota,
        vec!["claude".into()],
        p.units,
        p.price,
        "demo-credits".into(),
        now,
        now + 300,
        vec![service_for_role(&p.role)],
    );
    Json(json!({
        "display_name": p.name, "role": p.role, "offer": offer,
        "jobs_done": p.jobs_done.load(Ordering::Relaxed),
    }))
}

async fn job(State(p): State<Arc<Provider>>, Json(req): Json<JobReq>) -> Result<Json<JobResp>, (StatusCode, String)> {
    if req.text.chars().count() > MAX_TEXT_CHARS || req.job_id.is_empty() || req.job_id.len() > 64 {
        return Err((StatusCode::BAD_REQUEST, "bad job".into()));
    }
    let t0 = Instant::now();
    let output = match p.role.as_str() {
        "writer" => summarize(&req.text),
        "reviewer" => review(&req.text, req.summary.as_deref().ok_or((StatusCode::BAD_REQUEST, "summary required".to_string()))?),
        _ => return Err((StatusCode::INTERNAL_SERVER_ERROR, "role".into())),
    };
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let sig_hex = sign_result(&p.key, &req.job_id, &p.role, &output);
    p.jobs_done.fetch_add(1, Ordering::Relaxed);
    Ok(Json(JobResp { output, sig_hex, ms }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let name = required_env("PROVIDER_NAME");
    let role = required_env("PROVIDER_ROLE");
    let _ = service_for_role(&role);
    let units: u64 = required_env("PROVIDER_UNITS").parse().expect("PROVIDER_UNITS must be a number");
    let price: u64 = required_env("PROVIDER_PRICE").parse().expect("PROVIDER_PRICE must be a number");
    let listen = std::env::var("PROVIDER_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into());
    // A real, random identity -- generated once at process start, held only in this process.
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    let key = SigningKey::from_bytes(&seed);
    eprintln!("auction-demo-provider[{name}]: identity {} (role={role}, units={units}, price={price}) listening on {listen}", hex(&key.verifying_key().to_bytes()));
    let p = Arc::new(Provider { name, role, units, price, key, jobs_done: AtomicU64::new(0) });
    let app = Router::new()
        .route("/health", get(health))
        .route("/quote", post(quote))
        .route("/job", post(job))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(p);
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
