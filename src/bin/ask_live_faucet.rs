//! Ask a faucet that is actually deployed.
//!
//! The regtest scenarios spin up everything themselves, which proves the
//! logic and nothing about the deployment. This one points at a faucet that
//! is already running somewhere — installed from a package, started by
//! systemd, on a chain with real block times — and asks it for coins.
//!
//! It is what stands between "passes its own tests" and "works", and it is
//! how a miner gets verified after `nostr-faucet` is deployed beside it.
//!
//! ```text
//! FAUCET_RELAY=wss://relay.dwdc.club \
//! FAUCET_PUBKEY=<hex> \
//! DEST_ADDRESS=<address> \
//! AMOUNT_BTC=1 \
//!   cargo run --bin ask_live_faucet
//! ```
//!
//! Nothing here is spun up or torn down. Everything it talks to outlives it.


use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use nostr_faucet_e2e_test::*;
use serde_json::json;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(txid) => {
            println!("\ntxid: {txid}");
            println!("=== PASS ===");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("\n=== FAIL ===\n{e:?}");
            std::process::exit(1);
        }
    }
}

fn env(k: &str) -> Result<String> {
    std::env::var(k).with_context(|| format!("{k} must be set — see the module docs"))
}

async fn run() -> Result<String> {
    let relay = env("FAUCET_RELAY")?;
    let faucet = PublicKey::parse(&env("FAUCET_PUBKEY")?).context("FAUCET_PUBKEY is not a key")?;
    let dest = env("DEST_ADDRESS")?;
    let amount = std::env::var("AMOUNT_BTC").unwrap_or_else(|_| "1".to_string());

    // A fresh key each run. On an open policy that is not cheating — it is
    // what any new participant looks like, which is the case worth testing.
    let asker = Keys::generate();
    tracing::info!("asking {faucet} on {relay}");
    tracing::info!("  as {}", asker.public_key());
    tracing::info!("  for {amount} to {dest}");

    // What it says it can do, before asking it to do anything.
    let info = call(&relay, &asker, &faucet, "get_info", json!({}))
        .await
        .context("get_info failed — is the faucet running and on this relay?")?;
    let methods = info
        .pointer("/result/methods")
        .map(|m| m.to_string())
        .unwrap_or_else(|| "none".into());
    let network = info
        .pointer("/result/network")
        .and_then(|n| n.as_str())
        .unwrap_or("unknown");
    tracing::info!("  it says: network {network}, methods {methods}");

    // An address and an amount in sats. No BIP-321 URI: `pay_onchain` is
    // what a faucet with no Lightning wallet implements.
    let amount_sats = (amount.parse::<f64>().context("amount is not a number")? * 100_000_000.0)
        .round() as u64;
    let resp = ask(&relay, &asker, &faucet, &dest, amount_sats).await?;

    if let Some(message) = refusal(&resp) {
        anyhow::bail!("the faucet refused: {message}");
    }

    let txid = resp
        .pointer("/result/txid")
        .and_then(|t| t.as_str())
        .context("the faucet answered without a txid")?
        .to_string();

    // A txid is a promise, not money. Whoever runs this should watch the
    // destination chain for it before believing the coins arrived.
    tracing::info!("  paid — {txid}");
    tracing::info!("  a txid is not a confirmation: check the destination chain");
    Ok(txid)
}
