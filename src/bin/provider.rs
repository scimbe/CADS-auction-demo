//! auction-demo-provider: a genuinely independent bidder process. Generates its OWN
//! real ed25519 keypair (never touched by the bridge or by any other provider),
//! signs its OWN real `ct_common::channel::CapacityOffer`, and submits it to the
//! bridge's `POST /offers/submit`, which verifies the signature server-side
//! (`CapacityOffer::is_valid`) rather than trusting or re-signing it. Re-submits
//! periodically so it behaves like a real, live, independently-operated bidder rather
//! than a one-shot fixture -- one of these runs per named provider in
//! `compose.auction-demo.yml`, each its own container, never sharing a process or a
//! private key with the bridge or any other provider.

use ct_common::channel::{CapacityKind, CapacityOffer, ServiceType};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::json;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let name = required_env("PROVIDER_NAME");
    let role = required_env("PROVIDER_ROLE");
    let units: u64 = required_env("PROVIDER_UNITS").parse().expect("PROVIDER_UNITS must be a number");
    let price: u64 = required_env("PROVIDER_PRICE").parse().expect("PROVIDER_PRICE must be a number");
    let bridge_url = required_env("BRIDGE_URL");
    let interval_secs: u64 = env_or("SUBMIT_INTERVAL_SECS", "20").parse().unwrap_or(20);
    let service = service_for_role(&role);

    // A real, random identity -- generated once at process start, held only in this
    // process's memory, never derived from `name` and never shared with the bridge or
    // any other provider. Same OsRng-seeded pattern ct-agent's own `channel init` uses.
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let holder_pubkey = signing_key.verifying_key().to_bytes();
    eprintln!(
        "auction-demo-provider[{name}]: real identity {} (role={role}, units={units}, price={price})",
        hex(&holder_pubkey)
    );

    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;

    loop {
        let now = now_secs();
        let offer = CapacityOffer::sign_new_with_services(
            &signing_key,
            CapacityKind::CloudApiQuota,
            vec!["claude".into()],
            units,
            price,
            "demo-credits".into(),
            now,
            now + 10 * 365 * 24 * 3600,
            vec![service],
        );
        let body = json!({"display_name": name, "role": role, "offer": offer});
        match client.post(format!("{bridge_url}/offers/submit")).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {
                eprintln!("auction-demo-provider[{name}]: submitted a real signed offer to {bridge_url}");
            }
            Ok(resp) => {
                eprintln!("auction-demo-provider[{name}]: bridge rejected the offer: HTTP {}", resp.status());
            }
            Err(e) => {
                eprintln!("auction-demo-provider[{name}]: could not reach the bridge yet: {e}");
            }
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
