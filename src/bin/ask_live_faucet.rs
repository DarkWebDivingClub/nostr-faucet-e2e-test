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

use std::time::Duration;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use nwc::nostr::nips::nip04;
use nwc::nostr::nips::nip47::{Method, PayBip321Request, Request, RequestParams, Response};

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
    let info = round_trip(
        &relay,
        &asker,
        &faucet,
        Request { method: Method::GetInfo, params: RequestParams::GetInfo },
    )
    .await
    .context("get_info failed — is the faucet running and on this relay?")?;
    let info_json = serde_json::to_value(&info)?;
    let methods = info_json
        .pointer("/result/bip321_methods")
        .map(|m| m.to_string())
        .unwrap_or_else(|| "none".into());
    let network = info_json
        .pointer("/result/network")
        .and_then(|n| n.as_str())
        .unwrap_or("unknown");
    let height = info_json.pointer("/result/block_height").map(|h| h.to_string());
    tracing::info!("  it says: network {network}, height {}, bip321_methods {methods}",
        height.unwrap_or_else(|| "unknown".into()));

    let uri = format!("bitcoin:{dest}?amount={amount}");
    let resp = round_trip(
        &relay,
        &asker,
        &faucet,
        Request {
            method: Method::PayBip321,
            params: RequestParams::PayBip321(PayBip321Request { uri }),
        },
    )
    .await?;

    if let Some(err) = resp.error {
        anyhow::bail!("the faucet refused: {} ({:?})", err.message, err.code);
    }

    let value = serde_json::to_value(&resp)?;
    let txid = value
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

async fn round_trip(
    relay: &str,
    asker: &Keys,
    faucet: &PublicKey,
    req: Request,
) -> Result<Response> {
    let client = Client::builder().signer(asker.clone()).build();
    client.add_relay(relay).await?;
    client.connect().await;

    client
        .subscribe(
            Filter::new()
                .kind(Kind::WalletConnectResponse)
                .pubkey(asker.public_key())
                .since(Timestamp::now()),
        )
        .await?;

    let encrypted = nip04::encrypt(asker.secret_key(), faucet, serde_json::to_string(&req)?)?;
    client
        .send_event_builder(
            EventBuilder::new(Kind::WalletConnectRequest, encrypted).tag(Tag::public_key(*faucet)),
        )
        .await?;

    let mut notifications = client.notifications();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "no answer in 45s — a faucet that is down and one that refused should not \
             look the same, so this is a failure rather than a refusal"
        );
        let next = tokio::time::timeout(remaining, notifications.next()).await;
        let Ok(Some(ClientNotification::Event { event, .. })) = next else { continue };
        if event.kind != Kind::WalletConnectResponse || event.pubkey != *faucet {
            continue;
        }
        let plaintext = nip04::decrypt(asker.secret_key(), faucet, &event.content)?;
        let _ = client.disconnect().await;
        return Ok(serde_json::from_str(&plaintext)?);
    }
}
